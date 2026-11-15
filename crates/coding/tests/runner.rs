//! Runner tests with fake backends. They never start the real claude or codex.

use bunsin_coding::{ClaudeCode, Codex, CodingBackend, CodingError, CodingJob, CodingResult, CodingRunner, Generic};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const BASE: &str = "trunk";

/// One temp repo, worktree root, output dir and script dir per test.
struct Fixture {
    dir: TempDir,
    repo: PathBuf,
    runner: CodingRunner,
    out: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", BASE]);
        git(&repo, &["config", "user.name", "Test User"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("file.txt"), "original\n").unwrap();
        git(&repo, &["add", "file.txt"]);
        git(&repo, &["commit", "-q", "-m", "initial"]);
        let runner = CodingRunner::new(dir.path().join("worktrees"));
        let out = dir.path().join("out");
        Fixture { dir, repo, runner, out }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// Writes an executable shell script and runs it once, so a later timing is not
    /// slowed by the first launch of a new executable (about 0.4 s on macOS).
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.path(name);
        std::fs::write(&path, format!("#!/bin/sh\n[ -n \"$WARMUP\" ] && exit 0\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // On Linux, a test on another thread can fork while this file is open for writing.
        // Its child holds the file open until it calls exec, and running the file then fails
        // with "Text file busy". The child lets go within milliseconds.
        let warmup = (0..50)
            .find_map(|_| match Command::new(&path).env("WARMUP", "1").status() {
                Err(error) if error.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                    None
                }
                result => Some(result.unwrap()),
            })
            .expect("the script stayed busy for a second");
        assert!(warmup.success());
        path
    }

    fn generic(&self, body: &str) -> Generic {
        Generic {
            program: self.script("agent.sh", body),
            args_template: vec!["--some-flag".into()],
        }
    }

    fn job(&self, id: &str) -> CodingJob {
        CodingJob {
            id: id.into(),
            repo: self.repo.clone(),
            base_ref: BASE.into(),
            instructions: "Change file.txt".into(),
            test_command: None,
            timeout: Duration::from_secs(60),
        }
    }

    /// Fails the test if the run hangs, for example when a killed process kept a pipe open.
    async fn run(&self, backend: &dyn CodingBackend, job: &CodingJob) -> CodingResult {
        tokio::time::timeout(Duration::from_secs(30), self.runner.run(backend, job, &self.out))
            .await
            .expect("run finished within 30 s")
            .unwrap()
    }

    fn agent_branches(&self) -> String {
        git(&self.repo, &["for-each-ref", "refs/heads/agent"])
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn read_pid(path: &Path) -> libc::pid_t {
    std::fs::read_to_string(path).unwrap().trim().parse().unwrap()
}

/// Waits up to 5 s for the process to disappear. A killed orphan is reaped by launchd.
fn process_is_gone(pid: libc::pid_t) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        // SAFETY: signal 0 only checks that the process exists.
        if unsafe { libc::kill(pid, 0) } != 0 {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[tokio::test]
async fn edit_makes_branch_and_leaves_user_checkout_alone() {
    let fx = Fixture::new();
    let user_head = git(&fx.repo, &["rev-parse", "HEAD"]);
    let stdin_copy = fx.path("stdin.txt");
    let backend = fx.generic(&format!(
        "cat > '{}'\nprintf 'changed\\n' > file.txt",
        stdin_copy.display()
    ));

    let result = fx.run(&backend, &fx.job("edit-1")).await;

    assert_eq!(result.branch, "agent/edit-1");
    assert_eq!(result.base_commit, user_head);
    assert_ne!(result.head_commit, result.base_commit);
    assert_eq!(
        git(&fx.repo, &["rev-parse", "refs/heads/agent/edit-1"]),
        result.head_commit
    );
    assert!(result.diff_stat.contains("file.txt"), "{}", result.diff_stat);
    assert_eq!(
        git(&fx.repo, &["show", &format!("{}:file.txt", result.head_commit)]),
        "changed"
    );
    assert_eq!(result.exit.code, Some(0));
    assert!(!result.exit.timed_out);
    // The instructions arrive on stdin.
    assert_eq!(std::fs::read_to_string(&stdin_copy).unwrap(), "Change file.txt");
    // The user's checkout is untouched.
    assert_eq!(git(&fx.repo, &["rev-parse", "HEAD"]), user_head);
    assert_eq!(git(&fx.repo, &["symbolic-ref", "--short", "HEAD"]), BASE);
    assert_eq!(git(&fx.repo, &["status", "--porcelain"]), "");
    assert_eq!(std::fs::read_to_string(fx.repo.join("file.txt")).unwrap(), "original\n");
}

#[tokio::test]
async fn no_change_keeps_head_at_base() {
    let fx = Fixture::new();
    let backend = fx.generic("echo nothing to do");

    let result = fx.run(&backend, &fx.job("noop")).await;

    assert_eq!(result.head_commit, result.base_commit);
    assert_eq!(result.diff_stat, "");
    assert_eq!(
        git(&fx.repo, &["rev-parse", "refs/heads/agent/noop"]),
        result.base_commit
    );
}

#[tokio::test]
async fn commits_made_by_the_backend_are_kept() {
    let fx = Fixture::new();
    let backend = fx.generic(
        "echo new > new.txt && git add new.txt && git commit -q -m 'agent commit'\n\
         echo extra > extra.txt",
    );

    let result = fx.run(&backend, &fx.job("self-commit")).await;

    let log = git(
        &fx.repo,
        &[
            "log",
            "--format=%s",
            &format!("{}..{}", result.base_commit, result.head_commit),
        ],
    );
    assert_eq!(log, "Uncommitted changes left by coding job self-commit\nagent commit");
    assert!(result.diff_stat.contains("new.txt"), "{}", result.diff_stat);
    assert!(result.diff_stat.contains("extra.txt"), "{}", result.diff_stat);
}

/// A fake Claude Code that claims success in the documented stream-json shape.
fn claiming_claude(fx: &Fixture, edit: &str) -> ClaudeCode {
    ClaudeCode {
        program: fx.script(
            "claude",
            &format!(
                "{edit}\n\
                 echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s-1\"}}'\n\
                 echo '{{\"type\":\"result\",\"subtype\":\"success\",\"is_error\":false,\"result\":\"All tests pass\",\"session_id\":\"s-1\"}}'"
            ),
        ),
    }
}

#[tokio::test]
async fn passing_test_is_reported_with_its_exit_code() {
    let fx = Fixture::new();
    let backend = claiming_claude(&fx, "printf 'fixed\\n' > file.txt");
    let mut job = fx.job("test-pass");
    job.test_command = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "grep -q fixed file.txt && echo PASS-OUT".into(),
    ]);

    let result = fx.run(&backend, &job).await;

    let test = result.test.unwrap();
    assert_eq!(test.exit_code, Some(0));
    assert!(!test.timed_out);
    assert_eq!(test.output, "PASS-OUT\n");
    assert_eq!(test.command, job.test_command.unwrap());
    assert_eq!(result.summary.as_deref(), Some("All tests pass"));
    assert_eq!(result.session_id.as_deref(), Some("s-1"));
}

#[tokio::test]
async fn failing_test_is_reported_even_when_backend_claims_success() {
    let fx = Fixture::new();
    let backend = claiming_claude(&fx, "printf 'still broken\\n' > file.txt");
    let mut job = fx.job("test-fail");
    job.test_command = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "grep -q fixed file.txt || { echo FAIL-OUT >&2; exit 3; }".into(),
    ]);

    let result = fx.run(&backend, &job).await;

    let test = result.test.unwrap();
    assert_eq!(test.exit_code, Some(3));
    assert_eq!(test.output, "FAIL-OUT\n");
    assert_eq!(result.summary.as_deref(), Some("All tests pass"));
    assert_eq!(result.exit.code, Some(0));
}

#[tokio::test]
async fn long_test_output_keeps_the_tail() {
    let fx = Fixture::new();
    let backend = fx.generic("true");
    let mut job = fx.job("long-output");
    job.test_command = Some(vec![
        "/bin/sh".into(),
        "-c".into(),
        "yes 가나다 | head -n 30000; echo LAST-LINE".into(),
    ]);

    let result = fx.run(&backend, &job).await;

    let output = result.test.unwrap().output;
    assert!(output.starts_with("[... "), "{}", &output[..40]);
    assert!(output.ends_with("가나다\nLAST-LINE\n"));
    assert!(output.len() < 70 * 1024, "{}", output.len());
    assert!(!output.contains('\u{FFFD}'));
}

#[tokio::test]
async fn timeout_kills_the_whole_process_group() {
    let fx = Fixture::new();
    let shell_pid = fx.path("shell.pid");
    let sleep_pid = fx.path("sleep.pid");
    let backend = fx.generic(&format!(
        "echo $$ > '{}'\nsleep 300 &\necho $! > '{}'\necho started\nwait",
        shell_pid.display(),
        sleep_pid.display()
    ));
    let mut job = fx.job("slow");
    job.timeout = Duration::from_secs(2);

    let result = fx.run(&backend, &job).await;

    assert!(result.exit.timed_out);
    assert_eq!(result.exit.code, None);
    assert_eq!(result.exit.signal, Some(libc::SIGKILL));
    assert!(process_is_gone(read_pid(&shell_pid)));
    assert!(process_is_gone(read_pid(&sleep_pid)));
    let transcript = std::fs::read_to_string(&result.transcript).unwrap();
    assert!(transcript.contains("started"), "{transcript}");
}

#[tokio::test]
async fn background_child_holding_output_is_killed_at_timeout() {
    let fx = Fixture::new();
    let sleep_pid = fx.path("sleep.pid");
    // The script exits at once, but its background child keeps stdout open.
    let backend = fx.generic(&format!("sleep 300 &\necho $! > '{}'\nexit 0", sleep_pid.display()));
    let mut job = fx.job("orphan");
    job.timeout = Duration::from_secs(2);

    let result = fx.run(&backend, &job).await;

    assert!(result.exit.timed_out);
    assert_eq!(result.exit.code, Some(0));
    assert!(process_is_gone(read_pid(&sleep_pid)));
}

#[tokio::test]
async fn nonzero_backend_exit_is_reported() {
    let fx = Fixture::new();
    let backend = fx.generic("echo partial > file.txt\necho 'agent crashed' >&2\nexit 7");

    let result = fx.run(&backend, &fx.job("crash")).await;

    assert_eq!(result.exit.code, Some(7));
    assert_eq!(result.exit.signal, None);
    assert!(!result.exit.timed_out);
    // The partial work is still on the branch.
    assert_ne!(result.head_commit, result.base_commit);
}

#[tokio::test]
async fn transcript_holds_stdout_and_stderr() {
    let fx = Fixture::new();
    let backend = fx.generic("echo OUT-MARKER\necho ERR-MARKER >&2");

    let result = fx.run(&backend, &fx.job("talk")).await;

    assert_eq!(result.transcript, fx.out.join("talk.transcript.log"));
    let transcript = std::fs::read_to_string(&result.transcript).unwrap();
    let stdout_at = transcript.find("OUT-MARKER").expect("stdout in transcript");
    let stderr_at = transcript.find("ERR-MARKER").expect("stderr in transcript");
    assert!(stdout_at < stderr_at);
    assert!(transcript.contains("--some-flag"), "{transcript}");
}

#[tokio::test]
async fn codex_runs_in_the_worktree_with_its_dir_flag() {
    let fx = Fixture::new();
    let seen = fx.path("seen.txt");
    let backend = Codex {
        program: fx.script(
            "codex",
            &format!(
                "{{ pwd -P; printf '%s\\n' \"$@\"; }} > '{}'\n\
                 echo '{{\"type\":\"thread.started\",\"thread_id\":\"t-9\"}}'\n\
                 echo '{{\"type\":\"item.completed\",\"item\":{{\"id\":\"item_0\",\"type\":\"agent_message\",\"text\":\"Done\"}}}}'",
                seen.display()
            ),
        ),
    };

    let result = fx.run(&backend, &fx.job("codex-1")).await;

    let workdir = fx.dir.path().join("worktrees").join("codex-1");
    let seen = std::fs::read_to_string(&seen).unwrap();
    let lines: Vec<&str> = seen.lines().collect();
    assert_eq!(
        lines[0],
        workdir.canonicalize().unwrap().to_str().unwrap(),
        "process cwd"
    );
    assert_eq!(lines[1..], ["exec", "--json", "-C", workdir.to_str().unwrap(), "-"]);
    assert_eq!(result.summary.as_deref(), Some("Done"));
    assert_eq!(result.session_id.as_deref(), Some("t-9"));
}

#[tokio::test]
async fn relative_program_is_rejected_before_any_git_write() {
    let fx = Fixture::new();
    let backend = Generic {
        program: "bin/agent".into(),
        args_template: Vec::new(),
    };

    let error = fx.runner.run(&backend, &fx.job("rel"), &fx.out).await;

    assert!(
        matches!(&error, Err(CodingError::RelativeProgram(p)) if p == Path::new("bin/agent")),
        "{error:?}"
    );
    assert_eq!(fx.agent_branches(), "");
    assert!(!fx.dir.path().join("worktrees").exists());
}

#[tokio::test]
async fn unsafe_job_ids_are_rejected() {
    let fx = Fixture::new();
    let backend = fx.generic("echo x > file.txt");

    for id in ["../x", "a b", ""] {
        let error = fx.runner.run(&backend, &fx.job(id), &fx.out).await;
        assert!(
            matches!(&error, Err(CodingError::InvalidJobId(bad)) if bad == id),
            "{id}: {error:?}"
        );
    }
    assert_eq!(fx.agent_branches(), "");
    assert!(!fx.dir.path().join("worktrees").exists());
    assert!(!fx.dir.path().join("x").exists());
}

#[tokio::test]
async fn bad_base_ref_is_a_git_error_and_makes_no_branch() {
    let fx = Fixture::new();
    let backend = fx.generic("true");

    for base_ref in ["no-such-ref", "--all"] {
        let mut job = fx.job("bad-base");
        job.base_ref = base_ref.into();
        let error = fx.runner.run(&backend, &job, &fx.out).await;
        assert!(
            matches!(&error, Err(CodingError::Git { args, .. }) if args.starts_with("rev-parse")),
            "{base_ref}: {error:?}"
        );
    }
    assert_eq!(fx.agent_branches(), "");
}

#[tokio::test]
async fn second_run_with_same_id_fails_and_keeps_the_first_branch() {
    let fx = Fixture::new();
    let first = fx.run(&fx.generic("echo one > file.txt"), &fx.job("dup")).await;
    let second_backend = fx.generic("echo two > file.txt");

    let error = fx.runner.run(&second_backend, &fx.job("dup"), &fx.out).await;

    assert!(
        matches!(&error, Err(CodingError::BranchExists(b)) if b == "agent/dup"),
        "{error:?}"
    );
    assert_eq!(git(&fx.repo, &["rev-parse", "refs/heads/agent/dup"]), first.head_commit);
    let worktree_file = fx.dir.path().join("worktrees/dup/file.txt");
    assert_eq!(std::fs::read_to_string(worktree_file).unwrap(), "one\n");
}

#[tokio::test]
async fn leftover_worktree_directory_is_not_reused() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.dir.path().join("worktrees/stale")).unwrap();

    let error = fx.runner.run(&fx.generic("true"), &fx.job("stale"), &fx.out).await;

    assert!(matches!(error, Err(CodingError::WorktreeExists(_))), "{error:?}");
    assert_eq!(fx.agent_branches(), "");
}

#[tokio::test]
async fn empty_test_command_is_rejected() {
    let fx = Fixture::new();
    let mut job = fx.job("empty-test");
    job.test_command = Some(Vec::new());

    let error = fx.runner.run(&fx.generic("true"), &job, &fx.out).await;

    assert!(matches!(error, Err(CodingError::EmptyTestCommand)), "{error:?}");
    assert_eq!(fx.agent_branches(), "");
}
