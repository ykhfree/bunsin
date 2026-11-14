//! OpenAI Chat Completions wire format.

use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason, Usage};
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const PATH: &str = "/chat/completions";

/// Builds the request body. It sends no sampling parameters.
pub(crate) fn request_body(model: &str, request: &CompletionRequest) -> Value {
    let mut messages = Vec::with_capacity(request.messages.len() + 1);
    if !request.system.is_empty() {
        messages.push(json!({"role": "system", "content": request.system}));
    }
    messages.extend(request.messages.iter().map(|message| json!(message)));
    let mut body = json!({
        "model": model,
        "messages": messages,
        "max_completion_tokens": request.max_output_tokens,
    });
    if let Some(schema) = &request.response_schema {
        // Strict mode requires `additionalProperties: false` on every object.
        // A schema with a free-form object (such as the proposal `args`) would be rejected, so strict is off.
        body["response_format"] = json!({
            "type": "json_schema",
            "json_schema": {"name": "response", "schema": schema, "strict": false},
        });
    }
    body
}

#[derive(Deserialize)]
struct Reply {
    choices: Vec<Choice>,
    usage: ReplyUsage,
}

#[derive(Deserialize)]
struct Choice {
    message: ReplyMessage,
    finish_reason: String,
}

#[derive(Deserialize)]
struct ReplyMessage {
    content: Option<String>,
    refusal: Option<String>,
}

#[derive(Deserialize)]
struct ReplyUsage {
    prompt_tokens: u64,
    completion_tokens: u64,
}

pub(crate) fn parse_reply(body: &[u8]) -> Result<CompletionResponse, LlmError> {
    let reply: Reply = serde_json::from_slice(body).map_err(|e| LlmError::Malformed(e.to_string()))?;
    let choice = reply
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| LlmError::Malformed("the reply has no choices".into()))?;
    let usage = Usage {
        input_tokens: reply.usage.prompt_tokens,
        output_tokens: reply.usage.completion_tokens,
    };
    // A structured-output refusal comes with `finish_reason: "stop"` and the text in `refusal`.
    if let Some(refusal) = choice.message.refusal {
        return Ok(CompletionResponse {
            content: refusal,
            stop: StopReason::Refusal,
            usage,
        });
    }
    let stop = match choice.finish_reason.as_str() {
        "stop" => StopReason::End,
        "length" => StopReason::MaxTokens,
        "content_filter" => StopReason::Refusal,
        other => {
            return Err(LlmError::Malformed(format!("unexpected finish_reason `{other}`")));
        }
    };
    Ok(CompletionResponse {
        content: choice.message.content.unwrap_or_default(),
        stop,
        usage,
    })
}
