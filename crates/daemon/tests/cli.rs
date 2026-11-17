//! Runs the real `bunsind` binary: one server process and separate command processes.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

fn bunsind() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bunsind"));
    for name in [
        "BUNSIN_DATA_DIR",
        "BUNSIN_SERVER",
        "BUNSIN_TOKEN",
        "BUNSIN_CA_CERT",
        "BUNSIN_MASTER_KEY_FILE",
    ] {
        command.env_remove(name);
    }
    // No test may touch the real Keychain. On macOS the server would use it by default, and
    // a question of the Keychain would hang a test run that nobody watches.
    command.env("BUNSIN_KEY_STORE", "file");
    command
}

fn stdout_of(output: Output, what: &str) -> String {
    assert!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn stderr_of_failure(output: Output, what: &str) -> String {
    assert_eq!(output.status.code(), Some(1), "{what} should fail");
    String::from_utf8(output.stderr).unwrap()
}

/// Runs an operator command on the data directory.
fn operator(data_dir: &Path, args: &[&str]) -> Output {
    bunsind()
        .arg(args[0])
        .arg("--data-dir")
        .arg(data_dir)
        .args(&args[1..])
        .output()
        .unwrap()
}

/// A running `bunsind serve`. The process is killed when this is dropped.
struct Server {
    child: Child,
    url: String,
}

impl Server {
    fn start(data_dir: &Path) -> Self {
        let mut child = bunsind()
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(data_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let url = line
            .trim()
            .strip_prefix("listening on ")
            .unwrap_or_else(|| panic!("unexpected: {line:?}"));
        Self {
            url: url.to_owned(),
            child,
        }
    }

    /// Runs a remote command against this server.
    fn run(&self, token: &str, args: &[&str]) -> Output {
        bunsind()
            .arg(args[0])
            .arg("--server")
            .arg(&self.url)
            .args(&args[1..])
            .env("BUNSIN_TOKEN", token)
            .output()
            .unwrap()
    }

    /// Asks the server to stop and waits for it, like a service manager does.
    fn stop(mut self) {
        Command::new("kill")
            .arg("-TERM")
            .arg(self.child.id().to_string())
            .status()
            .unwrap();
        assert!(self.child.wait().unwrap().success(), "the server did not stop cleanly");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn add_profile(data_dir: &Path, id: &str, owner: &str) -> String {
    stdout_of(
        operator(data_dir, &["profile-add", id, "--owner", owner]),
        "profile-add",
    );
    stdout_of(operator(data_dir, &["token-add", id, "--label", "test"]), "token-add")
        .trim()
        .to_owned()
}

#[test]
fn an_owner_controls_only_the_own_profile_through_the_server() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    // The operator adds a profile while the server runs.
    let lee = add_profile(dir.path(), "lee", "Lee");

    assert_eq!(
        stdout_of(server.run(&kim, &["profile-list"]), "profile-list"),
        "kim\tKim\n"
    );
    assert_eq!(
        stdout_of(server.run(&lee, &["profile-list"]), "profile-list"),
        "lee\tLee\n"
    );
    assert!(stdout_of(server.run(&kim, &["status", "kim"]), "status").starts_with("kill switch: off\n"));
    assert_eq!(
        stdout_of(server.run(&kim, &["stop", "kim", "on"]), "stop"),
        "kill switch: on\n"
    );
    assert!(stdout_of(server.run(&lee, &["status", "lee"]), "status").starts_with("kill switch: off\n"));

    let error = stderr_of_failure(server.run(&kim, &["stop", "lee", "on"]), "stop for another profile");
    assert!(error.contains("not authorized"), "{error}");
    // A new profile has made no LLM call: the table has only its head line.
    assert_eq!(
        stdout_of(server.run(&kim, &["llm-usage", "kim"]), "llm-usage"),
        "day\tpurpose\tcalls\tfailed\tinput tokens\toutput tokens\taverage ms\n"
    );
    let error = stderr_of_failure(server.run(&kim, &["llm-usage", "lee"]), "llm-usage for another profile");
    assert!(error.contains("not authorized"), "{error}");
    assert!(stdout_of(server.run(&lee, &["status", "lee"]), "status").starts_with("kill switch: off\n"));
}

#[test]
fn the_kill_switch_stays_on_after_the_server_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let first = Server::start(dir.path());
    stdout_of(first.run(&kim, &["stop", "kim", "on"]), "stop");
    first.stop();

    let second = Server::start(dir.path());
    assert!(stdout_of(second.run(&kim, &["status", "kim"]), "status").starts_with("kill switch: on\n"));
}

#[test]
fn a_revoked_or_missing_token_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    assert!(server.run(&kim, &["status", "kim"]).status.success());

    let listed = stdout_of(operator(dir.path(), &["token-list"]), "token-list");
    let fields: Vec<&str> = listed.trim().split('\t').collect();
    assert_eq!(&fields[1..], ["kim", "test"]);
    // The list never shows the token itself.
    assert!(!listed.contains(&kim));
    assert_eq!(
        stdout_of(operator(dir.path(), &["token-revoke", fields[0]]), "token-revoke"),
        format!("token {} removed\n", fields[0])
    );

    let error = stderr_of_failure(server.run(&kim, &["status", "kim"]), "status with a revoked token");
    assert!(error.contains("not authorized"), "{error}");
    assert!(
        stderr_of_failure(operator(dir.path(), &["token-revoke", fields[0]]), "second revoke").contains("no token")
    );

    let no_token = bunsind()
        .args(["status", "kim", "--server", &server.url])
        .output()
        .unwrap();
    assert!(stderr_of_failure(no_token, "status without a token").contains("BUNSIN_TOKEN"));
}

#[test]
fn settings_round_trip_and_unsafe_settings_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    let shown = stdout_of(server.run(&kim, &["config-show", "kim"]), "config-show");
    let mut config: serde_json::Value = serde_json::from_str(&shown).unwrap();
    // A new profile can do nothing: no tools and no rules.
    assert_eq!(config["policy"]["rules"], serde_json::json!({}));
    assert_eq!(config["approved_tools"], serde_json::json!({}));

    let file = dir.path().join("kim.json");
    config["principles"] = serde_json::json!(["Answer in two sentences."]);
    std::fs::write(&file, config.to_string()).unwrap();
    stdout_of(
        server.run(&kim, &["config-set", "kim", file.to_str().unwrap()]),
        "config-set",
    );
    let again: serde_json::Value =
        serde_json::from_str(&stdout_of(server.run(&kim, &["config-show", "kim"]), "config-show")).unwrap();
    assert_eq!(again["principles"], serde_json::json!(["Answer in two sentences."]));

    // The real server refuses a connector that would run a command on it.
    config["connectors"] = serde_json::json!({
        "shell": {"mcp": {"transport": "stdio", "command": "/bin/sh", "args": [], "env": {}}}
    });
    std::fs::write(&file, config.to_string()).unwrap();
    let error = stderr_of_failure(
        server.run(&kim, &["config-set", "kim", file.to_str().unwrap()]),
        "stdio settings",
    );
    assert!(error.contains("--allow-stdio-connectors"), "{error}");

    // The settings file cannot name a token header that the connection sets itself.
    config["connectors"] = serde_json::json!({
        "wiki": {"mcp": {"transport": "http", "url": "https://mcp.example.com/wiki"}, "token_secret": null,
                 "token_header": "Host"}
    });
    std::fs::write(&file, config.to_string()).unwrap();
    let error = stderr_of_failure(
        server.run(&kim, &["config-set", "kim", file.to_str().unwrap()]),
        "a reserved token header",
    );
    assert!(error.contains("cannot carry a token"), "{error}");
    let stored = stdout_of(server.run(&kim, &["config-show", "kim"]), "config-show");
    assert!(!stored.contains("token_header"), "{stored}");
    // Another header name is accepted in the file.
    config["connectors"]["wiki"]["token_header"] = serde_json::json!("X-API-Key");
    std::fs::write(&file, config.to_string()).unwrap();
    stdout_of(
        server.run(&kim, &["config-set", "kim", file.to_str().unwrap()]),
        "config-set with a token header",
    );
}

#[test]
fn ingest_without_llm_settings_fails_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    let args = [
        "ingest",
        "kim",
        "--source",
        "slack",
        "--event-id",
        "e1",
        "--author",
        "lee",
        "--text",
        "hi",
    ];
    let error = stderr_of_failure(server.run(&kim, &args), "ingest");
    assert!(error.contains("the profile has no `llm` settings"), "{error}");
}

#[test]
fn operator_commands_check_their_input() {
    let dir = tempfile::tempdir().unwrap();
    add_profile(dir.path(), "kim", "Kim");
    assert!(
        stderr_of_failure(
            operator(dir.path(), &["profile-add", "kim", "--owner", "Other"]),
            "duplicate"
        )
        .contains("already exists")
    );
    assert!(
        stderr_of_failure(operator(dir.path(), &["profile-add", "../x", "--owner", "X"]), "bad id").contains("invalid")
    );
    assert!(
        stderr_of_failure(
            operator(dir.path(), &["token-add", "nobody", "--label", "x"]),
            "no profile"
        )
        .contains("does not exist")
    );
}

#[test]
fn the_server_refuses_a_public_address_without_tls_and_a_second_instance() {
    let dir = tempfile::tempdir().unwrap();
    let public = operator(dir.path(), &["serve", "--listen", "0.0.0.0:0"]);
    assert!(stderr_of_failure(public, "public serve").contains("TLS is required"));

    let _running = Server::start(dir.path());
    let second = operator(dir.path(), &["serve", "--listen", "127.0.0.1:0"]);
    assert!(stderr_of_failure(second, "second serve").contains("another engine"));
}

#[test]
fn a_connector_takes_a_header_for_its_token_and_needs_no_token() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    let set = ["connector-set", "kim", "wiki", "--url", "https://mcp.example.com/wiki"];
    // No token and no header: the connector is saved without a token.
    stdout_of(server.run(&kim, &set), "connector-set");
    assert_eq!(
        stdout_of(server.run(&kim, &["connections", "kim"]), "connections"),
        "llm\tnot set\nwiki\thttps://mcp.example.com/wiki\tno token\n"
    );
    let with_header = [&set[..], &["--token-header", "X-API-Key"]].concat();
    stdout_of(server.run(&kim, &with_header), "connector-set with a header");
    assert_eq!(
        stdout_of(server.run(&kim, &["connections", "kim"]), "connections"),
        "llm\tnot set\nwiki\thttps://mcp.example.com/wiki\tno token\tin the header X-API-Key\n"
    );
    let reserved = [&set[..], &["--token-header", "Content-Type"]].concat();
    let error = stderr_of_failure(server.run(&kim, &reserved), "a reserved header");
    assert!(error.contains("cannot carry a token"), "{error}");
}

#[test]
fn llm_test_reports_why_the_endpoint_does_not_work() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    add_profile(dir.path(), "lee", "Lee");
    let server = Server::start(dir.path());
    // The profile has no endpoint yet. The command fails with the reason.
    let error = stderr_of_failure(server.run(&kim, &["llm-test", "kim"]), "llm-test without settings");
    assert!(
        error.contains("the endpoint does not work: the LLM cannot be used: the profile has no `llm` settings"),
        "{error}"
    );
    // Another format than Claude's has no default address or model.
    let error = stderr_of_failure(
        server.run(&kim, &["llm-set", "kim", "--wire", "openai-responses"]),
        "llm-set without an address",
    );
    assert!(error.contains("give --base-url: this format has no default"), "{error}");
    let set = [
        "llm-set",
        "kim",
        "--wire",
        "openai-responses",
        "--base-url",
        "https://llm.example.com/v1",
    ];
    let error = stderr_of_failure(server.run(&kim, &set), "llm-set without a model");
    assert!(error.contains("give --model: this format has no default"), "{error}");
    stdout_of(server.run(&kim, &[&set[..], &["--model", "m"]].concat()), "llm-set");
    assert!(
        stdout_of(server.run(&kim, &["connections", "kim"]), "connections")
            .starts_with("llm\topenai-responses\thttps://llm.example.com/v1\tm\tno token\n")
    );
    // The Claude format keeps its defaults.
    stdout_of(server.run(&kim, &["llm-set", "kim"]), "llm-set with defaults");
    assert!(
        stdout_of(server.run(&kim, &["connections", "kim"]), "connections")
            .starts_with("llm\tanthropic-messages\thttps://api.anthropic.com\tclaude-opus-5-5\t")
    );
    // A token tests only its own profile.
    let error = stderr_of_failure(server.run(&kim, &["llm-test", "lee"]), "llm-test for another profile");
    assert!(error.contains("not authorized"), "{error}");
}

#[test]
fn a_key_store_that_cannot_work_stops_the_start_before_any_key_is_read() {
    let dir = tempfile::tempdir().unwrap();
    // These are refused from the options alone. No key file is made and no Keychain is asked.
    let unknown = operator(
        dir.path(),
        &["serve", "--listen", "127.0.0.1:0", "--key-store", "vault"],
    );
    assert!(stderr_of_failure(unknown, "an unknown key store").contains("not a key store"));
    let key_file = dir.path().join("other.key");
    let both = operator(
        dir.path(),
        &[
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--key-store",
            "keychain",
            "--master-key-file",
            key_file.to_str().unwrap(),
        ],
    );
    assert!(stderr_of_failure(both, "a key file with the keychain store").contains("--master-key-file"));
    // A public address without TLS is refused before the key is made.
    let public = operator(dir.path(), &["serve", "--listen", "0.0.0.0:0"]);
    stderr_of_failure(public, "a public address without TLS");
    assert!(!dir.path().join("master.key").exists() && !key_file.exists());
    assert!(!dir.path().join("keychain-id").exists());
}

#[test]
fn a_start_marks_a_cut_call_unknown_and_the_owner_resolves_it() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    // The last server stopped while it ran this call.
    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    let body = bunsin_core::records::InboxBody::ToolCall {
        tool: "slack/post".into(),
        args: serde_json::json!({"text": "hi"}).as_object().unwrap().clone(),
    };
    let item = store
        .add_inbox_item(
            "k1",
            &body,
            "asked",
            &[],
            bunsin_core::approval::ApprovalState::Executing,
            1,
        )
        .unwrap();
    drop(store);

    let server = Server::start(dir.path());
    let id = item.id.to_string();
    let status = stdout_of(server.run(&kim, &["status", "kim"]), "status");
    assert!(status.contains(&format!("#{id} [unknown] slack/post")), "{status}");
    assert_eq!(
        stdout_of(
            server.run(&kim, &["resolve-unknown", "kim", &id, "drop"]),
            "resolve-unknown"
        ),
        format!("item {id} resolved\n")
    );
    let status = stdout_of(server.run(&kim, &["status", "kim"]), "status");
    assert!(status.contains(&format!("#{id} [failed] slack/post")), "{status}");
    let error = stderr_of_failure(
        server.run(&kim, &["resolve-unknown", "kim", &id, "done"]),
        "a second resolve",
    );
    assert!(error.contains("not unknown"), "{error}");
}

#[test]
fn the_owner_lists_and_retires_corrections() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    let correction = bunsin_core::context::Correction {
        tool: "slack/post".into(),
        source: "slack".into(),
        task: "when is the deploy?".into(),
        original: "It is on Friday.\nI think.".into(),
        edited: "Thursday.".into(),
        reason: "it moved".into(),
    };
    let id = store.add_correction(4, &correction, 1).unwrap().to_string();
    drop(store);

    let server = Server::start(dir.path());
    let listed = stdout_of(server.run(&kim, &["corrections", "kim"]), "corrections");
    assert_eq!(
        listed,
        format!(
            "#{id} slack/post from slack\n  task: when is the deploy?\n  draft: It is on Friday.\n    I think.\n  \
             edit: Thursday.\n  reason: it moved\n"
        )
    );
    assert_eq!(
        stdout_of(
            server.run(&kim, &["retire-correction", "kim", &id]),
            "retire-correction"
        ),
        format!("correction {id} retired\n")
    );
    let listed = stdout_of(server.run(&kim, &["corrections", "kim"]), "corrections");
    assert!(
        listed.starts_with(&format!("#{id} slack/post from slack (retired)\n")),
        "{listed}"
    );
    let error = stderr_of_failure(server.run(&kim, &["retire-correction", "kim", &id]), "a second retire");
    assert!(error.contains("already retired"), "{error}");
}

#[test]
fn the_owner_reports_an_autonomous_call_and_clears_the_hold() {
    use bunsin_core::approval::ApprovalState;
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    // A call that the policy ran without the owner.
    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    let body = bunsin_core::records::InboxBody::ToolCall {
        tool: "slack/post".into(),
        args: serde_json::json!({"text": "hi"}).as_object().unwrap().clone(),
    };
    let item = store
        .add_inbox_item("k1", &body, "asked", &[], ApprovalState::Approved, 1)
        .unwrap();
    for (from, to) in [
        (ApprovalState::Approved, ApprovalState::Executing),
        (ApprovalState::Executing, ApprovalState::Executed),
    ] {
        assert!(store.move_inbox_state(item.id, from, to, 1).unwrap());
    }
    store
        .add_decision(
            1,
            Some(item.id),
            "slack/post",
            bunsin_core::records::DecisionKind::Autonomous,
        )
        .unwrap();
    drop(store);

    let server = Server::start(dir.path());
    let id = item.id.to_string();
    let header = "thresholds: none, so no tool is suggested; the counts cover all days\n\
                  tool\treviewed\tunchanged\tedited\trejected\tautonomous\tfailed\tstate\n";
    // The profile has no write rule and no hold yet.
    assert_eq!(stdout_of(server.run(&kim, &["autonomy", "kim"]), "autonomy"), header);
    assert_eq!(
        stdout_of(
            server.run(&kim, &["report-autonomous", "kim", &id]),
            "report-autonomous"
        ),
        format!("item {id} reported; its tool asks you until you clear the hold\n")
    );
    // The hold starts the counts again.
    assert_eq!(
        stdout_of(server.run(&kim, &["autonomy", "kim"]), "autonomy"),
        format!("{header}slack/post\t0\t0 (-)\t0\t0\t0\t0\theld\n")
    );
    let error = stderr_of_failure(server.run(&kim, &["report-autonomous", "kim", &id]), "a second report");
    assert!(error.contains("reported before"), "{error}");
    assert_eq!(
        stdout_of(
            server.run(&kim, &["clear-autonomy-hold", "kim", "slack/post"]),
            "clear-autonomy-hold"
        ),
        "hold of slack/post cleared\n"
    );
    let error = stderr_of_failure(
        server.run(&kim, &["clear-autonomy-hold", "kim", "slack/post"]),
        "a second clear",
    );
    assert!(error.contains("not held"), "{error}");
}

#[test]
fn the_owner_adds_replay_cases_and_runs_an_evaluation() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let file = dir.path().join("cases.json");
    let cases = serde_json::json!([
        {"event": {"source": "slack", "external_id": "c1", "thread": null, "author": "lee",
                   "text": "when do we deploy?\nasap"},
         "actual_reply": "At 3 pm."},
        {"event": {"source": "slack", "external_id": "c2", "thread": null, "author": "lee",
                   "text": "can I get a raise?"},
         "actual_reply": "", "expect": "escalate"}
    ]);
    std::fs::write(&file, cases.to_string()).unwrap();
    let path = file.to_str().unwrap();

    let server = Server::start(dir.path());
    assert_eq!(
        stdout_of(server.run(&kim, &["add-replay-cases", "kim", path]), "add-replay-cases"),
        "2 case(s) added: #1 #2\n"
    );
    assert_eq!(
        stdout_of(server.run(&kim, &["replay-cases", "kim"]), "replay-cases"),
        "#1 reply from slack by lee\n  event: when do we deploy?\n    asap\n  reply: At 3 pm.\n\
         #2 escalate from slack by lee\n  event: can I get a raise?\n  reply: \n"
    );
    assert_eq!(
        stdout_of(server.run(&kim, &["eval-report", "kim"]), "eval-report"),
        "no evaluation has run\n"
    );
    assert_eq!(
        stdout_of(server.run(&kim, &["start-eval", "kim"]), "start-eval"),
        "evaluation run 1 started; see it with eval-report\n"
    );
    // The profile has no LLM, so each replay fails. The server's background task still ends the run.
    let mut report = String::new();
    for _ in 0..60 {
        report = stdout_of(server.run(&kim, &["eval-report", "kim"]), "eval-report");
        if report.starts_with("run 1: done") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    let mut lines = report.lines();
    assert_eq!(lines.next(), Some("run 1: done"), "{report}");
    assert_eq!(lines.next(), Some("cases: 2, failed: 2"));
    assert_eq!(
        lines.next(),
        Some("kind -\toverlap -\tboundary -\tover-escalation -\tread -")
    );
    assert_eq!(lines.next(), Some("pass mark: none"));
    assert!(
        report.contains("#2\tescalate\tfailed: the LLM cannot be used"),
        "{report}"
    );

    assert_eq!(
        stdout_of(
            server.run(&kim, &["delete-replay-case", "kim", "1"]),
            "delete-replay-case"
        ),
        "replay case 1 deleted\n"
    );
    let error = stderr_of_failure(server.run(&kim, &["delete-replay-case", "kim", "1"]), "a second delete");
    assert!(error.contains("does not exist"), "{error}");
    let bad = dir.path().join("bad.json");
    std::fs::write(&bad, "{}").unwrap();
    let error = stderr_of_failure(
        server.run(&kim, &["add-replay-cases", "kim", bad.to_str().unwrap()]),
        "a file that is no list",
    );
    assert!(error.contains("is not a list of replay cases"), "{error}");
}

#[test]
fn the_owner_extracts_adopts_and_dismisses_principle_candidates() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    let first = store
        .add_principle_candidate("Ask first.", "ask me\nfirst", "corrections", 5)
        .unwrap()
        .to_string();
    let second = store
        .add_principle_candidate("Be brief.", "short", "read:slack/search", 6)
        .unwrap()
        .to_string();
    drop(store);

    let server = Server::start(dir.path());
    assert_eq!(
        stdout_of(
            server.run(&kim, &["principle-candidates", "kim"]),
            "principle-candidates"
        ),
        format!(
            "#{second} new from read:slack/search\n  text: Be brief.\n  evidence: short\n\
             #{first} new from corrections\n  text: Ask first.\n  evidence: ask me\n    first\n"
        )
    );
    assert_eq!(
        stdout_of(
            server.run(&kim, &["adopt-principle", "kim", &first, "--text", "Ask me first."]),
            "adopt-principle"
        ),
        format!("candidate {first} added to the principles\n")
    );
    assert_eq!(
        stdout_of(
            server.run(&kim, &["dismiss-principle", "kim", &second]),
            "dismiss-principle"
        ),
        format!("candidate {second} dismissed\n")
    );
    let error = stderr_of_failure(
        server.run(&kim, &["adopt-principle", "kim", &second]),
        "adopt a dismissed one",
    );
    assert!(error.contains("dismissed already"), "{error}");
    let config = stdout_of(server.run(&kim, &["config-show", "kim"]), "config-show");
    let config: serde_json::Value = serde_json::from_str(&config).unwrap();
    assert_eq!(config["principles"], serde_json::json!(["Ask me first."]));

    // The checks of the options run before the server is asked.
    let error = stderr_of_failure(
        server.run(&kim, &["extract-principles", "kim", "--from", "read"]),
        "read without a tool",
    );
    assert!(error.contains("needs --tool"), "{error}");
    // The profile has no correction yet.
    let error = stderr_of_failure(
        server.run(&kim, &["extract-principles", "kim", "--from", "corrections"]),
        "no texts",
    );
    assert!(error.contains("has no text"), "{error}");
}

#[test]
fn the_owner_checks_the_notes_and_dismisses_a_finding() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    let error = stderr_of_failure(server.run(&kim, &["lint-notes", "kim"]), "checks off");
    assert!(error.contains("checks of the notes are off"), "{error}");

    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    let text = store.setting("config").unwrap().unwrap();
    let mut config: serde_json::Value = serde_json::from_str(&text).unwrap();
    config["notes"] = serde_json::json!({"stale_days": 1});
    store.set_setting("config", &config.to_string()).unwrap();
    let page = bunsin_core::wiki::WikiText {
        title: "Deploy".into(),
        summary: "When".into(),
        body: "Fridays.".into(),
    };
    // A page from long ago: the server's clock is the real one.
    store
        .write_wiki_page("deploy", &page, &[], bunsin_core::wiki::WikiAuthor::Owner, 0, None, 1)
        .unwrap();
    drop(store);

    let out = stdout_of(server.run(&kim, &["lint-notes", "kim"]), "lint-notes");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "2 finding(s)");
    assert!(lines[1].contains(" stale deploy: nobody changed it for "), "{out}");
    assert!(lines[2].contains(" orphan deploy: no other page names it"), "{out}");
    let id = lines[1].trim_start_matches('#').split(' ').next().unwrap();
    assert_eq!(
        stdout_of(server.run(&kim, &["dismiss-finding", "kim", id]), "dismiss-finding"),
        format!("finding {id} dismissed\n")
    );
    let left = stdout_of(server.run(&kim, &["lint-findings", "kim"]), "lint-findings");
    assert_eq!(left, format!("{}\n", lines[2]));
}

#[test]
fn a_preload_needs_a_tool_with_a_read_rule() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    let error = stderr_of_failure(
        server.run(
            &kim,
            &["preload-notes", "kim", "--tool", "slack/search", "--args", "[1]"],
        ),
        "args not an object",
    );
    assert!(error.contains("--args is not a JSON object"), "{error}");
    let error = stderr_of_failure(
        server.run(&kim, &["preload-notes", "kim", "--tool", "slack/search"]),
        "no read rule",
    );
    assert!(error.contains("has no rule with the class `read`"), "{error}");
}

#[test]
fn coding_needs_both_operator_flags() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_str().unwrap();
    for flags in [
        vec!["--allow-coding"],
        vec!["--coding-repo-root", root],
        vec!["--coding-program", "codex=/usr/local/bin/codex"],
        vec!["--allow-coding-commands"],
    ] {
        let output = operator(dir.path(), &[["serve"].as_slice(), flags.as_slice()].concat());
        // clap refuses the arguments before the server starts.
        assert_eq!(output.status.code(), Some(2), "{flags:?}");
        let error = String::from_utf8(output.stderr).unwrap();
        assert!(error.contains("required"), "{error}");
    }
    // A custom program is no operator program: it needs --allow-coding-commands and the profile's settings.
    let output = operator(
        dir.path(),
        &[
            "serve",
            "--allow-coding",
            "--coding-repo-root",
            root,
            "--coding-program",
            "command=/bin/sh",
        ],
    );
    let error = stderr_of_failure(output, "a custom program");
    assert!(error.contains("not a backend"), "{error}");
}
