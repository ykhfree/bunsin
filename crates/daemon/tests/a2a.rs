//! The A2A v1.0 endpoint of each profile on the outer listeners, over real TCP.
//! The first tests check the wire types of `a2a-lf` against the examples and the JSON schema of A2A v1.0.0.

use bunsin_api::{EngineApi, ProfileConfig};
use bunsin_connectors::{McpConfig, McpConnector, TokenAuth};
use bunsin_core::inbound::{ArdListing, InboundScope};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_daemon::server::{OuterConfig, Reach, Running, serve_outer};
use bunsin_engine::{Backends, Engine, Runtime};
use bunsin_testkit::FakeLlm;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

// --- the wire types against the specification ---

/// Parses `text` into `T` and writes it again. The result must equal the original JSON.
fn round_trip<T: Serialize + DeserializeOwned>(section: &str, text: &str) {
    let original: Value = serde_json::from_str(text).unwrap_or_else(|e| panic!("{section}: not JSON: {e}"));
    let typed: T = serde_json::from_value(original.clone()).unwrap_or_else(|e| panic!("{section}: {e}"));
    assert_eq!(serde_json::to_value(&typed).unwrap(), original, "{section}");
}

#[test]
fn the_spec_examples_round_trip_through_the_wire_types() {
    use a2a::{
        AgentCard, CancelTaskRequest, GetTaskRequest, JsonRpcRequest, JsonRpcResponse, ListTasksRequest,
        ListTasksResponse, SendMessageRequest, SendMessageResponse,
    };
    // §6.1
    round_trip::<SendMessageRequest>(
        "6.1 request",
        r#"{"message": {"role": "ROLE_USER", "parts": [{"text": "What is the weather today?"}], "messageId": "msg-uuid"}}"#,
    );
    round_trip::<SendMessageResponse>(
        "6.1 response",
        r#"{"task": {"id": "task-uuid", "contextId": "context-uuid", "status": {"state": "TASK_STATE_COMPLETED"},
            "artifacts": [{"artifactId": "artifact-uuid", "name": "Weather Report",
                "parts": [{"text": "Today will be sunny with a high of 75°F"}]}]}}"#,
    );
    // §6.3 and §6.4: the follow-up names a task; a version error still has a normal request.
    round_trip::<SendMessageRequest>(
        "6.3 request",
        r#"{"message": {"role": "ROLE_USER", "parts": [{"text": "Book me a flight"}], "messageId": "msg-1"}}"#,
    );
    round_trip::<SendMessageRequest>(
        "6.3 follow-up",
        r#"{"message": {"taskId": "task-uuid", "role": "ROLE_USER", "parts": [{"text": "From San Francisco to New York"}],
            "messageId": "msg-2"}}"#,
    );
    round_trip::<SendMessageRequest>(
        "6.4 request",
        r#"{"message": {"role": "ROLE_USER", "parts": [{"text": "Hello"}], "messageId": "msg-uuid"}}"#,
    );
    // §6.5
    round_trip::<ListTasksResponse>(
        "6.5 by context",
        r#"{"tasks": [{"id": "3f36680c-7f37-4a5f-945e-d78981fafd36", "contextId": "c295ea44-7543-4f78-b524-7a38915ad6e4",
            "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "2024-03-15T10:15:00Z"}}],
            "totalSize": 5, "pageSize": 10, "nextPageToken": ""}"#,
    );
    round_trip::<ListTasksResponse>(
        "6.5 working",
        r#"{"tasks": [{"id": "789abc-def0-1234-5678-9abcdef01234", "contextId": "another-context-id",
            "status": {"state": "TASK_STATE_WORKING", "message": {"role": "ROLE_AGENT",
                "parts": [{"text": "Processing your document analysis..."}], "messageId": "msg-status-update"},
            "timestamp": "2024-03-15T10:20:00Z"}}], "totalSize": 1, "pageSize": 20, "nextPageToken": ""}"#,
    );
    // §6.6
    round_trip::<SendMessageRequest>(
        "6.6 request",
        r#"{"message": {"role": "ROLE_USER", "parts": [{"text": "Generate the Q1 sales report. This usually takes a while. Notify me when it's ready."}],
            "messageId": "6dbc13b5-bd57-4c2b-b503-24e381b6c8d6"},
            "configuration": {"taskPushNotificationConfig": {"url": "https://client.example.com/webhook/a2a-notifications",
                "authentication": {"scheme": "Bearer", "credentials": "secure-client-token-for-task-aaa"}}}}"#,
    );
    round_trip::<SendMessageResponse>(
        "6.6 response",
        r#"{"task": {"id": "43667960-d455-4453-b0cf-1bae4955270d", "contextId": "c295ea44-7543-4f78-b524-7a38915ad6e4",
            "status": {"state": "TASK_STATE_SUBMITTED", "timestamp": "2024-03-15T11:00:00Z"}}}"#,
    );
    // §6.7: the request of the example is not valid JSON (a missing and a trailing comma), so only the response.
    round_trip::<SendMessageResponse>(
        "6.7 response",
        r#"{"task": {"id": "43667960-d455-4453-b0cf-1bae4955270d", "contextId": "c295ea44-7543-4f78-b524-7a38915ad6e4",
            "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "2024-03-15T12:05:00Z"},
            "artifacts": [{"artifactId": "9b6934dd-37e3-4eb1-8766-962efaab63a1", "name": "processed_image_with_faces.png",
                "parts": [{"url": "https://storage.example.com/processed/task-bbb/output.png?token=xyz",
                    "filename": "output.png", "mediaType": "image/png"}]}]}}"#,
    );
    // §6.8
    round_trip::<SendMessageRequest>(
        "6.8 request",
        r#"{"message": {"role": "ROLE_USER", "parts": [{"text": "Show me a list of my open IT tickets",
            "metadata": {"mediaType": "application/json", "schema": {"type": "array", "items": {"type": "object",
                "properties": {"ticketNumber": {"type": "string"}, "description": {"type": "string"}}}}}}],
            "messageId": "85b26db5-ffbb-4278-a5da-a7b09dea1b47"}}"#,
    );
    round_trip::<SendMessageResponse>(
        "6.8 response",
        r#"{"task": {"id": "d8c6243f-5f7a-4f6f-821d-957ce51e856c", "contextId": "c295ea44-7543-4f78-b524-7a38915ad6e4",
            "status": {"state": "TASK_STATE_COMPLETED", "timestamp": "2025-04-17T17:47:09.680794Z"},
            "artifacts": [{"artifactId": "c5e0382f-b57f-4da7-87d8-b85171fad17c",
                "parts": [{"text": "[{\"ticketNumber\":\"REQ12312\",\"description\":\"request for VPN access\"},{\"ticketNumber\":\"REQ23422\",\"description\":\"Add to DL - team-gcp-onboarding\"}]"}]}]}}"#,
    );
    // §8.5: the endpoint writes its own card with the same type.
    round_trip::<AgentCard>("8.5 card", include_str!("fixtures/a2a-spec-8.5-agent-card.json"));
    // §9.4 and §9.5: the envelope and the parameters the endpoint reads.
    let get = r#"{"jsonrpc": "2.0", "id": 2, "method": "GetTask", "params": {"id": "task-uuid", "historyLength": 10}}"#;
    round_trip::<JsonRpcRequest>("9.4.3", get);
    round_trip::<GetTaskRequest>("9.4.3 params", r#"{"id": "task-uuid", "historyLength": 10}"#);
    round_trip::<ListTasksRequest>(
        "9.4.4 params",
        r#"{"contextId": "context-uuid", "status": "TASK_STATE_WORKING", "pageSize": 50, "pageToken": "cursor-token"}"#,
    );
    round_trip::<CancelTaskRequest>("9.4.5 params", r#"{"id": "task-uuid"}"#);
    round_trip::<JsonRpcRequest>(
        "9.4.8",
        r#"{"jsonrpc": "2.0", "id": 6, "method": "GetExtendedAgentCard"}"#,
    );
    round_trip::<JsonRpcResponse>(
        "9.5 invalid params",
        r#"{"jsonrpc": "2.0", "id": 1, "error": {"code": -32602, "message": "Invalid parameters", "data": [
            {"@type": "type.googleapis.com/google.rpc.BadRequest",
             "fieldViolations": [{"field": "message.parts", "description": "At least one part is required"}]}]}}"#,
    );
    round_trip::<JsonRpcResponse>(
        "9.5 task not found",
        r#"{"jsonrpc": "2.0", "id": 2, "error": {"code": -32001, "message": "Task not found", "data": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "TASK_NOT_FOUND", "domain": "a2a-protocol.org",
             "metadata": {"taskId": "nonexistent-task-id", "timestamp": "2025-11-09T10:30:00.000Z"}}]}}"#,
    );
}

/// The one example that the crate type refuses: `Task.contextId` is optional in §4.1.1, but `a2a::Task`
/// requires it. The endpoint only writes tasks, always with a context ID, so it keeps the crate type.
#[test]
fn the_crate_task_needs_a_context_id_that_the_spec_makes_optional() {
    // §6.3 response. Its status message also lacks `messageId`, which §4.1.4 requires: a fault of the example.
    let mut example = json!({"task": {"id": "task-uuid", "status": {"state": "TASK_STATE_INPUT_REQUIRED",
        "message": {"role": "ROLE_AGENT", "parts": [{"text": "I need more details. Where would you like to fly from and to?"}]}}}});
    let parse = |value: &Value| serde_json::from_value::<a2a::SendMessageResponse>(value.clone());
    assert!(parse(&example).is_err());
    example["task"]["status"]["message"]["messageId"] = json!("msg-3");
    let error = parse(&example).unwrap_err().to_string();
    assert!(error.contains("contextId"), "{error}");
    example["task"]["contextId"] = json!("ctx-1");
    let typed = parse(&example).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), example);
}

const SCHEMA: &str = include_str!("fixtures/a2a-v1.0.0.schema.json");
const SCHEMA_BASE: &str = "https://a2a-protocol.org/v1.0.0/spec/";

/// The file name that the schema's `$ref` values use for the definition titled `title`.
fn schema_file(title: &str) -> String {
    let compact: String = title.split_whitespace().collect();
    match compact.as_str() {
        "Struct" | "Timestamp" | "Value" => format!("google.protobuf.{compact}.jsonschema.json"),
        _ => format!("lf.a2a.v1.{compact}.jsonschema.json"),
    }
}

/// Serves the definitions of the bundle by their file names, so `$ref` resolves without a network.
struct Bundle(serde_json::Map<String, Value>);

impl jsonschema::Retrieve for Bundle {
    fn retrieve(&self, uri: &jsonschema::Uri<String>) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let name = uri.as_str().rsplit('/').next().unwrap_or_default();
        self.0
            .get(name)
            .cloned()
            .ok_or_else(|| format!("no definition {name}").into())
    }
}

/// The schema errors of `instance` against the definition titled `definition`, for example `Agent Card`.
fn schema_errors(definition: &str, instance: &Value) -> Vec<String> {
    let bundle: Value = serde_json::from_str(SCHEMA).unwrap();
    let files = bundle["definitions"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(title, schema)| (schema_file(title), schema.clone()))
        .collect();
    let validator = jsonschema::options()
        .with_base_uri(SCHEMA_BASE)
        .with_retriever(Bundle(files))
        .should_validate_formats(true)
        .build(&json!({"$ref": schema_file(definition)}))
        .unwrap();
    validator.iter_errors(instance).map(|error| error.to_string()).collect()
}

#[test]
fn the_schema_check_finds_a_wrong_field_and_a_wrong_state() {
    let task = json!({"id": "t", "contextId": "c", "status": {"state": "TASK_STATE_WORKING",
        "timestamp": "2026-11-20T10:00:00Z"}});
    assert_eq!(schema_errors("Task", &task), Vec::<String>::new());
    let mut wrong = task.clone();
    wrong["status"]["state"] = json!("TASK_STATE_RUNNING");
    assert!(!schema_errors("Task", &wrong).is_empty());
    let mut wrong = task.clone();
    wrong["context"] = json!("c");
    assert!(!schema_errors("Task", &wrong).is_empty());
    let mut wrong = task;
    wrong["status"]["timestamp"] = json!("yesterday");
    assert!(!schema_errors("Task", &wrong).is_empty());
    let card = json!({"name": "x", "securityRequirements": [{"schemes": {"callerToken": ["a"]}}]});
    assert!(!schema_errors("Agent Card", &card).is_empty());
}

// --- the endpoint over TCP ---

/// A scripted LLM, and `acme.com` as the one publisher domain that the operator allows.
struct Fakes {
    llm: Arc<FakeLlm>,
}

impl Backends for Fakes {
    fn build<'a>(&'a self, _profile: &'a str, _config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        Box::pin(async move {
            Ok(Runtime {
                llm: Ok(llm),
                connectors: std::collections::BTreeMap::new(),
            })
        })
    }

    fn ard_publishers(&self) -> Vec<String> {
        vec!["acme.com".into()]
    }
}

/// Kim serves A2A and MCP on the internal listener. Lee serves MCP only.
struct Rig {
    _dir: tempfile::TempDir,
    llm: Arc<FakeLlm>,
    clock: Arc<AtomicI64>,
    engine: Arc<Engine>,
    internal: Running,
    public: Running,
}

const START: i64 = 1_000_000;

impl Rig {
    fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }

    fn token(&self, profile: &str) -> String {
        self.engine.app_store().add_caller_token(profile, "bot", 1).unwrap().1
    }

    /// A client of kim's endpoint on the internal listener with a new caller token.
    fn kim_client(&self) -> A2a {
        A2a::new(&self.internal, "kim", &self.token("kim"))
    }

    async fn change(&self, profile: &str, change: impl FnOnce(&mut ProfileConfig)) {
        let mut config = self.engine.profile_config(profile).await.unwrap();
        change(&mut config);
        self.engine.set_profile_config(profile, config).await.unwrap();
    }

    /// The newest inbox item of kim.
    async fn newest_item(&self) -> i64 {
        self.engine.snapshot("kim").await.unwrap().inbox[0].id
    }
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let clock = Arc::new(AtomicI64::new(START));
    let time = clock.clone();
    let backends = Arc::new(Fakes { llm: llm.clone() });
    let now = move || time.load(Ordering::SeqCst);
    let engine = Arc::new(Engine::open(dir.path(), backends, Arc::new(now)).unwrap());
    for (profile, owner) in [("kim", "Kim"), ("lee", "Lee")] {
        engine.create_profile(profile, ProfileConfig::new(owner)).await.unwrap();
    }
    let wake = Arc::new(tokio::sync::Notify::new());
    let listener = |reach, url: &str| OuterConfig {
        reach,
        listen: "127.0.0.1:0".parse().unwrap(),
        url: url.parse().unwrap(),
        tls: None,
    };
    let internal = serve_outer(
        engine.clone(),
        listener(Reach::Internal, "http://127.0.0.1"),
        wake.clone(),
    )
    .await
    .unwrap();
    let public = serve_outer(engine.clone(), listener(Reach::Public, "http://localhost"), wake)
        .await
        .unwrap();
    let rig = Rig {
        _dir: dir,
        llm,
        clock,
        engine,
        internal,
        public,
    };
    rig.change("kim", |config| {
        config.inbound.enabled = true;
        config.inbound.a2a = true;
        config.inbound.title = Some("Kim's agent".into());
        config.inbound.description = "Questions about the deploy schedule.".into();
    })
    .await;
    rig.change("lee", |config| {
        config.inbound.enabled = true;
        config.inbound.description = "Questions for Lee.".into();
    })
    .await;
    rig
}

/// A JSON-RPC client of one profile's A2A endpoint.
struct A2a {
    url: String,
    token: String,
    /// The `Host` header to send instead of the address's own.
    host: Option<String>,
}

impl A2a {
    fn new(listener: &Running, profile: &str, token: &str) -> Self {
        A2a {
            url: format!("{}/agents/{profile}/a2a", listener.url()),
            token: token.to_owned(),
            host: None,
        }
    }

    /// Sends `body` with the header `A2A-Version: version` if given. Returns the HTTP response.
    async fn post(&self, version: Option<&str>, body: String) -> reqwest::Response {
        let mut request = reqwest::Client::new()
            .post(&self.url)
            .bearer_auth(&self.token)
            .header("content-type", "application/json")
            .body(body);
        if let Some(version) = version {
            request = request.header("A2A-Version", version);
        }
        if let Some(host) = &self.host {
            request = request.header("host", host);
        }
        request.send().await.unwrap()
    }

    /// One call with version 1.0. Returns the JSON-RPC response, which always comes with HTTP 200.
    async fn call(&self, method: &str, params: Value) -> Value {
        let body = json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params});
        let response = self.post(Some("1.0"), body.to_string()).await;
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let reply: Value = response.json().await.unwrap();
        assert_eq!((&reply["jsonrpc"], &reply["id"]), (&json!("2.0"), &json!(7)), "{reply}");
        reply
    }

    async fn result(&self, method: &str, params: Value) -> Value {
        let reply = self.call(method, params).await;
        assert!(reply.get("error").is_none(), "{reply}");
        reply["result"].clone()
    }

    /// The error of a call that must fail: its code and its message.
    async fn error(&self, method: &str, params: Value) -> (i64, String) {
        let reply = self.call(method, params).await;
        assert!(reply.get("result").is_none(), "{reply}");
        rpc_error(&reply)
    }

    /// Sends a question and returns its task.
    async fn ask(&self, text: &str, message_id: &str) -> Value {
        self.result("SendMessage", send(text, message_id)).await["task"].clone()
    }

    async fn task(&self, id: &str) -> Value {
        self.result("GetTask", json!({"id": id})).await
    }
}

/// The code and the message of a JSON-RPC error, after a check of its `ErrorInfo` detail (§9.5).
fn rpc_error(reply: &Value) -> (i64, String) {
    let error = &reply["error"];
    let info = &error["data"][0];
    assert_eq!(info["@type"], "type.googleapis.com/google.rpc.ErrorInfo", "{reply}");
    assert!(
        info["reason"].as_str().is_some_and(|reason| !reason.is_empty()),
        "{reply}"
    );
    (
        error["code"].as_i64().unwrap(),
        error["message"].as_str().unwrap().to_owned(),
    )
}

/// The parameters of a SendMessage that returns at once.
fn send(text: &str, message_id: &str) -> Value {
    json!({"message": {"role": "ROLE_USER", "parts": [{"text": text}], "messageId": message_id},
           "configuration": {"returnImmediately": true}})
}

fn answer(text: &str) -> String {
    json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "bunsin.inbound/reply",
        "args_json": json!({"text": text}).to_string()})
    .to_string()
}

fn ignore() -> String {
    json!({"kind": "ignore", "rationale": "a greeting", "summary": null, "tool": null, "args_json": null}).to_string()
}

/// The time of the rig's clock after `secs` seconds, as the task status writes it.
fn at(secs: i64) -> String {
    bunsin_core::time::utc_text(START + secs)
}

/// Checks a task against the schema and the required fields of §4.1.1, which the schema does not list.
fn check_task(task: &Value) {
    assert_eq!(schema_errors("Task", task), Vec::<String>::new(), "{task}");
    assert!(task["id"].as_str().is_some_and(|id| !id.is_empty()), "{task}");
    assert!(task["contextId"].as_str().is_some_and(|id| !id.is_empty()), "{task}");
    assert!(task["status"]["state"].as_str().is_some(), "{task}");
    // No history: the stored question is filtered text, and it would show the owner's filter.
    assert!(task.get("history").is_none(), "{task}");
    if let Some(message) = task["status"].get("message") {
        assert_eq!(message["role"], "ROLE_AGENT");
        assert!(message["messageId"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(
            (&message["taskId"], &message["contextId"]),
            (&task["id"], &task["contextId"])
        );
    }
}

#[tokio::test]
async fn send_message_without_return_immediately_gets_32004() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let message = json!({"role": "ROLE_USER", "parts": [{"text": "When do we deploy?"}], "messageId": "m-1"});
    for params in [
        json!({"message": message}),
        json!({"message": message, "configuration": {}}),
        json!({"message": message, "configuration": {"returnImmediately": false}}),
    ] {
        let (code, text) = kim.error("SendMessage", params).await;
        assert_eq!(code, -32004);
        assert!(text.contains("returnImmediately: true"), "{text}");
    }
    // Nothing was stored, so the same message ID still makes a new task.
    assert!(
        kim.result("ListTasks", json!({})).await["tasks"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let task = kim.ask("When do we deploy?", "m-1").await;
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");
    check_task(&task);
}

#[tokio::test]
async fn version_none_0_3_and_2_0_get_32009_and_1_0_5_passes() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}}).to_string();
    let send_with = |version: Option<&'static str>, query: &'static str| {
        let client = A2a {
            url: format!("{}{query}", kim.url),
            token: kim.token.clone(),
            host: None,
        };
        let body = body.clone();
        async move { client.post(version, body).await.json::<Value>().await.unwrap() }
    };
    // No version means 0.3 (§3.6.2), and an empty header is no version either.
    for (version, query, shown) in [
        (None, "", "0.3"),
        (Some(""), "?A2A-Version=1.0", "0.3"),
        (Some("0.3"), "", "0.3"),
        (Some("2.0"), "", "2.0"),
        (Some("1"), "", "1"),
        (Some("1.1"), "", "1.1"),
        // The header wins over the query parameter.
        (Some("0.3"), "?A2A-Version=1.0", "0.3"),
        (None, "?A2A-Version=2.0", "2.0"),
    ] {
        let reply = send_with(version, query).await;
        let (code, text) = rpc_error(&reply);
        assert_eq!(code, -32009, "{version:?} {query}: {reply}");
        assert!(text.contains(&format!("version {shown} ")), "{text}");
        assert_eq!(reply["error"]["data"][0]["reason"], "VERSION_NOT_SUPPORTED");
        assert_eq!(reply["error"]["data"][0]["metadata"]["supportedVersions"], "1.0");
    }
    // A patch number does not count, and the query parameter is enough without the header.
    for (version, query) in [
        (Some("1.0"), ""),
        (Some("1.0.5"), ""),
        (Some(" 1.0 "), ""),
        (None, "?A2A-Version=1.0"),
        (None, "?a2a-version=1.0.2"),
    ] {
        let reply = send_with(version, query).await;
        assert!(reply["result"]["tasks"].is_array(), "{version:?} {query}: {reply}");
    }
}

#[tokio::test]
async fn the_task_shows_only_the_owners_decisions() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let ignored = kim.ask("Hello?", "m-1").await;
    let declined = kim.ask("Salaries?", "m-2").await;
    let answered = kim.ask("When do we deploy?", "m-3").await;
    let id = |task: &Value| task["id"].as_str().unwrap().to_owned();
    let (ignored, declined, answered) = (id(&ignored), id(&declined), id(&answered));
    let before = kim.task(&ignored).await;
    check_task(&before);
    assert_eq!(before["status"]["state"], "TASK_STATE_WORKING");
    assert_eq!(
        before["status"]["message"]["parts"][0]["text"],
        "The owner of this agent approves every answer. Poll GetTask."
    );
    assert_eq!(before["status"]["timestamp"], at(0));
    assert!(before.get("artifacts").is_none());
    let declined_before = kim.task(&declined).await;

    // The agent ignores one question and answers two. A caller sees none of it: the tasks stay as they were.
    rig.advance(60);
    rig.llm
        .reply(&ignore(), 10)
        .reply(&answer("No."), 10)
        .reply(&answer("Friday."), 10);
    assert!(rig.engine.drain_intake().await.is_empty());
    assert_eq!(kim.task(&ignored).await, before);
    assert_eq!(kim.task(&declined).await, declined_before);

    // The owner declines one answer: REJECTED, at the time of the decision.
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let item_of = |text: &str| {
        inbox
            .iter()
            .find(|item| format!("{:?}", item.body).contains(text))
            .unwrap()
            .id
    };
    rig.advance(60);
    rig.engine.reject("kim", item_of("No."), "not for them").await.unwrap();
    let task = kim.task(&declined).await;
    check_task(&task);
    assert_eq!(task["status"]["state"], "TASK_STATE_REJECTED");
    assert_eq!(task["status"]["timestamp"], at(120));
    assert!(task.get("artifacts").is_none());

    // The owner approves the other: COMPLETED with the approved answer and its AI marker.
    rig.advance(60);
    rig.engine.approve("kim", item_of("Friday."), None).await.unwrap();
    let task = kim.task(&answered).await;
    check_task(&task);
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    assert_eq!(task["status"]["timestamp"], at(180));
    assert!(task["status"].get("message").is_none());
    let artifact = &task["artifacts"][0];
    assert_eq!(
        (&artifact["artifactId"], &artifact["name"]),
        (&json!("answer"), &json!("answer"))
    );
    let text = artifact["parts"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("Friday.") && text.contains("— AI agent"), "{text}");

    // The owner dismisses the note about the ignored question: now that one is REJECTED too.
    rig.engine.reject("kim", item_of("gave no answer"), "").await.unwrap();
    assert_eq!(kim.task(&ignored).await["status"]["state"], "TASK_STATE_REJECTED");
    // No question text is in any task.
    let all = kim
        .result("ListTasks", json!({"includeArtifacts": true}))
        .await
        .to_string();
    assert!(!all.contains("Salaries?") && !all.contains("Hello?"), "{all}");
}

#[tokio::test]
async fn a_cancel_is_idempotent_and_a_final_task_is_not_cancelable() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let waiting = kim.ask("When?", "m-1").await["id"].as_str().unwrap().to_owned();
    rig.advance(30);
    let task = kim.result("CancelTask", json!({"id": waiting})).await;
    check_task(&task);
    assert_eq!(task["status"]["state"], "TASK_STATE_CANCELED");
    assert_eq!(task["status"]["timestamp"], at(30));
    // A cancel is idempotent (§3.3.1): the same task again, with the first time.
    rig.advance(30);
    assert_eq!(kim.result("CancelTask", json!({"id": waiting})).await, task);
    assert_eq!(kim.task(&waiting).await, task);

    // A final state other than CANCELED is not cancelable.
    let answered = kim.ask("Where?", "m-2").await["id"].as_str().unwrap().to_owned();
    rig.llm.reply(&answer("Here."), 10);
    rig.engine.drain_intake().await;
    rig.engine.approve("kim", rig.newest_item().await, None).await.unwrap();
    let (code, _) = kim.error("CancelTask", json!({"id": answered})).await;
    assert_eq!(code, -32002);
    assert_eq!(kim.task(&answered).await["status"]["state"], "TASK_STATE_COMPLETED");

    // MCP's get_request shows the cancel as well.
    let mcp = McpConnector::connect(
        "inbound",
        McpConfig::Http {
            url: format!("{}/agents/kim/mcp", rig.internal.url()),
        },
        Some(TokenAuth::bearer(&kim.token)),
    )
    .await
    .unwrap();
    let state = mcp
        .call_tool(
            "get_request",
            json!({"request_id": waiting}).as_object().unwrap().clone(),
        )
        .await
        .unwrap();
    assert_eq!(state["status"], "canceled");
}

#[tokio::test]
async fn another_callers_task_is_not_found_for_get_cancel_and_follow_up() {
    let rig = rig().await;
    let owner = rig.kim_client();
    let other = rig.kim_client();
    let task = owner.ask("When?", "m-1").await["id"].as_str().unwrap().to_owned();
    let follow_up = |task_id: &str| {
        json!({"message": {"role": "ROLE_USER", "parts": [{"text": "And?"}], "messageId": "m-2", "taskId": task_id},
               "configuration": {"returnImmediately": true}})
    };
    for id in [task.as_str(), "req_00000000000000000000000000000000"] {
        assert_eq!(other.error("GetTask", json!({"id": id})).await.0, -32001);
        assert_eq!(other.error("CancelTask", json!({"id": id})).await.0, -32001);
        assert_eq!(other.error("SendMessage", follow_up(id)).await.0, -32001);
    }
    // The other caller's cancel did nothing.
    assert_eq!(owner.task(&task).await["status"]["state"], "TASK_STATE_WORKING");
    // The owner of the task may not add to it either: that would change the question the owner approves.
    let (code, text) = owner.error("SendMessage", follow_up(&task)).await;
    assert_eq!(code, -32004);
    assert!(text.contains("taskId"), "{text}");
    assert_eq!(owner.result("ListTasks", json!({})).await["totalSize"], 1);
}

#[tokio::test]
async fn list_tasks_shows_only_the_callers_tasks_with_a_cursor() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let caller = rig.engine.app_store().caller(&kim.token).unwrap().unwrap();
    // One question over MCP, then four over A2A, one second apart. Another caller asks too.
    let mut mine = vec![rig.engine.ask(&caller, "Over MCP?").unwrap().request_id];
    for n in 0..4 {
        rig.advance(1);
        let context = if n < 2 { "ctx-a" } else { "ctx-b" };
        let mut params = send(&format!("Question {n}?"), &format!("m-{n}"));
        params["message"]["contextId"] = json!(context);
        let task = kim.result("SendMessage", params).await["task"].clone();
        assert_eq!(task["contextId"], context);
        mine.push(task["id"].as_str().unwrap().to_owned());
    }
    rig.kim_client().ask("Not yours?", "m-0").await;
    mine.reverse();

    // Pages of two, newest first, until the token is empty.
    let mut seen = Vec::new();
    let mut token = String::new();
    loop {
        let page = kim
            .result("ListTasks", json!({"pageSize": 2, "pageToken": token}))
            .await;
        assert_eq!(
            (page["pageSize"].as_i64(), page["totalSize"].as_i64()),
            (Some(2), Some(5))
        );
        for task in page["tasks"].as_array().unwrap() {
            check_task(task);
            assert!(task.get("artifacts").is_none());
            seen.push(task["id"].as_str().unwrap().to_owned());
        }
        token = page["nextPageToken"].as_str().unwrap().to_owned();
        if token.is_empty() {
            break;
        }
    }
    assert_eq!(seen, mine);

    // The filters: context, state and time.
    let ids = |page: &Value| -> Vec<String> {
        page["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| task["id"].as_str().unwrap().to_owned())
            .collect()
    };
    let page = kim.result("ListTasks", json!({"contextId": "ctx-a"})).await;
    assert_eq!(
        (ids(&page), &page["totalSize"]),
        (vec![mine[2].clone(), mine[3].clone()], &json!(2))
    );
    let page = kim.result("ListTasks", json!({"statusTimestampAfter": at(3)})).await;
    assert_eq!(ids(&page), mine[..2]);
    // The MCP question has its request ID as its context.
    let page = kim.result("ListTasks", json!({"contextId": mine[4]})).await;
    assert_eq!(ids(&page), [mine[4].clone()]);
    // An answered task moves to the front, and only `includeArtifacts` shows its answer.
    rig.llm.reply(&answer("Friday."), 10);
    rig.llm
        .reply(&ignore(), 10)
        .reply(&ignore(), 10)
        .reply(&ignore(), 10)
        .reply(&ignore(), 10);
    rig.engine.drain_intake().await;
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let reply_item = inbox
        .iter()
        .find(|item| format!("{:?}", item.body).contains("Friday."))
        .unwrap()
        .id;
    rig.advance(100);
    rig.engine.approve("kim", reply_item, None).await.unwrap();
    let completed = json!({"status": "TASK_STATE_COMPLETED"});
    let page = kim.result("ListTasks", completed.clone()).await;
    assert_eq!(ids(&page), [mine[4].clone()]);
    assert!(page["tasks"][0].get("artifacts").is_none());
    let mut with_artifacts = completed;
    with_artifacts["includeArtifacts"] = json!(true);
    let page = kim.result("ListTasks", with_artifacts).await;
    assert!(
        page["tasks"][0]["artifacts"][0]["parts"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("Friday.")
    );
    let page = kim.result("ListTasks", json!({"pageSize": 1})).await;
    assert_eq!(ids(&page), [mine[4].clone()]);
    assert_eq!(page["totalSize"], 5);
    // The working tasks still show no artifacts with `includeArtifacts`: an empty list.
    let page = kim
        .result(
            "ListTasks",
            json!({"status": "TASK_STATE_WORKING", "includeArtifacts": true}),
        )
        .await;
    assert_eq!(page["tasks"][0]["artifacts"], json!([]));

    for params in [
        json!({"pageSize": 0}),
        json!({"pageSize": 101}),
        json!({"pageToken": "nonsense"}),
        json!({"status": "TASK_STATE_RUNNING"}),
    ] {
        assert_eq!(kim.error("ListTasks", params.clone()).await.0, -32602, "{params}");
    }
}

#[tokio::test]
async fn send_message_checks_the_message() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let with = |change: &dyn Fn(&mut Value)| {
        let mut params = send("When?", "m-1");
        change(&mut params);
        params
    };
    let cases: Vec<(Value, i64)> = vec![
        (with(&|p| p["message"]["role"] = json!("ROLE_AGENT")), -32602),
        (with(&|p| p["message"]["messageId"] = json!("")), -32602),
        (
            with(&|p| {
                p["message"]
                    .as_object_mut()
                    .unwrap()
                    .remove("messageId")
                    .map(drop)
                    .unwrap()
            }),
            -32602,
        ),
        (with(&|p| p["message"]["parts"] = json!([])), -32602),
        (with(&|p| p["message"]["parts"] = json!([{"text": "   "}])), -32602),
        (
            with(&|p| p["message"]["parts"] = json!([{"text": "x".repeat(4_001)}])),
            -32602,
        ),
        (with(&|p| p["message"]["contextId"] = json!("has space")), -32602),
        (with(&|p| p["message"]["contextId"] = json!("c".repeat(129))), -32602),
        (
            with(&|p| p["message"]["parts"] = json!([{"text": "a"}, {"data": {"x": 1}}])),
            -32005,
        ),
        (
            with(&|p| p["message"]["parts"] = json!([{"url": "https://evil.example/q.txt"}])),
            -32005,
        ),
        (
            with(&|p| p["message"]["parts"] = json!([{"text": "{}", "mediaType": "application/json"}])),
            -32005,
        ),
        (
            with(&|p| p["configuration"]["acceptedOutputModes"] = json!(["application/json"])),
            -32005,
        ),
        (
            with(&|p| p["configuration"]["taskPushNotificationConfig"] = json!({"url": "https://client.example/hook"})),
            -32003,
        ),
    ];
    for (params, code) in cases {
        assert_eq!(kim.error("SendMessage", params.clone()).await.0, code, "{params}");
    }
    assert_eq!(
        kim.result("ListTasks", json!({})).await["totalSize"],
        0,
        "no refused message was stored"
    );

    // Text parts are joined; `text/*` output and a 128-character context are fine.
    let mut params = with(&|p| {
        p["message"]["parts"] = json!([{"text": "When"}, {"text": "do we deploy?", "mediaType": "text/plain"}]);
        p["message"]["contextId"] = json!("c".repeat(128));
        p["configuration"]["acceptedOutputModes"] = json!(["text/*"]);
    });
    let task = kim.result("SendMessage", params.clone()).await["task"].clone();
    check_task(&task);
    // A resent message gets the first task back and is stored once.
    params["message"]["parts"] = json!([{"text": "Something else"}]);
    assert_eq!(kim.result("SendMessage", params).await["task"], task);
    assert_eq!(kim.result("ListTasks", json!({})).await["totalSize"], 1);
    rig.llm.reply(&ignore(), 10);
    rig.engine.drain_intake().await;
    let question = format!("{:?}", rig.llm.requests());
    assert!(question.contains("When\\ndo we deploy?"), "{question}");
}

#[tokio::test]
async fn capability_methods_get_32004_and_32003() {
    let rig = rig().await;
    let kim = rig.kim_client();
    for method in ["SendStreamingMessage", "SubscribeToTask", "GetExtendedAgentCard"] {
        assert_eq!(kim.error(method, json!({"id": "t"})).await.0, -32004, "{method}");
    }
    for method in [
        "CreateTaskPushNotificationConfig",
        "GetTaskPushNotificationConfig",
        "ListTaskPushNotificationConfigs",
        "DeleteTaskPushNotificationConfig",
    ] {
        assert_eq!(kim.error(method, json!({"taskId": "t"})).await.0, -32003, "{method}");
    }
    assert_eq!(kim.error("message/send", json!({})).await.0, -32601);
    assert_eq!(kim.error("GetTask", json!({"historyLength": 1})).await.0, -32602);

    // The envelope: JSON, one request object, an ID and version 2.0.
    let raw = |body: &'static str| {
        let kim = &kim;
        async move {
            kim.post(Some("1.0"), body.to_owned())
                .await
                .json::<Value>()
                .await
                .unwrap()
        }
    };
    let reply = raw("{not json").await;
    assert_eq!((rpc_error(&reply).0, &reply["id"]), (-32700, &Value::Null));
    for body in [
        r#"[{"jsonrpc": "2.0", "id": 1, "method": "ListTasks"}]"#,
        r#"{"jsonrpc": "2.0", "method": "ListTasks"}"#,
        r#"{"jsonrpc": "1.0", "id": 1, "method": "ListTasks"}"#,
        r#"{"jsonrpc": "2.0", "id": 1}"#,
    ] {
        assert_eq!(rpc_error(&raw(body).await).0, -32600, "{body}");
    }
    // A string ID comes back as it was sent.
    let reply = raw(r#"{"jsonrpc": "2.0", "id": "abc", "method": "ListTasks"}"#).await;
    assert_eq!(reply["id"], "abc");
}

#[tokio::test]
async fn the_endpoint_takes_only_a_caller_token_of_its_profile_and_its_own_host() {
    let rig = rig().await;
    let kim_token = rig.token("kim");
    let lee_token = rig.token("lee");
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {}}).to_string();
    let status = |listener: &Running, profile: &str, token: &str| {
        let client = A2a::new(listener, profile, token);
        let body = body.clone();
        async move { client.post(Some("1.0"), body).await }
    };
    assert_eq!(status(&rig.internal, "kim", &kim_token).await.status(), 200);
    for (listener, profile, token) in [
        (&rig.internal, "kim", lee_token.as_str()),
        (&rig.internal, "kim", "bsc_guess"),
        (&rig.internal, "nobody", kim_token.as_str()),
        // Lee serves MCP but not A2A.
        (&rig.internal, "lee", lee_token.as_str()),
        // Kim's scope is internal.
        (&rig.public, "kim", kim_token.as_str()),
    ] {
        let response = status(listener, profile, token).await;
        assert_eq!(response.status(), 401, "{profile} {token}");
        assert_eq!(response.headers()["www-authenticate"], "Bearer");
        let error: Value = response.json().await.unwrap();
        assert_eq!(
            error,
            json!({"error": "a valid caller token of this agent is required"})
        );
    }
    // The MCP endpoint refuses with the same answer.
    let mcp = reqwest::Client::new()
        .post(format!("{}/agents/kim/mcp", rig.internal.url()))
        .bearer_auth("bsc_guess")
        .send()
        .await
        .unwrap();
    assert_eq!(mcp.status(), 401);
    assert_eq!(mcp.headers()["www-authenticate"], "Bearer");

    let mut kim = A2a::new(&rig.internal, "kim", &kim_token);
    kim.host = Some("evil.example".into());
    assert_eq!(kim.post(Some("1.0"), body.clone()).await.status(), 403);
    kim.host = None;
    let big = json!({"jsonrpc": "2.0", "id": 1, "method": "ListTasks", "params": {"pad": "x".repeat(70_000)}});
    assert_eq!(kim.post(Some("1.0"), big.to_string()).await.status(), 413);

    // Public scope: the public listener serves kim, with its own host only.
    rig.change("kim", |config| config.inbound.scope = InboundScope::Public)
        .await;
    let mut public = A2a::new(&rig.public, "kim", &kim_token);
    assert_eq!(public.post(Some("1.0"), body.clone()).await.status(), 403);
    public.host = Some("localhost".into());
    assert_eq!(public.post(Some("1.0"), body.clone()).await.status(), 200);
    // A2A off: the A2A endpoint goes, the MCP endpoint stays.
    rig.change("kim", |config| config.inbound.a2a = false).await;
    assert_eq!(kim.post(Some("1.0"), body).await.status(), 401);
    let mcp = McpConnector::connect(
        "inbound",
        McpConfig::Http {
            url: format!("{}/agents/kim/mcp", rig.internal.url()),
        },
        Some(TokenAuth::bearer(&kim_token)),
    )
    .await;
    assert!(mcp.is_ok());
}

#[tokio::test]
async fn the_card_is_served_only_where_a2a_is_served() {
    let rig = rig().await;
    rig.change("kim", |config| {
        config.inbound.ard = Some(ArdListing {
            publisher: "acme.com".into(),
            agent_name: "kim".into(),
            representative_queries: vec!["When do we deploy?".into(), "Who reviews a release?".into()],
            tags: vec![" deploys ".into(), "releases".into()],
            ..ArdListing::default()
        });
    })
    .await;
    let get = |listener: &Running, profile: &str| {
        reqwest::Client::new()
            .get(format!("{}/agents/{profile}/a2a/agent-card.json", listener.url()))
            .send()
    };
    let response = get(&rig.internal, "kim").await.unwrap();
    assert_eq!(response.status(), 200);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(headers["cache-control"], "public, max-age=300");
    assert!(headers["etag"].to_str().unwrap().starts_with('"'));
    let card: Value = response.json().await.unwrap();
    assert_eq!(schema_errors("Agent Card", &card), Vec::<String>::new(), "{card}");
    // The crate type reads it back.
    serde_json::from_value::<a2a::AgentCard>(card.clone()).unwrap();
    assert_eq!(card["name"], "Kim's agent");
    assert_eq!(card["description"], "Questions about the deploy schedule.");
    assert_eq!(card["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        card["supportedInterfaces"],
        json!([{"url": "http://127.0.0.1/agents/kim/a2a", "protocolBinding": "JSONRPC", "protocolVersion": "1.0"}])
    );
    assert_eq!(
        card["capabilities"],
        json!({"streaming": false, "pushNotifications": false, "extendedAgentCard": false})
    );
    assert_eq!(
        card["securitySchemes"]["callerToken"]["httpAuthSecurityScheme"]["scheme"],
        "Bearer"
    );
    assert_eq!(
        card["securityRequirements"],
        json!([{"schemes": {"callerToken": {"list": []}}}])
    );
    let skill = &card["skills"][0];
    // The skill takes the ARD tags, trimmed.
    assert_eq!(
        (&skill["id"], &skill["tags"]),
        (&json!("ask"), &json!(["deploys", "releases"]))
    );
    assert!(
        skill["description"]
            .as_str()
            .unwrap()
            .ends_with("The owner approves every answer.")
    );
    assert_eq!(
        skill["examples"],
        json!(["When do we deploy?", "Who reviews a release?"])
    );
    // The required arrays are not empty (§5.7).
    for field in [
        "supportedInterfaces",
        "defaultInputModes",
        "defaultOutputModes",
        "skills",
    ] {
        assert!(card[field].as_array().is_some_and(|list| !list.is_empty()), "{field}");
    }
    // No owner name and no token.
    let text = card.to_string();
    assert!(
        !text.contains("Kim\"") && !text.contains("bsc_") && !text.contains("bsn_"),
        "{text}"
    );

    // Lee serves no A2A, kim is not on the public listener, and nobody does not exist.
    for (listener, profile) in [(&rig.internal, "lee"), (&rig.public, "kim"), (&rig.internal, "nobody")] {
        assert_eq!(get(listener, profile).await.unwrap().status(), 404, "{profile}");
    }
    // Turning A2A off removes the card at once; the MCP server card stays.
    rig.change("kim", |config| config.inbound.a2a = false).await;
    assert_eq!(get(&rig.internal, "kim").await.unwrap().status(), 404);
    let server_card = reqwest::get(format!("{}/agents/kim/mcp/server-card", rig.internal.url()))
        .await
        .unwrap();
    assert_eq!(server_card.status(), 200);
    // Without a title, the name is the profile ID.
    rig.change("kim", |config| {
        config.inbound.a2a = true;
        config.inbound.title = None;
    })
    .await;
    let card: Value = get(&rig.internal, "kim").await.unwrap().json().await.unwrap();
    assert_eq!(card["name"], "kim");
}

#[tokio::test]
async fn a2a_and_mcp_share_the_read_limit() {
    let rig = rig().await;
    let kim = rig.kim_client();
    let task = kim.ask("When?", "m-1").await["id"].as_str().unwrap().to_owned();
    let mcp = McpConnector::connect(
        "inbound",
        McpConfig::Http {
            url: format!("{}/agents/kim/mcp", rig.internal.url()),
        },
        Some(TokenAuth::bearer(&kim.token)),
    )
    .await
    .unwrap();
    let read_over_mcp = || mcp.call_tool("get_request", json!({"request_id": task}).as_object().unwrap().clone());
    for _ in 0..40 {
        kim.task(&task).await;
        kim.result("ListTasks", json!({})).await;
        read_over_mcp().await.unwrap();
    }
    // 120 reads: the next one fails over both protocols, and also for a cancel.
    let reply = kim.call("GetTask", json!({"id": task})).await;
    let (code, text) = rpc_error(&reply);
    assert_eq!(code, -32000);
    assert!(text.contains("a minute"), "{text}");
    assert_eq!(reply["error"]["data"][0]["reason"], "RATE_LIMIT_EXCEEDED");
    assert_eq!(kim.error("CancelTask", json!({"id": task})).await.0, -32000);
    let refused = read_over_mcp().await;
    assert!(refused.is_err_and(|error| error.to_string().contains("a minute")));
    // The cancel did not run, and a SendMessage is no read.
    kim.ask("Where?", "m-2").await;
    assert_eq!(
        rig.engine
            .caller_request(&rig.engine.app_store().caller(&kim.token).unwrap().unwrap(), &task)
            .unwrap()
            .status,
        bunsin_core::inbound::RequestStatus::Working
    );
    // The limit is per caller.
    let other = rig.kim_client();
    other.result("ListTasks", json!({})).await;
}
