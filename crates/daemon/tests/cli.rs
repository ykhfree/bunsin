//! Runs the real `bunsind` binary: one server process and separate command processes.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};

fn bunsind() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bunsind"));
    for name in ["BUNSIN_DATA_DIR", "BUNSIN_SERVER", "BUNSIN_TOKEN", "BUNSIN_CA_CERT"] {
        command.env_remove(name);
    }
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
    // A token tests only its own profile.
    let error = stderr_of_failure(server.run(&kim, &["llm-test", "lee"]), "llm-test for another profile");
    assert!(error.contains("not authorized"), "{error}");
}
