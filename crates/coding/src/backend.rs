use crate::CodingJob;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The process a backend wants the runner to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandSpec {
    /// Must be absolute. A macOS GUI app has a minimal PATH without the user's CLIs.
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Added to the runner's environment.
    pub env: Vec<(String, String)>,
    /// Written to the process stdin, then stdin is closed. `None` gives an empty stdin.
    pub stdin: Option<String>,
}

/// What a backend says about its own run. The runner does not trust it for facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackendReport {
    pub summary: Option<String>,
    pub session_id: Option<String>,
}

/// A coding agent CLI. It only builds the command and reads the output.
pub trait CodingBackend: Send + Sync {
    /// `workdir` is the job worktree. The runner also sets it as the process cwd.
    fn command(&self, job: &CodingJob, workdir: &Path) -> CommandSpec;
    fn parse(&self, stdout: &str) -> BackendReport;
}

/// Claude Code in print mode with `stream-json` output.
/// The instructions go to stdin, so they are never parsed as flags.
/// No permission flag is passed, so the CLI's own permission settings apply.
#[derive(Debug, Clone)]
pub struct ClaudeCode {
    pub program: PathBuf,
}

impl CodingBackend for ClaudeCode {
    fn command(&self, job: &CodingJob, _workdir: &Path) -> CommandSpec {
        // Claude Code has no cwd flag. The runner sets the process cwd.
        CommandSpec {
            program: self.program.clone(),
            args: ["-p", "--output-format", "stream-json", "--verbose"]
                .map(String::from)
                .to_vec(),
            env: Vec::new(),
            stdin: Some(job.instructions.clone()),
        }
    }

    /// Reads the `session_id` of the `system/init` event and the `result` event.
    /// The last `result` event gives the summary.
    fn parse(&self, stdout: &str) -> BackendReport {
        let mut report = BackendReport::default();
        for event in json_lines(stdout) {
            let session_id = event["session_id"].as_str().map(String::from);
            match (event["type"].as_str(), event["subtype"].as_str()) {
                (Some("system"), Some("init")) => {
                    report.session_id = session_id.or(report.session_id);
                }
                (Some("result"), _) => {
                    report.session_id = session_id.or(report.session_id);
                    report.summary = event["result"].as_str().map(String::from);
                }
                _ => {}
            }
        }
        report
    }
}

/// Codex `exec` with JSONL output. The `-` argument makes Codex read the instructions from stdin.
/// No sandbox or approval flag is passed, so the CLI's own defaults apply.
#[derive(Debug, Clone)]
pub struct Codex {
    pub program: PathBuf,
}

impl CodingBackend for Codex {
    fn command(&self, job: &CodingJob, workdir: &Path) -> CommandSpec {
        CommandSpec {
            program: self.program.clone(),
            args: vec![
                "exec".into(),
                "--json".into(),
                "-C".into(),
                workdir.to_string_lossy().into_owned(),
                "-".into(),
            ],
            env: Vec::new(),
            stdin: Some(job.instructions.clone()),
        }
    }

    /// Reads `thread_id` of `thread.started` and the text of the last `agent_message` item.
    fn parse(&self, stdout: &str) -> BackendReport {
        let mut report = BackendReport::default();
        for event in json_lines(stdout) {
            match event["type"].as_str() {
                Some("thread.started") => {
                    report.session_id = event["thread_id"].as_str().map(String::from);
                }
                Some("item.completed") if event["item"]["type"] == "agent_message" => {
                    report.summary = event["item"]["text"].as_str().map(String::from);
                }
                _ => {}
            }
        }
        report
    }
}

/// A user-defined command. The instructions go to stdin.
/// `args_template` is passed unchanged: no placeholder is expanded.
#[derive(Debug, Clone)]
pub struct Generic {
    pub program: PathBuf,
    pub args_template: Vec<String>,
}

impl CodingBackend for Generic {
    fn command(&self, job: &CodingJob, _workdir: &Path) -> CommandSpec {
        CommandSpec {
            program: self.program.clone(),
            args: self.args_template.clone(),
            env: Vec::new(),
            stdin: Some(job.instructions.clone()),
        }
    }

    /// The output format is unknown, so nothing is extracted.
    fn parse(&self, _stdout: &str) -> BackendReport {
        BackendReport::default()
    }
}

/// Parses each line that is a JSON object. Other lines are skipped.
fn json_lines(stdout: &str) -> impl Iterator<Item = Value> + '_ {
    stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(Value::is_object)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn job() -> CodingJob {
        CodingJob {
            id: "job-1".into(),
            repo: "/repo".into(),
            base_ref: "main".into(),
            instructions: "--dangerously-skip-permissions fix the bug".into(),
            test_command: None,
            timeout: Duration::from_secs(60),
        }
    }

    const BYPASS_FLAGS: [&str; 4] = [
        "--dangerously-skip-permissions",
        "--allow-dangerously-skip-permissions",
        "--dangerously-bypass-approvals-and-sandbox",
        "bypassPermissions",
    ];

    fn assert_no_bypass(spec: &CommandSpec) {
        for flag in BYPASS_FLAGS {
            assert!(
                !spec.args.iter().any(|arg| arg.contains(flag)),
                "{flag} in {:?}",
                spec.args
            );
        }
    }

    #[test]
    fn claude_code_command_puts_instructions_on_stdin() {
        let backend = ClaudeCode {
            program: "/opt/bin/claude".into(),
        };
        let spec = backend.command(&job(), Path::new("/work/job-1"));
        assert_eq!(spec.program, PathBuf::from("/opt/bin/claude"));
        assert_eq!(
            spec.args,
            ["-p", "--output-format", "stream-json", "--verbose"]
        );
        assert_eq!(spec.stdin.as_deref(), Some(job().instructions.as_str()));
        assert!(spec.env.is_empty());
        assert_no_bypass(&spec);
    }

    #[test]
    fn codex_command_names_workdir_and_reads_stdin() {
        let backend = Codex {
            program: "/opt/bin/codex".into(),
        };
        let spec = backend.command(&job(), Path::new("/work/job-1"));
        assert_eq!(spec.program, PathBuf::from("/opt/bin/codex"));
        assert_eq!(spec.args, ["exec", "--json", "-C", "/work/job-1", "-"]);
        assert_eq!(spec.stdin.as_deref(), Some(job().instructions.as_str()));
        assert_no_bypass(&spec);
    }

    #[test]
    fn generic_command_passes_args_unchanged() {
        let backend = Generic {
            program: "/opt/bin/agent".into(),
            args_template: vec!["run".into(), "{instructions}".into()],
        };
        let spec = backend.command(&job(), Path::new("/work/job-1"));
        assert_eq!(spec.args, ["run", "{instructions}"]);
        assert_eq!(spec.stdin.as_deref(), Some(job().instructions.as_str()));
        assert_eq!(
            backend.parse("{\"result\":\"done\"}"),
            BackendReport::default()
        );
    }

    // Hand-written sample in the documented stream-json shape. It is not a capture of a real run.
    const CLAUDE_SAMPLE: &str = r#"{"type":"system","subtype":"init","session_id":"sess-init","model":"m","tools":["Edit"]}
{"type":"assistant","message":{"content":[{"type":"text","text":"Working on it"}]},"session_id":"sess-init"}
not json: a stray stderr line
{"type":"result","subtype":"success","is_error":false,"result":"Fixed the bug in auth.rs","session_id":"sess-final"}
"#;

    #[test]
    fn claude_code_parse_reads_result_event() {
        let backend = ClaudeCode {
            program: "/opt/bin/claude".into(),
        };
        let report = backend.parse(CLAUDE_SAMPLE);
        assert_eq!(report.summary.as_deref(), Some("Fixed the bug in auth.rs"));
        assert_eq!(report.session_id.as_deref(), Some("sess-final"));
    }

    #[test]
    fn claude_code_parse_keeps_init_session_when_no_result() {
        let backend = ClaudeCode {
            program: "/opt/bin/claude".into(),
        };
        let first_line = CLAUDE_SAMPLE.lines().next().unwrap();
        let report = backend.parse(first_line);
        assert_eq!(report.summary, None);
        assert_eq!(report.session_id.as_deref(), Some("sess-init"));
        assert_eq!(backend.parse(""), BackendReport::default());
    }

    // Hand-written sample in the documented `codex exec --json` shape. It is not a capture.
    const CODEX_SAMPLE: &str = r#"{"type":"thread.started","thread_id":"0199a213-81c0-7800-8aa1-bbab2a035a53"}
{"type":"turn.started"}
{"type":"item.completed","item":{"id":"item_0","type":"reasoning","text":"Thinking"}}
{"type":"item.completed","item":{"id":"item_1","type":"agent_message","text":"First draft"}}
{"type":"item.completed","item":{"id":"item_2","type":"command_execution","command":"ls","aggregated_output":"a"}}
{"type":"item.completed","item":{"id":"item_3","type":"agent_message","text":"Renamed the function"}}
{"type":"turn.completed","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5,"reasoning_output_tokens":1}}
"#;

    #[test]
    fn codex_parse_reads_thread_and_last_agent_message() {
        let backend = Codex {
            program: "/opt/bin/codex".into(),
        };
        let report = backend.parse(CODEX_SAMPLE);
        assert_eq!(report.summary.as_deref(), Some("Renamed the function"));
        assert_eq!(
            report.session_id.as_deref(),
            Some("0199a213-81c0-7800-8aa1-bbab2a035a53")
        );
    }

    #[test]
    fn codex_parse_ignores_other_item_types() {
        let backend = Codex {
            program: "/opt/bin/codex".into(),
        };
        let only_reasoning = CODEX_SAMPLE.lines().nth(2).unwrap();
        assert_eq!(backend.parse(only_reasoning), BackendReport::default());
    }
}
