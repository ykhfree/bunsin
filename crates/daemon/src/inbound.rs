//! The inbound MCP endpoint of each profile and its server card, on the outer listeners.
//!
//! `POST /agents/{p}/mcp` is a stateless MCP server with two tools: `ask` and `get_request`.
//! A caller token of profile `p` opens it. `GET /agents/{p}/mcp/server-card` describes it without a token.
//! A listener serves a profile only if the profile turned the endpoint on for that listener's reach.
//! Each listener also serves its ARD catalog, and a `robots.txt` whose `Agentmap` names the catalog.
//! The A2A endpoint (`a2a.rs`) is on the same routes and shares the caller check and the limits.

use crate::server::{OuterConfig, Reach};
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use bunsin_api::http::{ARD_CATALOG, inbound_mcp_path};
use bunsin_api::{ApiError, EngineApi, ProfileConfig};
use bunsin_core::inbound::MAX_QUESTION_CHARS;
use bunsin_core::records::Caller;
use bunsin_engine::Engine;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, PaginatedRequestParams,
    ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{Notify, Semaphore};

/// The largest request body. A question has at most 4,000 characters, so this leaves room.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// How many requests one listener runs at the same time. More get 503.
pub const MAX_CONCURRENT: usize = 64;
/// How many `get_request` calls one caller may make in a minute. It counts in memory only.
pub const MAX_READS_PER_MINUTE: usize = 120;
/// The schema of the server card (MCP server card extension, snapshot 526201bb).
const CARD_SCHEMA: &str = "https://static.modelcontextprotocol.io/schemas/v1/server-card.schema.json";
const CARD_TYPE: &str = "application/mcp-server-card+json";

/// True if the listener of `reach` serves the inbound endpoint of a profile with these settings.
/// The routes and the server card use this one check.
pub fn served_on(config: &ProfileConfig, reach: Reach) -> bool {
    config.inbound.served_on(reach.scope())
}

/// The state of the inbound endpoints of one listener. MCP and A2A share it, and so share its limits.
pub(crate) struct Inbound {
    pub(crate) engine: Arc<Engine>,
    pub(crate) listener: OuterConfig,
    /// Wakes the task that hands stored questions to the agent.
    pub(crate) wake: Arc<Notify>,
    running: Arc<Semaphore>,
    /// The times of each caller's reads in the last minute, by caller token ID: `get_request`
    /// and the A2A methods `GetTask`, `ListTasks` and `CancelTask`.
    reads: Mutex<HashMap<i64, VecDeque<Instant>>>,
}

impl Inbound {
    /// True if the caller may read once more now. A true answer counts the read.
    pub(crate) fn may_read(&self, caller: i64) -> bool {
        let now = Instant::now();
        let mut reads = self.reads.lock().expect("inbound lock");
        let times = reads.entry(caller).or_default();
        while times
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            times.pop_front();
        }
        if times.len() >= MAX_READS_PER_MINUTE {
            return false;
        }
        times.push_back(now);
        true
    }
}

/// The routes of the inbound endpoints for one outer listener.
pub fn routes(engine: Arc<Engine>, listener: &OuterConfig, wake: Arc<Notify>) -> Router {
    let state = Arc::new(Inbound {
        engine,
        listener: listener.clone(),
        wake,
        running: Arc::new(Semaphore::new(MAX_CONCURRENT)),
        reads: Mutex::new(HashMap::new()),
    });
    // `OuterConfig::check` makes sure the address has a host. Only that host may be in `Host`.
    let host = listener.url.host_str().unwrap_or_default().to_owned();
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts([host])
        .with_max_request_body_bytes(MAX_BODY_BYTES);
    let handler = state.clone();
    // Stateless: one POST is one answer, so no stream outlives a revoked token or the stop.
    let service = StreamableHttpService::new(
        move || Ok(InboundServer(handler.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new()
        .route_service("/agents/{profile}/mcp", service)
        .route_layer(from_fn_with_state(state.clone(), authenticate))
        .route("/agents/{profile}/mcp/server-card", get(server_card))
        .route(ARD_CATALOG, get(ard_catalog))
        .route("/robots.txt", get(robots))
        .with_state(state.clone())
        .merge(crate::a2a::routes(state))
}

async fn authenticate(
    State(state): State<Arc<Inbound>>,
    Path(profile): Path<String>,
    request: Request,
    next: Next,
) -> Response {
    admit(&state, &profile, served_on, request, next).await
}

/// Lets a request through only with a valid caller token of the profile in the path, and only if
/// `served` holds for this listener. Every refusal is the same 401, so a caller learns nothing about profiles.
pub(crate) async fn admit(
    state: &Inbound,
    profile: &str,
    served: fn(&ProfileConfig, Reach) -> bool,
    mut request: Request,
    next: Next,
) -> Response {
    let token = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::to_owned);
    let caller = match token.map(|token| state.engine.app_store().caller(&token)) {
        Some(Ok(Some(caller))) if caller.profile == profile => caller,
        _ => return refused(),
    };
    match state.engine.profile_config(profile).await {
        Ok(config) if served(&config, state.listener.reach) => {}
        _ => return refused(),
    }
    let Ok(_permit) = state.running.clone().try_acquire_owned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "too many requests run now; try again later",
        )
            .into_response();
    };
    request.extensions_mut().insert(caller);
    next.run(request).await
}

fn refused() -> Response {
    let body = json!({"error": "a valid caller token of this agent is required"});
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(body),
    )
        .into_response()
}

/// A card that anyone may read: any origin, a cache of five minutes, and an ETag of the body.
/// The same ETag in `If-None-Match` gets 304 (RFC 9110 §13.1.2).
pub(crate) fn card_response(body: String, content_type: &str, request: &HeaderMap) -> Response {
    let etag = format!("\"{}\"", bunsin_core::sha256_hex(body.as_bytes()));
    // A client that holds this card asks with its ETag (A2A §8.6.2). It gets 304 without the body.
    let unchanged = request
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|tag| tag.trim().trim_start_matches("W/"))
        .any(|tag| tag == etag || tag == "*");
    let mut response = if unchanged {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        body.into_response()
    };
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, content_type),
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (header::CACHE_CONTROL, "public, max-age=300"),
        (header::ETAG, &etag),
    ] {
        headers.insert(name, HeaderValue::from_str(value).expect("a valid header value"));
    }
    response
}

/// The server card of a profile's endpoint. It holds no token and no owner name.
async fn server_card(State(state): State<Arc<Inbound>>, Path(profile): Path<String>, request: HeaderMap) -> Response {
    let config = match state.engine.profile_config(&profile).await {
        Ok(config) if served_on(&config, state.listener.reach) => config,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let mut card = json!({
        "$schema": CARD_SCHEMA,
        "name": card_name(&state.listener, &profile),
        "version": env!("CARGO_PKG_VERSION"),
        "description": config.inbound.description.trim(),
        "remotes": [{
            "type": "streamable-http",
            "url": state.listener.url_of(&inbound_mcp_path(&profile)),
            "headers": [{
                "name": "Authorization",
                "description": "Bearer and the caller token that the owner of this agent gave you",
                "isRequired": true,
                "isSecret": true
            }]
        }]
    });
    if let Some(title) = &config.inbound.title {
        card["title"] = json!(title.trim());
    }
    card_response(card.to_string(), CARD_TYPE, &request)
}

/// The ARD catalog of this listener: the profiles that it serves, each with the server card on this
/// listener. It has no token and no owner name.
async fn ard_catalog(State(state): State<Arc<Inbound>>) -> Response {
    let body = state.engine.ard_catalog(state.listener.reach.scope()).body();
    let mut response = body.into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "application/json"),
        (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        (header::CACHE_CONTROL, "public, max-age=300"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// The robots.txt of this listener: every crawler may read it, and `Agentmap` names its ARD catalog (§5.1).
async fn robots(State(state): State<Arc<Inbound>>) -> Response {
    let catalog = state.listener.url_of(ARD_CATALOG);
    let mut response = format!("User-agent: *\nDisallow:\n\nAgentmap: {catalog}\n").into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
        (header::CACHE_CONTROL, "public, max-age=300"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// The card name: the host of the listener's address with its labels reversed, then the profile,
/// for example `com.example.agents/kim`. Characters outside the card's name pattern become `-`.
fn card_name(listener: &OuterConfig, profile: &str) -> String {
    let host = listener.url.host_str().unwrap_or_default();
    let reversed: Vec<&str> = host.split('.').rev().collect();
    let clean = |text: &str, extra: &[char]| -> String {
        text.chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || extra.contains(&c) {
                    c
                } else {
                    '-'
                }
            })
            .collect()
    };
    format!(
        "{}/{}",
        clean(&reversed.join("."), &['.', '-']),
        clean(profile, &['.', '_', '-'])
    )
}

#[derive(Clone)]
struct InboundServer(Arc<Inbound>);

fn tool(name: &'static str, description: &'static str, schema: Value) -> Tool {
    let Value::Object(schema) = schema else {
        unreachable!("the schemas below are objects")
    };
    Tool::new(name, description, schema)
}

fn text_arg(args: &Option<Map<String, Value>>, name: &str) -> Option<String> {
    args.as_ref()?.get(name)?.as_str().map(str::to_owned)
}

/// A tool error the caller can read. An internal failure says nothing about the server.
fn failed(error: ApiError) -> CallToolResult {
    let text = match error {
        ApiError::Internal(_) => "the server could not handle the request; try again later".to_owned(),
        other => other.to_string(),
    };
    CallToolResult::error(vec![ContentBlock::text(text)])
}

impl ServerHandler for InboundServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult::with_all_items(vec![
            tool(
                "ask",
                "Ask the agent a question. It answers later, after its owner approves the answer. \
                 Returns a request_id; read the answer with get_request.",
                json!({"type": "object", "properties": {"question": {"type": "string", "maxLength": MAX_QUESTION_CHARS}},
                    "required": ["question"]}),
            ),
            tool(
                "get_request",
                "Read the state of a question you asked: working, pending_approval, answered (with the answer), \
                 declined, failed or canceled.",
                json!({"type": "object", "properties": {"request_id": {"type": "string"}}, "required": ["request_id"]}),
            ),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let state = &self.0;
        // The middleware put the caller there. Without it the request did not pass the check.
        let caller = context
            .extensions
            .get::<axum::http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<Caller>())
            .cloned()
            .ok_or_else(|| McpError::invalid_request("no caller", None))?;
        let result = match request.name.as_ref() {
            "ask" => match text_arg(&request.arguments, "question") {
                None => failed(ApiError::Invalid("`question` must be a string".into())),
                Some(question) => match state.engine.ask(&caller, &question) {
                    Ok(asked) => {
                        state.wake.notify_one();
                        CallToolResult::structured(json!(asked))
                    }
                    Err(error) => failed(error),
                },
            },
            "get_request" => match text_arg(&request.arguments, "request_id") {
                None => failed(ApiError::Invalid("`request_id` must be a string".into())),
                Some(_) if !state.may_read(caller.id) => failed(ApiError::NotAllowed(format!(
                    "a caller may read {MAX_READS_PER_MINUTE} times a minute; wait a little"
                ))),
                Some(id) => match state.engine.request(&caller, &id) {
                    Ok(found) => CallToolResult::structured(json!(found)),
                    Err(error) => failed(error),
                },
            },
            other => failed(ApiError::NotFound(format!("tool `{other}`"))),
        };
        Ok(result.into())
    }
}
