//! `LlmClient` over HTTP for a user-configured endpoint.
//! The harness sees only `CompletionRequest` and `CompletionResponse`, never the wire format.

mod anthropic;
mod openai;

use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError};
use bunsin_core::ports::{BoxFuture, LlmClient};
use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::Duration;

/// The longest error body kept in `LlmError::Http`, in bytes.
pub const MAX_ERROR_BODY_BYTES: usize = 2048;

const REDACTED: &str = "[REDACTED]";

/// The HTTP API shape that the endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wire {
    /// OpenAI Chat Completions: `POST {base_url}/chat/completions`.
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    /// Anthropic Messages: `POST {base_url}/v1/messages`.
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
}

#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub wire: Wire,
    /// The URL before the wire path. Follow each vendor's SDK convention:
    /// `https://api.openai.com/v1` for `OpenAiChat`, `https://api.anthropic.com` for `AnthropicMessages`.
    pub base_url: String,
    pub model: String,
    /// The limit for one whole request, from connect to the last body byte.
    pub timeout: Duration,
}

/// The endpoint credential. `Debug` never shows the value.
#[derive(Clone)]
pub struct ApiToken(String);

impl ApiToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }
}

impl fmt::Debug for ApiToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiToken(")?;
        f.write_str(REDACTED)?;
        f.write_str(")")
    }
}

#[derive(Debug)]
pub struct HttpLlmClient {
    config: LlmConfig,
    token: ApiToken,
    /// The auth header value for `config.wire`. It is marked sensitive, so its `Debug` hides it.
    auth: HeaderValue,
    http: reqwest::Client,
}

impl HttpLlmClient {
    /// Fails with `Transport` if the token is not a valid header value or the HTTP client cannot start.
    pub fn new(config: LlmConfig, token: ApiToken) -> Result<Self, LlmError> {
        let raw = match config.wire {
            Wire::OpenAiChat => format!("Bearer {}", token.0),
            Wire::AnthropicMessages => token.0.clone(),
        };
        let mut auth = HeaderValue::from_str(&raw)
            .map_err(|_| LlmError::Transport("the token is not a valid HTTP header value".into()))?;
        auth.set_sensitive(true);
        // Redirects are not followed: reqwest keeps `x-api-key` on a cross-host redirect.
        // A 3xx reply becomes `LlmError::Http`.
        let http = reqwest::Client::builder()
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| LlmError::Transport(error_chain(&e)))?;
        Ok(Self {
            config,
            token,
            auth,
            http,
        })
    }

    async fn send(&self, request: &CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let base = self.config.base_url.trim_end_matches('/');
        let model = &self.config.model;
        let builder = match self.config.wire {
            Wire::OpenAiChat => self
                .http
                .post(format!("{base}{}", openai::PATH))
                .header(AUTHORIZATION, self.auth.clone())
                .body(openai::request_body(model, request).to_string()),
            Wire::AnthropicMessages => self
                .http
                .post(format!("{base}{}", anthropic::PATH))
                .header("x-api-key", self.auth.clone())
                .header("anthropic-version", anthropic::VERSION)
                .body(anthropic::request_body(model, request).to_string()),
        };
        let response = builder
            .header(CONTENT_TYPE, "application/json")
            .send()
            .await
            .map_err(|e| LlmError::Transport(error_chain(&e)))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Err(LlmError::RateLimited);
        }
        let body = response
            .bytes()
            .await
            .map_err(|e| LlmError::Transport(error_chain(&e)))?;
        if !status.is_success() {
            return Err(LlmError::Http {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
        }
        match self.config.wire {
            Wire::OpenAiChat => openai::parse_reply(&body),
            Wire::AnthropicMessages => anthropic::parse_reply(&body),
        }
    }

    /// Removes the token from every text in the error, then shortens a long HTTP body.
    fn scrub(&self, error: LlmError) -> LlmError {
        let redact = |text: String| {
            if self.token.0.is_empty() {
                text
            } else {
                text.replace(&self.token.0, REDACTED)
            }
        };
        match error {
            LlmError::RateLimited => LlmError::RateLimited,
            LlmError::Http { status, body } => LlmError::Http {
                status,
                body: truncate(redact(body)),
            },
            LlmError::Transport(text) => LlmError::Transport(redact(text)),
            LlmError::Malformed(text) => LlmError::Malformed(redact(text)),
        }
    }
}

impl LlmClient for HttpLlmClient {
    fn complete(&self, request: CompletionRequest) -> BoxFuture<'_, Result<CompletionResponse, LlmError>> {
        Box::pin(async move { self.send(&request).await.map_err(|e| self.scrub(e)) })
    }
}

/// Joins an error and its sources, because reqwest puts the cause (timeout, refused) in a source.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Cuts `text` to at most `MAX_ERROR_BODY_BYTES` on a char boundary and marks the cut.
fn truncate(mut text: String) -> String {
    if text.len() > MAX_ERROR_BODY_BYTES {
        text.truncate(text.floor_char_boundary(MAX_ERROR_BODY_BYTES));
        text.push_str(" [truncated]");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_uses_kebab_case_names() {
        for (wire, name) in [
            (Wire::OpenAiChat, "\"openai-chat\""),
            (Wire::AnthropicMessages, "\"anthropic-messages\""),
        ] {
            assert_eq!(serde_json::to_string(&wire).unwrap(), name);
            assert_eq!(serde_json::from_str::<Wire>(name).unwrap(), wire);
        }
        assert!(serde_json::from_str::<Wire>("\"openai-responses\"").is_err());
    }

    #[test]
    fn truncate_keeps_short_text_and_cuts_on_a_char_boundary() {
        assert_eq!(truncate("short".into()), "short");
        let exact = "a".repeat(MAX_ERROR_BODY_BYTES);
        assert_eq!(truncate(exact.clone()), exact);
        // '한' is 3 bytes, so the limit falls inside a char.
        let cut = truncate("한".repeat(MAX_ERROR_BODY_BYTES));
        let kept = cut.strip_suffix(" [truncated]").unwrap();
        assert!(kept.len() <= MAX_ERROR_BODY_BYTES);
        assert!(kept.len() > MAX_ERROR_BODY_BYTES - 3);
        assert!(kept.chars().all(|c| c == '한'));
    }
}
