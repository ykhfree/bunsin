//! The server's coder. It runs a coding job of the engine with `bunsin-coding` on a tokio task and keeps
//! its report until the engine's background work collects it.
//!
//! Only an operator who starts the server with `--allow-coding` and `--coding-repo-root` gets one.
//! The operator also names the program of each coding agent CLI (`--coding-program`). A profile picks
//! only the kind. A program that a profile names itself (the backend `command`, a test command) runs only
//! if the operator passes `--allow-coding-commands`, which lets every profile owner run any program as the
//! server user. Every check runs when the settings are saved and again when a job starts.
//! A repository must be a git repository under the root. No flag turns off the permission prompts or the
//! sandbox of a coding CLI, and Bunsin adds no sandbox of its own: a job runs as the server user.

use bunsin_coding::{ClaudeCode, Codex, CodingBackend, CodingJob, CodingResult, CodingRunner, Generic};
use bunsin_core::coding::{BackendKind, CodingConfig, CodingJobSpec, CodingOutcome, CodingReport, TestOutcome};
use bunsin_core::ports::Coder;
use std::collections::BTreeMap;
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
    /// The program of each coding agent CLI that the operator allows. A backend without one cannot run.
    pub programs: BTreeMap<BackendKind, PathBuf>,
    /// True if a profile may name programs itself: the backend `command` and test commands.
    pub allow_commands: bool,
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

    /// Refuses settings that this server will not run: a backend without an operator program, a program
    /// that the profile names without `--allow-coding-commands`, or a repository outside the root.
    pub fn check_settings(&self, settings: &CodingConfig) -> Result<(), String> {
        self.program(settings.backend, settings.command.as_ref())?;
        if settings.names_programs() && !self.access.allow_commands {
            return Err(Self::COMMANDS_OFF.into());
        }
        for (name, repo) in &settings.repos {
            self.check_repo(name, &repo.path)?;
        }
        Ok(())
    }

    const COMMANDS_OFF: &'static str = "the settings name a program of their own (the backend `command` or a test \
        command), but the server operator did not start the server with --allow-coding-commands";

    /// The program and arguments that run `backend`. Claude Code and Codex get their fixed arguments from
    /// `bunsin-coding`; only `command` brings its own.
    fn program(&self, backend: BackendKind, command: Option<&Vec<String>>) -> Result<(PathBuf, Vec<String>), String> {
        match backend {
            BackendKind::Command if !self.access.allow_commands => Err(Self::COMMANDS_OFF.into()),
            BackendKind::Command => match command.map(Vec::as_slice) {
                Some([program, args @ ..]) => Ok((program.into(), args.to_vec())),
                _ => Err("the backend `command` needs `coding.command`".into()),
            },
            kind => match self.access.programs.get(&kind) {
                Some(program) => Ok((program.clone(), Vec::new())),
                None => Err(format!(
                    "the server operator named no program for the backend `{}` (--coding-program)",
                    kind.as_str()
                )),
            },
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

/// Reads one `--coding-program` value: `claude-code=<absolute path>` or `codex=<absolute path>`.
pub fn parse_program(text: &str) -> Result<(BackendKind, PathBuf), String> {
    let (name, path) = text
        .split_once('=')
        .ok_or_else(|| format!("--coding-program `{text}` is not <backend>=<path>"))?;
    let kind = match name {
        "claude-code" => BackendKind::ClaudeCode,
        "codex" => BackendKind::Codex,
        _ => {
            return Err(format!(
                "--coding-program: `{name}` is not a backend: use claude-code or codex"
            ));
        }
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(format!(
            "--coding-program: `{}` is not an absolute path",
            path.display()
        ));
    }
    Ok((kind, path))
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

fn backend(kind: BackendKind, program: PathBuf, args: Vec<String>) -> Box<dyn CodingBackend> {
    match kind {
        BackendKind::ClaudeCode => Box::new(ClaudeCode { program }),
        BackendKind::Codex => Box::new(Codex { program }),
        BackendKind::Command => Box::new(Generic {
            program,
            args_template: args,
        }),
    }
}

impl Coder for RealCoder {
    fn start(&self, spec: CodingJobSpec) -> Result<(), String> {
        // The settings were checked when they were saved. The operator's flags may differ since a restart.
        let (program, args) = self.program(spec.backend, spec.command.as_ref())?;
        if spec.test_command.is_some() && !self.access.allow_commands {
            return Err(Self::COMMANDS_OFF.into());
        }
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
            let result = runner
                .run(backend(spec.backend, program, args).as_ref(), &job, &logs)
                .await;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_operator_program_names_a_known_backend_and_an_absolute_path() {
        assert_eq!(
            parse_program("claude-code=/opt/bin/claude"),
            Ok((BackendKind::ClaudeCode, PathBuf::from("/opt/bin/claude")))
        );
        assert_eq!(
            parse_program("codex=/usr/local/bin/codex"),
            Ok((BackendKind::Codex, PathBuf::from("/usr/local/bin/codex")))
        );
        // A custom command is not an operator program: it needs --allow-coding-commands.
        assert!(parse_program("command=/bin/sh").unwrap_err().contains("not a backend"));
        assert!(parse_program("codex=codex").unwrap_err().contains("absolute"));
        assert!(
            parse_program("/opt/bin/claude")
                .unwrap_err()
                .contains("<backend>=<path>")
        );
    }
}
