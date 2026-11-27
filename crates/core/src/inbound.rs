//! The inbound endpoint: other people's agents ask the agent a question, and its answer waits for the owner.
//!
//! A question becomes an event from the source `bunsin.inbound`. The agent answers with the built-in
//! tool `bunsin.inbound/reply`. The owner approves every answer: the policy never runs it alone.

use crate::approval::ApprovalState;
use crate::records::{InboxBody, InboxItem};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The source of the events that callers make with `ask`.
pub const INBOUND_SOURCE: &str = "bunsin.inbound";
pub const INBOUND_TOOL_PREFIX: &str = "bunsin.inbound/";
/// The built-in tool that answers the caller of an inbound event.
pub const REPLY_TOOL: &str = "bunsin.inbound/reply";
/// The argument of the reply tool that holds the answer. It gets the AI marker.
pub const REPLY_TEXT: &str = "text";
/// The text of the answer that the server drafts after a call for a caller's request ran.
/// The owner edits it to tell the result: the result is read data, so the server does not put it in.
pub const FOLLOW_UP_TEXT: &str = "Done.";
/// The rationale of that answer. It also marks the item, so that an edit of it is no correction of the agent.
pub const FOLLOW_UP_RATIONALE: &str =
    "a call for a caller's request ran; edit this answer to tell the caller the result, or dismiss it to decline";
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
    /// The skills of the Agent Card, in this order. Empty: the card has the built-in skill `ask`.
    /// They describe the endpoint only: every message takes the same path, whatever skill it names.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<InboundSkill>,
}

/// One skill of the Agent Card (A2A §4.4.5, `AgentSkill`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InboundSkill {
    /// 1 to `MAX_SKILL_ID_CHARS` of a-z, A-Z, 0-9, `.`, `_` and `-`. Unique in the card.
    pub id: String,
    pub name: String,
    pub description: String,
    /// 1 to `MAX_ARD_TAGS` tags, each 1 to `MAX_ARD_TAG_CHARS` characters on one line. A2A requires one.
    pub tags: Vec<String>,
    /// Example requests: 0 to `MAX_SKILL_EXAMPLES`, each 1 to `MAX_SKILL_EXAMPLE_CHARS` characters on one line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
}

/// The most skills of a card.
pub const MAX_SKILLS: usize = 10;
pub const MAX_SKILL_ID_CHARS: usize = 64;
pub const MAX_SKILL_NAME_CHARS: usize = 100;
pub const MAX_SKILL_DESCRIPTION_CHARS: usize = 1_000;
pub const MAX_SKILL_EXAMPLES: usize = 10;
pub const MAX_SKILL_EXAMPLE_CHARS: usize = 500;

/// True if `text`, without the space around it, has `min..=max` characters on one line.
fn one_line(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.trim().chars().count()) && !text.contains(['\n', '\r'])
}

/// Refuses tags outside the limits. `field` names the list in the error.
fn check_tags(tags: &[String], min: usize, field: &str) -> Result<(), String> {
    if tags.len() > MAX_ARD_TAGS {
        return Err(format!("`{field}` has at most {MAX_ARD_TAGS} tags"));
    }
    if tags.len() < min {
        return Err(format!("`{field}` needs at least {min} tag"));
    }
    match tags.iter().find(|tag| !one_line(tag, 1, MAX_ARD_TAG_CHARS)) {
        Some(tag) => Err(format!(
            "the tag `{tag}` of `{field}` needs 1 to {MAX_ARD_TAG_CHARS} characters on one line"
        )),
        None => Ok(()),
    }
}

impl InboundSkill {
    /// Refuses a skill that breaks a limit. `n` is its position in `inbound.skills`.
    fn check(&self, n: usize) -> Result<(), String> {
        let field = |name: &str| format!("`inbound.skills[{n}].{name}`");
        let id_fits = (1..=MAX_SKILL_ID_CHARS).contains(&self.id.len())
            && self
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !id_fits {
            return Err(format!(
                "{} needs 1 to {MAX_SKILL_ID_CHARS} of a-z, A-Z, 0-9, `.`, `_` and `-`",
                field("id")
            ));
        }
        if !one_line(&self.name, 1, MAX_SKILL_NAME_CHARS) {
            return Err(format!(
                "{} needs 1 to {MAX_SKILL_NAME_CHARS} characters on one line",
                field("name")
            ));
        }
        if !(1..=MAX_SKILL_DESCRIPTION_CHARS).contains(&self.description.trim().chars().count()) {
            return Err(format!(
                "{} needs 1 to {MAX_SKILL_DESCRIPTION_CHARS} characters",
                field("description")
            ));
        }
        check_tags(&self.tags, 1, &format!("inbound.skills[{n}].tags"))?;
        if self.examples.len() > MAX_SKILL_EXAMPLES {
            return Err(format!(
                "{} has at most {MAX_SKILL_EXAMPLES} examples",
                field("examples")
            ));
        }
        match self.examples.iter().find(|e| !one_line(e, 1, MAX_SKILL_EXAMPLE_CHARS)) {
            Some(example) => Err(format!(
                "the example `{example}` of {} needs 1 to {MAX_SKILL_EXAMPLE_CHARS} characters on one line",
                field("examples")
            )),
            None => Ok(()),
        }
    }
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
    /// ARD `tags`: keywords for filtering. 0 to `MAX_ARD_TAGS`, each 1 to `MAX_ARD_TAG_CHARS` characters on one line.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The ARD `trustManifest` (§4.5), with the member names of ARD. Bunsin does not check its signature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_manifest: Option<Map<String, Value>>,
}

/// The most tags of an ARD listing.
pub const MAX_ARD_TAGS: usize = 10;
/// The longest tag, in characters.
pub const MAX_ARD_TAG_CHARS: usize = 40;
/// The largest trust manifest, in bytes of its JSON text.
pub const MAX_TRUST_MANIFEST_BYTES: usize = 16 * 1024;

impl ArdListing {
    /// Refuses tags outside the limits and a trust manifest that does not have the ARD shapes.
    pub fn check(&self) -> Result<(), String> {
        check_tags(&self.tags, 0, "inbound.ard.tags")?;
        match &self.trust_manifest {
            Some(manifest) => check_trust_manifest(manifest)
                .map_err(|why| format!("`inbound.ard.trust_manifest` is not an ARD trust manifest: {why}")),
            None => Ok(()),
        }
    }
}

/// Checks the members of a trust manifest that ARD names, by the shapes of `ard.cddl` and the
/// JSON schema. Other members can hold anything (§4.5). The size limit is Bunsin's.
pub fn check_trust_manifest(manifest: &Map<String, Value>) -> Result<(), String> {
    let size = serde_json::to_string(manifest).map_or(usize::MAX, |text| text.len());
    if size > MAX_TRUST_MANIFEST_BYTES {
        return Err(format!("it has more than {MAX_TRUST_MANIFEST_BYTES} bytes"));
    }
    match manifest.get("identity") {
        Some(Value::String(identity)) if !identity.trim().is_empty() => {}
        _ => return Err("`identity` must be a text that is not empty".into()),
    }
    check_members(
        manifest,
        "",
        &[("identityType", Shape::Text), ("signature", Shape::Text)],
    )?;
    if let Some(schema) = manifest.get("trustSchema") {
        let Value::Object(schema) = schema else {
            return Err("`trustSchema` must be an object".into());
        };
        let shapes = [
            ("identifier", Shape::Text),
            ("version", Shape::Text),
            ("governanceUri", Shape::Uri),
            ("verificationMethods", Shape::Texts),
        ];
        check_members(schema, "trustSchema.", &shapes)?;
    }
    let attestation = [
        ("type", Shape::Text),
        ("uri", Shape::Uri),
        ("mediaType", Shape::Text),
        ("digest", Shape::Text),
    ];
    check_list(manifest, "attestations", &["type", "uri"], &attestation)?;
    let link = [
        ("relation", Shape::Text),
        ("sourceId", Shape::Text),
        ("sourceDigest", Shape::Text),
    ];
    check_list(manifest, "provenance", &["relation", "sourceId"], &link)
}

/// Checks the array member `name`, if present: each item is an object with the `required` members,
/// and its members have the `shapes`.
fn check_list(
    manifest: &Map<String, Value>,
    name: &str,
    required: &[&str],
    shapes: &[(&str, Shape)],
) -> Result<(), String> {
    let Some(list) = manifest.get(name) else {
        return Ok(());
    };
    let Value::Array(items) = list else {
        return Err(format!("`{name}` must be an array"));
    };
    for (index, item) in items.iter().enumerate() {
        let path = format!("{name}[{index}].");
        let Value::Object(item) = item else {
            return Err(format!("`{name}[{index}]` must be an object"));
        };
        if let Some(missing) = required.iter().find(|key| !item.contains_key(**key)) {
            return Err(format!("`{path}{missing}` is required"));
        }
        check_members(item, &path, shapes)?;
    }
    Ok(())
}

/// The value shapes that `check_trust_manifest` checks.
#[derive(Clone, Copy)]
enum Shape {
    Text,
    /// A text that is an absolute URI (schema `format: uri`).
    Uri,
    /// An array of texts.
    Texts,
}

/// Checks the members of `object` that `shapes` names and that are present.
fn check_members(object: &Map<String, Value>, path: &str, shapes: &[(&str, Shape)]) -> Result<(), String> {
    for (key, shape) in shapes {
        let Some(value) = object.get(*key) else { continue };
        let fits = match shape {
            Shape::Text => value.is_string(),
            Shape::Uri => value.as_str().is_some_and(is_absolute_uri),
            Shape::Texts => value.as_array().is_some_and(|items| items.iter().all(Value::is_string)),
        };
        if !fits {
            let what = match shape {
                Shape::Text => "a text",
                Shape::Uri => "an absolute URI",
                Shape::Texts => "an array of texts",
            };
            return Err(format!("`{path}{key}` must be {what}"));
        }
    }
    Ok(())
}

/// True for `scheme:rest` by the scheme rule of RFC 3986, with a rest that is not empty and has
/// no space or control character. No URL crate is needed for this.
fn is_absolute_uri(text: &str) -> bool {
    let Some((scheme, rest)) = text.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        && !rest.is_empty()
        && !rest.chars().any(|c| c.is_whitespace() || c.is_control())
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

    /// Refuses a title or a description that the server card cannot hold, an ARD listing of an
    /// endpoint that is off, an ARD listing that `ArdListing::check` refuses, and skills that break
    /// a limit or share an ID.
    pub fn check(&self) -> Result<(), String> {
        if let Some(ard) = &self.ard {
            if !self.enabled {
                return Err("`inbound.ard` lists the endpoint, so the endpoint must be on".into());
            }
            ard.check()?;
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
        if self.skills.len() > MAX_SKILLS {
            return Err(format!("`inbound.skills` has at most {MAX_SKILLS} skills"));
        }
        for (n, skill) in self.skills.iter().enumerate() {
            skill.check(n)?;
            if self.skills[..n].iter().any(|earlier| earlier.id == skill.id) {
                return Err(format!("two skills of `inbound.skills` have the ID `{}`", skill.id));
            }
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
    /// The agent could not handle the question, or an approved answer or action failed.
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
    // Only an approved call can fail in the run: the answer, or a write the owner approved for the
    // request. So this state follows the owner's approval.
    // A failed item with a retry copy (`retry_of`) is not the end: the copy decides.
    let retried = |failed: &InboxItem| items.iter().any(|copy| copy.retry_of == Some(failed.id));
    if let Some(item) = items
        .iter()
        .find(|item| item.state == ApprovalState::Failed && !retried(item))
    {
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
    // An answer that still waits for the owner wins over another dismissed item, such as the note
    // that a coding job ended. Otherwise the caller would stop at `declined` before the answer.
    let open_answer = items.iter().any(|item| {
        matches!(&item.body, InboxBody::ToolCall { tool, .. } if tool == REPLY_TOOL)
            && matches!(
                item.state,
                ApprovalState::Pending | ApprovalState::Approved | ApprovalState::Executing | ApprovalState::Unknown
            )
    });
    if open_answer {
        return found(RequestStatus::PendingApproval, None, None);
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
        assert_eq!(status(Some("executed"), std::slice::from_ref(&other)), PendingApproval);
        // An answer that waits wins over a dismissed note, such as the end of a coding job.
        let waiting = [other.clone(), escalation(Rejected), reply("Done.", Pending)];
        assert_eq!(status(Some("executed"), &waiting), PendingApproval);
        // A dismissed or a drafted answer does not wait: the dismissal decides.
        for state in [Rejected, Draft] {
            assert_eq!(
                status(
                    Some("executed"),
                    &[other.clone(), escalation(Rejected), reply("x", state)]
                ),
                Declined,
                "{state:?}"
            );
        }
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
    fn a_failed_answer_with_a_retry_copy_follows_the_copy() {
        use ApprovalState::{Executed, Failed, Pending};
        let status = |items: &[InboxItem]| request_status(Some("pending_approval"), None, items);
        let failed = reply("Friday.", Failed);
        let copy = |state| InboxItem {
            id: 2,
            retry_of: Some(failed.id),
            ..reply("Friday.", state)
        };
        // The owner chose to retry: the copy waits, so the request is not over.
        assert_eq!(
            status(&[failed.clone(), copy(Pending)]).status,
            RequestStatus::PendingApproval
        );
        // The copy ran: the caller gets the answer.
        let done = status(&[failed.clone(), copy(Executed)]);
        assert_eq!(
            (done.status, done.answer.as_deref()),
            (RequestStatus::Answered, Some("Friday."))
        );
        // A copy of another item does not hide this failure.
        let other = InboxItem {
            id: 3,
            retry_of: Some(99),
            ..reply("x", Pending)
        };
        assert_eq!(status(&[failed, other]).status, RequestStatus::Failed);
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

    fn manifest(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn a_trust_manifest_needs_an_identity_and_the_ard_member_shapes() {
        // Every member that ARD names, in its shape, and one member that ARD does not name.
        let full = manifest(json!({
            "identity": "spiffe://acme.com/ns/agents/kim",
            "identityType": "spiffe",
            "trustSchema": {"identifier": "acme-trust", "version": "1", "governanceUri": "https://acme.com/trust",
                            "verificationMethods": ["x509"], "acme:extra": 1},
            "attestations": [{"type": "SOC2-Type2", "uri": "https://acme.com/soc2.pdf", "mediaType": "application/pdf",
                              "digest": "sha256:ab"}],
            "provenance": [{"relation": "derivedFrom", "sourceId": "urn:air:acme.com:base", "sourceDigest": "sha256:cd"}],
            "signature": "eyJhbGciOi",
            "acme:level": {"any": ["shape"]}
        }));
        assert_eq!(check_trust_manifest(&full), Ok(()));
        assert_eq!(
            check_trust_manifest(&manifest(json!({"identity": "did:web:acme.com"}))),
            Ok(())
        );
        // An empty list holds no item to check.
        assert_eq!(
            check_trust_manifest(&manifest(json!({"identity": "x", "attestations": []}))),
            Ok(())
        );

        let refused = |value: Value| check_trust_manifest(&manifest(value)).unwrap_err();
        assert!(refused(json!({})).contains("`identity`"));
        assert!(refused(json!({"identity": "  "})).contains("`identity`"));
        assert!(refused(json!({"identity": 7})).contains("`identity`"));
        for (value, member) in [
            (json!({"identity": "x", "identityType": 1}), "`identityType`"),
            (json!({"identity": "x", "signature": ["a"]}), "`signature`"),
            (json!({"identity": "x", "trustSchema": "acme"}), "`trustSchema`"),
            (
                json!({"identity": "x", "trustSchema": {"governanceUri": "/trust"}}),
                "`trustSchema.governanceUri`",
            ),
            (
                json!({"identity": "x", "trustSchema": {"verificationMethods": ["a", 1]}}),
                "`trustSchema.verificationMethods`",
            ),
            (
                json!({"identity": "x", "attestations": {"type": "a"}}),
                "`attestations`",
            ),
            (json!({"identity": "x", "attestations": ["SOC2"]}), "`attestations[0]`"),
            (
                json!({"identity": "x", "attestations": [{"type": "SOC2"}]}),
                "`attestations[0].uri` is required",
            ),
            (
                json!({"identity": "x", "attestations": [{"type": "a", "uri": "https://a.com/x"}, {"type": "b", "uri": "not a uri"}]}),
                "`attestations[1].uri`",
            ),
            (
                json!({"identity": "x", "attestations": [{"type": 1, "uri": "https://a.com/x"}]}),
                "`attestations[0].type`",
            ),
            (
                json!({"identity": "x", "provenance": [{"relation": "derivedFrom"}]}),
                "`provenance[0].sourceId` is required",
            ),
            (
                json!({"identity": "x", "provenance": [{"relation": "r", "sourceId": "s", "sourceDigest": 2}]}),
                "`provenance[0].sourceDigest`",
            ),
        ] {
            let why = refused(value.clone());
            assert!(why.contains(member), "{value}: {why}");
        }
        // 16 KiB of JSON text is the most.
        let room = MAX_TRUST_MANIFEST_BYTES - r#"{"identity":"x","note":""}"#.len();
        assert_eq!(
            check_trust_manifest(&manifest(json!({"identity": "x", "note": "n".repeat(room)}))),
            Ok(())
        );
        assert!(refused(json!({"identity": "x", "note": "n".repeat(room + 1)})).contains("bytes"));
    }

    #[test]
    fn an_absolute_uri_has_a_scheme_and_a_rest() {
        for good in [
            "https://acme.com/a",
            "urn:air:acme.com:kim",
            "did:web:acme.com",
            "a+b.c-d:x",
        ] {
            assert!(is_absolute_uri(good), "{good}");
        }
        for bad in [
            "",
            "acme.com",
            "/a/b",
            ":x",
            "1http://a",
            "https://a b",
            "https:",
            "ht tp://a",
        ] {
            assert!(!is_absolute_uri(bad), "{bad}");
        }
    }

    #[test]
    fn tags_and_the_trust_manifest_are_checked_with_the_listing() {
        let config = |ard: ArdListing| InboundConfig {
            enabled: true,
            description: "Questions about the deploy schedule.".into(),
            ard: Some(ard),
            ..InboundConfig::default()
        };
        let listing = |tags: Vec<String>| ArdListing {
            publisher: "acme.com".into(),
            agent_name: "kim".into(),
            tags,
            ..ArdListing::default()
        };
        let ten: Vec<String> = (0..MAX_ARD_TAGS).map(|i| format!("tag{i}")).collect();
        assert_eq!(config(listing(ten.clone())).check(), Ok(()));
        assert_eq!(config(listing(vec!["x".repeat(MAX_ARD_TAG_CHARS)])).check(), Ok(()));
        let mut eleven = ten;
        eleven.push("one more".into());
        for tags in [
            eleven,
            vec![String::new()],
            vec!["   ".into()],
            vec!["x".repeat(MAX_ARD_TAG_CHARS + 1)],
            vec!["two\nlines".into()],
        ] {
            let error = config(listing(tags.clone())).check().unwrap_err();
            assert!(error.contains("inbound.ard.tags"), "{tags:?}: {error}");
        }
        let mut bad = listing(Vec::new());
        bad.trust_manifest = Some(manifest(json!({"identityType": "spiffe"})));
        let error = config(bad).check().unwrap_err();
        assert!(error.contains("inbound.ard.trust_manifest"), "{error}");
        // A listing from before these fields loads without them, and writes no empty fields back.
        let old: ArdListing = serde_json::from_str(r#"{"publisher": "acme.com", "agent_name": "kim"}"#).unwrap();
        assert_eq!((old.tags.len(), &old.trust_manifest), (0, &None));
        let text = serde_json::to_string(&old).unwrap();
        assert!(!text.contains("tags") && !text.contains("trust_manifest"), "{text}");
    }

    fn skill(id: &str) -> InboundSkill {
        InboundSkill {
            id: id.into(),
            name: "File a ticket".into(),
            description: "Opens a Jira ticket. The owner approves it first.".into(),
            tags: vec!["jira".into()],
            examples: vec!["Open a ticket for the login bug".into()],
        }
    }

    fn with_skills(skills: Vec<InboundSkill>) -> InboundConfig {
        InboundConfig {
            skills,
            ..InboundConfig::default()
        }
    }

    /// One change to a valid skill.
    type Change = fn(&mut InboundSkill);

    #[test]
    fn skills_are_checked_at_each_limit() {
        let ten: Vec<InboundSkill> = (0..MAX_SKILLS).map(|i| skill(&format!("s{i}"))).collect();
        assert_eq!(with_skills(ten.clone()).check(), Ok(()));
        let mut eleven = ten;
        eleven.push(skill("one-more"));
        assert!(with_skills(eleven).check().unwrap_err().contains("at most 10 skills"));

        // Each change is made to one valid skill; the error names the field.
        let edge = |change: Change| {
            let mut one = skill("a.b_c-D9");
            change(&mut one);
            with_skills(vec![one]).check()
        };
        let fits: [Change; 6] = [
            |s| s.id = "x".repeat(MAX_SKILL_ID_CHARS),
            |s| s.name = "n".repeat(MAX_SKILL_NAME_CHARS),
            |s| s.description = "two\nlines are fine".into(),
            |s| s.tags = (0..MAX_ARD_TAGS).map(|i| format!("t{i}")).collect(),
            |s| s.examples = (0..MAX_SKILL_EXAMPLES).map(|i| format!("e{i}")).collect(),
            |s| s.examples = Vec::new(),
        ];
        for change in fits {
            assert_eq!(edge(change), Ok(()));
        }
        let refused: [(Change, &str); 13] = [
            (|s| s.id = String::new(), "skills[0].id"),
            (|s| s.id = "x".repeat(MAX_SKILL_ID_CHARS + 1), "skills[0].id"),
            (|s| s.id = "has space".into(), "skills[0].id"),
            (|s| s.id = "슬랙".into(), "skills[0].id"),
            (|s| s.name = "  ".into(), "skills[0].name"),
            (|s| s.name = "n".repeat(MAX_SKILL_NAME_CHARS + 1), "skills[0].name"),
            (|s| s.name = "two\nlines".into(), "skills[0].name"),
            (|s| s.description = String::new(), "skills[0].description"),
            (
                |s| s.description = "d".repeat(MAX_SKILL_DESCRIPTION_CHARS + 1),
                "skills[0].description",
            ),
            // A2A requires a tag.
            (|s| s.tags = Vec::new(), "at least 1 tag"),
            (|s| s.tags = vec!["two\nlines".into()], "skills[0].tags"),
            (
                |s| s.examples = (0..=MAX_SKILL_EXAMPLES).map(|i| format!("e{i}")).collect(),
                "skills[0].examples",
            ),
            (
                |s| s.examples = vec!["e".repeat(MAX_SKILL_EXAMPLE_CHARS + 1)],
                "skills[0].examples",
            ),
        ];
        for (change, field) in refused {
            let error = edge(change).unwrap_err();
            assert!(error.contains(field), "{field}: {error}");
        }
    }

    #[test]
    fn two_skills_may_not_share_an_id_and_the_error_names_the_later_one() {
        assert_eq!(with_skills(vec![skill("a"), skill("b")]).check(), Ok(()));
        let error = with_skills(vec![skill("a"), skill("b"), skill("a")])
            .check()
            .unwrap_err();
        assert!(error.contains("the ID `a`"), "{error}");
        // A bad second skill names its own position.
        let mut bad = skill("b");
        bad.name = String::new();
        assert!(
            with_skills(vec![skill("a"), bad])
                .check()
                .unwrap_err()
                .contains("skills[1].name")
        );
    }

    #[test]
    fn settings_without_skills_load_and_write_no_skills_back() {
        let old: InboundConfig = serde_json::from_str(r#"{"enabled": true, "description": "x", "a2a": true}"#).unwrap();
        assert!(old.skills.is_empty());
        assert!(!serde_json::to_string(&old).unwrap().contains("skills"));
        // A skill with an unknown field is refused, and examples may be left out.
        let unknown = r#"{"id": "a", "name": "A", "description": "d", "tags": ["t"], "input_modes": []}"#;
        assert!(serde_json::from_str::<InboundSkill>(unknown).is_err());
        let short: InboundSkill =
            serde_json::from_str(r#"{"id": "a", "name": "A", "description": "d", "tags": ["t"]}"#).unwrap();
        assert!(short.examples.is_empty());
        assert!(!serde_json::to_string(&short).unwrap().contains("examples"));
    }

    #[test]
    fn only_built_in_ids_are_inbound_tools() {
        assert!(is_inbound_tool(REPLY_TOOL));
        assert!(is_inbound_tool("bunsin.inbound/other"));
        assert!(!is_inbound_tool("inbound/reply"));
        assert!(!is_inbound_tool("bunsin.coding/start"));
    }
}
