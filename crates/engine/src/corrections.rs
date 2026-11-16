//! Corrections: the owner's edits of the agent's drafts.
//!
//! A correction is stored after the privacy filter and without the AI marker, together with the
//! event it answered. A live event gets the corrections of similar past tasks as quoted data.
//! A replay gets none: a correction can hold the real reply of the replayed case.

use crate::{Ctx, Engine, LIST_LIMIT, store_error};
use bunsin_api::{ApiError, ApiResult};
use bunsin_core::context::Correction;
use bunsin_core::event::InboundEvent;
use bunsin_core::marker::{strip_marker, text_places};
use bunsin_core::policy::OutgoingText;
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::records::{CorrectionRecord, InboxItem};
use bunsin_core::search::query_terms;
use bunsin_store::ProfileStore;
use serde_json::{Map, Value, json};

/// The most corrections one prompt gets.
const CORRECTIONS_IN_PROMPT: usize = 5;

/// The text of a call as the owner reads it: the texts at the places of the rule, without the
/// AI marker, joined by blank lines. A call whose rule names no place gives its arguments as JSON.
fn draft_text(args: &Map<String, Value>, outgoing: Option<&OutgoingText>, marker: &str) -> String {
    let places = outgoing
        .and_then(|outgoing| text_places(args, outgoing).ok())
        .unwrap_or_default();
    if places.is_empty() {
        return Value::Object(args.clone()).to_string();
    }
    let texts: Vec<&str> = places.iter().map(|place| strip_marker(&place.text, marker)).collect();
    texts.join("\n\n")
}

impl Engine {
    /// The correction that an edit of `item` makes, from the arguments before and after the edit.
    pub(crate) fn correction(
        ctx: &Ctx,
        item: &InboxItem,
        tool: &str,
        before: &Map<String, Value>,
        after: &Map<String, Value>,
        reason: &str,
    ) -> ApiResult<Correction> {
        let outgoing = ctx.config.policy.rules.get(tool).map(|rule| &rule.outgoing_text);
        let marker = &ctx.config.ai_marker;
        let filter = |text: &str| ctx.redactor.redact(text, &mut RedactionCounts::new());
        // The stored event passed the filter before it was stored.
        let event = ctx.store.event(&item.event_key).map_err(store_error)?;
        let (source, task) = event.map_or_else(Default::default, |event| (event.source, event.text));
        Ok(Correction {
            tool: tool.to_owned(),
            source,
            task,
            original: filter(&draft_text(before, outgoing, marker)),
            edited: filter(&draft_text(after, outgoing, marker)),
            reason: filter(reason),
        })
    }

    /// The corrections of the past tasks most like this event.
    pub(crate) fn similar_corrections(store: &ProfileStore, event: &InboundEvent) -> ApiResult<Vec<Correction>> {
        store
            .similar_corrections(&query_terms(&event.text), &event.source, CORRECTIONS_IN_PROMPT)
            .map_err(store_error)
    }

    pub(crate) fn read_corrections(&self, profile: &str) -> ApiResult<Vec<CorrectionRecord>> {
        self.store(profile)?.corrections(LIST_LIMIT).map_err(store_error)
    }

    /// Retires a correction: it stays stored, but no prompt gets it.
    pub(crate) fn retire(&self, profile: &str, id: i64) -> ApiResult<()> {
        let store = self.store(profile)?;
        if !store.retire_correction(id, self.now()).map_err(store_error)? {
            return Err(ApiError::NotAllowed(format!("correction {id} is already retired")));
        }
        self.audit(&store, "correction_retired", None, json!({"id": id}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MARKER: &str = "— AI agent (Bunsin) acting for Kim";

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    #[test]
    fn the_draft_text_joins_the_places_of_the_rule_without_the_marker() {
        let call = args(json!({
            "channel": "C1",
            "text": format!("Fallback\n\n{MARKER}"),
            "blocks": [{"text": format!("First\n\n{MARKER}")}, {"text": "Second"}]
        }));
        let paths = OutgoingText::Paths(vec!["text".into(), "blocks.*.text".into()]);
        assert_eq!(draft_text(&call, Some(&paths), MARKER), "Fallback\n\nFirst\n\nSecond");
        let arg = OutgoingText::Arg("text".into());
        assert_eq!(draft_text(&call, Some(&arg), MARKER), "Fallback");
        // A call without a place of text, or without a rule, gives its arguments.
        let plain = args(json!({"issue": "PROJ-1"}));
        assert_eq!(
            draft_text(&plain, Some(&OutgoingText::None), MARKER),
            r#"{"issue":"PROJ-1"}"#
        );
        assert_eq!(draft_text(&plain, None, MARKER), r#"{"issue":"PROJ-1"}"#);
    }
}
