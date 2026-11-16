//! The HTTP server in front of the engine. See `bunsin_api::http` for the routes.
//!
//! An access token gives access to one profile. The server checks the token before
//! it looks at the profile, so a caller learns nothing about other profiles.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use bunsin_api::http::{ApproveBody, ApproveToolsBody, ErrorBody, KillSwitchBody, PROFILES, RejectBody, status_of};
use bunsin_api::{
    ApiError, CommandConnectorUpdate, Connections, ConnectorUpdate, EngineApi, LlmTest, LlmUpdate, Outcome,
    Permissions, PermissionsUpdate, PollPreview, PrinciplesUpdate, PrivacyUpdate, ProfileConfig, ProfileSnapshot,
    ReplayCase, ReplayResult, ToolsReport, WikiPageUpdate,
};
use bunsin_core::event::InboundEvent;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::records::ProfileInfo;
use bunsin_core::wiki::{WikiPage, WikiSummary};
use bunsin_engine::Engine;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How long running requests may finish after a stop signal. One event can need two LLM
/// calls of up to 120 s each, so the grace covers that. A request that is cut leaves its
/// inbox item in the `executing` state.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(300);

struct Failure(ApiError);

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(status_of(&self.0)).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        (status, Json(ErrorBody { error: self.0 })).into_response()
    }
}

impl From<ApiError> for Failure {
    fn from(error: ApiError) -> Self {
        Self(error)
    }
}

type Reply<T> = Result<Json<T>, Failure>;

/// Returns the profile that the request's access token belongs to.
fn token_profile(engine: &Engine, headers: &HeaderMap) -> Result<String, ApiError> {
    let refused = || ApiError::Unauthorized("a valid access token is required".into());
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(refused)?;
    let token = value.strip_prefix("Bearer ").ok_or_else(refused)?;
    // The table is read on every request, so a revoked token stops working at once.
    let profile = engine
        .app_store()
        .token_profile(token)
        .map_err(|e| ApiError::Internal(e.to_string()))?;
    profile.ok_or_else(refused)
}

/// Fails unless the token belongs to `profile`. The answer is the same whether or not
/// `profile` exists.
fn authorize(engine: &Engine, headers: &HeaderMap, profile: &str) -> Result<(), ApiError> {
    if token_profile(engine, headers)? == profile {
        Ok(())
    } else {
        Err(ApiError::Unauthorized(
            "this access token is for another profile".into(),
        ))
    }
}

async fn list_profiles(State(engine): State<Arc<Engine>>, headers: HeaderMap) -> Reply<Vec<ProfileInfo>> {
    let own = token_profile(&engine, &headers)?;
    let all = engine.list_profiles().await?;
    Ok(Json(all.into_iter().filter(|profile| profile.id == own).collect()))
}

async fn get_config(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<ProfileConfig> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.profile_config(&profile).await?))
}

async fn put_config(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(config): Json<ProfileConfig>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_profile_config(&profile, config).await?))
}

async fn snapshot(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<ProfileSnapshot> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.snapshot(&profile).await?))
}

async fn ingest(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(event): Json<InboundEvent>,
) -> Reply<Outcome> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.ingest(&profile, event).await?))
}

async fn approve(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, inbox_id)): Path<(String, i64)>,
    Json(body): Json<ApproveBody>,
) -> Reply<Outcome> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.approve(&profile, inbox_id, body.edit).await?))
}

async fn reject(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, inbox_id)): Path<(String, i64)>,
    Json(body): Json<RejectBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.reject(&profile, inbox_id, &body.reason).await?))
}

async fn kill_switch(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<KillSwitchBody>,
) -> Reply<KillSwitchBody> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(KillSwitchBody {
        on: engine.set_kill_switch(&profile, body.on).await?,
    }))
}

async fn replay(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(case): Json<ReplayCase>,
) -> Reply<ReplayResult> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.replay(&profile, case).await?))
}

async fn connector_tools(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, connector)): Path<(String, String)>,
) -> Reply<ToolsReport> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.connector_tools(&profile, &connector).await?))
}

async fn approve_tools(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, connector)): Path<(String, String)>,
    Json(body): Json<ApproveToolsBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.approve_tools(&profile, &connector, body.shown).await?))
}

async fn get_permissions(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Permissions> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.permissions(&profile).await?))
}

async fn put_permissions(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<PermissionsUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_permissions(&profile, update).await?))
}

async fn get_principles(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<String>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.principles(&profile).await?))
}

async fn put_principles(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<PrinciplesUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_principles(&profile, update).await?))
}

async fn get_privacy(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<PrivacyConfig> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.privacy(&profile).await?))
}

async fn put_privacy(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<PrivacyUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_privacy(&profile, update).await?))
}

async fn test_llm(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<LlmTest> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.test_llm(&profile).await?))
}

async fn test_poll(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, index)): Path<(String, usize)>,
) -> Reply<PollPreview> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.test_poll(&profile, index).await?))
}

async fn get_connections(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Connections> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.connections(&profile).await?))
}

async fn put_llm(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<LlmUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_llm(&profile, update).await?))
}

async fn put_command_connector(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, name)): Path<(String, String)>,
    Json(update): Json<CommandConnectorUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_command_connector(&profile, &name, update).await?))
}

async fn put_connector(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, name)): Path<(String, String)>,
    Json(update): Json<ConnectorUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_connector(&profile, &name, update).await?))
}

async fn delete_connector(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, name)): Path<(String, String)>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.remove_connector(&profile, &name).await?))
}

async fn wiki_index(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<WikiSummary>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.wiki_index(&profile).await?))
}

async fn get_wiki_page(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, slug)): Path<(String, String)>,
) -> Reply<WikiPage> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.wiki_page(&profile, &slug).await?))
}

async fn put_wiki_page(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, slug)): Path<(String, String)>,
    Json(update): Json<WikiPageUpdate>,
) -> Reply<WikiPage> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_wiki_page(&profile, &slug, update).await?))
}

async fn delete_wiki_page(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, slug)): Path<(String, String)>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.delete_wiki_page(&profile, &slug).await?))
}

async fn wiki_history(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, slug)): Path<(String, String)>,
) -> Reply<Vec<Option<WikiPage>>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.wiki_history(&profile, &slug).await?))
}

pub fn router(engine: Arc<Engine>) -> Router {
    let profile = |rest: &str| format!("{PROFILES}/{{profile}}/{rest}");
    Router::new()
        .route(PROFILES, get(list_profiles))
        .route(&profile("config"), get(get_config).put(put_config))
        .route(&profile("snapshot"), get(snapshot))
        .route(&profile("events"), post(ingest))
        .route(&profile("inbox/{inbox_id}/approve"), post(approve))
        .route(&profile("inbox/{inbox_id}/reject"), post(reject))
        .route(&profile("kill-switch"), put(kill_switch))
        .route(&profile("replay"), post(replay))
        .route(&profile("connectors/{connector}/tools"), get(connector_tools))
        .route(&profile("connectors/{connector}/tools/approve"), post(approve_tools))
        .route(&profile("privacy"), get(get_privacy).put(put_privacy))
        .route(&profile("permissions"), get(get_permissions).put(put_permissions))
        .route(&profile("principles"), get(get_principles).put(put_principles))
        .route(&profile("polls/{index}/test"), post(test_poll))
        .route(&profile("llm/test"), post(test_llm))
        .route(&profile("wiki"), get(wiki_index))
        .route(&profile("wiki/{slug}/history"), get(wiki_history))
        .route(
            &profile("wiki/{slug}"),
            get(get_wiki_page).put(put_wiki_page).delete(delete_wiki_page),
        )
        .route(&profile("connections"), get(get_connections))
        .route(&profile("connections/llm"), put(put_llm))
        .route(
            &profile("connections/connectors/{name}"),
            put(put_connector).delete(delete_connector),
        )
        .route(
            &profile("connections/connectors/{name}/command"),
            put(put_command_connector),
        )
        .with_state(engine)
}

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    /// PEM certificate chain and private key. Required unless `listen` is a loopback address.
    pub tls: Option<(PathBuf, PathBuf)>,
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(
        "{0} is reachable from other machines, so TLS is required: give --tls-cert and --tls-key, \
         or listen on 127.0.0.1 behind a TLS proxy"
    )]
    TlsRequired(SocketAddr),
    #[error("the TLS certificate or key cannot be read: {0}")]
    Tls(std::io::Error),
    #[error("the server failed: {0}")]
    Io(#[from] std::io::Error),
}

/// A running server.
pub struct Running {
    pub addr: SocketAddr,
    pub tls: bool,
    handle: Handle<SocketAddr>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Running {
    /// The address a client on this machine can use.
    pub fn url(&self) -> String {
        format!("{}://{}", if self.tls { "https" } else { "http" }, self.addr)
    }

    /// Stops taking new requests, lets running ones finish, then returns.
    pub async fn shutdown(self) -> Result<(), ServeError> {
        self.handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
        self.task.await.map_err(std::io::Error::other)??;
        Ok(())
    }
}

/// Starts the server and returns when it is listening.
impl ServeConfig {
    /// Refuses an address that other computers reach without TLS. A caller can ask this
    /// before it opens the engine, so a refused start touches no key.
    pub fn check(&self) -> Result<(), ServeError> {
        if self.tls.is_none() && !self.listen.ip().is_loopback() {
            return Err(ServeError::TlsRequired(self.listen));
        }
        Ok(())
    }
}

pub async fn serve(engine: Arc<Engine>, config: ServeConfig) -> Result<Running, ServeError> {
    config.check()?;
    let app = router(engine).into_make_service();
    let handle = Handle::new();
    let tls = config.tls.is_some();
    let task = match config.tls {
        Some((cert, key)) => {
            let rustls = RustlsConfig::from_pem_file(cert, key).await.map_err(ServeError::Tls)?;
            tokio::spawn(
                axum_server::bind_rustls(config.listen, rustls)
                    .handle(handle.clone())
                    .serve(app),
            )
        }
        None => tokio::spawn(axum_server::bind(config.listen).handle(handle.clone()).serve(app)),
    };
    match handle.listening().await {
        Some(addr) => Ok(Running {
            addr,
            tls,
            handle,
            task,
        }),
        // The server task ended before it listened, for example because the port is taken.
        None => match task.await.map_err(std::io::Error::other)? {
            Err(error) => Err(ServeError::Io(error)),
            Ok(()) => Err(ServeError::Io(std::io::Error::other(
                "the server stopped before it listened",
            ))),
        },
    }
}

/// How often the server looks for polls that are due.
const POLL_TICK: Duration = Duration::from_secs(5);

/// The task that repeats the searches of all profiles.
pub struct Poller {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
}

impl Poller {
    /// Starts polling. A failure of one profile is written to stderr and does not stop the task.
    pub fn start(engine: Arc<Engine>) -> Self {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move {
            loop {
                for (profile, error) in engine.poll_all().await {
                    eprintln!("poll of profile {profile} failed: {error}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL_TICK) => {}
                    _ = stopped.changed() => break,
                }
            }
        });
        Self { stop, task }
    }

    /// Lets the running cycle finish, then ends the task.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        if tokio::time::timeout(SHUTDOWN_GRACE, self.task).await.is_err() {
            eprintln!("the poll task did not finish in {} s", SHUTDOWN_GRACE.as_secs());
        }
    }
}
