use serde::{Deserialize, Serialize};

/// A message or change that arrived from a connector. All text fields are untrusted data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboundEvent {
    /// Connector name, for example `slack`.
    pub source: String,
    /// The ID that the source system gave this event.
    pub external_id: String,
    pub thread: Option<String>,
    pub author: String,
    pub text: String,
    /// Who could make the event happen. Not part of the dedupe key. It is never read from the wire or
    /// from storage: the entry point on the server sets it, and every other event is outside.
    #[serde(default, skip_deserializing)]
    pub channel: Channel,
}

/// The way an event reached the engine. The default is `Outside`, so an event that no entry point
/// marked gets no trust it was not given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// The owner's own polls, or an event the owner handed in with the owner's access token.
    Owner,
    /// Any other way, for example a webhook or an inbound caller. Other people can write such an event
    /// to steer the agent, so nothing from it is stored without the owner.
    #[default]
    Outside,
}

impl InboundEvent {
    /// Two deliveries of the same source event have the same key.
    pub fn dedupe_key(&self) -> String {
        // The length prefix keeps ("a", "b\0c") and ("a\0b", "c") apart.
        crate::sha256_hex(format!("{}:{}\0{}", self.source.len(), self.source, self.external_id).as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(source: &str, id: &str, text: &str) -> InboundEvent {
        InboundEvent {
            source: source.into(),
            external_id: id.into(),
            thread: None,
            author: "someone".into(),
            text: text.into(),
            channel: Default::default(),
        }
    }

    #[test]
    fn the_channel_is_outside_unless_an_entry_point_sets_it() {
        let wire = r#"{"source":"slack","external_id":"1","thread":null,"author":"a","text":"t"}"#;
        let read: InboundEvent = serde_json::from_str(wire).unwrap();
        assert_eq!(read.channel, Channel::Outside);
        // A value on the wire is not trusted.
        let claimed = wire.replace("\"text\":\"t\"", "\"text\":\"t\",\"channel\":\"owner\"");
        assert!(claimed.contains("\"channel\":\"owner\""));
        let read: InboundEvent = serde_json::from_str(&claimed).unwrap();
        assert_eq!(read.channel, Channel::Outside);
        // An owner event that is stored and read back is outside again.
        let owner = InboundEvent {
            channel: Channel::Owner,
            ..read
        };
        let stored = serde_json::to_string(&owner).unwrap();
        assert_eq!(
            serde_json::from_str::<InboundEvent>(&stored).unwrap().channel,
            Channel::Outside
        );
        assert_eq!(owner.dedupe_key(), event("slack", "1", "x").dedupe_key());
    }

    #[test]
    fn redelivery_with_changed_text_keeps_the_key() {
        assert_eq!(
            event("slack", "1", "a").dedupe_key(),
            event("slack", "1", "edited").dedupe_key()
        );
    }

    #[test]
    fn source_and_id_are_not_confused() {
        assert_ne!(
            event("slack", "1", "a").dedupe_key(),
            event("jira", "1", "a").dedupe_key()
        );
        assert_ne!(
            event("a", "b\0c", "x").dedupe_key(),
            event("a\0b", "c", "x").dedupe_key()
        );
        assert_ne!(
            event("slack", "1", "a").dedupe_key(),
            event("slack", "2", "a").dedupe_key()
        );
    }
}
