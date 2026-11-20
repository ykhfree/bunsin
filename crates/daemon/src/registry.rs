//! The ARD registry on the outer listeners (§5.3, ARCHITECTURE 18): `POST /search`,
//! `POST /explore` and `GET /agents` at the listener root, so the listener address is the base URL.
//!
//! The routes need no token. Each listener answers at most [`MAX_REQUESTS_PER_MINUTE`] requests a
//! minute. Every answer allows any origin, and every error has the body `{errorCode, message}`
//! of Appendix B.

use crate::server::OuterConfig;
use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bunsin_api::{ApiError, ApiResult};
use bunsin_ard::{ErrorCode, QueryError, parse_explore, parse_list, parse_search};
use bunsin_core::inbound::InboundScope;
use bunsin_engine::Engine;
use serde::Serialize;
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The largest request body.
pub const MAX_BODY_BYTES: usize = 64 * 1024;
/// How many registry requests one listener answers in a minute. More get 429. It counts in memory only.
pub const MAX_REQUESTS_PER_MINUTE: usize = 600;

struct Registry {
    engine: Arc<Engine>,
    listener: InboundScope,
    /// The times of the requests of the last minute.
    recent: Mutex<VecDeque<Instant>>,
}

impl Registry {
    /// True if the listener may answer one more request now. A true answer counts it.
    fn may_answer(&self) -> bool {
        let now = Instant::now();
        let mut recent = self.recent.lock().expect("registry lock");
        while recent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Duration::from_secs(60))
        {
            recent.pop_front();
        }
        if recent.len() >= MAX_REQUESTS_PER_MINUTE {
            return false;
        }
        recent.push_back(now);
        true
    }
}

/// The registry routes of one outer listener. Mount them only where the engine serves the registry.
pub fn routes(engine: Arc<Engine>, listener: &OuterConfig) -> Router {
    let state = Arc::new(Registry {
        engine,
        listener: listener.reach.scope(),
        recent: Mutex::new(VecDeque::new()),
    });
    Router::new()
        .route("/search", post(search).options(preflight))
        .route("/explore", post(explore).options(preflight))
        .route("/agents", get(list).options(preflight))
        .layer(from_fn_with_state(state.clone(), guard))
        .with_state(state)
}

/// Counts the request against the limit of the listener, and lets any origin read the answer.
async fn guard(State(state): State<Arc<Registry>>, request: Request, next: Next) -> Response {
    let mut response = if state.may_answer() {
        next.run(request).await
    } else {
        failure(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::RateLimitExceeded,
            format!("this registry answers {MAX_REQUESTS_PER_MINUTE} requests a minute; try again later"),
        )
    };
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    response
}

/// The answer to a browser's preflight request: a JSON `POST` from another origin needs one.
async fn preflight() -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    let headers = response.headers_mut();
    for (name, value) in [
        (header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST"),
        (header::ACCESS_CONTROL_ALLOW_HEADERS, "content-type"),
        (header::ACCESS_CONTROL_MAX_AGE, "600"),
    ] {
        headers.insert(name, HeaderValue::from_static(value));
    }
    response
}

/// An error answer of Appendix B.
fn failure(status: StatusCode, code: ErrorCode, message: String) -> Response {
    let body = json!({"errorCode": code.to_string(), "message": message});
    (status, axum::Json(body)).into_response()
}

fn invalid(error: QueryError) -> Response {
    failure(StatusCode::BAD_REQUEST, ErrorCode::InvalidArgument, error.0)
}

/// The JSON answer of the engine, or its error. An internal failure says nothing about the server.
fn answer<T: Serialize>(result: ApiResult<T>) -> Response {
    match result {
        Ok(body) => axum::Json(body).into_response(),
        Err(ApiError::Invalid(message)) => invalid(QueryError(message)),
        Err(ApiError::NotFound(message)) => failure(StatusCode::NOT_FOUND, ErrorCode::NotFound, message),
        Err(error) => {
            eprintln!("the registry failed to answer: {error}");
            failure(
                StatusCode::INTERNAL_SERVER_ERROR,
                ErrorCode::InternalError,
                "the registry could not answer; try again later".into(),
            )
        }
    }
}

/// Reads at most [`MAX_BODY_BYTES`]. A larger body is a request error, so it gets 400.
async fn read(body: Body) -> Result<Bytes, QueryError> {
    axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| QueryError(format!("the body is larger than {} KiB", MAX_BODY_BYTES / 1024)))
}

async fn search(State(state): State<Arc<Registry>>, body: Body) -> Response {
    match read(body).await.and_then(|bytes| parse_search(&bytes)) {
        Ok(request) => answer(state.engine.registry_search(state.listener, &request)),
        Err(error) => invalid(error),
    }
}

async fn explore(State(state): State<Arc<Registry>>, body: Body) -> Response {
    match read(body).await.and_then(|bytes| parse_explore(&bytes)) {
        Ok(request) => answer(state.engine.registry_explore(state.listener, &request)),
        Err(error) => invalid(error),
    }
}

async fn list(State(state): State<Arc<Registry>>, uri: Uri) -> Response {
    let pairs = url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()).into_owned();
    match parse_list(pairs) {
        Ok(request) => answer(state.engine.registry_list(state.listener, &request)),
        Err(error) => invalid(error),
    }
}

/// The DNS records that let a client find the registry of a domain (§5.1), one line per listener
/// that lists the registry. The server does not publish them; the operator does.
///
/// A port other than 443 goes into the record. A listener address with a path gets a warning
/// instead, because a record cannot carry a path.
pub fn svcb_records(engine: &Engine, outer: &[OuterConfig]) -> Vec<String> {
    let mut lines = Vec::new();
    for listener in outer {
        let Some(domain) = engine.registry_publisher(listener.reach.scope()) else {
            continue;
        };
        let word = listener.reach.word();
        if listener.url.path() != "/" {
            lines.push(format!(
                "warning: --{word}-url has a path, so no DNS record can name the registry of the {word} listener"
            ));
            continue;
        }
        let host = listener.url.host_str().unwrap_or_default();
        let port = match listener.url.port_or_known_default() {
            Some(443) | None => String::new(),
            Some(port) => format!(" port={port}"),
        };
        lines.push(format!(
            "_search._agents.{domain}. 3600 IN SVCB 1 {host}.{port} ; the registry of the {word} listener"
        ));
    }
    lines
}
