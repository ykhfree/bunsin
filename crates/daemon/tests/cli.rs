//! Runs the real `bunsind` binary. Each command is its own process, like a restart.

use std::path::Path;
use std::process::{Command, Output};

fn bunsind(data_dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bunsind"))
        .arg("--data-dir")
        .arg(data_dir)
        .args(args)
        .env_remove("BUNSIN_DATA_DIR")
        .output()
        .expect("bunsind starts")
}

fn ok(data_dir: &Path, args: &[&str]) -> String {
    let output = bunsind(data_dir, args);
    assert!(
        output.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn fails(data_dir: &Path, args: &[&str]) -> String {
    let output = bunsind(data_dir, args);
    assert_eq!(output.status.code(), Some(1), "{args:?} should fail");
    String::from_utf8(output.stderr).unwrap()
}

#[test]
fn profiles_are_created_and_listed() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(
        ok(dir.path(), &["profile-add", "kim", "--owner", "Kim"]),
        "created profile kim for Kim\n"
    );
    ok(dir.path(), &["profile-add", "lee", "--owner", "Lee"]);
    assert_eq!(ok(dir.path(), &["profile-list"]), "kim\tKim\nlee\tLee\n");
    assert!(
        fails(dir.path(), &["profile-add", "kim", "--owner", "Other"]).contains("already exists")
    );
    assert!(fails(dir.path(), &["profile-add", "../x", "--owner", "X"]).contains("invalid"));
}

#[test]
fn the_kill_switch_stays_on_across_processes_and_is_per_profile() {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["profile-add", "kim", "--owner", "Kim"]);
    ok(dir.path(), &["profile-add", "lee", "--owner", "Lee"]);
    assert!(ok(dir.path(), &["status", "kim"]).starts_with("kill switch: off\n"));
    assert_eq!(ok(dir.path(), &["stop", "kim", "on"]), "kill switch: on\n");
    assert!(ok(dir.path(), &["status", "kim"]).starts_with("kill switch: on\n"));
    assert!(ok(dir.path(), &["status", "lee"]).starts_with("kill switch: off\n"));
    assert_eq!(
        ok(dir.path(), &["stop", "kim", "off"]),
        "kill switch: off\n"
    );
    assert!(fails(dir.path(), &["stop", "nobody", "on"]).contains("not found"));
}

#[test]
fn settings_round_trip_through_a_file() {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["profile-add", "kim", "--owner", "Kim"]);
    let shown = ok(dir.path(), &["config-show", "kim"]);
    let mut config: serde_json::Value = serde_json::from_str(&shown).unwrap();
    assert_eq!(config["owner_name"], "Kim");
    // A new profile can do nothing: no tools and no rules.
    assert_eq!(config["policy"]["rules"], serde_json::json!({}));
    assert_eq!(config["approved_tools"], serde_json::json!({}));

    config["principles"] = serde_json::json!(["Answer in two sentences."]);
    let file = dir.path().join("kim.json");
    std::fs::write(&file, config.to_string()).unwrap();
    ok(dir.path(), &["config-set", "kim", file.to_str().unwrap()]);
    let again: serde_json::Value =
        serde_json::from_str(&ok(dir.path(), &["config-show", "kim"])).unwrap();
    assert_eq!(
        again["principles"],
        serde_json::json!(["Answer in two sentences."])
    );

    std::fs::write(&file, "{\"owner_name\": 1}").unwrap();
    assert!(
        fails(dir.path(), &["config-set", "kim", file.to_str().unwrap()])
            .contains("settings file is invalid")
    );
}

#[test]
fn ingest_without_llm_settings_fails_with_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["profile-add", "kim", "--owner", "Kim"]);
    let error = fails(
        dir.path(),
        &[
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
        ],
    );
    assert!(
        error.contains("the profile has no `llm` settings"),
        "{error}"
    );
}

#[test]
fn an_edit_needs_both_the_arguments_and_the_reason() {
    let dir = tempfile::tempdir().unwrap();
    ok(dir.path(), &["profile-add", "kim", "--owner", "Kim"]);
    let output = bunsind(dir.path(), &["approve", "kim", "1", "--args", "{}"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--reason"));
}
