//! Narrow reads and saves of single parts of the settings, for the window: the inbound endpoint and
//! its ARD listing, the ARD registries, the webhooks, the notes and the evaluation.
//!
//! Each save replaces one part. The whole settings then pass the same checks and server rules as a
//! save of the settings file, so the window cannot store what the settings file cannot.

use crate::Engine;
use bunsin_api::{
    ApiError, ApiResult, ArdRegistriesUpdate, EvalSettingsUpdate, Expected, InboundUpdate, NotesSettingsUpdate,
    ProfileConfig, WebhooksUpdate,
};
use bunsin_core::ard::ArdRegistry;
use bunsin_core::eval::EvalConfig;
use bunsin_core::inbound::InboundConfig;
use bunsin_core::webhook::WebhookConfig;
use bunsin_core::wiki::NotesConfig;
use bunsin_store::ProfileStore;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;

impl Engine {
    /// Replaces one part of the settings, if the stored part is still the one the caller loaded.
    /// `what` names the part in the conflict error. Returns the store and the saved settings.
    fn replace_part<T: PartialEq>(
        &self,
        profile: &str,
        what: &str,
        part: fn(&mut ProfileConfig) -> &mut T,
        value: T,
        expected: Expected<T>,
    ) -> ApiResult<(Arc<ProfileStore>, ProfileConfig)> {
        let store = self.store(profile)?;
        let (loaded, before) = Self::config_with_text(&store)?;
        let mut config = before.clone();
        if let Expected::Value(expected) = &expected
            && *expected != *part(&mut config)
        {
            return Err(ApiError::Conflict(format!(
                "the {what} are not the ones this form loaded"
            )));
        }
        *part(&mut config) = value;
        // An endpoint that is turned on here gets its reply rule, as in a save of the settings file.
        let config = Self::with_reply_rule(&before, config);
        self.backends.validate(profile, &config).map_err(ApiError::Invalid)?;
        Self::check_config(&config)?;
        Self::save_config_from(&store, &loaded, &config)?;
        // A removed webhook takes its signing secret with it.
        self.prune_secrets(&store, &config)?;
        Ok((store, config))
    }

    fn read_config(&self, profile: &str) -> ApiResult<ProfileConfig> {
        Self::config(&*self.store(profile)?)
    }

    pub(crate) fn read_inbound_settings(&self, profile: &str) -> ApiResult<InboundConfig> {
        Ok(self.read_config(profile)?.inbound)
    }

    pub(crate) fn save_inbound_settings(&self, profile: &str, update: InboundUpdate) -> ApiResult<()> {
        let (store, config) = self.replace_part(
            profile,
            "inbound settings",
            |c| &mut c.inbound,
            update.inbound,
            update.expected,
        )?;
        // No text: the title and the description are the owner's words, not facts of the change.
        let inbound = &config.inbound;
        let detail = json!({"enabled": inbound.enabled, "scope": inbound.scope, "ard": inbound.ard.is_some()});
        self.audit(&store, "inbound_settings_changed", None, detail)
    }

    pub(crate) fn read_ard_registries(&self, profile: &str) -> ApiResult<Vec<ArdRegistry>> {
        Ok(self.read_config(profile)?.ard_registries)
    }

    pub(crate) fn save_ard_registries(&self, profile: &str, update: ArdRegistriesUpdate) -> ApiResult<()> {
        let (store, config) = self.replace_part(
            profile,
            "ARD registries",
            |c| &mut c.ard_registries,
            update.registries,
            update.expected,
        )?;
        // The search tool is built from the registries, so the next use builds it again.
        self.runtimes.lock().expect("engine lock").remove(profile);
        let names: Vec<&str> = config.ard_registries.iter().map(|r| r.name.as_str()).collect();
        self.audit(&store, "ard_registries_changed", None, json!({"registries": names}))
    }

    pub(crate) fn read_webhooks(&self, profile: &str) -> ApiResult<BTreeMap<String, WebhookConfig>> {
        Ok(self.read_config(profile)?.webhooks)
    }

    pub(crate) fn save_webhooks(&self, profile: &str, update: WebhooksUpdate) -> ApiResult<()> {
        let (store, config) = self.replace_part(
            profile,
            "webhooks",
            |c| &mut c.webhooks,
            update.webhooks,
            update.expected,
        )?;
        let names: Vec<&String> = config.webhooks.keys().collect();
        self.audit(&store, "webhooks_changed", None, json!({"webhooks": names}))
    }

    pub(crate) fn read_notes_settings(&self, profile: &str) -> ApiResult<NotesConfig> {
        Ok(self.read_config(profile)?.notes)
    }

    pub(crate) fn save_notes_settings(&self, profile: &str, update: NotesSettingsUpdate) -> ApiResult<()> {
        let (store, config) = self.replace_part(
            profile,
            "notes settings",
            |c| &mut c.notes,
            update.notes,
            update.expected,
        )?;
        self.audit(&store, "notes_settings_changed", None, json!({"notes": config.notes}))
    }

    pub(crate) fn read_eval_settings(&self, profile: &str) -> ApiResult<EvalConfig> {
        Ok(self.read_config(profile)?.eval)
    }

    pub(crate) fn save_eval_settings(&self, profile: &str, update: EvalSettingsUpdate) -> ApiResult<()> {
        let (store, config) = self.replace_part(
            profile,
            "evaluation settings",
            |c| &mut c.eval,
            update.eval,
            update.expected,
        )?;
        self.audit(&store, "eval_settings_changed", None, json!({"eval": config.eval}))
    }
}
