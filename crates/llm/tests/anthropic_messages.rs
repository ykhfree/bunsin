mod common;

use bunsin_core::llm::{CompletionResponse, LlmError, StopReason, Usage};
use bunsin_core::ports::LlmClient;
use bunsin_llm::Wire;
use common::*;
use serde_json::{Value, json};
use std::time::Duration;

fn reply(content: Value, stop_reason: &str) -> Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": MODEL,
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {"input_tokens": 11, "output_tokens": 7}
    })
}

fn text(value: &str) -> Value {
    json!([{"type": "text", "text": value}])
}

async fn complete(reply: Reply) -> Result<CompletionResponse, LlmError> {
    let (base, _) = serve_once(reply).await;
    client(Wire::AnthropicMessages, base)
        .complete(request("be brief", None))
        .await
}

#[tokio::test]
async fn sends_path_key_version_and_body_with_schema() {
    let (base, captured) = serve_once(ok(reply(text("hi"), "end_turn"))).await;
    client(Wire::AnthropicMessages, format!("{base}/"))
        .complete(request("be brief", Some(schema())))
        .await
        .unwrap();
    let captured = captured.await.unwrap();

    assert_eq!(captured.method, "POST");
    assert_eq!(captured.path, "/v1/messages");
    assert_eq!(captured.header("x-api-key"), Some(TOKEN));
    assert_eq!(captured.header("anthropic-version"), Some("2026-10-21"));
    assert_eq!(captured.header("authorization"), None);
    assert_eq!(captured.header("content-type"), Some("application/json"));

    let body = &captured.body;
    assert_eq!(body["model"], MODEL);
    assert_eq!(body["max_tokens"], 321);
    assert_eq!(body["system"], "be brief");
    assert_eq!(body["messages"], Value::Array(expected_messages()));
    assert_eq!(
        body["output_config"],
        json!({"format": {"type": "json_schema", "schema": schema()}})
    );
    assert!(body.get("response_format").is_none(), "{body}");
    assert_no_sampling_or_thinking(body);
}

#[tokio::test]
async fn omits_output_config_and_empty_system() {
    let (base, captured) = serve_once(ok(reply(text("hi"), "end_turn"))).await;
    client(Wire::AnthropicMessages, base)
        .complete(request("", None))
        .await
        .unwrap();
    let body = captured.await.unwrap().body;

    assert!(body.get("output_config").is_none(), "{body}");
    assert!(body.get("system").is_none(), "{body}");
    assert_no_sampling_or_thinking(&body);
}

#[tokio::test]
async fn joins_text_blocks_and_skips_thinking() {
    let content = json!([
        {"type": "thinking", "thinking": "", "signature": "sig"},
        {"type": "text", "text": "{\"answer\":"},
        {"type": "text", "text": "\"42\"}"}
    ]);
    let response = complete(ok(reply(content, "end_turn"))).await.unwrap();
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
async fn adds_cache_tokens_to_input_usage() {
    let mut body = reply(text("hi"), "end_turn");
    body["usage"] = json!({
        "input_tokens": 10,
        "cache_creation_input_tokens": 200,
        "cache_read_input_tokens": 3000,
        "output_tokens": 5
    });
    let response = complete(ok(body)).await.unwrap();
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 3210,
            output_tokens: 5
        }
    );
}

#[tokio::test]
async fn maps_token_limit_stops_to_max_tokens() {
    for stop_reason in ["max_tokens", "model_context_window_exceeded"] {
        let response = complete(ok(reply(text("{\"ans"), stop_reason)))
            .await
            .unwrap();
        assert_eq!(response.stop, StopReason::MaxTokens, "{stop_reason}");
        assert_eq!(response.content, "{\"ans");
    }
}

#[tokio::test]
async fn maps_refusal_with_empty_content() {
    let response = complete(ok(reply(json!([]), "refusal"))).await.unwrap();
    assert_eq!(response.stop, StopReason::Refusal);
    assert_eq!(response.content, "");
}

#[tokio::test]
async fn rejects_tool_use_stop() {
    let error = complete(ok(reply(text("hi"), "tool_use")))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, LlmError::Malformed(text) if text.contains("tool_use")),
        "{error:?}"
    );
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_429_to_rate_limited() {
    let error = complete(Reply::Status(429, "{\"type\":\"error\"}".into()))
        .await
        .unwrap_err();
    assert_eq!(error, LlmError::RateLimited);
}

#[tokio::test]
async fn maps_500_to_http_with_body() {
    let body = "{\"type\":\"error\",\"error\":{\"type\":\"api_error\"}}";
    let error = complete(Reply::Status(500, body.into())).await.unwrap_err();
    assert_eq!(
        error,
        LlmError::Http {
            status: 500,
            body: body.into()
        }
    );
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_invalid_json_to_malformed() {
    let error = complete(Reply::Status(200, "not json".into()))
        .await
        .unwrap_err();
    assert!(matches!(error, LlmError::Malformed(_)), "{error:?}");
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_missing_usage_to_malformed() {
    let mut body = reply(text("hi"), "end_turn");
    body.as_object_mut().unwrap().remove("usage");
    let error = complete(ok(body)).await.unwrap_err();
    assert!(matches!(error, LlmError::Malformed(_)), "{error:?}");
    assert_no_token(&error);
}

#[tokio::test]
async fn maps_timeout_to_transport() {
    let (base, _) = serve_once(Reply::Hang).await;
    let client = client_with_timeout(Wire::AnthropicMessages, base, Duration::from_millis(200));
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
