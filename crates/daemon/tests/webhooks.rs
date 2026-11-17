//! Webhooks of Slack and Jira Cloud over real TCP: signature checks, a fast answer, the later handling
//! through the intake, repeated deliveries, and the settings rules.

use bunsin_api::{ApiConnectorUpdate, ApiError, EngineApi, Expected, ProfileConfig, TokenChange};
use bunsin_core::approval::ApprovalState;
use bunsin_core::inbound::InboundScope;
use bunsin_core::policy::ToolRule;
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_core::records::InboxBody;
use bunsin_core::settings::{ApiAuthMethod, ApiConfig, ApiService};
use bunsin_core::webhook::WebhookConfig;
use bunsin_daemon::server::{OuterConfig, Reach, Running, serve_outer};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Backends, Engine, ProfileSecrets, Runtime};
use bunsin_testkit::{FakeConnector, FakeLlm, tool};
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;
use std::path::Path;
use std::sync::Arc;

const SLACK_SECRET: &str = "slack-signing-secret-81f2";
const JIRA_SECRET: &str = "jira-webhook-secret-5c07";
/// The engine's clock: 2001-09-09.
const NOW: i64 = 1_000_000_000;

/// The real backends, with a scripted LLM and a fake `slack` connector that has a search.
struct Fakes {
    real: RealBackends,
    llm: Arc<FakeLlm>,
    slack: Arc<FakeConnector>,
}

impl Fakes {
    fn with_fakes(&self, mut runtime: Runtime) -> Runtime {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        let slack: Arc<dyn Connector> = self.slack.clone();
        runtime.llm = Ok(llm);
        runtime.connectors.insert("slack".into(), Ok(slack));
        runtime
    }
}

impl Backends for Fakes {
    fn build<'a>(&'a self, profile: &'a str, config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move { Ok(self.with_fakes(self.real.build(profile, config).await?)) })
    }

    fn build_with_secrets<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
        secrets: &'a ProfileSecrets,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move { Ok(self.with_fakes(self.real.build_with_secrets(profile, config, secrets).await?)) })
    }

    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        self.real.validate(profile, config)
    }
}

struct Rig {
    dir: tempfile::TempDir,
    llm: Arc<FakeLlm>,
    slack: Arc<FakeConnector>,
    engine: Arc<Engine>,
    internal: Running,
    public: Running,
}

fn open(dir: &Path, llm: Arc<FakeLlm>, slack: Arc<FakeConnector>) -> Arc<Engine> {
    let backends = Arc::new(Fakes {
        real: RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default()),
        llm,
        slack,
    });
    Arc::new(Engine::open(dir, backends, Arc::new(|| NOW)).unwrap())
}

async fn listen(engine: &Arc<Engine>) -> (Running, Running) {
    let outer = |reach, url: &str| OuterConfig {
        reach,
        listen: "127.0.0.1:0".parse().unwrap(),
        url: url.parse().unwrap(),
        tls: None,
    };
    let wake = Arc::new(tokio::sync::Notify::new());
    let internal = serve_outer(engine.clone(), outer(Reach::Internal, "http://127.0.0.1"), wake.clone())
        .await
        .unwrap();
    let public = serve_outer(engine.clone(), outer(Reach::Public, "http://localhost"), wake)
        .await
        .unwrap();
    (internal, public)
}

fn api(service: ApiService, base_url: &str, auth: ApiAuthMethod, email: Option<&str>) -> ApiConnectorUpdate {
    ApiConnectorUpdate {
        api: ApiConfig {
            service,
            base_url: base_url.into(),
            auth,
            email: email.map(str::to_owned),
        },
        token: TokenChange::Set("api-token-1".into()),
        expected: Expected::Unchecked,
    }
}

fn hook(connector: &str, scope: InboundScope) -> WebhookConfig {
    WebhookConfig {
        connector: connector.into(),
        scope,
        skip_authors: vec!["U-KIM".into()],
    }
}

/// Kim has REST API connectors `slack` and `jira`, the public webhook `slack-events` and the internal
/// webhook `jira-events`, each with its secret. The agent may search Slack.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let slack = Arc::new(FakeConnector::new("slack", vec![tool("search", "Search messages")]));
    let engine = open(dir.path(), llm.clone(), slack.clone());
    engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
    engine.create_profile("lee", ProfileConfig::new("Lee")).await.unwrap();
    let slack_api = api(ApiService::Slack, "https://slack.com/api", ApiAuthMethod::Bearer, None);
    engine.set_api_connector("kim", "slack", slack_api).await.unwrap();
    let jira_api = api(
        ApiService::Jira,
        "https://acme.atlassian.net",
        ApiAuthMethod::Basic,
        Some("kim@acme.example"),
    );
    engine.set_api_connector("kim", "jira", jira_api).await.unwrap();
    let mut config = engine.profile_config("kim").await.unwrap();
    config
        .approved_tools
        .insert("slack".into(), vec![tool("search", "Search messages")]);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    config
        .webhooks
        .insert("slack-events".into(), hook("slack", InboundScope::Public));
    config
        .webhooks
        .insert("jira-events".into(), hook("jira", InboundScope::Internal));
    engine.set_profile_config("kim", config).await.unwrap();
    engine
        .set_webhook_secret("kim", "slack-events", SLACK_SECRET.into())
        .await
        .unwrap();
    engine
        .set_webhook_secret("kim", "jira-events", JIRA_SECRET.into())
        .await
        .unwrap();
    let (internal, public) = listen(&engine).await;
    Rig {
        dir,
        llm,
        slack,
        engine,
        internal,
        public,
    }
}

fn mac(secret: &str, data: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(data);
    hex::encode(mac.finalize().into_bytes())
}

fn slack_body(event_id: &str, user: &str, text: &str) -> String {
    json!({"type": "event_callback", "event_id": event_id,
        "event": {"type": "message", "channel": "C1", "ts": "1700000000.000100", "user": user, "text": text}})
    .to_string()
}

/// Sends `body` to `/hooks/{profile}/{name}` with a Slack signature made with `secret` at `timestamp`.
async fn slack(listener: &Running, path: &str, body: &str, secret: &str, timestamp: i64) -> reqwest::Response {
    let signature = format!("v0={}", mac(secret, format!("v0:{timestamp}:{body}").as_bytes()));
    reqwest::Client::new()
        .post(format!("{}{path}", listener.url()))
        .header("x-slack-request-timestamp", timestamp.to_string())
        .header("x-slack-signature", signature)
        .header("content-type", "application/json")
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
}

fn jira_body(comment_id: &str, sent_secs: i64) -> String {
    json!({"webhookEvent": "comment_created", "timestamp": sent_secs * 1_000,
        "issue": {"id": "10001", "key": "PROJ-7"},
        "comment": {"id": comment_id, "updated": "2001-09-09T01:46:40.000+0000",
            "author": {"accountId": "acc-lee"}, "body": "Is the deploy on Friday?"}})
    .to_string()
}

async fn jira(listener: &Running, body: &str, secret: &str, delivery: &str) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}/hooks/kim/jira-events", listener.url()))
        .header("x-hub-signature", format!("sha256={}", mac(secret, body.as_bytes())))
        .header("x-atlassian-webhook-identifier", delivery)
        .body(body.to_owned())
        .send()
        .await
        .unwrap()
}

fn ignore() -> String {
    json!({"kind": "ignore", "rationale": "nothing to do", "summary": null, "tool": null, "args_json": null})
        .to_string()
}

const SLACK_HOOK: &str = "/hooks/kim/slack-events";

#[tokio::test]
async fn a_slack_delivery_gets_200_at_once_and_reaches_the_agent_later_once() {
    let rig = rig().await;
    let body = slack_body("Ev1", "U-LEE", "When do we deploy?");
    let response = slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    // The answer came before any LLM call: the event waits in the intake.
    assert!(rig.llm.requests().is_empty());

    // A retry of the same delivery, and the same message sent again as `app_mention`, both get 200.
    assert_eq!(
        slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await.status(),
        reqwest::StatusCode::OK
    );
    let mention = slack_body("Ev2", "U-LEE", "When do we deploy?").replace("\"message\"", "\"app_mention\"");
    assert_eq!(
        slack(&rig.public, SLACK_HOOK, &mention, SLACK_SECRET, NOW)
            .await
            .status(),
        reqwest::StatusCode::OK
    );

    rig.llm.reply(&ignore(), 10).reply(&ignore(), 10);
    assert!(rig.engine.drain_intake().await.is_empty());
    // The agent handled the message once: the event key of a message is its channel and `ts`.
    let requests = rig.llm.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(requests[0].messages[0].content.contains("When do we deploy?"));
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert_eq!(audit.iter().filter(|a| a.kind == "webhook_received").count(), 2);
    assert_eq!(audit.iter().filter(|a| a.kind == "event_received").count(), 1);
}

#[tokio::test]
async fn every_refusal_is_the_same_401_and_a_correct_delivery_passes() {
    let rig = rig().await;
    let body = slack_body("Ev1", "U-LEE", "Hello");
    let refused = [
        (
            "wrong secret",
            slack(&rig.public, SLACK_HOOK, &body, "another-secret", NOW).await,
        ),
        (
            "stale timestamp",
            slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW - 301).await,
        ),
        (
            "unknown webhook",
            slack(&rig.public, "/hooks/kim/nothing", &body, SLACK_SECRET, NOW).await,
        ),
        (
            "another profile",
            slack(&rig.public, "/hooks/lee/slack-events", &body, SLACK_SECRET, NOW).await,
        ),
        // The Jira webhook is internal, so the public listener does not take it.
        (
            "not on this listener",
            jira(&rig.public, &jira_body("1", NOW), JIRA_SECRET, "d1").await,
        ),
        (
            "old Jira body",
            jira(&rig.internal, &jira_body("2", NOW - 2 * 3_600 - 1), JIRA_SECRET, "d2").await,
        ),
        (
            "Jira wrong secret",
            jira(&rig.internal, &jira_body("3", NOW), SLACK_SECRET, "d3").await,
        ),
    ];
    let mut texts = Vec::new();
    for (case, response) in refused {
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED, "{case}");
        texts.push(response.text().await.unwrap());
    }
    texts.dedup();
    assert_eq!(texts.len(), 1, "{texts:?}");
    let unsigned = reqwest::Client::new()
        .post(format!("{}{SLACK_HOOK}", rig.public.url()))
        .body(body.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(unsigned.status(), reqwest::StatusCode::UNAUTHORIZED);
    // Nothing of the refused deliveries was stored.
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert!(!audit.iter().any(|a| a.kind == "webhook_received"));

    // The controls: a timestamp within 5 minutes, a Jira body within 2 hours, on the right listeners.
    assert_eq!(
        slack(&rig.internal, SLACK_HOOK, &body, SLACK_SECRET, NOW - 299)
            .await
            .status(),
        reqwest::StatusCode::OK
    );
    let fresh = jira_body("4", NOW - 2 * 3_600 + 60);
    assert_eq!(
        jira(&rig.internal, &fresh, JIRA_SECRET, "d4").await.status(),
        reqwest::StatusCode::OK
    );
    // A repeated Jira delivery has the same identifier and is stored once.
    assert_eq!(
        jira(&rig.internal, &fresh, JIRA_SECRET, "d4").await.status(),
        reqwest::StatusCode::OK
    );
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert_eq!(audit.iter().filter(|a| a.kind == "webhook_received").count(), 2);
}

#[tokio::test]
async fn slack_url_verification_answers_the_challenge_only_when_signed() {
    let rig = rig().await;
    let body = json!({"type": "url_verification", "challenge": "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"})
        .to_string();
    let response = slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await;
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/plain");
    assert_eq!(
        response.text().await.unwrap(),
        "3eZbrw1aBm2rZgRNFdxV2595E9CY3gmdALWMmHkvFXO7tYXAYM8P"
    );
    let response = slack(&rig.public, SLACK_HOOK, &body, "wrong", NOW).await;
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    assert!(!response.text().await.unwrap().contains("3eZbrw1a"));
}

#[tokio::test]
async fn the_agents_own_posts_are_not_stored_and_a_large_body_is_refused() {
    let rig = rig().await;
    for (id, user, text) in [
        ("Ev1", "U-KIM", "my own message"),
        ("Ev2", "U-BOT", "Friday.\n— AI agent (Bunsin) acting for Kim"),
    ] {
        let body = slack_body(id, user, text);
        assert_eq!(
            slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await.status(),
            reqwest::StatusCode::OK
        );
    }
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert!(!audit.iter().any(|a| a.kind == "webhook_received"));

    let large = slack_body("Ev3", "U-LEE", &"x".repeat(1024 * 1024));
    let response = slack(&rig.public, SLACK_HOOK, &large, SLACK_SECRET, NOW).await;
    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_stored_delivery_is_handled_after_a_restart() {
    let rig = rig().await;
    let body = slack_body("Ev1", "U-LEE", "Still there after the restart?");
    assert_eq!(
        slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await.status(),
        reqwest::StatusCode::OK
    );
    let Rig {
        dir,
        llm,
        slack,
        engine,
        internal,
        public,
    } = rig;
    internal.shutdown().await.unwrap();
    public.shutdown().await.unwrap();
    drop(engine);
    let engine = open(dir.path(), llm.clone(), slack);
    llm.reply(&ignore(), 10);
    assert!(engine.drain_intake().await.is_empty());
    let requests = llm.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].messages[0]
            .content
            .contains("Still there after the restart?")
    );
}

#[tokio::test]
async fn a_page_change_from_a_webhook_event_waits_for_the_owner() {
    let rig = rig().await;
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.notes.auto_apply = true;
    rig.engine.set_profile_config("kim", config).await.unwrap();
    rig.slack
        .set_result("search", json!({"messages": [{"text": "We deploy on Fridays."}]}));
    rig.llm.reply(
        &json!({"kind": "tool_call", "rationale": "check", "summary": null, "tool": "slack/search",
            "args_json": json!({"query": "deploy"}).to_string()})
        .to_string(),
        10,
    );
    let mut reply: Value = serde_json::from_str(&ignore()).unwrap();
    reply["wiki_updates"] = json!([{"slug": "deploys", "title": "Deploys", "summary": "When we deploy",
        "body": "The team deploys on Fridays."}]);
    rig.llm.reply(&reply.to_string(), 10);
    let body = slack_body("Ev1", "U-LEE", "When do we deploy?");
    assert_eq!(
        slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await.status(),
        reqwest::StatusCode::OK
    );
    assert!(rig.engine.drain_intake().await.is_empty());
    // Anyone who writes in Slack can steer a webhook event, so `auto_apply` does not store its change.
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let change = inbox
        .iter()
        .find(|item| matches!(&item.body, InboxBody::WikiChange { slug, .. } if slug == "deploys"))
        .expect("a page change");
    assert_eq!(change.state, ApprovalState::Pending);
    assert!(rig.engine.wiki_page("kim", "deploys").await.is_err());
}

#[tokio::test]
async fn the_settings_refuse_a_webhook_that_cannot_work_and_its_secret_goes_with_it() {
    let rig = rig().await;
    let base = rig.engine.profile_config("kim").await.unwrap();
    let refused = |mut config: ProfileConfig, change: fn(&mut ProfileConfig)| {
        change(&mut config);
        config
    };
    // A poll on the connector of a webhook.
    let poll = refused(base.clone(), |config| {
        config.polls.push(
            serde_json::from_value(
                json!({"connector": "slack", "tool": "search", "args": {}, "every_secs": 60,
                "mapping": {"items": "messages", "id": "ts", "text": "text", "author": "user"}, "skip_authors": []}),
            )
            .unwrap(),
        );
    });
    // A webhook of a connector that does not exist, and a name that is no name.
    let missing = refused(base.clone(), |config| {
        config
            .webhooks
            .insert("x".into(), hook("nothing", InboundScope::Internal));
    });
    let bad_name = refused(base.clone(), |config| {
        config
            .webhooks
            .insert("Bad Name".into(), hook("slack", InboundScope::Internal));
    });
    for (config, why) in [
        (poll, "has a poll and webhook"),
        (missing, "needs a REST API connector of Slack or Jira"),
        (bad_name, "webhook name"),
    ] {
        let error = rig.engine.set_profile_config("kim", config).await.unwrap_err();
        assert!(matches!(&error, ApiError::Invalid(m) if m.contains(why)), "{error:?}");
    }
    let error = rig
        .engine
        .set_webhook_secret("kim", "nothing", "s".into())
        .await
        .unwrap_err();
    assert!(matches!(error, ApiError::NotFound(_)), "{error:?}");

    // The owner sees the address to give Slack, and that the secret is stored.
    let status = rig.engine.inbound_status("kim").await.unwrap();
    let shown = status.webhooks.iter().find(|h| h.name == "slack-events").unwrap();
    assert_eq!(
        shown.urls,
        [
            "http://127.0.0.1/hooks/kim/slack-events",
            "http://localhost/hooks/kim/slack-events"
        ]
    );
    assert!(shown.secret_stored);
    // A removed webhook leaves no secret behind: added again, it has none and refuses deliveries.
    let mut config = base.clone();
    config.webhooks.remove("slack-events");
    rig.engine.set_profile_config("kim", config).await.unwrap();
    rig.engine.set_profile_config("kim", base).await.unwrap();
    let status = rig.engine.inbound_status("kim").await.unwrap();
    assert!(
        !status
            .webhooks
            .iter()
            .find(|h| h.name == "slack-events")
            .unwrap()
            .secret_stored
    );
    let body = slack_body("Ev1", "U-LEE", "Hello");
    assert_eq!(
        slack(&rig.public, SLACK_HOOK, &body, SLACK_SECRET, NOW).await.status(),
        reqwest::StatusCode::UNAUTHORIZED
    );
}
