//! `POST /hooks/{p}/{name}` on the listeners for callers: deliveries of Slack and Jira Cloud.
//!
//! The handler checks the signature over the raw body first and reads the body after that. It stores
//! the event in the intake and answers at once: Slack wants an answer within 3 seconds. The intake task
//! hands the event to the agent later. Every refusal of the profile, the webhook, the listener and the
//! signature is the same 401.

use crate::server::OuterConfig;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use bunsin_connectors::webhook::{SlackDelivery, parse_jira, parse_slack, verify_atlassian, verify_slack};
use bunsin_core::event::InboundEvent;
use bunsin_core::settings::ApiService;
use bunsin_core::webhook::{JIRA_MAX_AGE_SECS, MAX_HOOK_BODY_BYTES};
use bunsin_engine::{Engine, WebhookTarget};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Notify;

struct Hooks {
    engine: Arc<Engine>,
    listener: OuterConfig,
    /// Wakes the task that hands stored events to the agent.
    wake: Arc<Notify>,
}

/// The webhook route of one listener for callers.
pub fn routes(engine: Arc<Engine>, listener: &OuterConfig, wake: Arc<Notify>) -> Router {
    let state = Arc::new(Hooks {
        engine,
        listener: listener.clone(),
        wake,
    });
    Router::new()
        .route("/hooks/{profile}/{name}", post(deliver))
        // A larger body gets 413 before the handler runs.
        .layer(DefaultBodyLimit::max(MAX_HOOK_BODY_BYTES))
        .with_state(state)
}

fn refused() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        "the delivery is not signed for a webhook here",
    )
        .into_response()
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

/// What a verified delivery holds.
enum Delivery {
    /// Slack's `url_verification`: answer with the challenge.
    Challenge(String),
    /// An event, with the ID of this delivery. A repeated delivery has the same ID.
    Event(String, InboundEvent),
    /// An event type that is not for the agent.
    Ignored,
}

/// Checks the signature and reads the body. `None`: refuse the delivery.
fn read(target: &WebhookTarget, headers: &HeaderMap, body: &[u8], now: i64) -> Option<Delivery> {
    match target.service {
        ApiService::Slack => {
            let timestamp = header_text(headers, "x-slack-request-timestamp")?;
            let signature = header_text(headers, "x-slack-signature")?;
            verify_slack(&target.secret, timestamp, signature, body, now).ok()?;
            match parse_slack(&target.connector, body).ok()? {
                SlackDelivery::Challenge(challenge) => Some(Delivery::Challenge(challenge)),
                SlackDelivery::Ignored => Some(Delivery::Ignored),
                SlackDelivery::Event(event) => {
                    // Slack repeats `event_id` in every retry of a delivery.
                    let json: Value = serde_json::from_slice(body).ok()?;
                    let id = json["event_id"].as_str().unwrap_or(&event.external_id).to_owned();
                    Some(Delivery::Event(id, event))
                }
            }
        }
        ApiService::Jira => {
            verify_atlassian(&target.secret, header_text(headers, "x-hub-signature")?, body).ok()?;
            // The signed body has the time of the event in milliseconds. A retry repeats the body,
            // so an old body is a replay or a retry after hours.
            let json: Value = serde_json::from_slice(body).ok()?;
            let sent_ms = json["timestamp"].as_i64()?;
            if now - sent_ms / 1_000 > JIRA_MAX_AGE_SECS {
                return None;
            }
            match parse_jira(&target.connector, body).ok()? {
                None => Some(Delivery::Ignored),
                Some(event) => {
                    let id = header_text(headers, "x-atlassian-webhook-identifier")
                        .unwrap_or(&event.external_id)
                        .to_owned();
                    Some(Delivery::Event(id, event))
                }
            }
        }
        // The settings check allows no webhook for another service.
        ApiService::Confluence => None,
    }
}

async fn deliver(
    State(state): State<Arc<Hooks>>,
    Path((profile, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let engine = &state.engine;
    let Some(target) = engine.webhook_target(&profile, &name, state.listener.reach.scope()) else {
        return refused();
    };
    let Some(delivery) = read(&target, &headers, &body, engine.now_unix()) else {
        return refused();
    };
    match delivery {
        Delivery::Challenge(challenge) => ([(header::CONTENT_TYPE, "text/plain")], challenge).into_response(),
        Delivery::Ignored => StatusCode::OK.into_response(),
        Delivery::Event(id, event) => match engine.take_webhook_event(&profile, &name, &id, event) {
            Ok(stored) => {
                if stored {
                    state.wake.notify_one();
                }
                // A repeated delivery gets 200 too, so the sender stops repeating it.
                StatusCode::OK.into_response()
            }
            // The sender tries again later.
            Err(_) => StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
    }
}
