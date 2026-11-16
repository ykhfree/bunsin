//! Coding delegation as the engine sees it: the settings, the built-in tool `bunsin.coding/start`,
//! the job the server runs, and the report it gives back. The runner itself is in `bunsin-coding`.
//!
//! The LLM chooses only a repository by name, a base and the instructions. The program, the paths and
//! the test command come from the settings. A coding job always needs the owner's approval, and the
//! engine never merges: the owner reviews the branch and merges it.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The tool IDs of the built-in coding tools start with this. A connector name cannot contain `.`.
pub const CODING_TOOL_PREFIX: &str = "bunsin.coding/";
/// The built-in tool that starts a coding job.
pub const CODING_START_TOOL: &str = "bunsin.coding/start";
/// The longest time one job may run, in seconds.
pub const MAX_TIMEOUT_SECS: u64 = 24 * 3_600;

pub fn is_coding_tool(tool: &str) -> bool {
    tool.starts_with(CODING_TOOL_PREFIX)
}

/// The ID of the job of inbox item `inbox_id`. It names the branch `agent/<job ID>`. With a profile ID of
/// at most 40 characters it has at most 61, within the runner's limit of 64.
pub fn job_id(inbox_id: i64, profile: &str) -> String {
    format!("b{inbox_id}-{profile}")
}

/// The coding agent CLI that runs a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendKind {
    ClaudeCode,
    Codex,
    /// A program the owner names, with fixed arguments. It reads the instructions on stdin.
    Command,
}

/// A repository that coding jobs may change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// The absolute path of the repository on the server.
    pub path: PathBuf,
    /// A program and its arguments, run in the job's worktree after the coding agent. No shell is used.
    #[serde(default)]
    pub test_command: Option<Vec<String>>,
}

/// The coding settings of a profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingConfig {
    pub backend: BackendKind,
    /// The absolute path of the coding agent CLI.
    pub program: PathBuf,
    /// The arguments of a `command` backend. The other backends have fixed arguments.
    #[serde(default)]
    pub args: Vec<String>,
    /// The limit of the coding agent's run. The test command gets the same limit.
    pub timeout_secs: u64,
    /// By name. The LLM sees only the names.
    pub repos: BTreeMap<String, RepoConfig>,
}

/// True if `name` can name a repository: 1 to 40 of a-z, 0-9, `-` and `_`.
pub fn valid_repo_name(name: &str) -> bool {
    (1..=40).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

impl CodingConfig {
    /// Refuses settings that cannot run a job. The error says what to change.
    pub fn check(&self) -> Result<(), String> {
        if !self.program.is_absolute() {
            return Err("`coding.program` must be an absolute path".into());
        }
        if self.backend != BackendKind::Command && !self.args.is_empty() {
            return Err("`coding.args` is only for the backend `command`".into());
        }
        if !(1..=MAX_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return Err(format!("`coding.timeout_secs` must be 1 to {MAX_TIMEOUT_SECS}"));
        }
        if self.repos.is_empty() {
            return Err("`coding.repos` names no repository".into());
        }
        for (name, repo) in &self.repos {
            if !valid_repo_name(name) {
                return Err(format!(
                    "the repository name `{name}` is invalid: use 1 to 40 of a-z, 0-9, `-` and `_`"
                ));
            }
            if !repo.path.is_absolute() {
                return Err(format!("the path of repository `{name}` must be absolute"));
            }
            if repo.test_command.as_ref().is_some_and(|command| command.is_empty()) {
                return Err(format!("the test command of repository `{name}` is empty"));
            }
        }
        Ok(())
    }

    /// The description of `bunsin.coding/start` as the LLM sees it: fixed text and the repository names.
    pub fn tool_description(&self) -> String {
        let names: Vec<&str> = self.repos.keys().map(String::as_str).collect();
        format!(
            "Ask a coding agent to change code in one of the owner's repositories: {}. \
             The owner approves each job. The job works on a new branch; the owner reviews and merges it.",
            names.join(", ")
        )
    }

    /// The argument schema of `bunsin.coding/start`.
    pub fn tool_schema(&self) -> Value {
        let names: Vec<&str> = self.repos.keys().map(String::as_str).collect();
        json!({
            "type": "object",
            "required": ["repo", "instructions"],
            "properties": {
                "repo": {"type": "string", "enum": names},
                "base_ref": {"type": "string", "description": "The branch or commit to start from. Default: HEAD."},
                "instructions": {"type": "string"}
            }
        })
    }
}

/// The arguments of a call of `bunsin.coding/start`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartArgs {
    pub repo: String,
    pub base_ref: String,
    pub instructions: String,
}

impl StartArgs {
    /// Reads the arguments of a call. A missing `base_ref` is `HEAD`.
    pub fn parse(args: &Map<String, Value>) -> Result<Self, String> {
        let text = |name: &str| args.get(name).and_then(Value::as_str).map(str::trim);
        let repo = text("repo")
            .filter(|repo| !repo.is_empty())
            .ok_or("the call names no `repo`")?;
        let instructions = text("instructions")
            .filter(|text| !text.is_empty())
            .ok_or("the call has no `instructions`")?;
        let base_ref = text("base_ref").filter(|base| !base.is_empty()).unwrap_or("HEAD");
        // A base that starts with `-` could be read as an option of git.
        if base_ref.starts_with('-') {
            return Err(format!("the base `{base_ref}` is invalid"));
        }
        Ok(Self {
            repo: repo.to_owned(),
            base_ref: base_ref.to_owned(),
            instructions: instructions.to_owned(),
        })
    }
}

/// One job, as the engine hands it to the server's runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingJobSpec {
    pub profile: String,
    pub inbox_id: i64,
    /// `b<inbox ID>-<profile>`. It names the branch `agent/<job ID>`.
    pub job_id: String,
    pub repo_name: String,
    pub repo_path: PathBuf,
    pub base_ref: String,
    pub instructions: String,
    pub test_command: Option<Vec<String>>,
    pub backend: BackendKind,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub timeout_secs: u64,
}

impl CodingJobSpec {
    /// The job of an approved call. Fails if the settings do not name the repository.
    pub fn new(profile: &str, inbox_id: i64, config: &CodingConfig, args: StartArgs) -> Result<Self, String> {
        let repo = config
            .repos
            .get(&args.repo)
            .ok_or_else(|| format!("the settings name no repository `{}`", args.repo))?;
        Ok(Self {
            profile: profile.to_owned(),
            inbox_id,
            job_id: job_id(inbox_id, profile),
            repo_name: args.repo,
            repo_path: repo.path.clone(),
            base_ref: args.base_ref,
            instructions: args.instructions,
            test_command: repo.test_command.clone(),
            backend: config.backend,
            program: config.program.clone(),
            args: config.args.clone(),
            timeout_secs: config.timeout_secs,
        })
    }
}

/// The run of the test command, as the runner measured it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestOutcome {
    pub command: Vec<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// The tail of stdout and stderr.
    pub output: String,
}

/// What a job that made a branch did. Every fact is measured by the runner, except `summary`,
/// which is what the coding agent says about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingOutcome {
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    pub diff_stat: String,
    pub test: Option<TestOutcome>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub timed_out: bool,
    pub summary: Option<String>,
    /// The text of the run's transcript file.
    pub transcript: String,
}

impl CodingOutcome {
    /// True if the coding agent ended by itself with exit code 0. The tests do not decide it:
    /// the owner reads their result.
    pub fn ended_well(&self) -> bool {
        self.exit_code == Some(0) && !self.timed_out
    }
}

/// The end of a job. `Err` holds why the runner made no branch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodingReport {
    pub profile: String,
    pub inbox_id: i64,
    pub outcome: Result<CodingOutcome, String>,
}

/// The text of the draft that tells the owner about an ended job.
pub fn report_text(job: &str, repo: &str, outcome: &Result<CodingOutcome, String>) -> String {
    let outcome = match outcome {
        Err(error) => {
            return format!(
                "The coding job {job} in `{repo}` failed before it made a branch: {error}. Nothing to merge."
            );
        }
        Ok(outcome) => outcome,
    };
    let ended = match (outcome.timed_out, outcome.exit_code) {
        (true, _) => "stopped at its time limit".to_owned(),
        (false, Some(code)) => format!("ended with exit code {code}"),
        (false, None) => "was ended by a signal".to_owned(),
    };
    let changes = match outcome.diff_stat.trim() {
        "" => "no change".to_owned(),
        stat => stat.to_owned(),
    };
    let tests = match &outcome.test {
        None => "no test command".to_owned(),
        Some(test) if test.timed_out => format!("`{}` stopped at the time limit", test.command.join(" ")),
        Some(test) => match test.exit_code {
            Some(code) => format!("`{}` ended with exit code {code}", test.command.join(" ")),
            None => format!("`{}` was ended by a signal", test.command.join(" ")),
        },
    };
    let short = |commit: &str| commit.chars().take(12).collect::<String>();
    format!(
        "The coding job {job} in `{repo}` {ended}. Branch `{}`: base {}, head {}.\nChanges:\n{changes}\nTests: {tests}.\n\
         Review the branch and merge it yourself. Bunsin never merges.",
        outcome.branch,
        short(&outcome.base_commit),
        short(&outcome.head_commit)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> CodingConfig {
        serde_json::from_value(json!({
            "backend": "command", "program": "/bin/sh", "args": ["-c", "true"], "timeout_secs": 60,
            "repos": {"app": {"path": "/srv/app", "test_command": ["/usr/bin/make", "test"]}}
        }))
        .unwrap()
    }

    #[test]
    fn settings_must_name_absolute_paths_and_a_repository() {
        assert_eq!(config().check(), Ok(()));
        let mut bad = config();
        bad.program = "sh".into();
        assert!(bad.check().unwrap_err().contains("absolute"));
        let mut bad = config();
        bad.backend = BackendKind::ClaudeCode;
        assert!(bad.check().unwrap_err().contains("only for the backend"));
        let mut bad = config();
        bad.timeout_secs = 0;
        assert!(bad.check().unwrap_err().contains("timeout_secs"));
        let mut bad = config();
        bad.repos.clear();
        assert!(bad.check().unwrap_err().contains("no repository"));
        let mut bad = config();
        bad.repos.insert("Bad Name".into(), bad.repos["app"].clone());
        assert!(bad.check().unwrap_err().contains("name"));
        let mut bad = config();
        bad.repos.get_mut("app").unwrap().path = "srv/app".into();
        assert!(bad.check().unwrap_err().contains("absolute"));
        let mut bad = config();
        bad.repos.get_mut("app").unwrap().test_command = Some(Vec::new());
        assert!(bad.check().unwrap_err().contains("empty"));
        // An unknown field is refused, so a typo does not drop a setting.
        let typo = json!({"backend": "codex", "program": "/x", "timeout_secs": 1, "repos": {}, "sandbox": "off"});
        assert!(serde_json::from_value::<CodingConfig>(typo).is_err());
    }

    #[test]
    fn the_llm_sees_only_the_repository_names() {
        let config = config();
        let description = config.tool_description();
        assert!(description.contains("app") && !description.contains("/srv/app") && !description.contains("/bin/sh"));
        let schema = config.tool_schema().to_string();
        assert!(
            schema.contains("\"enum\":[\"app\"]") && !schema.contains("/srv"),
            "{schema}"
        );
    }

    #[test]
    fn a_call_needs_a_repository_and_instructions() {
        let args = |value: Value| value.as_object().unwrap().clone();
        let parsed = StartArgs::parse(&args(json!({"repo": "app", "instructions": " Fix it. "}))).unwrap();
        assert_eq!(
            parsed,
            StartArgs {
                repo: "app".into(),
                base_ref: "HEAD".into(),
                instructions: "Fix it.".into()
            }
        );
        assert!(StartArgs::parse(&args(json!({"instructions": "x"}))).is_err());
        assert!(StartArgs::parse(&args(json!({"repo": "app", "instructions": " "}))).is_err());
        assert!(
            StartArgs::parse(&args(
                json!({"repo": "app", "instructions": "x", "base_ref": "--upload-pack=x"})
            ))
            .is_err()
        );

        let spec = CodingJobSpec::new("kim", 12, &config(), parsed.clone()).unwrap();
        assert_eq!(spec.job_id, "b12-kim");
        assert_eq!(spec.repo_path, PathBuf::from("/srv/app"));
        assert_eq!(spec.test_command, Some(vec!["/usr/bin/make".into(), "test".into()]));
        let unknown = StartArgs {
            repo: "other".into(),
            ..parsed
        };
        assert!(
            CodingJobSpec::new("kim", 12, &config(), unknown)
                .unwrap_err()
                .contains("no repository")
        );
    }

    #[test]
    fn the_report_names_the_branch_and_says_the_owner_merges() {
        let outcome = CodingOutcome {
            branch: "agent/b12-kim".into(),
            base_commit: "a".repeat(40),
            head_commit: "b".repeat(40),
            diff_stat: " src/lib.rs | 2 +-".into(),
            test: Some(TestOutcome {
                command: vec!["/usr/bin/make".into(), "test".into()],
                exit_code: Some(2),
                timed_out: false,
                output: "FAILED".into(),
            }),
            exit_code: Some(0),
            signal: None,
            timed_out: false,
            summary: None,
            transcript: String::new(),
        };
        assert!(outcome.ended_well());
        let text = report_text("b12-kim", "app", &Ok(outcome.clone()));
        assert!(text.contains("ended with exit code 0"), "{text}");
        assert!(
            text.contains("Branch `agent/b12-kim`: base aaaaaaaaaaaa, head bbbbbbbbbbbb."),
            "{text}"
        );
        assert!(text.contains("src/lib.rs | 2 +-"));
        assert!(text.contains("`/usr/bin/make test` ended with exit code 2"));
        assert!(text.contains("merge it yourself"));
        let late = CodingOutcome {
            timed_out: true,
            exit_code: None,
            diff_stat: String::new(),
            test: None,
            ..outcome
        };
        assert!(!late.ended_well());
        let text = report_text("b12-kim", "app", &Ok(late));
        assert!(
            text.contains("stopped at its time limit")
                && text.contains("no change")
                && text.contains("no test command")
        );
        let failed = report_text("b12-kim", "app", &Err("branch agent/b12-kim already exists".into()));
        assert!(failed.contains("failed before it made a branch") && failed.contains("Nothing to merge"));
    }
}
