mod common;

use bunsin_core::llm::{LlmError, StopReason, Usage};
use bunsin_core::ports::LlmClient;
use bunsin_llm::Wire;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn reply(content: Value, refusal: Value, finish_reason: &str) -> Value {
    json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content, "refusal": refusal},
            "finish_reason": finish_reason
        }],
        "usage": {"prompt_tokens": 11, "completion_tokens": 7, "total_tokens": 18}
    })
}

async fn complete(reply: Reply) -> Result<bunsin_core::llm::CompletionResponse, LlmError> {
    let (base, _) = serve_once(reply).await;
    client(Wire::OpenAiChat, format!("{base}/v1"))
        .complete(request("be brief", None))
        .await
}

#[tokio::test]
async fn sends_path_bearer_auth_and_body_with_schema() {
    let (base, captured) = serve_once(ok(reply(json!("hi"), Value::Null, "stop"))).await;
    // A trailing slash on the base URL must not double the slash in the path.
    client(Wire::OpenAiChat, format!("{base}/v1/"))
        .complete(request("be brief", Some(schema())))
        .await
        .unwrap();
    let captured = captured.await.unwrap();

    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/v1/chat/completions");
    assert_eq!(
        captured.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(captured.header("x-api-key"), None);
    assert_eq!(captured.header("content-type"), Some("application/json"));

    let body = &captured.body;
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_completion_tokens"], 321);
    let mut expected = vec![json!({"role": "system", "content": "be brief"})];
    expected.extend(expected_messages());
    assert_eq!(body["messages"], Value::Array(expected));
    assert_eq!(
        body["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {"name": "response", "schema": schema(), "strict": false}
        })
    );
    assert_no_sampling_or_thinking(body);
}

#[tokio::test]
async fn omits_response_format_and_empty_system() {
    let (base, captured) = serve_once(ok(reply(json!("hi"), Value::Null, "stop"))).await;
    client(Wire::OpenAiChat, format!("{base}/v1"))
        .complete(request("", None))
        .await
        .unwrap();
    let body = captured.await.unwrap().body;

    assert!(body.get("response_format").is_none(), "{body}");
    assert_eq!(body["messages"], Value::Array(expected_messages()));
    assert_no_sampling_or_thinking(&body);
}

#[tokio::test]
async fn maps_normal_end_and_usage() {
    let response = complete(ok(reply(json!("{\"answer\":\"42\"}"), Value::Null, "stop")))
        .await
        .unwrap();
    assert_eq!(response.content, "{\"answer\":\"42\"}");
    assert_eq!(response.stop, StopReason::End);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 11,
            output_tokens: 7
        }
    );
}

#[tokio::test]
async fn maps_length_to_max_tokens() {
    let response = complete(ok(reply(json!("{\"ans"), Value::Null, "length")))
        .await
        .unwrap();
    assert_eq!(response.stop, StopReason::MaxTokens);
    assert_eq!(response.content, "{\"ans");
}

#[tokio::test]
async fn maps_refusal_field_to_refusal() {
    // A structured-output refusal has finish_reason "stop", null content and a refusal text.
    let response = complete(ok(reply(
        Value::Null,
        json!("I can't help with that."),
        "stop",
    )))
    .await
    .unwrap();
    assert_eq!(response.stop, StopReason::Refusal);
    assert_eq!(response.content, "I can't help with that.");
}

#[tokio::test]
async fn maps_content_filter_to_refusal() {
    let response = complete(ok(reply(Value::Null, Value::Null, "content_filter")))
        .await
        .unwrap();
    assert_eq!(response.stop, StopReason::Refusal);
    assert_eq!(response.content, "");
}

#[tokio::test]
async fn rejects_tool_calls_finish_reason() {
    let error = complete(ok(reply(Value::Null, Value::Null, "tool_calls")))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("tool_calls")),
        "{error:?}"
    );
    assert_no_token(&error);
}

#[tokio::test]
async fn rejects_reply_without_choices() {
    let error = complete(ok(json!({
        "choices": [],
        "usage": {"prompt_tokens": 1, "completion_tokens": 0}
    })))
    .await
    .unwrap_err();
    assert!(matches!(error, LlmError::Malformed(_)), "{error:?}");
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_429_to_rate_limited() {
    let error = complete(Reply::Status(429, "{\"error\":\"slow down\"}".into()))
        .await
        .unwrap_err();
    assert_eq!(error, LlmError::RateLimited);
}

#[tokio::test]
async fn maps_500_to_http_with_body() {
    let error = complete(Reply::Status(500, "{\"error\":\"boom\"}".into()))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        LlmError::Http {
            status: 500,
            body: "{\"error\":\"boom\"}".into()
        }
    );
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_invalid_json_to_malformed() {
    let error = complete(Reply::Status(200, "{\"choices\": [".into()))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Malformed(_)), "{error:?}");
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_timeout_to_transport() {
    let (base, _) = serve_once(Reply::Hang).await;
    let client = client_with_timeout(Wire::OpenAiChat, base, Duration::from_millis(200));
    // The outer limit makes a missing client timeout fail the test instead of hanging it.
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        client.complete(request("be brief", None)),
    )
    .await
    .expect("the client did not time out")
    .unwrap_err();
    assert!(matches!(error, LlmError::Transport(_)), "{error:?}");
    assert_no_token(&error);
}
