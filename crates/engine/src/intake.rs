//! Questions from inbound callers. `ask` stores the question and returns at once, because the agent
//! needs one or more LLM calls. `drain_intake` hands each stored question to the agent later.
//!
//! The answer is a call of the built-in tool `bunsin.inbound/reply`. It waits for the owner like any
//! write, gets the AI marker and keeps the owner's edit. Running it calls no connector: it marks the
//! request as answered, and `get_request` then returns the text.

use crate::{Ctx, Engine, MAX_EVENT_ATTEMPTS, internal, store_error};
use bunsin_api::{ApiError, ApiResult, Outcome, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::context::ToolHint;
use bunsin_core::event::{Channel, InboundEvent};
use bunsin_core::inbound::{
    Asked, INBOUND_SOURCE, MAX_ASKS_PER_HOUR, MAX_OPEN_REQUESTS, MAX_QUESTION_CHARS, REPLY_TEXT, REPLY_TOOL,
    RequestState, is_inbound_tool, reply_description, reply_schema, request_status,
};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::proposal::Proposal;
use bunsin_core::records::{Caller, InboxBody, InboxItem};
use bunsin_store::ProfileStore;
use serde_json::json;

/// How many stored events of one profile one drain hands to the agent.
const DRAIN_BATCH: u32 = 10;
/// The wait before the next try of a failed event, per try made.
const RETRY_STEP_SECS: i64 = 60;

impl Engine {
    /// The reply tool for an inbound event with a rule for it. Other events never see it.
    pub(crate) fn reply_hint(config: &ProfileConfig, event: &InboundEvent) -> Option<ToolHint> {
        (event.source == INBOUND_SOURCE && config.policy.rules.contains_key(REPLY_TOOL)).then(|| ToolHint {
            tool_id: REPLY_TOOL.to_owned(),
            description: reply_description(),
            input_schema: reply_schema(),
        })
    }

    /// Why a proposal must be repaired: an answer to a caller for an event that has no caller.
    pub(crate) fn reply_misuse(event: &InboundEvent, proposal: &Proposal) -> Option<String> {
        match proposal {
            Proposal::ToolCall { tool, .. } if is_inbound_tool(tool) && event.source != INBOUND_SOURCE => {
                Some(format!(
                    "`{tool}` answers only an event from `{INBOUND_SOURCE}`; choose another tool, `escalate` or `ignore`"
                ))
            }
            _ => None,
        }
    }

    /// Refuses inbound settings the card cannot show, and a rule for an inbound tool that is not a
    /// write of `text` at most at `approve`. An answer always waits for the owner.
    pub(crate) fn check_inbound(config: &ProfileConfig) -> ApiResult<()> {
        config.inbound.check().map_err(ApiError::Invalid)?;
        for (tool, rule) in &config.policy.rules {
            if !is_inbound_tool(tool) {
                continue;
            }
            if tool != REPLY_TOOL {
                return Err(ApiError::Invalid(format!("there is no built-in tool `{tool}`")));
            }
            if rule.class != ToolClass::Write || rule.outgoing_text.locations() != [REPLY_TEXT] {
                return Err(ApiError::Invalid(format!(
                    "the rule of `{tool}` must have the class `write` and the outgoing text {{\"arg\": \"{REPLY_TEXT}\"}}"
                )));
            }
            if rule.tier == Tier::Autonomous {
                return Err(ApiError::Invalid(format!(
                    "the rule of `{tool}` goes at most to `approve`: the owner approves every answer"
                )));
            }
        }
        Ok(())
    }

    /// Adds the reply rule when the settings turn the endpoint on and have no rule for it yet.
    /// A rule the owner removed later stays removed until the endpoint is turned on again.
    pub(crate) fn with_reply_rule(before: &ProfileConfig, mut config: ProfileConfig) -> ProfileConfig {
        if config.inbound.enabled && !before.inbound.enabled {
            config
                .policy
                .rules
                .entry(REPLY_TOOL.to_owned())
                .or_insert_with(|| ToolRule::write(ToolClass::Write, OutgoingText::Arg(REPLY_TEXT.to_owned())));
        }
        config
    }

    /// Stores a caller's question as an event for the agent and returns its request ID at once.
    pub fn ask(&self, caller: &Caller, question: &str) -> ApiResult<Asked> {
        let store = self.store(&caller.profile)?;
        let config = Self::config(&store)?;
        if !config.inbound.enabled {
            return Err(ApiError::NotAllowed("this agent takes no questions".into()));
        }
        let question = question.trim();
        if question.is_empty() || question.chars().count() > MAX_QUESTION_CHARS {
            return Err(ApiError::Invalid(format!(
                "a question has 1 to {MAX_QUESTION_CHARS} characters"
            )));
        }
        let now = self.now();
        // The limits count stored rows, so a restart does not reset them.
        if store
            .intake_count_by_caller(caller.id, now - 3_600)
            .map_err(store_error)?
            >= MAX_ASKS_PER_HOUR
        {
            return Err(ApiError::NotAllowed(format!(
                "a caller may ask {MAX_ASKS_PER_HOUR} questions an hour; ask again later"
            )));
        }
        if store.open_intake_count(INBOUND_SOURCE).map_err(store_error)? >= MAX_OPEN_REQUESTS {
            return Err(ApiError::NotAllowed(
                "too many questions wait for this agent; ask again later".into(),
            ));
        }
        let mut random = [0u8; 16];
        getrandom::fill(&mut random).map_err(internal)?;
        let request_id = format!("req_{}", random.iter().map(|b| format!("{b:02x}")).collect::<String>());
        let event = InboundEvent {
            source: INBOUND_SOURCE.to_owned(),
            external_id: request_id.clone(),
            thread: None,
            author: format!("caller:{}", caller.label),
            text: question.to_owned(),
            // A caller can write anything to steer the agent.
            channel: Channel::Outside,
        };
        // The filter runs before the question is stored, so removed text is in no table.
        let mut counts = RedactionCounts::new();
        let event = Self::redact_event(&Self::redactor(&config)?, event, &mut counts);
        store
            .add_intake(INBOUND_SOURCE, &request_id, Some(caller.id), &event, now)
            .map_err(store_error)?;
        let detail = json!({"request": request_id, "caller": caller.id, "redacted": counts});
        self.audit(&store, "inbound_asked", Some(&event.dedupe_key()), detail)?;
        Ok(Asked {
            request_id,
            pending_approval: true,
        })
    }

    /// The state of a request of this caller. Another caller's request and a request that does not
    /// exist give the same error.
    pub fn request(&self, caller: &Caller, request_id: &str) -> ApiResult<RequestState> {
        let store = self.store(&caller.profile)?;
        let row = store
            .caller_intake(INBOUND_SOURCE, request_id, caller.id)
            .map_err(store_error)?
            .ok_or_else(|| ApiError::NotFound(format!("request `{request_id}` of this caller")))?;
        let items = store.inbox_for_event(&row.event_key).map_err(store_error)?;
        let (status, answer) = request_status(row.outcome.as_deref(), &items);
        Ok(RequestState {
            request_id: request_id.to_owned(),
            status,
            answer,
        })
    }

    /// Hands the stored events that are due to the agent, for every profile whose kill switch is off.
    /// One failing profile does not stop the others. Run it from one task at a time.
    pub async fn drain_intake(&self) -> Vec<(String, ApiError)> {
        let mut failures = Vec::new();
        for profile in self.app.list_profiles().unwrap_or_default() {
            if let Err(error) = self.drain_profile(&profile.id, &mut failures).await {
                failures.push((profile.id, error));
            }
        }
        failures
    }

    async fn drain_profile(&self, profile: &str, failures: &mut Vec<(String, ApiError)>) -> ApiResult<()> {
        let store = self.store(profile)?;
        // With the kill switch on, the proposals would be refused and the events closed.
        if store.kill_switch().map_err(store_error)? {
            return Ok(());
        }
        for row in store.due_intake(self.now(), DRAIN_BATCH).map_err(store_error)? {
            let from_caller = row.event.source == INBOUND_SOURCE;
            let outcome = self.ingest_event(profile, row.event).await;
            match outcome {
                Ok(Outcome::Failed { .. }) | Err(_) => {
                    // A failed event stays open in `events`, so the next try is a retry there. The try after
                    // the last one gives up and puts a note into the inbox; that ends this row too.
                    let tries = row.attempts + 1;
                    if tries > MAX_EVENT_ATTEMPTS {
                        store.finish_intake(row.id, "failed").map_err(store_error)?;
                    } else {
                        store
                            .intake_tried(row.id, self.now() + RETRY_STEP_SECS * i64::from(tries))
                            .map_err(store_error)?;
                    }
                    if let Err(error) = outcome {
                        failures.push((profile.to_owned(), error));
                    }
                }
                Ok(outcome) => {
                    let word = serde_json::to_value(&outcome).map_err(internal)?;
                    let word = word["outcome"].as_str().unwrap_or("handled").to_owned();
                    // The caller sees only the owner's decisions. If the agent gave no answer, the owner
                    // decides; otherwise the LLM's choice after its reads would tell the caller one bit.
                    if from_caller && matches!(outcome, Outcome::Ignored | Outcome::Denied { .. } | Outcome::Recorded) {
                        let summary = format!(
                            "The agent gave no answer to a caller's question ({word}). Dismiss this item to \
                             decline the question."
                        );
                        store
                            .add_inbox_item(
                                &row.event_key,
                                &InboxBody::Escalation { summary },
                                "a caller waits for your decision",
                                &[],
                                ApprovalState::Draft,
                                self.now(),
                            )
                            .map_err(store_error)?;
                    }
                    store.finish_intake(row.id, &word).map_err(store_error)?;
                }
            }
        }
        Ok(())
    }

    /// Runs an approved answer that is `executing`: it marks the caller's request as answered.
    /// No connector is called. The caller reads the text with `get_request`.
    pub(crate) fn answer_request(&self, ctx: &Ctx, item: &InboxItem) -> ApiResult<Outcome> {
        let store: &ProfileStore = &ctx.store;
        if !store.has_intake(INBOUND_SOURCE, &item.event_key).map_err(store_error)? {
            let error = "no inbound request belongs to this answer".to_owned();
            return self.execution_failed(ctx, item, REPLY_TOOL, error);
        }
        store
            .move_inbox_state(item.id, ApprovalState::Executing, ApprovalState::Executed, self.now())
            .map_err(store_error)?;
        self.audit(
            store,
            "inbound_answered",
            Some(&item.event_key),
            json!({"inbox_id": item.id}),
        )?;
        Ok(Outcome::Executed { inbox_id: item.id })
    }
}
