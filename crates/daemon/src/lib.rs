//! Builds the real LLM client and connectors of a profile from its config.
//!
//! Secrets are not in the config. The config names an environment variable
//! and the value is read from the process environment when the client is built.

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

/// The `llm` section of a profile config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    pub wire: Wire,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// The environment variable that holds the endpoint token.
    pub token_env: String,
}

fn default_timeout_secs() -> u64 {
    120
}

/// One entry of the `connectors` section of a profile config.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorSection {
    pub mcp: McpConfig,
    /// The environment variable that holds the bearer token, if the server needs one.
    pub token_env: Option<String>,
}

/// Looks up a secret by name. The real one reads the process environment.
pub type SecretLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct RealBackends {
    secrets: SecretLookup,
}

impl RealBackends {
    pub fn from_env() -> Self {
        Self::new(Arc::new(|name| std::env::var(name).ok()))
    }

    pub fn new(secrets: SecretLookup) -> Self {
        Self { secrets }
    }

    fn secret(&self, name: &str) -> Result<String, String> {
        match (self.secrets)(name) {
            Some(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("the environment variable `{name}` is not set")),
        }
    }

    pub fn llm(&self, section: &Value) -> Result<HttpLlmClient, String> {
        if section.is_null() {
            return Err("the profile has no `llm` settings".to_owned());
        }
        let section: LlmSection = serde_json::from_value(section.clone())
            .map_err(|e| format!("the `llm` settings are invalid: {e}"))?;
        let token = ApiToken::new(self.secret(&section.token_env)?);
        let config = LlmConfig {
            wire: section.wire,
            base_url: section.base_url,
            model: section.model,
            timeout: Duration::from_secs(section.timeout_secs),
        };
        HttpLlmClient::new(config, token).map_err(|e| format!("the LLM client cannot start: {e}"))
    }

    async fn connector(&self, name: &str, section: &Value) -> Result<McpConnector, String> {
        let section: ConnectorSection = serde_json::from_value(section.clone())
            .map_err(|e| format!("the settings of connector `{name}` are invalid: {e}"))?;
        let token = section
            .token_env
            .as_deref()
            .map(|env| self.secret(env))
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
        _profile: &'a str,
        config: &'a ProfileConfig,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move {
            let llm: Arc<dyn LlmClient> = Arc::new(self.llm(&config.llm)?);
            let mut connectors: BTreeMap<String, Arc<dyn Connector>> = BTreeMap::new();
            for (name, section) in &config.connectors {
                connectors.insert(name.clone(), Arc::new(self.connector(name, section).await?));
            }
            Ok(Runtime { llm, connectors })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn backends(secrets: &'static [(&'static str, &'static str)]) -> RealBackends {
        RealBackends::new(Arc::new(move |name| {
            secrets
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_owned())
        }))
    }

    fn llm_section() -> Value {
        json!({"wire": "anthropic-messages", "base_url": "https://llm.example", "model": "m1", "token_env": "LLM_TOKEN"})
    }

    #[test]
    fn the_llm_client_is_built_from_settings_and_the_named_secret() {
        let client = backends(&[("LLM_TOKEN", "s3cret-value")])
            .llm(&llm_section())
            .unwrap();
        let shown = format!("{client:?}");
        assert!(shown.contains("https://llm.example"), "{shown}");
        assert!(!shown.contains("s3cret-value"), "{shown}");
    }

    #[test]
    fn a_missing_or_empty_secret_names_the_variable() {
        for secrets in [&[][..], &[("LLM_TOKEN", "")][..]] {
            let error = backends(secrets).llm(&llm_section()).unwrap_err();
            assert_eq!(error, "the environment variable `LLM_TOKEN` is not set");
        }
    }

    #[test]
    fn missing_and_invalid_llm_settings_are_reported() {
        let b = backends(&[("LLM_TOKEN", "x")]);
        assert_eq!(
            b.llm(&Value::Null).unwrap_err(),
            "the profile has no `llm` settings"
        );
        let unknown_wire =
            json!({"wire": "telepathy", "base_url": "u", "model": "m", "token_env": "LLM_TOKEN"});
        assert!(
            b.llm(&unknown_wire)
                .unwrap_err()
                .starts_with("the `llm` settings are invalid")
        );
        // A token written into the config is refused, so it is never stored.
        let mut inline = llm_section();
        inline["token"] = json!("s3cret-value");
        let error = b.llm(&inline).unwrap_err();
        assert!(
            error.starts_with("the `llm` settings are invalid"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_connector_error_names_the_connector_and_hides_the_token() {
        let b = backends(&[("SLACK_TOKEN", "xoxb-s3cret")]);
        let relative =
            json!({"mcp": {"transport": "stdio", "command": "slack-mcp", "args": [], "env": {}}});
        let error = b.connector("slack", &relative).await.err().unwrap();
        assert!(
            error.starts_with("connector `slack` cannot connect"),
            "{error}"
        );

        let dead = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_env": "SLACK_TOKEN"});
        let error = b.connector("slack", &dead).await.err().unwrap();
        assert!(error.contains("connector `slack`"), "{error}");
        assert!(!error.contains("xoxb-s3cret"), "{error}");

        let no_secret = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_env": "MISSING"});
        let error = b.connector("slack", &no_secret).await.err().unwrap();
        assert_eq!(error, "the environment variable `MISSING` is not set");
    }
}
