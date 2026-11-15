//! The OpenAI Responses wire against a local server that records the request.

mod common;

use bunsin_core::llm::{CompletionResponse, LlmError, StopReason, Usage};
use bunsin_core::ports::LlmClient;
use bunsin_llm::Wire;
use common::*;
use serde_json::{Value, json};

/// A reply in the shape of the "Text input" example of the API description:
/// the same fields, with `status`, `incomplete_details`, `error` and `output` set by the test.
fn reply(status: &str, incomplete: Value, error: Value, output: Value) -> Value {
    json!({
        "id": "resp_67ccd2bed1ec8190b14f964abc0542670bb6a6b452d3795b",
        "object": "response",
        "created_at": 1741476542,
        "status": status,
        "error": error,
        "incomplete_details": incomplete,
        "instructions": null,
        "max_output_tokens": null,
        "model": "gpt-4.1-2025-04-14",
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": {"effort": null, "summary": null},
        "store": false,
        "temperature": 1.0,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": 1.0,
        "truncation": "disabled",
        "usage": {
            "input_tokens": 36,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 87,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 123
        },
        "user": null,
        "metadata": {}
    })
}

fn message(parts: Value) -> Value {
    json!({"type": "message", "id": "msg_1", "status": "completed", "role": "assistant", "content": parts})
}

fn text(value: &str) -> Value {
    json!({"type": "output_text", "text": value, "annotations": []})
}

fn completed(output: Value) -> Reply {
    ok(reply("completed", Value::Null, Value::Null, output))
}

async fn complete(reply: Reply) -> Result<CompletionResponse, LlmError> {
    let (base, _) = serve_once(reply).await;
    client(Wire::OpenAiResponses, format!("{base}/v1"))
        .complete(request("be brief", None))
        .await
}

#[tokio::test]
async fn sends_path_bearer_auth_and_body_with_schema() {
    let (base, captured) = serve_once(completed(json!([message(json!([text("hi")]))]))).await;
    // A trailing slash on the base URL must not double the slash in the path.
    client(Wire::OpenAiResponses, format!("{base}/v1/"))
        .complete(request("be brief", Some(schema())))
        .await
        .unwrap();
    let captured = captured.await.unwrap();

    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/v1/responses");
    assert_eq!(
        captured.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(captured.header("x-api-key"), None);
    assert_eq!(captured.header("content-type"), Some("application/json"));

    let body = &captured.body;
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_output_tokens"], 321);
    assert_eq!(body["instructions"], "be brief");
    // The system text is not repeated as a message.
    assert_eq!(body["input"], Value::Array(expected_messages()));
    // The API must not keep the reply.
    assert_eq!(body["store"], false);
    assert_eq!(
        body["text"],
        json!({"format": {"type": "json_schema", "name": "response", "schema": schema(), "strict": false}})
    );
    assert_no_sampling_or_thinking(body);
}

#[tokio::test]
async fn omits_text_format_and_empty_instructions() {
    let (base, captured) = serve_once(completed(json!([message(json!([text("hi")]))]))).await;
    client(Wire::OpenAiResponses, format!("{base}/v1"))
        .complete(request("", None))
        .await
        .unwrap();
    let body = captured.await.unwrap().body;
    assert!(body.get("text").is_none(), "{body}");
    assert!(body.get("instructions").is_none(), "{body}");
    assert_eq!(body["input"], Value::Array(expected_messages()));
}

#[tokio::test]
async fn joins_the_text_of_messages_and_skips_other_items() {
    let output = json!([
        {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": "NOT-THE-ANSWER"}]},
        message(json!([text("{\"answer\":"), text("\"42\"}")])),
    ]);
    let response = complete(completed(output)).await.unwrap();
    assert_eq!(response.content, "{\"answer\":\"42\"}");
    assert_eq!(response.stop, StopReason::End);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 36,
            output_tokens: 87
        }
    );
}

#[tokio::test]
async fn maps_the_reasons_of_an_incomplete_response() {
    let cut = json!([message(json!([text("{\"ans")]))]);
    let limit = reply("incomplete", json!({"reason": "max_output_tokens"}), Value::Null, cut);
    let response = complete(ok(limit)).await.unwrap();
    assert_eq!(
        (response.stop, response.content.as_str()),
        (StopReason::MaxTokens, "{\"ans")
    );

    let filtered = reply(
        "incomplete",
        json!({"reason": "content_filter"}),
        Value::Null,
        json!([]),
    );
    let response = complete(ok(filtered)).await.unwrap();
    assert_eq!((response.stop, response.content.as_str()), (StopReason::Refusal, ""));

    let unknown = reply("incomplete", json!({"reason": "something_new"}), Value::Null, json!([]));
    let error = complete(ok(unknown)).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("something_new")),
        "{error:?}"
    );
}

#[tokio::test]
async fn maps_a_refusal_part_to_refusal() {
    let output = json!([message(
        json!([{"type": "refusal", "refusal": "I can't help with that."}])
    )]);
    let response = complete(completed(output)).await.unwrap();
    assert_eq!(response.stop, StopReason::Refusal);
    assert_eq!(response.content, "I can't help with that.");
}

#[tokio::test]
async fn a_failed_response_with_http_200_is_an_error_with_its_reason() {
    let failed = reply(
        "failed",
        Value::Null,
        json!({"code": "server_error", "message": "The model failed to generate a response."}),
        json!([]),
    );
    let error = complete(ok(failed)).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("The model failed to generate a response.")),
        "{error:?}"
    );
    // A status this client does not know is refused too, and no answer is made up.
    let running = reply("in_progress", Value::Null, Value::Null, json!([]));
    let error = complete(ok(running)).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("in_progress")),
        "{error:?}"
    );
}

#[tokio::test]
async fn the_token_is_removed_from_an_error_the_endpoint_echoes() {
    let failed = reply(
        "failed",
        Value::Null,
        json!({"code": "invalid_api_key", "message": format!("Incorrect API key provided: {TOKEN}")}),
        json!([]),
    );
    let error = complete(ok(failed)).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("Incorrect API key")),
        "{error:?}"
    );
    assert_no_token(&error);
}

#[tokio::test]
async fn rejects_a_reply_without_usage_or_with_invalid_json() {
    let mut no_usage = reply(
        "completed",
        Value::Null,
        Value::Null,
        json!([message(json!([text("hi")]))]),
    );
    no_usage.as_object_mut().unwrap().remove("usage");
    let error = complete(ok(no_usage)).await.unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("usage")),
        "{error:?}"
    );

    let error = complete(ok(json!({"object": "response"}))).await.unwrap_err();
    assert!(matches!(error, LlmError::Malformed(_)), "{error:?}");
}
