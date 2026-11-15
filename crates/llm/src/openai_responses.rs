//! OpenAI Responses wire format: `POST {base_url}/responses`.
//! Field names are from the OpenAPI description of the API (openai/openai-openapi), read on 2026-11-15.

use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const PATH: &str = "/responses";

/// Builds the request body. It sends no sampling parameters.
pub(crate) fn request_body(model: &str, request: &CompletionRequest) -> Value {
    let input: Vec<Value> = request.messages.iter().map(|message| json!(message)).collect();
    let mut body = json!({
        "model": model,
        "input": input,
        "max_output_tokens": request.max_output_tokens,
        // The API keeps a reply for later retrieval unless told not to. The agent never retrieves one.
        "store": false,
    });
    if !request.system.is_empty() {
        body["instructions"] = json!(request.system);
    }
    if let Some(schema) = &request.response_schema {
        // Strict mode requires `additionalProperties: false` on every object, so strict is off,
        // as on the Chat Completions wire.
        body["text"] = json!({
            "format": {"type": "json_schema", "name": "response", "schema": schema, "strict": false},
        });
    }
    body
}

#[derive(Deserialize)]
struct Reply {
    status: String,
    error: Option<ReplyError>,
    incomplete_details: Option<Incomplete>,
    #[serde(default)]
    output: Vec<Value>,
    usage: Option<ReplyUsage>,
}

#[derive(Deserialize)]
struct ReplyError {
    message: String,
}

#[derive(Deserialize)]
struct Incomplete {
    reason: String,
}

#[derive(Deserialize)]
struct ReplyUsage {
    input_tokens: u64,
    output_tokens: u64,
}

pub(crate) fn parse_reply(body: &[u8]) -> Result<CompletionResponse, LlmError> {
    let reply: Reply = serde_json::from_slice(body).map_err(|e| LlmError::Malformed(e.to_string()))?;
    let stop = match (reply.status.as_str(), &reply.incomplete_details) {
        ("completed", _) => StopReason::End,
        ("incomplete", Some(details)) => match details.reason.as_str() {
            "max_output_tokens" => StopReason::MaxTokens,
            "content_filter" => StopReason::Refusal,
            other => return Err(LlmError::Malformed(format!("unexpected incomplete reason `{other}`"))),
        },
        // A failed response arrives with HTTP 200. It carries the reason in `error`.
        ("failed", _) => {
            let reason = reply
                .error
                .map_or_else(|| "no reason given".to_owned(), |error| error.message);
            return Err(LlmError::Malformed(format!("the response failed: {reason}")));
        }
        (other, _) => return Err(LlmError::Malformed(format!("unexpected status `{other}`"))),
    };
    let usage = reply
        .usage
        .map(|usage| Usage {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        })
        .ok_or_else(|| LlmError::Malformed("the reply has no usage".into()))?;
    // The output also holds items that are not messages, such as reasoning. Only message text counts.
    let mut content = String::new();
    for part in reply
        .output
        .iter()
        .filter(|item| item["type"] == "message")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
    {
        match part["type"].as_str() {
            Some("output_text") => content.push_str(part["text"].as_str().unwrap_or_default()),
            Some("refusal") => {
                return Ok(CompletionResponse {
                    content: part["refusal"].as_str().unwrap_or_default().to_owned(),
                    stop: StopReason::Refusal,
                    usage,
                });
            }
            _ => {}
        }
    }
    Ok(CompletionResponse { content, stop, usage })
}
