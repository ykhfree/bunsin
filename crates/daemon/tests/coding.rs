//! The server side of coding delegation: the operator's flags, programs and root, and whole jobs with a
//! temporary git repository and `/bin/sh` scripts as the coding agent. No real coding CLI runs.

use bunsin_api::{ApiError, EngineApi, Outcome, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::coding::{BackendKind, CODING_START_TOOL, CodingConfig};
use bunsin_core::event::{Channel, InboundEvent};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, Coder, LlmClient};
use bunsin_core::records::InboxBody;
use bunsin_daemon::coding::CodingAccess;
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::{Backends, Engine, Runtime};
use bunsin_testkit::FakeLlm;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

fn with_coding(coding: Value) -> ProfileConfig {
    let mut config = ProfileConfig::new("Kim");
    let coding: CodingConfig = serde_json::from_value(coding).unwrap();
    config.coding = Some(coding);
    let rule = ToolRule {
        class: ToolClass::Write,
        tier: Tier::Approve,
        outgoing_text: OutgoingText::None,
    };
    config.policy.rules.insert(CODING_START_TOOL.into(), rule);
    config
}

/// A `command` backend with a test command: both are programs that the profile names.
fn settings(path: &Path) -> ProfileConfig {
    with_coding(json!({
        "backend": "command",
        "command": ["/bin/sh", "-c", "cat > instructions.txt && echo done > result.txt"],
        "timeout_secs": 60,
        "repos": {"app": {"path": path, "test_command": ["/bin/sh", "-c", "test -f result.txt && echo tests pass"]}}
    }))
}

/// A backend at the path the operator names, with no test command.
fn backend_settings(backend: &str, path: &Path) -> ProfileConfig {
    with_coding(json!({"backend": backend, "timeout_secs": 60, "repos": {"app": {"path": path}}}))
}

/// What the operator allows: no coding, or coding with these programs, and with or without the
/// programs that profiles name.
enum Grant {
    None,
    Coding {
        programs: Vec<(BackendKind, PathBuf)>,
        allow_commands: bool,
    },
}

/// The command flag without operator programs.
fn commands() -> Grant {
    Grant::Coding {
        programs: Vec::new(),
        allow_commands: true,
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    root: PathBuf,
    llm: Arc<FakeLlm>,
    engine: Engine,
}

impl Rig {
    /// A git repository `app` under the root.
    fn app(&self) -> PathBuf {
        let path = self.root.join("app");
        repo(&path);
        path
    }
}

/// An engine whose server allows coding under `<temp>/code` as `grant` says.
fn rig(grant: Grant) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("code");
    std::fs::create_dir_all(&root).unwrap();
    let data = dir.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    let mut real = RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default());
    if let Grant::Coding {
        programs,
        allow_commands,
    } = grant
    {
        real = real.with_coding(CodingAccess {
            repo_root: root.clone(),
            data_dir: data.clone(),
            programs: programs.into_iter().collect(),
            allow_commands,
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

/// Saves `config` like the owner does, with the server's check.
async fn save(rig: &Rig, config: ProfileConfig) -> Result<(), ApiError> {
    rig.engine.set_profile_config("kim", config).await
}

fn refused_with(result: Result<(), ApiError>, text: &str) {
    assert!(
        matches!(&result, Err(ApiError::Invalid(error)) if error.contains(text)),
        "expected a refusal with `{text}`: {result:?}"
    );
}

/// Hands in an event that the agent answers with a coding call. Returns the waiting item.
async fn propose(rig: &Rig) -> i64 {
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
    match rig.engine.ingest("kim", event).await.unwrap() {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("the job does not wait for the owner: {other:?}"),
    }
}

/// Approves the job and waits until its end is recorded.
async fn run_job(rig: &Rig, inbox_id: i64) {
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
}

#[tokio::test]
async fn without_the_operator_flag_coding_settings_are_refused() {
    let rig = rig(Grant::None);
    rig.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let path = rig.app();
    refused_with(save(&rig, settings(&path)).await, "--allow-coding");
    refused_with(
        save(&rig, backend_settings("claude_code", &path)).await,
        "--allow-coding",
    );
}

#[tokio::test]
async fn a_repository_must_be_a_git_repository_under_the_root() {
    let rig = rig(commands());
    rig.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let outside = rig.root.parent().unwrap().join("elsewhere");
    repo(&outside);
    refused_with(save(&rig, settings(&outside)).await, "not under");
    // A link under the root that leads out of it is outside too.
    let link = rig.root.join("link");
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    refused_with(save(&rig, settings(&link)).await, "not under");
    let plain = rig.root.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    refused_with(save(&rig, settings(&plain)).await, "not a git repository");
    save(&rig, settings(&rig.app())).await.unwrap();
}

#[tokio::test]
async fn a_backend_needs_a_program_from_the_operator() {
    let rig = rig(Grant::Coding {
        programs: vec![(BackendKind::ClaudeCode, "/opt/bin/claude".into())],
        allow_commands: false,
    });
    rig.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let path = rig.app();
    save(&rig, backend_settings("claude_code", &path)).await.unwrap();
    refused_with(
        save(&rig, backend_settings("codex", &path)).await,
        "named no program for the backend `codex`",
    );
}

#[tokio::test]
async fn a_program_of_the_profile_needs_the_operators_command_flag() {
    let without = rig(Grant::Coding {
        programs: vec![(BackendKind::ClaudeCode, "/opt/bin/claude".into())],
        allow_commands: false,
    });
    without
        .engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let path = without.app();
    refused_with(save(&without, settings(&path)).await, "--allow-coding-commands");
    // A test command is a program of the profile too, also with an operator program for the backend.
    let mut tested = backend_settings("claude_code", &path);
    let repo = tested.coding.as_mut().unwrap().repos.get_mut("app").unwrap();
    repo.test_command = Some(vec!["/usr/bin/make".into(), "test".into()]);
    refused_with(save(&without, tested.clone()).await, "--allow-coding-commands");

    let with = rig(Grant::Coding {
        programs: vec![(BackendKind::ClaudeCode, "/opt/bin/claude".into())],
        allow_commands: true,
    });
    with.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    let path = with.app();
    save(&with, settings(&path)).await.unwrap();
    let mut tested = backend_settings("claude_code", &path);
    let repo = tested.coding.as_mut().unwrap().repos.get_mut("app").unwrap();
    repo.test_command = Some(vec!["/usr/bin/make".into(), "test".into()]);
    save(&with, tested).await.unwrap();
}

#[tokio::test]
async fn the_flags_are_checked_again_when_a_job_starts() {
    // Settings that a server with the command flag stored, on a server without it.
    let rig = rig(Grant::Coding {
        programs: Vec::new(),
        allow_commands: false,
    });
    let path = rig.app();
    rig.engine.create_profile("kim", settings(&path)).await.unwrap();
    let inbox_id = propose(&rig).await;
    match rig.engine.approve("kim", inbox_id, None).await.unwrap() {
        Outcome::ExecutionFailed { error, .. } => assert!(error.contains("--allow-coding-commands"), "{error}"),
        other => panic!("the job started: {other:?}"),
    }
    assert!(git(&path, &["branch", "--list", "agent/*"]).is_empty());
}

#[tokio::test]
async fn the_operators_program_runs_a_job_without_the_command_flag() {
    let dir = tempfile::tempdir().unwrap();
    // A stand-in for Claude Code. It ignores its arguments and reads the instructions on stdin.
    let script = dir.path().join("claude");
    std::fs::write(&script, "#!/bin/sh\ncat > instructions.txt\necho done > result.txt\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let rig = rig(Grant::Coding {
        programs: vec![(BackendKind::ClaudeCode, script)],
        allow_commands: false,
    });
    let path = rig.app();
    rig.engine
        .create_profile("kim", ProfileConfig::new("Kim"))
        .await
        .unwrap();
    save(&rig, backend_settings("claude_code", &path)).await.unwrap();
    let inbox_id = propose(&rig).await;
    run_job(&rig, inbox_id).await;
    let branch = format!("agent/b{inbox_id}-kim");
    let files = git(&path, &["ls-tree", "--name-only", &branch]);
    assert!(files.contains("result.txt"), "{files}");
    assert_eq!(
        git(&path, &["show", &format!("{branch}:instructions.txt")]),
        "Fix the login."
    );
}

#[tokio::test]
async fn an_approved_job_runs_in_a_worktree_and_the_owner_gets_its_branch() {
    let rig = rig(commands());
    let path = rig.app();
    let head = git(&path, &["rev-parse", "HEAD"]);
    rig.engine.create_profile("kim", settings(&path)).await.unwrap();
    let inbox_id = propose(&rig).await;
    run_job(&rig, inbox_id).await;

    // The branch holds the work. It is checked out in the job's worktree, outside the repository.
    let branch = format!("agent/b{inbox_id}-kim");
    git(&path, &["rev-parse", "--verify", &format!("refs/heads/{branch}")]);
    let worktrees = git(&path, &["worktree", "list", "--porcelain"]);
    assert!(
        worktrees.contains(&format!("coding/kim/worktrees/b{inbox_id}-kim")),
        "{worktrees}"
    );
    // The owner's checkout is unchanged.
    assert_eq!(git(&path, &["rev-parse", "HEAD"]), head);
    assert!(!path.join("result.txt").exists());
    let files = git(&path, &["ls-tree", "--name-only", &branch]);
    assert!(
        files.contains("result.txt") && files.contains("instructions.txt"),
        "{files}"
    );
    // The coding agent got the instructions on stdin.
    assert_eq!(
        git(&path, &["show", &format!("{branch}:instructions.txt")]),
        "Fix the login."
    );

    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    let InboxBody::Escalation { summary } = &snapshot.inbox[0].body else {
        panic!("no draft for the owner");
    };
    assert!(summary.contains(&format!("Branch `{branch}`")), "{summary}");
    assert!(
        summary.contains("result.txt") && summary.contains("ended with exit code 0"),
        "{summary}"
    );
    assert!(
        summary.contains("Tests: `/bin/sh -c test -f result.txt && echo tests pass` ended with exit code 0"),
        "{summary}"
    );
    assert!(summary.contains("merge it yourself"));
    let kinds: Vec<&str> = snapshot.artifacts.iter().map(|a| a.kind.as_str()).collect();
    assert!(
        kinds.contains(&"coding_result") && kinds.contains(&"coding_transcript"),
        "{kinds:?}"
    );
}
