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
