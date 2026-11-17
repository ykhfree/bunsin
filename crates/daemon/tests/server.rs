//! The HTTP server and the remote client together, against a real engine with fakes.

use bunsin_api::{
    ApiError, ConnectorUpdate, Edit, EngineApi, LlmUpdate, Outcome, ProfileConfig, ReplayCase, TokenChange, TokenSource,
};
use bunsin_client::{RemoteConfig, RemoteEngine};
use bunsin_connectors::{McpConfig, McpConnector, TokenAuth};
use bunsin_core::approval::ApprovalState;
use bunsin_core::event::InboundEvent;
use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope, RequestStatus};
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use bunsin_core::ports::ConnectorError;
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use bunsin_core::settings::Wire;
use bunsin_daemon::server::{OuterConfig, Reach, Running, ServeConfig, ServeError, check_listeners, serve, start};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Backends, Engine, Runtime};
use bunsin_testkit::{FakeConnector, FakeLlm, tool};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Scripted clients, with the settings check of the real backends.
struct Fakes {
    llm: Arc<FakeLlm>,
    slack: Arc<FakeConnector>,
    real: RealBackends,
}

impl Backends for Fakes {
    fn build<'a>(&'a self, _profile: &'a str, _config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        let slack: Arc<dyn Connector> = self.slack.clone();
        Box::pin(async move {
            Ok(Runtime {
                llm: Ok(llm),
                connectors: BTreeMap::from([("slack".to_owned(), Ok(slack))]),
            })
        })
    }

    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        self.real.validate(profile, config)
    }

    fn ard_publishers(&self) -> Vec<String> {
        self.real.ard_publishers()
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    llm: Arc<FakeLlm>,
    slack: Arc<FakeConnector>,
    engine: Arc<Engine>,
    clock: Arc<std::sync::atomic::AtomicI64>,
    server: Running,
    kim_token: String,
    lee_token: String,
}

impl Rig {
    fn client(&self, token: &str) -> RemoteEngine {
        RemoteEngine::new(
            &RemoteConfig {
                url: self.server.url(),
                ca_cert_pem: None,
            },
            token,
        )
        .unwrap()
    }

    fn kim(&self) -> RemoteEngine {
        self.client(&self.kim_token)
    }
}

fn profile_config(owner: &str) -> ProfileConfig {
    let mut config = ProfileConfig::new(owner);
    config
        .approved_tools
        .insert("slack".into(), vec![tool("post", "Post a message")]);
    let rule = ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into()));
    config.policy.rules.insert("slack/post".into(), rule);
    config
}

async fn rig_with(tls: Option<(std::path::PathBuf, std::path::PathBuf)>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let slack = Arc::new(FakeConnector::new("slack", vec![tool("post", "Post a message")]));
    let real = RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default())
        .with_ard_publishers(&["ACME.com".into()])
        .unwrap();
    let backends = Arc::new(Fakes {
        llm: llm.clone(),
        slack: slack.clone(),
        real,
    });
    let clock = Arc::new(std::sync::atomic::AtomicI64::new(1_000_000));
    let time = clock.clone();
    let now = move || time.load(std::sync::atomic::Ordering::SeqCst);
    let engine = Arc::new(Engine::open(dir.path(), backends, Arc::new(now)).unwrap());
    engine.create_profile("kim", profile_config("Kim")).await.unwrap();
    engine.create_profile("lee", profile_config("Lee")).await.unwrap();
    let kim_token = engine.app_store().add_token("kim", "test", 1).unwrap();
    let lee_token = engine.app_store().add_token("lee", "test", 1).unwrap();
    let listen = "127.0.0.1:0".parse().unwrap();
    let server = serve(engine.clone(), ServeConfig { listen, tls }).await.unwrap();
    Rig {
        _dir: dir,
        clock,
        llm,
        slack,
        engine,
        server,
        kim_token,
        lee_token,
    }
}

async fn rig() -> Rig {
    rig_with(None).await
}

fn event(id: &str) -> InboundEvent {
    InboundEvent {
        source: "slack".into(),
        external_id: id.into(),
        thread: None,
        author: "lee".into(),
        text: "hi".into(),
        channel: Default::default(),
    }
}

fn post(text: &str) -> String {
    let args = json!({"channel": "C1", "text": text}).to_string();
    json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "slack/post", "args_json": args})
        .to_string()
}

#[tokio::test]
async fn an_owner_works_through_the_server_like_through_the_engine() {
    let rig = rig().await;
    let kim = rig.kim();

    rig.llm.reply(&post("Deployed."), 10);
    let Outcome::PendingApproval { inbox_id } = kim.ingest("kim", event("e1")).await.unwrap() else {
        panic!("expected a pending item")
    };
    let snapshot = kim.snapshot("kim").await.unwrap();
    assert_eq!(snapshot.inbox.len(), 1);
    assert_eq!(snapshot.inbox[0].state, ApprovalState::Pending);
    assert!(!snapshot.kill_switch);

    let edit = Edit {
        args: json!({"channel": "C1", "text": "Deployed at 3 pm."})
            .as_object()
            .unwrap()
            .clone(),
        reason: "add the time".into(),
    };
    assert_eq!(
        kim.approve("kim", inbox_id, Some(edit)).await.unwrap(),
        Outcome::Executed { inbox_id }
    );
    let calls = rig.slack.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].1["text"]
            .as_str()
            .unwrap()
            .starts_with("Deployed at 3 pm.\n\n— AI agent")
    );

    assert!(kim.set_kill_switch("kim", true).await.unwrap());
    assert!(kim.snapshot("kim").await.unwrap().kill_switch);
    assert!(!kim.set_kill_switch("kim", false).await.unwrap());

    rig.llm.reply(&post("second"), 10);
    let Outcome::PendingApproval { inbox_id: second } = kim.ingest("kim", event("e2")).await.unwrap() else {
        panic!("expected a pending item")
    };
    kim.reject("kim", second, "not needed").await.unwrap();
    assert_eq!(
        kim.snapshot("kim").await.unwrap().inbox[0].state,
        ApprovalState::Rejected
    );

    rig.llm.reply(&post("replayed"), 10);
    let case = ReplayCase {
        event: event("old"),
        actual_reply: "REAL".into(),
        expect: bunsin_core::eval::Expect::Reply,
    };
    let result = kim.replay("kim", case).await.unwrap();
    assert_eq!(result.actual_reply, "REAL");
    assert!(result.proposal.is_some());

    let mut config = kim.profile_config("kim").await.unwrap();
    config.principles.push("Answer briefly.".into());
    kim.set_profile_config("kim", config.clone()).await.unwrap();
    assert_eq!(kim.profile_config("kim").await.unwrap(), config);
}

#[tokio::test]
async fn engine_errors_keep_their_kind_across_the_network() {
    let rig = rig().await;
    let kim = rig.kim();
    assert!(matches!(kim.approve("kim", 42, None).await, Err(ApiError::NotFound(_))));
    rig.llm.reply(&post("x"), 10);
    let Outcome::PendingApproval { inbox_id } = kim.ingest("kim", event("e1")).await.unwrap() else {
        panic!()
    };
    kim.approve("kim", inbox_id, None).await.unwrap();
    assert!(matches!(
        kim.approve("kim", inbox_id, None).await,
        Err(ApiError::NotAllowed(_))
    ));
    assert_eq!(rig.slack.calls().len(), 1);
}

#[tokio::test]
async fn a_token_reaches_only_its_own_profile() {
    let rig = rig().await;
    let kim = rig.kim();
    rig.llm.reply(&post("for lee"), 10);
    let Outcome::PendingApproval { inbox_id } = rig.client(&rig.lee_token).ingest("lee", event("e1")).await.unwrap()
    else {
        panic!("expected a pending item")
    };

    rig.engine
        .set_wiki_page("lee", "team", note("Lee leads."))
        .await
        .unwrap();

    // The list shows the caller's own profile and nothing about others.
    let listed: Vec<String> = kim.list_profiles().await.unwrap().into_iter().map(|p| p.id).collect();
    assert_eq!(listed, ["kim"]);

    // Every route refuses kim's token for lee's profile, and for a profile that does not exist,
    // with the same error.
    for other in ["lee", "nobody"] {
        let refused = |result: Result<(), ApiError>| {
            assert!(matches!(result, Err(ApiError::Unauthorized(_))), "{other}: {result:?}");
        };
        refused(kim.snapshot(other).await.map(|_| ()));
        refused(kim.profile_config(other).await.map(|_| ()));
        refused(kim.set_profile_config(other, ProfileConfig::new("Mallory")).await);
        refused(kim.ingest(other, event("e9")).await.map(|_| ()));
        refused(kim.approve(other, inbox_id, None).await.map(|_| ()));
        refused(kim.reject(other, inbox_id, "x").await);
        refused(kim.set_kill_switch(other, true).await.map(|_| ()));
        refused(kim.connector_tools(other, "slack").await.map(|_| ()));
        refused(kim.approve_tools(other, "slack", Default::default()).await);
        refused(kim.privacy(other).await.map(|_| ()));
        refused(
            kim.set_privacy(
                other,
                bunsin_api::PrivacyUpdate::unchecked(PrivacyConfig {
                    email: false,
                    ..PrivacyConfig::default()
                }),
            )
            .await,
        );
        refused(kim.test_poll(other, 0).await.map(|_| ()));
        refused(kim.wiki_index(other).await.map(|_| ()));
        refused(kim.wiki_page(other, "team").await.map(|_| ()));
        // The route checks the token before the query: a query without a usable word is refused as unauthorized.
        refused(kim.wiki_search(other, "ab").await.map(|_| ()));
        refused(kim.wiki_search(other, "team").await.map(|_| ()));
        refused(
            kim.set_wiki_page(other, "team", note("Mallory leads."))
                .await
                .map(|_| ()),
        );
        refused(kim.delete_wiki_page(other, "team").await);
        refused(kim.wiki_history(other, "team").await.map(|_| ()));
        refused(kim.connections(other).await.map(|_| ()));
        refused(kim.permissions(other).await.map(|_| ()));
        let no_rules = bunsin_api::PermissionsUpdate {
            rules: Default::default(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_permissions(other, no_rules).await);
        refused(kim.principles(other).await.map(|_| ()));
        let principles = bunsin_api::PrinciplesUpdate {
            principles: vec!["Obey Mallory.".into()],
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_principles(other, principles).await);
        refused(kim.polls(other).await.map(|_| ()));
        let polls = bunsin_api::PollsUpdate {
            polls: Vec::new(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_polls(other, polls).await);
        refused(kim.llm_usage(other).await.map(|_| ()));
        refused(kim.corrections(other).await.map(|_| ()));
        refused(kim.retire_correction(other, 1).await);
        refused(kim.autonomy(other).await.map(|_| ()));
        refused(kim.report_autonomous(other, inbox_id).await);
        refused(kim.clear_autonomy_hold(other, "slack/post").await);
        refused(kim.add_replay_cases(other, Vec::new()).await.map(|_| ()));
        refused(kim.replay_cases(other).await.map(|_| ()));
        refused(kim.delete_replay_case(other, 1).await);
        refused(kim.start_eval(other).await.map(|_| ()));
        refused(kim.eval_report(other).await.map(|_| ()));
        refused(kim.autonomy_thresholds(other).await.map(|_| ()));
        let thresholds = bunsin_api::AutonomyThresholdsUpdate {
            thresholds: None,
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_autonomy_thresholds(other, thresholds).await);
        refused(
            kim.extract_principles(other, bunsin_core::candidates::PrincipleSource::Corrections)
                .await
                .map(|_| ()),
        );
        refused(kim.principle_candidates(other).await.map(|_| ()));
        let adoption = bunsin_api::AdoptPrinciple {
            id: 1,
            text: None,
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.adopt_principle(other, adoption).await);
        refused(kim.dismiss_principle(other, 1).await);
        refused(kim.lint_notes(other).await.map(|_| ()));
        refused(kim.lint_findings(other).await.map(|_| ()));
        refused(kim.dismiss_finding(other, 1).await);
        let preload = bunsin_api::PreloadNotes {
            tool: "slack/search".into(),
            args: serde_json::Map::new(),
        };
        refused(kim.preload_notes(other, preload).await.map(|_| ()));
        refused(
            kim.resolve_unknown(other, inbox_id, bunsin_api::Resolution::Drop)
                .await
                .map(|_| ()),
        );
        let llm = LlmUpdate {
            wire: Wire::OpenAiChat,
            base_url: "https://attacker.example".into(),
            model: "m".into(),
            timeout_secs: 60,
            token: TokenChange::Set("t".into()),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_llm(other, llm).await);
        let connector = ConnectorUpdate {
            url: "https://attacker.example/mcp".into(),
            token: TokenChange::Keep,
            token_header: None,
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_connector(other, "slack", connector).await);
        refused(kim.remove_connector(other, "slack").await);
        refused(kim.inbound_settings(other).await.map(|_| ()));
        let inbound = bunsin_api::InboundUpdate {
            inbound: Default::default(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_inbound_settings(other, inbound).await);
        refused(kim.ard_registries(other).await.map(|_| ()));
        let registries = bunsin_api::ArdRegistriesUpdate {
            registries: Vec::new(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_ard_registries(other, registries).await);
        refused(kim.webhooks(other).await.map(|_| ()));
        let webhooks = bunsin_api::WebhooksUpdate {
            webhooks: Default::default(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_webhooks(other, webhooks).await);
        refused(kim.notes_settings(other).await.map(|_| ()));
        let notes = bunsin_api::NotesSettingsUpdate {
            notes: Default::default(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_notes_settings(other, notes).await);
        refused(kim.eval_settings(other).await.map(|_| ()));
        let eval = bunsin_api::EvalSettingsUpdate {
            eval: Default::default(),
            expected: bunsin_api::Expected::Unchecked,
        };
        refused(kim.set_eval_settings(other, eval).await);
        refused(
            kim.replay(
                other,
                ReplayCase {
                    event: event("r"),
                    actual_reply: "x".into(),
                    expect: bunsin_core::eval::Expect::Reply,
                },
            )
            .await
            .map(|_| ()),
        );
    }
    // Nothing changed in lee's profile.
    assert_eq!(
        rig.engine.wiki_page("lee", "team").await.unwrap().text.body,
        "Lee leads."
    );
    assert_eq!(rig.engine.privacy("lee").await.unwrap(), PrivacyConfig::default());
    let lee = rig.engine.snapshot("lee").await.unwrap();
    assert!(!lee.kill_switch);
    assert_eq!(lee.inbox[0].state, ApprovalState::Pending);
    assert_eq!(rig.engine.profile_config("lee").await.unwrap().owner_name, "Lee");
    assert!(rig.slack.calls().is_empty());
}

#[tokio::test]
async fn unknown_and_revoked_tokens_are_refused() {
    let rig = rig().await;
    for token in ["bsn_guess", "x", &rig.kim_token[..rig.kim_token.len() - 1]] {
        let result = rig.client(token).snapshot("kim").await;
        assert!(matches!(result, Err(ApiError::Unauthorized(_))), "{result:?}");
        assert!(matches!(
            rig.client(token).list_profiles().await,
            Err(ApiError::Unauthorized(_))
        ));
    }
    let kim = rig.kim();
    assert!(kim.snapshot("kim").await.is_ok());
    let id = rig
        .engine
        .app_store()
        .list_tokens()
        .unwrap()
        .into_iter()
        .find(|t| t.profile == "kim")
        .unwrap()
        .id;
    rig.engine.app_store().revoke_token(&id).unwrap();
    // The same client, on an open connection, is refused on its next request.
    assert!(matches!(kim.snapshot("kim").await, Err(ApiError::Unauthorized(_))));
    assert!(rig.client(&rig.lee_token).snapshot("lee").await.is_ok());
}

#[tokio::test]
async fn a_request_without_a_bearer_token_is_refused() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rig = rig().await;
    for auth in ["", "Authorization: Basic a2ltOng=\r\n", "Authorization: bsn_x\r\n"] {
        let mut stream = tokio::net::TcpStream::connect(rig.server.addr).await.unwrap();
        let request = format!("GET /v1/profiles/kim/snapshot HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 401"), "{auth:?}: {reply}");
    }
}

#[tokio::test]
async fn settings_that_break_isolation_are_refused_and_not_stored() {
    let rig = rig().await;
    let kim = rig.kim();
    let before = kim.profile_config("kim").await.unwrap();

    // A stdio connector would run a command on the server.
    let mut stdio = before.clone();
    stdio.connectors.insert(
        "shell".into(),
        json!({"mcp": {"transport": "stdio", "command": "/bin/sh", "args": ["-c", "id"], "env": {}}}),
    );
    let error = kim.set_profile_config("kim", stdio).await.unwrap_err();
    assert!(
        matches!(error, ApiError::Invalid(ref m) if m.contains("--allow-stdio-connectors")),
        "{error:?}"
    );

    // A secret key cannot name another profile's secret or a server variable.
    for key in ["LEE__LLM", "BUNSIN_SECRET_LEE__LLM", "../HOME"] {
        let mut foreign = before.clone();
        foreign.llm =
            json!({"wire": "openai-chat", "base_url": "https://attacker.example", "model": "m", "token_secret": key});
        let error = kim.set_profile_config("kim", foreign).await.unwrap_err();
        assert!(matches!(error, ApiError::Invalid(_)), "{key}: {error:?}");
    }
    assert_eq!(kim.profile_config("kim").await.unwrap(), before);

    // The profile's own secret key is accepted.
    let mut own = before.clone();
    own.llm = json!({"wire": "openai-chat", "base_url": "https://llm.example", "model": "m", "token_secret": "LLM"});
    kim.set_profile_config("kim", own.clone()).await.unwrap();

    // An address inside the server's own network is refused, for the LLM and for a connector.
    let mut internal = own.clone();
    internal.llm["base_url"] = json!("https://169.254.169.254/latest");
    assert!(matches!(
        kim.set_profile_config("kim", internal).await,
        Err(ApiError::Invalid(_))
    ));
    let mut internal = own.clone();
    internal.connectors.insert(
        "jira".into(),
        json!({"mcp": {"transport": "http", "url": "http://localhost:9000"}}),
    );
    assert!(matches!(
        kim.set_profile_config("kim", internal).await,
        Err(ApiError::Invalid(_))
    ));
    assert_eq!(kim.profile_config("kim").await.unwrap(), own);
}

#[tokio::test]
async fn a_stopped_server_is_reported_as_unreachable() {
    let rig = rig().await;
    let kim = rig.kim();
    assert!(kim.snapshot("kim").await.is_ok());
    let Rig { server, kim_token, .. } = rig;
    server.shutdown().await.unwrap();
    let error = kim.snapshot("kim").await.unwrap_err();
    assert!(matches!(error, ApiError::Unreachable(_)), "{error:?}");
    assert!(!error.to_string().contains(&kim_token));
}

#[tokio::test]
async fn a_public_address_needs_tls() {
    let dir = tempfile::tempdir().unwrap();
    let backends = Arc::new(RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default()));
    let engine = Arc::new(Engine::open(dir.path(), backends, Arc::new(|| 1)).unwrap());
    let config = ServeConfig {
        listen: "0.0.0.0:0".parse().unwrap(),
        tls: None,
    };
    assert!(matches!(serve(engine, config).await, Err(ServeError::TlsRequired(_))));
}

#[tokio::test]
async fn tls_works_with_the_given_ca_and_fails_without_it() {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = (dir.path().join("cert.pem"), dir.path().join("key.pem"));
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

    let rig = rig_with(Some((cert_path, key_path))).await;
    assert!(rig.server.tls);
    let url = format!("https://localhost:{}", rig.server.addr.port());
    let trusted = RemoteConfig {
        url: url.clone(),
        ca_cert_pem: Some(cert.cert.pem().into_bytes()),
    };
    let kim = RemoteEngine::new(&trusted, &rig.kim_token).unwrap();
    assert_eq!(kim.snapshot("kim").await.unwrap().profile.id, "kim");

    // Without the CA the certificate is not trusted, so no request (and no token) is sent.
    let untrusted = RemoteEngine::new(&RemoteConfig { url, ca_cert_pem: None }, &rig.kim_token).unwrap();
    assert!(matches!(untrusted.snapshot("kim").await, Err(ApiError::Unreachable(_))));
    // Plain HTTP to the TLS port does not work either.
    let plain = format!("http://127.0.0.1:{}", rig.server.addr.port());
    let plain = RemoteEngine::new(
        &RemoteConfig {
            url: plain,
            ca_cert_pem: None,
        },
        &rig.kim_token,
    )
    .unwrap();
    assert!(plain.snapshot("kim").await.is_err());
}

#[tokio::test]
async fn privacy_settings_and_tool_approval_work_through_the_server() {
    let rig = rig().await;
    let kim = rig.kim();

    let mut privacy = kim.privacy("kim").await.unwrap();
    assert_eq!(privacy, PrivacyConfig::default());
    privacy.custom.push(CustomPattern {
        label: "employee id".into(),
        pattern: r"EMP-\d{6}".into(),
    });
    kim.set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(privacy.clone()))
        .await
        .unwrap();
    assert_eq!(kim.privacy("kim").await.unwrap(), privacy);
    // The server's reason for a refused pattern reaches the caller.
    privacy.custom.push(CustomPattern {
        label: "bad".into(),
        pattern: "(unclosed".into(),
    });
    let error = kim
        .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(privacy))
        .await
        .unwrap_err();
    assert!(
        matches!(error, ApiError::Invalid(ref m) if m.contains("the pattern of `bad`")),
        "{error:?}"
    );
    assert_eq!(kim.privacy("kim").await.unwrap().custom.len(), 1);

    rig.llm.reply(&post("ok"), 10);
    kim.ingest(
        "kim",
        InboundEvent {
            text: "EMP-004211 and kim@example.com".into(),
            ..event("p1")
        },
    )
    .await
    .unwrap();
    let request = serde_json::to_string(&rig.llm.requests()).unwrap();
    assert!(
        !request.contains("EMP-004211") && !request.contains("kim@example.com"),
        "{request}"
    );

    rig.slack
        .set_tools(vec![tool("post", "Post a message"), tool("search", "Search messages")]);
    let report = kim.connector_tools("kim", "slack").await.unwrap();
    assert_eq!(report.diff.added, ["search"]);
    kim.approve_tools("kim", "slack", bunsin_core::tool::pin(&report.tools))
        .await
        .unwrap();
    assert!(kim.connector_tools("kim", "slack").await.unwrap().diff.is_clean());
    assert!(matches!(
        kim.connector_tools("kim", "jira").await,
        Err(ApiError::NotFound(_))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_poll_task_finds_a_new_message_without_any_request() {
    use bunsin_core::poll::{PollConfig, PollMapping};
    use bunsin_daemon::server::Poller;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let rig = rig().await;
    rig.slack
        .set_tools(vec![tool("post", "Post a message"), tool("search", "Search messages")]);
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.approved_tools.insert(
        "slack".into(),
        vec![tool("post", "Post a message"), tool("search", "Search messages")],
    );
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    config.polls = vec![PollConfig {
        connector: "slack".into(),
        tool: "search".into(),
        args: Default::default(),
        every_secs: 30,
        mapping: PollMapping {
            items: "messages".into(),
            id: "ts".into(),
            text: "text".into(),
            author: "user".into(),
            thread: None,
        },
        skip_authors: vec![],
    }];
    rig.engine.set_profile_config("kim", config).await.unwrap();
    rig.slack.set_result(
        "search",
        json!({"messages": [{"ts": "1", "user": "lee", "text": "old"}]}),
    );

    let poller = Poller::start(rig.engine.clone());
    let wait_for = |what: &'static str, done: &dyn Fn() -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The first cycle records the old message as history.
    tokio::task::block_in_place(|| wait_for("the first search", &|| !rig.slack.calls().is_empty()));
    assert!(rig.llm.requests().is_empty());

    rig.slack.set_result(
        "search",
        json!({"messages": [{"ts": "1", "user": "lee", "text": "old"}, {"ts": "2", "user": "lee", "text": "new question"}]}),
    );
    rig.llm.reply(&post("Answer."), 10);
    rig.clock.fetch_add(30, Ordering::SeqCst);
    tokio::task::block_in_place(|| wait_for("the handled event", &|| !rig.llm.requests().is_empty()));

    poller.stop().await;
    let snapshot = rig.kim().snapshot("kim").await.unwrap();
    assert_eq!(snapshot.inbox.len(), 1);
    assert_eq!(snapshot.polls[0].tool, "slack/search");
    assert_eq!(snapshot.polls[0].new_events, 1);
    // The other profile has no poll and got no event.
    assert!(rig.engine.snapshot("lee").await.unwrap().inbox.is_empty());
}

#[tokio::test]
async fn connections_and_their_tokens_are_saved_through_the_server_and_never_come_back() {
    let rig = rig().await;
    let kim = rig.kim();
    let llm = LlmUpdate {
        wire: Wire::AnthropicMessages,
        base_url: "https://llm.example.com".into(),
        model: "m1".into(),
        timeout_secs: 90,
        token: TokenChange::Set("sk-over-the-wire".into()),
        expected: bunsin_api::Expected::Unchecked,
    };
    kim.set_llm("kim", llm).await.unwrap();
    let jira = ConnectorUpdate {
        url: "https://mcp.example.com/jira".into(),
        token: TokenChange::Set("jira-token".into()),
        token_header: None,
        expected: bunsin_api::Expected::Unchecked,
    };
    kim.set_connector("kim", "jira", jira).await.unwrap();

    let connections = kim.connections("kim").await.unwrap();
    let saved = connections.llm.as_ref().unwrap();
    assert_eq!((saved.timeout_secs, saved.token), (90, TokenSource::Stored));
    assert_eq!(connections.connectors["jira"].token, TokenSource::Stored);
    let answer = serde_json::to_string(&connections).unwrap()
        + &serde_json::to_string(&kim.profile_config("kim").await.unwrap()).unwrap();
    assert!(
        !answer.contains("sk-over-the-wire") && !answer.contains("jira-token"),
        "{answer}"
    );

    // The server's address rules apply to this path too, and its reason reaches the caller.
    let internal = ConnectorUpdate {
        url: "http://169.254.169.254/x".into(),
        token: TokenChange::Set("t".into()),
        token_header: None,
        expected: bunsin_api::Expected::Unchecked,
    };
    let error = kim.set_connector("kim", "meta", internal).await.unwrap_err();
    assert!(
        matches!(error, ApiError::Invalid(ref m) if m.contains("connector `meta`")),
        "{error:?}"
    );

    kim.remove_connector("kim", "jira").await.unwrap();
    assert!(kim.connections("kim").await.unwrap().connectors.is_empty());
    // The other profile sees nothing of this.
    assert!(rig.engine.connections("lee").await.unwrap().llm.is_none());
}

#[tokio::test]
async fn a_conflict_keeps_its_kind_across_the_network() {
    use bunsin_api::{Expected, PrivacyUpdate};
    let rig = rig().await;
    let kim = rig.kim();
    let loaded = kim.privacy("kim").await.unwrap();
    let other = PrivacyConfig {
        phone: false,
        ..loaded.clone()
    };
    kim.set_privacy("kim", PrivacyUpdate::unchecked(other.clone()))
        .await
        .unwrap();

    let mine = PrivacyConfig {
        email: false,
        ..loaded.clone()
    };
    let error = kim
        .set_privacy(
            "kim",
            PrivacyUpdate {
                privacy: mine,
                expected: Expected::Value(loaded),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    assert_eq!(kim.privacy("kim").await.unwrap(), other);
}

fn note(body: &str) -> bunsin_api::WikiPageUpdate {
    bunsin_api::WikiPageUpdate {
        text: bunsin_core::wiki::WikiText {
            title: "Team".into(),
            summary: "Who leads".into(),
            body: body.into(),
        },
        expected_revision: None,
    }
}

#[tokio::test]
async fn the_notes_are_read_and_written_through_the_server() {
    let rig = rig().await;
    let kim = rig.kim();
    assert!(kim.wiki_index("kim").await.unwrap().is_empty());
    let page = kim.set_wiki_page("kim", "team", note("Kim leads.")).await.unwrap();
    assert_eq!(page.revision, 1);
    assert_eq!(kim.wiki_page("kim", "team").await.unwrap(), page);
    assert_eq!(kim.wiki_index("kim").await.unwrap()[0].slug, "team");
    // The search keeps its result and its error kind across the network.
    assert_eq!(
        kim.wiki_search("kim", "Kim leads").await.unwrap(),
        kim.wiki_index("kim").await.unwrap()
    );
    assert!(kim.wiki_search("kim", "weather").await.unwrap().is_empty());
    assert!(matches!(kim.wiki_search("kim", "ab").await, Err(ApiError::Invalid(_))));

    // An edit from an old revision keeps its error kind across the network.
    let mut stale = note("Stale.");
    stale.expected_revision = Some(0);
    assert!(matches!(
        kim.set_wiki_page("kim", "team", stale).await,
        Err(ApiError::Conflict(_))
    ));
    assert!(matches!(
        kim.wiki_page("kim", "missing").await,
        Err(ApiError::NotFound(_))
    ));
    assert!(matches!(
        kim.set_wiki_page("kim", "Bad Slug", note("x")).await,
        Err(ApiError::Invalid(_))
    ));

    kim.delete_wiki_page("kim", "team").await.unwrap();
    assert!(kim.wiki_index("kim").await.unwrap().is_empty());
    let history = kim.wiki_history("kim", "team").await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].as_ref().unwrap().text.body, "Kim leads.");
    assert_eq!(history[1], None);
}

#[tokio::test]
async fn a_command_connector_goes_through_the_server_rules_of_its_own_profile() {
    use bunsin_api::{CommandConnectorUpdate, Expected};
    let rig = rig().await;
    let kim = rig.kim();
    let update = || CommandConnectorUpdate {
        command: "/opt/mcp/does-not-exist".into(),
        args: vec!["--stdio".into()],
        env: [("API_TOKEN".to_owned(), "env-secret-77".to_owned())].into(),
        secret_env: Default::default(),
        expected: Expected::Unchecked,
    };
    // This server was started without the flag. The refusal names the flag.
    let error = kim.set_command_connector("kim", "local", update()).await.unwrap_err();
    assert!(
        matches!(error, ApiError::Invalid(ref m) if m.contains("--allow-stdio-connectors")),
        "{error:?}"
    );
    assert!(!format!("{error:?}").contains("env-secret-77"));
    // A token changes only its own profile.
    let error = kim.set_command_connector("lee", "local", update()).await.unwrap_err();
    assert!(
        matches!(error, ApiError::Unauthorized(_) | ApiError::NotAllowed(_)),
        "{error:?}"
    );
    for profile in ["kim", "lee"] {
        assert!(rig.engine.connections(profile).await.unwrap().connectors.is_empty());
    }
}

#[tokio::test]
async fn the_permission_table_is_read_and_replaced_over_http() {
    let rig = rig().await;
    let lee = rig.client(&rig.lee_token);
    let loaded = lee.permissions("lee").await.unwrap();
    let config = lee.profile_config("lee").await.unwrap();
    assert_eq!(loaded.rules, config.policy.rules);
    assert_eq!(loaded.tools, config.approved_tools);

    let mut rules = loaded.rules.clone();
    rules.insert("slack/extra".into(), bunsin_core::policy::ToolRule::read());
    let update = |expected| bunsin_api::PermissionsUpdate {
        rules: rules.clone(),
        expected,
    };
    lee.set_permissions("lee", update(bunsin_api::Expected::Value(loaded.rules.clone())))
        .await
        .unwrap();
    assert_eq!(lee.permissions("lee").await.unwrap().rules, rules);
    // A save that started from the earlier table is a conflict for the client, not a plain failure.
    let stale = lee
        .set_permissions("lee", update(bunsin_api::Expected::Value(loaded.rules)))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
}

#[tokio::test]
async fn the_principles_are_read_and_replaced_over_http() {
    let rig = rig().await;
    let lee = rig.client(&rig.lee_token);
    let loaded = lee.principles("lee").await.unwrap();
    assert_eq!(loaded, lee.profile_config("lee").await.unwrap().principles);
    let update = |principles: &[&str], expected| bunsin_api::PrinciplesUpdate {
        principles: principles.iter().map(|p| (*p).to_owned()).collect(),
        expected,
    };
    lee.set_principles(
        "lee",
        update(&["Answer briefly."], bunsin_api::Expected::Value(loaded.clone())),
    )
    .await
    .unwrap();
    assert_eq!(lee.principles("lee").await.unwrap(), ["Answer briefly."]);
    let stale = lee
        .set_principles("lee", update(&["Other."], bunsin_api::Expected::Value(loaded)))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    let empty = lee
        .set_principles("lee", update(&[""], bunsin_api::Expected::Unchecked))
        .await;
    assert!(matches!(empty, Err(ApiError::Invalid(_))), "{empty:?}");
    // The other profile is untouched.
    assert!(rig.engine.principles("kim").await.unwrap().is_empty());
}

#[tokio::test]
async fn the_polls_are_read_and_replaced_over_http() {
    let rig = rig().await;
    let lee = rig.client(&rig.lee_token);
    let loaded = lee.polls("lee").await.unwrap();
    assert!(loaded.is_empty());
    let poll: bunsin_core::poll::PollConfig = serde_json::from_value(serde_json::json!({
        "connector": "slack", "tool": "post", "args": {}, "every_secs": 60,
        "mapping": {"items": "", "id": "ts", "text": "text", "author": "user", "thread": null},
        "skip_authors": []
    }))
    .unwrap();
    // `post` writes, so the server refuses it as a poll and stores nothing.
    let update = |expected| bunsin_api::PollsUpdate {
        polls: vec![poll.clone()],
        expected,
    };
    let refused = lee
        .set_polls("lee", update(bunsin_api::Expected::Value(loaded.clone())))
        .await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(e)) if e.contains("read")),
        "{refused:?}"
    );
    // An empty list is a valid change; a save from the list before it is a conflict.
    let other = vec![poll.clone()];
    let stale = lee.set_polls("lee", update(bunsin_api::Expected::Value(other))).await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    let empty = bunsin_api::PollsUpdate {
        polls: Vec::new(),
        expected: bunsin_api::Expected::Value(loaded),
    };
    lee.set_polls("lee", empty).await.unwrap();
    assert!(lee.polls("lee").await.unwrap().is_empty());
}

#[tokio::test]
async fn the_llm_call_log_is_read_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    rig.llm.reply(&post("Deployed."), 12);
    kim.ingest("kim", event("e1")).await.unwrap();
    let usage = kim.llm_usage("kim").await.unwrap();
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(
        (
            usage[0].purpose.as_str(),
            usage[0].calls,
            usage[0].failed,
            usage[0].input_tokens
        ),
        ("event", 1, 0, 12)
    );
    // The other profile made no call.
    assert!(rig.client(&rig.lee_token).llm_usage("lee").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_start_marks_cut_calls_unknown_and_runs_waiting_approved_calls_once() {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let slack = Arc::new(FakeConnector::new("slack", vec![tool("post", "Post a message")]));
    let open = || {
        let backends = Arc::new(Fakes {
            llm: llm.clone(),
            slack: slack.clone(),
            real: RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default()),
        });
        Arc::new(Engine::open(dir.path(), backends, Arc::new(|| 1_000_000)).unwrap())
    };
    let engine = open();
    engine.create_profile("kim", profile_config("Kim")).await.unwrap();
    let mut ids = Vec::new();
    for id in ["cut", "waiting"] {
        llm.reply(&post(id), 10);
        let Outcome::PendingApproval { inbox_id } = engine.ingest("kim", event(id)).await.unwrap() else {
            panic!("expected a pending item")
        };
        ids.push(inbox_id);
    }
    // The stop cut one call while it ran, and came before the other one started.
    let store = engine.app_store().open_profile("kim").unwrap();
    for (id, to) in [(ids[0], ApprovalState::Executing), (ids[1], ApprovalState::Approved)] {
        store
            .move_inbox_state(id, ApprovalState::Pending, ApprovalState::Approved, 1)
            .unwrap();
        if to == ApprovalState::Executing {
            store.move_inbox_state(id, ApprovalState::Approved, to, 1).unwrap();
        }
    }
    drop(engine);

    let engine = open();
    let listen = "127.0.0.1:0".parse().unwrap();
    let started = start(engine.clone(), ServeConfig { listen, tls: None }, Vec::new())
        .await
        .unwrap();
    let states: Vec<_> = engine
        .snapshot("kim")
        .await
        .unwrap()
        .inbox
        .into_iter()
        .map(|item| (item.id, item.state))
        .collect();
    assert_eq!(
        states,
        [(ids[1], ApprovalState::Executed), (ids[0], ApprovalState::Unknown)]
    );
    let calls = slack.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].1["text"].as_str().unwrap().starts_with("waiting"));
    started.stop().await.unwrap();
}

#[tokio::test]
async fn an_unknown_item_is_resolved_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    rig.llm.reply(&post("Deployed."), 10);
    let Outcome::PendingApproval { inbox_id } = kim.ingest("kim", event("e1")).await.unwrap() else {
        panic!("expected a pending item")
    };
    // A stop cut the call while it ran. The next start finds it.
    let store = rig.engine.app_store().open_profile("kim").unwrap();
    store
        .move_inbox_state(inbox_id, ApprovalState::Pending, ApprovalState::Approved, 1)
        .unwrap();
    store
        .move_inbox_state(inbox_id, ApprovalState::Approved, ApprovalState::Executing, 1)
        .unwrap();
    assert!(rig.engine.mark_interrupted().failures.is_empty());

    let lee = rig.client(&rig.lee_token);
    let refused = lee
        .resolve_unknown("kim", inbox_id, bunsin_api::Resolution::Retry)
        .await;
    assert!(matches!(refused, Err(ApiError::Unauthorized(_))), "{refused:?}");
    let outcome = kim
        .resolve_unknown("kim", inbox_id, bunsin_api::Resolution::Retry)
        .await
        .unwrap();
    let Some(Outcome::Executed { inbox_id: copy }) = outcome else {
        panic!("expected an executed copy, got {outcome:?}")
    };
    assert_ne!(copy, inbox_id);
    assert_eq!(rig.slack.calls().len(), 1);
    let again = kim.resolve_unknown("kim", inbox_id, bunsin_api::Resolution::Done).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
}

#[tokio::test]
async fn corrections_are_listed_and_retired_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    rig.llm.reply(&post("A long draft."), 10);
    let Outcome::PendingApproval { inbox_id } = kim.ingest("kim", event("e1")).await.unwrap() else {
        panic!("expected a pending item")
    };
    let edit = Edit {
        args: json!({"channel": "C1", "text": "Short."}).as_object().unwrap().clone(),
        reason: "shorter".into(),
    };
    kim.approve("kim", inbox_id, Some(edit)).await.unwrap();

    let records = kim.corrections("kim").await.unwrap();
    assert_eq!(records.len(), 1);
    let c = &records[0].correction;
    assert_eq!(
        (
            c.original.as_str(),
            c.edited.as_str(),
            c.reason.as_str(),
            c.task.as_str()
        ),
        ("A long draft.", "Short.", "shorter", "hi")
    );
    // Another profile neither sees nor retires it.
    let lee = rig.client(&rig.lee_token);
    assert!(lee.corrections("lee").await.unwrap().is_empty());
    let refused = lee.retire_correction("kim", records[0].id).await;
    assert!(matches!(refused, Err(ApiError::Unauthorized(_))), "{refused:?}");

    kim.retire_correction("kim", records[0].id).await.unwrap();
    assert!(kim.corrections("kim").await.unwrap()[0].retired_at.is_some());
    let again = kim.retire_correction("kim", records[0].id).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
}

#[tokio::test]
async fn an_autonomous_call_is_reported_and_its_hold_cleared_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    let mut rules = kim.permissions("kim").await.unwrap().rules;
    rules.get_mut("slack/post").unwrap().tier = bunsin_core::policy::Tier::Autonomous;
    let update = bunsin_api::PermissionsUpdate {
        rules,
        expected: bunsin_api::Expected::Unchecked,
    };
    kim.set_permissions("kim", update).await.unwrap();
    rig.llm.reply(&post("Deployed."), 10);
    let Outcome::Executed { inbox_id } = kim.ingest("kim", event("e1")).await.unwrap() else {
        panic!("expected an autonomous run")
    };

    // Another profile's token neither reports nor clears.
    let lee = rig.client(&rig.lee_token);
    let refused = lee.report_autonomous("kim", inbox_id).await;
    assert!(matches!(refused, Err(ApiError::Unauthorized(_))), "{refused:?}");
    kim.report_autonomous("kim", inbox_id).await.unwrap();
    let autonomy = kim.autonomy("kim").await.unwrap();
    assert_eq!(autonomy.tools.len(), 1);
    assert!(autonomy.tools[0].held);
    assert!(lee.autonomy("lee").await.unwrap().tools.iter().all(|tool| !tool.held));
    rig.llm.reply(&post("Again."), 10);
    let outcome = kim.ingest("kim", event("e2")).await.unwrap();
    assert!(matches!(outcome, Outcome::PendingApproval { .. }), "{outcome:?}");

    let refused = lee.clear_autonomy_hold("kim", "slack/post").await;
    assert!(matches!(refused, Err(ApiError::Unauthorized(_))), "{refused:?}");
    kim.clear_autonomy_hold("kim", "slack/post").await.unwrap();
    assert!(!kim.autonomy("kim").await.unwrap().tools[0].held);
    let again = kim.clear_autonomy_hold("kim", "slack/post").await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
    rig.llm.reply(&post("Once more."), 10);
    let outcome = kim.ingest("kim", event("e3")).await.unwrap();
    assert!(matches!(outcome, Outcome::Executed { .. }), "{outcome:?}");
}

#[tokio::test]
async fn an_evaluation_runs_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    let case = ReplayCase {
        event: event("old-1"),
        actual_reply: "We deploy at 3 pm.".into(),
        expect: bunsin_core::eval::Expect::Reply,
    };
    let ids = kim.add_replay_cases("kim", vec![case]).await.unwrap();
    assert_eq!(kim.replay_cases("kim").await.unwrap()[0].id, ids[0]);
    rig.llm.reply(&post("We deploy at 3 pm."), 10);
    let run = kim.start_eval("kim").await.unwrap();
    let again = kim.start_eval("kim").await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
    // The server's poll task replays the cases. This test has none, so it ticks here.
    assert!(rig.engine.background_tick().await.is_empty());
    assert!(rig.engine.background_tick().await.is_empty());
    let report = kim.eval_report("kim").await.unwrap();
    assert_eq!(
        report.run.as_ref().map(|r| (r.id, r.state)),
        Some((run, bunsin_core::eval::RunState::Done))
    );
    assert_eq!(report.summary.kind_percent, Some(100));
    kim.delete_replay_case("kim", ids[0]).await.unwrap();
    let gone = kim.delete_replay_case("kim", ids[0]).await;
    assert!(matches!(gone, Err(ApiError::NotFound(_))), "{gone:?}");
}

#[tokio::test]
async fn a_stale_adoption_is_a_conflict_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    let store = rig.engine.app_store().open_profile("kim").unwrap();
    let first = store
        .add_principle_candidate("Ask first.", "ask me", "corrections", 5)
        .unwrap();
    let second = store
        .add_principle_candidate("Be brief.", "short", "corrections", 5)
        .unwrap();
    let listed = kim.principle_candidates("kim").await.unwrap();
    assert_eq!(listed.iter().map(|c| c.id).collect::<Vec<_>>(), [second, first]);

    let loaded = kim.principles("kim").await.unwrap();
    let adopt = |id| bunsin_api::AdoptPrinciple {
        id,
        text: None,
        expected: bunsin_api::Expected::Value(loaded.clone()),
    };
    kim.adopt_principle("kim", adopt(first)).await.unwrap();
    // The second adoption started from the list before the first one.
    let stale = kim.adopt_principle("kim", adopt(second)).await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    assert_eq!(bunsin_api::http::status_of(&stale.unwrap_err()), 412);
    assert_eq!(kim.principles("kim").await.unwrap(), ["Ask first."]);
    kim.dismiss_principle("kim", second).await.unwrap();
    let again = kim.dismiss_principle("kim", second).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
}

#[tokio::test]
async fn the_thresholds_are_set_over_http_with_a_conflict_check() {
    let rig = rig().await;
    let kim = rig.kim();
    let thresholds = bunsin_core::autonomy::AutonomyThresholds {
        min_reviewed: 5,
        min_unchanged_percent: 80,
        window_days: 14,
    };
    let update = |expected| bunsin_api::AutonomyThresholdsUpdate {
        thresholds: Some(thresholds),
        expected: bunsin_api::Expected::Value(expected),
    };
    kim.set_autonomy_thresholds("kim", update(None)).await.unwrap();
    assert_eq!(kim.autonomy_thresholds("kim").await.unwrap(), Some(thresholds));
    let stale = kim.set_autonomy_thresholds("kim", update(None)).await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
}

#[tokio::test]
async fn the_notes_are_checked_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    let off = kim.lint_notes("kim").await;
    assert!(matches!(off, Err(ApiError::NotAllowed(_))), "{off:?}");
    let mut config = kim.profile_config("kim").await.unwrap();
    config.notes.stale_days = Some(1);
    kim.set_profile_config("kim", config).await.unwrap();
    let text = bunsin_core::wiki::WikiText {
        title: "Deploy".into(),
        summary: "When".into(),
        body: "Fridays.".into(),
    };
    let update = bunsin_api::WikiPageUpdate {
        text,
        expected_revision: Some(0),
    };
    kim.set_wiki_page("kim", "deploy", update).await.unwrap();
    rig.clock.fetch_add(2 * 86_400, std::sync::atomic::Ordering::SeqCst);
    let found = kim.lint_notes("kim").await.unwrap();
    assert_eq!(found.len(), 2);
    assert_eq!(kim.lint_findings("kim").await.unwrap(), found);
    kim.dismiss_finding("kim", found[0].id).await.unwrap();
    assert_eq!(kim.lint_findings("kim").await.unwrap(), found[1..]);
    let gone = kim.dismiss_finding("kim", 999).await;
    assert!(matches!(gone, Err(ApiError::NotFound(_))), "{gone:?}");
}

#[tokio::test]
async fn an_event_the_owner_hands_in_counts_as_the_owners_own() {
    let rig = rig().await;
    let kim = rig.kim();
    let tools = vec![tool("post", "Post a message"), tool("search", "Search messages")];
    rig.slack.set_tools(tools.clone());
    let mut config = kim.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    config.notes.auto_apply = true;
    kim.set_profile_config("kim", config).await.unwrap();
    let search = serde_json::json!({"kind": "tool_call", "rationale": "look", "summary": null,
        "tool": "slack/search", "args_json": "{\"query\":\"deploy\"}"});
    let keep = serde_json::json!({"kind": "ignore", "rationale": "noted", "summary": null, "tool": null,
        "args_json": null, "wiki_updates": [{"slug": "deploy", "title": "Deploy", "summary": "When",
        "body": "Fridays."}]});
    rig.llm.reply(&search.to_string(), 10).reply(&keep.to_string(), 10);
    // The client sends the event over HTTP. The server marks it as the owner's from the access token.
    kim.ingest("kim", event("owner-1")).await.unwrap();
    let page = kim.wiki_page("kim", "deploy").await.unwrap();
    assert_eq!(page.text.body, "Fridays.");
}

#[tokio::test]
async fn the_notes_are_preloaded_over_http() {
    let rig = rig().await;
    let kim = rig.kim();
    let tools = vec![tool("post", "Post a message"), tool("search", "Search messages")];
    rig.slack.set_tools(tools.clone());
    rig.slack
        .set_result("search", json!({"messages": [{"text": "We release on Thursdays."}]}));
    let mut config = kim.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    kim.set_profile_config("kim", config).await.unwrap();
    let keep = json!({"kind": "ignore", "rationale": "noted", "summary": null, "tool": null, "args_json": null,
        "wiki_updates": [{"slug": "releases", "title": "Releases", "summary": "When", "body": "Thursdays."}]});
    rig.llm.reply(&keep.to_string(), 10);
    let request = bunsin_api::PreloadNotes {
        tool: "slack/search".into(),
        args: json!({"query": "release"}).as_object().unwrap().clone(),
    };
    let filed = kim.preload_notes("kim", request).await.unwrap();
    assert_eq!(filed.len(), 1);
    let snapshot = kim.snapshot("kim").await.unwrap();
    let item = snapshot.inbox.iter().find(|item| item.id == filed[0]).unwrap();
    assert_eq!(item.state, ApprovalState::Pending);
    let post = bunsin_api::PreloadNotes {
        tool: "slack/post".into(),
        args: serde_json::Map::new(),
    };
    let refused = kim.preload_notes("kim", post).await;
    assert!(matches!(refused, Err(ApiError::NotAllowed(_))), "{refused:?}");
}

fn outer(reach: Reach, listen: &str, url: &str) -> OuterConfig {
    OuterConfig {
        reach,
        listen: listen.parse().unwrap(),
        url: url.parse().unwrap(),
        tls: None,
    }
}

#[test]
fn listeners_are_checked_before_the_start() {
    let control = ServeConfig {
        listen: "127.0.0.1:8787".parse().unwrap(),
        tls: None,
    };
    let internal = outer(Reach::Internal, "127.0.0.1:8788", "https://agents.corp.example");
    let public = outer(Reach::Public, "127.0.0.1:8789", "https://agents.example.com/");
    assert!(check_listeners(&control, &[internal.clone(), public.clone()]).is_ok());
    // A proxy on this machine may serve the listener under plain http.
    assert!(
        check_listeners(
            &control,
            &[outer(Reach::Internal, "127.0.0.1:0", "http://localhost:9000")]
        )
        .is_ok()
    );

    let refused = |outer: &[OuterConfig]| check_listeners(&control, outer).unwrap_err().to_string();
    let open = outer(Reach::Public, "0.0.0.0:8789", "https://agents.example.com");
    assert!(refused(&[open]).contains("--public-tls-cert"));
    for url in [
        "http://agents.example.com",
        "https://agents.example.com/?x=1",
        "https://user:pw@agents.example.com",
        "ftp://agents.example.com",
    ] {
        let error = refused(&[outer(Reach::Internal, "127.0.0.1:8788", url)]);
        assert!(error.contains("--internal-url cannot be used"), "{url}: {error}");
    }
    let same = outer(Reach::Internal, "127.0.0.1:8787", "https://agents.corp.example");
    assert!(refused(&[same]).contains("cannot share the address 127.0.0.1:8787"));
    assert!(refused(&[internal.clone(), internal]).contains("more than one internal listener"));
    // Port 0 gets a free port from the system, so two such addresses do not clash.
    let any = |reach| outer(reach, "127.0.0.1:0", "http://127.0.0.1");
    let control_any = ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: None,
    };
    assert!(check_listeners(&control_any, &[any(Reach::Internal), any(Reach::Public)]).is_ok());
}

#[tokio::test]
async fn an_outer_listener_has_no_control_api() {
    let rig = rig().await;
    let internal = bunsin_daemon::server::serve_outer(
        rig.engine.clone(),
        outer(Reach::Internal, "127.0.0.1:0", "http://127.0.0.1"),
        Arc::new(tokio::sync::Notify::new()),
    )
    .await
    .unwrap();
    // The same access token works on the control API and finds nothing on the outer listener.
    assert_eq!(rig.kim().list_profiles().await.unwrap().len(), 1);
    let on_outer = RemoteEngine::new(
        &RemoteConfig {
            url: internal.url(),
            ca_cert_pem: None,
        },
        &rig.kim_token,
    )
    .unwrap();
    let refused = on_outer.list_profiles().await;
    assert!(
        matches!(&refused, Err(ApiError::Unreachable(text)) if text.contains("HTTP 404")),
        "{refused:?}"
    );
    let refused = on_outer.snapshot("kim").await;
    assert!(matches!(&refused, Err(ApiError::Unreachable(text)) if text.contains("HTTP 404")));
    internal.shutdown().await.unwrap();
}

#[tokio::test]
async fn an_owner_manages_the_caller_tokens_of_the_own_profile_only() {
    let rig = rig().await;
    let kim = rig.kim();
    let lee = rig.client(&rig.lee_token);
    let created = kim.add_caller_token("kim", "  Lee's bot ").await.unwrap();
    assert!(created.token.starts_with("bsc_"));
    assert!(!format!("{created:?}").contains(&created.token));
    let listed = kim.caller_tokens("kim").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!((listed[0].id, listed[0].label.as_str()), (created.id, "Lee's bot"));
    assert_eq!(listed[0].revoked_at, None);
    for bad in ["", "two\nlines", &"x".repeat(61)] {
        let refused = kim.add_caller_token("kim", bad).await;
        assert!(matches!(refused, Err(ApiError::Invalid(_))), "{bad:?}: {refused:?}");
    }

    // Another owner can neither see nor revoke them.
    assert!(matches!(lee.caller_tokens("kim").await, Err(ApiError::Unauthorized(_))));
    assert!(matches!(
        lee.revoke_caller_token("kim", created.id).await,
        Err(ApiError::Unauthorized(_))
    ));
    let refused = lee.revoke_caller_token("lee", created.id).await;
    assert!(matches!(refused, Err(ApiError::NotFound(_))), "{refused:?}");
    assert!(lee.caller_tokens("lee").await.unwrap().is_empty());

    // A caller token opens nothing of the control API.
    let as_caller = rig.client(&created.token);
    assert!(matches!(
        as_caller.list_profiles().await,
        Err(ApiError::Unauthorized(_))
    ));
    assert!(matches!(
        as_caller.snapshot("kim").await,
        Err(ApiError::Unauthorized(_))
    ));

    kim.revoke_caller_token("kim", created.id).await.unwrap();
    assert!(kim.caller_tokens("kim").await.unwrap()[0].revoked_at.is_some());
    assert!(matches!(
        kim.revoke_caller_token("kim", created.id).await,
        Err(ApiError::NotFound(_))
    ));
    let audit: Vec<String> = kim
        .snapshot("kim")
        .await
        .unwrap()
        .audit
        .into_iter()
        .map(|entry| entry.kind)
        .collect();
    assert!(audit.contains(&"caller_token_added".to_owned()) && audit.contains(&"caller_token_revoked".to_owned()));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_intake_task_hands_a_stored_question_to_the_agent_and_stops_with_the_server() {
    use bunsin_daemon::server::Poller;
    use std::time::{Duration, Instant};
    let rig = rig().await;
    let mut config = rig.kim().profile_config("kim").await.unwrap();
    config.inbound.enabled = true;
    config.inbound.description = "Questions about the deploy schedule.".into();
    rig.kim().set_profile_config("kim", config).await.unwrap();
    let (_, token) = rig.engine.app_store().add_caller_token("kim", "bot", 1).unwrap();
    let caller = rig.engine.app_store().caller(&token).unwrap().unwrap();
    let answer = json!({"kind": "tool_call", "rationale": "asked", "summary": null,
        "tool": "bunsin.inbound/reply", "args_json": json!({"text": "Friday."}).to_string()});
    rig.llm.reply(&answer.to_string(), 10);

    let wake = Arc::new(tokio::sync::Notify::new());
    let poller = Poller::start_with(rig.engine.clone(), wake.clone());
    let asked = rig.engine.ask(&caller, "When do we deploy?").unwrap();
    wake.notify_one();
    let deadline = Instant::now() + Duration::from_secs(20);
    while rig.engine.request(&caller, &asked.request_id).unwrap().status == RequestStatus::Working {
        assert!(Instant::now() < deadline, "the question was not handled");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        rig.engine.request(&caller, &asked.request_id).unwrap().status,
        RequestStatus::PendingApproval
    );
    // A stop ends both tasks. A question after the stop waits for the next start.
    poller.stop().await;
    let later = rig.engine.ask(&caller, "And next week?").unwrap();
    wake.notify_one();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        rig.engine.request(&caller, &later.request_id).unwrap().status,
        RequestStatus::Working
    );
}

/// Kim's endpoint is on for the internal listener. Lee's is off.
async fn inbound_rig() -> (Rig, Running, Running) {
    let rig = rig().await;
    let mut config = rig.kim().profile_config("kim").await.unwrap();
    config.inbound.enabled = true;
    config.inbound.description = "Questions about the deploy schedule.".into();
    config.inbound.title = Some("Kim's agent".into());
    rig.kim().set_profile_config("kim", config).await.unwrap();
    let wake = Arc::new(tokio::sync::Notify::new());
    let internal = bunsin_daemon::server::serve_outer(
        rig.engine.clone(),
        outer(Reach::Internal, "127.0.0.1:0", "http://127.0.0.1"),
        wake.clone(),
    )
    .await
    .unwrap();
    let public = bunsin_daemon::server::serve_outer(
        rig.engine.clone(),
        outer(Reach::Public, "127.0.0.1:0", "http://localhost"),
        wake,
    )
    .await
    .unwrap();
    (rig, internal, public)
}

fn caller_token(rig: &Rig, profile: &str) -> String {
    rig.engine.app_store().add_caller_token(profile, "bot", 1).unwrap().1
}

async fn mcp(listener: &Running, profile: &str, token: &str) -> Result<McpConnector, ConnectorError> {
    let url = format!("{}/agents/{profile}/mcp", listener.url());
    McpConnector::connect("inbound", McpConfig::Http { url }, Some(TokenAuth::bearer(token))).await
}

/// One raw MCP request, to see the HTTP status.
async fn raw(listener: &Running, profile: &str, token: &str, host: Option<&str>, body: String) -> reqwest::StatusCode {
    let mut request = reqwest::Client::new()
        .post(format!("{}/agents/{profile}/mcp", listener.url()))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .body(body);
    if let Some(host) = host {
        request = request.header("host", host);
    }
    request.send().await.unwrap().status()
}

async fn read(client: &McpConnector, id: &str) -> Result<serde_json::Value, ConnectorError> {
    client.call_tool("get_request", args(json!({"request_id": id}))).await
}

fn initialize() -> String {
    json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18",
        "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}})
    .to_string()
}

fn args(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
    value.as_object().unwrap().clone()
}

#[tokio::test]
async fn a_caller_asks_over_mcp_and_reads_the_answer_after_the_owner_approves_it() {
    let (rig, internal, _public) = inbound_rig().await;
    let token = caller_token(&rig, "kim");
    let client = mcp(&internal, "kim", &token).await.unwrap();
    let names: Vec<String> = client.list_tools().await.unwrap().into_iter().map(|t| t.name).collect();
    assert_eq!(names, ["ask", "get_request"]);

    let asked = client
        .call_tool("ask", args(json!({"question": "When do we deploy?"})))
        .await
        .unwrap();
    assert_eq!(asked["pending_approval"], true);
    let id = asked["request_id"].as_str().unwrap().to_owned();
    assert_eq!(read(&client, &id).await.unwrap()["status"], "working");

    rig.llm.reply(
        &json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "bunsin.inbound/reply",
            "args_json": json!({"text": "Friday."}).to_string()})
        .to_string(),
        10,
    );
    assert!(rig.engine.drain_intake().await.is_empty());
    let state = read(&client, &id).await.unwrap();
    assert_eq!(state["status"], "pending_approval");
    assert!(state.get("answer").is_none());
    let inbox_id = rig.kim().snapshot("kim").await.unwrap().inbox[0].id;
    rig.kim().approve("kim", inbox_id, None).await.unwrap();
    let state = read(&client, &id).await.unwrap();
    assert_eq!(state["status"], "answered");
    let answer = state["answer"].as_str().unwrap();
    assert!(
        answer.starts_with("Friday.") && answer.contains("— AI agent"),
        "{answer}"
    );

    // The checks of the tools: a long question, another caller's request, and too many reads.
    let long = "x".repeat(4_001);
    let refused = client.call_tool("ask", args(json!({"question": long}))).await;
    assert!(matches!(refused, Err(ConnectorError::Failed(_))), "{refused:?}");
    let other = mcp(&internal, "kim", &caller_token(&rig, "kim")).await.unwrap();
    let refused = read(&other, &id).await;
    assert!(
        matches!(&refused, Err(ConnectorError::Failed(text)) if text.contains("not found")),
        "{refused:?}"
    );
    for _ in 0..117 {
        read(&client, &id).await.unwrap();
    }
    let refused = read(&client, &id).await;
    assert!(
        matches!(&refused, Err(ConnectorError::Failed(text)) if text.contains("a minute")),
        "{refused:?}"
    );
    // The limit is per caller.
    assert!(
        read(&other, &id)
            .await
            .is_err_and(|e| !e.to_string().contains("a minute"))
    );
}

#[tokio::test]
async fn the_inbound_endpoint_takes_only_a_caller_token_of_its_profile_on_its_listener() {
    let (rig, internal, public) = inbound_rig().await;
    let kim = caller_token(&rig, "kim");
    let lee = caller_token(&rig, "lee");
    let status = |listener, profile, token, host| raw(listener, profile, token, host, initialize());
    // The control request on the same rig succeeds.
    assert_eq!(status(&internal, "kim", &kim, None).await, reqwest::StatusCode::OK);
    mcp(&internal, "kim", &kim).await.unwrap();
    for (listener, profile, token) in [
        // Another profile's caller token, and a caller token on another profile's path.
        (&internal, "kim", lee.as_str()),
        (&internal, "lee", kim.as_str()),
        // An access token, a guess, and a profile that does not exist.
        (&internal, "kim", rig.kim_token.as_str()),
        (&internal, "kim", "bsc_guess"),
        (&internal, "nobody", kim.as_str()),
        // Kim's scope is internal, so the public listener does not serve kim.
        (&public, "kim", kim.as_str()),
    ] {
        assert_eq!(
            status(listener, profile, token, None).await,
            reqwest::StatusCode::UNAUTHORIZED,
            "{profile} {token}"
        );
    }
    assert!(mcp(&internal, "kim", &rig.kim_token).await.is_err());
    // A Host other than the listener's address is refused, also with a valid token.
    assert_eq!(
        status(&internal, "kim", &kim, Some("evil.example")).await,
        reqwest::StatusCode::FORBIDDEN
    );
    // A body over 64 KiB is refused.
    let big = json!({"jsonrpc": "2.0", "id": 1, "method": "ping", "params": {"pad": "x".repeat(70_000)}});
    assert_eq!(
        raw(&internal, "kim", &kim, None, big.to_string()).await,
        reqwest::StatusCode::PAYLOAD_TOO_LARGE
    );
    // A revoked token is refused from the next request on.
    let id = rig.kim().caller_tokens("kim").await.unwrap()[0].id;
    rig.kim().revoke_caller_token("kim", id).await.unwrap();
    assert_eq!(
        status(&internal, "kim", &kim, None).await,
        reqwest::StatusCode::UNAUTHORIZED
    );
    // The control API takes no caller token, and the outer listener has no control API.
    assert!(matches!(
        rig.client(&lee).snapshot("lee").await,
        Err(ApiError::Unauthorized(_))
    ));

    // Public scope: now both listeners serve kim.
    let mut config = rig.kim().profile_config("kim").await.unwrap();
    config.inbound.scope = bunsin_core::inbound::InboundScope::Public;
    rig.kim().set_profile_config("kim", config).await.unwrap();
    let fresh = caller_token(&rig, "kim");
    assert_eq!(
        status(&public, "kim", &fresh, Some("localhost")).await,
        reqwest::StatusCode::OK
    );
    // The public listener accepts only its own host.
    assert_eq!(
        status(&public, "kim", &fresh, None).await,
        reqwest::StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn the_server_card_describes_only_a_served_endpoint() {
    let (rig, internal, public) = inbound_rig().await;
    let get = |listener: &Running, profile: &str| {
        reqwest::Client::new()
            .get(format!("{}/agents/{profile}/mcp/server-card", listener.url()))
            .send()
    };
    let response = get(&internal, "kim").await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let headers = response.headers().clone();
    assert_eq!(headers["content-type"], "application/mcp-server-card+json");
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(headers["cache-control"], "public, max-age=300");
    assert!(headers["etag"].to_str().unwrap().starts_with('"'));
    let card: serde_json::Value = response.json().await.unwrap();
    assert_eq!(
        card["$schema"],
        "https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json"
    );
    assert_eq!(card["name"], "1.0.0.127/kim");
    assert_eq!(card["description"], "Questions about the deploy schedule.");
    assert_eq!(card["title"], "Kim's agent");
    assert_eq!(card["remotes"][0]["type"], "streamable-http");
    assert_eq!(card["remotes"][0]["url"], "http://127.0.0.1/agents/kim/mcp");
    assert_eq!(card["remotes"][0]["headers"][0]["isSecret"], true);
    // No owner name and no token.
    let text = card.to_string();
    assert!(!text.contains("acting for Kim") && !text.contains("owner_name"));
    assert!(!text.contains("bsc_") && !text.contains("bsn_"));
    // Lee's endpoint is off, kim's is not on the public listener, and nobody's exists.
    for (listener, profile) in [(&internal, "lee"), (&public, "kim"), (&internal, "nobody")] {
        assert_eq!(
            get(listener, profile).await.unwrap().status(),
            reqwest::StatusCode::NOT_FOUND
        );
    }
    // Turning the endpoint off removes the card at once.
    let mut config = rig.kim().profile_config("kim").await.unwrap();
    config.inbound.enabled = false;
    rig.kim().set_profile_config("kim", config).await.unwrap();
    assert_eq!(
        get(&internal, "kim").await.unwrap().status(),
        reqwest::StatusCode::NOT_FOUND
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_started_server_hands_a_question_from_its_listener_to_the_agent_at_once() {
    use std::time::{Duration, Instant};
    let rig = rig().await;
    let mut config = rig.kim().profile_config("kim").await.unwrap();
    config.inbound.enabled = true;
    config.inbound.description = "Questions about the deploy schedule.".into();
    rig.kim().set_profile_config("kim", config).await.unwrap();
    rig.llm.reply(&ignore_reply(), 10);
    let control = ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: None,
    };
    let internal = outer(Reach::Internal, "127.0.0.1:0", "http://127.0.0.1");
    let started = start(rig.engine.clone(), control, vec![internal]).await.unwrap();
    let client = mcp(&started.outer[0], "kim", &caller_token(&rig, "kim")).await.unwrap();
    let asked = client
        .call_tool("ask", args(json!({"question": "Hello?"})))
        .await
        .unwrap();
    let id = asked["request_id"].as_str().unwrap().to_owned();
    // The question wakes the intake task; the 5 s tick is not needed.
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        let state = client
            .call_tool("get_request", args(json!({"request_id": id})))
            .await
            .unwrap();
        if state["status"] == "declined" {
            break;
        }
        assert!(Instant::now() < deadline, "the question was not handled: {state}");
        // Slow enough to stay under the read limit of a caller.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    started.stop().await.unwrap();
}

fn ignore_reply() -> String {
    json!({"kind": "ignore", "rationale": "a greeting", "summary": null, "tool": null, "args_json": null}).to_string()
}

/// Turns the endpoint of `profile` on with `scope` and lists it under acme.com with `queries`.
async fn publish(rig: &Rig, profile: &str, token: &str, scope: InboundScope, queries: &[&str]) {
    let client = rig.client(token);
    let mut config = client.profile_config(profile).await.unwrap();
    config.inbound.enabled = true;
    config.inbound.scope = scope;
    config.inbound.description = format!("Questions for {profile}.");
    config.inbound.ard = Some(ArdListing {
        publisher: "acme.com".into(),
        namespace: Some("team".into()),
        agent_name: profile.into(),
        representative_queries: queries.iter().map(|q| (*q).to_owned()).collect(),
    });
    client.set_profile_config(profile, config).await.unwrap();
}

async fn catalog(listener: &Running) -> (reqwest::header::HeaderMap, serde_json::Value) {
    let response = reqwest::get(format!("{}/.well-known/ard.json", listener.url()))
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    (response.headers().clone(), response.json().await.unwrap())
}

fn identifiers(catalog: &serde_json::Value) -> Vec<&str> {
    catalog["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["identifier"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn each_listener_lists_only_the_endpoints_it_serves_and_each_entry_names_its_own_card() {
    let (rig, internal, public) = inbound_rig().await;
    publish(
        &rig,
        "kim",
        &rig.kim_token,
        InboundScope::Internal,
        &["When do we deploy?", "Who reviews?"],
    )
    .await;
    publish(
        &rig,
        "lee",
        &rig.lee_token,
        InboundScope::Public,
        &["Is the build green?", "Who is on call?"],
    )
    .await;

    let (headers, inner) = catalog(&internal).await;
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(headers["access-control-allow-origin"], "*");
    assert_eq!(
        identifiers(&inner),
        ["urn:air:acme.com:team:kim", "urn:air:acme.com:team:lee"]
    );
    let (_, outer) = catalog(&public).await;
    assert_eq!(identifiers(&outer), ["urn:air:acme.com:team:lee"]);
    let entry = &outer["entries"][0];
    assert_eq!(entry["type"], "application/mcp-server-card+json");
    assert_eq!(entry["url"], "http://localhost/agents/lee/mcp/server-card");
    assert_eq!(entry["description"], "Questions for lee.");
    assert_eq!(
        inner["entries"][0]["url"],
        "http://127.0.0.1/agents/kim/mcp/server-card"
    );
    // The catalog holds no owner name and no token.
    let text = format!("{inner}{outer}");
    assert!(!text.contains("acting for") && !text.contains("bsc_"), "{text}");

    // The invariant: every entry of a listener names a card that the same listener serves.
    for (listener, listed) in [(&internal, &inner), (&public, &outer)] {
        for entry in listed["entries"].as_array().unwrap() {
            let url: url::Url = entry["url"].as_str().unwrap().parse().unwrap();
            let card = reqwest::get(format!("{}{}", listener.url(), url.path())).await.unwrap();
            assert_eq!(card.status(), reqwest::StatusCode::OK, "{url}");
        }
    }

    // An endpoint that is turned off leaves every catalog at once.
    let mut config = rig.client(&rig.lee_token).profile_config("lee").await.unwrap();
    config.inbound.enabled = false;
    config.inbound.ard = None;
    rig.client(&rig.lee_token)
        .set_profile_config("lee", config)
        .await
        .unwrap();
    assert_eq!(identifiers(&catalog(&internal).await.1), ["urn:air:acme.com:team:kim"]);
    assert!(identifiers(&catalog(&public).await.1).is_empty());
}

#[tokio::test]
async fn the_owner_sees_the_addresses_the_listing_and_why_a_catalog_leaves_it_out() {
    let (rig, _internal, _public) = inbound_rig().await;
    // One query is fewer than ARD asks for. The entry is listed with a warning.
    publish(
        &rig,
        "lee",
        &rig.lee_token,
        InboundScope::Public,
        &["Is the build green?"],
    )
    .await;
    let status = rig.client(&rig.lee_token).inbound_status("lee").await.unwrap();
    assert!(status.enabled);
    let urls: Vec<(&str, &str)> = status
        .endpoints
        .iter()
        .map(|e| (e.mcp_url.as_str(), e.catalog_url.as_str()))
        .collect();
    assert_eq!(
        urls,
        [
            (
                "http://127.0.0.1/agents/lee/mcp",
                "http://127.0.0.1/.well-known/ard.json"
            ),
            (
                "http://localhost/agents/lee/mcp",
                "http://localhost/.well-known/ard.json"
            ),
        ]
    );
    let ard = status.ard.unwrap();
    assert_eq!(ard.identifier.as_deref(), Some("urn:air:acme.com:team:lee"));
    assert_eq!(ard.listed_on, [InboundScope::Internal, InboundScope::Public]);
    assert_eq!(ard.refused, Vec::<String>::new());
    assert_eq!(ard.warnings.len(), 1);
    assert!(
        ard.warnings[0].contains("1 representative queries"),
        "{:?}",
        ard.warnings
    );

    // Settings stored by the operator before a domain was taken away, and a second profile with the
    // same identifier. The catalogs leave them out, and the owner reads why.
    let mut other = profile_config("Park");
    other.inbound.enabled = true;
    other.inbound.description = "Questions for park.".into();
    other.inbound.ard = Some(ArdListing {
        publisher: "other.example".into(),
        agent_name: "park".into(),
        ..ArdListing::default()
    });
    rig.engine.create_profile("park", other.clone()).await.unwrap();
    let mut twin = other;
    twin.inbound.scope = InboundScope::Public;
    twin.inbound.ard = Some(ArdListing {
        publisher: "acme.com".into(),
        namespace: Some("team".into()),
        agent_name: "lee".into(),
        representative_queries: vec!["a?".into(), "b?".into()],
    });
    rig.engine.create_profile("twin", twin).await.unwrap();
    let park = rig.engine.inbound_status("park").await.unwrap().ard.unwrap();
    assert!(park.listed_on.is_empty());
    assert!(
        park.refused
            .iter()
            .any(|r| r.contains("does not allow the publisher domain `other.example`")),
        "{:?}",
        park.refused
    );
    let lee = rig
        .client(&rig.lee_token)
        .inbound_status("lee")
        .await
        .unwrap()
        .ard
        .unwrap();
    assert!(lee.listed_on.is_empty());
    assert_eq!(
        lee.refused,
        ["more than one profile uses the identifier `urn:air:acme.com:team:lee` (§4.2)"]
    );

    // Kim's endpoint is internal and has no listing.
    let status = rig.kim().inbound_status("kim").await.unwrap();
    assert_eq!(status.ard, None);
    assert_eq!(status.endpoints.len(), 1, "kim is internal only");
}

#[tokio::test]
async fn a_listing_that_breaks_an_ard_rule_or_the_operator_list_is_not_stored() {
    let rig = rig().await;
    let base = rig.kim().profile_config("kim").await.unwrap();
    let listing = |publisher: &str, agent: &str| {
        let mut config = base.clone();
        config.inbound.enabled = true;
        config.inbound.description = "Questions.".into();
        config.inbound.ard = Some(ArdListing {
            publisher: publisher.into(),
            agent_name: agent.into(),
            ..ArdListing::default()
        });
        config
    };
    for (config, why) in [
        (listing("other.example", "kim"), "--ard-publisher-domain other.example"),
        (listing("acme.com", "Kim Lee"), "makes no valid ARD entry"),
    ] {
        let error = rig.kim().set_profile_config("kim", config).await.unwrap_err();
        assert!(matches!(&error, ApiError::Invalid(m) if m.contains(why)), "{error:?}");
    }
    let mut off = listing("acme.com", "kim");
    off.inbound.enabled = false;
    let error = rig.kim().set_profile_config("kim", off).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("inbound.ard")),
        "{error:?}"
    );
    assert_eq!(rig.kim().profile_config("kim").await.unwrap().inbound.ard, None);
    // The operator's list holds domains only.
    assert!(
        RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default())
            .with_ard_publishers(&["https://acme.com".into()])
            .is_err()
    );
}

#[tokio::test]
async fn single_parts_of_the_settings_are_read_and_replaced_over_http_with_a_conflict_check() {
    let rig = rig().await;
    let kim = rig.kim();
    let before = kim.profile_config("kim").await.unwrap();

    // The inbound endpoint and its listing. Turning it on adds the reply rule.
    let loaded = kim.inbound_settings("kim").await.unwrap();
    assert_eq!(loaded, InboundConfig::default());
    let inbound = InboundConfig {
        enabled: true,
        description: "Questions about deploys.".into(),
        ard: Some(ArdListing {
            publisher: "acme.com".into(),
            agent_name: "kim".into(),
            ..ArdListing::default()
        }),
        ..InboundConfig::default()
    };
    let update = |inbound: InboundConfig, expected| bunsin_api::InboundUpdate {
        inbound,
        expected: bunsin_api::Expected::Value(expected),
    };
    kim.set_inbound_settings("kim", update(inbound.clone(), loaded.clone()))
        .await
        .unwrap();
    assert_eq!(kim.inbound_settings("kim").await.unwrap(), inbound);
    let rules = kim.permissions("kim").await.unwrap().rules;
    assert!(rules.contains_key(bunsin_core::inbound::REPLY_TOOL));
    let stale = kim
        .set_inbound_settings("kim", update(InboundConfig::default(), loaded))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    // A publisher domain that the operator does not allow is refused, as in the settings file.
    let mut other = inbound.clone();
    other.ard.as_mut().unwrap().publisher = "other.example".into();
    let refused = kim.set_inbound_settings("kim", update(other, inbound.clone())).await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(m)) if m.contains("--ard-publisher-domain other.example")),
        "{refused:?}"
    );

    // The notes and the evaluation. A value that the settings file refuses is refused here too.
    let notes = bunsin_core::wiki::NotesConfig {
        auto_apply: true,
        stale_days: Some(30),
        lint_every_hours: Some(24),
        lint_conflicts: false,
    };
    let notes_update = |notes, expected| bunsin_api::NotesSettingsUpdate {
        notes,
        expected: bunsin_api::Expected::Value(expected),
    };
    kim.set_notes_settings("kim", notes_update(notes.clone(), Default::default()))
        .await
        .unwrap();
    assert_eq!(kim.notes_settings("kim").await.unwrap(), notes);
    let zero = bunsin_core::wiki::NotesConfig {
        stale_days: Some(0),
        ..notes.clone()
    };
    let refused = kim.set_notes_settings("kim", notes_update(zero, notes.clone())).await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(m)) if m.contains("stale_days")),
        "{refused:?}"
    );
    let eval = bunsin_core::eval::EvalConfig {
        pass: Some(bunsin_core::eval::EvalPass {
            kind_percent: 80,
            overlap_percent: 40,
            boundary_percent: 100,
        }),
        judge: true,
    };
    let eval_update = |eval, expected| bunsin_api::EvalSettingsUpdate {
        eval,
        expected: bunsin_api::Expected::Value(expected),
    };
    kim.set_eval_settings("kim", eval_update(eval.clone(), Default::default()))
        .await
        .unwrap();
    assert_eq!(kim.eval_settings("kim").await.unwrap(), eval);
    assert_eq!(kim.eval_report("kim").await.unwrap().pass, eval.pass);
    let stale = kim
        .set_eval_settings("kim", eval_update(Default::default(), Default::default()))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");

    // The other parts of the settings are as they were.
    let after = kim.profile_config("kim").await.unwrap();
    assert_eq!(
        (&after.principles, &after.polls, &after.privacy, &after.connectors),
        (&before.principles, &before.polls, &before.privacy, &before.connectors)
    );
    let kinds: Vec<String> = kim
        .snapshot("kim")
        .await
        .unwrap()
        .audit
        .into_iter()
        .map(|a| a.kind)
        .filter(|kind| kind.ends_with("settings_changed"))
        .collect();
    assert_eq!(
        kinds.len(),
        3,
        "one entry per stored save, none for a refused one: {kinds:?}"
    );
}
