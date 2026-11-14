//! A real engine with a scripted LLM and a recording connector.

use bunsin_api::{EngineApi, ProfileConfig};
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_engine::{Backends, Engine, Runtime};
use bunsin_testkit::{FakeConnector, FakeLlm, tool};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

struct Fakes {
    llm: Arc<FakeLlm>,
    slack: Arc<FakeConnector>,
}

impl Backends for Fakes {
    fn build<'a>(
        &'a self,
        _profile: &'a str,
        _config: &'a ProfileConfig,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        let slack: Arc<dyn Connector> = self.slack.clone();
        Box::pin(async move {
            Ok(Runtime {
                llm,
                connectors: BTreeMap::from([("slack".to_owned(), slack)]),
            })
        })
    }
}

pub struct Rig {
    pub _dir: tempfile::TempDir,
    pub slack: Arc<FakeConnector>,
    pub engine: Arc<Engine>,
}

/// An engine with profiles `kim` and `lee`. `kim` has one pending post with ID 1.
pub async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let tools = vec![tool("post", "Post a message")];
    let slack = Arc::new(FakeConnector::new("slack", tools.clone()));
    let backends = Arc::new(Fakes {
        llm: llm.clone(),
        slack: slack.clone(),
    });
    let engine = Arc::new(Engine::open(dir.path(), backends, Arc::new(|| 1_000)).unwrap());

    for owner in ["Kim", "Lee"] {
        let mut config = ProfileConfig::new(owner);
        config.approved_tools.insert("slack".into(), tools.clone());
        config.policy.rules.insert(
            "slack/post".into(),
            ToolRule::write(ToolClass::Write, Some("text")),
        );
        engine
            .create_profile(&owner.to_lowercase(), config)
            .await
            .unwrap();
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
        slack,
        engine,
    }
}
