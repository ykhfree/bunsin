//! Marks outgoing text as AI-generated. The harness does this; the LLM is not trusted to.

use crate::policy::OutgoingText;
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    #[error("the marker is empty")]
    EmptyMarker,
    #[error("`{0}` must hold text (a string) so that the AI marker can be added")]
    NoTextAt(String),
}

/// Appends `marker` to `text` unless the text already ends with it.
pub fn apply_marker(text: &str, marker: &str) -> String {
    let body = text.trim_end();
    if body.ends_with(marker) {
        body.to_owned()
    } else {
        format!("{body}\n\n{marker}")
    }
}

/// Marks the text at the end of `steps`. Returns false if a step finds no value of the right kind.
fn mark_at(value: &mut Value, steps: &[&str], marker: &str) -> bool {
    match (steps.split_first(), value) {
        (None, Value::String(text)) => {
            *text = apply_marker(text, marker);
            true
        }
        (Some((&"*", rest)), Value::Array(items)) => items.iter_mut().all(|item| mark_at(item, rest, marker)),
        (Some((name, rest)), Value::Object(fields)) if *name != "*" => {
            fields.get_mut(*name).is_some_and(|field| mark_at(field, rest, marker))
        }
        _ => false,
    }
}

/// Adds the marker to every place of `text` in the arguments of a tool call.
///
/// Every place must hold a string. Under `*`, every item of the list must have the rest of the path.
/// If one place does not, the arguments stay unchanged and the call must not be made.
pub fn mark_args(args: &mut Map<String, Value>, text: &OutgoingText, marker: &str) -> Result<(), MarkerError> {
    let locations = text.locations();
    if locations.is_empty() {
        return Ok(());
    }
    if marker.trim().is_empty() {
        return Err(MarkerError::EmptyMarker);
    }
    let mut marked = Value::Object(args.clone());
    for location in locations {
        // An `arg` names one argument, also if its name contains a dot.
        let steps: Vec<&str> = match text {
            OutgoingText::Paths(_) => location.split('.').collect(),
            _ => vec![location],
        };
        if !mark_at(&mut marked, &steps, marker) {
            return Err(MarkerError::NoTextAt(location.to_owned()));
        }
    }
    if let Value::Object(marked) = marked {
        *args = marked;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MARKER: &str = "— AI agent (Bunsin) on behalf of Kim";

    #[test]
    fn marker_is_added_once() {
        let once = apply_marker("Done. See PROJ-1.", MARKER);
        assert_eq!(once, format!("Done. See PROJ-1.\n\n{MARKER}"));
        assert_eq!(apply_marker(&once, MARKER), once);
        assert_eq!(apply_marker(&format!("{once}\n \n"), MARKER), once);
    }

    #[test]
    fn a_marker_quoted_in_the_middle_does_not_count() {
        let text = format!("Someone wrote \"{MARKER}\" earlier, but this is my answer.");
        assert!(apply_marker(&text, MARKER).ends_with(&format!("\n\n{MARKER}")));
    }

    fn args(value: Value) -> Map<String, Value> {
        value.as_object().unwrap().clone()
    }

    fn arg(name: &str) -> OutgoingText {
        OutgoingText::Arg(name.into())
    }

    fn paths(list: &[&str]) -> OutgoingText {
        OutgoingText::Paths(list.iter().map(|p| (*p).to_owned()).collect())
    }

    fn marked(text: &str) -> String {
        format!("{text}\n\n{MARKER}")
    }

    #[test]
    fn mark_args_changes_only_the_text_argument() {
        let mut args = args(json!({"channel": "C1", "text": "hello"}));
        mark_args(&mut args, &arg("text"), MARKER).unwrap();
        assert_eq!(args["channel"], "C1");
        assert_eq!(args["text"], marked("hello"));
        // A tool that sends no text keeps its arguments, also with an empty marker.
        let before = args.clone();
        mark_args(&mut args, &OutgoingText::None, "").unwrap();
        assert_eq!(args, before);
    }

    #[test]
    fn a_write_without_markable_text_is_an_error() {
        let mut missing = args(json!({"channel": "C1"}));
        assert_eq!(
            mark_args(&mut missing, &arg("text"), MARKER),
            Err(MarkerError::NoTextAt("text".into()))
        );
        let mut not_text = args(json!({"text": ["a", "b"]}));
        assert_eq!(
            mark_args(&mut not_text, &arg("text"), MARKER),
            Err(MarkerError::NoTextAt("text".into()))
        );
        let mut args = args(json!({"text": "hello"}));
        assert_eq!(mark_args(&mut args, &arg("text"), "  "), Err(MarkerError::EmptyMarker));
    }

    #[test]
    fn every_named_place_gets_the_marker() {
        let mut call = args(json!({
            "channel": "C1",
            "text": "Fallback",
            "blocks": [
                {"type": "section", "text": {"type": "mrkdwn", "text": "First"}},
                {"type": "section", "text": {"type": "mrkdwn", "text": "Second"}}
            ]
        }));
        mark_args(&mut call, &paths(&["text", "blocks.*.text.text"]), MARKER).unwrap();
        assert_eq!(call["text"], marked("Fallback"));
        assert_eq!(call["blocks"][0]["text"]["text"], marked("First"));
        assert_eq!(call["blocks"][1]["text"]["text"], marked("Second"));
        // The other values stay as they were.
        assert_eq!(call["channel"], "C1");
        assert_eq!(call["blocks"][1]["text"]["type"], "mrkdwn");
        // A second pass adds nothing.
        let once = call.clone();
        mark_args(&mut call, &paths(&["text", "blocks.*.text.text"]), MARKER).unwrap();
        assert_eq!(call, once);
        // An empty list holds no text, so there is nothing to mark in it.
        let mut empty = args(json!({"text": "hi", "blocks": []}));
        mark_args(&mut empty, &paths(&["text", "blocks.*.text"]), MARKER).unwrap();
        assert_eq!(empty["text"], marked("hi"));
    }

    #[test]
    fn a_missing_place_refuses_the_call_and_changes_nothing() {
        let both = paths(&["text", "blocks.*.text"]);
        let cases = [
            // The list is missing.
            json!({"text": "hi"}),
            // One item has no text: the text may be somewhere the rule does not name.
            json!({"text": "hi", "blocks": [{"text": "a"}, {"type": "divider"}]}),
            // The text of an item is not a string.
            json!({"text": "hi", "blocks": [{"text": {"text": "a"}}]}),
            // `*` needs a list, and a name needs an object.
            json!({"text": "hi", "blocks": {"text": "a"}}),
            json!({"text": "hi", "blocks": ["a"]}),
        ];
        for case in cases {
            let mut call = args(case.clone());
            assert_eq!(
                mark_args(&mut call, &both, MARKER),
                Err(MarkerError::NoTextAt("blocks.*.text".into())),
                "{case}"
            );
            // The first place was marked before the second failed. The arguments are unchanged.
            assert_eq!(Value::Object(call), case);
        }
        let mut no_text = args(json!({"blocks": [{"text": "a"}]}));
        assert_eq!(
            mark_args(&mut no_text, &both, MARKER),
            Err(MarkerError::NoTextAt("text".into()))
        );
    }

    #[test]
    fn an_arg_with_a_dot_is_one_name_and_a_path_is_steps() {
        let mut call = args(json!({"a.b": "top", "a": {"b": "inner"}}));
        mark_args(&mut call, &arg("a.b"), MARKER).unwrap();
        assert_eq!(call["a.b"], marked("top"));
        assert_eq!(call["a"]["b"], "inner");
        mark_args(&mut call, &paths(&["a.b"]), MARKER).unwrap();
        assert_eq!(call["a"]["b"], marked("inner"));
    }
}
