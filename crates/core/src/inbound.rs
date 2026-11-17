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
}

/// The reply of `get_request`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestState {
    pub request_id: String,
    pub status: RequestStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}

/// The status of a request from the outcome of its handling and the inbox items of its event.
/// `outcome` is `None` while the agent has not handled it. The first rule that fits wins.
pub fn request_status(outcome: Option<&str>, items: &[InboxItem]) -> (RequestStatus, Option<String>) {
    let Some(outcome) = outcome else {
        return (RequestStatus::Working, None);
    };
    if outcome == "failed" || items.iter().any(|item| item.state == ApprovalState::Failed) {
        return (RequestStatus::Failed, None);
    }
    let answer = items.iter().find_map(|item| match &item.body {
        InboxBody::ToolCall { tool, args } if tool == REPLY_TOOL && item.state == ApprovalState::Executed => {
            args.get(REPLY_TEXT).and_then(Value::as_str).map(str::to_owned)
        }
        _ => None,
    });
    if let Some(answer) = answer {
        return (RequestStatus::Answered, Some(answer));
    }
    let declined = ["ignored", "denied", "recorded"].contains(&outcome)
        || items.iter().any(|item| item.state == ApprovalState::Rejected);
    if declined {
        return (RequestStatus::Declined, None);
    }
    (RequestStatus::PendingApproval, None)
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
            updated_at: 0,
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
        let status = |outcome: Option<&str>, items: &[InboxItem]| request_status(outcome, items).0;
        assert_eq!(status(None, &[reply("hi", Executed)]), Working);
        assert_eq!(status(Some("failed"), &[]), Failed);
        // A failed item wins over an answer.
        assert_eq!(
            status(
                Some("executed"),
                &[reply("a", Executed), escalation(ApprovalState::Failed)]
            ),
            Failed
        );
        assert_eq!(
            request_status(Some("pending_approval"), &[reply("Friday.", Executed)]),
            (Answered, Some("Friday.".into()))
        );
        for outcome in ["ignored", "denied", "recorded"] {
            assert_eq!(status(Some(outcome), &[]), Declined, "{outcome}");
        }
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
