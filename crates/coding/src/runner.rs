use crate::{CodingBackend, CodingError, CodingJob, CodingResult, ExitInfo, TestRun};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::{Instant, timeout_at};

/// Test output longer than this keeps only its last bytes.
const MAX_TEST_OUTPUT_BYTES: usize = 64 * 1024;

/// Runs coding jobs in git worktrees. It never merges and never pushes.
#[derive(Debug, Clone)]
pub struct CodingRunner {
    /// Each job gets the worktree `<worktree_root>/<job id>`.
    /// Put it outside the user's repository. The runner does not remove worktrees.
    worktree_root: PathBuf,
}

impl CodingRunner {
    pub fn new(worktree_root: impl Into<PathBuf>) -> Self {
        Self {
            worktree_root: worktree_root.into(),
        }
    }

    /// Runs `job` with `backend` on the new branch `agent/<job id>`.
    /// The transcript goes to `<out_dir>/<job id>.transcript.log`.
    /// A timeout or a non-zero exit is reported in `CodingResult::exit`, not as an error.
    pub async fn run(
        &self,
        backend: &dyn CodingBackend,
        job: &CodingJob,
        out_dir: &Path,
    ) -> Result<CodingResult, CodingError> {
        // Check everything that needs no git write first.
        validate_job_id(&job.id)?;
        if job.test_command.as_ref().is_some_and(Vec::is_empty) {
            return Err(CodingError::EmptyTestCommand);
        }
        let workdir = std::path::absolute(self.worktree_root.join(&job.id))?;
        let spec = backend.command(job, &workdir);
        if !spec.program.is_absolute() {
            return Err(CodingError::RelativeProgram(spec.program));
        }

        let branch = format!("agent/{}", job.id);
        let base_commit = git(
            &job.repo,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{commit}}", job.base_ref),
            ],
        )
        .await?;
        let branch_ref = format!("refs/heads/{branch}");
        if git(&job.repo, &["show-ref", "--verify", "--quiet", &branch_ref])
            .await
            .is_ok()
        {
            return Err(CodingError::BranchExists(branch));
        }
        if workdir.exists() {
            return Err(CodingError::WorktreeExists(workdir));
        }
        // `worktree add` leaves the user's checkout and current branch alone.
        let workdir_arg = workdir.to_string_lossy();
        git(
            &job.repo,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                &branch,
                &workdir_arg,
                &base_commit,
            ],
        )
        .await?;

        let run = run_process(
            &spec.program,
            &spec.args,
            &spec.env,
            spec.stdin,
            &workdir,
            job.timeout,
        )
        .await?;
        std::fs::create_dir_all(out_dir)?;
        let transcript = out_dir.join(format!("{}.transcript.log", job.id));
        std::fs::write(
            &transcript,
            transcript_text(&spec.program, &spec.args, &run),
        )?;

        commit_leftovers(&workdir, &job.id).await?;
        let head_commit = git(&workdir, &["rev-parse", "HEAD"]).await?;
        let diff_stat = git(&workdir, &["diff", "--stat", &base_commit, &head_commit]).await?;

        let test = match &job.test_command {
            Some(command) => Some(run_test(command, &workdir, job.timeout).await?),
            None => None,
        };

        let report = backend.parse(&String::from_utf8_lossy(&run.stdout));
        Ok(CodingResult {
            branch,
            base_commit,
            head_commit,
            diff_stat,
            test,
            exit: run.exit,
            transcript,
            summary: report.summary,
            session_id: report.session_id,
        })
    }
}

/// Allows 1-64 ASCII letters, digits, `-` and `_`, with a letter or digit first.
/// No dots, so the id cannot form `..` or `.lock` in a branch or path.
fn validate_job_id(id: &str) -> Result<(), CodingError> {
    let mut chars = id.chars();
    let valid = id.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if valid {
        Ok(())
    } else {
        Err(CodingError::InvalidJobId(id.to_string()))
    }
}

/// Runs `git -C <dir> <args>` and returns its trimmed stdout.
async fn git(dir: &Path, args: &[&str]) -> Result<String, CodingError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|source| CodingError::Spawn {
            program: "git".into(),
            source,
        })?;
    if !output.status.success() {
        return Err(CodingError::Git {
            args: args.join(" "),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Commits what the backend left uncommitted, so the result is always a branch.
/// Hooks and signing are off: a hook failure or a pinentry prompt must not lose the work.
async fn commit_leftovers(workdir: &Path, job_id: &str) -> Result<(), CodingError> {
    if git(workdir, &["status", "--porcelain"]).await?.is_empty() {
        return Ok(());
    }
    git(workdir, &["add", "-A"]).await?;
    let message = format!("Uncommitted changes left by coding job {job_id}");
    git(
        workdir,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-m",
            &message,
        ],
    )
    .await?;
    Ok(())
}

async fn run_test(
    command: &[String],
    workdir: &Path,
    timeout: Duration,
) -> Result<TestRun, CodingError> {
    let run = run_process(
        Path::new(&command[0]),
        &command[1..],
        &[],
        None,
        workdir,
        timeout,
    )
    .await?;
    let mut output = run.stdout;
    output.extend_from_slice(&run.stderr);
    Ok(TestRun {
        command: command.to_vec(),
        exit_code: run.exit.code,
        timed_out: run.exit.timed_out,
        output: tail(&output, MAX_TEST_OUTPUT_BYTES),
    })
}

struct ProcessOutput {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit: ExitInfo,
}

/// Runs a process in its own process group.
/// When `timeout` passes, the whole group gets SIGKILL.
/// The timeout also covers children that keep stdout or stderr open after the main process exits.
async fn run_process(
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    stdin: Option<String>,
    cwd: &Path,
    timeout: Duration,
) -> Result<ProcessOutput, CodingError> {
    let deadline = Instant::now() + timeout;
    let mut child = Command::new(program)
        .args(args)
        .envs(env.iter().map(|(key, value)| (key, value)))
        .current_dir(cwd)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|source| CodingError::Spawn {
            program: program.to_path_buf(),
            source,
        })?;
    // With `process_group(0)` the group id is the child's pid.
    let group = child.id().expect("a just spawned child has a pid") as libc::pid_t;

    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        tokio::spawn(async move {
            // A process may exit without reading stdin. The write error is expected then.
            let _ = pipe.write_all(text.as_bytes()).await;
        });
    }
    let stdout = tokio::spawn(read_all(child.stdout.take().expect("stdout is piped")));
    let stderr = tokio::spawn(read_all(child.stderr.take().expect("stderr is piped")));

    let mut timed_out = false;
    let status = match timeout_at(deadline, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            timed_out = true;
            kill_group(group);
            child.wait().await?
        }
    };

    let streams = async { (stdout.await, stderr.await) };
    tokio::pin!(streams);
    let (stdout, stderr) = if timed_out {
        streams.await
    } else {
        match timeout_at(deadline, streams.as_mut()).await {
            Ok(streams) => streams,
            Err(_) => {
                // A child of the process still holds a pipe. It is in the same group.
                timed_out = true;
                kill_group(group);
                streams.await
            }
        }
    };

    Ok(ProcessOutput {
        stdout: stdout.expect("reader task does not panic")?,
        stderr: stderr.expect("reader task does not panic")?,
        exit: ExitInfo {
            code: status.code(),
            signal: status.signal(),
            timed_out,
        },
    })
}

fn kill_group(group: libc::pid_t) {
    // SAFETY: killpg only sends a signal. An error (no such group) is not a problem here.
    unsafe {
        libc::killpg(group, libc::SIGKILL);
    }
}

async fn read_all(mut reader: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

fn transcript_text(program: &Path, args: &[String], run: &ProcessOutput) -> String {
    format!(
        "$ {} {}\n== stdout ==\n{}\n== stderr ==\n{}\n== exit: code={:?} signal={:?} timed_out={} ==\n",
        program.display(),
        args.join(" "),
        String::from_utf8_lossy(&run.stdout),
        String::from_utf8_lossy(&run.stderr),
        run.exit.code,
        run.exit.signal,
        run.exit.timed_out,
    )
}

/// Keeps the last `limit` bytes of `bytes`, cut at a UTF-8 character start.
fn tail(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        return String::from_utf8_lossy(bytes).into_owned();
    }
    let mut cut = bytes.len() - limit;
    // Skip UTF-8 continuation bytes, so the kept part starts on a whole character.
    while cut < bytes.len() && bytes[cut] & 0b1100_0000 == 0b1000_0000 {
        cut += 1;
    }
    format!(
        "[... {cut} bytes truncated ...]\n{}",
        String::from_utf8_lossy(&bytes[cut..])
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_id_allowlist() {
        for id in ["a", "job-1", "A_b-9", &"x".repeat(64)] {
            assert!(validate_job_id(id).is_ok(), "{id}");
        }
        for id in [
            "",
            "../x",
            "a b",
            "-a",
            "_a",
            "a.lock",
            "a/b",
            "a..b",
            "한글",
            &"x".repeat(65),
        ] {
            assert!(
                matches!(validate_job_id(id), Err(CodingError::InvalidJobId(_))),
                "{id}"
            );
        }
    }

    #[test]
    fn tail_keeps_short_output() {
        assert_eq!(tail(b"ok\n", 10), "ok\n");
    }

    #[test]
    fn tail_keeps_last_bytes_on_char_boundary() {
        // Each '가' is 3 bytes. A 10-byte limit cuts inside a character.
        let text = "가".repeat(10) + "END";
        let kept = tail(text.as_bytes(), 10);
        assert!(!kept.contains('\u{FFFD}'), "{kept}");
        assert!(kept.ends_with("가가END"), "{kept}");
        assert!(kept.starts_with("[... 24 bytes truncated ...]\n"), "{kept}");
    }
}
