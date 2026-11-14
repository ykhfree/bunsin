//! Checks that apply to both wires: the token never leaks, and shared error mapping.

mod common;

use bunsin_core::llm::LlmError;
use bunsin_core::ports::LlmClient;
use bunsin_llm::{ApiToken, HttpLlmClient, LlmConfig, MAX_ERROR_BODY_BYTES, Wire};
use common::*;
use serde_json::json;
use std::time::Duration;

const WIRES: [Wire; 2] = [Wire::OpenAiChat, Wire::AnthropicMessages];

#[test]
fn debug_output_hides_the_token() {
    assert!(!format!("{:?}", ApiToken::new(TOKEN)).contains(TOKEN));
    for wire in WIRES {
        let client = client(wire, "http://127.0.0.1:9".into());
        let debug = format!("{client:?}");
        assert!(!debug.contains(TOKEN), "{debug}");
        // The Debug text still shows the non-secret config.
        assert!(debug.contains(MODEL), "{debug}");
    }
}

#[tokio::test]
async fn redacts_a_token_echoed_in_an_error_body() {
    for wire in WIRES {
        let body = format!("{{\"error\":\"invalid key {TOKEN}\"}}");
        let (base, _) = serve_once(Reply::Status(401, body)).await;
        let error = client(wire, base)
            .complete(request("be brief", None))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            LlmError::Http {
                status: 401,
                body: "{\"error\":\"invalid key [REDACTED]\"}".into()
            },
            "{wire:?}"
        );
        assert_no_token(&error);
    }
}

#[tokio::test]
async fn redacts_a_token_quoted_in_a_malformed_reply() {
    // serde_json quotes the offending string value in its error text.
    let replies = [
        (Wire::OpenAiChat, json!({"choices": [], "usage": TOKEN})),
        (
            Wire::AnthropicMessages,
            json!({"content": [], "stop_reason": "end_turn", "usage": TOKEN}),
        ),
        // The unknown stop reason is quoted by this crate's own message.
        (
            Wire::AnthropicMessages,
            json!({"content": [], "stop_reason": TOKEN, "usage": {"input_tokens": 1, "output_tokens": 1}}),
        ),
    ];
    for (wire, reply) in replies {
        let (base, _) = serve_once(ok(reply)).await;
        let error = client(wire, base)
            .complete(request("be brief", None))
            .await
            .unwrap_err();
        let LlmError::Malformed(text) = &error else {
            panic!("expected Malformed, got {error:?}");
        };
        assert!(text.contains("[REDACTED]"), "{text}");
        assert_no_token(&error);
    }
}

#[tokio::test]
async fn truncates_a_long_multibyte_error_body() {
    let (base, _) = serve_once(Reply::Status(502, "한".repeat(5000))).await;
    let error = client(Wire::AnthropicMessages, base)
        .complete(request("be brief", None))
        .await
        .unwrap_err();
    assert_no_token(&error);
    let LlmError::Http { status, body } = error else {
        panic!("expected Http, got {error:?}");
    };
    assert_eq!(status, 502);
    let kept = body.strip_suffix(" [truncated]").expect(&body);
    assert!(kept.len() <= MAX_ERROR_BODY_BYTES);
    assert!(kept.len() > MAX_ERROR_BODY_BYTES - 3);
    assert!(kept.chars().all(|c| c == '한'));
}

#[tokio::test]
async fn returns_a_redirect_as_http_without_following_it() {
    for wire in WIRES {
        // A different host name, so a followed redirect would be cross-host.
        let (target, mut reached) = serve_once(ok(json!({}))).await;
        let target = target.replace("127.0.0.1", "localhost");
        let (base, _) = serve_once(Reply::Redirect(format!("{target}/elsewhere"))).await;
        let error = client(wire, base)
            .complete(request("be brief", None))
            .await
            .unwrap_err();
        assert_eq!(
            error,
            LlmError::Http {
                status: 302,
                body: String::new()
            },
            "{wire:?}"
        );
        assert!(reached.try_recv().is_err(), "the redirect target got a request");
        assert_no_token(&error);
    }
}

#[tokio::test]
async fn maps_a_refused_connection_to_transport() {
    // Bind a port, then free it, so nothing listens there.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    for wire in WIRES {
        let error = client(wire, base.clone())
            .complete(request("be brief", None))
            .await
            .unwrap_err();
        assert!(matches!(error, LlmError::Transport(_)), "{error:?}");
        assert_no_token(&error);
    }
}

#[test]
fn rejects_a_token_that_is_not_a_header_value() {
    let secret = format!("{TOKEN}\nx-injected: 1");
    for wire in WIRES {
        let config = LlmConfig {
            wire,
            base_url: "http://127.0.0.1:9".into(),
            model: MODEL.into(),
            timeout: Duration::from_secs(1),
        };
        let error = HttpLlmClient::new(config, ApiToken::new(secret.clone())).unwrap_err();
        assert!(matches!(error, LlmError::Transport(_)), "{error:?}");
        assert_no_token(&error);
    }
}
