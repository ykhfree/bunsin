//! Connection settings and their tokens: stored encrypted, never returned, bound to an address.

mod common;

use bunsin_api::{ApiError, ConnectorUpdate, EngineApi, LlmUpdate, ProfileConfig, TokenChange, TokenSource};
use bunsin_core::ports::BoxFuture;
use bunsin_core::settings::{ConnectorKind, McpConfig, Wire};
use bunsin_engine::{Backends, Engine, ProfileSecrets, Runtime};
use common::*;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "sk-llm-token-7f3a9c";
const SLACK_TOKEN: &str = "xoxb-slack-token-51d2";

fn llm(base_url: &str, token: TokenChange) -> LlmUpdate {
    LlmUpdate {
        wire: Wire::AnthropicMessages,
        base_url: base_url.into(),
        model: "m1".into(),
        timeout_secs: 120,
        token,
        expected: bunsin_api::Expected::Unchecked,
    }
}

fn connector(url: &str, token: TokenChange) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.into(),
        token,
        token_header: None,
        expected: bunsin_api::Expected::Unchecked,
    }
}

fn set(token: &str) -> TokenChange {
    TokenChange::Set(token.into())
}

#[tokio::test]
async fn a_saved_token_is_reported_as_stored_and_is_nowhere_in_plain_text() {
    let rig = rig().await;
    rig.engine
        .set_llm("kim", llm("https://llm.example/v1", set(TOKEN)))
        .await
        .unwrap();
    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", set(SLACK_TOKEN)))
        .await
        .unwrap();

    let connections = rig.engine.connections("kim").await.unwrap();
    let saved = connections.llm.unwrap();
    assert_eq!(
        (saved.base_url.as_str(), saved.model.as_str(), saved.token),
        ("https://llm.example/v1", "m1", TokenSource::Stored)
    );
    let jira = &connections.connectors["jira"];
    assert_eq!(
        jira.kind,
        ConnectorKind::Mcp(McpConfig::Http {
            url: "https://mcp.example/jira".into()
        })
    );
    assert_eq!(jira.token, TokenSource::Stored);

    // The token is in no file, no settings output, no audit entry and no API answer.
    for token in [TOKEN, SLACK_TOKEN] {
        assert!(!rig.stores(token), "{token} is on disk in plain text");
        let config = serde_json::to_string(&rig.engine.profile_config("kim").await.unwrap()).unwrap();
        let snapshot = serde_json::to_string(&rig.engine.snapshot("kim").await.unwrap()).unwrap();
        let listed = serde_json::to_string(&rig.engine.connections("kim").await.unwrap()).unwrap();
        assert!(!config.contains(token) && !snapshot.contains(token) && !listed.contains(token));
    }
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert!(
        audit
            .iter()
            .any(|a| a.kind == "connection_changed" && a.detail["target"] == "jira" && a.detail["token"] == "set")
    );
}

#[tokio::test]
async fn a_kept_token_does_not_follow_the_address_to_another_host() {
    let rig = rig().await;
    rig.engine
        .set_llm("kim", llm("https://llm.example/v1", set(TOKEN)))
        .await
        .unwrap();
    // Another path on the same host is the same origin.
    rig.engine
        .set_llm("kim", llm("https://llm.example/v2", TokenChange::Keep))
        .await
        .unwrap();

    for moved in [
        "https://attacker.example/v1",
        "https://llm.example:8443/v1",
        "http://llm.example/v1",
    ] {
        let error = rig
            .engine
            .set_llm("kim", llm(moved, TokenChange::Keep))
            .await
            .unwrap_err();
        assert!(
            matches!(error, ApiError::Invalid(ref m) if m.contains("enter the token again")),
            "{moved}: {error:?}"
        );
    }
    // The refused saves changed nothing.
    assert_eq!(
        rig.engine.connections("kim").await.unwrap().llm.unwrap().base_url,
        "https://llm.example/v2"
    );

    // With the token entered again, the new address is accepted.
    rig.engine
        .set_llm("kim", llm("https://other.example/v1", set("sk-new")))
        .await
        .unwrap();
    assert_eq!(
        rig.engine.connections("kim").await.unwrap().llm.unwrap().base_url,
        "https://other.example/v1"
    );

    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", set(SLACK_TOKEN)))
        .await
        .unwrap();
    let error = rig
        .engine
        .set_connector("kim", "jira", connector("https://evil.example/jira", TokenChange::Keep))
        .await;
    assert!(matches!(error, Err(ApiError::Invalid(_))), "{error:?}");
}

#[tokio::test]
async fn a_token_can_be_cleared_and_bad_input_is_refused() {
    let rig = rig().await;
    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", set(SLACK_TOKEN)))
        .await
        .unwrap();
    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", TokenChange::Clear))
        .await
        .unwrap();
    assert_eq!(
        rig.engine.connections("kim").await.unwrap().connectors["jira"].token,
        TokenSource::None
    );

    let refused = |result: Result<(), ApiError>, what: &str| {
        assert!(matches!(result, Err(ApiError::Invalid(_))), "{what}: {result:?}")
    };
    refused(
        rig.engine
            .set_connector("kim", "jira", connector("https://mcp.example/jira", set("  ")))
            .await,
        "empty token",
    );
    refused(
        rig.engine
            .set_connector("kim", "jira", connector("not an address", set("t")))
            .await,
        "bad address",
    );
    for name in ["", "Jira", "my jira", "a/b", "../x"] {
        refused(
            rig.engine
                .set_connector("kim", name, connector("https://mcp.example/x", TokenChange::Keep))
                .await,
            name,
        );
    }
    let mut no_model = llm("https://llm.example", set(TOKEN));
    no_model.model = " ".into();
    refused(rig.engine.set_llm("kim", no_model).await, "empty model");
    assert!(rig.engine.connections("kim").await.unwrap().llm.is_none());
    assert!(!rig.stores(TOKEN));
}

#[tokio::test]
async fn removing_a_connector_removes_its_token() {
    let rig = rig().await;
    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", set(SLACK_TOKEN)))
        .await
        .unwrap();
    rig.engine.remove_connector("kim", "jira").await.unwrap();
    assert!(rig.engine.connections("kim").await.unwrap().connectors.is_empty());
    assert!(matches!(
        rig.engine.remove_connector("kim", "jira").await,
        Err(ApiError::NotFound(_))
    ));

    // A new connector with the same name starts without the old token.
    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", TokenChange::Keep))
        .await
        .unwrap();
    assert_eq!(
        rig.engine.connections("kim").await.unwrap().connectors["jira"].token,
        TokenSource::None
    );
}

#[tokio::test]
async fn saving_a_connection_keeps_the_other_settings() {
    let rig = rig().await;
    let mut before = rig.engine.profile_config("kim").await.unwrap();
    before.principles = vec!["Answer briefly.".into()];
    before.polls = vec![poll()];
    rig.engine.set_profile_config("kim", before.clone()).await.unwrap();
    rig.engine
        .set_llm("kim", llm("https://llm.example", set(TOKEN)))
        .await
        .unwrap();
    rig.engine
        .set_connector("kim", "jira", connector("https://mcp.example/jira", TokenChange::Keep))
        .await
        .unwrap();
    let after = rig.engine.profile_config("kim").await.unwrap();
    assert_eq!(
        (&after.principles, &after.polls, &after.policy, &after.privacy),
        (&before.principles, &before.polls, &before.policy, &before.privacy)
    );
}

/// Records the secrets that each build of the clients received.
struct Recording {
    seen: Mutex<Vec<Vec<(String, String, String)>>>,
}

impl Backends for Recording {
    fn build<'a>(&'a self, _: &'a str, _: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async { Err("build without secrets must not be used".to_owned()) })
    }

    fn build_with_secrets<'a>(
        &'a self,
        _profile: &'a str,
        _config: &'a ProfileConfig,
        secrets: &'a ProfileSecrets,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        let flat = secrets
            .iter()
            .map(|(key, secret)| {
                let secret = secret.clone().expect("the stored secret decrypts");
                (key.clone(), secret.value, secret.origin)
            })
            .collect();
        self.seen.lock().unwrap().push(flat);
        Box::pin(async {
            Ok(Runtime {
                llm: Err("no LLM in this test".to_owned()),
                connectors: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn the_clients_get_the_decrypted_token_with_its_origin_and_are_rebuilt_after_a_change() {
    let dir = tempfile::tempdir().unwrap();
    let backends = Arc::new(Recording {
        seen: Mutex::new(Vec::new()),
    });
    let engine = Engine::open(dir.path(), backends.clone(), Arc::new(|| 1_000)).unwrap();
    engine.create_profile("kim", config()).await.unwrap();
    engine.create_profile("lee", config()).await.unwrap();

    engine
        .set_llm("kim", llm("https://llm.example/v1", set(TOKEN)))
        .await
        .unwrap();
    engine
        .set_connector(
            "kim",
            "jira",
            connector("https://mcp.example:8443/jira", set(SLACK_TOKEN)),
        )
        .await
        .unwrap();
    let _ = engine.connector_tools("kim", "jira").await;
    let _ = engine.connector_tools("kim", "jira").await;
    // Two uses built the clients once.
    assert_eq!(backends.seen.lock().unwrap().len(), 1);
    assert_eq!(
        backends.seen.lock().unwrap()[0],
        [
            ("LLM".to_owned(), TOKEN.to_owned(), "https://llm.example:443".to_owned()),
            (
                "MCP_JIRA".to_owned(),
                SLACK_TOKEN.to_owned(),
                "https://mcp.example:8443".to_owned()
            ),
        ]
    );

    // A new token makes the next use build the clients again, with the new token.
    engine
        .set_llm("kim", llm("https://llm.example/v1", set("sk-second")))
        .await
        .unwrap();
    let _ = engine.connector_tools("kim", "jira").await;
    let builds = backends.seen.lock().unwrap().clone();
    assert_eq!(builds.len(), 2);
    assert_eq!(builds[1][0].1, "sk-second");

    // Another profile gets none of these secrets.
    let _ = engine.connector_tools("lee", "jira").await;
    assert!(backends.seen.lock().unwrap()[2].is_empty());
}

#[tokio::test]
async fn a_token_sealed_with_another_key_is_reported_and_not_used() {
    let dir = tempfile::tempdir().unwrap();
    let backends = Arc::new(Unreadable {
        errors: Mutex::new(Vec::new()),
    });
    {
        let engine = Engine::open(dir.path(), backends.clone(), Arc::new(|| 1_000)).unwrap();
        engine.create_profile("kim", config()).await.unwrap();
        engine
            .set_llm("kim", llm("https://llm.example", set(TOKEN)))
            .await
            .unwrap();
    }
    // The server starts with another key file, as after a lost key.
    let other_key = dir.path().join("other.key");
    std::fs::write(&other_key, [7u8; 32]).unwrap();
    let engine = Engine::open_with_key_file(dir.path(), &other_key, backends.clone(), Arc::new(|| 1_000)).unwrap();
    let _ = engine.connector_tools("kim", "slack").await;
    let errors = backends.errors.lock().unwrap().clone();
    assert_eq!(errors.len(), 1);
    assert!(
        errors[0].contains("cannot be read with this server's key; enter it again"),
        "{errors:?}"
    );
    assert!(!errors[0].contains(TOKEN));
    // The owner sees that a token is stored, and can enter it again.
    assert_eq!(
        engine.connections("kim").await.unwrap().llm.unwrap().token,
        TokenSource::Stored
    );
    engine
        .set_llm("kim", llm("https://llm.example", set(TOKEN)))
        .await
        .unwrap();
}

/// Records the errors of secrets that could not be decrypted.
struct Unreadable {
    errors: Mutex<Vec<String>>,
}

impl Backends for Unreadable {
    fn build<'a>(&'a self, _: &'a str, _: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async { Err("unused".to_owned()) })
    }

    fn build_with_secrets<'a>(
        &'a self,
        _profile: &'a str,
        _config: &'a ProfileConfig,
        secrets: &'a ProfileSecrets,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        self.errors
            .lock()
            .unwrap()
            .extend(secrets.values().filter_map(|secret| secret.clone().err()));
        Box::pin(async {
            Ok(Runtime {
                llm: Err("no LLM".to_owned()),
                connectors: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn one_failing_connector_does_not_stop_the_rest_of_the_profile() {
    use bunsin_core::ports::{Connector, LlmClient};
    use bunsin_testkit::{FakeConnector, FakeLlm};
    struct Half {
        llm: Arc<FakeLlm>,
        slack: Arc<FakeConnector>,
    }
    impl Backends for Half {
        fn build<'a>(&'a self, _: &'a str, _: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
            let llm: Arc<dyn LlmClient> = self.llm.clone();
            let slack: Arc<dyn Connector> = self.slack.clone();
            Box::pin(async move {
                let connectors = std::collections::BTreeMap::from([
                    ("slack".to_owned(), Ok(slack)),
                    (
                        "jira".to_owned(),
                        Err("connector `jira` cannot connect: connection refused".to_owned()),
                    ),
                ]);
                Ok(Runtime {
                    llm: Ok(llm),
                    connectors,
                })
            })
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let slack = Arc::new(FakeConnector::new("slack", slack_tools()));
    let engine = Engine::open(
        dir.path(),
        Arc::new(Half {
            llm: llm.clone(),
            slack: slack.clone(),
        }),
        Arc::new(|| 1_000),
    )
    .unwrap();
    engine.create_profile("kim", config()).await.unwrap();

    // Slack still works.
    llm.reply(&post("hello"), 10);
    let outcome = engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap();
    let bunsin_api::Outcome::PendingApproval { inbox_id } = outcome else {
        panic!("expected a pending item, got {outcome:?}")
    };
    engine.approve("kim", inbox_id, None).await.unwrap();
    assert_eq!(slack.calls().len(), 1);
    assert!(engine.connector_tools("kim", "slack").await.is_ok());

    // The broken connector says why it cannot be used.
    let error = engine.connector_tools("kim", "jira").await.unwrap_err();
    assert!(
        matches!(error, ApiError::NotAllowed(ref m) if m.contains("connection refused")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_connector_that_runs_as_a_command_is_not_replaced_by_an_address() {
    let rig = rig().await;
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    let stdio = serde_json::json!({"mcp": {"transport": "stdio", "command": "/opt/mcp/wiki", "args": [], "env": {}}});
    config.connectors.insert("wiki".into(), stdio.clone());
    rig.engine.set_profile_config("kim", config).await.unwrap();

    let result = rig
        .engine
        .set_connector("kim", "wiki", connector("https://attacker.example/mcp", set("t")))
        .await;
    assert!(
        matches!(result, Err(ApiError::Invalid(ref m)) if m.contains("runs a command on the server")),
        "{result:?}"
    );
    assert_eq!(
        rig.engine.profile_config("kim").await.unwrap().connectors["wiki"],
        stdio
    );
    assert!(matches!(
        rig.engine.connections("kim").await.unwrap().connectors["wiki"].kind,
        ConnectorKind::Mcp(McpConfig::Stdio { .. })
    ));
}
