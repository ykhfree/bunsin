//! A one-request HTTP stub on 127.0.0.1 and shared test values.
//! Each test starts its own stub, so tests share no state.

#![allow(dead_code)] // Each test binary uses a different part of this module.

use bunsin_core::llm::{CompletionRequest, LlmError, Message, Role};
use bunsin_llm::{ApiToken, HttpLlmClient, LlmConfig, Wire};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub const TOKEN: &str = "sk-test-SECRET-7f3a9c";
pub const MODEL: &str = "test-model";

/// What the stub received.
#[derive(Debug)]
pub struct Captured {
    pub method: String,
    pub path: String,
    /// Header names are lowercase.
    pub headers: Vec<(String, String)>,
    pub body: Value,
}

impl Captured {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

pub enum Reply {
    /// Answer with this status and body.
    Status(u16, String),
    /// Answer `302 Found` with this `location` header.
    Redirect(String),
    /// Read the request, then never answer.
    Hang,
}

pub fn ok(body: Value) -> Reply {
    Reply::Status(200, body.to_string())
}

/// Starts a stub that serves one request. Returns its base URL and the captured request.
pub async fn serve_once(reply: Reply) -> (String, oneshot::Receiver<Captured>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let captured = read_request(&mut socket).await;
        let _ = sender.send(captured);
        let (status, extra_header, body) = match reply {
            Reply::Status(status, body) => (status, String::new(), body),
            Reply::Redirect(location) => (302, format!("location: {location}\r\n"), String::new()),
            Reply::Hang => {
                // Keep the socket open until the test runtime ends.
                tokio::time::sleep(Duration::from_secs(3600)).await;
                return;
            }
        };
        let head = format!(
            "HTTP/1.1 {status} Stub\r\ncontent-type: application/json\r\n{extra_header}content-length: {}\r\nconnection: close\r\n\r\n",
            body.len()
        );
        socket.write_all(head.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    (base, receiver)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Captured {
    let mut buffer = Vec::new();
    let head_end = loop {
        if let Some(at) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "the client closed before the headers ended");
        buffer.extend_from_slice(&chunk[..read]);
    };
    let head = String::from_utf8(buffer[..head_end].to_vec()).unwrap();
    let mut lines = head.split("\r\n");
    let mut request_line = lines.next().unwrap().split(' ');
    let method = request_line.next().unwrap().to_string();
    let path = request_line.next().unwrap().to_string();
    let headers: Vec<(String, String)> = lines
        .map(|line| {
            let (name, value) = line.split_once(':').unwrap();
            (name.trim().to_ascii_lowercase(), value.trim().to_string())
        })
        .collect();
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .map_or(0, |(_, value)| value.parse().unwrap());
    let mut body = buffer[head_end + 4..].to_vec();
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let read = socket.read(&mut chunk).await.unwrap();
        assert!(read > 0, "the client closed before the body ended");
        body.extend_from_slice(&chunk[..read]);
    }
    Captured {
        method,
        path,
        headers,
        body: serde_json::from_slice(&body).unwrap(),
    }
}

pub fn client(wire: Wire, base_url: String) -> HttpLlmClient {
    client_with_timeout(wire, base_url, Duration::from_secs(10))
}

pub fn client_with_timeout(wire: Wire, base_url: String, timeout: Duration) -> HttpLlmClient {
    let config = LlmConfig {
        wire,
        base_url,
        model: MODEL.into(),
        timeout,
        workspace_id: None,
    };
    HttpLlmClient::new(config, ApiToken::new(TOKEN)).unwrap()
}

pub fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["answer"],
        "properties": {"answer": {"type": "string"}}
    })
}

pub fn request(system: &str, response_schema: Option<Value>) -> CompletionRequest {
    CompletionRequest {
        system: system.into(),
        messages: vec![
            Message {
                role: Role::User,
                content: "first question".into(),
            },
            Message {
                role: Role::Assistant,
                content: "first answer".into(),
            },
            Message {
                role: Role::User,
                content: "second question".into(),
            },
        ],
        response_schema,
        max_output_tokens: 321,
    }
}

/// The messages of `request()` in the shape both wires use.
pub fn expected_messages() -> Vec<Value> {
    vec![
        json!({"role": "user", "content": "first question"}),
        json!({"role": "assistant", "content": "first answer"}),
        json!({"role": "user", "content": "second question"}),
    ]
}

/// Fails if the token shows in the error's `Debug` or `Display` text.
pub fn assert_no_token(error: &LlmError) {
    let debug = format!("{error:?}");
    let display = error.to_string();
    assert!(!debug.contains(TOKEN), "token in Debug: {debug}");
    assert!(!display.contains(TOKEN), "token in Display: {display}");
}

/// Fails if the body has a sampling or thinking field.
pub fn assert_no_sampling_or_thinking(body: &Value) {
    for key in ["temperature", "top_p", "top_k", "thinking"] {
        assert!(body.get(key).is_none(), "body has `{key}`: {body}");
    }
}
