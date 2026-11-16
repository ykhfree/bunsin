//! Turns the result of a search tool into events. The engine calls the tool on a timer.
//!
//! MCP servers return different shapes, so the settings say where the items and their
//! fields are. Nothing here is specific to one service.

use crate::event::{Channel, InboundEvent};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The shortest allowed time between two runs of one poll, in seconds.
pub const MIN_POLL_SECS: u64 = 30;

/// Where the fields of one item are. Each value is a dot path, for example `user.id`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollMapping {
    /// The path from the tool result to the list of items. Empty if the result is the list.
    pub items: String,
    /// The path in an item to its ID. The ID must stay the same for the same message.
    pub id: String,
    pub text: String,
    pub author: String,
    pub thread: Option<String>,
}

/// One search that the engine repeats to find new events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PollConfig {
    pub connector: String,
    /// A tool of that connector. Its permission rule must have the class `read`.
    pub tool: String,
    pub args: Map<String, Value>,
    pub every_secs: u64,
    pub mapping: PollMapping,
    /// Authors whose items are skipped: the owner's own account names or IDs.
    pub skip_authors: Vec<String>,
}

impl PollConfig {
    pub fn tool_id(&self) -> String {
        format!("{}/{}", self.connector, self.tool)
    }

    /// Changes when the search itself changes, so an edited search gets a new start.
    pub fn identity(&self) -> String {
        let args = Value::Object(self.args.clone()).to_string();
        crate::sha256_hex(format!("{}\0{}\0{args}", self.connector, self.tool).as_bytes())
    }

    /// Checks the parts that need no other data.
    pub fn check(&self) -> Result<(), String> {
        if self.every_secs < MIN_POLL_SECS {
            return Err(format!(
                "poll `{}` runs every {} s; the shortest interval is {MIN_POLL_SECS} s",
                self.tool_id(),
                self.every_secs
            ));
        }
        for (name, path) in [
            ("id", &self.mapping.id),
            ("text", &self.mapping.text),
            ("author", &self.mapping.author),
        ] {
            if path.trim().is_empty() {
                return Err(format!("poll `{}` has no `mapping.{name}` path", self.tool_id()));
            }
        }
        Ok(())
    }
}

/// What one tool result gave.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Extracted {
    /// How many items the result held.
    pub items: u32,
    /// The items that had an ID, text and an author.
    pub events: Vec<InboundEvent>,
}

fn at_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.')
        .filter(|part| !part.is_empty())
        .try_fold(value, |value, part| value.get(part))
}

/// A string or a number as text. Other kinds of value are not usable.
fn as_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// Many MCP servers return their JSON as text in a content block. This unwraps it.
fn unwrap_content_blocks(result: &Value) -> Option<Value> {
    let blocks = result.as_array()?;
    let first = blocks.first()?;
    if first.get("type")?.as_str()? != "text" {
        return None;
    }
    serde_json::from_str(first.get("text")?.as_str()?).ok()
}

/// Reads the items of a tool result. An error means the result does not have the mapped shape.
pub fn extract(poll: &PollConfig, result: &Value) -> Result<Extracted, String> {
    let unwrapped = unwrap_content_blocks(result);
    let root = unwrapped.as_ref().unwrap_or(result);
    let list = at_path(root, &poll.mapping.items)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("the result has no list at `{}`", poll.mapping.items))?;

    let mut extracted = Extracted {
        items: list.len() as u32,
        events: Vec::new(),
    };
    for item in list {
        let field = |path: &str| at_path(item, path).and_then(as_text);
        let (Some(id), Some(text), Some(author)) = (
            field(&poll.mapping.id),
            field(&poll.mapping.text),
            field(&poll.mapping.author),
        ) else {
            continue;
        };
        extracted.events.push(InboundEvent {
            source: poll.connector.clone(),
            external_id: id,
            thread: poll.mapping.thread.as_deref().and_then(field),
            author,
            text,
            channel: Channel::Owner,
        });
    }
    Ok(extracted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn poll(items: &str) -> PollConfig {
        PollConfig {
            connector: "slack".into(),
            tool: "search_messages".into(),
            args: json!({"query": "to:me"}).as_object().unwrap().clone(),
            every_secs: 60,
            mapping: PollMapping {
                items: items.into(),
                id: "ts".into(),
                text: "text".into(),
                author: "user.name".into(),
                thread: Some("thread_ts".into()),
            },
            skip_authors: vec![],
        }
    }

    fn message(ts: &str, text: &str, author: &str) -> Value {
        json!({"ts": ts, "text": text, "user": {"name": author}})
    }

    #[test]
    fn items_become_events_with_mapped_fields() {
        let mut first = message("1700000000.000100", "deploy?", "lee");
        first["thread_ts"] = json!("1699999999.000001");
        let result = json!({"messages": {"matches": [first, message("1700000001.000200", "ping", "park")]}});
        let extracted = extract(&poll("messages.matches"), &result).unwrap();
        assert_eq!(extracted.items, 2);
        assert_eq!(
            extracted.events[0],
            InboundEvent {
                source: "slack".into(),
                external_id: "1700000000.000100".into(),
                thread: Some("1699999999.000001".into()),
                author: "lee".into(),
                text: "deploy?".into(),
                // A poll is the owner's own search.
                channel: Channel::Owner,
            }
        );
        assert_eq!(extracted.events[1].thread, None);
    }

    #[test]
    fn a_result_that_is_the_list_itself_and_numeric_ids_work() {
        let result = json!([{"ts": 42, "text": "hi", "user": {"name": "lee"}}]);
        let extracted = extract(&poll(""), &result).unwrap();
        assert_eq!(extracted.events[0].external_id, "42");
    }

    #[test]
    fn json_inside_a_text_content_block_is_unwrapped() {
        let inner = json!({"messages": [message("1", "hi", "lee")]}).to_string();
        let result = json!([{"type": "text", "text": inner}]);
        let extracted = extract(&poll("messages"), &result).unwrap();
        assert_eq!(extracted.events.len(), 1);
        // Plain prose in a content block is not a list.
        let prose = json!([{"type": "text", "text": "Found 3 messages."}]);
        assert!(extract(&poll("messages"), &prose).is_err());
    }

    #[test]
    fn items_without_an_id_text_or_author_are_counted_but_not_used() {
        let result = json!({"messages": [
            message("1", "ok", "lee"),
            {"text": "no id", "user": {"name": "lee"}},
            {"ts": "2", "text": "", "user": {"name": "lee"}},
            {"ts": "3", "text": "no author"},
            {"ts": "4", "text": ["not", "text"], "user": {"name": "lee"}},
        ]});
        let extracted = extract(&poll("messages"), &result).unwrap();
        assert_eq!(extracted.items, 5);
        assert_eq!(extracted.events.len(), 1);
    }

    #[test]
    fn a_wrong_items_path_is_an_error_and_an_empty_list_is_not() {
        let result = json!({"messages": []});
        assert_eq!(extract(&poll("messages"), &result).unwrap(), Extracted::default());
        assert!(
            extract(&poll("results"), &result)
                .unwrap_err()
                .contains("no list at `results`")
        );
        assert!(extract(&poll("messages"), &json!({"messages": "none"})).is_err());
    }

    #[test]
    fn the_identity_follows_the_search_and_not_the_interval_or_the_mapping() {
        let base = poll("messages");
        let mut slower = base.clone();
        slower.every_secs = 600;
        slower.mapping.items = "other".into();
        assert_eq!(base.identity(), slower.identity());
        let mut other_query = base.clone();
        other_query.args.insert("query".into(), json!("from:lee"));
        assert_ne!(base.identity(), other_query.identity());
        let mut other_tool = base.clone();
        other_tool.tool = "list_mentions".into();
        assert_ne!(base.identity(), other_tool.identity());
    }

    #[test]
    fn check_refuses_a_short_interval_and_empty_paths() {
        assert_eq!(poll("messages").check(), Ok(()));
        let mut fast = poll("messages");
        fast.every_secs = 5;
        assert!(fast.check().unwrap_err().contains("shortest interval is 30 s"));
        let mut no_id = poll("messages");
        no_id.mapping.id = " ".into();
        assert!(no_id.check().unwrap_err().contains("mapping.id"));
    }
}
