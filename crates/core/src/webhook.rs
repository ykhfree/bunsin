//! Webhooks: Slack or Jira Cloud sends events to `POST /hooks/{profile}/{name}` on a listener for callers.
//!
//! A webhook belongs to a REST API connector of the same service, and its events have that connector
//! as their source. The server checks the signature with a secret that the owner stores, keeps the event
//! in the intake, answers at once, and hands the event to the agent later. A webhook event is from
//! outside: anyone who can write in the service can steer it.

use crate::event::InboundEvent;
use crate::inbound::InboundScope;
use serde::{Deserialize, Serialize};

/// Every key of a webhook secret starts with this. No other key that the server chooses does.
pub const HOOK_SECRET_PREFIX: &str = "HOOK_";
/// How long a handled delivery is kept to drop a repeated delivery, in seconds.
pub const DELIVERY_KEEP_SECS: i64 = 7 * 24 * 3_600;
/// A Jira delivery whose signed `timestamp` is older than this is refused, in seconds.
/// Jira tries a delivery again up to 5 times, 5 to 15 minutes apart.
pub const JIRA_MAX_AGE_SECS: i64 = 2 * 3_600;
/// The largest body that a webhook takes, in bytes.
pub const MAX_HOOK_BODY_BYTES: usize = 1024 * 1024;

/// One webhook of a profile, by name in `webhooks`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// A REST API connector of Slack or Jira. Its service fixes how deliveries are checked and read.
    pub connector: String,
    /// Which listeners take deliveries. Slack and Jira Cloud reach only a public listener.
    #[serde(default)]
    pub scope: InboundScope,
    /// Authors whose events are skipped: the owner's own account IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skip_authors: Vec<String>,
}

impl WebhookConfig {
    /// True if the listener of `listener` takes deliveries for this webhook.
    pub fn served_on(&self, listener: InboundScope) -> bool {
        crate::inbound::reaches(self.scope, listener)
    }
}

/// The secret key of a webhook: `HOOK_` and the name in upper case with `-` as `_`.
pub fn hook_secret_key(name: &str) -> String {
    format!("{HOOK_SECRET_PREFIX}{}", name.to_ascii_uppercase().replace('-', "_"))
}

/// What a webhook secret is stored for, in the place of a token's address. It is not an address,
/// so the value is never sent as the token of an address.
pub fn hook_secret_origin(name: &str) -> String {
    format!("webhook of {name}")
}

/// True if an event is the agent's own post or a post of an author to skip. The polls and the
/// webhooks use this one check, or the agent would answer its own replies.
pub fn own_post(event: &InboundEvent, skip_authors: &[String], ai_marker: &str) -> bool {
    skip_authors.contains(&event.author) || event.text.contains(ai_marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_and_origins_of_webhook_secrets() {
        assert_eq!(hook_secret_key("slack-events"), "HOOK_SLACK_EVENTS");
        assert_ne!(hook_secret_key("a-b"), hook_secret_key("ab"));
        // The origin is not an address, so no token lookup can match it.
        assert_eq!(crate::settings::origin_of(&hook_secret_origin("x")), None);
    }

    #[test]
    fn the_agents_own_posts_and_skipped_authors_are_not_events() {
        let event = |author: &str, text: &str| InboundEvent {
            source: "slack".into(),
            external_id: "C1:1".into(),
            thread: None,
            author: author.into(),
            text: text.into(),
            channel: Default::default(),
        };
        let skip = ["U-KIM".to_owned()];
        assert!(own_post(&event("U-KIM", "hi"), &skip, "— AI agent"));
        assert!(own_post(&event("U-BOT", "Friday.\n— AI agent"), &skip, "— AI agent"));
        assert!(!own_post(&event("U-LEE", "When?"), &skip, "— AI agent"));
    }

    #[test]
    fn the_internal_listener_takes_every_webhook_and_the_public_one_only_public_ones() {
        let hook = |scope| WebhookConfig {
            connector: "slack".into(),
            scope,
            skip_authors: vec![],
        };
        assert!(hook(InboundScope::Internal).served_on(InboundScope::Internal));
        assert!(!hook(InboundScope::Internal).served_on(InboundScope::Public));
        assert!(hook(InboundScope::Public).served_on(InboundScope::Public));
        // A config without `scope` is internal.
        let read: WebhookConfig = serde_json::from_str(r#"{"connector": "slack"}"#).unwrap();
        assert_eq!(read.scope, InboundScope::Internal);
        assert!(serde_json::from_str::<WebhookConfig>(r#"{"connector": "slack", "mapping": {}}"#).is_err());
    }
}
