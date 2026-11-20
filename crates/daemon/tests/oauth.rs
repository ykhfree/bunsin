//! OAuth sign-in of MCP connectors with the real backends, against a stub authorization server.

#[path = "support/oauth_stub.rs"]
mod stub;

use bunsin_api::{ApiError, ConnectorUpdate, EngineApi, OAuthState, ProfileConfig, TokenChange};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::Engine;
use serde_json::json;
use std::path::Path;
use std::sync::Arc;
use stub::{ACCESS, CODE, REFRESH, REFRESHED, Stub};

const REDIRECT: &str = "http://127.0.0.1:8976/callback";

/// An engine on `dir` whose operator allows only 127.0.0.1 and whose clock shows `now`.
fn open_at(dir: &Path, now: i64) -> Engine {
    let allowed = UpstreamPolicy {
        allowed_hosts: vec!["127.0.0.1".into()],
    };
    let backends = Arc::new(RealBackends::new(Arc::new(|_| None), false, allowed));
    Engine::open(dir, backends, Arc::new(move || now)).unwrap()
}

/// An engine whose operator allows only 127.0.0.1, with profiles `kim` and `lee`.
async fn engine(dir: &Path) -> Engine {
    let engine = open_at(dir, 1_000);
    for (id, owner) in [("kim", "Kim"), ("lee", "Lee")] {
        engine.create_profile(id, ProfileConfig::new(owner)).await.unwrap();
    }
    engine
}

/// Stores connector `docs` of `profile` at the stub, with `oauth` as its OAuth settings.
async fn add_docs(engine: &Engine, profile: &str, stub: &Stub, oauth: serde_json::Value) {
    let mut config = engine.profile_config(profile).await.unwrap();
    config.connectors.insert(
        "docs".into(),
        json!({"mcp": {"transport": "http", "url": stub.mcp_url()}, "oauth": oauth}),
    );
    engine.set_profile_config(profile, config).await.unwrap();
}

async fn state(engine: &Engine, profile: &str) -> OAuthState {
    let connections = engine.connections(profile).await.unwrap();
    connections.connectors["docs"].oauth.as_ref().unwrap().state
}

/// Signs `profile` in to `docs` as the owner does: start, the browser, finish.
async fn sign_in(engine: &Engine, profile: &str) {
    let started = engine.start_connector_oauth(profile, "docs", REDIRECT).await.unwrap();
    let callback = stub::browser(&started.authorization_url).await;
    engine.finish_connector_oauth(profile, "docs", callback).await.unwrap();
}

fn invalid(result: Result<impl std::fmt::Debug, ApiError>, part: &str) -> String {
    match result {
        Err(ApiError::Invalid(message)) if message.contains(part) => message,
        other => panic!("expected a refusal with `{part}`, got {other:?}"),
    }
}

/// Fails if any token or the code is in `text`. `text` must not be empty, so the check is real.
fn assert_no_secret(text: &str, what: &str) {
    assert!(!text.is_empty(), "{what} is empty");
    for secret in [ACCESS, REFRESHED, REFRESH, CODE] {
        assert!(!text.contains(secret), "{what} holds `{secret}`: {text}");
    }
}

/// Fails if any token is in a file of the data directory. The sealed credentials are there.
fn assert_no_secret_on_disk(dir: &Path) {
    let mut folders = vec![dir.to_owned()];
    let mut files = 0;
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            files += 1;
            for secret in [ACCESS, REFRESHED, REFRESH] {
                let found = bytes.windows(secret.len()).any(|window| window == secret.as_bytes());
                assert!(!found, "{} holds `{secret}` in plain text", path.display());
            }
        }
    }
    assert!(files > 0, "{} has no files", dir.display());
}

#[tokio::test]
async fn a_sign_in_seals_the_tokens_and_the_connector_lists_its_tools() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    add_docs(&engine, "kim", &stub, json!({"scopes": ["docs:read"]})).await;

    assert_eq!(state(&engine, "kim").await, OAuthState::SignInNeeded);
    let error = engine.connector_tools("kim", "docs").await.unwrap_err().to_string();
    assert!(
        error.contains("nobody signed in yet") && error.contains("connector-oauth"),
        "{error}"
    );
    assert_eq!(
        stub.count("/mcp"),
        0,
        "a connector without credentials must not connect"
    );

    let started = engine.start_connector_oauth("kim", "docs", REDIRECT).await.unwrap();
    let address = url::Url::parse(&started.authorization_url).unwrap();
    let query: std::collections::HashMap<_, _> = address.query_pairs().into_owned().collect();
    assert_eq!(query["redirect_uri"], REDIRECT);
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["scope"], "docs:read");
    assert_eq!(query["client_id"], stub::REGISTERED);
    // The client registered itself with the loopback address.
    assert_eq!(stub.knobs().registrations[0]["redirect_uris"], json!([REDIRECT]));

    let callback = stub::browser(&started.authorization_url).await;
    assert!(callback.starts_with(REDIRECT), "{callback}");
    engine.finish_connector_oauth("kim", "docs", callback).await.unwrap();

    assert_eq!(state(&engine, "kim").await, OAuthState::SignedIn);
    let report = engine.connector_tools("kim", "docs").await.unwrap();
    assert_eq!(
        report.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        ["hello"]
    );
    let sent = stub
        .knobs()
        .seen
        .iter()
        .filter(|s| s.path == "/mcp")
        .any(|s| s.authorization.as_deref() == Some(&format!("Bearer {ACCESS}")));
    assert!(sent, "the MCP server never got the access token");

    let snapshot = engine.snapshot("kim").await.unwrap();
    assert!(snapshot.audit.iter().any(|entry| entry.kind == "oauth_signed_in"));
    assert_no_secret(&serde_json::to_string(&snapshot.audit).unwrap(), "the audit log");
    assert_no_secret(
        &serde_json::to_string(&engine.profile_config("kim").await.unwrap()).unwrap(),
        "the settings",
    );
    assert_no_secret(
        &serde_json::to_string(&engine.connections("kim").await.unwrap()).unwrap(),
        "the connections",
    );
    drop(engine);
    assert_no_secret_on_disk(dir.path());
}

#[tokio::test]
async fn a_callback_must_carry_the_state_of_a_waiting_sign_in_of_this_profile() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    add_docs(&engine, "kim", &stub, json!({})).await;
    add_docs(&engine, "lee", &stub, json!({})).await;

    let started = engine.start_connector_oauth("kim", "docs", REDIRECT).await.unwrap();
    let callback = stub::browser(&started.authorization_url).await;
    let forged = callback.replace("state=", "state=x");
    let message = invalid(
        engine.finish_connector_oauth("kim", "docs", forged).await,
        "no sign-in waits",
    );
    assert_no_secret(&message, "the error");
    // Another profile cannot use the callback of Kim's sign-in, and does not use it up.
    invalid(
        engine.finish_connector_oauth("lee", "docs", callback.clone()).await,
        "no sign-in waits",
    );
    assert_eq!(state(&engine, "lee").await, OAuthState::SignInNeeded);
    assert_eq!(
        stub.count("/token"),
        0,
        "a refused callback must not reach the token endpoint"
    );

    engine
        .finish_connector_oauth("kim", "docs", callback.clone())
        .await
        .unwrap();
    assert_eq!(state(&engine, "kim").await, OAuthState::SignedIn);
    // A sign-in is used once.
    invalid(
        engine.finish_connector_oauth("kim", "docs", callback).await,
        "no sign-in waits",
    );
}

#[tokio::test]
async fn a_callback_at_another_place_ends_the_sign_in_without_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    add_docs(&engine, "kim", &stub, json!({})).await;

    let started = engine.start_connector_oauth("kim", "docs", REDIRECT).await.unwrap();
    let callback = stub::browser(&started.authorization_url).await;
    let moved = callback.replace("127.0.0.1:8976/callback", "127.0.0.1:8977/callback");
    invalid(
        engine.finish_connector_oauth("kim", "docs", moved).await,
        "must start with the redirect address",
    );
    // The state was seen at a wrong place, so the sign-in is over.
    invalid(
        engine.finish_connector_oauth("kim", "docs", callback).await,
        "no sign-in waits",
    );
    assert_eq!(stub.count("/token"), 0);
    assert_eq!(state(&engine, "kim").await, OAuthState::SignInNeeded);
}

#[tokio::test]
async fn a_start_needs_a_loopback_redirect_and_room_for_one_more_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    add_docs(&engine, "kim", &stub, json!({})).await;
    add_docs(&engine, "lee", &stub, json!({})).await;

    for bad in ["https://bunsin.example.com/callback", "http://10.0.0.5:8976/callback"] {
        invalid(engine.start_connector_oauth("kim", "docs", bad).await, "loopback");
    }
    for _ in 0..3 {
        engine.start_connector_oauth("kim", "docs", REDIRECT).await.unwrap();
    }
    invalid(
        engine.start_connector_oauth("kim", "docs", REDIRECT).await,
        "3 sign-ins of this profile wait",
    );
    // The limit is per profile.
    engine.start_connector_oauth("lee", "docs", REDIRECT).await.unwrap();
    // A connector without OAuth has nothing to sign in to.
    engine
        .set_connector("kim", "plain", connector(&stub.mcp_url(), TokenChange::Keep))
        .await
        .unwrap();
    invalid(
        engine.start_connector_oauth("kim", "plain", REDIRECT).await,
        "does not sign in with OAuth",
    );
    assert!(matches!(
        engine.start_connector_oauth("kim", "nothing", REDIRECT).await,
        Err(ApiError::NotFound(_))
    ));
}

fn connector(url: &str, token: TokenChange) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.to_owned(),
        token,
        token_header: None,
        expected: bunsin_api::Expected::Unchecked,
    }
}

#[tokio::test]
async fn oauth_and_a_token_never_go_together_and_a_form_save_keeps_oauth() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.insert(
        "docs".into(),
        json!({"mcp": {"transport": "http", "url": stub.mcp_url()}, "token_secret": "MCP_DOCS", "oauth": {}}),
    );
    invalid(engine.set_profile_config("kim", config).await, "not both");
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.insert(
        "shell".into(),
        json!({"mcp": {"transport": "stdio", "command": "/bin/sh"}, "oauth": {}}),
    );
    assert!(engine.set_profile_config("kim", config).await.is_err());

    add_docs(&engine, "kim", &stub, json!({"client_id": "bunsin-app"})).await;
    let typed = connector(&stub.mcp_url(), TokenChange::Set("typed-token-73".into()));
    invalid(engine.set_connector("kim", "docs", typed).await, "not both");
    // A save from the form or `connector-set` does not know `oauth` and keeps it.
    engine
        .set_connector("kim", "docs", connector(&stub.mcp_url(), TokenChange::Keep))
        .await
        .unwrap();
    let docs = engine.connections("kim").await.unwrap().connectors["docs"].clone();
    assert_eq!(docs.oauth.unwrap().config.client_id.as_deref(), Some("bunsin-app"));
}

#[tokio::test]
async fn every_oauth_request_obeys_the_address_rules_and_follows_no_redirect() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    add_docs(&engine, "kim", &stub, json!({})).await;

    // The metadata names endpoints on `localhost`, which the operator did not allow.
    stub.knobs().endpoint_host = "localhost".into();
    let message = invalid(
        engine.start_connector_oauth("kim", "docs", REDIRECT).await,
        "`localhost`, which the server operator has not allowed",
    );
    assert!(stub.knobs().registrations.is_empty());
    // With a client ID there is no registration, but the token endpoint is refused at the finish.
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.get_mut("docs").unwrap()["oauth"] = json!({"client_id": "bunsin-app"});
    engine.set_profile_config("kim", config).await.unwrap();
    let started = engine.start_connector_oauth("kim", "docs", REDIRECT).await.unwrap();
    let callback = stub::browser(&started.authorization_url.replace("localhost", "127.0.0.1")).await;
    invalid(
        engine.finish_connector_oauth("kim", "docs", callback).await,
        "has not allowed",
    );
    assert!(
        stub.knobs().seen.iter().all(|seen| !seen.host.starts_with("localhost")),
        "a request went to a host outside the list"
    );
    assert_no_secret(&message, "the error");

    // The token endpoint answers with a redirect on an allowed host. The client does not follow it.
    stub.knobs().endpoint_host = "127.0.0.1".into();
    stub.knobs().move_token_endpoint = true;
    let started = engine.start_connector_oauth("kim", "docs", REDIRECT).await.unwrap();
    let callback = stub::browser(&started.authorization_url).await;
    assert!(engine.finish_connector_oauth("kim", "docs", callback).await.is_err());
    assert_eq!(stub.count("/token"), 1);
    assert_eq!(stub.count("/token-moved"), 0, "the client followed a redirect");
    assert_eq!(state(&engine, "kim").await, OAuthState::SignInNeeded);
}

#[tokio::test]
async fn a_refresh_is_sealed_and_a_refused_refresh_asks_for_a_new_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub::start().await;
    // The first token is about to expire, so its first use refreshes it.
    stub.knobs().expires_in = 1;
    {
        let engine = engine(dir.path()).await;
        add_docs(&engine, "kim", &stub, json!({})).await;
        sign_in(&engine, "kim").await;
        engine.connector_tools("kim", "docs").await.unwrap();
        assert_eq!(stub.knobs().refreshes, 1);
    }
    // A restarted server reads the refreshed credentials and does not refresh again.
    let allowed = UpstreamPolicy {
        allowed_hosts: vec!["127.0.0.1".into()],
    };
    let backends = Arc::new(RealBackends::new(Arc::new(|_| None), false, allowed));
    let engine = Engine::open(dir.path(), backends, Arc::new(|| 1_000)).unwrap();
    engine.connector_tools("kim", "docs").await.unwrap();
    assert_eq!(stub.knobs().refreshes, 1);
    let last = stub
        .knobs()
        .seen
        .iter()
        .rev()
        .find(|s| s.path == "/mcp")
        .cloned()
        .unwrap();
    assert_eq!(last.authorization, Some(format!("Bearer {REFRESHED}")));

    // A new sign-in, whose refresh the authorization server refuses.
    sign_in(&engine, "kim").await;
    stub.knobs().refuse_refresh = true;
    let error = engine.connector_tools("kim", "docs").await.unwrap_err().to_string();
    assert!(error.contains("needs a new sign-in"), "{error}");
    assert_no_secret(&error, "the error");
    assert_eq!(stub.knobs().refreshes, 2);
    assert_eq!(state(&engine, "kim").await, OAuthState::SignInNeeded);
    let audit = engine.snapshot("kim").await.unwrap().audit;
    let needed = audit.iter().find(|entry| entry.kind == "oauth_sign_in_needed").unwrap();
    assert_eq!(needed.detail, json!({"target": "docs"}));
    assert_no_secret(&serde_json::to_string(&audit).unwrap(), "the audit log");

    // The next build says what to do.
    engine
        .set_profile_config("kim", engine.profile_config("kim").await.unwrap())
        .await
        .unwrap();
    let error = engine.connector_tools("kim", "docs").await.unwrap_err().to_string();
    assert!(error.contains("nobody signed in yet"), "{error}");
}

#[tokio::test]
async fn credentials_count_only_for_the_origin_they_were_given_for() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let stub = stub::start().await;
    let other = stub::start().await;
    add_docs(&engine, "kim", &stub, json!({})).await;
    sign_in(&engine, "kim").await;
    // The owner points the connector at another server. The tokens are for the first one.
    add_docs(&engine, "kim", &other, json!({})).await;
    assert_eq!(state(&engine, "kim").await, OAuthState::SignInNeeded);
    let error = engine.connector_tools("kim", "docs").await.unwrap_err().to_string();
    assert!(error.contains("signed in for another address"), "{error}");
    assert!(other.knobs().seen.is_empty(), "the other server was asked");

    // Turning OAuth off removes the credentials with the next save.
    let mut config = engine.profile_config("kim").await.unwrap();
    config
        .connectors
        .get_mut("docs")
        .unwrap()
        .as_object_mut()
        .unwrap()
        .remove("oauth");
    engine.set_profile_config("kim", config).await.unwrap();
    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    assert!(store.secret_keys().unwrap().is_empty());
}

#[tokio::test]
async fn an_expired_access_token_without_a_refresh_token_asks_for_a_new_sign_in() {
    let dir = tempfile::tempdir().unwrap();
    let stub = stub::start().await;
    // The token from the code lives one second, and nothing can renew it.
    stub.knobs().expires_in = 1;
    stub.knobs().no_refresh_token = true;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    {
        // The clock of this engine shows a time before the token was given.
        let engine = engine(dir.path()).await;
        add_docs(&engine, "kim", &stub, json!({})).await;
        sign_in(&engine, "kim").await;
        assert_eq!(state(&engine, "kim").await, OAuthState::SignedIn);
    }

    let engine = open_at(dir.path(), now + 7_200);
    assert_eq!(state(&engine, "kim").await, OAuthState::SignInNeeded);
    let error = engine.connector_tools("kim", "docs").await.unwrap_err().to_string();
    assert!(
        error.contains("needs a new sign-in") && error.contains("connector-oauth"),
        "{error}"
    );
    assert_no_secret(&error, "the error");
    assert_eq!(stub.knobs().refreshes, 0, "there is no refresh token to send");

    // A new sign-in gives a token that the connector can use again.
    stub.knobs().expires_in = 3_600;
    sign_in(&engine, "kim").await;
    let report = engine.connector_tools("kim", "docs").await.unwrap();
    assert_eq!(report.tools.len(), 1);
}
