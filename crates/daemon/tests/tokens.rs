//! Stored tokens with the real backends: a token goes only to the address it was entered for.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use bunsin_api::{ApiError, ConnectorUpdate, EngineApi, ProfileConfig, TokenChange};
use bunsin_daemon::{RealBackends, UpstreamPolicy};
use bunsin_engine::Engine;
use std::sync::{Arc, Mutex};

const TOKEN: &str = "xoxb-stored-token-93ac";

/// A server that records the `Authorization` header of every request and answers 401.
struct Recorder {
    url: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl Recorder {
    async fn start() -> Self {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = |State(seen): State<Arc<Mutex<Vec<Option<String>>>>>, headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            seen.lock().unwrap().push(auth);
            StatusCode::UNAUTHORIZED
        };
        let app = axum::Router::new().fallback(record).with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/mcp", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Self { url, seen }
    }

    fn got_token(&self) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .flatten()
            .any(|auth| auth.contains(TOKEN))
    }

    fn requests(&self) -> usize {
        self.seen.lock().unwrap().len()
    }
}

async fn engine(dir: &std::path::Path) -> Engine {
    let allowed = UpstreamPolicy {
        allowed_hosts: vec!["127.0.0.1".into()],
    };
    let backends = Arc::new(RealBackends::new(Arc::new(|_| None), false, allowed));
    let engine = Engine::open(dir, backends, Arc::new(|| 1_000)).unwrap();
    engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
    engine
}

fn update(url: &str, token: TokenChange) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.to_owned(),
        token,
        expected: bunsin_api::Expected::Unchecked,
    }
}

#[tokio::test]
async fn the_token_reaches_its_own_server_and_no_other_after_the_address_changes() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let own = Recorder::start().await;
    let other = Recorder::start().await;

    engine
        .set_connector("kim", "slack", update(&own.url, TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    // The check after a save connects with the stored token. This server answers 401, so the check fails.
    let error = engine.connector_tools("kim", "slack").await.unwrap_err();
    assert!(own.got_token(), "the stored token was not sent to its own server");
    assert!(!error.to_string().contains(TOKEN), "{error}");

    // Path 1: the owner (or someone with the access token) changes the address and keeps the token.
    let kept = engine
        .set_connector("kim", "slack", update(&other.url, TokenChange::Keep))
        .await;
    assert!(
        matches!(kept, Err(ApiError::Invalid(ref m)) if m.contains("enter the token again")),
        "{kept:?}"
    );

    // Path 2: the whole settings are replaced, with the same secret name and the other address.
    let mut config = engine.profile_config("kim").await.unwrap();
    config.connectors.get_mut("slack").unwrap()["mcp"]["url"] = serde_json::json!(other.url);
    engine.set_profile_config("kim", config).await.unwrap();
    let error = engine.connector_tools("kim", "slack").await.unwrap_err();
    assert!(error.to_string().contains("entered for another address"), "{error}");

    assert!(!other.got_token(), "the token was sent to another server");
    assert_eq!(other.requests(), 0);

    // With the token entered again for the new address, that server gets it.
    engine
        .set_connector("kim", "slack", update(&other.url, TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    let _ = engine.connector_tools("kim", "slack").await;
    assert!(other.got_token());
}

#[tokio::test]
async fn an_address_the_server_does_not_allow_is_refused_before_the_token_is_stored() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    // Only 127.0.0.1 is on the operator's list.
    for url in [
        "https://mcp.example.com/slack",
        "http://169.254.169.254/latest",
        "http://localhost:9000/mcp",
    ] {
        let result = engine
            .set_connector("kim", "slack", update(url, TokenChange::Set(TOKEN.into())))
            .await;
        assert!(matches!(result, Err(ApiError::Invalid(_))), "{url}: {result:?}");
    }
    assert!(engine.connections("kim").await.unwrap().connectors.is_empty());
    // Nothing of the token is on disk, not even encrypted.
    let store = bunsin_store::AppStore::open(dir.path())
        .unwrap()
        .open_profile("kim")
        .unwrap();
    assert!(store.secret_keys().unwrap().is_empty());
}

#[tokio::test]
async fn a_connector_without_a_token_sends_no_authorization_header() {
    let dir = tempfile::tempdir().unwrap();
    let engine = engine(dir.path()).await;
    let server = Recorder::start().await;
    engine
        .set_connector("kim", "wiki", update(&server.url, TokenChange::Keep))
        .await
        .unwrap();
    let _ = engine.connector_tools("kim", "wiki").await;
    assert!(server.requests() > 0);
    assert!(server.seen.lock().unwrap().iter().all(Option::is_none));
}
