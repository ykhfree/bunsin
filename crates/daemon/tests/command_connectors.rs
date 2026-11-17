//! Connectors that the server starts as a command, with the real backends.
//! No test starts a process: the command does not exist, and no tool is listed or called.

use bunsin_api::{
    ApiError, CommandConnectorUpdate, ConnectorUpdate, EngineApi, Expected, ProfileConfig, SecretChange, TokenChange,
};
use bunsin_core::settings::{ConnectorKind, McpConfig};
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
        secret_env: Default::default(),
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
        shown.kind,
        ConnectorKind::Mcp(McpConfig::Stdio {
            command: "/opt/mcp/does-not-exist".into(),
            args: vec!["-y".into(), "some-mcp-server".into()],
            env: [("API_TOKEN".to_owned(), SECRET.to_owned())].into(),
        })
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
        engine.connections("kim").await.unwrap().connectors["wiki"].kind,
        ConnectorKind::Mcp(McpConfig::Http { .. })
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
        engine.connections("kim").await.unwrap().connectors["local"].kind,
        ConnectorKind::Mcp(McpConfig::Stdio { .. })
    ));
    // After a removal the name is free for the other kind.
    engine.remove_connector("kim", "local").await.unwrap();
    engine
        .set_connector("kim", "local", address("https://mcp.example.com/local"))
        .await
        .unwrap();
}

/// Every file below `dir`, as one text. A value that is stored in plain text is in it.
fn stored_text(dir: &std::path::Path) -> String {
    let mut text = String::new();
    let mut folders = vec![dir.to_owned()];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else {
                text.push_str(&String::from_utf8_lossy(&std::fs::read(path).unwrap()));
            }
        }
    }
    text
}

fn keys(dir: &std::path::Path) -> Vec<String> {
    let store = bunsin_store::AppStore::open(dir).unwrap().open_profile("kim").unwrap();
    store.secret_keys().unwrap()
}

/// A command with one plain variable and the secret variables `changes`.
fn with_secrets(changes: &[(&str, SecretChange)]) -> CommandConnectorUpdate {
    CommandConnectorUpdate {
        env: [("REGION".to_owned(), "eu".to_owned())].into(),
        secret_env: changes
            .iter()
            .map(|(name, change)| ((*name).to_owned(), change.clone()))
            .collect(),
        ..command("/opt/mcp/does-not-exist")
    }
}

fn set(value: &str) -> SecretChange {
    SecretChange::Set(value.to_owned())
}

#[tokio::test]
async fn a_secret_variable_is_stored_encrypted_and_only_its_name_is_shown() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true).await;
    let update = with_secrets(&[("API_TOKEN", set(SECRET)), ("lower_key", set("second-value-88"))]);
    engine.set_command_connector("kim", "local", update).await.unwrap();

    let shown = engine.connections("kim").await.unwrap().connectors["local"].clone();
    assert_eq!(shown.secret_env, ["API_TOKEN", "lower_key"]);
    // The value is in no answer of the server and in no file in plain text.
    let answers = format!(
        "{shown:?} {:?} {:?}",
        engine.profile_config("kim").await.unwrap(),
        engine.snapshot("kim").await.unwrap()
    );
    assert!(
        !answers.contains(SECRET) && !answers.contains("second-value-88"),
        "{answers}"
    );
    let files = stored_text(dir.path());
    assert!(files.contains("API_TOKEN") && !files.contains(SECRET) && !files.contains("second-value-88"));
    assert_eq!(keys(dir.path()).len(), 2);

    // A later save keeps a value without sending it again, and removes the variable that is left out.
    let kept = with_secrets(&[("API_TOKEN", SecretChange::Keep)]);
    engine.set_command_connector("kim", "local", kept).await.unwrap();
    assert_eq!(
        engine.connections("kim").await.unwrap().connectors["local"].secret_env,
        ["API_TOKEN"]
    );
    assert_eq!(keys(dir.path()).len(), 1);

    // The removal of the connector removes the value.
    engine.remove_connector("kim", "local").await.unwrap();
    assert!(keys(dir.path()).is_empty());
}

#[tokio::test]
async fn a_refused_save_stores_no_value() {
    let dir = tempfile::tempdir().unwrap();
    // Without the flag of the operator.
    let refused = engine(dir.path(), false).await;
    let update = with_secrets(&[("API_TOKEN", set(SECRET))]);
    let error = refused.set_command_connector("kim", "local", update).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("--allow-stdio-connectors")),
        "{error:?}"
    );
    assert!(keys(dir.path()).is_empty());
    drop(refused);

    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true).await;
    let cases = [
        // A value to keep that was never stored.
        (with_secrets(&[("API_TOKEN", SecretChange::Keep)]), "no value is stored"),
        (with_secrets(&[("API_TOKEN", set(""))]), "is empty"),
        (with_secrets(&[("1BAD", set(SECRET))]), "environment variable"),
        // The same name as a plain variable.
        (with_secrets(&[("REGION", set(SECRET))]), "in both lists"),
    ];
    for (update, reason) in cases {
        let error = engine.set_command_connector("kim", "local", update).await.unwrap_err();
        assert!(
            matches!(&error, ApiError::Invalid(m) if m.contains(reason)),
            "{reason}: {error:?}"
        );
        assert!(!error.to_string().contains(SECRET));
        assert!(keys(dir.path()).is_empty(), "{reason}");
        assert!(engine.connections("kim").await.unwrap().connectors.is_empty());
    }
    // A good value and a bad one in the same save: neither is stored.
    let mixed = with_secrets(&[("API_TOKEN", set(SECRET)), ("OTHER", SecretChange::Keep)]);
    assert!(engine.set_command_connector("kim", "local", mixed).await.is_err());
    assert!(keys(dir.path()).is_empty());
    // A save that started from another state of the connector stores no value either.
    engine
        .set_command_connector("kim", "local", with_secrets(&[]))
        .await
        .unwrap();
    let mut stale = with_secrets(&[("API_TOKEN", set(SECRET))]);
    stale.expected = Expected::Value(None);
    let error = engine.set_command_connector("kim", "local", stale).await.unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    assert!(keys(dir.path()).is_empty());
}

#[tokio::test]
async fn the_settings_file_cannot_leave_a_value_behind_or_use_one_as_a_token() {
    let dir = tempfile::tempdir().unwrap();
    let allowed = UpstreamPolicy {
        allowed_hosts: vec!["127.0.0.1".into()],
    };
    let backends = Arc::new(RealBackends::new(Arc::new(|_| None), true, allowed));
    let engine = Engine::open(dir.path(), backends, Arc::new(|| 1_000)).unwrap();
    engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
    let update = with_secrets(&[("API_TOKEN", set(SECRET))]);
    engine.set_command_connector("kim", "local", update).await.unwrap();
    let key = keys(dir.path())[0].clone();
    assert!(key.starts_with("ENV_LOCAL_"), "{key}");

    // A server that records the `Authorization` header of every request.
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let record = {
        let seen = seen.clone();
        move |headers: axum::http::HeaderMap| {
            let seen = seen.clone();
            async move {
                seen.lock().unwrap().push(headers.get("authorization").cloned());
                axum::http::StatusCode::UNAUTHORIZED
            }
        }
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, axum::Router::new().fallback(record)).await });

    // A connector with an address names the key of the variable as its token.
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.insert(
        "thief".into(),
        json!({"mcp": {"transport": "http", "url": url}, "token_secret": key}),
    );
    engine.set_profile_config("kim", config.clone()).await.unwrap();
    let error = engine.connector_tools("kim", "thief").await.unwrap_err();
    assert!(!error.to_string().contains(SECRET), "{error}");
    // This rule stopped it: the value was not entered for an address.
    assert!(error.to_string().contains("was entered for another address"), "{error}");
    // The value was stored for the environment of a command, so no request carries it.
    assert!(
        seen.lock().unwrap().iter().all(Option::is_none),
        "{:?}",
        seen.lock().unwrap()
    );

    // The settings file drops the command. Its value goes with it.
    config.connectors.remove("local");
    config.connectors.remove("thief");
    engine.set_profile_config("kim", config).await.unwrap();
    assert!(keys(dir.path()).is_empty());
}

#[tokio::test]
async fn a_variable_that_the_settings_name_without_a_value_is_reported_as_missing() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path(), true).await;
    // The settings file names two secret variables. It cannot give their values.
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.insert(
        "local".into(),
        json!({"mcp": {"transport": "stdio", "command": "/opt/mcp/does-not-exist", "args": [], "env": {}},
               "secret_env": ["API_TOKEN", "SECOND"]}),
    );
    engine.set_profile_config("kim", config).await.unwrap();
    let shown = engine.connections("kim").await.unwrap().connectors["local"].clone();
    assert_eq!(shown.secret_env, ["API_TOKEN", "SECOND"]);
    assert_eq!(shown.secret_env_missing, ["API_TOKEN", "SECOND"]);

    // One gets its value. The other is left out, so it is removed.
    let update = CommandConnectorUpdate {
        env: Default::default(),
        ..with_secrets(&[("API_TOKEN", set(SECRET))])
    };
    engine.set_command_connector("kim", "local", update).await.unwrap();
    let shown = engine.connections("kim").await.unwrap().connectors["local"].clone();
    assert_eq!(shown.secret_env, ["API_TOKEN"]);
    assert!(shown.secret_env_missing.is_empty());

    // A settings file without the name removes the value for good, and the log says so.
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.get_mut("local").unwrap()["secret_env"] = json!([]);
    engine.set_profile_config("kim", config).await.unwrap();
    assert!(keys(dir.path()).is_empty());
    let audit = engine.snapshot("kim").await.unwrap().audit;
    assert!(
        audit
            .iter()
            .any(|entry| entry.kind == "secret_values_removed" && entry.detail == json!({"count": 1})),
        "{audit:?}"
    );
}
