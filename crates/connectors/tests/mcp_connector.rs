//! Integration tests for `McpConnector`. Each test starts its own test server.

#[path = "support/server.rs"]
mod server;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::AUTHORIZATION;
use axum::middleware::{Next, from_fn};
use axum::response::IntoResponse;
use bunsin_connectors::{McpConfig, McpConnector, TokenAuth};
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
    assert_eq!(echo.description, "Returns the arguments as structured content");
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

    let result = connector.call_tool("echo", args(sent.clone())).await.unwrap();

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

    let connector = McpConnector::connect("jira", config.clone(), Some(TokenAuth::bearer(TOKEN)))
        .await
        .expect("the server accepts the right token");
    // Every request carries the token, not only the handshake.
    assert_eq!(connector.list_tools().await.unwrap().len(), 5);
    let debug = format!("{connector:?}");
    assert!(debug.contains(&url), "Debug shows the config: {debug}");
    assert!(!debug.contains(TOKEN), "Debug leaks the token: {debug}");

    let wrong = "wrong-token-77e1";
    let error = connect_error(McpConnector::connect("jira", config.clone(), Some(TokenAuth::bearer(wrong))).await);
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
        McpConnector::connect("confluence", config, Some(TokenAuth::bearer(TOKEN))),
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
    assert_eq!(result, json!({"args": ["--flag", "two words"], "value": "from-config"}));
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

    let error = connect_error(McpConnector::connect("local", config, Some(TokenAuth::bearer(TOKEN))).await);

    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    assert!(!error.to_string().contains(TOKEN));
}

#[test]
fn config_round_trips_through_json_and_has_no_token_field() {
    let http: McpConfig =
        serde_json::from_value(json!({"transport": "http", "url": "https://mcp.example/mcp"})).unwrap();
    assert_eq!(
        http,
        McpConfig::Http {
            url: "https://mcp.example/mcp".into()
        }
    );

    let stdio: McpConfig =
        serde_json::from_value(json!({"transport": "stdio", "command": "/usr/local/bin/server"})).unwrap();
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

#[tokio::test]
async fn a_redirect_is_not_followed_and_the_token_does_not_reach_the_other_host() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // The target is a working MCP server. It counts the requests that carry a bearer token.
    let with_token = Arc::new(AtomicUsize::new(0));
    let service = StreamableHttpService::new(
        || Ok(TestServer::default()),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    let counter = with_token.clone();
    let target =
        axum::Router::new()
            .nest_service("/mcp", service)
            .layer(from_fn(move |request: Request, next: Next| {
                if request.headers().contains_key(AUTHORIZATION) {
                    counter.fetch_add(1, Ordering::SeqCst);
                }
                next.run(request)
            }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target_url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, target).await });

    // The configured server answers every request with a redirect to the target.
    let location = target_url.clone();
    let redirector = axum::Router::new().fallback(move || {
        let location = location.clone();
        async move { (StatusCode::TEMPORARY_REDIRECT, [("location", location)]).into_response() }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, redirector).await });

    // The target itself works, so only the refusal to follow the redirect can fail the connect.
    // This direct connection sends no token, so it does not count.
    let direct = McpConnector::connect("slack", McpConfig::Http { url: target_url }, None).await;
    assert!(direct.is_ok());

    let result = tokio::time::timeout(
        Duration::from_secs(10),
        McpConnector::connect("slack", McpConfig::Http { url }, Some(TokenAuth::bearer(TOKEN))),
    )
    .await
    .expect("connect returns");
    let error = connect_error(result);
    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    assert_eq!(with_token.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_call_that_does_not_end_in_time_is_cancelled_on_the_server() {
    let server = TestServer::default();
    *server.hang.lock().unwrap() = true;
    let limit = Duration::from_millis(300);
    let connector = connect_http(&server).await.with_call_timeout(limit);

    let started = std::time::Instant::now();
    // The outer limit only stops a hung test. The connector must end the call first.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        connector.call_tool("echo", args(json!({"text": "hi"}))),
    )
    .await
    .expect("the call must end at the connector's time limit")
    .unwrap_err();
    assert!(started.elapsed() >= limit, "{:?}", started.elapsed());
    match &error {
        ConnectorError::Transport(text) => assert!(
            text.contains("`echo` did not end in 0.3 s") && text.contains("may have done it"),
            "{text}"
        ),
        other => panic!("expected a transport error, got {other:?}"),
    }

    // The server hears that the client gave up on the call.
    wait_for_cancels(&server, 1).await;

    // The session stays usable.
    *server.hang.lock().unwrap() = false;
    let result = connector
        .call_tool("echo", args(json!({"text": "again"})))
        .await
        .unwrap();
    assert_eq!(result, json!({"text": "again"}));
}

/// Waits until the server has seen `n` cancelled requests. Fails after 5 seconds.
async fn wait_for_cancels(server: &TestServer, n: usize) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while *server.cancelled.lock().unwrap() < n {
        assert!(std::time::Instant::now() < deadline, "the server was not told");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn a_tool_list_that_does_not_end_in_time_is_cancelled_too() {
    let server = TestServer::default();
    let limit = Duration::from_millis(300);
    let connector = connect_http(&server).await.with_call_timeout(limit);
    *server.hang_list.lock().unwrap() = true;

    // The list alone, as the engine asks for it before a call.
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_secs(10), connector.list_tools())
        .await
        .expect("the list must end at the connector's time limit")
        .unwrap_err();
    assert!(started.elapsed() >= limit, "{:?}", started.elapsed());
    assert_eq!(
        error,
        ConnectorError::Transport("the tool list did not end in 0.3 s and was cancelled".into())
    );
    wait_for_cancels(&server, 1).await;

    // A call lists first. The tool never runs.
    let error = tokio::time::timeout(
        Duration::from_secs(10),
        connector.call_tool("echo", args(json!({"text": "hi"}))),
    )
    .await
    .expect("the call must end at the connector's time limit")
    .unwrap_err();
    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    wait_for_cancels(&server, 2).await;
    assert!(calls(&server).is_empty());

    *server.hang_list.lock().unwrap() = false;
    assert_eq!(connector.list_tools().await.unwrap().len(), 5);
}
