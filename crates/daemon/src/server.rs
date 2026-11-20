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
use bunsin_api::http::{
    AddCallerBody, AddReplayCasesBody, ApproveBody, ApproveToolsBody, ClearAutonomyHoldBody, DeleteReplayCaseBody,
    DismissFindingBody, DismissPrincipleBody, ErrorBody, ExtractPrinciplesBody, KillSwitchBody, OAuthFinishBody,
    OAuthStartBody, PROFILES, RejectBody, ReportAutonomousBody, ResolveUnknownBody, RetireCorrectionBody,
    WebhookSecretBody, WikiSearchBody, status_of,
};
use bunsin_api::{
    AdoptPrinciple, ApiConnectorUpdate, ApiError, ArdRegistriesUpdate, Autonomy, AutonomyThresholdsUpdate,
    CommandConnectorUpdate, Connections, ConnectorUpdate, EngineApi, EvalReport, EvalSettingsUpdate, InboundStatus,
    InboundUpdate, LlmTest, LlmUpdate, NewCallerToken, NotesSettingsUpdate, OAuthStarted, Outcome, Permissions,
    PermissionsUpdate, PollPreview, PollsUpdate, PreloadNotes, PrinciplesUpdate, PrivacyUpdate, ProfileConfig,
    ProfileSnapshot, ReplayCase, ReplayResult, ToolsReport, WebhooksUpdate, WikiPageUpdate,
};
use bunsin_core::ard::ArdRegistry;
use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::candidates::PrincipleCandidate;
use bunsin_core::eval::EvalConfig;
use bunsin_core::eval::ReplayCaseRecord;
use bunsin_core::event::InboundEvent;
use bunsin_core::inbound::InboundConfig;
use bunsin_core::inbound::InboundScope;
use bunsin_core::lint::LintFinding;
use bunsin_core::poll::PollConfig;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::records::{CallerToken, CorrectionRecord, LlmUsage, ProfileInfo};
use bunsin_core::webhook::WebhookConfig;
use bunsin_core::wiki::NotesConfig;
use bunsin_core::wiki::{WikiPage, WikiSummary};
use bunsin_engine::Engine;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use url::Url;

/// How long running requests may finish after a stop signal. One event can need two LLM
/// calls of up to 120 s each, so the grace covers that. A request that is cut leaves its
/// inbox item in the `executing` state. The next `start` marks it `unknown`.
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
    // The access token proves the owner. Only this route marks an event as the owner's.
    let event = InboundEvent {
        channel: bunsin_core::event::Channel::Owner,
        ..event
    };
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

async fn llm_usage(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<LlmUsage>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.llm_usage(&profile).await?))
}

async fn resolve_unknown(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<ResolveUnknownBody>,
) -> Reply<Option<Outcome>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(
        engine.resolve_unknown(&profile, body.inbox_id, body.resolution).await?,
    ))
}

async fn get_polls(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<PollConfig>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.polls(&profile).await?))
}

async fn put_polls(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<PollsUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_polls(&profile, update).await?))
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

async fn put_api_connector(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, name)): Path<(String, String)>,
    Json(update): Json<ApiConnectorUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_api_connector(&profile, &name, update).await?))
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

async fn corrections(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<CorrectionRecord>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.corrections(&profile).await?))
}

async fn retire_correction(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<RetireCorrectionBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.retire_correction(&profile, body.id).await?))
}

async fn autonomy(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Autonomy> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.autonomy(&profile).await?))
}

async fn report_autonomous(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<ReportAutonomousBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.report_autonomous(&profile, body.inbox_id).await?))
}

async fn clear_autonomy_hold(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<ClearAutonomyHoldBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.clear_autonomy_hold(&profile, &body.tool).await?))
}

async fn autonomy_thresholds(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Option<AutonomyThresholds>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.autonomy_thresholds(&profile).await?))
}

async fn set_autonomy_thresholds(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<AutonomyThresholdsUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_autonomy_thresholds(&profile, update).await?))
}

async fn add_replay_cases(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<AddReplayCasesBody>,
) -> Reply<Vec<i64>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.add_replay_cases(&profile, body.cases).await?))
}

async fn replay_cases(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<ReplayCaseRecord>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.replay_cases(&profile).await?))
}

async fn delete_replay_case(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<DeleteReplayCaseBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.delete_replay_case(&profile, body.id).await?))
}

async fn start_eval(State(engine): State<Arc<Engine>>, headers: HeaderMap, Path(profile): Path<String>) -> Reply<i64> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.start_eval(&profile).await?))
}

async fn eval_report(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<EvalReport> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.eval_report(&profile).await?))
}

async fn extract_principles(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<ExtractPrinciplesBody>,
) -> Reply<Vec<PrincipleCandidate>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.extract_principles(&profile, body.source).await?))
}

async fn principle_candidates(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<PrincipleCandidate>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.principle_candidates(&profile).await?))
}

async fn adopt_principle(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(adoption): Json<AdoptPrinciple>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.adopt_principle(&profile, adoption).await?))
}

async fn dismiss_principle(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<DismissPrincipleBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.dismiss_principle(&profile, body.id).await?))
}

async fn preload_notes(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(request): Json<PreloadNotes>,
) -> Reply<Vec<i64>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.preload_notes(&profile, request).await?))
}

async fn lint_notes(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<LintFinding>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.lint_notes(&profile).await?))
}

async fn lint_findings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<LintFinding>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.lint_findings(&profile).await?))
}

async fn dismiss_finding(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<DismissFindingBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.dismiss_finding(&profile, body.id).await?))
}

async fn wiki_index(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<WikiSummary>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.wiki_index(&profile).await?))
}

async fn wiki_search(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<WikiSearchBody>,
) -> Reply<Vec<WikiSummary>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.wiki_search(&profile, &body.query).await?))
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

async fn caller_tokens(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<CallerToken>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.caller_tokens(&profile).await?))
}

async fn add_caller_token(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(body): Json<AddCallerBody>,
) -> Reply<NewCallerToken> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.add_caller_token(&profile, &body.label).await?))
}

async fn inbound_status(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<InboundStatus> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.inbound_status(&profile).await?))
}

async fn put_webhook_secret(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, name)): Path<(String, String)>,
    Json(body): Json<WebhookSecretBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_webhook_secret(&profile, &name, body.secret).await?))
}

async fn inbound_settings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<InboundConfig> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.inbound_settings(&profile).await?))
}

async fn set_inbound_settings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<InboundUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_inbound_settings(&profile, update).await?))
}

async fn ard_registries(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<Vec<ArdRegistry>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.ard_registries(&profile).await?))
}

async fn set_ard_registries(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<ArdRegistriesUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_ard_registries(&profile, update).await?))
}

async fn webhooks(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<BTreeMap<String, WebhookConfig>> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.webhooks(&profile).await?))
}

async fn set_webhooks(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<WebhooksUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_webhooks(&profile, update).await?))
}

async fn notes_settings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<NotesConfig> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.notes_settings(&profile).await?))
}

async fn set_notes_settings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<NotesSettingsUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_notes_settings(&profile, update).await?))
}

async fn eval_settings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
) -> Reply<EvalConfig> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.eval_settings(&profile).await?))
}

async fn set_eval_settings(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path(profile): Path<String>,
    Json(update): Json<EvalSettingsUpdate>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.set_eval_settings(&profile, update).await?))
}

async fn start_connector_oauth(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, connector)): Path<(String, String)>,
    Json(body): Json<OAuthStartBody>,
) -> Reply<OAuthStarted> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(
        engine
            .start_connector_oauth(&profile, &connector, &body.redirect_uri)
            .await?,
    ))
}

async fn finish_connector_oauth(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, connector)): Path<(String, String)>,
    Json(body): Json<OAuthFinishBody>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(
        engine
            .finish_connector_oauth(&profile, &connector, body.callback_url)
            .await?,
    ))
}

async fn revoke_caller_token(
    State(engine): State<Arc<Engine>>,
    headers: HeaderMap,
    Path((profile, id)): Path<(String, i64)>,
) -> Reply<()> {
    authorize(&engine, &headers, &profile)?;
    Ok(Json(engine.revoke_caller_token(&profile, id).await?))
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
        .route(
            &profile("connectors/{connector}/oauth/start"),
            post(start_connector_oauth),
        )
        .route(
            &profile("connectors/{connector}/oauth/finish"),
            post(finish_connector_oauth),
        )
        .route(&profile("privacy"), get(get_privacy).put(put_privacy))
        .route(&profile("permissions"), get(get_permissions).put(put_permissions))
        .route(&profile("principles"), get(get_principles).put(put_principles))
        .route(&profile("polls"), get(get_polls).put(put_polls))
        .route(&profile("polls/{index}/test"), post(test_poll))
        .route(&profile("llm/test"), post(test_llm))
        .route(&profile("llm-usage"), post(llm_usage))
        .route(&profile("resolve-unknown"), post(resolve_unknown))
        .route(&profile("corrections"), post(corrections))
        .route(&profile("retire-correction"), post(retire_correction))
        .route(&profile("autonomy"), post(autonomy))
        .route(&profile("report-autonomous"), post(report_autonomous))
        .route(&profile("clear-autonomy-hold"), post(clear_autonomy_hold))
        .route(&profile("autonomy-thresholds"), post(autonomy_thresholds))
        .route(&profile("set-autonomy-thresholds"), post(set_autonomy_thresholds))
        .route(&profile("add-replay-cases"), post(add_replay_cases))
        .route(&profile("replay-cases"), post(replay_cases))
        .route(&profile("delete-replay-case"), post(delete_replay_case))
        .route(&profile("start-eval"), post(start_eval))
        .route(&profile("eval-report"), post(eval_report))
        .route(&profile("extract-principles"), post(extract_principles))
        .route(&profile("principle-candidates"), post(principle_candidates))
        .route(&profile("adopt-principle"), post(adopt_principle))
        .route(&profile("dismiss-principle"), post(dismiss_principle))
        .route(&profile("lint-notes"), post(lint_notes))
        .route(&profile("preload-notes"), post(preload_notes))
        .route(&profile("lint-findings"), post(lint_findings))
        .route(&profile("dismiss-finding"), post(dismiss_finding))
        .route(&profile("wiki"), get(wiki_index))
        .route(&profile("wiki-search"), post(wiki_search))
        .route(&profile("wiki/{slug}/history"), get(wiki_history))
        .route(
            &profile("wiki/{slug}"),
            get(get_wiki_page).put(put_wiki_page).delete(delete_wiki_page),
        )
        .route(&profile("callers"), get(caller_tokens).post(add_caller_token))
        .route(&profile("callers/{id}"), axum::routing::delete(revoke_caller_token))
        .route(&profile("inbound"), get(inbound_status))
        .route(&profile("webhooks/{name}/secret"), put(put_webhook_secret))
        .route(&profile("inbound-settings"), post(inbound_settings))
        .route(&profile("set-inbound-settings"), post(set_inbound_settings))
        .route(&profile("ard-registries"), post(ard_registries))
        .route(&profile("set-ard-registries"), post(set_ard_registries))
        .route(&profile("webhooks"), post(webhooks))
        .route(&profile("set-webhooks"), post(set_webhooks))
        .route(&profile("notes-settings"), post(notes_settings))
        .route(&profile("set-notes-settings"), post(set_notes_settings))
        .route(&profile("eval-settings"), post(eval_settings))
        .route(&profile("set-eval-settings"), post(set_eval_settings))
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
        .route(&profile("connections/connectors/{name}/api"), put(put_api_connector))
        .with_state(engine)
}

#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub listen: SocketAddr,
    /// PEM certificate chain and private key. Required unless `listen` is a loopback address.
    pub tls: Option<(PathBuf, PathBuf)>,
}

/// Who reaches an outer listener. The control API is on neither of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// Callers in the operator's own network.
    Internal,
    /// Callers on the internet.
    Public,
}

impl Reach {
    /// The endpoint scope that this listener serves at most.
    pub fn scope(self) -> InboundScope {
        match self {
            Reach::Internal => InboundScope::Internal,
            Reach::Public => InboundScope::Public,
        }
    }

    /// The word in the options of this listener, for example `--internal-listen`.
    pub fn word(self) -> &'static str {
        match self {
            Reach::Internal => "internal",
            Reach::Public => "public",
        }
    }
}

/// A listener for callers who are not profile owners. It has no `/v1` routes, so an access
/// token is of no use on it, and a caller token is of no use on the control API.
#[derive(Debug, Clone)]
pub struct OuterConfig {
    pub reach: Reach,
    pub listen: SocketAddr,
    /// The address that callers use, for example `https://agents.example.com`. It can differ from
    /// `listen` behind a proxy. Server cards name it, and the inbound MCP endpoint accepts only its host.
    pub url: Url,
    /// PEM certificate chain and private key. Required unless `listen` is a loopback address.
    pub tls: Option<(PathBuf, PathBuf)>,
}

impl OuterConfig {
    /// Refuses an address that other computers reach without TLS, and an address for callers
    /// that would send their tokens in clear text or carry more than a location.
    pub fn check(&self) -> Result<(), ServeError> {
        if self.tls.is_none() && !self.listen.ip().is_loopback() {
            return Err(ServeError::OuterTlsRequired(self.reach.word(), self.listen));
        }
        let url = &self.url;
        let bad = |why: &str| Err(ServeError::BadUrl(self.reach.word(), why.to_owned()));
        let loopback = match url.host() {
            Some(url::Host::Domain(name)) => name == "localhost",
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            None => return bad("it has no host"),
        };
        match url.scheme() {
            "https" => {}
            "http" if loopback => {}
            _ => return bad("use https://, or http:// for this machine only"),
        }
        if !url.username().is_empty() || url.password().is_some() || url.query().is_some() || url.fragment().is_some() {
            return bad("it must not hold a user name, password, query or fragment");
        }
        Ok(())
    }

    /// The address that callers use for `path`, which starts with `/`.
    pub fn url_of(&self, path: &str) -> String {
        format!("{}{path}", self.url.as_str().trim_end_matches('/'))
    }
}

/// Checks all listeners before the engine opens, so a refused start touches no key.
/// Each outer reach is given once, and no two listeners share a fixed address.
pub fn check_listeners(control: &ServeConfig, outer: &[OuterConfig]) -> Result<(), ServeError> {
    control.check()?;
    let mut fixed = vec![control.listen];
    for (index, listener) in outer.iter().enumerate() {
        listener.check()?;
        if outer[..index].iter().any(|other| other.reach == listener.reach) {
            return Err(ServeError::TwoListeners(listener.reach.word()));
        }
        // Port 0 asks the system for a free port, so such addresses never clash.
        if listener.listen.port() != 0 && fixed.contains(&listener.listen) {
            return Err(ServeError::SameAddress(listener.listen));
        }
        fixed.push(listener.listen);
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(
        "{0} is reachable from other machines, so TLS is required: give --tls-cert and --tls-key, \
         or listen on 127.0.0.1 behind a TLS proxy"
    )]
    TlsRequired(SocketAddr),
    #[error(
        "{1} is reachable from other machines, so TLS is required: give --{0}-tls-cert and --{0}-tls-key, \
         or listen on 127.0.0.1 behind a TLS proxy"
    )]
    OuterTlsRequired(&'static str, SocketAddr),
    #[error("--{0}-url cannot be used: {1}")]
    BadUrl(&'static str, String),
    #[error("there is more than one {0} listener")]
    TwoListeners(&'static str),
    #[error("two listeners cannot share the address {0}: the control API must stay apart from the other callers")]
    SameAddress(SocketAddr),
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
        self.finish().await
    }

    /// Waits until the server task ends after `graceful_shutdown`.
    async fn finish(self) -> Result<(), ServeError> {
        self.task.await.map_err(std::io::Error::other)??;
        Ok(())
    }
}

/// The listeners and the background work of a started server.
pub struct Started {
    pub control: Running,
    /// In the order of the outer configs that `start` got.
    pub outer: Vec<Running>,
    pub poller: Poller,
}

impl Started {
    /// Ends the background work, then lets the running requests of every listener finish.
    pub async fn stop(self) -> Result<(), ServeError> {
        self.poller.stop().await;
        let listeners: Vec<Running> = std::iter::once(self.control).chain(self.outer).collect();
        // All listeners stop taking requests at once, so the grace runs in parallel.
        for listener in &listeners {
            listener.handle.graceful_shutdown(Some(SHUTDOWN_GRACE));
        }
        let mut result = Ok(());
        for listener in listeners {
            if let Err(error) = listener.finish().await {
                result = Err(error);
            }
        }
        result
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
    bind(router(engine), config.listen, config.tls).await
}

/// Starts an outer listener and returns when it is listening. Nothing of the control API is on it.
/// A question that a caller asks there, or a stored webhook delivery, signals `wake`.
pub async fn serve_outer(engine: Arc<Engine>, config: OuterConfig, wake: Arc<Notify>) -> Result<Running, ServeError> {
    config.check()?;
    // The catalogs and the owner's inbound status name the address that callers use.
    engine.add_outer_listener(bunsin_engine::OuterListener {
        scope: config.reach.scope(),
        url: config.url.as_str().to_owned(),
    });
    let mut router = crate::inbound::routes(engine.clone(), &config, wake.clone()).merge(crate::webhooks::routes(
        engine.clone(),
        &config,
        wake,
    ));
    // The registry routes exist only on a listener that serves the registry. Elsewhere they are 404.
    if engine.registry_serves(config.reach.scope()) {
        router = router.merge(crate::registry::routes(engine, &config));
    }
    bind(router, config.listen, config.tls).await
}

async fn bind(router: Router, listen: SocketAddr, tls: Option<(PathBuf, PathBuf)>) -> Result<Running, ServeError> {
    let app = router.into_make_service();
    let handle = Handle::new();
    let task = match &tls {
        Some((cert, key)) => {
            let rustls = RustlsConfig::from_pem_file(cert, key).await.map_err(ServeError::Tls)?;
            tokio::spawn(
                axum_server::bind_rustls(listen, rustls)
                    .handle(handle.clone())
                    .serve(app),
            )
        }
        None => tokio::spawn(axum_server::bind(listen).handle(handle.clone()).serve(app)),
    };
    let tls = tls.is_some();
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

/// Starts the server and the poll task, after the work that the last stop cut is sorted out:
///
/// 1. Each `executing` item becomes `unknown`. It is never run again.
/// 2. The control API and the outer listeners take requests.
/// 3. Each `approved` tool call that waited at step 1 runs if the policy allows it now.
///    Otherwise a pending copy asks the owner.
/// 4. The poll task starts.
///
/// A profile that fails at step 1 or 3 is written to stderr and does not stop the start.
pub async fn start(engine: Arc<Engine>, config: ServeConfig, outer: Vec<OuterConfig>) -> Result<Started, ServeError> {
    check_listeners(&config, &outer)?;
    // The inbound endpoints wake the intake task when a question arrives.
    let wake = Arc::new(Notify::new());
    let interrupted = engine.mark_interrupted();
    for (profile, error) in &interrupted.failures {
        eprintln!("cut calls of profile {profile} were not checked: {error}");
    }
    let control = serve(engine.clone(), config).await?;
    let mut listeners = Vec::new();
    for listener in outer.clone() {
        listeners.push(serve_outer(engine.clone(), listener, wake.clone()).await?);
    }
    for line in crate::registry::svcb_records(&engine, &outer) {
        eprintln!("{line}");
    }
    for (profile, error) in engine.resume_approved(interrupted).await {
        eprintln!("an approved call of profile {profile} was not resumed: {error}");
    }
    Ok(Started {
        control,
        outer: listeners,
        poller: Poller::start_with(engine, wake),
    })
}

/// How often the server looks for polls that are due.
const POLL_TICK: Duration = Duration::from_secs(5);

/// The background tasks: one repeats the searches of all profiles, one hands stored questions
/// to the agent, and one keeps the registry index up to date if the registry is on. A stop ends all.
pub struct Poller {
    stop: tokio::sync::watch::Sender<bool>,
    task: tokio::task::JoinHandle<()>,
    intake: tokio::task::JoinHandle<()>,
    registry: Option<tokio::task::JoinHandle<()>>,
}

impl Poller {
    /// Starts polling. A failure of one profile is written to stderr and does not stop the task.
    pub fn start(engine: Arc<Engine>) -> Self {
        Self::start_with(engine, Arc::new(Notify::new()))
    }

    /// Like `start`. A signal on `wake` makes the intake task run at once instead of at its next tick.
    pub fn start_with(engine: Arc<Engine>, wake: Arc<Notify>) -> Self {
        let (stop, mut stopped) = tokio::sync::watch::channel(false);
        let mut intake_stopped = stopped.clone();
        let drainer = engine.clone();
        // The internal listener serves every registry, so this is true if the registry is on.
        let registry = engine.registry_serves(InboundScope::Internal).then(|| {
            let indexer = engine.clone();
            let mut registry_stopped = stopped.clone();
            tokio::spawn(async move {
                loop {
                    crate::crawl::tick(&indexer);
                    tokio::select! {
                        _ = tokio::time::sleep(crate::crawl::CRAWL_TICK) => {}
                        _ = registry_stopped.changed() => break,
                    }
                }
            })
        });
        // One task drains, so a stored question is never handed to the agent twice at once.
        let intake = tokio::spawn(async move {
            loop {
                for (profile, error) in drainer.drain_intake().await {
                    eprintln!("a stored question of profile {profile} failed: {error}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL_TICK) => {}
                    _ = wake.notified() => {}
                    _ = intake_stopped.changed() => break,
                }
            }
        });
        let task = tokio::spawn(async move {
            loop {
                for (profile, error) in engine.poll_all().await {
                    eprintln!("poll of profile {profile} failed: {error}");
                }
                for (profile, error) in engine.background_tick().await {
                    eprintln!("the background work of profile {profile} failed: {error}");
                }
                tokio::select! {
                    _ = tokio::time::sleep(POLL_TICK) => {}
                    _ = stopped.changed() => break,
                }
            }
        });
        Self {
            stop,
            task,
            intake,
            registry,
        }
    }

    /// Lets the running cycles finish, then ends the tasks.
    pub async fn stop(self) {
        let _ = self.stop.send(true);
        let all = async {
            let _ = self.task.await;
            let _ = self.intake.await;
            if let Some(registry) = self.registry {
                let _ = registry.await;
            }
        };
        if tokio::time::timeout(SHUTDOWN_GRACE, all).await.is_err() {
            eprintln!("the background tasks did not finish in {} s", SHUTDOWN_GRACE.as_secs());
        }
    }
}
