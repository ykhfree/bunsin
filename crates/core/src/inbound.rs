//! The inbound endpoint: other people's agents ask the agent a question, and its answer waits for the owner.
//!
//! A question becomes an event from the source `bunsin.inbound`. The agent answers with the built-in
//! tool `bunsin.inbound/reply`. The owner approves every answer: the policy never runs it alone.

use crate::approval::ApprovalState;
use crate::records::{InboxBody, InboxItem};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The source of the events that callers make with `ask`.
pub const INBOUND_SOURCE: &str = "bunsin.inbound";
pub const INBOUND_TOOL_PREFIX: &str = "bunsin.inbound/";
/// The built-in tool that answers the caller of an inbound event.
pub const REPLY_TOOL: &str = "bunsin.inbound/reply";
/// The argument of the reply tool that holds the answer. It gets the AI marker.
pub const REPLY_TEXT: &str = "text";
/// The longest question, in characters.
pub const MAX_QUESTION_CHARS: usize = 4_000;
/// How many questions one caller may ask in an hour.
pub const MAX_ASKS_PER_HOUR: u32 = 20;
/// How many questions of one profile may wait for the agent at the same time.
pub const MAX_OPEN_REQUESTS: u32 = 50;
/// The longest title and description, in characters. The server card allows no more.
pub const MAX_CARD_TEXT_CHARS: usize = 100;

/// True for the IDs of built-in inbound tools. A connector name has no `.`, so no connector tool matches.
pub fn is_inbound_tool(tool: &str) -> bool {
    tool.starts_with(INBOUND_TOOL_PREFIX)
}

/// Who may reach the inbound endpoint of a profile.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundScope {
    /// Only the internal listener serves it.
    #[default]
    Internal,
    /// The internal and the public listener serve it.
    Public,
}

/// True if a listener of `listener` serves something of `scope`: the internal listener serves both
/// scopes, the public listener only `public`.
pub fn reaches(scope: InboundScope, listener: InboundScope) -> bool {
    listener == InboundScope::Internal || scope == InboundScope::Public
}

/// The settings of the inbound endpoint. It is off by default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub scope: InboundScope,
    /// A short name for the server card. `None`: the card has no title.
    #[serde(default)]
    pub title: Option<String>,
    /// What callers can ask about. The server card shows it. Required while the endpoint is on.
    #[serde(default)]
    pub description: String,
    /// The entry of the endpoint in the ARD catalogs of the listeners. `None`: not listed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ard: Option<ArdListing>,
    /// True if the listeners that serve the endpoint also serve it over A2A (JSON-RPC) with an Agent Card.
    #[serde(default)]
    pub a2a: bool,
}

/// How the endpoint is listed in `/.well-known/ard.json`. The identifier is
/// `urn:air:<publisher>:[<namespace>:]<agent_name>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArdListing {
    /// A domain that the server operator allows with `--ard-publisher-domain`.
    pub publisher: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    pub agent_name: String,
    /// Questions that the agent answers well. ARD asks for 2 to 5; another count is listed with a warning.
    #[serde(default)]
    pub representative_queries: Vec<String>,
}

impl InboundConfig {
    /// True if a listener for callers of `listener` serves this endpoint: the internal listener serves
    /// every endpoint that is on, the public listener only the public ones. The routes, the server card
    /// and the ARD catalog use this one check.
    pub fn served_on(&self, listener: InboundScope) -> bool {
        self.enabled && reaches(self.scope, listener)
    }

    /// True if a listener of `listener` serves the A2A endpoint and its Agent Card: it serves the
    /// endpoint, and the owner turned A2A on.
    pub fn a2a_served_on(&self, listener: InboundScope) -> bool {
        self.a2a && self.served_on(listener)
    }

    /// Refuses a title or a description that the server card cannot hold, and an ARD listing of an
    /// endpoint that is off.
    pub fn check(&self) -> Result<(), String> {
        if self.ard.is_some() && !self.enabled {
            return Err("`inbound.ard` lists the endpoint, so the endpoint must be on".into());
        }
        let fits = |text: &str| (1..=MAX_CARD_TEXT_CHARS).contains(&text.trim().chars().count());
        if self.enabled && !fits(&self.description) {
            return Err(format!(
                "`inbound.description` needs 1 to {MAX_CARD_TEXT_CHARS} characters while the endpoint is on"
            ));
        }
        if let Some(title) = &self.title
            && !fits(title)
        {
            return Err(format!("`inbound.title` has 1 to {MAX_CARD_TEXT_CHARS} characters"));
        }
        Ok(())
    }
}

/// What the agent sees of the reply tool.
pub fn reply_description() -> String {
    format!(
        "Answer the caller of this inbound request. Use it only for an event from `{INBOUND_SOURCE}`. \
         The owner approves every answer before the caller gets it."
    )
}

pub fn reply_schema() -> Value {
    json!({
        "type": "object",
        "properties": {REPLY_TEXT: {"type": "string", "description": "The answer for the caller."}},
        "required": [REPLY_TEXT],
        "additionalProperties": false
    })
}

/// The reply of `ask`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asked {
    /// `req_` and 32 hex digits.
    pub request_id: String,
    /// Always true: the owner approves every answer.
    pub pending_approval: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RequestStatus {
    /// The agent has not handled the question yet.
    Working,
    /// An answer or a question to the owner waits in the owner's inbox.
    PendingApproval,
    /// The owner approved an answer. The reply holds it.
    Answered,
    /// The agent or the owner decided not to answer.
    Declined,
    /// The agent could not handle the question.
    Failed,
    /// The caller canceled the question before a decision ended it.
    Canceled,
}

/// The reply of `get_request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestState {
    pub request_id: String,
    pub status: RequestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// A caller's request as the A2A endpoint shows it. It holds only the owner's decisions and the cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallerRequest {
    pub request_id: String,
    /// The context that the caller gave with the question, or else the request ID.
    pub context_id: String,
    pub status: RequestStatus,
    /// The approved answer, only for `answered`.
    pub answer: Option<String>,
    /// When the status began: the time of the decision, or the time the question arrived.
    pub status_at: i64,
}

/// What `request_status` found for a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusFound {
    pub status: RequestStatus,
    /// The approved answer, only for `answered`.
    pub answer: Option<String>,
    /// When the decision that ended the request was made: the cancel, or the last change of the
    /// deciding item. `None` while no decision ended it.
    pub decided_at: Option<i64>,
}

/// The status of a request from the outcome of its handling, the time the caller canceled it and
/// the inbox items of its event. `outcome` is `None` while the agent has not handled it.
/// The first rule that fits wins.
/// Past `working`, only the owner's decisions show. The agent's own choice (ignore, a refused
/// call, giving up) shows as `pending_approval`: a caller could make that choice depend on what
/// the agent read, and so learn it without the owner.
pub fn request_status(outcome: Option<&str>, canceled_at: Option<i64>, items: &[InboxItem]) -> StatusFound {
    let found = |status, answer, decided_at| StatusFound {
        status,
        answer,
        decided_at,
    };
    if canceled_at.is_some() {
        return found(RequestStatus::Canceled, None, canceled_at);
    }
    if outcome.is_none() {
        return found(RequestStatus::Working, None, None);
    }
    // Only an approved answer can fail in the run, so this state follows the owner's approval.
    if let Some(item) = items.iter().find(|item| item.state == ApprovalState::Failed) {
        return found(RequestStatus::Failed, None, Some(item.updated_at));
    }
    let answer = items.iter().find_map(|item| match &item.body {
        InboxBody::ToolCall { tool, args } if tool == REPLY_TOOL && item.state == ApprovalState::Executed => args
            .get(REPLY_TEXT)
            .and_then(Value::as_str)
            .map(|text| (text.to_owned(), item.updated_at)),
        _ => None,
    });
    if let Some((answer, at)) = answer {
        return found(RequestStatus::Answered, Some(answer), Some(at));
    }
    if let Some(item) = items.iter().find(|item| item.state == ApprovalState::Rejected) {
        return found(RequestStatus::Declined, None, Some(item.updated_at));
    }
    found(RequestStatus::PendingApproval, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(body: InboxBody, state: ApprovalState) -> InboxItem {
        InboxItem {
            id: 1,
            event_key: "k".into(),
            body,
            rationale: String::new(),
            basis: Vec::new(),
            state,
            created_at: 0,
            updated_at: 500,
            autonomous: false,
            retry_of: None,
        }
    }

    fn reply(text: &str, state: ApprovalState) -> InboxItem {
        let args = json!({REPLY_TEXT: text}).as_object().unwrap().clone();
        item(
            InboxBody::ToolCall {
                tool: REPLY_TOOL.into(),
                args,
            },
            state,
        )
    }

    fn escalation(state: ApprovalState) -> InboxItem {
        item(
            InboxBody::Escalation {
                summary: "decide".into(),
            },
            state,
        )
    }

    #[test]
    fn the_status_follows_the_first_rule_that_fits() {
        use ApprovalState::{Draft, Executed, Pending, Rejected};
        use RequestStatus::*;
        let status = |outcome: Option<&str>, items: &[InboxItem]| request_status(outcome, None, items).status;
        assert_eq!(status(None, &[reply("hi", Executed)]), Working);
        // The agent's own choices do not show: giving up, ignoring, a refused or recorded call.
        for outcome in ["failed", "ignored", "denied", "recorded"] {
            assert_eq!(status(Some(outcome), &[]), PendingApproval, "{outcome}");
            assert_eq!(
                status(Some(outcome), &[escalation(Draft)]),
                PendingApproval,
                "{outcome}"
            );
            // The owner dismisses the item: now the caller sees the decision.
            assert_eq!(status(Some(outcome), &[escalation(Rejected)]), Declined, "{outcome}");
        }
        // A failed item wins over an answer.
        assert_eq!(
            status(
                Some("executed"),
                &[reply("a", Executed), escalation(ApprovalState::Failed)]
            ),
            Failed
        );
        assert_eq!(
            request_status(Some("pending_approval"), None, &[reply("Friday.", Executed)]),
            StatusFound {
                status: Answered,
                answer: Some("Friday.".into()),
                decided_at: Some(500)
            }
        );
        assert_eq!(status(Some("pending_approval"), &[reply("a", Rejected)]), Declined);
        assert_eq!(
            status(Some("pending_approval"), &[reply("a", Pending)]),
            PendingApproval
        );
        assert_eq!(status(Some("escalated"), &[escalation(Draft)]), PendingApproval);
        // An answer of another tool is no answer.
        let other = item(
            InboxBody::ToolCall {
                tool: "slack/post".into(),
                args: json!({"text": "x"}).as_object().unwrap().clone(),
            },
            Executed,
        );
        assert_eq!(status(Some("executed"), &[other]), PendingApproval);
    }

    #[test]
    fn only_a_decision_has_a_time_and_it_is_the_time_of_the_deciding_item() {
        use ApprovalState::{Draft, Executed, Pending, Rejected};
        let at = |outcome: Option<&str>, items: &[InboxItem]| request_status(outcome, None, items).decided_at;
        let mut late = escalation(Rejected);
        late.updated_at = 900;
        // The agent's work changes items, but no time shows until a decision.
        assert_eq!(at(None, &[]), None);
        assert_eq!(
            at(Some("pending_approval"), &[reply("a", Pending), escalation(Draft)]),
            None
        );
        assert_eq!(at(Some("ignored"), &[late.clone()]), Some(900));
        // The executed answer decides, not a later rejected item.
        assert_eq!(at(Some("executed"), &[reply("a", Executed), late.clone()]), Some(500));
        let mut failed = escalation(ApprovalState::Failed);
        failed.updated_at = 700;
        assert_eq!(at(Some("executed"), &[late, failed]), Some(700));
    }

    #[test]
    fn a_canceled_request_is_canceled_whatever_its_items() {
        use ApprovalState::{Draft, Executed, Failed, Pending, Rejected};
        let items = [
            vec![],
            vec![reply("Friday.", Executed)],
            vec![escalation(Failed)],
            vec![reply("No.", Rejected)],
            vec![reply("a", Pending), escalation(Draft)],
        ];
        for outcome in [None, Some("executed"), Some("ignored")] {
            for list in &items {
                let found = request_status(outcome, Some(42), list);
                let canceled = StatusFound {
                    status: RequestStatus::Canceled,
                    answer: None,
                    decided_at: Some(42),
                };
                assert_eq!(found, canceled, "{outcome:?} {list:?}");
            }
        }
        assert_eq!(serde_json::to_value(RequestStatus::Canceled).unwrap(), "canceled");
    }

    #[test]
    fn an_endpoint_that_is_on_needs_a_description_that_fits_the_card() {
        let mut config = InboundConfig::default();
        assert_eq!(config.check(), Ok(()));
        config.enabled = true;
        assert!(config.check().unwrap_err().contains("inbound.description"));
        config.description = "Questions about the deploy schedule.".into();
        assert_eq!(config.check(), Ok(()));
        config.description = "x".repeat(MAX_CARD_TEXT_CHARS + 1);
        assert!(config.check().is_err());
        config.description = "ok".into();
        config.title = Some("  ".into());
        assert!(config.check().unwrap_err().contains("inbound.title"));
        // Settings from before this field load with the endpoint off.
        let old: InboundConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(old, InboundConfig::default());
        assert_eq!(old.scope, InboundScope::Internal);
    }

    #[test]
    fn the_internal_listener_serves_every_endpoint_that_is_on_and_the_public_one_only_public_ones() {
        use InboundScope::{Internal, Public};
        let config = |enabled, scope| InboundConfig {
            enabled,
            scope,
            ..InboundConfig::default()
        };
        assert!(config(true, Internal).served_on(Internal));
        assert!(!config(true, Internal).served_on(Public));
        assert!(config(true, Public).served_on(Internal));
        assert!(config(true, Public).served_on(Public));
        assert!(!config(false, Public).served_on(Internal));
        assert!(!config(false, Public).served_on(Public));
    }

    #[test]
    fn a2a_is_served_only_where_the_endpoint_is_and_only_when_turned_on() {
        use InboundScope::{Internal, Public};
        let config = |enabled, scope, a2a| InboundConfig {
            enabled,
            scope,
            a2a,
            ..InboundConfig::default()
        };
        assert!(config(true, Public, true).a2a_served_on(Public));
        assert!(config(true, Internal, true).a2a_served_on(Internal));
        assert!(!config(true, Internal, true).a2a_served_on(Public));
        assert!(!config(true, Public, false).a2a_served_on(Internal));
        assert!(!config(false, Public, true).a2a_served_on(Internal));
        // Settings from before this field load with A2A off.
        let old: InboundConfig = serde_json::from_str(r#"{"enabled": true, "description": "x"}"#).unwrap();
        assert!(!old.a2a);
    }

    #[test]
    fn an_ard_listing_needs_the_endpoint_on() {
        let mut config = InboundConfig {
            description: "Questions about the deploy schedule.".into(),
            ard: Some(ArdListing {
                publisher: "acme.com".into(),
                agent_name: "kim".into(),
                ..ArdListing::default()
            }),
            ..InboundConfig::default()
        };
        assert!(config.check().unwrap_err().contains("inbound.ard"));
        config.enabled = true;
        assert_eq!(config.check(), Ok(()));
        // A listing from the settings file with an unknown field is refused.
        let wrong = r#"{"publisher": "acme.com", "agent_name": "kim", "url": "https://x"}"#;
        assert!(serde_json::from_str::<ArdListing>(wrong).is_err());
    }

    #[test]
    fn only_built_in_ids_are_inbound_tools() {
        assert!(is_inbound_tool(REPLY_TOOL));
        assert!(is_inbound_tool("bunsin.inbound/other"));
        assert!(!is_inbound_tool("inbound/reply"));
        assert!(!is_inbound_tool("bunsin.coding/start"));
    }
}
