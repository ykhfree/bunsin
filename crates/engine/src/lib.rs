//! The harness pipeline: intake, proposal, policy check, approval, execution, audit.
//!
//! The LLM only proposes. Every permission decision is made here from stored facts.

use bunsin_api::{
    ApiError, ApiResult, Edit, EngineApi, Outcome, ProfileConfig, ProfileSnapshot, ReplayCase,
    ReplayResult,
};
use bunsin_core::approval::ApprovalState;
use bunsin_core::context::{ContextInput, Correction, ToolHint, assemble};
use bunsin_core::event::InboundEvent;
use bunsin_core::llm::{CompletionRequest, Message, Role, StopReason};
use bunsin_core::marker::mark_args;
use bunsin_core::policy::Decision;
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_core::proposal::{Proposal, parse_proposal, split_tool_id};
use bunsin_core::records::{InboxBody, InboxItem, ProfileInfo};
use bunsin_core::tool::{pin, pin_holds};
use bunsin_store::{AppStore, ProfileStore, StoreError};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

const CONFIG_KEY: &str = "config";
const LIST_LIMIT: u32 = 200;
const CORRECTIONS_IN_PROMPT: u32 = 5;

/// The live clients of one profile.
pub struct Runtime {
    pub llm: Arc<dyn LlmClient>,
    /// By connector name.
    pub connectors: BTreeMap<String, Arc<dyn Connector>>,
}

/// Builds the LLM client and the connectors of a profile from its config.
pub trait Backends: Send + Sync {
    fn build<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
    ) -> BoxFuture<'a, Result<Runtime, String>>;
}

/// Returns the current time in Unix seconds.
pub type Clock = Arc<dyn Fn() -> i64 + Send + Sync>;

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("another engine already uses this data directory")]
    AlreadyRunning,
    #[error("the data directory cannot be opened: {0}")]
    Store(#[from] StoreError),
    #[error("the lock file cannot be opened: {0}")]
    Io(#[from] std::io::Error),
}

/// What the handling of one request needs from a profile.
struct Ctx {
    store: Arc<ProfileStore>,
    config: ProfileConfig,
    runtime: Arc<Runtime>,
}

pub struct Engine {
    app: AppStore,
    backends: Arc<dyn Backends>,
    clock: Clock,
    stores: Mutex<HashMap<String, Arc<ProfileStore>>>,
    runtimes: Mutex<HashMap<String, Arc<Runtime>>>,
    /// Held for the life of the engine. A second engine on the same directory fails to open.
    _lock: File,
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(error.to_string())
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::NoSuchProfile(_) | StoreError::NotFound(..) => {
            ApiError::NotFound(error.to_string())
        }
        StoreError::BadProfileId(_) | StoreError::ProfileExists(_) => {
            ApiError::Invalid(error.to_string())
        }
        other => internal(other),
    }
}

/// The text the user sees and edits: the outgoing-text argument, or all arguments.
fn draft_text(args: &Map<String, Value>, text_arg: Option<&str>) -> String {
    match text_arg
        .and_then(|name| args.get(name))
        .and_then(Value::as_str)
    {
        Some(text) => text.to_owned(),
        None => Value::Object(args.clone()).to_string(),
    }
}

impl Engine {
    pub fn open(
        data_dir: &Path,
        backends: Arc<dyn Backends>,
        clock: Clock,
    ) -> Result<Self, OpenError> {
        let app = AppStore::open(data_dir)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(data_dir.join("engine.lock"))?;
        lock.try_lock().map_err(|_| OpenError::AlreadyRunning)?;
        Ok(Self {
            app,
            backends,
            clock,
            stores: Mutex::new(HashMap::new()),
            runtimes: Mutex::new(HashMap::new()),
            _lock: lock,
        })
    }

    fn now(&self) -> i64 {
        (self.clock)()
    }

    fn store(&self, profile: &str) -> ApiResult<Arc<ProfileStore>> {
        let mut stores = self.stores.lock().expect("engine lock");
        if let Some(store) = stores.get(profile) {
            return Ok(store.clone());
        }
        let store = Arc::new(self.app.open_profile(profile).map_err(store_error)?);
        stores.insert(profile.to_owned(), store.clone());
        Ok(store)
    }

    fn config(store: &ProfileStore) -> ApiResult<ProfileConfig> {
        let text = store.setting(CONFIG_KEY).map_err(store_error)?;
        let text = text.ok_or_else(|| internal("the profile has no stored config"))?;
        serde_json::from_str(&text).map_err(internal)
    }

    /// The runtime is built on first use, so a broken LLM or connector setting
    /// does not block the inbox, the audit log or the kill switch.
    async fn runtime(&self, profile: &str, config: &ProfileConfig) -> ApiResult<Arc<Runtime>> {
        if let Some(runtime) = self.runtimes.lock().expect("engine lock").get(profile) {
            return Ok(runtime.clone());
        }
        let runtime = Arc::new(
            self.backends
                .build(profile, config)
                .await
                .map_err(ApiError::Internal)?,
        );
        self.runtimes
            .lock()
            .expect("engine lock")
            .insert(profile.to_owned(), runtime.clone());
        Ok(runtime)
    }

    async fn ctx(&self, profile: &str) -> ApiResult<Ctx> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let runtime = self.runtime(profile, &config).await?;
        Ok(Ctx {
            store,
            config,
            runtime,
        })
    }

    fn audit(
        &self,
        store: &ProfileStore,
        kind: &str,
        event_key: Option<&str>,
        detail: Value,
    ) -> ApiResult<()> {
        store
            .append_audit(self.now(), kind, event_key, &detail)
            .map_err(store_error)
    }

    /// The tools the agent may propose: in the permission table and approved by the user.
    fn tool_hints(config: &ProfileConfig) -> Vec<ToolHint> {
        config
            .policy
            .rules
            .keys()
            .filter_map(|tool_id| {
                let (connector, tool) = split_tool_id(tool_id)?;
                let spec = config
                    .approved_tools
                    .get(connector)?
                    .iter()
                    .find(|spec| spec.name == tool)?;
                Some(ToolHint {
                    tool_id: tool_id.clone(),
                    description: spec.description.clone(),
                })
            })
            .collect()
    }

    /// Asks the LLM for a proposal. An invalid reply gets one repair request.
    /// Returns the proposal or the reason there is none, and every raw LLM reply.
    async fn propose(
        &self,
        ctx: &Ctx,
        event: &InboundEvent,
    ) -> ApiResult<(Result<Proposal, String>, Vec<String>)> {
        let Ctx {
            store,
            config,
            runtime,
        } = ctx;
        let corrections = store
            .recent_corrections(CORRECTIONS_IN_PROMPT)
            .map_err(store_error)?;
        let tools = Self::tool_hints(config);
        let mut request: CompletionRequest = assemble(&ContextInput {
            owner_name: &config.owner_name,
            principles: &config.principles,
            corrections: &corrections,
            knowledge: &[],
            tools: &tools,
            event,
            max_output_tokens: config.max_output_tokens,
        });

        let mut replies = Vec::new();
        let mut used_tokens = 0;
        for attempt in 0..2 {
            let response = match runtime.llm.complete(request.clone()).await {
                Ok(response) => response,
                Err(error) => return Ok((Err(format!("LLM call failed: {error}")), replies)),
            };
            used_tokens += response.usage.total();
            replies.push(response.content.clone());
            if response.stop == StopReason::Refusal {
                return Ok((Err("the LLM refused the request".into()), replies));
            }
            let error = match parse_proposal(&response.content) {
                Ok(proposal) => return Ok((Ok(proposal), replies)),
                Err(error) => error,
            };
            if attempt == 1 {
                return Ok((
                    Err(format!("the proposal is invalid after one repair: {error}")),
                    replies,
                ));
            }
            if used_tokens >= config.token_budget {
                return Ok((
                    Err(format!(
                        "token budget of {} is used up",
                        config.token_budget
                    )),
                    replies,
                ));
            }
            request.messages.push(Message {
                role: Role::Assistant,
                content: response.content,
            });
            request.messages.push(Message {
                role: Role::User,
                content: format!(
                    "Your reply was rejected: {error}. Reply again with one JSON object that matches the schema."
                ),
            });
        }
        unreachable!("the loop returns on its second pass")
    }

    async fn decide(&self, ctx: &Ctx, tool_id: &str) -> ApiResult<Decision> {
        let Ctx {
            store,
            config,
            runtime,
        } = ctx;
        let kill_switch = store.kill_switch().map_err(store_error)?;
        // The connector is asked only about tools that are in the permission table.
        let pin_ok = match split_tool_id(tool_id) {
            Some((connector, tool)) if config.policy.rules.contains_key(tool_id) => {
                match (
                    config.approved_tools.get(connector),
                    runtime.connectors.get(connector),
                ) {
                    (Some(approved), Some(live)) => match live.list_tools().await {
                        Ok(current) => pin_holds(&pin(approved), &current, tool),
                        Err(_) => false,
                    },
                    _ => false,
                }
            }
            _ => false,
        };
        Ok(config.policy.decide(tool_id, pin_ok, kill_switch))
    }

    /// Runs an approved item. Only the caller that moves it to `Executing` runs it.
    async fn execute(&self, ctx: &Ctx, item: &InboxItem) -> ApiResult<Outcome> {
        let Ctx { store, runtime, .. } = ctx;
        let InboxBody::ToolCall {
            tool: tool_id,
            args,
        } = &item.body
        else {
            return Err(ApiError::NotAllowed("only a tool call can run".into()));
        };
        if !store
            .move_inbox_state(
                item.id,
                ApprovalState::Approved,
                ApprovalState::Executing,
                self.now(),
            )
            .map_err(store_error)?
        {
            return Err(ApiError::NotAllowed("the item was already handled".into()));
        }
        let key = Some(item.event_key.as_str());
        let result =
            match split_tool_id(tool_id).and_then(|(c, t)| Some((runtime.connectors.get(c)?, t))) {
                Some((connector, tool)) => connector
                    .call_tool(tool, args.clone())
                    .await
                    .map_err(|e| e.to_string()),
                None => Err(format!("no connector for `{tool_id}`")),
            };
        match result {
            Ok(value) => {
                let content = serde_json::to_vec_pretty(&value).map_err(internal)?;
                store
                    .put_artifact("tool_result", &content, self.now(), key, Some(item.id))
                    .map_err(store_error)?;
                store
                    .move_inbox_state(
                        item.id,
                        ApprovalState::Executing,
                        ApprovalState::Executed,
                        self.now(),
                    )
                    .map_err(store_error)?;
                self.audit(
                    store,
                    "executed",
                    key,
                    json!({"inbox_id": item.id, "tool": tool_id}),
                )?;
                Ok(Outcome::Executed { inbox_id: item.id })
            }
            Err(error) => {
                store
                    .move_inbox_state(
                        item.id,
                        ApprovalState::Executing,
                        ApprovalState::Failed,
                        self.now(),
                    )
                    .map_err(store_error)?;
                self.audit(
                    store,
                    "execution_failed",
                    key,
                    json!({"inbox_id": item.id, "tool": tool_id, "error": error}),
                )?;
                Ok(Outcome::ExecutionFailed {
                    inbox_id: item.id,
                    error,
                })
            }
        }
    }

    async fn handle_tool_call(
        &self,
        ctx: &Ctx,
        key: &str,
        rationale: &str,
        tool_id: String,
        mut args: Map<String, Value>,
    ) -> ApiResult<Outcome> {
        let Ctx { store, config, .. } = ctx;
        let decision = self.decide(ctx, &tool_id).await?;
        self.audit(
            store,
            "policy_decision",
            Some(key),
            json!({"tool": tool_id, "decision": decision}),
        )?;
        let state = match decision {
            Decision::Deny(reason) => return Ok(Outcome::Denied { reason }),
            Decision::RecordOnly => return Ok(Outcome::Recorded),
            Decision::DraftOnly => ApprovalState::Draft,
            Decision::NeedsApproval => ApprovalState::Pending,
            Decision::Execute => ApprovalState::Approved,
        };
        // The marker is added before the user sees the draft, so the inbox shows what will be sent.
        let text_arg = config
            .policy
            .rules
            .get(&tool_id)
            .and_then(|rule| rule.text_arg.as_deref());
        if let Some(text_arg) = text_arg
            && let Err(error) = mark_args(&mut args, text_arg, &config.ai_marker)
        {
            self.audit(
                store,
                "proposal_invalid",
                Some(key),
                json!({"error": error.to_string()}),
            )?;
            return Ok(Outcome::Failed {
                error: error.to_string(),
            });
        }
        let body = InboxBody::ToolCall {
            tool: tool_id,
            args,
        };
        let item = store
            .add_inbox_item(key, &body, rationale, state, self.now())
            .map_err(store_error)?;
        match state {
            ApprovalState::Draft => Ok(Outcome::Drafted { inbox_id: item.id }),
            ApprovalState::Pending => Ok(Outcome::PendingApproval { inbox_id: item.id }),
            _ => self.execute(ctx, &item).await,
        }
    }

    async fn ingest_event(&self, profile: &str, event: InboundEvent) -> ApiResult<Outcome> {
        let store = self.store(profile)?;
        let key = event.dedupe_key();
        let payload = serde_json::to_value(&event).map_err(internal)?;
        if !store
            .record_event(&key, &payload, self.now())
            .map_err(store_error)?
        {
            return Ok(Outcome::Duplicate);
        }
        self.audit(
            &store,
            "event_received",
            Some(&key),
            json!({"source": event.source, "author": event.author}),
        )?;

        let ctx = self.ctx(profile).await?;
        let (proposal, replies) = self.propose(&ctx, &event).await?;
        for reply in &replies {
            store
                .put_artifact("llm_reply", reply.as_bytes(), self.now(), Some(&key), None)
                .map_err(store_error)?;
        }
        let proposal = match proposal {
            Ok(proposal) => proposal,
            Err(error) => {
                self.audit(
                    &store,
                    "proposal_failed",
                    Some(&key),
                    json!({"error": error}),
                )?;
                return Ok(Outcome::Failed { error });
            }
        };
        let content = serde_json::to_vec_pretty(&proposal).map_err(internal)?;
        store
            .put_artifact("proposal", &content, self.now(), Some(&key), None)
            .map_err(store_error)?;
        self.audit(
            &store,
            "proposal",
            Some(&key),
            serde_json::to_value(&proposal).map_err(internal)?,
        )?;

        match proposal {
            Proposal::Ignore { .. } => Ok(Outcome::Ignored),
            Proposal::Escalate { rationale, summary } => {
                let body = InboxBody::Escalation { summary };
                let item = store
                    .add_inbox_item(&key, &body, &rationale, ApprovalState::Draft, self.now())
                    .map_err(store_error)?;
                Ok(Outcome::Escalated { inbox_id: item.id })
            }
            Proposal::ToolCall {
                rationale,
                tool,
                args,
            } => {
                self.handle_tool_call(&ctx, &key, &rationale, tool, args)
                    .await
            }
        }
    }

    async fn approve_item(
        &self,
        profile: &str,
        inbox_id: i64,
        edit: Option<Edit>,
    ) -> ApiResult<Outcome> {
        let store = self.store(profile)?;
        let mut item = store.inbox_item(inbox_id).map_err(store_error)?;
        let InboxBody::ToolCall {
            tool: tool_id,
            args,
        } = item.body.clone()
        else {
            return Err(ApiError::NotAllowed(
                "an escalation cannot be approved".into(),
            ));
        };
        if item.state != ApprovalState::Pending {
            return Err(ApiError::NotAllowed(format!(
                "the item is {}, not pending",
                item.state.as_str()
            )));
        }
        let ctx = self.ctx(profile).await?;
        let config = &ctx.config;
        let key = item.event_key.clone();
        let text_arg = config
            .policy
            .rules
            .get(&tool_id)
            .and_then(|rule| rule.text_arg.as_deref());

        if let Some(Edit {
            args: mut edited,
            reason,
        }) = edit
        {
            // The user may have removed the marker while editing. It is added again.
            if let Some(text_arg) = text_arg {
                mark_args(&mut edited, text_arg, &config.ai_marker)
                    .map_err(|e| ApiError::Invalid(e.to_string()))?;
            }
            let correction = Correction {
                original: draft_text(&args, text_arg),
                edited: draft_text(&edited, text_arg),
                reason,
            };
            let body = InboxBody::ToolCall {
                tool: tool_id.clone(),
                args: edited,
            };
            if !store
                .replace_pending_body(inbox_id, &body, self.now())
                .map_err(store_error)?
            {
                return Err(ApiError::NotAllowed("the item was already handled".into()));
            }
            store
                .add_correction(inbox_id, &correction, self.now())
                .map_err(store_error)?;
            store
                .put_artifact(
                    "edited_draft",
                    correction.edited.as_bytes(),
                    self.now(),
                    Some(&key),
                    Some(inbox_id),
                )
                .map_err(store_error)?;
            self.audit(
                &store,
                "edited",
                Some(&key),
                json!({"inbox_id": inbox_id, "reason": correction.reason}),
            )?;
            item.body = body;
        }

        // The rules may have changed since the item was created, so they are checked again.
        let decision = self.decide(&ctx, &tool_id).await?;
        if !matches!(decision, Decision::NeedsApproval | Decision::Execute) {
            self.audit(
                &store,
                "approval_blocked",
                Some(&key),
                json!({"inbox_id": inbox_id, "decision": decision}),
            )?;
            return Err(ApiError::NotAllowed(format!(
                "the policy does not allow this call now: {decision:?}"
            )));
        }
        if !store
            .move_inbox_state(
                inbox_id,
                ApprovalState::Pending,
                ApprovalState::Approved,
                self.now(),
            )
            .map_err(store_error)?
        {
            return Err(ApiError::NotAllowed("the item was already handled".into()));
        }
        self.audit(
            &store,
            "approved",
            Some(&key),
            json!({"inbox_id": inbox_id}),
        )?;
        self.execute(&ctx, &item).await
    }

    fn reject_item(&self, profile: &str, inbox_id: i64, reason: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        let item = store.inbox_item(inbox_id).map_err(store_error)?;
        let moved = [ApprovalState::Pending, ApprovalState::Draft]
            .into_iter()
            .any(|from| {
                store
                    .move_inbox_state(inbox_id, from, ApprovalState::Rejected, self.now())
                    .unwrap_or(false)
            });
        if !moved {
            return Err(ApiError::NotAllowed(format!(
                "the item is {}",
                item.state.as_str()
            )));
        }
        self.audit(
            &store,
            "rejected",
            Some(&item.event_key),
            json!({"inbox_id": inbox_id, "reason": reason}),
        )
    }

    async fn replay_case(&self, profile: &str, case: ReplayCase) -> ApiResult<ReplayResult> {
        let ctx = self.ctx(profile).await?;
        let store = &ctx.store;
        // Only the event goes to the LLM. The real reply is kept for the comparison.
        let (proposal, replies) = self.propose(&ctx, &case.event).await?;
        let key = case.event.dedupe_key();
        for reply in &replies {
            store
                .put_artifact(
                    "replay_reply",
                    reply.as_bytes(),
                    self.now(),
                    Some(&key),
                    None,
                )
                .map_err(store_error)?;
        }
        let (proposal, error) = match proposal {
            Ok(proposal) => (Some(proposal), None),
            Err(error) => (None, Some(error)),
        };
        Ok(ReplayResult {
            proposal,
            error,
            actual_reply: case.actual_reply,
        })
    }
}

impl EngineApi for Engine {
    fn list_profiles(&self) -> BoxFuture<'_, ApiResult<Vec<ProfileInfo>>> {
        Box::pin(async move { self.app.list_profiles().map_err(store_error) })
    }

    fn create_profile<'a>(
        &'a self,
        id: &'a str,
        config: ProfileConfig,
    ) -> BoxFuture<'a, ApiResult<ProfileInfo>> {
        Box::pin(async move {
            let text = serde_json::to_string(&config).map_err(internal)?;
            let info = self
                .app
                .create_profile(id, &config.owner_name, self.now())
                .map_err(store_error)?;
            let store = self.store(id)?;
            store.set_setting(CONFIG_KEY, &text).map_err(store_error)?;
            self.audit(&store, "profile_created", None, json!({"id": id}))?;
            Ok(info)
        })
    }

    fn profile_config<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileConfig>> {
        Box::pin(async move { Self::config(&*self.store(profile)?) })
    }

    fn set_profile_config<'a>(
        &'a self,
        profile: &'a str,
        config: ProfileConfig,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let store = self.store(profile)?;
            let text = serde_json::to_string(&config).map_err(internal)?;
            store.set_setting(CONFIG_KEY, &text).map_err(store_error)?;
            // The next use builds the clients again from the new settings.
            self.runtimes.lock().expect("engine lock").remove(profile);
            self.audit(&store, "config_changed", None, json!({}))
        })
    }

    fn snapshot<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileSnapshot>> {
        Box::pin(async move {
            let store = self.store(profile)?;
            let profiles = self.app.list_profiles().map_err(store_error)?;
            let info = profiles
                .into_iter()
                .find(|p| p.id == profile)
                .ok_or_else(|| ApiError::NotFound(format!("profile `{profile}`")))?;
            Ok(ProfileSnapshot {
                profile: info,
                kill_switch: store.kill_switch().map_err(store_error)?,
                inbox: store.list_inbox(LIST_LIMIT).map_err(store_error)?,
                audit: store.list_audit(LIST_LIMIT).map_err(store_error)?,
                artifacts: store.list_artifacts(LIST_LIMIT).map_err(store_error)?,
            })
        })
    }

    fn ingest<'a>(
        &'a self,
        profile: &'a str,
        event: InboundEvent,
    ) -> BoxFuture<'a, ApiResult<Outcome>> {
        Box::pin(self.ingest_event(profile, event))
    }

    fn approve<'a>(
        &'a self,
        profile: &'a str,
        inbox_id: i64,
        edit: Option<Edit>,
    ) -> BoxFuture<'a, ApiResult<Outcome>> {
        Box::pin(self.approve_item(profile, inbox_id, edit))
    }

    fn reject<'a>(
        &'a self,
        profile: &'a str,
        inbox_id: i64,
        reason: &'a str,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.reject_item(profile, inbox_id, reason) })
    }

    fn set_kill_switch<'a>(&'a self, profile: &'a str, on: bool) -> BoxFuture<'a, ApiResult<bool>> {
        Box::pin(async move {
            let store = self.store(profile)?;
            store.set_kill_switch(on).map_err(store_error)?;
            self.audit(&store, "kill_switch", None, json!({"on": on}))?;
            store.kill_switch().map_err(store_error)
        })
    }

    fn replay<'a>(
        &'a self,
        profile: &'a str,
        case: ReplayCase,
    ) -> BoxFuture<'a, ApiResult<ReplayResult>> {
        Box::pin(self.replay_case(profile, case))
    }
}
