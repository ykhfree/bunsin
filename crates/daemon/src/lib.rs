//! Builds the real LLM client and connectors of a profile from its settings.
//!
//! Secrets are not in the settings. The settings name a secret key, and the value is
//! read from the environment variable `BUNSIN_SECRET_<PROFILE>__<KEY>` of the server.
//! A profile can therefore name only its own secrets.
//!
//! A profile owner chooses the LLM and MCP addresses, and the server sends the requests.
//! [`UpstreamPolicy`] stops an owner from pointing the server at its own internal network.

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
    let group = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
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

/// Which LLM and MCP addresses profile settings may name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpstreamPolicy {
    /// Host names the operator allows, in lower case. If the list is not empty, only
    /// these hosts are allowed, with `http://` or `https://`.
    ///
    /// If the list is empty, an address must use `https://` and its host must not be
    /// `localhost` or an IP address that is loopback, private, link-local or otherwise
    /// not public. That default does not resolve names: a public name that resolves to
    /// an internal address passes. An operator who must rule that out gives a list.
    pub allowed_hosts: Vec<String>,
}

fn is_internal_ip(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
                // 100.64.0.0/10, shared address space of carrier networks.
                || (ip.octets()[0] == 100 && ip.octets()[1] & 0xc0 == 64)
        }
        std::net::IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                // fc00::/7 unique local and fe80::/10 link-local.
                || ip.segments()[0] & 0xfe00 == 0xfc00
                || ip.segments()[0] & 0xffc0 == 0xfe80
                || ip.to_ipv4_mapped().is_some_and(|v4| is_internal_ip(v4.into()))
        }
    }
}

impl UpstreamPolicy {
    /// Checks one address from profile settings. `what` names the setting in the error.
    pub fn check(&self, what: &str, address: &str) -> Result<(), String> {
        let url = url::Url::parse(address).map_err(|e| format!("{what} is not a valid address: {e}"))?;
        let host = url.host().ok_or_else(|| format!("{what} has no host"))?;
        let name = match &host {
            url::Host::Domain(name) => name.to_ascii_lowercase(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Ipv6(ip) => ip.to_string(),
        };
        if !self.allowed_hosts.is_empty() {
            return match (url.scheme(), self.allowed_hosts.contains(&name)) {
                ("http" | "https", true) => Ok(()),
                (_, true) => Err(format!("{what} must start with http:// or https://")),
                (_, false) => Err(format!(
                    "{what} names the host `{name}`, which the server operator has not allowed"
                )),
            };
        }
        if url.scheme() != "https" {
            return Err(format!("{what} must start with https://"));
        }
        let internal = match host {
            url::Host::Domain(_) => name == "localhost" || name.ends_with(".localhost"),
            url::Host::Ipv4(ip) => is_internal_ip(ip.into()),
            url::Host::Ipv6(ip) => is_internal_ip(ip.into()),
        };
        if internal {
            return Err(format!(
                "{what} names `{name}`, an address inside the server's own network. \
                 The server operator can allow it with --allow-upstream-host"
            ));
        }
        Ok(())
    }
}

/// Looks up an environment variable. The real one reads the process environment.
pub type SecretLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

pub struct RealBackends {
    secrets: SecretLookup,
    /// A stdio connector runs a command on this machine. Off unless the operator allows it.
    allow_stdio: bool,
    upstream: UpstreamPolicy,
}

impl RealBackends {
    pub fn from_env(allow_stdio: bool, upstream: UpstreamPolicy) -> Self {
        Self::new(Arc::new(|name| std::env::var(name).ok()), allow_stdio, upstream)
    }

    pub fn new(secrets: SecretLookup, allow_stdio: bool, upstream: UpstreamPolicy) -> Self {
        Self {
            secrets,
            allow_stdio,
            upstream,
        }
    }

    fn secret(&self, profile: &str, key: &str) -> Result<String, String> {
        let name = secret_env_name(profile, key)?;
        match (self.secrets)(&name) {
            Some(value) if !value.is_empty() => Ok(value),
            _ => Err(format!("the environment variable `{name}` is not set")),
        }
    }

    /// Parses and checks the `llm` settings. Every caller gets the address check,
    /// so settings stored before a rule existed are checked again when they are used.
    fn llm_section(&self, section: &Value) -> Result<LlmSection, String> {
        if section.is_null() {
            return Err("the profile has no `llm` settings".to_owned());
        }
        let section: LlmSection =
            serde_json::from_value(section.clone()).map_err(|e| format!("the `llm` settings are invalid: {e}"))?;
        self.upstream.check("the LLM `base_url`", &section.base_url)?;
        Ok(section)
    }

    fn connector_section(&self, name: &str, section: &Value) -> Result<ConnectorSection, String> {
        let section: ConnectorSection = serde_json::from_value(section.clone())
            .map_err(|e| format!("the settings of connector `{name}` are invalid: {e}"))?;
        match &section.mcp {
            McpConfig::Stdio { .. } if !self.allow_stdio => {
                return Err(format!(
                    "connector `{name}` uses the stdio transport, which runs a command on the server. \
                     The server operator must start the server with --allow-stdio-connectors"
                ));
            }
            McpConfig::Stdio { .. } => {}
            McpConfig::Http { url } => self
                .upstream
                .check(&format!("the address of connector `{name}`"), url)?,
        }
        Ok(section)
    }

    pub fn llm(&self, profile: &str, section: &Value) -> Result<HttpLlmClient, String> {
        let section = self.llm_section(section)?;
        let token = ApiToken::new(self.secret(profile, &section.token_secret)?);
        let config = LlmConfig {
            wire: section.wire,
            base_url: section.base_url,
            model: section.model,
            timeout: Duration::from_secs(section.timeout_secs),
        };
        HttpLlmClient::new(config, token).map_err(|e| format!("the LLM client cannot start: {e}"))
    }

    async fn connector(&self, profile: &str, name: &str, section: &Value) -> Result<McpConnector, String> {
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
    fn build<'a>(&'a self, profile: &'a str, config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move {
            let llm: Arc<dyn LlmClient> = Arc::new(self.llm(profile, &config.llm)?);
            let mut connectors: BTreeMap<String, Arc<dyn Connector>> = BTreeMap::new();
            for (name, section) in &config.connectors {
                connectors.insert(name.clone(), Arc::new(self.connector(profile, name, section).await?));
            }
            Ok(Runtime { llm, connectors })
        })
    }

    /// Refuses settings that this server will not run. Empty `llm` settings are allowed,
    /// because a profile is set up step by step.
    fn validate(&self, profile: &str, config: &ProfileConfig) -> Result<(), String> {
        if !config.llm.is_null() {
            secret_env_name(profile, &self.llm_section(&config.llm)?.token_secret)?;
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
        RealBackends::new(Arc::new(lookup), allow_stdio, UpstreamPolicy::default())
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
        assert_eq!(secret_env_name("kim", "LLM").unwrap(), "BUNSIN_SECRET_KIM__LLM");
        assert_eq!(
            secret_env_name("team-a", "SLACK_BOT").unwrap(),
            "BUNSIN_SECRET_TEAM_A__SLACK_BOT"
        );
        // Profile `a` cannot form the name of a secret of profile `a-b` or of profile `a--b`.
        assert_eq!(secret_env_name("a-b", "LLM").unwrap(), "BUNSIN_SECRET_A_B__LLM");
        assert_eq!(secret_env_name("a", "B_LLM").unwrap(), "BUNSIN_SECRET_A__B_LLM");
        assert_eq!(secret_env_name("a--b", "LLM").unwrap(), "BUNSIN_SECRET_A__B__LLM");
        assert!(secret_env_name("a", "B__LLM").is_err());
        for key in ["", "llm", "LLM-1", "_LLM", "LLM_", "A__B", "PATH/../X", "A B"] {
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
        assert_eq!(error, "the environment variable `BUNSIN_SECRET_LEE__LLM` is not set");
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
            assert_eq!(error, "the environment variable `BUNSIN_SECRET_KIM__LLM` is not set");
        }
    }

    #[test]
    fn missing_and_invalid_llm_settings_are_reported() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "x")], false);
        assert_eq!(
            b.llm("kim", &Value::Null).unwrap_err(),
            "the profile has no `llm` settings"
        );
        let unknown_wire = json!({"wire": "telepathy", "base_url": "u", "model": "m", "token_secret": "LLM"});
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
        let http =
            |key: &str| json!({"mcp": {"transport": "http", "url": "https://mcp.example/slack"}, "token_secret": key});
        assert_eq!(b.validate("kim", &config(Value::Null, &[])), Ok(()));
        assert_eq!(
            b.validate("kim", &config(llm_section(), &[("slack", http("SLACK"))])),
            Ok(())
        );
        // A secret key that tries to leave the profile's own names is refused at once.
        let error = b
            .validate("kim", &config(llm_section(), &[("slack", http("LEE__SLACK"))]))
            .unwrap_err();
        assert!(error.contains("secret key `LEE__SLACK` is invalid"), "{error}");
        let mut llm = llm_section();
        llm["token_secret"] = json!("../LLM");
        assert!(b.validate("kim", &config(llm, &[])).is_err());
        // The earlier field, which named any environment variable, is refused.
        let old_field = json!({"mcp": {"transport": "http", "url": "https://x"}, "token_env": "HOME"});
        assert!(
            b.validate("kim", &config(Value::Null, &[("slack", old_field)]))
                .is_err()
        );
    }

    #[tokio::test]
    async fn a_connector_error_names_the_connector_and_hides_the_token() {
        let b = backends(&[("BUNSIN_SECRET_KIM__SLACK", "xoxb-s3cret")], true);
        let relative = json!({"mcp": {"transport": "stdio", "command": "slack-mcp", "args": [], "env": {}}});
        let error = b.connector("kim", "slack", &relative).await.err().unwrap();
        assert!(error.starts_with("connector `slack` cannot connect"), "{error}");

        // The operator allows this host, so the request is tried and fails at connect.
        let b = RealBackends::new(
            Arc::new(|name| (name == "BUNSIN_SECRET_KIM__SLACK").then(|| "xoxb-s3cret".to_owned())),
            true,
            UpstreamPolicy {
                allowed_hosts: vec!["127.0.0.1".into()],
            },
        );
        let dead = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_secret": "SLACK"});
        let error = b.connector("kim", "slack", &dead).await.err().unwrap();
        assert!(error.contains("connector `slack`"), "{error}");
        assert!(!error.contains("xoxb-s3cret"), "{error}");

        let missing = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_secret": "JIRA"});
        let error = b.connector("kim", "slack", &missing).await.err().unwrap();
        assert_eq!(error, "the environment variable `BUNSIN_SECRET_KIM__JIRA` is not set");
    }

    #[test]
    fn by_default_only_public_https_addresses_are_allowed() {
        let policy = UpstreamPolicy::default();
        for address in [
            "https://api.example.com/v1",
            "https://8.8.8.8",
            "https://[2606:4700::1111]/x",
        ] {
            assert_eq!(policy.check("the address", address), Ok(()), "{address}");
        }
        for address in [
            "http://api.example.com",
            "https://localhost:8080",
            "https://db.localhost",
            "https://127.0.0.1",
            "https://127.1.2.3",
            "https://10.0.0.5",
            "https://172.16.0.1",
            "https://192.168.1.1",
            "https://169.254.169.254/latest/meta-data",
            "https://100.64.0.1",
            "https://0.0.0.0",
            "https://[::1]",
            "https://[fd00::1]",
            "https://[fe80::1]",
            "https://[::ffff:10.0.0.5]",
            "https://2130706433",
            "https://0x7f.0.0.1",
            "file:///etc/passwd",
            "not an address",
        ] {
            assert!(policy.check("the address", address).is_err(), "{address}");
        }
    }

    #[test]
    fn an_operator_list_allows_exactly_its_hosts() {
        let policy = UpstreamPolicy {
            allowed_hosts: vec!["llm.corp.internal".into(), "10.0.0.5".into()],
        };
        // The operator may allow an internal gateway, also without TLS.
        assert_eq!(policy.check("x", "http://llm.corp.internal:8080/v1"), Ok(()));
        assert_eq!(policy.check("x", "https://LLM.Corp.Internal/v1"), Ok(()));
        assert_eq!(policy.check("x", "http://10.0.0.5"), Ok(()));
        // With a list, a public host that is not on it is refused too.
        let refused = [
            "https://api.example.com",
            "http://10.0.0.6",
            "https://llm.corp.internal.example.com",
            "ftp://10.0.0.5",
        ];
        for address in refused {
            assert!(policy.check("x", address).is_err(), "{address}");
        }
    }

    #[tokio::test]
    async fn internal_addresses_in_settings_are_refused_when_stored_and_when_used() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "x")], false);
        let mut llm = llm_section();
        llm["base_url"] = json!("https://169.254.169.254");
        let mcp = json!({"mcp": {"transport": "http", "url": "http://10.0.0.5:9000/mcp"}});

        let error = b.validate("kim", &config(llm.clone(), &[])).unwrap_err();
        assert!(
            error.contains("the LLM `base_url`") && error.contains("169.254.169.254"),
            "{error}"
        );
        let error = b
            .validate("kim", &config(Value::Null, &[("jira", mcp.clone())]))
            .unwrap_err();
        assert!(error.contains("connector `jira`"), "{error}");

        // Settings that were stored earlier are refused at use, before any request is sent.
        assert!(b.llm("kim", &llm).is_err());
        let error = b.connector("kim", "jira", &mcp).await.err().unwrap();
        assert!(error.contains("must start with https://"), "{error}");
    }
}
