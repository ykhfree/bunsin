//! A real engine with a scripted LLM and a recording connector.

pub mod window;

use bunsin_api::{EngineApi, ProfileConfig};
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
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
}

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
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let tools = vec![tool("post", "Post a message")];
    let slack = Arc::new(FakeConnector::new("slack", tools.clone()));
    // The real address rules, with stdio connectors allowed and no token in the environment.
    let real = RealBackends::new(Arc::new(|_| None), true, UpstreamPolicy::default());
    let backends = Arc::new(Fakes {
        llm: llm.clone(),
        slack: slack.clone(),
        real,
    });
    let engine = Arc::new(Engine::open(dir.path(), backends, Arc::new(|| 1_000)).unwrap());

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
    };
    rig.engine.ingest("kim", event).await.unwrap();
}

/// Like `ingest_event_with_a_read`, and the agent also wants to keep what it read:
/// a pending note with ID 2 (a long page) and a pending post with ID 3.
#[allow(dead_code)]
pub async fn ingest_event_with_a_note(rig: &Rig) {
    let tools = vec![tool("post", "Post a message"), tool("search", "Search messages")];
    rig.slack.set_tools(tools.clone());
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let body = (1..=40)
        .map(|i| format!("Line {i} of the schedule."))
        .collect::<Vec<_>>()
        .join("\n")
        + "\nLAST-LINE-OF-THE-PAGE";
    let search = json!({
        "kind": "tool_call", "rationale": "Lee asked for the date", "summary": null,
        "tool": "slack/search", "args_json": json!({"query": "deploy date"}).to_string()
    });
    let answer = json!({
        "kind": "tool_call", "rationale": "Lee asked for the date", "summary": null,
        "tool": "slack/post", "args_json": json!({"channel": "C1", "text": "Friday."}).to_string(),
        "wiki_updates": [{"slug": "deploy-schedule", "title": "Deploy schedule", "summary": "When the team deploys", "body": body}]
    });
    rig.llm.reply(&search.to_string(), 10).reply(&answer.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "e-note".into(),
        thread: None,
        author: "lee".into(),
        text: "when is the deploy now?".into(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
}
