//! A real engine with a scripted LLM, a recording connector and a clock the test sets.
#![allow(dead_code)]

use bunsin_api::ProfileConfig;
use bunsin_core::coding::{CodingJobSpec, CodingReport};
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use bunsin_core::poll::{PollConfig, PollMapping};
use bunsin_core::ports::{BoxFuture, Coder, Connector, LlmClient};
use bunsin_core::tool::ToolSpec;
use bunsin_engine::{Backends, Engine, Runtime};
use bunsin_testkit::{FakeConnector, FakeLlm, tool};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

pub const MARKER: &str = "— AI agent (Bunsin) acting for Kim";

struct Fakes {
    llm: Arc<FakeLlm>,
    slack: Arc<FakeConnector>,
    coder: Arc<FakeCoder>,
}

/// A coder that records the jobs it is asked to start and gives back the reports a test hands it.
#[derive(Default)]
pub struct FakeCoder {
    started: Mutex<Vec<CodingJobSpec>>,
    finished: Mutex<Vec<CodingReport>>,
    refusal: Mutex<Option<String>>,
}

impl FakeCoder {
    pub fn started(&self) -> Vec<CodingJobSpec> {
        self.started.lock().unwrap().clone()
    }

    /// The report of a job that ended. The next background tick collects it.
    pub fn finish(&self, report: CodingReport) {
        self.finished.lock().unwrap().push(report);
    }

    /// Every later start fails with `reason`.
    pub fn refuse(&self, reason: &str) {
        *self.refusal.lock().unwrap() = Some(reason.to_owned());
    }
}

impl Coder for FakeCoder {
    fn start(&self, spec: CodingJobSpec) -> Result<(), String> {
        if let Some(reason) = self.refusal.lock().unwrap().clone() {
            return Err(reason);
        }
        self.started.lock().unwrap().push(spec);
        Ok(())
    }

    fn finished(&self) -> Vec<CodingReport> {
        std::mem::take(&mut *self.finished.lock().unwrap())
    }
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

    fn coder(&self) -> Result<Arc<dyn Coder>, String> {
        Ok(self.coder.clone())
    }
}

pub struct Rig {
    pub dir: tempfile::TempDir,
    pub llm: Arc<FakeLlm>,
    pub slack: Arc<FakeConnector>,
    pub coder: Arc<FakeCoder>,
    pub clock: Arc<AtomicI64>,
    pub engine: Engine,
}

impl Rig {
    /// Moves the clock forward.
    pub fn advance(&self, secs: i64) {
        self.clock.fetch_add(secs, Ordering::SeqCst);
    }

    /// Every byte the engine wrote: the databases and the artifact files.
    pub fn stored_bytes(&self) -> Vec<u8> {
        fn walk(dir: &Path, out: &mut Vec<u8>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.extend(std::fs::read(&path).unwrap());
                }
            }
        }
        let mut out = Vec::new();
        walk(self.dir.path(), &mut out);
        out
    }

    /// Stops the engine like a crash would and opens a new one on the same data directory,
    /// with the same fakes and clock.
    pub fn restart(self) -> Rig {
        let Rig {
            dir,
            llm,
            slack,
            coder,
            clock,
            engine,
        } = self;
        drop(engine);
        let backends = Arc::new(Fakes {
            llm: llm.clone(),
            slack: slack.clone(),
            coder: coder.clone(),
        });
        let time = clock.clone();
        let engine = Engine::open(dir.path(), backends, Arc::new(move || time.load(Ordering::SeqCst))).unwrap();
        Rig {
            dir,
            llm,
            slack,
            coder,
            clock,
            engine,
        }
    }

    /// The store of profile `kim`, opened beside the engine.
    pub fn kim_store(&self) -> bunsin_store::ProfileStore {
        bunsin_store::AppStore::open(self.dir.path())
            .unwrap()
            .open_profile("kim")
            .unwrap()
    }

    pub fn stores(&self, value: &str) -> bool {
        self.stored_bytes()
            .windows(value.len())
            .any(|window| window == value.as_bytes())
    }
}

pub fn slack_tools() -> Vec<ToolSpec> {
    vec![tool("search", "Search messages"), tool("post", "Post a message")]
}

/// A profile where `search` is a read and `post` needs approval.
pub fn config() -> ProfileConfig {
    let mut config = ProfileConfig::new("Kim");
    config.approved_tools.insert("slack".into(), slack_tools());
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    config.policy.rules.insert(
        "slack/post".into(),
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
    );
    config
}

/// A poll of `slack/search` every 60 seconds.
pub fn poll() -> PollConfig {
    PollConfig {
        connector: "slack".into(),
        tool: "search".into(),
        args: json!({"query": "to:me"}).as_object().unwrap().clone(),
        every_secs: 60,
        mapping: PollMapping {
            items: "messages".into(),
            id: "ts".into(),
            text: "text".into(),
            author: "user".into(),
            thread: None,
        },
        skip_authors: vec!["kim".into()],
    }
}

pub fn message(ts: &str, user: &str, text: &str) -> Value {
    json!({"ts": ts, "user": user, "text": text})
}

pub async fn rig_with(config: ProfileConfig) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let llm = Arc::new(FakeLlm::new());
    let slack = Arc::new(FakeConnector::new("slack", slack_tools()));
    let clock = Arc::new(AtomicI64::new(1_000_000));
    let time = clock.clone();
    let coder = Arc::new(FakeCoder::default());
    let backends = Arc::new(Fakes {
        llm: llm.clone(),
        slack: slack.clone(),
        coder: coder.clone(),
    });
    let engine = Engine::open(dir.path(), backends, Arc::new(move || time.load(Ordering::SeqCst))).unwrap();
    engine.create_profile("kim", config).await.unwrap();
    Rig {
        dir,
        llm,
        slack,
        coder,
        clock,
        engine,
    }
}

pub async fn rig() -> Rig {
    rig_with(config()).await
}

pub fn event(id: &str, author: &str, text: &str) -> InboundEvent {
    InboundEvent {
        source: "slack".into(),
        external_id: id.into(),
        thread: None,
        author: author.into(),
        text: text.into(),
        channel: Default::default(),
    }
}

pub fn call(tool: &str, args: Value) -> String {
    json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": tool, "args_json": args.to_string()})
        .to_string()
}

pub fn post(text: &str) -> String {
    call("slack/post", json!({"channel": "C1", "text": text}))
}

pub fn ignore() -> String {
    json!({"kind": "ignore", "rationale": "no action needed", "summary": null, "tool": null, "args_json": null})
        .to_string()
}

/// Every LLM request so far as one JSON text.
pub fn all_requests(rig: &Rig) -> String {
    serde_json::to_string(&rig.llm.requests()).unwrap()
}
