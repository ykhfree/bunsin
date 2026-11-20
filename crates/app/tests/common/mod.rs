//! A real engine with a scripted LLM and a recording connector.

pub mod window;

use bunsin_api::{EngineApi, ProfileConfig};
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_core::settings::origin_of;
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Backends, Engine, OAuthSignIn, Runtime};
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
    /// `slack` always works. Every other connector in the settings cannot connect.
    fn build<'a>(&'a self, _profile: &'a str, config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        let slack: Arc<dyn Connector> = self.slack.clone();
        let mut connectors: BTreeMap<_, _> = config
            .connectors
            .keys()
            .map(|name| {
                (
                    name.clone(),
                    Err(format!("connector `{name}` cannot connect: {REFUSED}")),
                )
            })
            .collect();
        connectors.insert("slack".to_owned(), Ok(slack));
        Box::pin(async move {
            Ok(Runtime {
                llm: Ok(llm),
                connectors,
            })
        })
    }

    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        self.real.validate(profile, config)
    }

    fn ard_publishers(&self) -> Vec<String> {
        self.real.ard_publishers()
    }

    /// The authorization server grants at once: its page sends the browser back with `state=s1`.
    fn oauth_start<'a>(
        &'a self,
        _profile: &'a str,
        _name: &'a str,
        _section: &'a serde_json::Value,
        redirect_uri: &'a str,
    ) -> BoxFuture<'a, Result<String, String>> {
        let address = format!("{AUTHORIZE}?state=s1&redirect_uri={redirect_uri}");
        Box::pin(async move { Ok(address) })
    }

    /// Gives tokens for the MCP server at `OAUTH_URL`, unless the browser came back with an error.
    fn oauth_finish<'a>(
        &'a self,
        _profile: &'a str,
        _name: &'a str,
        callback_url: &'a str,
    ) -> BoxFuture<'a, Result<OAuthSignIn, String>> {
        let result = if callback_url.contains("error=") {
            Err("the authorization server did not give access: access_denied".to_owned())
        } else {
            let credentials = json!({
                "client_id": "c1",
                "token_response": {"access_token": OAUTH_ACCESS, "token_type": "bearer", "refresh_token": "r1"},
            });
            Ok(OAuthSignIn {
                credentials: credentials.to_string(),
                origin: origin_of(OAUTH_URL).unwrap(),
            })
        };
        Box::pin(async move { result })
    }
}

/// The address of the MCP server of the OAuth connector in the tests.
#[allow(dead_code)]
pub const OAUTH_URL: &str = "https://mcp.example/docs";
/// The page where the owner signs in.
#[allow(dead_code)]
pub const AUTHORIZE: &str = "https://auth.example/authorize";
/// The access token of a finished sign-in. The window must never show it.
#[allow(dead_code)]
pub const OAUTH_ACCESS: &str = "oauth-window-access-3c9e1d";

/// The only ARD publisher domain that the operator of the rig allows.
#[allow(dead_code)]
pub const PUBLISHER: &str = "acme.com";

/// Why a connector other than `slack` cannot connect.
#[allow(dead_code)]
pub const REFUSED: &str = "connection refused";

pub struct Rig {
    pub _dir: tempfile::TempDir,
    pub llm: Arc<FakeLlm>,
    pub slack: Arc<FakeConnector>,
    pub engine: Arc<Engine>,
}

/// An engine with profiles `kim` and `lee`. `kim` has one pending post with ID 1.
pub async fn rig() -> Rig {
    rig_at(1_000).await
}

/// Like `rig`, with a clock that always shows `now` (seconds since 1970).
#[allow(dead_code)]
pub async fn rig_at(now: i64) -> Rig {
    rig_with(now, true).await
}

/// Like `rig`, on a server whose operator allows connectors that run a command, or does not.
#[allow(dead_code)]
pub async fn rig_with(now: i64, allow_stdio: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let tools = vec![tool("post", "Post a message")];
    let slack = Arc::new(FakeConnector::new("slack", tools.clone()));
    // The real address rules, with no token in the environment. The operator allows ARD listings
    // under `acme.com` only.
    let real = RealBackends::new(Arc::new(|_| None), allow_stdio, UpstreamPolicy::default())
        .with_ard_publishers(&[PUBLISHER.into()])
        .unwrap();
    let backends = Arc::new(Fakes {
        llm: llm.clone(),
        slack: slack.clone(),
        real,
    });
    let engine = Arc::new(Engine::open(dir.path(), backends, Arc::new(move || now)).unwrap());

    for owner in ["Kim", "Lee"] {
        let mut config = ProfileConfig::new(owner);
        config.approved_tools.insert("slack".into(), tools.clone());
        config.policy.rules.insert(
            "slack/post".into(),
            ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
        );
        engine.create_profile(&owner.to_lowercase(), config).await.unwrap();
    }
    let proposal = json!({
        "kind": "tool_call", "rationale": "Lee asked for the time", "summary": null,
        "tool": "slack/post",
        "args_json": json!({"channel": "C1", "text": "We deploy at 3 pm."}).to_string()
    });
    llm.reply(&proposal.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "e1".into(),
        thread: None,
        author: "lee".into(),
        text: "when do we deploy?".into(),
        channel: Default::default(),
    };
    engine.ingest("kim", event).await.unwrap();
    Rig {
        _dir: dir,
        llm,
        slack,
        engine,
    }
}

/// Lets `kim`'s agent search Slack. Then hands `kim` an event that the agent answers
/// after one search: a pending post with ID 2 that is based on that read.
#[allow(dead_code)]
pub async fn ingest_event_with_a_read(rig: &Rig) {
    let tools = vec![tool("post", "Post a message"), tool("search", "Search messages")];
    rig.slack.set_tools(tools.clone());
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    rig.engine.set_profile_config("kim", config).await.unwrap();
    rig.slack
        .set_result("search", json!({"messages": [{"text": "The deploy moved to Friday."}]}));
    let call = |tool: &str, args: serde_json::Value| {
        json!({
            "kind": "tool_call", "rationale": "Lee asked for the date", "summary": null,
            "tool": tool, "args_json": args.to_string()
        })
        .to_string()
    };
    rig.llm
        .reply(&call("slack/search", json!({"query": "deploy date"})), 10)
        .reply(&call("slack/post", json!({"channel": "C1", "text": "Friday."})), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "e-read".into(),
        thread: None,
        author: "lee".into(),
        text: "when is the deploy now?".into(),
        channel: Default::default(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
}

/// Hands `kim` a second event that ends as a pending post with ID 2.
#[allow(dead_code)]
pub async fn ingest_second_event(rig: &Rig) {
    let proposal = json!({
        "kind": "tool_call", "rationale": "Lee asked again", "summary": null,
        "tool": "slack/post",
        "args_json": json!({"channel": "C1", "text": "Still 3 pm."}).to_string()
    });
    rig.llm.reply(&proposal.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "e2".into(),
        thread: None,
        author: "lee".into(),
        text: "is it still 3 pm?".into(),
        channel: Default::default(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
}

/// Like `ingest_event_with_a_read`, and the agent also wants to keep what it read:
/// a pending note with ID 2 (a long page) and a pending post with ID 3.
#[allow(dead_code)]
pub async fn ingest_event_with_a_note(rig: &Rig) {
    let body = (1..=40)
        .map(|i| format!("Line {i} of the schedule."))
        .collect::<Vec<_>>()
        .join("\n")
        + "\nLAST-LINE-OF-THE-PAGE";
    ingest_note(rig, "e-note", false, &body).await;
}

/// Handles an event in which the agent searches and then proposes the page `deploy-schedule`
/// with `body`, together with a post. With `reads_page`, the agent reads the page first,
/// as it must before it changes an existing page.
#[allow(dead_code)]
pub async fn ingest_note(rig: &Rig, event_id: &str, reads_page: bool, body: &str) {
    let tools = vec![tool("post", "Post a message"), tool("search", "Search messages")];
    rig.slack.set_tools(tools.clone());
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let call = |tool: &str, args: serde_json::Value| json!({"kind": "tool_call", "rationale": "Lee asked for the date", "summary": null, "tool": tool, "args_json": args.to_string()});
    if reads_page {
        let read = call(bunsin_core::wiki::WIKI_READ_TOOL, json!({"slug": "deploy-schedule"}));
        rig.llm.reply(&read.to_string(), 10);
    }
    let search = call("slack/search", json!({"query": "deploy date"}));
    let mut answer = call("slack/post", json!({"channel": "C1", "text": "Friday."}));
    answer["wiki_updates"] = json!([{"slug": "deploy-schedule", "title": "Deploy schedule", "summary": "When the team deploys", "body": body}]);
    rig.llm.reply(&search.to_string(), 10).reply(&answer.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: event_id.into(),
        thread: None,
        author: "lee".into(),
        text: "when is the deploy now?".into(),
        channel: Default::default(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
}
