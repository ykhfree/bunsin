//! The A2A v1.0 endpoint of each profile on the outer listeners, next to its MCP endpoint.
//!
//! `POST /agents/{p}/a2a` takes JSON-RPC 2.0 calls (A2A §9). `GET /agents/{p}/a2a/agent-card.json`
//! describes it without a token. A listener serves both only where `InboundConfig::a2a_served_on` holds.
//! A task is a caller's request. Its state comes only from `request_status`, so the caller sees the
//! owner's decisions and never the agent's work.

use crate::inbound::{Inbound, MAX_BODY_BYTES, MAX_READS_PER_MINUTE, admit, card_response};
use crate::server::{OuterConfig, Reach};
use a2a::error_code::{
    CONTENT_TYPE_NOT_SUPPORTED, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST, METHOD_NOT_FOUND, PARSE_ERROR,
    PUSH_NOTIFICATION_NOT_SUPPORTED, TASK_NOT_CANCELABLE, TASK_NOT_FOUND, UNSUPPORTED_OPERATION, VERSION_NOT_SUPPORTED,
};
use a2a::{
    AgentCapabilities, AgentCard, AgentInterface, AgentSkill, Artifact, CancelTaskRequest, GetTaskRequest,
    HttpAuthSecurityScheme, JsonRpcError, JsonRpcId, JsonRpcRequest, JsonRpcResponse, ListTasksRequest,
    ListTasksResponse, Message, Part, PartContent, Role, SecurityScheme, SendMessageConfiguration, SendMessageRequest,
    SendMessageResponse, Task, TaskState, TaskStatus, TypedDetail, methods,
};
use axum::Router;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{Next, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bunsin_api::http::inbound_a2a_path;
use bunsin_api::{ApiError, EngineApi, ProfileConfig};
use bunsin_core::inbound::{CallerRequest, RequestStatus};
use bunsin_core::records::Caller;
use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// The only A2A version (Major.Minor) that the endpoint speaks.
const VERSION: &str = "1.0";
/// A SendMessage must carry `configuration.returnImmediately: true` (ARCHITECTURE 7, "A2A endpoint").
/// A2A §3.2.2 makes any other SendMessage wait for a final state, and the owner may decide days later.
const BLOCKING_REFUSED: &str = "send configuration.returnImmediately: true and poll GetTask";
/// The fixed text of a task that waits for the owner.
const WORKING_TEXT: &str = "The owner of this agent approves every answer. Poll GetTask.";
const REJECTED_TEXT: &str = "The owner of this agent declined the question.";
const FAILED_TEXT: &str = "The approved answer could not be delivered.";
/// The output modes that a text answer fits.
const TEXT_MODES: [&str; 3] = ["text/plain", "text/*", "*/*"];
const MAX_CONTEXT_ID_CHARS: usize = 128;
const DEFAULT_PAGE_SIZE: i32 = 50;
const MAX_PAGE_SIZE: i32 = 100;
/// The name of the caller token in the security schemes of the card.
const TOKEN_SCHEME: &str = "callerToken";

/// True if the listener of `reach` serves the A2A endpoint and the card of a profile with these settings.
pub fn a2a_served_on(config: &ProfileConfig, reach: Reach) -> bool {
    config.inbound.a2a_served_on(reach.scope())
}

/// The A2A routes of one outer listener. `state` is the one that the MCP routes of the listener use.
pub(crate) fn routes(state: Arc<Inbound>) -> Router {
    Router::new()
        .route("/agents/{profile}/a2a", post(rpc))
        .route_layer(from_fn_with_state(state.clone(), authenticate))
        .route("/agents/{profile}/a2a/agent-card.json", get(agent_card))
        .with_state(state)
}

/// The caller check of the MCP endpoint, with the A2A switch of the profile.
async fn authenticate(
    State(state): State<Arc<Inbound>>,
    Path(profile): Path<String>,
    request: Request,
    next: Next,
) -> Response {
    admit(&state, &profile, a2a_served_on, request, next).await
}

/// The Agent Card of a profile (A2A §4.4.1). It holds no token and no owner name.
async fn agent_card(State(state): State<Arc<Inbound>>, Path(profile): Path<String>) -> Response {
    let config = match state.engine.profile_config(&profile).await {
        Ok(config) if a2a_served_on(&config, state.listener.reach) => config,
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    let inbound = &config.inbound;
    let examples = inbound
        .ard
        .as_ref()
        .map(|ard| ard.representative_queries.clone())
        .filter(|queries| !queries.is_empty());
    // A skill needs at least one tag (§5.7). Without ARD tags the card names the kind of skill.
    let tags = inbound
        .ard
        .as_ref()
        .map(|ard| ard.tags.iter().map(|tag| tag.trim().to_owned()).collect::<Vec<_>>())
        .filter(|tags| !tags.is_empty())
        .unwrap_or_else(|| vec!["questions".into()]);
    let skill = AgentSkill {
        id: "ask".into(),
        name: "Ask a question".into(),
        description: "Ask a question in plain text. The answer comes later as the artifact `answer` of the task. \
                      The owner approves every answer."
            .into(),
        tags,
        examples,
        input_modes: None,
        output_modes: None,
        security_requirements: None,
    };
    let scheme = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
        scheme: "Bearer".into(),
        description: Some("The caller token that the owner of this agent gave you".into()),
        bearer_format: None,
    });
    let card = AgentCard {
        name: inbound.title.as_deref().map_or(profile.as_str(), str::trim).to_owned(),
        description: inbound.description.trim().to_owned(),
        version: env!("CARGO_PKG_VERSION").into(),
        supported_interfaces: vec![AgentInterface::new(
            state.listener.url_of(&inbound_a2a_path(&profile)),
            a2a::TRANSPORT_PROTOCOL_JSONRPC,
        )],
        capabilities: AgentCapabilities {
            streaming: Some(false),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: Some(false),
        },
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![skill],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: Some(HashMap::from([(TOKEN_SCHEME.to_owned(), scheme)])),
        security_requirements: Some(vec![HashMap::from([(TOKEN_SCHEME.to_owned(), Vec::new())])]),
        signatures: None,
    };
    let body = serde_json::to_string(&card).expect("a card is JSON");
    card_response(body, "application/json")
}

/// One JSON-RPC call. The middleware let the caller in. Then come the host, the body, the
/// envelope, the version and the method, in this order. A JSON-RPC error is an HTTP 200 answer.
async fn rpc(State(state): State<Arc<Inbound>>, request: Request) -> Response {
    if !host_allowed(&state.listener, request.headers()) {
        return (StatusCode::FORBIDDEN, "Forbidden: Host header is not allowed").into_response();
    }
    let Some(caller) = request.extensions().get::<Caller>().cloned() else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let version = requested_version(&request);
    let Ok(body) = axum::body::to_bytes(request.into_body(), MAX_BODY_BYTES).await else {
        return (StatusCode::PAYLOAD_TOO_LARGE, "the body is larger than 64 KiB").into_response();
    };
    axum::Json(answer(&state, &caller, &version, &body)).into_response()
}

/// True if `Host` names the host of the listener's address, with any port. It keeps a DNS
/// rebinding page away, like the MCP endpoint does.
fn host_allowed(listener: &OuterConfig, headers: &HeaderMap) -> bool {
    let brackets: &[char] = &['[', ']'];
    let expected = listener.url.host_str().unwrap_or_default().trim_matches(brackets);
    headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<axum::http::uri::Authority>().ok())
        .is_some_and(|authority| authority.host().trim_matches(brackets).eq_ignore_ascii_case(expected))
}

/// The `A2A-Version` header, or else the query parameter of that name (§3.6.1). Empty if neither.
fn requested_version(request: &Request) -> String {
    if let Some(value) = request.headers().get(a2a::SVC_PARAM_VERSION) {
        return value.to_str().unwrap_or_default().trim().to_owned();
    }
    let query = request.uri().query().unwrap_or_default();
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name.eq_ignore_ascii_case(a2a::SVC_PARAM_VERSION))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_default()
}

/// Accepts only 1.0. The patch number does not count, and an empty value means 0.3 (§3.6).
fn check_version(version: &str) -> Result<(), JsonRpcError> {
    let mut numbers = version.split('.');
    let major_minor = format!(
        "{}.{}",
        numbers.next().unwrap_or_default(),
        numbers.next().unwrap_or_default()
    );
    if major_minor == VERSION {
        return Ok(());
    }
    let asked = if version.is_empty() { "0.3" } else { version };
    let metadata = HashMap::from([("supportedVersions".to_owned(), VERSION.to_owned())]);
    Err(error_with(
        VERSION_NOT_SUPPORTED,
        format!("A2A version {asked} is not supported; send the header A2A-Version: {VERSION}"),
        "VERSION_NOT_SUPPORTED",
        Some(metadata),
    ))
}

fn answer(state: &Inbound, caller: &Caller, version: &str, body: &[u8]) -> JsonRpcResponse {
    let Ok(value) = serde_json::from_slice::<Value>(body) else {
        return JsonRpcResponse::error(JsonRpcId::Null, rpc_error(PARSE_ERROR, "the body is not JSON"));
    };
    let id = value
        .get("id")
        .and_then(|id| serde_json::from_value(id.clone()).ok())
        .unwrap_or(JsonRpcId::Null);
    // A batch, a notification without an ID, and another JSON-RPC version are not taken.
    let request = match serde_json::from_value::<JsonRpcRequest>(value) {
        Ok(request) if request.jsonrpc == "2.0" => request,
        _ => {
            let error = rpc_error(
                INVALID_REQUEST,
                "send one JSON-RPC 2.0 request with jsonrpc, id and method",
            );
            return JsonRpcResponse::error(id, error);
        }
    };
    let result = check_version(version).and_then(|()| {
        let params = request.params.unwrap_or_else(|| json!({}));
        dispatch(state, caller, &request.method, params)
    });
    match result {
        Ok(result) => JsonRpcResponse::success(request.id, result),
        Err(error) => JsonRpcResponse::error(request.id, error),
    }
}

fn dispatch(state: &Inbound, caller: &Caller, method: &str, params: Value) -> Result<Value, JsonRpcError> {
    match method {
        methods::SEND_MESSAGE => send_message(state, caller, params),
        methods::GET_TASK => {
            read_allowed(state, caller)?;
            let request: GetTaskRequest = params_of(params)?;
            let found = state.engine.caller_request(caller, &request.id).map_err(api_error)?;
            Ok(json!(task(&found)))
        }
        methods::LIST_TASKS => {
            read_allowed(state, caller)?;
            list_tasks(state, caller, params_of(params)?)
        }
        methods::CANCEL_TASK => {
            read_allowed(state, caller)?;
            let request: CancelTaskRequest = params_of(params)?;
            let found = state.engine.cancel_request(caller, &request.id).map_err(api_error)?;
            if found.status != RequestStatus::Canceled {
                let state = task_state(found.status);
                let message = format!("the task is in a final state and cannot be canceled: {state:?}");
                return Err(rpc_error(TASK_NOT_CANCELABLE, message));
            }
            Ok(json!(task(&found)))
        }
        methods::SEND_STREAMING_MESSAGE | methods::SUBSCRIBE_TO_TASK | methods::GET_EXTENDED_AGENT_CARD => Err(
            rpc_error(UNSUPPORTED_OPERATION, format!("this agent does not support `{method}`")),
        ),
        methods::CREATE_PUSH_CONFIG
        | methods::GET_PUSH_CONFIG
        | methods::LIST_PUSH_CONFIGS
        | methods::DELETE_PUSH_CONFIG => Err(rpc_error(
            PUSH_NOTIFICATION_NOT_SUPPORTED,
            "this agent sends no push notifications; poll GetTask",
        )),
        other => Err(rpc_error(METHOD_NOT_FOUND, format!("there is no method `{other}`"))),
    }
}

/// The rule of `BLOCKING_REFUSED`. It is the only place that reads `returnImmediately`.
fn refuse_blocking(configuration: Option<&SendMessageConfiguration>) -> Result<(), JsonRpcError> {
    match configuration.and_then(|configuration| configuration.return_immediately) {
        Some(true) => Ok(()),
        _ => Err(rpc_error(UNSUPPORTED_OPERATION, BLOCKING_REFUSED)),
    }
}

fn send_message(state: &Inbound, caller: &Caller, params: Value) -> Result<Value, JsonRpcError> {
    let request: SendMessageRequest = params_of(params)?;
    let configuration = request.configuration.as_ref();
    refuse_blocking(configuration)?;
    let message = &request.message;
    if message.role != Role::User {
        return Err(rpc_error(INVALID_PARAMS, "message.role must be ROLE_USER"));
    }
    if message.message_id.is_empty() {
        return Err(rpc_error(INVALID_PARAMS, "message.messageId is required"));
    }
    let question = message_text(message)?;
    let modes = configuration.and_then(|configuration| configuration.accepted_output_modes.as_deref());
    if let Some(modes) = modes.filter(|modes| !modes.is_empty())
        && !modes
            .iter()
            .any(|mode| TEXT_MODES.contains(&mode.trim().to_ascii_lowercase().as_str()))
    {
        return Err(rpc_error(
            CONTENT_TYPE_NOT_SUPPORTED,
            "this agent answers in text/plain only",
        ));
    }
    if configuration.is_some_and(|configuration| configuration.task_push_notification_config.is_some()) {
        return Err(rpc_error(
            PUSH_NOTIFICATION_NOT_SUPPORTED,
            "this agent sends no push notifications; poll GetTask",
        ));
    }
    if let Some(task_id) = &message.task_id {
        // Text added to a waiting question would change what the owner approves.
        state.engine.caller_request(caller, task_id).map_err(api_error)?;
        return Err(rpc_error(
            UNSUPPORTED_OPERATION,
            "a message cannot add to a task; send a new message without taskId",
        ));
    }
    if let Some(context) = &message.context_id {
        let fits = (1..=MAX_CONTEXT_ID_CHARS).contains(&context.len()) && context.bytes().all(|b| b.is_ascii_graphic());
        if !fits {
            let error = format!("message.contextId has 1 to {MAX_CONTEXT_ID_CHARS} visible ASCII characters");
            return Err(rpc_error(INVALID_PARAMS, error));
        }
    }
    let found = state
        .engine
        .send_message(caller, &question, &message.message_id, message.context_id.as_deref())
        .map_err(api_error)?;
    state.wake.notify_one();
    Ok(json!(SendMessageResponse::Task(task(&found))))
}

/// The question: the text parts, joined by line breaks. A part of another kind is refused, and a
/// `url` part is not fetched.
fn message_text(message: &Message) -> Result<String, JsonRpcError> {
    let mut texts = Vec::new();
    for part in &message.parts {
        let text_type = part.media_type.as_deref().is_none_or(|media| {
            let media = media.trim().to_ascii_lowercase();
            media.is_empty() || media.starts_with("text/")
        });
        match &part.content {
            PartContent::Text(text) if text_type => texts.push(text.as_str()),
            _ => {
                return Err(rpc_error(
                    CONTENT_TYPE_NOT_SUPPORTED,
                    "this agent reads text parts only and fetches no url",
                ));
            }
        }
    }
    Ok(texts.join("\n"))
}

fn list_tasks(state: &Inbound, caller: &Caller, request: ListTasksRequest) -> Result<Value, JsonRpcError> {
    let page_size = match request.page_size {
        None => DEFAULT_PAGE_SIZE,
        Some(size) if (1..=MAX_PAGE_SIZE).contains(&size) => size,
        Some(size) => {
            let error = format!("pageSize must be 1 to {MAX_PAGE_SIZE}, not {size}");
            return Err(rpc_error(INVALID_PARAMS, error));
        }
    };
    let after = match request.page_token.as_deref().filter(|token| !token.is_empty()) {
        Some(token) => Some(parse_cursor(token)?),
        None => None,
    };
    let status = request.status.filter(|status| *status != TaskState::Unspecified);
    let mut found: Vec<CallerRequest> = state
        .engine
        .caller_requests(caller)
        .map_err(api_error)?
        .into_iter()
        .filter(|found| status.as_ref().is_none_or(|status| *status == task_state(found.status)))
        .filter(|found| {
            request
                .context_id
                .as_ref()
                .is_none_or(|context| *context == found.context_id)
        })
        .filter(|found| {
            request
                .status_timestamp_after
                .is_none_or(|after| timestamp(found.status_at).is_some_and(|at| at >= after))
        })
        .collect();
    // The newest status first (§3.1.4). The request ID orders the tasks of one second.
    found.sort_by(|a, b| (b.status_at, &b.request_id).cmp(&(a.status_at, &a.request_id)));
    let total_size = found.len() as i32;
    let mut rest = found
        .into_iter()
        .filter(|found| {
            after
                .as_ref()
                .is_none_or(|(at, id)| (found.status_at, &found.request_id) < (*at, id))
        })
        .peekable();
    let page: Vec<CallerRequest> = rest.by_ref().take(page_size as usize).collect();
    let next_page_token = match (rest.peek(), page.last()) {
        (Some(_), Some(last)) => format!("{}:{}", last.status_at, last.request_id),
        _ => String::new(),
    };
    let include = request.include_artifacts == Some(true);
    let tasks = page
        .iter()
        .map(|found| {
            let mut task = task(found);
            // Without `includeArtifacts`, the field is left out entirely (§3.1.4).
            task.artifacts = include.then(|| task.artifacts.unwrap_or_default());
            task
        })
        .collect();
    Ok(json!(ListTasksResponse {
        tasks,
        next_page_token,
        page_size,
        total_size,
    }))
}

/// The status time and request ID of the last task of the previous page.
fn parse_cursor(token: &str) -> Result<(i64, String), JsonRpcError> {
    token
        .split_once(':')
        .and_then(|(at, id)| Some((at.parse().ok()?, id.to_owned())))
        .ok_or_else(|| rpc_error(INVALID_PARAMS, "pageToken is not a token of this list"))
}

fn task_state(status: RequestStatus) -> TaskState {
    match status {
        // The owner's approval is work inside the agent, not input that the client must give (§4.1.3).
        RequestStatus::Working | RequestStatus::PendingApproval => TaskState::Working,
        RequestStatus::Answered => TaskState::Completed,
        RequestStatus::Declined => TaskState::Rejected,
        RequestStatus::Failed => TaskState::Failed,
        RequestStatus::Canceled => TaskState::Canceled,
    }
}

fn timestamp(at: i64) -> Option<DateTime<Utc>> {
    DateTime::from_timestamp(at, 0)
}

/// The task of a request. It holds no history and no text of the agent: only fixed texts and the
/// answer that the owner approved.
fn task(found: &CallerRequest) -> Task {
    let (word, text) = match found.status {
        RequestStatus::Working | RequestStatus::PendingApproval => ("working", Some(WORKING_TEXT)),
        RequestStatus::Declined => ("rejected", Some(REJECTED_TEXT)),
        RequestStatus::Failed => ("failed", Some(FAILED_TEXT)),
        RequestStatus::Answered => ("completed", None),
        RequestStatus::Canceled => ("canceled", None),
    };
    let message = text.map(|text| Message {
        message_id: format!("{}-{word}", found.request_id),
        context_id: Some(found.context_id.clone()),
        task_id: Some(found.request_id.clone()),
        role: Role::Agent,
        parts: vec![Part::text(text)],
        metadata: None,
        extensions: None,
        reference_task_ids: None,
    });
    let artifacts = found.answer.as_ref().map(|answer| {
        vec![Artifact {
            artifact_id: "answer".into(),
            name: Some("answer".into()),
            description: None,
            parts: vec![Part::text(answer.as_str())],
            metadata: None,
            extensions: None,
        }]
    });
    Task {
        id: found.request_id.clone(),
        context_id: found.context_id.clone(),
        status: TaskStatus {
            state: task_state(found.status),
            message,
            timestamp: timestamp(found.status_at),
        },
        artifacts,
        history: None,
        metadata: None,
    }
}

/// Counts a read of the caller. The MCP tool `get_request` counts in the same place.
fn read_allowed(state: &Inbound, caller: &Caller) -> Result<(), JsonRpcError> {
    if state.may_read(caller.id) {
        return Ok(());
    }
    Err(limited(format!(
        "a caller may read {MAX_READS_PER_MINUTE} times a minute; wait a little"
    )))
}

fn params_of<T: DeserializeOwned>(params: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(params).map_err(|error| rpc_error(INVALID_PARAMS, format!("invalid params: {error}")))
}

/// An error of a limit: a JSON-RPC server error with the reason `RATE_LIMIT_EXCEEDED`.
fn limited(message: String) -> JsonRpcError {
    error_with(-32000, message, "RATE_LIMIT_EXCEEDED", None)
}

fn api_error(error: ApiError) -> JsonRpcError {
    match error {
        ApiError::Invalid(_) => rpc_error(INVALID_PARAMS, error.to_string()),
        ApiError::NotFound(_) => rpc_error(TASK_NOT_FOUND, error.to_string()),
        ApiError::NotAllowed(_) => limited(error.to_string()),
        // An internal failure says nothing about the server.
        _ => rpc_error(
            INTERNAL_ERROR,
            "the server could not handle the request; try again later",
        ),
    }
}

/// An error with the A2A reason of its code (§9.5).
fn rpc_error(code: i32, message: impl Into<String>) -> JsonRpcError {
    error_with(code, message.into(), a2a::error_reason(code), None)
}

/// An error whose `data` holds one `google.rpc.ErrorInfo` with `reason`.
fn error_with(code: i32, message: String, reason: &str, metadata: Option<HashMap<String, String>>) -> JsonRpcError {
    let info = TypedDetail::error_info(reason, a2a::PROTOCOL_DOMAIN, metadata);
    JsonRpcError {
        code,
        message,
        data: Some(json!([info])),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(status: RequestStatus, answer: Option<&str>) -> CallerRequest {
        CallerRequest {
            request_id: "req_1".into(),
            context_id: "ctx".into(),
            status,
            answer: answer.map(str::to_owned),
            status_at: 1_800_000_000,
        }
    }

    #[test]
    fn each_request_status_has_one_task_state_with_only_fixed_text() {
        use RequestStatus::*;
        let cases = [
            (Working, "TASK_STATE_WORKING", Some(WORKING_TEXT)),
            (PendingApproval, "TASK_STATE_WORKING", Some(WORKING_TEXT)),
            (Declined, "TASK_STATE_REJECTED", Some(REJECTED_TEXT)),
            (Failed, "TASK_STATE_FAILED", Some(FAILED_TEXT)),
            (Canceled, "TASK_STATE_CANCELED", None),
        ];
        for (status, state, text) in cases {
            let task = json!(task(&found(status, None)));
            assert_eq!(task["status"]["state"], state, "{status:?}");
            assert_eq!(
                task["status"]["message"]["parts"][0]["text"].as_str(),
                text,
                "{status:?}"
            );
            assert_eq!(task["status"]["timestamp"], "2027-01-15T08:00:00Z");
            assert!(task.get("artifacts").is_none() && task.get("history").is_none());
        }
        // Waiting for the agent and waiting for the owner look the same to the caller.
        assert_eq!(
            json!(task(&found(Working, None))),
            json!(task(&found(PendingApproval, None)))
        );
        let done = json!(task(&found(Answered, Some("Friday."))));
        assert_eq!(done["status"]["state"], "TASK_STATE_COMPLETED");
        assert_eq!(done["artifacts"][0]["parts"], json!([{"text": "Friday."}]));
    }

    #[test]
    fn only_major_minor_1_0_passes() {
        for version in ["1.0", "1.0.0", "1.0.17"] {
            assert!(check_version(version).is_ok(), "{version}");
        }
        for version in ["", "0.3", "1", "1.1", "2.0", "10.0", "1.00", "v1.0"] {
            assert_eq!(
                check_version(version).unwrap_err().code,
                VERSION_NOT_SUPPORTED,
                "{version}"
            );
        }
    }
}
