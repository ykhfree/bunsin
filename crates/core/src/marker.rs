//! Marks outgoing text as AI-generated. The harness does this; the LLM is not trusted to.

use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkerError {
    #[error("the marker is empty")]
    EmptyMarker,
    #[error("argument `{0}` must be a string so that the AI marker can be added")]
    TextArgNotString(String),
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

/// Marks the outgoing-text argument of a tool call.
pub fn mark_args(
    args: &mut Map<String, Value>,
    text_arg: &str,
    marker: &str,
) -> Result<(), MarkerError> {
    if marker.trim().is_empty() {
        return Err(MarkerError::EmptyMarker);
    }
    match args.get_mut(text_arg) {
        Some(Value::String(text)) => {
            *text = apply_marker(text, marker);
            Ok(())
        }
        _ => Err(MarkerError::TextArgNotString(text_arg.to_owned())),
    }
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

    #[test]
    fn mark_args_changes_only_the_text_argument() {
        let mut args = json!({"channel": "C1", "text": "hello"})
            .as_object()
            .unwrap()
            .clone();
        mark_args(&mut args, "text", MARKER).unwrap();
        assert_eq!(args["channel"], "C1");
        assert_eq!(args["text"], format!("hello\n\n{MARKER}"));
    }

    #[test]
    fn a_write_without_markable_text_is_an_error() {
        let mut missing = json!({"channel": "C1"}).as_object().unwrap().clone();
        assert_eq!(
            mark_args(&mut missing, "text", MARKER),
            Err(MarkerError::TextArgNotString("text".into()))
        );
        let mut not_text = json!({"text": ["a", "b"]}).as_object().unwrap().clone();
        assert_eq!(
            mark_args(&mut not_text, "text", MARKER),
            Err(MarkerError::TextArgNotString("text".into()))
        );
        let mut args = json!({"text": "hello"}).as_object().unwrap().clone();
        assert_eq!(
            mark_args(&mut args, "text", "  "),
            Err(MarkerError::EmptyMarker)
        );
    }
}
