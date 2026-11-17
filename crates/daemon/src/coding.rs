//! The server's coder. It runs a coding job of the engine with `bunsin-coding` on a tokio task and keeps
//! its report until the engine's background work collects it.
//!
//! Only an operator who starts the server with `--allow-coding` and `--coding-repo-root` gets one.
//! A repository must be a git repository under that root, when the settings are saved and when a job starts.
//! No flag turns off the permission prompts or the sandbox of a coding CLI.

use bunsin_coding::{ClaudeCode, Codex, CodingBackend, CodingJob, CodingResult, CodingRunner, Generic};
use bunsin_core::coding::{BackendKind, CodingJobSpec, CodingOutcome, CodingReport, TestOutcome};
use bunsin_core::ports::Coder;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// What the operator allows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodingAccess {
    /// Every repository of a coding job must be under this directory.
    pub repo_root: PathBuf,
    /// The data directory of the server. Jobs keep their worktrees and transcripts in `coding/<profile>`.
    pub data_dir: PathBuf,
}

pub struct RealCoder {
    access: CodingAccess,
    finished: Arc<Mutex<Vec<CodingReport>>>,
}

impl RealCoder {
    pub fn new(access: CodingAccess) -> Self {
        Self {
            access,
            finished: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Refuses a repository that is not a git repository under the operator's root. Links are followed
    /// first, so a link cannot lead out of the root.
    pub fn check_repo(&self, name: &str, path: &Path) -> Result<(), String> {
        let root = std::fs::canonicalize(&self.access.repo_root)
            .map_err(|e| format!("the coding root of the server cannot be read: {e}"))?;
        let real = std::fs::canonicalize(path).map_err(|e| format!("repository `{name}` cannot be read: {e}"))?;
        if !real.starts_with(&root) {
            return Err(format!(
                "repository `{name}` is not under the directory that the server operator allows for coding"
            ));
        }
        if !real.join(".git").exists() {
            return Err(format!("repository `{name}` is not a git repository"));
        }
        Ok(())
    }
}

/// The facts of a run, with the text of its transcript file.
fn outcome(result: CodingResult) -> CodingOutcome {
    let transcript = match std::fs::read(&result.transcript) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) => format!("the transcript cannot be read: {error}"),
    };
    CodingOutcome {
        branch: result.branch,
        base_commit: result.base_commit,
        head_commit: result.head_commit,
        diff_stat: result.diff_stat,
        test: result.test.map(|test| TestOutcome {
            command: test.command,
            exit_code: test.exit_code,
            timed_out: test.timed_out,
            output: test.output,
        }),
        exit_code: result.exit.code,
        signal: result.exit.signal,
        timed_out: result.exit.timed_out,
        summary: result.summary,
        transcript,
    }
}

fn backend(spec: &CodingJobSpec) -> Box<dyn CodingBackend> {
    let program = spec.program.clone();
    match spec.backend {
        BackendKind::ClaudeCode => Box::new(ClaudeCode { program }),
        BackendKind::Codex => Box::new(Codex { program }),
        BackendKind::Command => Box::new(Generic {
            program,
            args_template: spec.args.clone(),
        }),
    }
}

impl Coder for RealCoder {
    fn start(&self, spec: CodingJobSpec) -> Result<(), String> {
        self.check_repo(&spec.repo_name, &spec.repo_path)?;
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| "the server has no task runtime".to_owned())?;
        let base = self.access.data_dir.join("coding").join(&spec.profile);
        let runner = CodingRunner::new(base.join("worktrees"));
        let logs = base.join("logs");
        let finished = self.finished.clone();
        runtime.spawn(async move {
            let job = CodingJob {
                id: spec.job_id.clone(),
                repo: spec.repo_path.clone(),
                base_ref: spec.base_ref.clone(),
                instructions: spec.instructions.clone(),
                test_command: spec.test_command.clone(),
                timeout: Duration::from_secs(spec.timeout_secs),
            };
            let result = runner.run(backend(&spec).as_ref(), &job, &logs).await;
            let report = CodingReport {
                profile: spec.profile,
                inbox_id: spec.inbox_id,
                outcome: result.map(outcome).map_err(|e| e.to_string()),
            };
            finished.lock().expect("coder lock").push(report);
        });
        Ok(())
    }

    fn finished(&self) -> Vec<CodingReport> {
        std::mem::take(&mut *self.finished.lock().expect("coder lock"))
    }
}
