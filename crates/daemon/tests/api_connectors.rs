//! REST API connectors with the real backends, against a local stub of Jira Cloud:
//! tool list, approval, rules, a read and a write that waits for the owner.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use base64::Engine as _;
use bunsin_api::{
    ApiConnectorUpdate, ApiError, EngineApi, Expected, Outcome, PermissionsUpdate, ProfileConfig, TokenChange,
};
use bunsin_core::approval::ApprovalState;
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, LlmClient};
use bunsin_core::settings::{ApiAuthMethod, ApiConfig, ApiService, ConnectorKind};
use bunsin_core::tool::pin;
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Backends, Engine, ProfileSecrets, Runtime};
use bunsin_testkit::FakeLlm;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const TOKEN: &str = "atlassian-api-token-7f3e";
const EMAIL: &str = "kim@acme.example";
const MARKER: &str = "— AI agent (Bunsin) acting for Kim";

/// One request that the stub got.
#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    authorization: Option<String>,
    body: String,
}

/// A stub of the Jira Cloud REST API. It records every request and answers each path it knows.
struct Jira {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

impl Jira {
    async fn start() -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = |State(seen): State<Arc<Mutex<Vec<Seen>>>>, request: Request| async move {
            let (parts, body) = request.into_parts();
            let body = axum::body::to_bytes(body, 1 << 20).await.unwrap();
            let path = parts.uri.path().to_owned();
            seen.lock().unwrap().push(Seen {
                method: parts.method.to_string(),
                path: path.clone(),
                authorization: parts
                    .headers
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned),
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            match path.as_str() {
                "/rest/api/3/issue/PROJ-7" => axum::Json(json!({
                    "key": "PROJ-7", "fields": {"summary": "Deploy moved to Friday"}
                }))
                .into_response(),
                "/rest/api/3/issue/PROJ-7/comment" => {
                    (StatusCode::CREATED, axum::Json(json!({"id": "10001"}))).into_response()
                }
                // A proxy in front of the server answers with a login page after it passed the write on.
                "/rest/api/3/issue/PROJ-8/comment" => (StatusCode::OK, "<html>login</html>").into_response(),
                _ => (
                    StatusCode::NOT_FOUND,
                    axum::Json(json!({"errorMessages": ["no such path"]})),
                )
                    .into_response(),
            }
        };
        let app = axum::Router::new().fallback(record).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
}

/// The real backends for the connectors, with a scripted LLM.
struct Rig {
    real: RealBackends,
    llm: Arc<FakeLlm>,
}

impl Rig {
    fn with_llm(&self, mut runtime: Runtime) -> Runtime {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        runtime.llm = Ok(llm);
        runtime
    }
}

impl Backends for Rig {
    fn build<'a>(&'a self, profile: &'a str, config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move { Ok(self.with_llm(self.real.build(profile, config).await?)) })
    }

    fn build_with_secrets<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
        secrets: &'a ProfileSecrets,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move { Ok(self.with_llm(self.real.build_with_secrets(profile, config, secrets).await?)) })
    }

    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        self.real.validate(profile, config)
    }
}

async fn engine(dir: &std::path::Path, upstream: UpstreamPolicy) -> (Engine, Arc<FakeLlm>) {
    let llm = Arc::new(FakeLlm::new());
    let backends = Arc::new(Rig {
        real: RealBackends::new(Arc::new(|_| None), false, upstream),
        llm: llm.clone(),
    });
    let engine = Engine::open(dir, backends, Arc::new(|| 1_000)).unwrap();
    engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
    (engine, llm)
}

fn local() -> UpstreamPolicy {
    UpstreamPolicy {
        allowed_hosts: vec!["127.0.0.1".into()],
    }
}

fn jira(base_url: &str, token: TokenChange) -> ApiConnectorUpdate {
    ApiConnectorUpdate {
        api: ApiConfig {
            service: ApiService::Jira,
            base_url: base_url.to_owned(),
            auth: ApiAuthMethod::Basic,
            email: Some(EMAIL.into()),
        },
        token,
        expected: Expected::Unchecked,
    }
}

fn call(tool: &str, args: Value) -> String {
    json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": tool, "args_json": args.to_string()})
        .to_string()
}

fn event(text: &str) -> InboundEvent {
    InboundEvent {
        source: "slack".into(),
        external_id: "e1".into(),
        thread: None,
        author: "lee".into(),
        text: text.into(),
        channel: Default::default(),
    }
}

#[tokio::test]
async fn a_rest_api_connector_lists_reads_and_writes_only_after_approval() {
    let stub = Jira::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, llm) = engine(dir.path(), local()).await;
    engine
        .set_api_connector("kim", "jira", jira(&stub.url, TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    let shown = engine.connections("kim").await.unwrap().connectors["jira"].clone();
    assert!(matches!(&shown.kind, ConnectorKind::Api(api) if api.service == ApiService::Jira));
    assert_eq!(shown.token, bunsin_api::TokenSource::Stored);

    // The fixed tools are listed like the tools of an MCP server, and approved by their fingerprints.
    let report = engine.connector_tools("kim", "jira").await.unwrap();
    let names: Vec<&str> = report.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["search_issues", "get_issue", "add_comment"]);
    assert_eq!(report.diff.added.len(), 3);
    engine.approve_tools("kim", "jira", pin(&report.tools)).await.unwrap();
    assert!(engine.connector_tools("kim", "jira").await.unwrap().diff.is_clean());
    // Listing tools sends no request: the tools are in the code.
    assert!(stub.seen().is_empty());

    let rules = [
        ("jira/get_issue".to_owned(), ToolRule::read()),
        (
            "jira/add_comment".to_owned(),
            ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
        ),
    ]
    .into();
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    engine.set_permissions("kim", update).await.unwrap();

    // The agent reads the issue, then proposes a comment. The comment waits for the owner.
    llm.reply(&call("jira/get_issue", json!({"issue": "PROJ-7"})), 10)
        .reply(
            &call("jira/add_comment", json!({"issue": "PROJ-7", "text": "Friday it is."})),
            10,
        );
    let outcome = engine.ingest("kim", event("when is the deploy?")).await.unwrap();
    let Outcome::PendingApproval { inbox_id } = outcome else {
        panic!("expected a pending comment, got {outcome:?}")
    };
    let seen = stub.seen();
    assert_eq!(seen.len(), 1, "{seen:?}");
    assert_eq!(
        (seen[0].method.as_str(), seen[0].path.as_str()),
        ("GET", "/rest/api/3/issue/PROJ-7")
    );
    // Basic auth carries the email and the stored token. Nothing else in the request does.
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{EMAIL}:{TOKEN}"));
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some(format!("Basic {basic}").as_str())
    );
    // The read result reached the LLM as quoted data.
    let requests = llm.requests();
    assert!(requests[1].messages[2].content.contains("Deploy moved to Friday"));

    // Approval runs the write. The harness added the AI marker to the comment.
    let outcome = engine.approve("kim", inbox_id, None).await.unwrap();
    assert_eq!(outcome, Outcome::Executed { inbox_id });
    let seen = stub.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(
        (seen[1].method.as_str(), seen[1].path.as_str()),
        ("POST", "/rest/api/3/issue/PROJ-7/comment")
    );
    let body: Value = serde_json::from_str(&seen[1].body).unwrap();
    let text: Vec<&str> = body["body"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p["content"][0]["text"].as_str())
        .collect();
    assert_eq!(text.first(), Some(&"Friday it is."));
    assert!(seen[1].body.contains(MARKER), "{}", seen[1].body);

    // The token is in no settings text and no audit entry.
    let config = serde_json::to_string(&engine.profile_config("kim").await.unwrap()).unwrap();
    assert!(!config.contains(TOKEN));
    let audit = format!("{:?}", engine.snapshot("kim").await.unwrap().audit);
    assert!(!audit.contains(TOKEN));
}

#[tokio::test]
async fn a_write_answered_with_a_page_that_is_not_json_becomes_unknown() {
    let stub = Jira::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, llm) = engine(dir.path(), local()).await;
    engine
        .set_api_connector("kim", "jira", jira(&stub.url, TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    let report = engine.connector_tools("kim", "jira").await.unwrap();
    engine.approve_tools("kim", "jira", pin(&report.tools)).await.unwrap();
    let rules = [(
        "jira/add_comment".to_owned(),
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
    )]
    .into();
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    engine.set_permissions("kim", update).await.unwrap();

    llm.reply(
        &call("jira/add_comment", json!({"issue": "PROJ-8", "text": "Done."})),
        10,
    );
    let Outcome::PendingApproval { inbox_id } = engine.ingest("kim", event("comment please")).await.unwrap() else {
        panic!("expected a pending comment")
    };
    let outcome = engine.approve("kim", inbox_id, None).await.unwrap();
    assert!(
        matches!(&outcome, Outcome::ExecutionUnknown { error, .. } if error.contains("may be done")),
        "{outcome:?}"
    );
    let snapshot = engine.snapshot("kim").await.unwrap();
    let item = snapshot.inbox.iter().find(|i| i.id == inbox_id).unwrap();
    assert_eq!(item.state, ApprovalState::Unknown);
    assert!(snapshot.audit.iter().any(|a| a.kind == "outcome_unknown"));
    // The write reached the server once and is not sent again.
    assert!(matches!(
        engine.approve("kim", inbox_id, None).await,
        Err(ApiError::NotAllowed(_))
    ));
    assert_eq!(stub.seen().len(), 1);
}

#[tokio::test]
async fn a_tool_without_a_rule_is_not_called() {
    let stub = Jira::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, llm) = engine(dir.path(), local()).await;
    engine
        .set_api_connector("kim", "jira", jira(&stub.url, TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    let report = engine.connector_tools("kim", "jira").await.unwrap();
    engine.approve_tools("kim", "jira", pin(&report.tools)).await.unwrap();
    // No rule for get_issue: the read is not run, and the proposal is denied.
    llm.reply(&call("jira/get_issue", json!({"issue": "PROJ-7"})), 10);
    let outcome = engine.ingest("kim", event("read PROJ-7")).await.unwrap();
    assert!(matches!(outcome, Outcome::Denied { .. }), "{outcome:?}");
    assert!(stub.seen().is_empty());
}

#[tokio::test]
async fn the_token_is_bound_to_the_origin_of_the_base_url() {
    let stub = Jira::start().await;
    let other = Jira::start().await;
    let dir = tempfile::tempdir().unwrap();
    let (engine, _) = engine(dir.path(), local()).await;
    engine
        .set_api_connector("kim", "jira", jira(&stub.url, TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    // Another path of the same origin keeps the token.
    engine
        .set_api_connector("kim", "jira", jira(&format!("{}/x", stub.url), TokenChange::Keep))
        .await
        .unwrap();
    // Another port is another origin: a kept token does not follow.
    let error = engine
        .set_api_connector("kim", "jira", jira(&other.url, TokenChange::Keep))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("enter the token again")),
        "{error:?}"
    );
    assert!(!error.to_string().contains(TOKEN));
}

#[tokio::test]
async fn the_server_rules_and_the_auth_rules_refuse_a_save_before_the_token_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    // The default policy allows only public https addresses.
    let (engine, _) = engine(dir.path(), UpstreamPolicy::default()).await;
    let error = engine
        .set_api_connector(
            "kim",
            "jira",
            jira("http://127.0.0.1:9", TokenChange::Set(TOKEN.into())),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("base URL of connector `jira`")),
        "{error:?}"
    );

    let mut slack = jira("https://slack.com/api", TokenChange::Set(TOKEN.into()));
    slack.api.service = ApiService::Slack;
    let error = engine.set_api_connector("kim", "slack", slack).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("only a bearer token")),
        "{error:?}"
    );

    let mut no_email = jira("https://acme.atlassian.net", TokenChange::Set(TOKEN.into()));
    no_email.api.email = None;
    let error = engine.set_api_connector("kim", "jira", no_email).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("account email")),
        "{error:?}"
    );

    // A REST API connector needs a token.
    let error = engine
        .set_api_connector("kim", "jira", jira("https://acme.atlassian.net", TokenChange::Keep))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("needs a token")),
        "{error:?}"
    );
    assert!(engine.connections("kim").await.unwrap().connectors.is_empty());

    // A name keeps its kind.
    engine
        .set_api_connector(
            "kim",
            "jira",
            jira("https://acme.atlassian.net", TokenChange::Set(TOKEN.into())),
        )
        .await
        .unwrap();
    let address = bunsin_api::ConnectorUpdate {
        url: "https://mcp.example.com/jira".into(),
        token: TokenChange::Keep,
        token_header: None,
        expected: Expected::Unchecked,
    };
    let error = engine.set_connector("kim", "jira", address).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("calls a REST API")),
        "{error:?}"
    );
}
