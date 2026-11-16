//! Checks and reads the events that Slack and Jira Cloud send to a webhook.
//!
//! Verify the signature over the raw body first. Parse the body only after that.
//! The formats follow the Slack and Atlassian docs. Nobody ran this code against the real services.

use bunsin_core::event::InboundEvent;
use hmac::{Hmac, Mac};
use serde_json::Value;
use sha2::Sha256;

/// Slack rejects a request whose timestamp differs from the local time by more than this.
pub const SLACK_MAX_SKEW_SECS: i64 = 5 * 60;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WebhookError {
    #[error("the signature or timestamp header is missing or malformed")]
    BadHeader,
    #[error("the request timestamp is more than {SLACK_MAX_SKEW_SECS} s away from the local time")]
    Stale,
    #[error("the signature does not match the body")]
    BadSignature,
    #[error("the body is not a valid event: {0}")]
    BadBody(String),
}

fn mac(secret: &str) -> Hmac<Sha256> {
    Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts a key of any length")
}

/// Compares `expected_hex` with the MAC in constant time.
fn verify_hex(mac: Hmac<Sha256>, expected_hex: &str) -> Result<(), WebhookError> {
    let expected = hex::decode(expected_hex).map_err(|_| WebhookError::BadHeader)?;
    mac.verify_slice(&expected).map_err(|_| WebhookError::BadSignature)
}

/// Checks a Slack request: `X-Slack-Request-Timestamp` is `timestamp`, `X-Slack-Signature` is `signature`.
/// The signature is `v0=` and the hex HMAC-SHA256 of `v0:{timestamp}:{body}`, keyed with the signing secret.
/// `now_unix_secs` is the local time. The caller passes it in.
pub fn verify_slack(
    signing_secret: &str,
    timestamp: &str,
    signature: &str,
    body: &[u8],
    now_unix_secs: i64,
) -> Result<(), WebhookError> {
    let sent_at: i64 = timestamp.parse().map_err(|_| WebhookError::BadHeader)?;
    // An old request could be a replay. Slack's docs check the absolute difference.
    if now_unix_secs.abs_diff(sent_at) > SLACK_MAX_SKEW_SECS as u64 {
        return Err(WebhookError::Stale);
    }
    let hex = signature.strip_prefix("v0=").ok_or(WebhookError::BadHeader)?;
    let mut mac = mac(signing_secret);
    mac.update(format!("v0:{timestamp}:").as_bytes());
    mac.update(body);
    verify_hex(mac, hex)
}

/// Checks a Jira Cloud webhook that has a secret. `signature` is the `X-Hub-Signature` header:
/// `sha256=` and the hex HMAC-SHA256 of the body, keyed with the secret.
/// Atlassian may add other methods later. This accepts only `sha256`.
pub fn verify_atlassian(secret: &str, signature: &str, body: &[u8]) -> Result<(), WebhookError> {
    let hex = signature.strip_prefix("sha256=").ok_or(WebhookError::BadHeader)?;
    let mut mac = mac(secret);
    mac.update(body);
    verify_hex(mac, hex)
}

/// What a verified Slack delivery asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackDelivery {
    /// A `url_verification` request. Answer HTTP 200 with this challenge.
    Challenge(String),
    Event(InboundEvent),
    /// Another event type, or a message subtype that is not a new message.
    Ignored,
}

/// Message subtypes that carry a new message. Others are changes, deletions and channel notices.
/// `bot_message` stays: the engine decides which authors to skip.
const SLACK_MESSAGE_SUBTYPES: [&str; 3] = ["thread_broadcast", "file_share", "bot_message"];

fn parse_json(body: &[u8]) -> Result<Value, WebhookError> {
    serde_json::from_slice(body).map_err(|e| WebhookError::BadBody(e.to_string()))
}

fn text_at<'a>(value: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter()
        .try_fold(value, |value, key| value.get(key))?
        .as_str()
        .filter(|text| !text.trim().is_empty())
}

/// Reads a verified Slack Events API body. `source` is the connector name.
///
/// The event ID is `<channel>:<ts>`. Slack sends one message as both `message` and
/// `app_mention` when the app has both subscriptions, so the event's own `event_id` would
/// let the same message through twice. The thread is `<channel>:<ts of the thread parent>`:
/// the `channel` and `thread_ts` that a reply with `post_message` needs.
pub fn parse_slack(source: &str, body: &[u8]) -> Result<SlackDelivery, WebhookError> {
    let body = parse_json(body)?;
    match body.get("type").and_then(Value::as_str) {
        Some("url_verification") => text_at(&body, &["challenge"])
            .map(|challenge| SlackDelivery::Challenge(challenge.to_owned()))
            .ok_or_else(|| WebhookError::BadBody("url_verification has no challenge".into())),
        Some("event_callback") => {
            let event = body
                .get("event")
                .ok_or_else(|| WebhookError::BadBody("event_callback has no event".into()))?;
            if !matches!(
                event.get("type").and_then(Value::as_str),
                Some("message" | "app_mention")
            ) {
                return Ok(SlackDelivery::Ignored);
            }
            if let Some(subtype) = event.get("subtype").and_then(Value::as_str)
                && !SLACK_MESSAGE_SUBTYPES.contains(&subtype)
            {
                return Ok(SlackDelivery::Ignored);
            }
            let field = |name: &str| text_at(event, &[name]);
            // A bot message can have no `user`. Its `bot_id` names the author.
            let (Some(channel), Some(ts), Some(text), Some(author)) = (
                field("channel"),
                field("ts"),
                field("text"),
                field("user").or_else(|| field("bot_id")),
            ) else {
                return Ok(SlackDelivery::Ignored);
            };
            let parent = field("thread_ts").unwrap_or(ts);
            Ok(SlackDelivery::Event(InboundEvent {
                source: source.to_owned(),
                external_id: format!("{channel}:{ts}"),
                thread: Some(format!("{channel}:{parent}")),
                author: author.to_owned(),
                text: text.to_owned(),
            }))
        }
        _ => Ok(SlackDelivery::Ignored),
    }
}

/// Reads a verified Jira Cloud webhook body. `source` is the connector name.
/// Returns `None` for an event that is not a comment or an issue creation or update.
///
/// A comment has the ID `comment:<comment id>:<updated>`. Jira sends a new comment as
/// `comment_created` and also as `jira:issue_updated` with a `comment`; both get the same ID.
/// An edit changes `updated`, so an edit is a new event. Another issue event has the ID
/// `issue:<issue id>:<timestamp>`. A retry repeats the body, so it repeats the ID.
/// The thread is the issue key.
pub fn parse_jira(source: &str, body: &[u8]) -> Result<Option<InboundEvent>, WebhookError> {
    let body = parse_json(body)?;
    let kind = body.get("webhookEvent").and_then(Value::as_str).unwrap_or_default();
    if !matches!(
        kind,
        "comment_created" | "comment_updated" | "jira:issue_created" | "jira:issue_updated"
    ) {
        return Ok(None);
    }
    let missing = |what: &str| WebhookError::BadBody(format!("`{kind}` has no {what}"));
    let issue = body.get("issue").ok_or_else(|| missing("issue"))?;
    let key = text_at(issue, &["key"]).ok_or_else(|| missing("issue key"))?;

    let (external_id, author, text) = match body.get("comment") {
        Some(comment) => {
            let id = scalar_text(comment.get("id")).ok_or_else(|| missing("comment id"))?;
            let updated = text_at(comment, &["updated"]).ok_or_else(|| missing("comment update time"))?;
            // `updateAuthor` is the person who wrote this version. It equals `author` for a new comment.
            let author = text_at(comment, &["updateAuthor", "accountId"])
                .or_else(|| text_at(comment, &["author", "accountId"]))
                .ok_or_else(|| missing("comment author"))?;
            let text = comment.get("body").map(document_text).unwrap_or_default();
            (format!("comment:{id}:{updated}"), author, text)
        }
        None if kind.starts_with("comment_") => return Err(missing("comment")),
        None => {
            let id = scalar_text(issue.get("id")).ok_or_else(|| missing("issue id"))?;
            let timestamp = scalar_text(body.get("timestamp")).ok_or_else(|| missing("timestamp"))?;
            let author = text_at(&body, &["user", "accountId"]).ok_or_else(|| missing("user"))?;
            let summary = text_at(issue, &["fields", "summary"]).unwrap_or_default();
            let description = issue
                .get("fields")
                .and_then(|fields| fields.get("description"))
                .map(document_text)
                .unwrap_or_default();
            let text = [summary, description.as_str()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            (format!("issue:{id}:{timestamp}"), author, text)
        }
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(InboundEvent {
        source: source.to_owned(),
        external_id,
        thread: Some(key.to_owned()),
        author: author.to_owned(),
        text,
    }))
}

/// A string or a number as text. Jira sends some IDs and the timestamp as numbers.
fn scalar_text(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// The text of a Jira body. A body is wiki markup text, or an Atlassian Document Format tree.
fn document_text(body: &Value) -> String {
    fn walk(node: &Value, out: &mut String) {
        match node.get("type").and_then(Value::as_str) {
            Some("text") => out.push_str(node.get("text").and_then(Value::as_str).unwrap_or_default()),
            Some("hardBreak") => out.push('\n'),
            Some("mention") => out.push_str(text_at(node, &["attrs", "text"]).unwrap_or_default()),
            _ => {}
        }
        for child in node.get("content").and_then(Value::as_array).into_iter().flatten() {
            walk(child, out);
        }
        if matches!(
            node.get("type").and_then(Value::as_str),
            Some("paragraph" | "heading" | "codeBlock")
        ) {
            out.push('\n');
        }
    }
    match body {
        Value::String(text) => text.clone(),
        node => {
            let mut out = String::new();
            walk(node, &mut out);
            out.trim_end().to_owned()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn adf_text_keeps_paragraphs_breaks_and_mentions() {
        let doc = json!({"type": "doc", "version": 1, "content": [
            {"type": "paragraph", "content": [
                {"type": "mention", "attrs": {"id": "1", "text": "@Lee"}},
                {"type": "text", "text": " please check"},
                {"type": "hardBreak"},
                {"type": "text", "text": "line two"}
            ]},
            {"type": "paragraph", "content": [{"type": "text", "text": "second"}]}
        ]});
        assert_eq!(document_text(&doc), "@Lee please check\nline two\nsecond");
        assert_eq!(document_text(&json!("plain *wiki*")), "plain *wiki*");
    }
}
