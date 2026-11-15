//! Connectors that the server starts as a command, with the real backends.
//! No test starts a process: the command does not exist, and no tool is listed or called.

use bunsin_api::{ApiError, CommandConnectorUpdate, ConnectorUpdate, EngineApi, Expected, ProfileConfig, TokenChange};
use bunsin_core::settings::McpConfig;
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::Engine;
use serde_json::json;
use std::sync::Arc;

const SECRET: &str = "env-secret-5b21";

async fn engine(dir: &std::path::Path, allow_stdio: bool) -> Engine {
    let backends = Arc::new(RealBackends::new(
        Arc::new(|_| None),
        allow_stdio,
        UpstreamPolicy::default(),
    ));
    let engine = Engine::open(dir, backends, Arc::new(|| 1_000)).unwrap();
    engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
    engine
}

fn command(path: &str) -> CommandConnectorUpdate {
    CommandConnectorUpdate {
        command: path.into(),
        args: vec!["-y".into(), "some-mcp-server".into()],
        env: [("API_TOKEN".to_owned(), SECRET.to_owned())].into(),
        expected: Expected::Unchecked,
    }
}

fn address(url: &str) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.into(),
        token: TokenChange::Keep,
        token_header: None,
        expected: Expected::Unchecked,
    }
}

#[tokio::test]
async fn an_allowed_command_connector_is_stored_and_not_started() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true).await;
    engine
        .set_command_connector("kim", "local", command("/opt/mcp/does-not-exist"))
        .await
        .unwrap();
    let shown = engine.connections("kim").await.unwrap().connectors["local"].clone();
    assert_eq!(
        shown.mcp,
        McpConfig::Stdio {
            command: "/opt/mcp/does-not-exist".into(),
            args: vec!["-y".into(), "some-mcp-server".into()],
            env: [("API_TOKEN".to_owned(), SECRET.to_owned())].into(),
        }
    );
    // The audit log names the connector. It holds no argument and no value of a variable.
    let audit = engine.snapshot("kim").await.unwrap().audit;
    assert_eq!(audit[0].kind, "connection_changed");
    assert_eq!(audit[0].detail, json!({"target": "local", "kind": "command"}));
    assert!(!format!("{audit:?}").contains(SECRET));

    // A second save changes it, and a save that started from the first one is refused.
    let mut changed = command("/opt/mcp/other");
    changed.expected = Expected::Value(Some(shown.clone()));
    engine.set_command_connector("kim", "local", changed).await.unwrap();
    let mut stale = command("/opt/mcp/third");
    stale.expected = Expected::Value(Some(shown));
    let error = engine.set_command_connector("kim", "local", stale).await.unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    engine.remove_connector("kim", "local").await.unwrap();
    assert!(engine.connections("kim").await.unwrap().connectors.is_empty());
}

#[tokio::test]
async fn without_the_flag_of_the_operator_nothing_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), false).await;
    let error = engine
        .set_command_connector("kim", "local", command("/opt/mcp/does-not-exist"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("--allow-stdio-connectors")),
        "{error:?}"
    );
    assert!(!error.to_string().contains(SECRET));
    assert!(engine.connections("kim").await.unwrap().connectors.is_empty());
}

#[tokio::test]
async fn a_relative_command_and_a_bad_variable_name_are_refused_in_every_path() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true).await;
    let error = engine
        .set_command_connector("kim", "local", command("npx"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("absolute path")),
        "{error:?}"
    );
    for name in ["", "1TOKEN", "API TOKEN", "API-TOKEN", "A=B"] {
        let mut update = command("/opt/mcp/does-not-exist");
        update.env = [(name.to_owned(), SECRET.to_owned())].into();
        let error = engine.set_command_connector("kim", "local", update).await.unwrap_err();
        assert!(
            matches!(&error, ApiError::Invalid(m) if m.contains("environment variable")),
            "{name}: {error:?}"
        );
        assert!(!error.to_string().contains(SECRET));
    }
    let error = engine
        .set_command_connector("kim", "Bad Name", command("/opt/mcp/does-not-exist"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("connector name")),
        "{error:?}"
    );
    assert!(engine.connections("kim").await.unwrap().connectors.is_empty());

    // The settings file passes the same checks.
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.insert(
        "local".into(),
        json!({"mcp": {"transport": "stdio", "command": "npx", "args": [], "env": {}}, "token_secret": null}),
    );
    let error = engine.set_profile_config("kim", config.clone()).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("absolute path")),
        "{error:?}"
    );
    config.connectors.insert(
        "local".into(),
        json!({"mcp": {"transport": "stdio", "command": "/opt/mcp/x", "args": [], "env": {"1X": "v"}},
               "token_secret": null}),
    );
    let error = engine.set_profile_config("kim", config).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("environment variable")),
        "{error:?}"
    );
}

#[tokio::test]
async fn a_name_keeps_its_kind_until_the_connector_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true).await;
    engine
        .set_connector("kim", "wiki", address("https://mcp.example.com/wiki"))
        .await
        .unwrap();
    let error = engine
        .set_command_connector("kim", "wiki", command("/opt/mcp/does-not-exist"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("has an address")),
        "{error:?}"
    );
    assert!(matches!(
        engine.connections("kim").await.unwrap().connectors["wiki"].mcp,
        McpConfig::Http { .. }
    ));

    engine
        .set_command_connector("kim", "local", command("/opt/mcp/does-not-exist"))
        .await
        .unwrap();
    let error = engine
        .set_connector("kim", "local", address("https://mcp.example.com/local"))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("runs a command")),
        "{error:?}"
    );
    assert!(matches!(
        engine.connections("kim").await.unwrap().connectors["local"].mcp,
        McpConfig::Stdio { .. }
    ));
    // After a removal the name is free for the other kind.
    engine.remove_connector("kim", "local").await.unwrap();
    engine
        .set_connector("kim", "local", address("https://mcp.example.com/local"))
        .await
        .unwrap();
}
