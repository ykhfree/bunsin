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
}

impl InboundEvent {
    /// Two deliveries of the same source event have the same key.
    pub fn dedupe_key(&self) -> String {
        // The length prefix keeps ("a", "b\0c") and ("a\0b", "c") apart.
        crate::sha256_hex(
            format!(
                "{}:{}\0{}",
                self.source.len(),
                self.source,
                self.external_id
            )
            .as_bytes(),
        )
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
        }
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
