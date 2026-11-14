//! Coding delegation. The persona agent never writes code itself.
//! It hands a `CodingJob` to a coding agent CLI through a `CodingBackend`.
//! `CodingRunner` runs the job in a git worktree and reports facts it measured itself.

mod backend;
mod runner;

pub use backend::{BackendReport, ClaudeCode, Codex, CodingBackend, CommandSpec, Generic};
pub use runner::CodingRunner;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

/// One unit of work for a coding agent.
#[derive(Debug, Clone)]
pub struct CodingJob {
    /// Names the branch `agent/<id>` and the worktree directory.
    /// Allowed: 1-64 ASCII letters, digits, `-` and `_`; the first character is a letter or digit.
    pub id: String,
    /// The user's repository. The runner never changes its checkout.
    pub repo: PathBuf,
    /// The commit-ish the job branch starts from.
    pub base_ref: String,
    pub instructions: String,
    /// Program and arguments, run in the worktree after the backend. No shell is used.
    pub test_command: Option<Vec<String>>,
    /// Limit for the backend run. The test command gets the same limit.
    pub timeout: Duration,
}

/// What the runner measured. The backend's own claims are only in `summary`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingResult {
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    /// Output of `git diff --stat <base_commit> <head_commit>`. Empty when nothing changed.
    pub diff_stat: String,
    pub test: Option<TestRun>,
    pub exit: ExitInfo,
    pub transcript: PathBuf,
    pub summary: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestRun {
    pub command: Vec<String>,
    /// `None` when a signal ended the process (this includes the kill on timeout).
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// Stdout followed by stderr. Only the tail is kept when the output is long.
    pub output: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExitInfo {
    pub code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum CodingError {
    #[error("program must be an absolute path: {0}")]
    RelativeProgram(PathBuf),
    #[error("invalid job id {0:?}: use 1-64 ASCII letters, digits, '-' or '_', starting with a letter or digit")]
    InvalidJobId(String),
    #[error("test command is empty")]
    EmptyTestCommand,
    #[error("branch {0} already exists")]
    BranchExists(String),
    #[error("worktree directory already exists: {0}")]
    WorktreeExists(PathBuf),
    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },
    #[error("could not start {program}: {source}")]
    Spawn { program: PathBuf, source: std::io::Error },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
