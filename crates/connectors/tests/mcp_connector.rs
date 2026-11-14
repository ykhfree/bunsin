//! Integration tests for `McpConnector`. Each test starts its own test server.

#[path = "support/server.rs"]
mod server;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{Next, from_fn};
use axum::response::IntoResponse;
use bunsin_connectors::{McpConfig, McpConnector};
use bunsin_core::ports::{Connector, ConnectorError};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::{Map, Value, json};
use server::TestServer;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

const TOKEN: &str = "test-token-5f2c9a";

/// Serves `server` on 127.0.0.1 and returns the MCP URL.
/// If `token` is set, a request without `Authorization: Bearer <token>` gets 401.
async fn serve_http(server: TestServer, token: Option<&'static str>) -> String {
    let service = StreamableHttpService::new(
        move || Ok(server.clone()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let mut router = axum::Router::new().nest_service("/mcp", service);
    if let Some(token) = token {
        router = router.layer(from_fn(move |request: Request, next: Next| async move {
            let expected = format!("Bearer {token}");
            let header = request.headers().get(AUTHORIZATION);
            if header.is_some_and(|h| h.as_bytes() == expected.as_bytes()) {
                next.run(request).await
            } else {
                StatusCode::UNAUTHORIZED.into_response()
            }
        }));
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    format!("http://{addr}/mcp")
}

async fn connect_http(server: &TestServer) -> McpConnector {
    let url = serve_http(server.clone(), None).await;
    McpConnector::connect("slack", McpConfig::Http { url }, None)
        .await
        .expect("connects to the test server")
}

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("arguments must be a JSON object, got {other}"),
    }
}

fn calls(server: &TestServer) -> Vec<server::Call> {
    server.calls.lock().unwrap().clone()
}

/// The stdio test server that cargo builds from `examples/stdio_test_server.rs`.
fn stdio_server_path() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    // The test binary is in `<target>/<profile>/deps/`; examples are in `<target>/<profile>/examples/`.
    let path = exe
        .parent()
        .and_then(|deps| deps.parent())
        .unwrap()
        .join("examples")
        .join(format!("stdio_test_server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.is_file(),
        "{} is missing; run `cargo test -p bunsin-connectors` so cargo builds the example",
        path.display()
    );
    path
}

fn connect_error(result: Result<McpConnector, ConnectorError>) -> ConnectorError {
    match result {
        Ok(connector) => panic!("expected a connect error, got {connector:?}"),
        Err(e) => e,
    }
}

#[tokio::test]
async fn lists_every_page_with_schema_and_empty_missing_description() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;

    let tools = connector.list_tools().await.unwrap();

    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["echo", "plain", "fail", "reject", "whoami"]);
    // 5 tools at 2 per page: the connector followed the cursor twice.
    assert_eq!(tools.len().div_ceil(server::PAGE_SIZE), 3);
    assert_eq!(*server.list_requests.lock().unwrap(), 3);

    let echo = &tools[0];
    assert_eq!(
        echo.description,
        "Returns the arguments as structured content"
    );
    assert_eq!(echo.input_schema, Value::Object(server::echo_schema()));
    let plain = &tools[1];
    assert_eq!(plain.description, "");
    assert_eq!(plain.input_schema, json!({"type": "object"}));
    assert_eq!(connector.name(), "slack");
}

#[tokio::test]
async fn call_passes_arguments_unchanged_and_returns_structured_content() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;
    let sent = json!({
        "text": "배포 승인 요청 \"quoted\" \n line",
        "count": 9007199254740993_u64,
        "nested": {"list": [1, 2.5, null, true], "empty": {}}
    });

    let result = connector
        .call_tool("echo", args(sent.clone()))
        .await
        .unwrap();

    assert_eq!(result, sent);
    assert_eq!(calls(&server), [("echo".to_owned(), Some(args(sent)))]);
}

#[tokio::test]
async fn call_without_structured_content_returns_the_content_blocks() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;

    let result = connector.call_tool("plain", Map::new()).await.unwrap();

    assert_eq!(
        result,
        json!([
            {"type": "text", "text": "first"},
            {"type": "text", "text": "second"}
        ])
    );
}

#[tokio::test]
async fn tool_error_result_becomes_failed_with_the_server_text() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;

    let error = connector.call_tool("fail", Map::new()).await.unwrap_err();

    assert_eq!(error, ConnectorError::Failed("quota exceeded".into()));
    assert_eq!(calls(&server).len(), 1);
}

#[tokio::test]
async fn tool_error_without_text_becomes_failed_with_the_structured_content() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;

    let error = connector
        .call_tool("fail", args(json!({"no_text": true})))
        .await
        .unwrap_err();

    assert_eq!(error, ConnectorError::Failed(r#"{"code":"quota"}"#.into()));
}

#[tokio::test]
async fn json_rpc_error_on_a_call_becomes_failed() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;

    let error = connector.call_tool("reject", Map::new()).await.unwrap_err();

    assert_eq!(error, ConnectorError::Failed("channel is missing".into()));
}

#[tokio::test]
async fn unlisted_tool_is_rejected_without_a_server_call() {
    let server = TestServer::default();
    let connector = connect_http(&server).await;

    let error = connector
        .call_tool("delete_all", args(json!({"confirm": true})))
        .await
        .unwrap_err();

    assert_eq!(error, ConnectorError::UnknownTool("delete_all".into()));
    assert!(calls(&server).is_empty());
    // The check used a fresh listing.
    assert!(*server.list_requests.lock().unwrap() >= 1);
}

#[tokio::test]
async fn bearer_token_is_sent_and_kept_out_of_debug_and_errors() {
    let url = serve_http(TestServer::default(), Some(TOKEN)).await;
    let config = McpConfig::Http { url: url.clone() };

    let connector = McpConnector::connect("jira", config.clone(), Some(TOKEN))
        .await
        .expect("the server accepts the right token");
    // Every request carries the token, not only the handshake.
    assert_eq!(connector.list_tools().await.unwrap().len(), 5);
    let debug = format!("{connector:?}");
    assert!(debug.contains(&url), "Debug shows the config: {debug}");
    assert!(!debug.contains(TOKEN), "Debug leaks the token: {debug}");

    let wrong = "wrong-token-77e1";
    let error = connect_error(McpConnector::connect("jira", config.clone(), Some(wrong)).await);
    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    for text in [error.to_string(), format!("{error:?}")] {
        assert!(!text.contains(wrong), "the error leaks the token: {text}");
    }

    let error = connect_error(McpConnector::connect("jira", config, None).await);
    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
}

#[tokio::test]
async fn connecting_to_a_dead_address_is_a_transport_error() {
    // Bind and drop a listener, so nothing listens on the port.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let config = McpConfig::Http {
        url: format!("http://{addr}/mcp"),
    };

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        McpConnector::connect("confluence", config, Some(TOKEN)),
    )
    .await
    .expect("connect gives up within 10 s");

    let error = connect_error(result);
    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    assert!(!error.to_string().contains(TOKEN));
}

#[tokio::test]
async fn stdio_server_gets_args_and_env_and_answers_calls() {
    let config = McpConfig::Stdio {
        command: stdio_server_path(),
        args: vec!["--flag".into(), "two words".into()],
        env: BTreeMap::from([("BUNSIN_TEST_VALUE".into(), "from-config".into())]),
    };
    let connector = McpConnector::connect("local", config, None).await.unwrap();

    assert_eq!(connector.list_tools().await.unwrap().len(), 5);
    let result = connector.call_tool("whoami", Map::new()).await.unwrap();
    assert_eq!(
        result,
        json!({"args": ["--flag", "two words"], "value": "from-config"})
    );
}

#[tokio::test]
async fn stdio_rejects_a_relative_command_path() {
    // `sh` is on PATH, so only the absolute-path check gives this message.
    // Without the check, `sh -c "exit 0"` starts and exits, and the handshake fails.
    let config = McpConfig::Stdio {
        command: PathBuf::from("sh"),
        args: vec!["-c".into(), "exit 0".into()],
        env: BTreeMap::new(),
    };

    let error = connect_error(McpConnector::connect("local", config, None).await);

    match error {
        ConnectorError::Transport(message) => {
            assert!(message.contains("absolute path"), "{message}");
            assert!(message.contains("`sh`"), "{message}");
        }
        other => panic!("expected Transport, got {other:?}"),
    }
}

#[tokio::test]
async fn stdio_rejects_a_token() {
    let config = McpConfig::Stdio {
        command: stdio_server_path(),
        args: vec![],
        env: BTreeMap::new(),
    };

    let error = connect_error(McpConnector::connect("local", config, Some(TOKEN)).await);

    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    assert!(!error.to_string().contains(TOKEN));
}

#[test]
fn config_round_trips_through_json_and_has_no_token_field() {
    let http: McpConfig =
        serde_json::from_value(json!({"transport": "http", "url": "https://mcp.example/mcp"}))
            .unwrap();
    assert_eq!(
        http,
        McpConfig::Http {
            url: "https://mcp.example/mcp".into()
        }
    );

    let stdio: McpConfig =
        serde_json::from_value(json!({"transport": "stdio", "command": "/usr/local/bin/server"}))
            .unwrap();
    assert_eq!(
        stdio,
        McpConfig::Stdio {
            command: "/usr/local/bin/server".into(),
            args: vec![],
            env: BTreeMap::new(),
        }
    );
    let text = serde_json::to_value(&stdio).unwrap();
    assert_eq!(serde_json::from_value::<McpConfig>(text).unwrap(), stdio);

    let with_token = json!({"transport": "http", "url": "https://mcp.example/mcp", "token": "x"});
    assert!(serde_json::from_value::<McpConfig>(with_token).is_err());
}
