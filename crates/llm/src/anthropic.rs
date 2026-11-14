//! Anthropic Messages wire format.

use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const PATH: &str = "/v1/messages";
/// The `anthropic-version` header value.
pub(crate) const VERSION: &str = "2026-10-21";

/// Builds the request body. It sends no sampling parameters and no thinking config.
pub(crate) fn request_body(model: &str, request: &CompletionRequest) -> Value {
    let mut body = json!({
        "model": model,
        "max_tokens": request.max_output_tokens,
        "messages": request.messages,
    });
    if !request.system.is_empty() {
        body["system"] = json!(request.system);
    }
    if let Some(schema) = &request.response_schema {
        // This format has no strict switch: the endpoint always constrains the reply to the schema.
        body["output_config"] = json!({"format": {"type": "json_schema", "schema": schema}});
    }
    body
}

#[derive(Deserialize)]
struct Reply {
    content: Vec<Block>,
    stop_reason: String,
    usage: ReplyUsage,
}

/// One content block. Only `text` blocks carry the answer; other types (such as `thinking`) are skipped.
#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type")]
    kind: String,
    text: Option<String>,
}

#[derive(Deserialize)]
struct ReplyUsage {
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
}

pub(crate) fn parse_reply(body: &[u8]) -> Result<CompletionResponse, LlmError> {
    let reply: Reply =
        serde_json::from_slice(body).map_err(|e| LlmError::Malformed(e.to_string()))?;
    let stop = match reply.stop_reason.as_str() {
        "end_turn" | "stop_sequence" => StopReason::End,
        // Both mean the output was cut off by a token limit.
        "max_tokens" | "model_context_window_exceeded" => StopReason::MaxTokens,
        "refusal" => StopReason::Refusal,
        other => {
            return Err(LlmError::Malformed(format!(
                "unexpected stop_reason `{other}`"
            )));
        }
    };
    let content = reply
        .content
        .into_iter()
        .filter(|block| block.kind == "text")
        .filter_map(|block| block.text)
        .collect();
    let usage = &reply.usage;
    // The total input is the sum of uncached, cache-write and cache-read tokens.
    let input_tokens = usage.input_tokens
        + usage.cache_creation_input_tokens.unwrap_or(0)
        + usage.cache_read_input_tokens.unwrap_or(0);
    Ok(CompletionResponse {
        content,
        stop,
        usage: Usage {
            input_tokens,
            output_tokens: usage.output_tokens,
        },
    })
}
