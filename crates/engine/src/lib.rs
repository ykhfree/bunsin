//! The harness pipeline: intake, proposal, policy check, approval, execution, audit.
//!
//! The LLM only proposes. Every permission decision is made here from stored facts.

mod ard;
mod autonomy;
mod callers;
mod candidates;
mod coding;
mod connections;
mod corrections;
mod eval;
mod intake;
mod lint;
mod llm_log;
mod notes;
mod oauth;
mod poll_settings;
mod polling;
mod preload;
mod principles;
mod registry;
mod resume;
mod secrets;
mod settings_parts;
mod webhooks;

pub use ard::OuterListener;
pub use oauth::{OAuthSignIn, SecretWriter};
pub use preload::PRELOAD_SOURCE;
pub use resume::Interrupted;
pub use secrets::{KeyFile, KeySource, MasterKey, ProfileSecrets, StoredSecret};
pub use webhooks::WebhookTarget;

use bunsin_api::{
    AdoptPrinciple, ApiConnectorUpdate, ApiError, ApiResult, ArdRegistriesUpdate, Autonomy, AutonomyThresholdsUpdate,
    CommandConnectorUpdate, Connections, ConnectorUpdate, Edit, EngineApi, EvalReport, EvalSettingsUpdate, Expected,
    InboundStatus, InboundUpdate, LlmTest, LlmUpdate, NewCallerToken, NotesSettingsUpdate, OAuthStarted, Outcome,
    Permissions, PermissionsUpdate, PollPreview, PollsUpdate, PreloadNotes, PrinciplesUpdate, PrivacyUpdate,
    ProfileConfig, ProfileSnapshot, ReplayCase, ReplayResult, Resolution, ToolsReport, WebhooksUpdate, WikiPageUpdate,
};
use bunsin_core::approval::ApprovalState;
use bunsin_core::ard::{ARD_CONNECTOR, ArdRegistry};
use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::candidates::{PrincipleCandidate, PrincipleSource};
use bunsin_core::coding::is_coding_tool;
use bunsin_core::context::{ContextInput, ToolHint, assemble};
use bunsin_core::eval::{EvalConfig, ReplayCaseRecord};
use bunsin_core::event::InboundEvent;
use bunsin_core::inbound::InboundConfig;
use bunsin_core::lint::LintFinding;
use bunsin_core::llm::{CompletionRequest, Message, Role, StopReason};
use bunsin_core::marker::mark_args;
use bunsin_core::policy::{Decision, ToolClass};
use bunsin_core::poll::PollConfig;
use bunsin_core::ports::{BoxFuture, Coder, Connector, ConnectorError, LlmClient};
use bunsin_core::privacy::{PLACEHOLDER_PREFIX, PrivacyConfig, RedactionCounts, Redactor};
use bunsin_core::proposal::{Proposal, parse_reply, proposal_schema, split_tool_id};
use bunsin_core::records::{
    CallerToken, CorrectionRecord, DecisionKind, InboxBody, InboxItem, LlmUsage, ProfileInfo, ReadNote,
};
use bunsin_core::tool::{PinSet, diff_pins, pin, pin_holds};
use bunsin_core::webhook::WebhookConfig;
use bunsin_core::wiki::{
    NotesConfig, WIKI_READ_TOOL, WIKI_SEARCH_TOOL, WikiPage, WikiSummary, WikiUpdate, is_notes_tool,
};
use bunsin_store::{AppStore, EventStart, ProfileStore, StoreError};
use llm_log::Purpose;
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::{Arc, Mutex};

const CONFIG_KEY: &str = "config";
const LIST_LIMIT: u32 = 200;
/// How many times the handling of one event is tried before the owner is asked.
const MAX_EVENT_ATTEMPTS: u32 = 3;
/// How many tools that read the agent may call for one event.
const MAX_READS_PER_EVENT: usize = 5;
/// The most characters of one tool result that are stored and sent to the LLM.
const MAX_RESULT_CHARS: usize = 20_000;

/// What `propose` ended with.
enum Proposed {
    Ready {
        proposal: Proposal,
        reads: Vec<ReadNote>,
        /// The page changes the final reply asks for.
        wiki_updates: Vec<WikiUpdate>,
        /// The pages of the notes that were read in this event.
        pages_read: BTreeSet<String>,
    },
    /// No proposal this time. Another try can work, for example after a rate limit.
    Retry(String),
    /// No proposal, and another try would end the same way. The owner must look at the event.
    Stop(String),
}

/// The live clients of one profile. A client that could not be built holds the reason,
/// so one wrong address stops only the part that needs it.
pub struct Runtime {
    pub llm: Result<Arc<dyn LlmClient>, String>,
    /// By connector name.
    pub connectors: BTreeMap<String, Result<Arc<dyn Connector>, String>>,
}

impl Runtime {
    /// The connector with this name, or why it cannot be used.
    pub fn connector(&self, name: &str) -> Result<&Arc<dyn Connector>, String> {
        match self.connectors.get(name) {
            Some(Ok(connector)) => Ok(connector),
            Some(Err(error)) => Err(error.clone()),
            None => Err(format!("there is no connector `{name}`")),
        }
    }
}

/// Builds the LLM client and the connectors of a profile from its config.
pub trait Backends: Send + Sync {
    fn build<'a>(&'a self, profile: &'a str, config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>>;

    /// Like `build`, with the tokens the owner stored on the server.
    fn build_with_secrets<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
        _secrets: &'a ProfileSecrets,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        self.build(profile, config)
    }

    /// Like `build_with_secrets`. A connector stores new OAuth credentials through `writer`.
    fn build_with_writer<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
        secrets: &'a ProfileSecrets,
        _writer: Arc<dyn SecretWriter>,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        self.build_with_secrets(profile, config, secrets)
    }

    /// Starts the OAuth sign-in of connector `name`, whose settings are `section`.
    /// Returns the address of the page where the owner signs in.
    fn oauth_start<'a>(
        &'a self,
        _profile: &'a str,
        _name: &'a str,
        _section: &'a Value,
        _redirect_uri: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async { Err("this server cannot sign in to connectors with OAuth".to_owned()) })
    }

    /// Ends a sign-in that `oauth_start` began, with the address the browser went to after it.
    fn oauth_finish<'a>(
        &'a self,
        _profile: &'a str,
        _name: &'a str,
        _callback_url: &'a str,
    ) -> BoxFuture<'a, Result<OAuthSignIn, String>> {
        Box::pin(async { Err("this server cannot sign in to connectors with OAuth".to_owned()) })
    }

    /// Checks new settings before they are stored. The error says what to change.
    fn validate(&self, _profile: &str, _config: &ProfileConfig) -> Result<(), String> {
        Ok(())
    }

    /// True if the server operator supplies this secret outside the store.
    fn has_server_secret(&self, _profile: &str, _key: &str) -> bool {
        false
    }

    /// The publisher domains that the server operator allows in ARD entries, in lower case.
    fn ard_publishers(&self) -> Vec<String> {
        Vec::new()
    }

    /// The runner of coding jobs. Only a server whose operator allows coding has one.
    fn coder(&self) -> Result<Arc<dyn Coder>, String> {
        Err("this server does not run coding jobs: its operator did not start it with --allow-coding".into())
    }
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
    #[error("the key that encrypts stored tokens cannot be used: {0}")]
    Key(String),
}

/// What the handling of one request needs from a profile.
struct Ctx {
    profile: String,
    store: Arc<ProfileStore>,
    config: ProfileConfig,
    runtime: Arc<Runtime>,
    redactor: Redactor,
}

pub struct Engine {
    app: AppStore,
    backends: Arc<dyn Backends>,
    clock: Clock,
    stores: Mutex<HashMap<String, Arc<ProfileStore>>>,
    runtimes: Mutex<HashMap<String, Arc<Runtime>>>,
    /// The profiles that are being polled now. A profile is polled by one task at a time.
    polling: Mutex<HashSet<String>>,
    /// When each poll last ran: profile and poll identity to Unix seconds.
    last_polls: Mutex<HashMap<(String, String), i64>>,
    /// Encrypts and decrypts the tokens that owners store.
    secret_box: secrets::SecretBox,
    /// The listeners for callers of the running server. The ARD catalogs and the inbound status name them.
    outer: Mutex<Vec<OuterListener>>,
    /// Taken by every write of OAuth credentials. See `oauth::ProfileWriter`.
    oauth_writes: Arc<Mutex<()>>,
    /// The ARD registry, if the server operator turned it on. See `registry`.
    registry: std::sync::OnceLock<registry::Registry>,
    /// Held for the life of the engine. A second engine on the same directory fails to open.
    _lock: File,
}

fn internal(error: impl std::fmt::Display) -> ApiError {
    ApiError::Internal(error.to_string())
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::NoSuchProfile(_) | StoreError::NotFound(..) => ApiError::NotFound(error.to_string()),
        StoreError::BadProfileId(_) | StoreError::ProfileExists(_) => ApiError::Invalid(error.to_string()),
        other => internal(other),
    }
}

/// Creates a profile with its settings. The server operator does this, not a profile owner.
/// It needs no running engine, so it also works while a server uses the data directory.
pub fn create_profile(app: &AppStore, id: &str, config: &ProfileConfig, now: i64) -> ApiResult<ProfileInfo> {
    let text = serde_json::to_string(config).map_err(internal)?;
    let info = app.create_profile(id, &config.owner_name, now).map_err(store_error)?;
    let store = app.open_profile(id).map_err(store_error)?;
    store.set_setting(CONFIG_KEY, &text).map_err(store_error)?;
    store
        .append_audit(now, "profile_created", None, &json!({"id": id}))
        .map_err(store_error)?;
    Ok(info)
}

impl Engine {
    /// The shared store. The server reads access tokens from it.
    pub fn app_store(&self) -> &AppStore {
        &self.app
    }

    pub async fn create_profile(&self, id: &str, config: ProfileConfig) -> ApiResult<ProfileInfo> {
        create_profile(&self.app, id, &config, self.now())
    }

    /// Opens the engine with the master key file `master.key` in the data directory.
    pub fn open(data_dir: &Path, backends: Arc<dyn Backends>, clock: Clock) -> Result<Self, OpenError> {
        Self::open_with_key_file(data_dir, &data_dir.join("master.key"), backends, clock)
    }

    /// Opens the engine. `key_file` holds the key that encrypts stored tokens. It is
    /// created if it does not exist. Keep it out of backups of the data directory.
    pub fn open_with_key_file(
        data_dir: &Path,
        key_file: &Path,
        backends: Arc<dyn Backends>,
        clock: Clock,
    ) -> Result<Self, OpenError> {
        Self::open_with_key(data_dir, &KeyFile(key_file.to_owned()), backends, clock)
    }

    /// Opens the engine with the master key of `source`. The source is asked only after this
    /// process holds the lock of the data directory, so a second instance never touches the key.
    /// It may make a new key only if the data directory holds no encrypted value.
    pub fn open_with_key(
        data_dir: &Path,
        source: &dyn KeySource,
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
        // A profile that cannot be read counts as one with encrypted values. It must not stop
        // the other profiles: each profile is opened again, and reported, on its first use.
        let sealed = app.list_profiles()?.iter().any(|profile| {
            match app.open_profile(&profile.id).and_then(|store| store.secret_keys()) {
                Ok(keys) => !keys.is_empty(),
                Err(_) => true,
            }
        });
        let secret_box = secrets::SecretBox::new(&source.key(!sealed).map_err(OpenError::Key)?);
        Ok(Self {
            app,
            backends,
            clock,
            stores: Mutex::new(HashMap::new()),
            runtimes: Mutex::new(HashMap::new()),
            polling: Mutex::new(HashSet::new()),
            last_polls: Mutex::new(HashMap::new()),
            secret_box,
            outer: Mutex::new(Vec::new()),
            oauth_writes: Arc::new(Mutex::new(())),
            registry: std::sync::OnceLock::new(),
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
        let store = self.store(profile)?;
        let mut secrets = ProfileSecrets::new();
        for key in store.secret_keys().map_err(store_error)? {
            if let Some(sealed) = store.secret(&key).map_err(store_error)? {
                // A token that cannot be decrypted is an error for its user, never a silent fallback.
                secrets.insert(key.clone(), self.secret_box.open(profile, &key, &sealed));
            }
        }
        let writer = self.writer(profile, store);
        let built = self.backends.build_with_writer(profile, config, &secrets, writer).await;
        let runtime = Arc::new(built.map_err(ApiError::Internal)?);
        self.runtimes
            .lock()
            .expect("engine lock")
            .insert(profile.to_owned(), runtime.clone());
        Ok(runtime)
    }

    /// Compiles the privacy filter. With broken settings nothing is stored and the LLM is
    /// not called: text must never pass without the filter.
    fn redactor(config: &ProfileConfig) -> ApiResult<Redactor> {
        Redactor::new(&config.privacy).map_err(|e| ApiError::Invalid(format!("the privacy settings are invalid: {e}")))
    }

    /// Checks the parts of new settings that the engine itself relies on.
    fn check_config(config: &ProfileConfig) -> ApiResult<()> {
        Self::redactor(config)?;
        for (tool_id, rule) in &config.policy.rules {
            rule.outgoing_text
                .check()
                .map_err(|e| ApiError::Invalid(format!("the rule of `{tool_id}`: {e}")))?;
        }
        if let Some(thresholds) = &config.autonomy {
            thresholds.check().map_err(ApiError::Invalid)?;
        }
        if let Some(pass) = &config.eval.pass {
            pass.check().map_err(ApiError::Invalid)?;
        }
        config.notes.check().map_err(ApiError::Invalid)?;
        Self::check_coding_rules(config)?;
        Self::check_inbound(config)?;
        Self::check_ard(config)?;
        bunsin_core::ard::check_registries(&config.ard_registries).map_err(ApiError::Invalid)?;
        bunsin_core::ard::check_domains(&config.ard_domains).map_err(ApiError::Invalid)?;
        Self::check_webhooks(config)?;
        for poll in &config.polls {
            poll.check().map_err(ApiError::Invalid)?;
            let tool_id = poll.tool_id();
            // A poll runs without the owner. It may use only a tool that reads.
            match config.policy.rules.get(&tool_id) {
                Some(rule) if rule.class == ToolClass::Read => {}
                Some(_) => {
                    return Err(ApiError::Invalid(format!(
                        "poll `{tool_id}` needs a rule with the class `read`"
                    )));
                }
                None => return Err(ApiError::Invalid(format!("poll `{tool_id}` has no permission rule"))),
            }
        }
        Ok(())
    }

    /// The settings and the exact text they were stored as. The text is the base for
    /// `save_config_from`.
    fn config_with_text(store: &ProfileStore) -> ApiResult<(String, ProfileConfig)> {
        let text = store.setting(CONFIG_KEY).map_err(store_error)?;
        let text = text.ok_or_else(|| internal("the profile has no stored config"))?;
        let config = serde_json::from_str(&text).map_err(internal)?;
        Ok((text, config))
    }

    /// Stores changed settings only if the stored settings are still the ones that were
    /// loaded. Two changes that started from the same settings cannot both be stored,
    /// so neither is lost without notice.
    fn save_config_from(store: &ProfileStore, loaded: &str, config: &ProfileConfig) -> ApiResult<()> {
        let text = serde_json::to_string(config).map_err(internal)?;
        if store
            .replace_setting_if(CONFIG_KEY, loaded, &text)
            .map_err(store_error)?
        {
            Ok(())
        } else {
            Err(ApiError::Conflict(
                "another change was saved at the same time; load the settings and try again".into(),
            ))
        }
    }

    fn save_config(store: &ProfileStore, config: &ProfileConfig) -> ApiResult<()> {
        let text = serde_json::to_string(config).map_err(internal)?;
        store.set_setting(CONFIG_KEY, &text).map_err(store_error)
    }

    async fn ctx(&self, profile: &str) -> ApiResult<Ctx> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let redactor = Self::redactor(&config)?;
        let runtime = self.runtime(profile, &config).await?;
        Ok(Ctx {
            profile: profile.to_owned(),
            store,
            config,
            runtime,
            redactor,
        })
    }

    fn audit(&self, store: &ProfileStore, kind: &str, event_key: Option<&str>, detail: Value) -> ApiResult<()> {
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
                    input_schema: spec.input_schema.clone(),
                })
            })
            .chain(Self::coding_hint(config))
            .collect()
    }

    /// The ARD tools that the settings offer: the registry tools and `resolve`.
    fn ard_hints(config: &ProfileConfig) -> Vec<ToolHint> {
        bunsin_core::ard::tool_specs(&config.ard_registries, &config.ard_domains)
            .into_iter()
            .map(|spec| ToolHint {
                tool_id: format!("{ARD_CONNECTOR}/{}", spec.name),
                description: spec.description,
                input_schema: spec.input_schema,
            })
            .collect()
    }

    /// A tool result as text for the prompt and for the disk: filtered, and cut if it is long.
    /// One function serves both, so nothing unfiltered is stored.
    fn result_text(redactor: &Redactor, result: &Value) -> String {
        let pretty = serde_json::to_string_pretty(result).expect("a JSON value always serializes");
        let mut text = redactor.redact(&pretty, &mut RedactionCounts::new());
        if text.chars().count() > MAX_RESULT_CHARS {
            text = text.chars().take(MAX_RESULT_CHARS).collect();
            text.push_str("\n[cut: the result was longer]");
        }
        text
    }

    /// Asks the LLM what to do about an event.
    ///
    /// If the LLM asks for a tool that reads, and `allow_reads` is set, the tool runs and the
    /// LLM gets its result and is asked again. This repeats until the LLM proposes something
    /// else. An invalid reply gets one repair request. `first_read` is a read made before the request, with
    /// its filtered result: the result goes in as quoted data, like the result of a later read.
    /// Returns the outcome and every raw reply.
    async fn propose(
        &self,
        ctx: &Ctx,
        event: &InboundEvent,
        key: &str,
        allow_reads: bool,
        first_read: Option<(ReadNote, String)>,
        purpose: Purpose,
    ) -> ApiResult<(Proposed, Vec<String>)> {
        let Ctx {
            store,
            config,
            runtime,
            redactor,
            ..
        } = ctx;
        let mut tools = Self::tool_hints(config);
        tools.extend(Self::reply_hint(config, event));
        // Replay gets no notes and no corrections: they hold facts from after the replayed event,
        // and a correction can hold the real reply.
        let (corrections, notes_index) = match allow_reads {
            true => (
                Self::similar_corrections(store, event)?,
                Some(self.notes_index(store, event)?),
            ),
            false => (Vec::new(), None),
        };
        if allow_reads {
            tools.extend(notes::notes_tools());
            tools.extend(Self::ard_hints(config));
        }
        let mut counts = RedactionCounts::new();
        let mut request: CompletionRequest = assemble(
            &ContextInput {
                owner_name: &config.owner_name,
                principles: &config.principles,
                corrections: &corrections,
                knowledge: &[],
                tools: &tools,
                event,
                max_output_tokens: config.max_output_tokens,
                redactor,
                notes_index: notes_index.as_deref(),
            },
            &mut counts,
        );
        let mut reads = Vec::new();
        if let Some((note, text)) = first_read {
            let quoted = bunsin_core::context::quote_untrusted("tool-result", &text);
            request.messages[0].content.push_str(&format!(
                "\nResult of `{}`, read before this request:\n{quoted}",
                note.tool
            ));
            reads.push(note);
        }

        let mut replies = Vec::new();
        let mut pages_read: BTreeSet<String> = BTreeSet::new();
        let mut used_tokens = 0;
        let mut repaired = false;
        loop {
            // A retry would spend the same tokens again, so a spent budget ends the event.
            if used_tokens >= config.token_budget {
                let reason = format!("the token budget of {} for one event is used up", config.token_budget);
                return Ok((Proposed::Stop(reason), replies));
            }
            let llm = match &runtime.llm {
                Ok(llm) => llm,
                Err(error) => return Ok((Proposed::Retry(format!("the LLM cannot be used: {error}")), replies)),
            };
            let response = match self
                .complete(store, llm.as_ref(), Some(key), purpose, request.clone())
                .await?
            {
                Ok(response) => response,
                Err(error) => return Ok((Proposed::Retry(format!("LLM call failed: {error}")), replies)),
            };
            used_tokens += response.usage.total();
            replies.push(response.content.clone());
            match response.stop {
                StopReason::Refusal => return Ok((Proposed::Retry("the LLM refused the request".into()), replies)),
                // The same request would be cut at the same place again.
                StopReason::MaxTokens => {
                    let reason = format!(
                        "the LLM reply was cut off at the output limit of {} tokens; raise `max_output_tokens`",
                        config.max_output_tokens
                    );
                    return Ok((Proposed::Stop(reason), replies));
                }
                StopReason::End => {}
            }
            let parsed = match parse_reply(&response.content).map_err(|e| e.to_string()) {
                // A placeholder stands for removed text. Sending it to other people makes no sense.
                Ok(reply)
                    if matches!(&reply.proposal, Proposal::ToolCall { args, .. }
                        if Value::Object(args.clone()).to_string().contains(PLACEHOLDER_PREFIX)) =>
                {
                    Err(
                        "the tool arguments contain a [REDACTED:...] placeholder; the removed value is not known. \
                         Write the arguments without it, or choose `escalate`"
                            .to_owned(),
                    )
                }
                Ok(reply) => match Self::reply_misuse(event, &reply.proposal) {
                    Some(error) => Err(error),
                    None => Ok(reply),
                },
                other => other,
            };
            let (proposal, wiki_updates) = match parsed {
                Ok(reply) => (reply.proposal, reply.wiki_updates),
                Err(error) if repaired => {
                    let reason = format!("the proposal is invalid after one repair: {error}");
                    return Ok((Proposed::Retry(reason), replies));
                }
                Err(error) => {
                    repaired = true;
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
                    continue;
                }
            };

            // A proposal other than an allowed read ends the loop.
            let ready = |proposal, reads, pages_read| Proposed::Ready {
                proposal,
                reads,
                wiki_updates,
                pages_read,
            };
            let Proposal::ToolCall { tool, args, .. } = &proposal else {
                return Ok((ready(proposal, reads, pages_read), replies));
            };
            // The built-in notes tools and the ARD tools are handled here. They are not in the permission table.
            let reads_notes = tool == WIKI_READ_TOOL || tool == WIKI_SEARCH_TOOL;
            let reads_ard = Self::ard_hints(config).iter().any(|hint| hint.tool_id == *tool);
            let is_read = config
                .policy
                .rules
                .get(tool)
                .is_some_and(|rule| rule.class == ToolClass::Read);
            let allowed = allow_reads
                && (reads_notes || reads_ard || (is_read && self.decide(ctx, tool).await? == Decision::Execute));
            if !allowed {
                return Ok((ready(proposal, reads, pages_read), replies));
            }
            // The limit counts the reads of the notes and of connectors separately. An ARD tool is a
            // read of a connector: it leaves the server.
            let same_kind = reads
                .iter()
                .filter(|read| is_notes_tool(&read.tool) == reads_notes)
                .count();
            if same_kind >= MAX_READS_PER_EVENT {
                let reason = format!("the agent asked for more than {MAX_READS_PER_EVENT} reads");
                return Ok((Proposed::Stop(reason), replies));
            }
            let note = ReadNote {
                tool: tool.clone(),
                args: args.clone(),
            };
            if reads.contains(&note) {
                return Ok((
                    Proposed::Stop(format!("the agent repeated the same read of `{tool}`")),
                    replies,
                ));
            }

            let (text, ok) = if reads_notes {
                self.run_notes_tool(ctx, tool, args, &mut pages_read)?
            } else {
                let result = match split_tool_id(tool) {
                    Some((connector, name)) => match runtime.connector(connector) {
                        Ok(connector) => connector.call_tool(name, args.clone()).await.map_err(|e| e.to_string()),
                        Err(error) => Err(error),
                    },
                    None => Err(format!("`{tool}` is not a tool ID")),
                };
                // A failed read goes back to the LLM as data. It can then choose another step.
                let text = match &result {
                    Ok(value) => Self::result_text(redactor, value),
                    Err(error) => {
                        format!(
                            "The tool call failed: {}",
                            redactor.redact(error, &mut RedactionCounts::new())
                        )
                    }
                };
                store
                    .put_artifact("read_result", text.as_bytes(), self.now(), Some(key), None)
                    .map_err(store_error)?;
                (text, result.is_ok())
            };
            let detail =
                json!({"tool": tool, "args": Self::result_text(redactor, &Value::Object(args.clone())), "ok": ok});
            self.audit(store, "read", Some(key), detail)?;
            reads.push(note);

            let quoted = bunsin_core::context::quote_untrusted("tool-result", &text);
            request.messages.push(Message {
                role: Role::Assistant,
                content: response.content,
            });
            request.messages.push(Message {
                role: Role::User,
                content: format!(
                    "Result of `{tool}`:\n{quoted}\nDecide the next step. Reply with one JSON object that matches the schema."
                ),
            });
        }
    }

    async fn decide(&self, ctx: &Ctx, tool_id: &str) -> ApiResult<Decision> {
        let Ctx {
            store, config, runtime, ..
        } = ctx;
        let kill_switch = store.kill_switch().map_err(store_error)?;
        let held = store.is_held(tool_id).map_err(store_error)?;
        // The connector is asked only about tools that are in the permission table.
        let pin_ok = match split_tool_id(tool_id) {
            // The specification of a built-in tool is in the code, so it cannot change.
            Some(_) if is_coding_tool(tool_id) || tool_id == bunsin_core::inbound::REPLY_TOOL => true,
            Some((connector, tool)) if config.policy.rules.contains_key(tool_id) => {
                match (config.approved_tools.get(connector), runtime.connector(connector)) {
                    (Some(approved), Ok(live)) => match live.list_tools().await {
                        Ok(current) => pin_holds(&pin(approved), &current, tool),
                        Err(_) => false,
                    },
                    _ => false,
                }
            }
            _ => false,
        };
        Ok(config.policy.decide(tool_id, pin_ok, kill_switch, held))
    }

    /// Runs an approved item. Only the caller that moves it to `Executing` runs it.
    /// The inbox ID identifies the run. If a stop cuts the call, the next start marks it `unknown`.
    async fn execute(&self, ctx: &Ctx, item: &InboxItem) -> ApiResult<Outcome> {
        let Ctx { store, runtime, .. } = ctx;
        let InboxBody::ToolCall { tool: tool_id, args } = &item.body else {
            return Err(ApiError::NotAllowed("only a tool call can run".into()));
        };
        if !store
            .move_inbox_state(item.id, ApprovalState::Approved, ApprovalState::Executing, self.now())
            .map_err(store_error)?
        {
            return Err(ApiError::NotAllowed("the item was already handled".into()));
        }
        let key = Some(item.event_key.as_str());
        self.audit(
            store,
            "execution_started",
            key,
            json!({"inbox_id": item.id, "tool": tool_id}),
        )?;
        // A coding job runs on the server in the background. Its end is recorded later.
        if is_coding_tool(tool_id) {
            return match self.start_coding(ctx, item, args) {
                Ok(()) => Ok(Outcome::Started { inbox_id: item.id }),
                Err(error) => self.execution_failed(ctx, item, tool_id, error),
            };
        }
        // An answer to an inbound caller goes back through `get_request`, not through a connector.
        if tool_id == bunsin_core::inbound::REPLY_TOOL {
            return self.answer_request(ctx, item);
        }
        let result = match split_tool_id(tool_id) {
            Some((connector, tool)) => match runtime.connector(connector) {
                Ok(connector) => connector.call_tool(tool, args.clone()).await,
                Err(error) => return self.execution_failed(ctx, item, tool_id, error),
            },
            None => return self.execution_failed(ctx, item, tool_id, format!("`{tool_id}` is not a tool ID")),
        };
        match result {
            Ok(value) => {
                let content = Self::result_text(&ctx.redactor, &value);
                store
                    .put_artifact("tool_result", content.as_bytes(), self.now(), key, Some(item.id))
                    .map_err(store_error)?;
                store
                    .move_inbox_state(item.id, ApprovalState::Executing, ApprovalState::Executed, self.now())
                    .map_err(store_error)?;
                self.audit(store, "executed", key, json!({"inbox_id": item.id, "tool": tool_id}))?;
                self.follow_up_caller(store, &ctx.config, item)?;
                Ok(Outcome::Executed { inbox_id: item.id })
            }
            // The call may have reached its tool, so it is not run again. Only the owner resolves it.
            Err(error @ ConnectorError::Unanswered(_)) => {
                let error = error.to_string();
                store
                    .move_inbox_state(item.id, ApprovalState::Executing, ApprovalState::Unknown, self.now())
                    .map_err(store_error)?;
                let detail = json!({"inbox_id": item.id, "tool": tool_id, "error": error});
                self.audit(store, "outcome_unknown", key, detail)?;
                Ok(Outcome::ExecutionUnknown {
                    inbox_id: item.id,
                    error,
                })
            }
            Err(error) => self.execution_failed(ctx, item, tool_id, error.to_string()),
        }
    }

    /// Ends an item whose call failed.
    fn execution_failed(&self, ctx: &Ctx, item: &InboxItem, tool_id: &str, error: String) -> ApiResult<Outcome> {
        ctx.store
            .move_inbox_state(item.id, ApprovalState::Executing, ApprovalState::Failed, self.now())
            .map_err(store_error)?;
        self.audit(
            &ctx.store,
            "execution_failed",
            Some(&item.event_key),
            json!({"inbox_id": item.id, "tool": tool_id, "error": error}),
        )?;
        Ok(Outcome::ExecutionFailed {
            inbox_id: item.id,
            error,
        })
    }

    async fn handle_tool_call(
        &self,
        ctx: &Ctx,
        key: &str,
        rationale: &str,
        basis: &[ReadNote],
        tool_id: String,
        mut args: Map<String, Value>,
    ) -> ApiResult<Outcome> {
        let Ctx { store, config, .. } = ctx;
        let mut decision = self.decide(ctx, &tool_id).await?;
        // An inbound caller's question never makes a change without the owner (ARCHITECTURE 7).
        // A rule at `autonomous` holds for the owner's own events only. The intake row, not the
        // event text, tells that the event came from a caller.
        let changes = config
            .policy
            .rules
            .get(&tool_id)
            .is_some_and(|rule| rule.class != ToolClass::Read);
        if changes
            && decision == Decision::Execute
            && store
                .has_intake(bunsin_core::inbound::INBOUND_SOURCE, key)
                .map_err(store_error)?
        {
            decision = Decision::NeedsApproval;
        }
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
        let rule = config.policy.rules.get(&tool_id);
        if let Some(rule) = rule
            && let Err(error) = mark_args(&mut args, &rule.outgoing_text, &config.ai_marker)
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
        let body = InboxBody::ToolCall { tool: tool_id, args };
        let item = store
            .add_inbox_item(key, &body, rationale, basis, state, self.now())
            .map_err(store_error)?;
        match state {
            ApprovalState::Draft => Ok(Outcome::Drafted { inbox_id: item.id }),
            ApprovalState::Pending => Ok(Outcome::PendingApproval { inbox_id: item.id }),
            _ => {
                if let InboxBody::ToolCall { tool, .. } = &item.body {
                    self.record_decision(store, &item, tool, DecisionKind::Autonomous)?;
                }
                // The event is closed before the call starts. If a step after the call fails,
                // a later delivery of the event must not make the same write again.
                store.finish_event(key).map_err(store_error)?;
                self.execute(ctx, &item).await
            }
        }
    }

    /// Removes personal data from the parts of an event that are stored and sent on.
    fn redact_event(redactor: &Redactor, mut event: InboundEvent, counts: &mut RedactionCounts) -> InboundEvent {
        event.text = redactor.redact(&event.text, counts);
        // An author is often an email address.
        event.author = redactor.redact(&event.author, counts);
        event.thread = event.thread.map(|thread| redactor.redact(&thread, counts));
        event
    }

    async fn ingest_event(&self, profile: &str, event: InboundEvent) -> ApiResult<Outcome> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        // The filter runs before anything is stored, so removed text is in no table or file.
        let mut counts = RedactionCounts::new();
        let event = Self::redact_event(&Self::redactor(&config)?, event, &mut counts);
        let key = event.dedupe_key();
        let payload = serde_json::to_value(&event).map_err(internal)?;
        let start = store
            .begin_event(&key, &payload, self.now(), MAX_EVENT_ATTEMPTS)
            .map_err(store_error)?;
        let detail = json!({"source": event.source, "author": event.author, "redacted": counts});
        match start {
            EventStart::Done => return Ok(Outcome::Duplicate),
            EventStart::New => self.audit(&store, "event_received", Some(&key), detail)?,
            EventStart::Retry { attempt } => {
                self.audit(&store, "event_retry", Some(&key), json!({"attempt": attempt}))?;
            }
            EventStart::GaveUp => {
                let summary = format!(
                    "Bunsin could not handle an event from {} by {} after {MAX_EVENT_ATTEMPTS} tries. Handle it yourself.",
                    event.source, event.author
                );
                let body = InboxBody::Escalation { summary };
                let rationale = "the handling of this event failed each time";
                store
                    .add_inbox_item(&key, &body, rationale, &[], ApprovalState::Draft, self.now())
                    .map_err(store_error)?;
                self.audit(&store, "event_given_up", Some(&key), json!({}))?;
                return Ok(Outcome::Failed {
                    error: format!("the event failed {MAX_EVENT_ATTEMPTS} times"),
                });
            }
        }

        let outcome = self.handle_event(profile, &store, &key, &event).await;
        // A failure leaves the event open, so that a later delivery of it is a retry.
        if !matches!(outcome, Ok(Outcome::Failed { .. }) | Err(_)) {
            store.finish_event(&key).map_err(store_error)?;
        }
        outcome
    }

    async fn handle_event(
        &self,
        profile: &str,
        store: &ProfileStore,
        key: &str,
        event: &InboundEvent,
    ) -> ApiResult<Outcome> {
        let ctx = self.ctx(profile).await?;
        let (proposed, replies) = self.propose(&ctx, event, key, true, None, Purpose::Event).await?;
        for reply in &replies {
            // The LLM can write personal data that it made up or was told in the owner's
            // principles. The stored copy of its reply passes the filter like other text.
            let stored = ctx.redactor.redact(reply, &mut RedactionCounts::new());
            store
                .put_artifact("llm_reply", stored.as_bytes(), self.now(), Some(key), None)
                .map_err(store_error)?;
        }
        let (proposal, reads) = match proposed {
            Proposed::Ready {
                proposal,
                reads,
                wiki_updates,
                pages_read,
            } => {
                self.file_wiki_changes(&ctx, key, event, &reads, &pages_read, wiki_updates)?;
                (proposal, reads)
            }
            Proposed::Retry(error) => {
                self.audit(store, "proposal_failed", Some(key), json!({"error": error}))?;
                return Ok(Outcome::Failed { error });
            }
            // Another try would end the same way and cost the same again. The owner takes over.
            Proposed::Stop(reason) => {
                let summary = format!(
                    "Bunsin stopped working on an event from {} by {}: {reason}. Handle it yourself.",
                    event.source, event.author
                );
                let body = InboxBody::Escalation { summary };
                let rationale = "the agent could not finish this event";
                let item = store
                    .add_inbox_item(key, &body, rationale, &[], ApprovalState::Draft, self.now())
                    .map_err(store_error)?;
                self.audit(store, "event_stopped", Some(key), json!({"reason": reason}))?;
                return Ok(Outcome::Escalated { inbox_id: item.id });
            }
        };
        let content = serde_json::to_vec_pretty(&proposal).map_err(internal)?;
        store
            .put_artifact("proposal", &content, self.now(), Some(key), None)
            .map_err(store_error)?;
        self.audit(
            store,
            "proposal",
            Some(key),
            serde_json::to_value(&proposal).map_err(internal)?,
        )?;

        match proposal {
            Proposal::Ignore { .. } => Ok(Outcome::Ignored),
            Proposal::Escalate { rationale, summary } => {
                let body = InboxBody::Escalation { summary };
                let item = store
                    .add_inbox_item(key, &body, &rationale, &reads, ApprovalState::Draft, self.now())
                    .map_err(store_error)?;
                Ok(Outcome::Escalated { inbox_id: item.id })
            }
            Proposal::ToolCall { rationale, tool, args } => {
                self.handle_tool_call(&ctx, key, &rationale, &reads, tool, args).await
            }
        }
    }

    async fn approve_item(&self, profile: &str, inbox_id: i64, edit: Option<Edit>) -> ApiResult<Outcome> {
        let store = self.store(profile)?;
        let mut item = store.inbox_item(inbox_id).map_err(store_error)?;
        let (tool_id, args) = match item.body.clone() {
            InboxBody::ToolCall { tool, args } => (tool, args),
            InboxBody::WikiChange { .. } => return self.approve_wiki_change(&store, &item, edit.is_some(), false),
            InboxBody::Escalation { .. } => {
                return Err(ApiError::NotAllowed("an escalation cannot be approved".into()));
            }
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
        let rule = config.policy.rules.get(&tool_id);
        let kind = match edit {
            Some(_) => DecisionKind::Edited,
            None => DecisionKind::Approved,
        };

        if let Some(Edit {
            args: mut edited,
            reason,
        }) = edit
        {
            // The user may have removed the marker while editing. It is added again.
            if let Some(rule) = rule {
                mark_args(&mut edited, &rule.outgoing_text, &config.ai_marker)
                    .map_err(|e| ApiError::Invalid(e.to_string()))?;
            }
            // The answer that the server drafted after a caller's call ran is no draft of the agent.
            // The owner's text there tells a result, so it would teach the agent to claim one.
            let server_made = tool_id == bunsin_core::inbound::REPLY_TOOL
                && item.rationale == bunsin_core::inbound::FOLLOW_UP_RATIONALE;
            let correction = (!server_made)
                .then(|| Self::correction(&ctx, &item, &tool_id, &args, &edited, &reason))
                .transpose()?;
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
            if let Some(correction) = &correction {
                store
                    .add_correction(inbox_id, correction, self.now())
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
            }
            let reason = correction.as_ref().map(|correction| correction.reason.clone());
            self.audit(
                &store,
                "edited",
                Some(&key),
                json!({"inbox_id": inbox_id, "reason": reason, "correction": correction.is_some()}),
            )?;
            item.body = body;
        }

        // The rule may name other places of text than when the item was made. The AI marker
        // follows the rule that is in force now, so a corrected rule also covers a waiting item.
        if let (Some(rule), InboxBody::ToolCall { tool, args }) = (rule, &item.body) {
            let mut marked = args.clone();
            mark_args(&mut marked, &rule.outgoing_text, &config.ai_marker)
                .map_err(|e| ApiError::Invalid(e.to_string()))?;
            if marked != *args {
                let body = InboxBody::ToolCall {
                    tool: tool.clone(),
                    args: marked,
                };
                if !store
                    .replace_pending_body(inbox_id, &body, self.now())
                    .map_err(store_error)?
                {
                    return Err(ApiError::NotAllowed("the item was already handled".into()));
                }
                item.body = body;
            }
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
            .move_inbox_state(inbox_id, ApprovalState::Pending, ApprovalState::Approved, self.now())
            .map_err(store_error)?
        {
            return Err(ApiError::NotAllowed("the item was already handled".into()));
        }
        self.audit(&store, "approved", Some(&key), json!({"inbox_id": inbox_id}))?;
        self.record_decision(&store, &item, &tool_id, kind)?;
        self.execute(&ctx, &item).await
    }

    fn reject_item(&self, profile: &str, inbox_id: i64, reason: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        let item = store.inbox_item(inbox_id).map_err(store_error)?;
        let moved_from = [ApprovalState::Pending, ApprovalState::Draft].into_iter().find(|from| {
            store
                .move_inbox_state(inbox_id, *from, ApprovalState::Rejected, self.now())
                .unwrap_or(false)
        });
        let Some(from) = moved_from else {
            return Err(ApiError::NotAllowed(format!("the item is {}", item.state.as_str())));
        };
        self.audit(
            &store,
            "rejected",
            Some(&item.event_key),
            json!({"inbox_id": inbox_id, "reason": reason}),
        )?;
        // Only a waiting call is a review. A draft never runs, and a note or an escalation is no call.
        match (&item.body, from) {
            (InboxBody::ToolCall { tool, .. }, ApprovalState::Pending) => self.record_rejection(&store, &item, tool),
            _ => Ok(()),
        }
    }

    /// Sends one request to the saved LLM endpoint. The request has the reply schema and the output
    /// limit of a real request, because an endpoint can accept a plain request and refuse those.
    /// It holds no data of the profile. It runs with the kill switch on: it changes nothing.
    async fn test_llm_endpoint(&self, profile: &str) -> ApiResult<LlmTest> {
        let ctx = self.ctx(profile).await?;
        let request = CompletionRequest {
            system: "This is a connection test. Reply with the kind `ignore` and the rationale `connection test`."
                .into(),
            messages: vec![Message {
                role: Role::User,
                content: "Connection test.".into(),
            }],
            response_schema: Some(proposal_schema()),
            max_output_tokens: ctx.config.max_output_tokens,
        };
        let outcome = match &ctx.runtime.llm {
            Err(error) => Err(format!("the LLM cannot be used: {error}")),
            // The client of the endpoint ends the request at the timeout of the profile's settings.
            Ok(llm) => match self
                .complete(&ctx.store, llm.as_ref(), None, Purpose::LlmTest, request)
                .await?
            {
                Err(error) => Err(format!("the request failed: {error}")),
                Ok(response) => match (response.stop, parse_reply(&response.content)) {
                    (StopReason::End, Ok(_)) => Ok(format!(
                        "the endpoint answered in the form the agent needs ({} input tokens, {} output tokens)",
                        response.usage.input_tokens, response.usage.output_tokens
                    )),
                    (StopReason::End, Err(error)) => Err(format!(
                        "the endpoint answered, but not in the form the agent needs: {error}"
                    )),
                    (StopReason::MaxTokens, _) => Err(format!(
                        "the endpoint answered, but the reply was cut off at the output limit of {} tokens",
                        ctx.config.max_output_tokens
                    )),
                    (StopReason::Refusal, _) => Err("the endpoint answered, but it refused the request".to_owned()),
                },
            },
        };
        let (works, detail) = match outcome {
            Ok(detail) => (true, detail),
            Err(detail) => (false, detail),
        };
        self.audit(&ctx.store, "llm_tested", None, json!({"works": works}))?;
        Ok(LlmTest { works, detail })
    }

    async fn tools_report(&self, profile: &str, connector: &str) -> ApiResult<ToolsReport> {
        let ctx = self.ctx(profile).await?;
        if !ctx.config.connectors.contains_key(connector) && !ctx.runtime.connectors.contains_key(connector) {
            return Err(ApiError::NotFound(format!("connector `{connector}`")));
        }
        // This also tells the owner whether an address and token that were just saved work.
        let live = ctx
            .runtime
            .connector(connector)
            .map_err(ApiError::NotAllowed)?
            .list_tools()
            .await
            .map_err(|e| ApiError::NotAllowed(format!("connector `{connector}` cannot list its tools: {e}")))?;
        let approved = ctx.config.approved_tools.get(connector).cloned().unwrap_or_default();
        Ok(ToolsReport {
            diff: diff_pins(&pin(&approved), &live),
            tools: live,
        })
    }

    async fn approve_shown_tools(&self, profile: &str, connector: &str, shown: PinSet) -> ApiResult<()> {
        let live = self.tools_report(profile, connector).await?.tools;
        // The owner approves what was on screen. A list that changed meanwhile is not approved.
        if pin(&live) != shown {
            // The names tell the owner whether the server changed one tool or lists differently each time.
            let diff = diff_pins(&shown, &live);
            let part = |label: &str, names: &[String]| {
                let more = names.len().saturating_sub(5);
                let more = if more == 0 {
                    String::new()
                } else {
                    format!(" and {more} more")
                };
                let names = names.iter().take(5).map(|name| format!("`{name}`")).collect::<Vec<_>>();
                (!names.is_empty()).then(|| format!("{label}: {}{more}", names.join(", ")))
            };
            let parts = [
                part("new", &diff.added),
                part("gone", &diff.removed),
                part("changed", &diff.changed),
            ];
            let parts = parts.into_iter().flatten().collect::<Vec<_>>().join("; ");
            return Err(ApiError::NotAllowed(format!(
                "connector `{connector}` lists other tools now than the ones shown ({parts}); list them again"
            )));
        }
        let store = self.store(profile)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        let count = live.len();
        config.approved_tools.insert(connector.to_owned(), live);
        Self::save_config_from(&store, &loaded, &config)?;
        self.audit(
            &store,
            "tools_approved",
            None,
            json!({"connector": connector, "tools": count}),
        )
    }

    /// Replaces the permission table. The audit entry names each tool whose rule changed,
    /// with the rule before and after, so a change of what the agent may do can be traced.
    fn replace_permissions(&self, profile: &str, update: PermissionsUpdate) -> ApiResult<()> {
        let store = self.store(profile)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        if let Expected::Value(expected) = &update.expected
            && *expected != config.policy.rules
        {
            return Err(ApiError::Conflict(
                "the permission rules are not the ones this form loaded".into(),
            ));
        }
        let before = std::mem::replace(&mut config.policy.rules, update.rules);
        let after = &config.policy.rules;
        let changes: Vec<Value> = before
            .keys()
            .chain(after.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter(|tool| before.get(*tool) != after.get(*tool))
            .map(|tool| json!({"tool": tool, "from": before.get(tool), "to": after.get(tool)}))
            .collect();
        // A poll needs a read rule for its tool. A change that takes it away is refused here.
        Self::check_config(&config)?;
        Self::save_config_from(&store, &loaded, &config)?;
        self.audit(&store, "permissions_changed", None, json!({"changes": changes}))
    }

    fn replace_privacy(&self, profile: &str, update: PrivacyUpdate) -> ApiResult<()> {
        let store = self.store(profile)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        if let Expected::Value(expected) = &update.expected
            && *expected != config.privacy
        {
            return Err(ApiError::Conflict(
                "the privacy settings are not the ones this form loaded".into(),
            ));
        }
        config.privacy = update.privacy;
        Self::check_config(&config)?;
        Self::save_config_from(&store, &loaded, &config)?;
        // Counts only. The patterns can describe what the owner wants hidden.
        let detail = json!({"custom_patterns": config.privacy.custom.len(), "terms": config.privacy.terms.len()});
        self.audit(&store, "privacy_changed", None, detail)
    }
}

impl EngineApi for Engine {
    fn list_profiles(&self) -> BoxFuture<'_, ApiResult<Vec<ProfileInfo>>> {
        Box::pin(async move { self.app.list_profiles().map_err(store_error) })
    }

    fn profile_config<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileConfig>> {
        Box::pin(async move { Self::config(&*self.store(profile)?) })
    }

    fn set_profile_config<'a>(&'a self, profile: &'a str, config: ProfileConfig) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let store = self.store(profile)?;
            let before = Self::config(&store)?;
            let config = Self::with_reply_rule(&before, config);
            self.backends.validate(profile, &config).map_err(ApiError::Invalid)?;
            Self::check_config(&config)?;
            Self::save_config(&store, &config)?;
            self.stamp_inbound(&store, &before, &config)?;
            // A settings file can drop a secret variable, its connector or a webhook. Its value goes too.
            self.prune_secrets(&store, &config)?;
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
                polls: self.poll_statuses(&store)?,
            })
        })
    }

    fn ingest<'a>(&'a self, profile: &'a str, event: InboundEvent) -> BoxFuture<'a, ApiResult<Outcome>> {
        Box::pin(self.ingest_event(profile, event))
    }

    fn approve<'a>(&'a self, profile: &'a str, inbox_id: i64, edit: Option<Edit>) -> BoxFuture<'a, ApiResult<Outcome>> {
        Box::pin(self.approve_item(profile, inbox_id, edit))
    }

    fn reject<'a>(&'a self, profile: &'a str, inbox_id: i64, reason: &'a str) -> BoxFuture<'a, ApiResult<()>> {
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

    fn replay<'a>(&'a self, profile: &'a str, case: ReplayCase) -> BoxFuture<'a, ApiResult<ReplayResult>> {
        Box::pin(self.replay_case(profile, case))
    }

    fn connector_tools<'a>(&'a self, profile: &'a str, connector: &'a str) -> BoxFuture<'a, ApiResult<ToolsReport>> {
        Box::pin(self.tools_report(profile, connector))
    }

    fn approve_tools<'a>(
        &'a self,
        profile: &'a str,
        connector: &'a str,
        shown: PinSet,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(self.approve_shown_tools(profile, connector, shown))
    }

    fn permissions<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Permissions>> {
        Box::pin(async move {
            // The stored settings only. A look at the rules must not start a connector.
            let config = Self::config(&*self.store(profile)?)?;
            Ok(Permissions {
                tools: config.approved_tools,
                rules: config.policy.rules,
                ai_marker: config.ai_marker,
            })
        })
    }

    fn set_permissions<'a>(&'a self, profile: &'a str, update: PermissionsUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.replace_permissions(profile, update) })
    }

    fn principles<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<String>>> {
        Box::pin(async move { self.read_principles(profile) })
    }

    fn set_principles<'a>(&'a self, profile: &'a str, update: PrinciplesUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.replace_principles(profile, update) })
    }

    fn privacy<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<PrivacyConfig>> {
        Box::pin(async move { Ok(Self::config(&*self.store(profile)?)?.privacy) })
    }

    fn set_privacy<'a>(&'a self, profile: &'a str, update: PrivacyUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.replace_privacy(profile, update) })
    }

    fn polls<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<PollConfig>>> {
        Box::pin(async move { self.read_polls(profile) })
    }

    fn set_polls<'a>(&'a self, profile: &'a str, update: PollsUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.replace_polls(profile, update) })
    }

    fn test_poll<'a>(&'a self, profile: &'a str, index: usize) -> BoxFuture<'a, ApiResult<PollPreview>> {
        Box::pin(self.preview_poll(profile, index))
    }

    fn test_llm<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<LlmTest>> {
        Box::pin(self.test_llm_endpoint(profile))
    }

    fn llm_usage<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<LlmUsage>>> {
        Box::pin(async move { self.read_llm_usage(profile) })
    }

    fn resolve_unknown<'a>(
        &'a self,
        profile: &'a str,
        inbox_id: i64,
        resolution: Resolution,
    ) -> BoxFuture<'a, ApiResult<Option<Outcome>>> {
        Box::pin(self.resolve_unknown_item(profile, inbox_id, resolution))
    }

    fn autonomy<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Autonomy>> {
        Box::pin(async move { self.read_autonomy(profile) })
    }

    fn report_autonomous<'a>(&'a self, profile: &'a str, inbox_id: i64) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.report(profile, inbox_id) })
    }

    fn clear_autonomy_hold<'a>(&'a self, profile: &'a str, tool: &'a str) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.clear_hold(profile, tool) })
    }

    fn autonomy_thresholds<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Option<AutonomyThresholds>>> {
        Box::pin(async move { self.read_thresholds(profile) })
    }

    fn set_autonomy_thresholds<'a>(
        &'a self,
        profile: &'a str,
        update: AutonomyThresholdsUpdate,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_thresholds(profile, update) })
    }

    fn add_replay_cases<'a>(&'a self, profile: &'a str, cases: Vec<ReplayCase>) -> BoxFuture<'a, ApiResult<Vec<i64>>> {
        Box::pin(async move { self.add_cases(profile, cases) })
    }

    fn replay_cases<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<ReplayCaseRecord>>> {
        Box::pin(async move { self.read_cases(profile) })
    }

    fn delete_replay_case<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.delete_case(profile, id) })
    }

    fn start_eval<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<i64>> {
        Box::pin(async move { self.start_run(profile) })
    }

    fn eval_report<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<EvalReport>> {
        Box::pin(async move { self.read_eval_report(profile) })
    }

    fn extract_principles<'a>(
        &'a self,
        profile: &'a str,
        source: PrincipleSource,
    ) -> BoxFuture<'a, ApiResult<Vec<PrincipleCandidate>>> {
        Box::pin(self.extract(profile, source))
    }

    fn principle_candidates<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<PrincipleCandidate>>> {
        Box::pin(async move { self.read_candidates(profile) })
    }

    fn adopt_principle<'a>(&'a self, profile: &'a str, adoption: AdoptPrinciple) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.adopt(profile, adoption) })
    }

    fn dismiss_principle<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.dismiss(profile, id) })
    }

    fn lint_notes<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<LintFinding>>> {
        Box::pin(self.check_notes(profile))
    }

    fn lint_findings<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<LintFinding>>> {
        Box::pin(async move { self.new_findings(profile) })
    }

    fn dismiss_finding<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.dismiss_lint_finding(profile, id) })
    }

    fn preload_notes<'a>(&'a self, profile: &'a str, request: PreloadNotes) -> BoxFuture<'a, ApiResult<Vec<i64>>> {
        Box::pin(self.preload(profile, request))
    }

    fn connections<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Connections>> {
        Box::pin(async move { self.read_connections(profile) })
    }

    fn set_llm<'a>(&'a self, profile: &'a str, update: LlmUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_llm(profile, update) })
    }

    fn set_connector<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        update: ConnectorUpdate,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_connector(profile, name, update) })
    }

    fn set_command_connector<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        update: CommandConnectorUpdate,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_command_connector(profile, name, update) })
    }

    fn set_api_connector<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        update: ApiConnectorUpdate,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_api_connector(profile, name, update) })
    }

    fn remove_connector<'a>(&'a self, profile: &'a str, name: &'a str) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.delete_connector(profile, name) })
    }

    fn start_connector_oauth<'a>(
        &'a self,
        profile: &'a str,
        connector: &'a str,
        redirect_uri: &'a str,
    ) -> BoxFuture<'a, ApiResult<OAuthStarted>> {
        Box::pin(async move { self.start_oauth(profile, connector, redirect_uri).await })
    }

    fn finish_connector_oauth<'a>(
        &'a self,
        profile: &'a str,
        connector: &'a str,
        callback_url: String,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.finish_oauth(profile, connector, &callback_url).await })
    }

    fn corrections<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<CorrectionRecord>>> {
        Box::pin(async move { self.read_corrections(profile) })
    }

    fn retire_correction<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.retire(profile, id) })
    }

    fn wiki_index<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<WikiSummary>>> {
        Box::pin(async move { self.store(profile)?.wiki_index().map_err(store_error) })
    }

    fn wiki_page<'a>(&'a self, profile: &'a str, slug: &'a str) -> BoxFuture<'a, ApiResult<WikiPage>> {
        Box::pin(async move { self.read_wiki_page(profile, slug) })
    }

    fn wiki_search<'a>(&'a self, profile: &'a str, query: &'a str) -> BoxFuture<'a, ApiResult<Vec<WikiSummary>>> {
        Box::pin(async move { self.search_owner_notes(profile, query) })
    }

    fn set_wiki_page<'a>(
        &'a self,
        profile: &'a str,
        slug: &'a str,
        update: WikiPageUpdate,
    ) -> BoxFuture<'a, ApiResult<WikiPage>> {
        Box::pin(async move { self.write_owner_page(profile, slug, update) })
    }

    fn delete_wiki_page<'a>(&'a self, profile: &'a str, slug: &'a str) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.delete_owner_page(profile, slug) })
    }

    fn wiki_history<'a>(&'a self, profile: &'a str, slug: &'a str) -> BoxFuture<'a, ApiResult<Vec<Option<WikiPage>>>> {
        Box::pin(async move { self.store(profile)?.wiki_history(slug).map_err(store_error) })
    }

    fn caller_tokens<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<CallerToken>>> {
        Box::pin(async move { self.read_caller_tokens(profile) })
    }

    fn add_caller_token<'a>(&'a self, profile: &'a str, label: &'a str) -> BoxFuture<'a, ApiResult<NewCallerToken>> {
        Box::pin(async move { self.new_caller_token(profile, label) })
    }

    fn revoke_caller_token<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.revoke_caller(profile, id) })
    }

    fn inbound_status<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<InboundStatus>> {
        Box::pin(async move { self.read_inbound_status(profile) })
    }

    fn set_webhook_secret<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        secret: String,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_webhook_secret(profile, name, &secret) })
    }

    fn inbound_settings<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<InboundConfig>> {
        Box::pin(async move { self.read_inbound_settings(profile) })
    }

    fn set_inbound_settings<'a>(&'a self, profile: &'a str, update: InboundUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_inbound_settings(profile, update) })
    }

    fn ard_registries<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<ArdRegistry>>> {
        Box::pin(async move { self.read_ard_registries(profile) })
    }

    fn set_ard_registries<'a>(&'a self, profile: &'a str, update: ArdRegistriesUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_ard_registries(profile, update) })
    }

    fn webhooks<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<BTreeMap<String, WebhookConfig>>> {
        Box::pin(async move { self.read_webhooks(profile) })
    }

    fn set_webhooks<'a>(&'a self, profile: &'a str, update: WebhooksUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_webhooks(profile, update) })
    }

    fn notes_settings<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<NotesConfig>> {
        Box::pin(async move { self.read_notes_settings(profile) })
    }

    fn set_notes_settings<'a>(&'a self, profile: &'a str, update: NotesSettingsUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_notes_settings(profile, update) })
    }

    fn eval_settings<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<EvalConfig>> {
        Box::pin(async move { self.read_eval_settings(profile) })
    }

    fn set_eval_settings<'a>(&'a self, profile: &'a str, update: EvalSettingsUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move { self.save_eval_settings(profile, update) })
    }
}
