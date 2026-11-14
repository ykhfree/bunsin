//! Builds the real LLM client and connectors of a profile from its settings.
//!
//! Secrets are not in the settings. The settings name a secret key, and the value is
//! read from the environment variable `BUNSIN_SECRET_<PROFILE>__<KEY>` of the server.
//! A profile can therefore name only its own secrets.

pub mod server;

use bunsin_api::ProfileConfig;
use bunsin_connectors::{McpConfig, McpConnector};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_engine::{Backends, Runtime};
use bunsin_llm::{ApiToken, HttpLlmClient, LlmConfig, Wire};
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The `llm` section of the profile settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    pub wire: Wire,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// The key of the secret that holds the endpoint token. See [`secret_env_name`].
    pub token_secret: String,
}

fn default_timeout_secs() -> u64 {
    120
}

/// One entry of the `connectors` section of the profile settings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorSection {
    pub mcp: McpConfig,
    /// The key of the secret that holds the bearer token, if the MCP server needs one.
    pub token_secret: Option<String>,
}

/// The environment variable that holds secret `key` of `profile`:
/// `BUNSIN_SECRET_<PROFILE>__<KEY>`, with the profile ID in upper case and `-` as `_`.
///
/// A key is upper-case letters and digits in groups joined by one `_`. It has no `__`,
/// so the name of one profile's secret is never the name of another profile's secret.
pub fn secret_env_name(profile: &str, key: &str) -> Result<String, String> {
    let group = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
    };
    if !key.split('_').all(group) {
        return Err(format!(
            "the secret key `{key}` is invalid: use A-Z and 0-9 in groups joined by one `_`"
        ));
    }
    Ok(format!(
        "BUNSIN_SECRET_{}__{key}",
        profile.to_ascii_uppercase().replace('-', "_")
    ))
}

/// Looks up an environment variable. The real one reads the process environment.
pub type SecretLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct RealBackends {
    secrets: SecretLookup,
    /// A stdio connector runs a command on this machine. Off unless the operator allows it.
    allow_stdio: bool,
}

impl RealBackends {
    pub fn from_env(allow_stdio: bool) -> Self {
        Self::new(Arc::new(|name| std::env::var(name).ok()), allow_stdio)
    }

    pub fn new(secrets: SecretLookup, allow_stdio: bool) -> Self {
        Self {
            secrets,
            allow_stdio,
        }
    }

    fn secret(&self, profile: &str, key: &str) -> Result<String, String> {
        let name = secret_env_name(profile, key)?;
        match (self.secrets)(&name) {
            Some(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("the environment variable `{name}` is not set")),
        }
    }

    fn llm_section(section: &Value) -> Result<LlmSection, String> {
        if section.is_null() {
            return Err("the profile has no `llm` settings".to_owned());
        }
        serde_json::from_value(section.clone())
            .map_err(|e| format!("the `llm` settings are invalid: {e}"))
    }

    fn connector_section(&self, name: &str, section: &Value) -> Result<ConnectorSection, String> {
        let section: ConnectorSection = serde_json::from_value(section.clone())
            .map_err(|e| format!("the settings of connector `{name}` are invalid: {e}"))?;
        if matches!(section.mcp, McpConfig::Stdio { .. }) && !self.allow_stdio {
            return Err(format!(
                "connector `{name}` uses the stdio transport, which runs a command on the server. \
                 The server operator must start the server with --allow-stdio-connectors"
            ));
        }
        Ok(section)
    }

    pub fn llm(&self, profile: &str, section: &Value) -> Result<HttpLlmClient, String> {
        let section = Self::llm_section(section)?;
        let token = ApiToken::new(self.secret(profile, &section.token_secret)?);
        let config = LlmConfig {
            wire: section.wire,
            base_url: section.base_url,
            model: section.model,
            timeout: Duration::from_secs(section.timeout_secs),
        };
        HttpLlmClient::new(config, token).map_err(|e| format!("the LLM client cannot start: {e}"))
    }

    async fn connector(
        &self,
        profile: &str,
        name: &str,
        section: &Value,
    ) -> Result<McpConnector, String> {
        let section = self.connector_section(name, section)?;
        let token = section
            .token_secret
            .as_deref()
            .map(|key| self.secret(profile, key))
            .transpose()?;
        // A server that starts but never answers must not block the profile forever.
        let connect = McpConnector::connect(name, section.mcp, token.as_deref());
        match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
            Ok(Ok(connector)) => Ok(connector),
            Ok(Err(error)) => Err(format!("connector `{name}` cannot connect: {error}")),
            Err(_) => Err(format!(
                "connector `{name}` did not answer in {} s",
                CONNECT_TIMEOUT.as_secs()
            )),
        }
    }
}

impl Backends for RealBackends {
    fn build<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move {
            let llm: Arc<dyn LlmClient> = Arc::new(self.llm(profile, &config.llm)?);
            let mut connectors: BTreeMap<String, Arc<dyn Connector>> = BTreeMap::new();
            for (name, section) in &config.connectors {
                connectors.insert(
                    name.clone(),
                    Arc::new(self.connector(profile, name, section).await?),
                );
            }
            Ok(Runtime { llm, connectors })
        })
    }

    /// Refuses settings that this server will not run. Empty `llm` settings are allowed,
    /// because a profile is set up step by step.
    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        if !config.llm.is_null() {
            secret_env_name(profile, &Self::llm_section(&config.llm)?.token_secret)?;
        }
        for (name, section) in &config.connectors {
            if let Some(key) = self.connector_section(name, section)?.token_secret {
                secret_env_name(profile, &key)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn backends(env: &'static [(&'static str, &'static str)], allow_stdio: bool) -> RealBackends {
        let lookup = move |name: &str| {
            env.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        };
        RealBackends::new(Arc::new(lookup), allow_stdio)
    }

    fn llm_section() -> Value {
        json!({"wire": "anthropic-messages", "base_url": "https://llm.example", "model": "m1", "token_secret": "LLM"})
    }

    fn config(llm: Value, connectors: &[(&str, Value)]) -> ProfileConfig {
        let mut config = ProfileConfig::new("Kim");
        config.llm = llm;
        config.connectors = connectors
            .iter()
            .map(|(name, section)| ((*name).to_owned(), section.clone()))
            .collect();
        config
    }

    #[test]
    fn secret_names_are_per_profile_and_never_overlap() {
        assert_eq!(
            secret_env_name("kim", "LLM").unwrap(),
            "BUNSIN_SECRET_KIM__LLM"
        );
        assert_eq!(
            secret_env_name("team-a", "SLACK_BOT").unwrap(),
            "BUNSIN_SECRET_TEAM_A__SLACK_BOT"
        );
        // Profile `a` cannot form the name of a secret of profile `a-b` or of profile `a--b`.
        assert_eq!(
            secret_env_name("a-b", "LLM").unwrap(),
            "BUNSIN_SECRET_A_B__LLM"
        );
        assert_eq!(
            secret_env_name("a", "B_LLM").unwrap(),
            "BUNSIN_SECRET_A__B_LLM"
        );
        assert_eq!(
            secret_env_name("a--b", "LLM").unwrap(),
            "BUNSIN_SECRET_A__B__LLM"
        );
        assert!(secret_env_name("a", "B__LLM").is_err());
        for key in [
            "",
            "llm",
            "LLM-1",
            "_LLM",
            "LLM_",
            "A__B",
            "PATH/../X",
            "A B",
        ] {
            assert!(secret_env_name("kim", key).is_err(), "{key:?}");
        }
    }

    #[test]
    fn the_llm_client_is_built_from_settings_and_the_profiles_own_secret() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "s3cret-value")], false);
        let shown = format!("{:?}", b.llm("kim", &llm_section()).unwrap());
        assert!(shown.contains("https://llm.example"), "{shown}");
        assert!(!shown.contains("s3cret-value"), "{shown}");
    }

    #[test]
    fn one_profile_cannot_read_another_profiles_secret_or_a_server_variable() {
        let env = &[
            ("BUNSIN_SECRET_KIM__LLM", "kim-secret"),
            ("LLM", "server-secret"),
            ("HOME", "/root"),
        ];
        let b = backends(env, false);
        // The same key under another profile is another variable, which is not set.
        let error = b.llm("lee", &llm_section()).unwrap_err();
        assert_eq!(
            error,
            "the environment variable `BUNSIN_SECRET_LEE__LLM` is not set"
        );
        // No key reaches a variable outside the profile's own names.
        for key in ["HOME", "KIM__LLM", "BUNSIN_SECRET_KIM__LLM"] {
            let mut section = llm_section();
            section["token_secret"] = json!(key);
            assert!(b.llm("lee", &section).is_err(), "{key}");
        }
    }

    #[test]
    fn a_missing_or_empty_secret_names_the_variable() {
        for env in [&[][..], &[("BUNSIN_SECRET_KIM__LLM", "")][..]] {
            let error = backends(env, false).llm("kim", &llm_section()).unwrap_err();
            assert_eq!(
                error,
                "the environment variable `BUNSIN_SECRET_KIM__LLM` is not set"
            );
        }
    }

    #[test]
    fn missing_and_invalid_llm_settings_are_reported() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "x")], false);
        assert_eq!(
            b.llm("kim", &Value::Null).unwrap_err(),
            "the profile has no `llm` settings"
        );
        let unknown_wire =
            json!({"wire": "telepathy", "base_url": "u", "model": "m", "token_secret": "LLM"});
        assert!(
            b.llm("kim", &unknown_wire)
                .unwrap_err()
                .starts_with("the `llm` settings are invalid")
        );
        // A token written into the settings is refused, so it is never stored.
        let mut inline = llm_section();
        inline["token"] = json!("s3cret-value");
        assert!(
            b.llm("kim", &inline)
                .unwrap_err()
                .starts_with("the `llm` settings are invalid")
        );
    }

    #[test]
    fn a_stdio_connector_is_refused_unless_the_operator_allows_it() {
        let stdio = json!({"mcp": {"transport": "stdio", "command": "/usr/bin/true", "args": [], "env": {}}});
        let settings = config(Value::Null, &[("slack", stdio)]);
        let error = backends(&[], false).validate("kim", &settings).unwrap_err();
        assert!(error.contains("--allow-stdio-connectors"), "{error}");
        assert_eq!(backends(&[], true).validate("kim", &settings), Ok(()));
    }

    #[tokio::test]
    async fn a_stdio_connector_is_not_started_when_it_is_not_allowed() {
        // Settings stored before the check existed must not run a command either.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let stdio = json!({"mcp": {
            "transport": "stdio", "command": "/usr/bin/touch", "args": [marker.to_str().unwrap()], "env": {}
        }});
        let error = backends(&[], false)
            .connector("kim", "slack", &stdio)
            .await
            .err()
            .unwrap();
        assert!(error.contains("--allow-stdio-connectors"), "{error}");
        assert!(!marker.exists());
    }

    #[test]
    fn settings_are_checked_before_they_are_stored() {
        let b = backends(&[], false);
        let http = |key: &str| json!({"mcp": {"transport": "http", "url": "https://mcp.example/slack"}, "token_secret": key});
        assert_eq!(b.validate("kim", &config(Value::Null, &[])), Ok(()));
        assert_eq!(
            b.validate("kim", &config(llm_section(), &[("slack", http("SLACK"))])),
            Ok(())
        );
        // A secret key that tries to leave the profile's own names is refused at once.
        let error = b
            .validate(
                "kim",
                &config(llm_section(), &[("slack", http("LEE__SLACK"))]),
            )
            .unwrap_err();
        assert!(
            error.contains("secret key `LEE__SLACK` is invalid"),
            "{error}"
        );
        let mut llm = llm_section();
        llm["token_secret"] = json!("../LLM");
        assert!(b.validate("kim", &config(llm, &[])).is_err());
        // The earlier field, which named any environment variable, is refused.
        let old_field =
            json!({"mcp": {"transport": "http", "url": "https://x"}, "token_env": "HOME"});
        assert!(
            b.validate("kim", &config(Value::Null, &[("slack", old_field)]))
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_connector_error_names_the_connector_and_hides_the_token() {
        let b = backends(&[("BUNSIN_SECRET_KIM__SLACK", "xoxb-s3cret")], true);
        let relative =
            json!({"mcp": {"transport": "stdio", "command": "slack-mcp", "args": [], "env": {}}});
        let error = b.connector("kim", "slack", &relative).await.err().unwrap();
        assert!(
            error.starts_with("connector `slack` cannot connect"),
            "{error}"
        );

        let dead = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_secret": "SLACK"});
        let error = b.connector("kim", "slack", &dead).await.err().unwrap();
        assert!(error.contains("connector `slack`"), "{error}");
        assert!(!error.contains("xoxb-s3cret"), "{error}");

        let missing = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_secret": "JIRA"});
        let error = b.connector("kim", "slack", &missing).await.err().unwrap();
        assert_eq!(
            error,
            "the environment variable `BUNSIN_SECRET_KIM__JIRA` is not set"
        );
    }
}
