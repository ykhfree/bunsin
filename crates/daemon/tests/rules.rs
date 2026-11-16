//! The permission rules on the command line: the real `bunsind` against a real server,
//! and the rule functions against a real engine.

use bunsin_api::{ApiError, EngineApi, Expected, Permissions, PermissionsUpdate, ProfileConfig};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::tool::ToolSpec;
use bunsin_daemon::rules::{change, parse_rule, save_rule, table_lines};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::Engine;
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::sync::Arc;

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
    // No test may touch the real Keychain.
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
    assert!(!output.status.success(), "{what} should fail");
    String::from_utf8(output.stderr).unwrap()
}

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

fn spec(name: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: format!("The {name} tool"),
        input_schema: json!({"type": "object", "properties": {"text": {"type": "string"}}}),
    }
}

fn slack_tools() -> Vec<ToolSpec> {
    vec![spec("post"), spec("search"), spec("remove")]
}

/// Gives the profile the approved Slack tools, through `config-set`.
fn approve_tools(server: &Server, token: &str, profile: &str, dir: &Path) {
    let shown = stdout_of(server.run(token, &["config-show", profile]), "config-show");
    let mut config: serde_json::Value = serde_json::from_str(&shown).unwrap();
    config["approved_tools"] = json!({"slack": slack_tools()});
    let file = dir.join(format!("{profile}.json"));
    std::fs::write(&file, config.to_string()).unwrap();
    stdout_of(
        server.run(token, &["config-set", profile, file.to_str().unwrap()]),
        "config-set",
    );
}

fn stored_rules(server: &Server, token: &str, profile: &str) -> serde_json::Value {
    let shown = stdout_of(server.run(token, &["config-show", profile]), "config-show");
    serde_json::from_str::<serde_json::Value>(&shown).unwrap()["policy"]["rules"].clone()
}

#[test]
fn rules_are_shown_set_and_removed_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let server = Server::start(dir.path());
    approve_tools(&server, &kim, "kim", dir.path());
    assert_eq!(
        stdout_of(server.run(&kim, &["rules", "kim"]), "rules"),
        "slack/post\toff\nslack/search\toff\nslack/remove\toff\n"
    );

    // A write without a text choice is refused before the server is asked.
    let error = stderr_of_failure(
        server.run(&kim, &["rule-set", "kim", "slack/post", "--class", "write"]),
        "a write without a text choice",
    );
    assert!(
        error.contains("--text <place>") && error.contains("--no-text"),
        "{error}"
    );
    assert_eq!(stored_rules(&server, &kim, "kim"), json!({}));

    let set = |args: &[&str]| {
        let mut all = vec!["rule-set", "kim"];
        all.extend_from_slice(args);
        stdout_of(server.run(&kim, &all), "rule-set")
    };
    assert_eq!(
        set(&[
            "slack/post",
            "--class",
            "write",
            "--text",
            "text",
            "--text",
            "blocks.*.text.text"
        ]),
        "rule of slack/post in kim saved\n"
    );
    set(&["slack/search", "--class", "read"]);
    set(&["slack/remove", "--class", "delete", "--tier", "draft", "--no-text"]);
    // A rule for a tool that is not approved yet is stored and marked.
    set(&["jira/create", "--class", "write", "--text", "summary"]);
    assert_eq!(
        stored_rules(&server, &kim, "kim"),
        json!({
            "jira/create": {"class": "write", "tier": "approve", "outgoing_text": {"arg": "summary"}},
            "slack/post": {"class": "write", "tier": "approve",
                           "outgoing_text": {"paths": ["text", "blocks.*.text.text"]}},
            "slack/remove": {"class": "delete", "tier": "draft", "outgoing_text": "none"},
            "slack/search": {"class": "read", "tier": "autonomous", "outgoing_text": "none"}
        })
    );
    assert_eq!(
        stdout_of(server.run(&kim, &["rules", "kim"]), "rules"),
        "jira/create\twrite\tapprove\ttext in summary\t(the tool is not approved)\n\
         slack/post\twrite\tapprove\ttext in text, blocks.*.text.text\n\
         slack/remove\tdelete\tdraft\tno text\n\
         slack/search\tread\n"
    );

    // A change of one rule keeps the others.
    set(&[
        "slack/post",
        "--class",
        "write",
        "--tier",
        "autonomous",
        "--text",
        "text",
    ]);
    let rules = stored_rules(&server, &kim, "kim");
    assert_eq!(
        rules["slack/post"],
        json!({"class": "write", "tier": "autonomous", "outgoing_text": {"arg": "text"}})
    );
    assert_eq!(rules.as_object().unwrap().len(), 4);

    assert_eq!(
        stdout_of(server.run(&kim, &["rule-remove", "kim", "slack/search"]), "rule-remove"),
        "rule of slack/search in kim removed\n"
    );
    let error = stderr_of_failure(
        server.run(&kim, &["rule-remove", "kim", "slack/search"]),
        "a second removal",
    );
    assert!(error.contains("`slack/search` has no rule"), "{error}");
    let rules = stored_rules(&server, &kim, "kim");
    assert!(
        rules.get("slack/search").is_none() && rules.get("slack/post").is_some(),
        "{rules}"
    );
}

#[test]
fn wrong_rules_and_other_profiles_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let kim = add_profile(dir.path(), "kim", "Kim");
    let lee = add_profile(dir.path(), "lee", "Lee");
    let server = Server::start(dir.path());
    let refused = |token: &str, args: &[&str], reason: &str| {
        let error = stderr_of_failure(server.run(token, args), reason);
        assert!(error.contains(reason), "{args:?}: {error}");
    };
    refused(
        &kim,
        &[
            "rule-set",
            "kim",
            "slack/remove",
            "--class",
            "delete",
            "--tier",
            "autonomous",
            "--no-text",
        ],
        "a delete always asks",
    );
    refused(
        &kim,
        &[
            "rule-set",
            "kim",
            "slack/post",
            "--class",
            "write",
            "--text",
            "blocks..text",
        ],
        "the path `blocks..text` is invalid",
    );
    refused(
        &kim,
        &["rule-set", "kim", "slack/search", "--class", "read", "--no-text"],
        "a read rule has no step",
    );
    refused(
        &kim,
        &["rule-set", "kim", "post", "--class", "read"],
        "is not a tool ID",
    );
    refused(&kim, &["rule-remove", "kim", "slack/"], "is not a tool ID");
    // Text places and "no text" exclude each other.
    let both = server.run(
        &kim,
        &[
            "rule-set",
            "kim",
            "slack/post",
            "--class",
            "write",
            "--text",
            "text",
            "--no-text",
        ],
    );
    assert!(!both.status.success());
    // Lee's token does not reach Kim's rules.
    refused(
        &lee,
        &["rule-set", "kim", "slack/search", "--class", "read"],
        "not authorized",
    );
    refused(&lee, &["rules", "kim"], "not authorized");
    assert_eq!(stored_rules(&server, &kim, "kim"), json!({}));
}

fn open_engine(dir: &Path) -> Engine {
    let backends = RealBackends::new(Arc::new(|_| None), false, UpstreamPolicy::default());
    Engine::open(dir, Arc::new(backends), Arc::new(|| 1_000)).unwrap()
}

#[tokio::test]
async fn a_rule_change_expects_the_table_it_read_and_a_change_in_between_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let engine = open_engine(dir.path());
    let mut config = ProfileConfig::new("Kim");
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    engine.create_profile("kim", config).await.unwrap();

    let read = engine.permissions("kim").await.unwrap().rules;
    let rule = parse_rule("write", None, &["text".into()], false).unwrap();
    let update = change(&read, "slack/post", Some(rule.clone())).unwrap();
    assert_eq!(update.expected, Expected::Value(read.clone()));
    assert_eq!(update.rules.len(), 2);
    assert_eq!(update.rules["slack/search"], ToolRule::read());

    // Someone else saves a table after the read. The update from the old read is refused.
    let other = PermissionsUpdate {
        rules: BTreeMap::new(),
        expected: Expected::Unchecked,
    };
    engine.set_permissions("kim", other).await.unwrap();
    let error = engine.set_permissions("kim", update).await.unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    assert!(engine.permissions("kim").await.unwrap().rules.is_empty());

    // `save_rule` reads the table itself, so it works on the table as it is now.
    save_rule(&engine, "kim", "slack/post", Some(rule.clone()))
        .await
        .unwrap();
    let now = engine.permissions("kim").await.unwrap().rules;
    assert_eq!(now, BTreeMap::from([("slack/post".to_owned(), rule)]));
    let error = save_rule(&engine, "kim", "slack/other", None).await.unwrap_err();
    assert!(error.contains("has no rule"), "{error}");
}

#[test]
fn options_become_rules_in_the_stored_forms() {
    let texts = |list: &[&str]| list.iter().map(|t| (*t).to_owned()).collect::<Vec<_>>();
    let write = |tier, outgoing_text| ToolRule {
        class: ToolClass::Write,
        tier,
        outgoing_text,
    };
    // One argument at the top is stored as `arg`, as the window does it.
    assert_eq!(
        parse_rule("write", None, &texts(&["text"]), false),
        Ok(write(Tier::Approve, OutgoingText::Arg("text".into())))
    );
    // One place inside a value, or several places, are stored as `paths`.
    assert_eq!(
        parse_rule("write", Some("draft"), &texts(&["blocks.*.text"]), false),
        Ok(write(Tier::Draft, OutgoingText::Paths(texts(&["blocks.*.text"]))))
    );
    assert_eq!(
        parse_rule("write", Some("observe"), &texts(&["text", "title"]), false),
        Ok(write(Tier::Observe, OutgoingText::Paths(texts(&["text", "title"]))))
    );
    assert_eq!(parse_rule("read", None, &[], false), Ok(ToolRule::read()));
    assert!(parse_rule("read", Some("draft"), &[], false).is_err());
    assert!(parse_rule("read", None, &texts(&["text"]), false).is_err());
    assert!(parse_rule("write", None, &texts(&["text"]), true).is_err());
    assert!(
        parse_rule("delete", None, &[], false)
            .unwrap_err()
            .contains("--no-text")
    );
    assert!(parse_rule("delete", Some("autonomous"), &[], true).is_err());
    assert!(parse_rule("delete", Some("approve"), &[], true).is_ok());
    assert!(
        parse_rule("execute", None, &[], true)
            .unwrap_err()
            .contains("is not a class")
    );
    assert!(
        parse_rule("write", Some("soon"), &[], true)
            .unwrap_err()
            .contains("is not a step")
    );
}

#[test]
fn the_table_lists_rules_then_approved_tools_without_a_rule() {
    let permissions = Permissions {
        tools: BTreeMap::from([("slack".to_owned(), vec![spec("post"), spec("search")])]),
        rules: BTreeMap::from([(
            "slack/post".to_owned(),
            ToolRule::write(ToolClass::Delete, OutgoingText::None),
        )]),
    };
    assert_eq!(
        table_lines(&permissions),
        ["slack/post\tdelete\tapprove\tno text", "slack/search\toff"]
    );
}
