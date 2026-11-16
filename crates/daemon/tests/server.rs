//! The HTTP server and the remote client together, against a real engine with fakes.

use bunsin_api::{
    ApiError, ConnectorUpdate, Edit, EngineApi, LlmUpdate, Outcome, ProfileConfig, ReplayCase, TokenChange, TokenSource,
};
use bunsin_client::{RemoteConfig, RemoteEngine};
use bunsin_core::approval::ApprovalState;
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use bunsin_core::settings::Wire;
use bunsin_daemon::server::{Running, ServeConfig, ServeError, serve, start};
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
    let real = RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default());
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
    let (running, poller) = start(engine.clone(), ServeConfig { listen, tls: None }).await.unwrap();
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
    poller.stop().await;
    running.shutdown().await.unwrap();
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
