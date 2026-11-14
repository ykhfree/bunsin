use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// What the agent proposes to do about one event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Proposal {
    /// The event needs no action.
    Ignore { rationale: String },
    /// The user must handle the event.
    Escalate { rationale: String, summary: String },
    /// Call one connector tool. `tool` is a tool ID: `<connector>/<tool>`.
    ToolCall {
        rationale: String,
        tool: String,
        args: Map<String, Value>,
    },
}

impl Proposal {
    pub fn rationale(&self) -> &str {
        match self {
            Proposal::Ignore { rationale }
            | Proposal::Escalate { rationale, .. }
            | Proposal::ToolCall { rationale, .. } => rationale,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProposalError {
    #[error("the reply is not the expected JSON object: {0}")]
    Json(String),
    #[error("`rationale` is empty")]
    EmptyRationale,
    #[error("kind `{kind}` needs `{field}`")]
    MissingField {
        kind: &'static str,
        field: &'static str,
    },
    #[error("kind `{kind}` must not set `{field}`")]
    UnexpectedField {
        kind: &'static str,
        field: &'static str,
    },
    #[error("`tool` must be `<connector>/<tool>`")]
    BadToolId,
    #[error("`args_json` is not a JSON object: {0}")]
    BadArgs(String),
}

/// The flat object the LLM returns. Every field is present; unused ones are null.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    kind: Kind,
    rationale: String,
    summary: Option<String>,
    tool: Option<String>,
    /// The tool arguments: a JSON object encoded as a string.
    /// A free-form object cannot be part of a strict reply schema, so it travels as text.
    args_json: Option<String>,
}

#[derive(Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Ignore,
    Escalate,
    ToolCall,
}

/// The JSON Schema sent to the LLM as the required reply format.
pub fn proposal_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["kind", "rationale", "summary", "tool", "args_json"],
        "properties": {
            "kind": {"type": "string", "enum": ["ignore", "escalate", "tool_call"]},
            "rationale": {"type": "string"},
            "summary": {"type": ["string", "null"]},
            "tool": {"type": ["string", "null"]},
            "args_json": {
                "type": ["string", "null"],
                "description": "The tool arguments: one JSON object, encoded as a string."
            }
        }
    })
}

/// Parses and checks an LLM reply. A reply that fails here never reaches the policy check.
pub fn parse_proposal(reply: &str) -> Result<Proposal, ProposalError> {
    let raw: Raw =
        serde_json::from_str(reply.trim()).map_err(|e| ProposalError::Json(e.to_string()))?;
    if raw.rationale.trim().is_empty() {
        return Err(ProposalError::EmptyRationale);
    }
    let unexpected = |kind, field, present: bool| {
        if present {
            Err(ProposalError::UnexpectedField { kind, field })
        } else {
            Ok(())
        }
    };
    let filled = |text: &Option<String>| text.as_deref().is_some_and(|t| !t.trim().is_empty());
    match raw.kind {
        Kind::Ignore => {
            unexpected("ignore", "tool", raw.tool.is_some())?;
            unexpected("ignore", "args_json", raw.args_json.is_some())?;
            unexpected("ignore", "summary", raw.summary.is_some())?;
            Ok(Proposal::Ignore {
                rationale: raw.rationale,
            })
        }
        Kind::Escalate => {
            unexpected("escalate", "tool", raw.tool.is_some())?;
            unexpected("escalate", "args_json", raw.args_json.is_some())?;
            if !filled(&raw.summary) {
                return Err(ProposalError::MissingField {
                    kind: "escalate",
                    field: "summary",
                });
            }
            Ok(Proposal::Escalate {
                rationale: raw.rationale,
                summary: raw.summary.unwrap_or_default(),
            })
        }
        Kind::ToolCall => {
            unexpected("tool_call", "summary", raw.summary.is_some())?;
            let tool = raw.tool.ok_or(ProposalError::MissingField {
                kind: "tool_call",
                field: "tool",
            })?;
            let args = raw.args_json.ok_or(ProposalError::MissingField {
                kind: "tool_call",
                field: "args_json",
            })?;
            let args: Map<String, Value> =
                serde_json::from_str(&args).map_err(|e| ProposalError::BadArgs(e.to_string()))?;
            split_tool_id(&tool).ok_or(ProposalError::BadToolId)?;
            Ok(Proposal::ToolCall {
                rationale: raw.rationale,
                tool,
                args,
            })
        }
    }
}

/// Splits `<connector>/<tool>`. Both parts must be non-empty.
pub fn split_tool_id(tool_id: &str) -> Option<(&str, &str)> {
    let (connector, tool) = tool_id.split_once('/')?;
    (!connector.is_empty() && !tool.is_empty()).then_some((connector, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(kind: &str, summary: Value, tool: Value, args: Value) -> String {
        // `args` is a JSON value or null. It is sent as a string, like the LLM sends it.
        let args_json = if args.is_null() {
            Value::Null
        } else {
            Value::String(args.to_string())
        };
        json!({"kind": kind, "rationale": "because", "summary": summary, "tool": tool, "args_json": args_json})
            .to_string()
    }

    #[test]
    fn parses_each_kind() {
        let n = Value::Null;
        assert_eq!(
            parse_proposal(&reply("ignore", n.clone(), n.clone(), n.clone())),
            Ok(Proposal::Ignore {
                rationale: "because".into()
            })
        );
        assert_eq!(
            parse_proposal(&reply(
                "escalate",
                json!("needs a decision"),
                n.clone(),
                n.clone()
            )),
            Ok(Proposal::Escalate {
                rationale: "because".into(),
                summary: "needs a decision".into()
            })
        );
        let parsed = parse_proposal(&reply(
            "tool_call",
            n,
            json!("slack/post"),
            json!({"text": "hi"}),
        ))
        .unwrap();
        let Proposal::ToolCall { tool, args, .. } = parsed else {
            panic!("expected a tool call")
        };
        assert_eq!(tool, "slack/post");
        assert_eq!(args["text"], "hi");
    }

    #[test]
    fn rejects_malformed_replies() {
        let n = Value::Null;
        type Check = fn(&ProposalError) -> bool;
        let cases: Vec<(String, Check)> = vec![
            ("I think we should reply.".into(), |e| matches!(e, ProposalError::Json(_))),
            (reply("launch", n.clone(), n.clone(), n.clone()), |e| matches!(e, ProposalError::Json(_))),
            // An extra field is rejected, not ignored.
            (
                json!({"kind": "ignore", "rationale": "r", "summary": null, "tool": null, "args_json": null, "tier": "autonomous"})
                    .to_string(),
                |e| matches!(e, ProposalError::Json(_)),
            ),
            (
                json!({"kind": "ignore", "rationale": "  ", "summary": null, "tool": null, "args_json": null}).to_string(),
                |e| *e == ProposalError::EmptyRationale,
            ),
            (reply("tool_call", n.clone(), n.clone(), json!({})), |e| {
                *e == ProposalError::MissingField { kind: "tool_call", field: "tool" }
            }),
            (reply("tool_call", n.clone(), json!("slack/post"), n.clone()), |e| {
                *e == ProposalError::MissingField { kind: "tool_call", field: "args_json" }
            }),
            // The arguments must be an object, not another JSON value and not prose.
            (reply("tool_call", n.clone(), json!("slack/post"), json!(["a"])), |e| {
                matches!(e, ProposalError::BadArgs(_))
            }),
            (
                json!({"kind": "tool_call", "rationale": "r", "summary": null, "tool": "slack/post", "args_json": "channel C1"})
                    .to_string(),
                |e| matches!(e, ProposalError::BadArgs(_)),
            ),
            (reply("tool_call", n.clone(), json!("post"), json!({})), |e| *e == ProposalError::BadToolId),
            (reply("tool_call", n.clone(), json!("/post"), json!({})), |e| *e == ProposalError::BadToolId),
            // "ignore" that still carries a tool call is contradictory.
            (reply("ignore", n.clone(), json!("slack/post"), json!({})), |e| {
                *e == ProposalError::UnexpectedField { kind: "ignore", field: "tool" }
            }),
            (reply("escalate", json!(" "), n.clone(), n.clone()), |e| {
                *e == ProposalError::MissingField { kind: "escalate", field: "summary" }
            }),
        ];
        for (text, expected) in cases {
            let error = parse_proposal(&text).expect_err(&text);
            assert!(expected(&error), "{text} gave {error:?}");
        }
    }

    #[test]
    fn schema_lists_every_raw_field_as_required() {
        let schema = proposal_schema();
        let mut required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let mut properties: Vec<&str> = schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        required.sort_unstable();
        properties.sort_unstable();
        assert_eq!(required, properties);
        // A reply with all schema fields parses.
        assert!(parse_proposal(&reply("ignore", Value::Null, Value::Null, Value::Null)).is_ok());
    }
}
