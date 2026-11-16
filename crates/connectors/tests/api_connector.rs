//! Integration tests for `ApiConnector`. Each test starts its own stub server on 127.0.0.1.
//! The stubs answer like the documented APIs. No test reaches the real services.

use axum::extract::Request;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use bunsin_connectors::api::{SLACK_BASE_URL, tools};
use bunsin_connectors::{ApiAuth, ApiConnector, ApiService};
use bunsin_core::ports::{Connector, ConnectorError};
use bunsin_core::tool::pin;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TOKEN: &str = "test-token-5f2c9a";
const EMAIL: &str = "owner@example.com";

/// One request as the stub received it.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: Option<String>,
    authorization: Option<String>,
    content_type: Option<String>,
    body: Value,
}

type Answer = Arc<dyn Fn(&Seen) -> (StatusCode, String) + Send + Sync>;

/// Serves a stub that records every request and answers with `answer`. Returns the base URL.
async fn stub(answer: Answer) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let router = axum::Router::new().fallback(move |request: Request| {
        let record = record.clone();
        let answer = answer.clone();
        async move {
            let (parts, body) = request.into_parts();
            let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
            let header = |name: &str| parts.headers.get(name).map(|v| v.to_str().unwrap().to_owned());
            let request = Seen {
                method: parts.method.to_string(),
                path: parts.uri.path().to_owned(),
                query: parts.uri.query().map(str::to_owned),
                authorization: header("authorization"),
                content_type: header("content-type"),
                body: serde_json::from_slice(&bytes).unwrap_or(Value::Null),
            };
            let (status, body) = answer(&request);
            record.lock().unwrap().push(request);
            (status, body).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    (format!("http://{addr}"), seen)
}

fn ok(body: Value) -> Answer {
    Arc::new(move |_| (StatusCode::OK, body.to_string()))
}

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        other => panic!("arguments must be a JSON object, got {other}"),
    }
}

fn bearer() -> ApiAuth {
    ApiAuth::Bearer(TOKEN.into())
}

fn basic() -> ApiAuth {
    ApiAuth::Basic {
        email: EMAIL.into(),
        token: TOKEN.into(),
    }
}

/// `query` decoded into pairs.
fn query_pairs(seen: &Seen) -> Vec<(String, String)> {
    let url = reqwest::Url::parse(&format!("http://x/?{}", seen.query.clone().unwrap_or_default())).unwrap();
    url.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn pair(key: &str, value: &str) -> (String, String) {
    (key.to_owned(), value.to_owned())
}

#[tokio::test]
async fn lists_the_fixed_tools_and_the_same_list_each_time() {
    let connector = ApiConnector::new("jira", ApiService::Jira, "https://example.atlassian.net", basic()).unwrap();
    let first = connector.list_tools().await.unwrap();
    let names: Vec<&str> = first.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search_issues", "get_issue", "add_comment"]);
    // A stable list keeps the pins valid between calls.
    assert_eq!(pin(&first), pin(&connector.list_tools().await.unwrap()));
    assert_eq!(connector.name(), "jira");

    let slack: Vec<String> = tools(ApiService::Slack).into_iter().map(|t| t.name).collect();
    assert_eq!(slack, ["search_messages", "get_thread", "post_message"]);
    let confluence: Vec<String> = tools(ApiService::Confluence).into_iter().map(|t| t.name).collect();
    assert_eq!(confluence, ["search_pages", "get_page", "create_page"]);
}

#[tokio::test]
async fn slack_post_message_sends_bearer_and_json_body() {
    let answer = json!({"ok": true, "channel": "C1", "ts": "1503435956.000247"});
    let (base, seen) = stub(ok(answer.clone())).await;
    let connector = ApiConnector::new("slack", ApiService::Slack, &format!("{base}/api"), bearer()).unwrap();

    let result = connector
        .call_tool(
            "post_message",
            args(json!({"channel": "C1", "text": "배포 \"완료\"", "thread_ts": "1.2"})),
        )
        .await
        .unwrap();

    assert_eq!(result, answer);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/api/chat.postMessage");
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(seen[0].content_type.as_deref(), Some("application/json; charset=utf-8"));
    assert_eq!(
        seen[0].body,
        json!({"channel": "C1", "text": "배포 \"완료\"", "thread_ts": "1.2"})
    );
}

#[tokio::test]
async fn slack_reads_put_the_arguments_in_the_query() {
    let (base, seen) = stub(ok(json!({"ok": true, "messages": {"matches": []}}))).await;
    let connector = ApiConnector::new("slack", ApiService::Slack, &format!("{base}/api/"), bearer()).unwrap();

    connector
        .call_tool(
            "search_messages",
            args(json!({"query": "to:me 배포 & more", "count": 20})),
        )
        .await
        .unwrap();
    connector
        .call_tool("get_thread", args(json!({"channel": "C1", "ts": "1700000000.000100"})))
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    // The trailing slash of the base URL does not double.
    assert_eq!(seen[0].path, "/api/search.messages");
    assert_eq!(seen[0].method, "GET");
    assert_eq!(
        query_pairs(&seen[0]),
        [pair("query", "to:me 배포 & more"), pair("count", "20")]
    );
    assert_eq!(seen[1].path, "/api/conversations.replies");
    assert_eq!(
        query_pairs(&seen[1]),
        [pair("channel", "C1"), pair("ts", "1700000000.000100")]
    );
}

#[tokio::test]
async fn slack_ok_false_becomes_failed_with_the_error_code() {
    let (base, _) = stub(ok(
        json!({"ok": false, "error": "missing_scope", "needed": "search:read"}),
    ))
    .await;
    let connector = ApiConnector::new("slack", ApiService::Slack, &base, bearer()).unwrap();

    let error = connector
        .call_tool("search_messages", args(json!({"query": "x"})))
        .await
        .unwrap_err();

    assert_eq!(
        error,
        ConnectorError::Failed("missing_scope; needed: search:read".into())
    );
}

#[tokio::test]
async fn jira_uses_basic_auth_and_sends_the_comment_as_adf() {
    let (base, seen) = stub(Arc::new(|_| (StatusCode::CREATED, json!({"id": "10000"}).to_string()))).await;
    let connector = ApiConnector::new("jira", ApiService::Jira, &base, basic()).unwrap();

    let result = connector
        .call_tool(
            "add_comment",
            args(json!({"issue": "PROJ-12", "text": "line one\n\nline three"})),
        )
        .await
        .unwrap();

    assert_eq!(result, json!({"id": "10000"}));
    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].method, "POST");
    assert_eq!(seen[0].path, "/rest/api/3/issue/PROJ-12/comment");
    // base64("owner@example.com:test-token-5f2c9a"), computed outside this code.
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Basic b3duZXJAZXhhbXBsZS5jb206dGVzdC10b2tlbi01ZjJjOWE=")
    );
    assert_eq!(
        seen[0].body,
        json!({"body": {"type": "doc", "version": 1, "content": [
            {"type": "paragraph", "content": [{"type": "text", "text": "line one"}]},
            {"type": "paragraph"},
            {"type": "paragraph", "content": [{"type": "text", "text": "line three"}]}
        ]}})
    );
}

#[tokio::test]
async fn jira_search_and_get_use_the_documented_paths() {
    let (base, seen) = stub(ok(json!({"issues": []}))).await;
    let connector = ApiConnector::new("jira", ApiService::Jira, &base, bearer()).unwrap();

    connector
        .call_tool(
            "search_issues",
            args(json!({"jql": "assignee = currentUser()", "max_results": 5})),
        )
        .await
        .unwrap();
    connector
        .call_tool("get_issue", args(json!({"issue": "PROJ-12"})))
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("POST", "/rest/api/3/search/jql")
    );
    assert_eq!(seen[0].body["jql"], "assignee = currentUser()");
    assert_eq!(seen[0].body["maxResults"], 5);
    assert!(seen[0].body["fields"].as_array().unwrap().contains(&json!("summary")));
    assert_eq!(
        (seen[1].method.as_str(), seen[1].path.as_str()),
        ("GET", "/rest/api/3/issue/PROJ-12")
    );
}

#[tokio::test]
async fn confluence_tools_use_the_wiki_paths_and_escape_the_page_text() {
    let (base, seen) = stub(ok(json!({"id": "123"}))).await;
    let connector = ApiConnector::new("confluence", ApiService::Confluence, &base, basic()).unwrap();

    connector
        .call_tool(
            "search_pages",
            args(json!({"cql": "type = page and text ~ \"deploy\"", "limit": 3})),
        )
        .await
        .unwrap();
    connector
        .call_tool("get_page", args(json!({"page_id": "98765"})))
        .await
        .unwrap();
    connector
        .call_tool(
            "create_page",
            args(json!({"space_id": "42", "title": "Notes", "text": "<b>x</b>", "parent_id": "7"})),
        )
        .await
        .unwrap();

    let seen = seen.lock().unwrap();
    assert_eq!(seen[0].path, "/wiki/rest/api/search");
    assert_eq!(
        query_pairs(&seen[0]),
        [pair("cql", "type = page and text ~ \"deploy\""), pair("limit", "3")]
    );
    assert_eq!(seen[1].path, "/wiki/api/v2/pages/98765");
    assert_eq!(query_pairs(&seen[1]), [pair("body-format", "storage")]);
    assert_eq!(
        (seen[2].method.as_str(), seen[2].path.as_str()),
        ("POST", "/wiki/api/v2/pages")
    );
    assert_eq!(
        seen[2].body,
        json!({
            "spaceId": "42",
            "status": "current",
            "title": "Notes",
            "parentId": "7",
            "body": {"representation": "storage", "value": "<p>&lt;b&gt;x&lt;/b&gt;</p>"}
        })
    );
}

#[tokio::test]
async fn http_errors_become_failed_with_the_api_messages() {
    let (base, _) = stub(Arc::new(|seen: &Seen| {
        if seen.path.ends_with("/pages/1") {
            let body = json!({"errors": [{"status": 404, "title": "Not Found", "detail": "Page 1 is gone"}]});
            (StatusCode::NOT_FOUND, body.to_string())
        } else if seen.path.contains("/issue/") {
            let body = json!({"errorMessages": ["Issue does not exist"], "errors": {}});
            (StatusCode::NOT_FOUND, body.to_string())
        } else {
            (StatusCode::BAD_GATEWAY, "<html>bad gateway</html>".into())
        }
    }))
    .await;
    let jira = ApiConnector::new("jira", ApiService::Jira, &base, bearer()).unwrap();
    let confluence = ApiConnector::new("confluence", ApiService::Confluence, &base, bearer()).unwrap();

    let error = jira
        .call_tool("get_issue", args(json!({"issue": "NO-1"})))
        .await
        .unwrap_err();
    assert_eq!(error, ConnectorError::Failed("HTTP 404: Issue does not exist".into()));

    let error = confluence
        .call_tool("get_page", args(json!({"page_id": "1"})))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ConnectorError::Failed("HTTP 404: Not Found; Page 1 is gone".into())
    );

    let error = jira
        .call_tool("search_issues", args(json!({"jql": "x"})))
        .await
        .unwrap_err();
    assert_eq!(error, ConnectorError::Failed("HTTP 502: no error detail".into()));
}

#[tokio::test]
async fn a_success_that_is_not_json_is_a_transport_error() {
    let (base, _) = stub(Arc::new(|_| (StatusCode::OK, "<html>login</html>".into()))).await;
    let connector = ApiConnector::new("jira", ApiService::Jira, &base, bearer()).unwrap();
    let error = connector
        .call_tool("get_issue", args(json!({"issue": "PROJ-1"})))
        .await
        .unwrap_err();
    assert_eq!(error, ConnectorError::Transport("the response is not JSON".into()));
}

#[tokio::test]
async fn an_echoed_token_is_redacted_from_errors_and_debug() {
    // The stub puts the whole Authorization header and its parts into the error body.
    let (base, _) = stub(Arc::new(|seen: &Seen| {
        let header = seen.authorization.clone().unwrap_or_default();
        let encoded = header.split(' ').nth(1).unwrap_or_default().to_owned();
        let body = json!({"errorMessages": [format!("bad header {header}"), format!("bad pair {encoded}")]});
        (StatusCode::UNAUTHORIZED, body.to_string())
    }))
    .await;

    for (service, auth, tool, tool_args) in [
        (ApiService::Jira, basic(), "get_issue", json!({"issue": "PROJ-1"})),
        (ApiService::Confluence, bearer(), "get_page", json!({"page_id": "1"})),
    ] {
        let connector = ApiConnector::new("svc", service, &base, auth).unwrap();
        let error = connector.call_tool(tool, args(tool_args)).await.unwrap_err();
        let text = error.to_string();
        assert!(text.contains("HTTP 401"), "{text}");
        assert!(text.contains("[redacted]"), "{text}");
        assert!(!text.contains(TOKEN), "the error leaks the token: {text}");
        assert!(
            !text.contains("b3duZXJAZXhhbXBsZS5jb20"),
            "the error leaks the encoded pair: {text}"
        );
        let debug = format!("{connector:?}");
        assert!(!debug.contains(TOKEN), "Debug leaks the token: {debug}");
    }

    // A Slack error code that echoes the token is redacted too.
    let (base, _) = stub(ok(json!({"ok": false, "error": format!("invalid_auth {TOKEN}")}))).await;
    let slack = ApiConnector::new("slack", ApiService::Slack, &base, bearer()).unwrap();
    let error = slack
        .call_tool("get_thread", args(json!({"channel": "C1", "ts": "1.2"})))
        .await
        .unwrap_err();
    assert_eq!(error, ConnectorError::Failed("invalid_auth [redacted]".into()));
}

#[tokio::test]
async fn a_redirect_is_not_followed_and_the_token_does_not_reach_the_other_host() {
    // The target works and records every request, so a followed redirect would show there.
    let (target, target_seen) = stub(ok(json!({"ok": true}))).await;
    let location = format!("{target}/api/chat.postMessage");
    // The configured server answers every request with a redirect to the target.
    let redirector = axum::Router::new().fallback(move || {
        let location = location.clone();
        async move { (StatusCode::TEMPORARY_REDIRECT, [("location", location)]).into_response() }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/api", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, redirector).await });
    let connector = ApiConnector::new("slack", ApiService::Slack, &base, bearer()).unwrap();

    let error = tokio::time::timeout(
        Duration::from_secs(10),
        connector.call_tool("post_message", args(json!({"channel": "C1", "text": "hi"}))),
    )
    .await
    .expect("the call returns")
    .unwrap_err();

    assert!(
        matches!(&error, ConnectorError::Transport(text) if text.contains("HTTP 307")),
        "{error:?}"
    );
    assert!(target_seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bad_arguments_and_unknown_tools_send_no_request() {
    let (base, seen) = stub(ok(json!({"ok": true}))).await;
    let jira = ApiConnector::new("jira", ApiService::Jira, &base, bearer()).unwrap();
    let slack = ApiConnector::new("slack", ApiService::Slack, &base, bearer()).unwrap();

    let error = jira.call_tool("delete_issue", Map::new()).await.unwrap_err();
    assert_eq!(error, ConnectorError::UnknownTool("delete_issue".into()));

    let refused = [
        (
            &jira,
            "add_comment",
            json!({"issue": "PROJ-1"}),
            "needs the argument `text`",
        ),
        (
            &jira,
            "add_comment",
            json!({"issue": "PROJ-1", "text": "x", "visibility": "all"}),
            "takes no argument `visibility`",
        ),
        (
            &jira,
            "get_issue",
            json!({"issue": "../../admin"}),
            "`issue` may hold only",
        ),
        (
            &jira,
            "get_issue",
            json!({"issue": "PROJ-1?x=1"}),
            "`issue` may hold only",
        ),
        (&jira, "get_issue", json!({"issue": 12}), "`issue` must be a text"),
        (
            &jira,
            "search_issues",
            json!({"jql": "x", "max_results": 0}),
            "from 1 to 100",
        ),
        (
            &jira,
            "search_issues",
            json!({"jql": "x", "max_results": 101}),
            "from 1 to 100",
        ),
        (
            &slack,
            "post_message",
            json!({"channel": "C1", "text": "  "}),
            "`text` must be a text",
        ),
        (
            &slack,
            "post_message",
            json!({"channel": "C1", "text": "x", "thread_ts": 1.5}),
            "`thread_ts` must be a text",
        ),
    ];
    for (connector, tool, tool_args, expected) in refused {
        let error = connector.call_tool(tool, args(tool_args.clone())).await.unwrap_err();
        assert!(
            matches!(&error, ConnectorError::Failed(text) if text.contains(expected)),
            "{tool} {tool_args}: {error:?}"
        );
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_dead_address_is_a_transport_error_without_the_token() {
    // Bind and drop a listener, so the port is very likely closed.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let connector = ApiConnector::new("jira", ApiService::Jira, &base, basic()).unwrap();

    let error = connector
        .call_tool("get_issue", args(json!({"issue": "PROJ-1"})))
        .await
        .unwrap_err();

    assert!(matches!(error, ConnectorError::Transport(_)), "{error:?}");
    assert!(!error.to_string().contains(TOKEN));
}

#[test]
fn new_rejects_a_bad_base_url_and_basic_auth_for_slack() {
    for url in ["ftp://example.com", "not a url", "https://example.com/?a=1"] {
        let error = ApiConnector::new("jira", ApiService::Jira, url, bearer()).unwrap_err();
        assert!(matches!(error, ConnectorError::Transport(_)), "{url}: {error:?}");
    }
    let error = ApiConnector::new("slack", ApiService::Slack, SLACK_BASE_URL, basic()).unwrap_err();
    assert_eq!(
        error,
        ConnectorError::Transport("Slack accepts only a bearer token".into())
    );
    let error = ApiConnector::new(
        "slack",
        ApiService::Slack,
        SLACK_BASE_URL,
        ApiAuth::Bearer("a\nb".into()),
    )
    .unwrap_err();
    assert!(!error.to_string().contains("a\nb"));
}
