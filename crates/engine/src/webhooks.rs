//! Webhooks: the settings check, the stored signing secret, and the intake of a verified delivery.
//!
//! The daemon checks the signature and reads the body with `bunsin-connectors::webhook`. The engine
//! decides whether a webhook exists on a listener, gives the secret for the check, and stores the event.
//! The intake task hands it to the agent later, like a question of an inbound caller.

use crate::secrets::StoredSecret;
use crate::{Engine, internal, store_error};
use bunsin_api::{ApiError, ApiResult, ProfileConfig};
use bunsin_core::event::InboundEvent;
use bunsin_core::inbound::InboundScope;
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::settings::{ApiService, ConnectorKind, ConnectorSection, valid_connector_name};
use bunsin_core::webhook::{DELIVERY_KEEP_SECS, hook_secret_key, hook_secret_origin, own_post};
use serde_json::json;

/// What the daemon needs to check and read a delivery.
pub struct WebhookTarget {
    pub service: ApiService,
    /// The source of the events: the connector of the webhook.
    pub connector: String,
    /// The signing secret. It is used for the check only.
    pub secret: String,
}

impl Engine {
    /// The current time of the engine's clock, in Unix seconds. The signature checks use it.
    pub fn now_unix(&self) -> i64 {
        self.now()
    }

    /// Refuses a webhook whose name is invalid, whose connector is not a REST API connector of Slack
    /// or Jira, or whose connector also has a poll: the two give one message two event IDs.
    pub(crate) fn check_webhooks(config: &ProfileConfig) -> ApiResult<()> {
        for (name, hook) in &config.webhooks {
            if !valid_connector_name(name) {
                return Err(ApiError::Invalid(format!(
                    "the webhook name `{name}` is invalid: use 1 to 32 of a-z, 0-9 and `-`"
                )));
            }
            let service = config
                .connectors
                .get(&hook.connector)
                .and_then(|value| serde_json::from_value::<ConnectorSection>(value.clone()).ok())
                .and_then(|section| match section.kind {
                    ConnectorKind::Api(api) => Some(api.service),
                    ConnectorKind::Mcp(_) => None,
                });
            if !matches!(service, Some(ApiService::Slack | ApiService::Jira)) {
                return Err(ApiError::Invalid(format!(
                    "webhook `{name}` needs a REST API connector of Slack or Jira; `{}` is none",
                    hook.connector
                )));
            }
            if config.polls.iter().any(|poll| poll.connector == hook.connector) {
                return Err(ApiError::Invalid(format!(
                    "connector `{}` has a poll and webhook `{name}`; use one of them, or one message is handled twice",
                    hook.connector
                )));
            }
        }
        Ok(())
    }

    /// The webhook `name` of `profile` if the listener of `listener` takes its deliveries and its
    /// secret is stored. `None` for every other case, so the caller learns nothing about profiles.
    pub fn webhook_target(&self, profile: &str, name: &str, listener: InboundScope) -> Option<WebhookTarget> {
        let store = self.store(profile).ok()?;
        let config = Self::config(&store).ok()?;
        let hook = config.webhooks.get(name).filter(|hook| hook.served_on(listener))?;
        let section: ConnectorSection = serde_json::from_value(config.connectors.get(&hook.connector)?.clone()).ok()?;
        let ConnectorKind::Api(api) = section.kind else {
            return None;
        };
        let key = hook_secret_key(name);
        let sealed = store.secret(&key).ok()??;
        let secret = self.secret_box.open(profile, &key, &sealed).ok()?;
        // A value stored for something else is not a signing secret of this webhook.
        (secret.origin == hook_secret_origin(name)).then(|| WebhookTarget {
            service: api.service,
            connector: hook.connector.clone(),
            secret: secret.value,
        })
    }

    /// Stores a verified event of webhook `name` for the agent. Returns false if the event is the
    /// agent's own post or an author to skip, or if the delivery was stored before.
    pub fn take_webhook_event(
        &self,
        profile: &str,
        name: &str,
        delivery_id: &str,
        event: InboundEvent,
    ) -> ApiResult<bool> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let hook = config
            .webhooks
            .get(name)
            .ok_or_else(|| ApiError::NotFound(format!("webhook `{name}`")))?;
        if own_post(&event, &hook.skip_authors, &config.ai_marker) {
            return Ok(false);
        }
        let now = self.now();
        store
            .prune_intake(name, now - DELIVERY_KEEP_SECS)
            .map_err(store_error)?;
        // The filter runs before the event is stored, so removed text is in no table.
        let mut counts = RedactionCounts::new();
        let event = Self::redact_event(&Self::redactor(&config)?, event, &mut counts);
        let stored = store
            .add_intake(name, delivery_id, None, &event, now)
            .map_err(store_error)?;
        if stored {
            let detail = json!({"webhook": name, "redacted": counts});
            self.audit(&store, "webhook_received", Some(&event.dedupe_key()), detail)?;
        }
        Ok(stored)
    }

    /// Stores the signing secret of webhook `name`, sealed and bound to that webhook.
    pub(crate) fn save_webhook_secret(&self, profile: &str, name: &str, secret: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        if !config.webhooks.contains_key(name) {
            return Err(ApiError::NotFound(format!("webhook `{name}` in the settings")));
        }
        if secret.trim().is_empty() {
            return Err(ApiError::Invalid("the signing secret is empty".into()));
        }
        let key = hook_secret_key(name);
        let value = StoredSecret {
            value: secret.trim().to_owned(),
            origin: hook_secret_origin(name),
        };
        let sealed = self.secret_box.seal(profile, &key, &value).map_err(internal)?;
        store.put_secret(&key, &sealed, self.now()).map_err(store_error)?;
        self.audit(&store, "webhook_secret_set", None, json!({"webhook": name}))
    }
}
