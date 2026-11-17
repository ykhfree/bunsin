//! The LLM endpoint and the connectors of a profile, and their stored tokens.
//!
//! An address and its token are saved in one step. A token is bound to the origin
//! (scheme, host, port) it was entered for, so that a later change of the address
//! cannot send it to another host.

use crate::secrets::StoredSecret;
use crate::{Engine, internal, store_error};
use bunsin_api::{
    ApiConnectorUpdate, ApiError, ApiResult, CommandConnectorUpdate, Connections, ConnectorConnection, ConnectorUpdate,
    Expected, LlmConnection, LlmUpdate, ProfileConfig, SecretChange, TokenChange, TokenSource,
};
use bunsin_core::settings::{
    ConnectorKind, ConnectorSection, ENV_SECRET_PREFIX, LLM_SECRET_KEY, LlmSection, McpConfig, check_token_header,
    connector_secret_key, env_secret_key, env_secret_origin, origin_of, valid_connector_name,
};
use bunsin_core::webhook::{HOOK_SECRET_PREFIX, hook_secret_key};
use bunsin_store::ProfileStore;
use serde_json::{Value, json};

fn llm_section(config: &ProfileConfig) -> ApiResult<Option<LlmSection>> {
    if config.llm.is_null() {
        return Ok(None);
    }
    let section = serde_json::from_value(config.llm.clone());
    section
        .map(Some)
        .map_err(|e| ApiError::Invalid(format!("the stored `llm` settings are invalid: {e}")))
}

fn connector_section(name: &str, value: &Value) -> ApiResult<ConnectorSection> {
    serde_json::from_value(value.clone())
        .map_err(|e| ApiError::Invalid(format!("the stored settings of connector `{name}` are invalid: {e}")))
}

impl Engine {
    fn token_source(&self, profile: &str, store: &ProfileStore, key: Option<&str>) -> ApiResult<TokenSource> {
        let Some(key) = key else { return Ok(TokenSource::None) };
        if store.secret(key).map_err(store_error)?.is_some() {
            Ok(TokenSource::Stored)
        } else if self.backends.has_server_secret(profile, key) {
            Ok(TokenSource::ServerEnvironment)
        } else {
            Ok(TokenSource::None)
        }
    }

    pub(crate) fn read_connections(&self, profile: &str) -> ApiResult<Connections> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let llm = match llm_section(&config)? {
            Some(section) => Some(LlmConnection {
                token: self.token_source(profile, &store, Some(&section.token_secret))?,
                wire: section.wire,
                base_url: section.base_url,
                model: section.model,
                timeout_secs: section.timeout_secs,
            }),
            None => None,
        };
        let mut connectors = std::collections::BTreeMap::new();
        for (name, value) in &config.connectors {
            let section = connector_section(name, value)?;
            let token = self.token_source(profile, &store, section.token_secret.as_deref())?;
            let mut secret_env_missing = Vec::new();
            for variable in &section.secret_env {
                if store
                    .secret(&env_secret_key(name, variable))
                    .map_err(store_error)?
                    .is_none()
                {
                    secret_env_missing.push(variable.clone());
                }
            }
            connectors.insert(
                name.clone(),
                ConnectorConnection {
                    kind: section.kind,
                    token,
                    token_header: section.token_header,
                    secret_env: section.secret_env.into_iter().collect(),
                    secret_env_missing,
                },
            );
        }
        Ok(Connections { llm, connectors })
    }

    /// Applies a token change for `key` at `address`. Returns whether a token exists afterwards.
    ///
    /// The checks run before anything is written. The caller writes the settings after this.
    fn change_token(
        &self,
        profile: &str,
        store: &ProfileStore,
        key: &str,
        address: &str,
        change: &TokenChange,
        had_token: bool,
    ) -> ApiResult<bool> {
        let origin = origin_of(address).ok_or_else(|| ApiError::Invalid(format!("`{address}` is not an address")))?;
        match change {
            TokenChange::Keep => {
                // A kept token must not follow the address to another host.
                if let Some(sealed) = store.secret(key).map_err(store_error)?
                    && sealed.origin != origin
                {
                    return Err(ApiError::Invalid(
                        "the address changed to another host or port; enter the token again".into(),
                    ));
                }
                Ok(had_token)
            }
            TokenChange::Set(value) => {
                if value.trim().is_empty() {
                    return Err(ApiError::Invalid("the token is empty".into()));
                }
                let secret = StoredSecret {
                    value: value.trim().to_owned(),
                    origin,
                };
                let sealed = self.secret_box.seal(profile, key, &secret).map_err(internal)?;
                store.put_secret(key, &sealed, self.now()).map_err(store_error)?;
                Ok(true)
            }
            TokenChange::Clear => {
                store.delete_secret(key).map_err(store_error)?;
                Ok(false)
            }
        }
    }

    /// Checks and stores changed settings, and makes the next use build new clients.
    fn commit_connections(
        &self,
        profile: &str,
        store: &ProfileStore,
        loaded: &str,
        config: &ProfileConfig,
    ) -> ApiResult<()> {
        Self::save_config_from(store, loaded, config)?;
        // The cached clients hold the old address and the old token.
        self.runtimes.lock().expect("engine lock").remove(profile);
        Ok(())
    }

    fn token_note(change: &TokenChange) -> &'static str {
        match change {
            TokenChange::Keep => "kept",
            TokenChange::Set(_) => "set",
            TokenChange::Clear => "cleared",
        }
    }

    pub(crate) fn save_llm(&self, profile: &str, update: LlmUpdate) -> ApiResult<()> {
        let store = self.store(profile)?;
        if let Expected::Value(expected) = &update.expected
            && *expected != self.read_connections(profile)?.llm
        {
            return Err(ApiError::Conflict(
                "the LLM endpoint is not the one this form loaded".into(),
            ));
        }
        let (loaded, mut config) = Self::config_with_text(&store)?;
        let section = LlmSection {
            wire: update.wire,
            base_url: update.base_url.trim().to_owned(),
            model: update.model.trim().to_owned(),
            timeout_secs: update.timeout_secs,
            token_secret: LLM_SECRET_KEY.to_owned(),
            // The workspace ID is set in the settings file. A save from the form keeps it.
            workspace_id: llm_section(&config)?.and_then(|earlier| earlier.workspace_id),
        };
        if section.model.is_empty() {
            return Err(ApiError::Invalid("the model name is empty".into()));
        }
        config.llm = serde_json::to_value(&section).map_err(internal)?;
        // The address rules of the server apply before the token is touched.
        self.backends.validate(profile, &config).map_err(ApiError::Invalid)?;
        Self::check_config(&config)?;
        self.change_token(profile, &store, LLM_SECRET_KEY, &section.base_url, &update.token, true)?;
        self.commit_connections(profile, &store, &loaded, &config)?;
        let detail = json!({"target": "llm", "token": Self::token_note(&update.token)});
        self.audit(&store, "connection_changed", None, detail)
    }

    pub(crate) fn save_connector(&self, profile: &str, name: &str, update: ConnectorUpdate) -> ApiResult<()> {
        let token_header = update
            .token_header
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned);
        if let Some(name) = &token_header {
            check_token_header(name).map_err(ApiError::Invalid)?;
        }
        let kind = ConnectorKind::Mcp(McpConfig::Http {
            url: update.url.trim().to_owned(),
        });
        self.save_token_connector(profile, name, kind, token_header, &update.token, &update.expected)
    }

    pub(crate) fn save_api_connector(&self, profile: &str, name: &str, update: ApiConnectorUpdate) -> ApiResult<()> {
        let mut api = update.api;
        api.base_url = api.base_url.trim().to_owned();
        api.email = api
            .email
            .map(|email| email.trim().to_owned())
            .filter(|email| !email.is_empty());
        self.save_token_connector(
            profile,
            name,
            ConnectorKind::Api(api),
            None,
            &update.token,
            &update.expected,
        )
    }

    /// Saves a connector with an address and its token in one step. The token is bound to the
    /// origin of the address.
    fn save_token_connector(
        &self,
        profile: &str,
        name: &str,
        kind: ConnectorKind,
        token_header: Option<String>,
        token: &TokenChange,
        expected: &Expected<Option<ConnectorConnection>>,
    ) -> ApiResult<()> {
        let store = self.checked_connector_store(profile, name, expected)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        // An earlier entry may name its secret itself (set with `config-set`). A kept token keeps that name.
        let earlier = Self::same_kind(name, &config, &kind)?.and_then(|section| section.token_secret);
        let key = match (token, &earlier) {
            (TokenChange::Keep, Some(earlier)) => earlier.clone(),
            _ => connector_secret_key(name),
        };
        let will_have_token = match token {
            TokenChange::Keep => earlier.is_some(),
            TokenChange::Set(_) => true,
            TokenChange::Clear => false,
        };
        let address = kind
            .address()
            .expect("a connector with a token has an address")
            .to_owned();
        let section = ConnectorSection {
            kind,
            token_secret: will_have_token.then(|| key.clone()),
            token_header,
            secret_env: Default::default(),
        };
        config
            .connectors
            .insert(name.to_owned(), serde_json::to_value(&section).map_err(internal)?);
        self.backends.validate(profile, &config).map_err(ApiError::Invalid)?;
        Self::check_config(&config)?;
        self.change_token(profile, &store, &key, &address, token, earlier.is_some())?;
        self.commit_connections(profile, &store, &loaded, &config)?;
        let detail = json!({"target": name, "token": Self::token_note(token)});
        self.audit(&store, "connection_changed", None, detail)
    }

    /// Checks the connector name and the conflict check of a connector save, and opens the store.
    fn checked_connector_store(
        &self,
        profile: &str,
        name: &str,
        expected: &Expected<Option<ConnectorConnection>>,
    ) -> ApiResult<std::sync::Arc<ProfileStore>> {
        if !valid_connector_name(name) {
            return Err(ApiError::Invalid(format!(
                "the connector name `{name}` is invalid: use 1 to 32 of a-z, 0-9 and `-`"
            )));
        }
        let store = self.store(profile)?;
        if let Expected::Value(expected) = expected
            && *expected != self.read_connections(profile)?.connectors.get(name).cloned()
        {
            return Err(ApiError::Conflict(format!(
                "connector `{name}` is not the one this form loaded"
            )));
        }
        Ok(store)
    }

    /// The stored connector `name`, if there is one. A name keeps its kind: the owner removes the
    /// connector to make another kind of it, so a save never turns one kind into another by accident.
    fn same_kind(name: &str, config: &ProfileConfig, kind: &ConnectorKind) -> ApiResult<Option<ConnectorSection>> {
        let Some(value) = config.connectors.get(name) else {
            return Ok(None);
        };
        let earlier = connector_section(name, value)?;
        if earlier.kind.describe() != kind.describe() {
            return Err(ApiError::Invalid(format!(
                "connector `{name}` {}. Remove it first to give the name another kind",
                earlier.kind.describe()
            )));
        }
        Ok(Some(earlier))
    }

    /// Deletes every stored value of a secret environment variable, and every webhook secret, that
    /// `config` no longer names. Every path that stores settings calls this, so a removed variable,
    /// connector or webhook leaves no value behind, whichever way it was removed.
    pub(crate) fn prune_secrets(&self, store: &ProfileStore, config: &ProfileConfig) -> ApiResult<()> {
        let mut named = std::collections::BTreeSet::new();
        for (name, value) in &config.connectors {
            // Settings that cannot be read name no variable. Validation refused them before.
            if let Ok(section) = connector_section(name, value) {
                named.extend(section.secret_env.iter().map(|variable| env_secret_key(name, variable)));
            }
        }
        named.extend(config.webhooks.keys().map(|name| hook_secret_key(name)));
        let mut removed = 0;
        for key in store.secret_keys().map_err(store_error)? {
            let pruned = key.starts_with(ENV_SECRET_PREFIX) || key.starts_with(HOOK_SECRET_PREFIX);
            if pruned && !named.contains(&key) {
                store.delete_secret(&key).map_err(store_error)?;
                removed += 1;
            }
        }
        if removed > 0 {
            // A removed value cannot be brought back, so the log says that it happened.
            self.audit(store, "secret_values_removed", None, json!({"count": removed}))?;
        }
        Ok(())
    }

    /// Saves a connector that the server starts as a command. The backends refuse it unless the
    /// server operator allows such connectors. That is the rule for the settings file too.
    pub(crate) fn save_command_connector(
        &self,
        profile: &str,
        name: &str,
        update: CommandConnectorUpdate,
    ) -> ApiResult<()> {
        let store = self.checked_connector_store(profile, name, &update.expected)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        let kind = ConnectorKind::Mcp(McpConfig::Stdio {
            command: update.command,
            args: update.args,
            env: update.env,
        });
        let earlier = Self::same_kind(name, &config, &kind)?;
        // Every value is checked before anything is stored, so a refused save changes nothing.
        let stored = earlier.map(|section| section.secret_env).unwrap_or_default();
        for (variable, change) in &update.secret_env {
            match change {
                SecretChange::Set(value) if value.is_empty() => {
                    return Err(ApiError::Invalid(format!(
                        "the value of the secret variable `{variable}` is empty"
                    )));
                }
                SecretChange::Set(_) => {}
                SecretChange::Keep => {
                    let kept = stored.contains(variable)
                        && store
                            .secret(&env_secret_key(name, variable))
                            .map_err(store_error)?
                            .is_some();
                    if !kept {
                        return Err(ApiError::Invalid(format!(
                            "no value is stored for the secret variable `{variable}`. Enter its value"
                        )));
                    }
                }
            }
        }
        let section = ConnectorSection {
            kind,
            token_secret: None,
            token_header: None,
            secret_env: update.secret_env.keys().cloned().collect(),
        };
        config
            .connectors
            .insert(name.to_owned(), serde_json::to_value(&section).map_err(internal)?);
        self.backends.validate(profile, &config).map_err(ApiError::Invalid)?;
        Self::check_config(&config)?;
        for (variable, change) in &update.secret_env {
            if let SecretChange::Set(value) = change {
                let secret = StoredSecret {
                    value: value.clone(),
                    origin: env_secret_origin(name),
                };
                let key = env_secret_key(name, variable);
                let sealed = self.secret_box.seal(profile, &key, &secret).map_err(internal)?;
                store.put_secret(&key, &sealed, self.now()).map_err(store_error)?;
            }
        }
        self.commit_connections(profile, &store, &loaded, &config)?;
        // The values of removed variables go only after the settings are stored.
        self.prune_secrets(&store, &config)?;
        // The arguments and the variables can hold secrets. The log names only the connector.
        self.audit(
            &store,
            "connection_changed",
            None,
            json!({"target": name, "kind": "command"}),
        )
    }

    pub(crate) fn delete_connector(&self, profile: &str, name: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        if config.connectors.remove(name).is_none() {
            return Err(ApiError::NotFound(format!("connector `{name}`")));
        }
        // The token goes with the connector. The server stores a connector's token only under this key.
        store.delete_secret(&connector_secret_key(name)).map_err(store_error)?;
        self.commit_connections(profile, &store, &loaded, &config)?;
        self.prune_secrets(&store, &config)?;
        self.audit(&store, "connection_removed", None, json!({"target": name}))
    }
}
