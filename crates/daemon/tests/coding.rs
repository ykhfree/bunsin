//! The server side of coding delegation: the operator's flag and root, and a whole job with a temporary
//! git repository and a `/bin/sh` command as the coding agent. No real coding CLI runs.

use bunsin_api::{ApiError, EngineApi, Outcome, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::coding::{CODING_START_TOOL, CodingConfig};
use bunsin_core::event::{Channel, InboundEvent};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, Coder, LlmClient};
use bunsin_core::records::InboxBody;
use bunsin_daemon::coding::CodingAccess;
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Backends, Engine, Runtime};
use bunsin_testkit::FakeLlm;
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A scripted LLM, with the settings check and the coder of the real backends.
struct Fakes {
    llm: Arc<FakeLlm>,
    real: RealBackends,
}

impl Backends for Fakes {
    fn build<'a>(&'a self, _profile: &'a str, _config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        let llm: Arc<dyn LlmClient> = self.llm.clone();
        Box::pin(async move {
            Ok(Runtime {
                llm: Ok(llm),
                connectors: BTreeMap::new(),
            })
        })
    }

    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        self.real.validate(profile, config)
    }

    fn coder(&self) -> Result<Arc<dyn Coder>, String> {
        self.real.coder()
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

/// A git repository with one commit.
fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    std::fs::write(dir.join("README"), "app\n").unwrap();
    git(dir, &["add", "README"]);
    git(dir, &["commit", "-q", "-m", "start"]);
}

fn settings(path: &Path) -> ProfileConfig {
    let mut config = ProfileConfig::new("Kim");
    let coding: CodingConfig = serde_json::from_value(json!({
        "backend": "command", "program": "/bin/sh",
        "args": ["-c", "cat > instructions.txt && echo done > result.txt"],
        "timeout_secs": 60,
        "repos": {"app": {"path": path, "test_command": ["/bin/sh", "-c", "test -f result.txt && echo tests pass"]}}
    }))
    .unwrap();
    config.coding = Some(coding);
    let rule = ToolRule {
        class: ToolClass::Write,
        tier: Tier::Approve,
        outgoing_text: OutgoingText::None,
    };
    config.policy.rules.insert(CODING_START_TOOL.into(), rule);
    config
}

struct Rig {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    llm: Arc<FakeLlm>,
    engine: Engine,
}

/// An engine whose server allows coding under `<temp>/code`, or does not.
fn rig(allow_coding: bool) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut real = RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default());
    if allow_coding {
        real = real.with_coding(CodingAccess {
            repo_root: root.clone(),
            data_dir: data.clone(),
        });
    }
    let llm = Arc::new(FakeLlm::new());
    let backends = Arc::new(Fakes { llm: llm.clone(), real });
    let engine = Engine::open(&data, backends, Arc::new(|| 1_000)).unwrap();
    Rig {
        _dir: dir,
        root,
        llm,
        engine,
    }
}

#[tokio::test]
async fn without_the_operator_flag_coding_settings_are_refused() {
    let rig = rig(false);
    rig.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let path = rig.root.join("app");
    repo(&path);
    let refused = rig.engine.set_profile_config("kim", settings(&path)).await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(text)) if text.contains("--allow-coding")),
        "{refused:?}"
    );
}

#[tokio::test]
async fn a_repository_must_be_a_git_repository_under_the_root() {
    let rig = rig(true);
    rig.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let outside = rig.root.parent().unwrap().join("elsewhere");
    repo(&outside);
    let refused = rig.engine.set_profile_config("kim", settings(&outside)).await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(text)) if text.contains("not under")),
        "{refused:?}"
    );
    // A link under the root that leads out of it is outside too.
    let link = rig.root.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let refused = rig.engine.set_profile_config("kim", settings(&link)).await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(text)) if text.contains("not under")),
        "{refused:?}"
    );
    let plain = rig.root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let refused = rig.engine.set_profile_config("kim", settings(&plain)).await;
    assert!(
        matches!(&refused, Err(ApiError::Invalid(text)) if text.contains("not a git repository")),
        "{refused:?}"
    );
    let path = rig.root.join("app");
    repo(&path);
    rig.engine.set_profile_config("kim", settings(&path)).await.unwrap();
}

#[tokio::test]
async fn an_approved_job_runs_in_a_worktree_and_the_owner_gets_its_branch() {
    let rig = rig(true);
    let path = rig.root.join("app");
    repo(&path);
    let head = git(&path, &["rev-parse", "HEAD"]);
    rig.engine.create_profile("kim", settings(&path)).await.unwrap();
    let call = json!({"kind": "tool_call", "rationale": "Lee reported a bug", "summary": null,
        "tool": CODING_START_TOOL, "args_json": json!({"repo": "app", "instructions": "Fix the login."}).to_string()});
    rig.llm.reply(&call.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "1".into(),
        thread: None,
        author: "lee".into(),
        text: "the login is broken".into(),
        channel: Channel::Owner,
    };
    let Outcome::PendingApproval { inbox_id } = rig.engine.ingest("kim", event).await.unwrap() else {
        panic!("the job does not wait for the owner");
    };
    assert_eq!(
        rig.engine.approve("kim", inbox_id, None).await.unwrap(),
        Outcome::Started { inbox_id }
    );

    let store = rig.engine.app_store().open_profile("kim").unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while store.inbox_item(inbox_id).unwrap().state == ApprovalState::Executing {
        assert!(rig.engine.background_tick().await.is_empty());
        assert!(Instant::now() < deadline, "the job did not end");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(store.inbox_item(inbox_id).unwrap().state, ApprovalState::Executed);

    // The branch holds the work. The owner's checkout is unchanged.
    let branch = format!("agent/b{inbox_id}-kim");
    // The branch is checked out in the job's worktree, outside the repository.
    git(&path, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]);
    let worktrees = git(&path, &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains(&format!("coding/kim/worktrees/b{inbox_id}-kim")),
        "{worktrees}"
    );
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    assert!(!path.join("result.txt").exists());
    let files = git(&path, &["ls-tree", "--name-only", &branch]);
    assert!(
        files.contains("result.txt") && files.contains("instructions.txt"),
        "{files}"
    );
    // The coding agent got the instructions on stdin.
    let given = git(&path, &["show", &format!("{branch}:instructions.txt")]);
    assert_eq!(given, "Fix the login.");

    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    let InboxBody::Escalation { summary } = &snapshot.inbox[0].body else {
        panic!("no draft for the owner");
    };
    assert!(summary.contains(&format!("Branch `{branch}`")), "{summary}");
    assert!(
        summary.contains("result.txt") && summary.contains("ended with exit code 0"),
        "{summary}"
    );
    assert!(summary.contains("merge it yourself"));
    let kinds: Vec<&str> = snapshot.artifacts.iter().map(|a| a.kind.as_str()).collect();
    assert!(
        kinds.contains(&"coding_result") && kinds.contains(&"coding_transcript"),
        "{kinds:?}"
    );
}
