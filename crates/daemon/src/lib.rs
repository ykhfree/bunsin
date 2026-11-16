//! Builds the real LLM client and connectors of a profile from its settings.
//!
//! Secrets are not in the settings. The settings name a secret key, and the value is
//! read from the environment variable `BUNSIN_SECRET_<PROFILE>__<KEY>` of the server.
//! A profile can therefore name only its own secrets.
//!
//! A profile owner chooses the LLM and MCP addresses, and the server sends the requests.
//! [`UpstreamPolicy`] stops an owner from pointing the server at its own internal network.

pub mod keystore;
pub mod server;

use bunsin_api::ProfileConfig;
use bunsin_connectors::{McpConnector, TokenAuth};
use bunsin_core::ports::{BoxFuture, Connector, LlmClient};
use bunsin_core::settings::{
    ConnectorSection, LlmSection, McpConfig, check_token_header, env_secret_key, env_secret_origin, origin_of,
    valid_env_name,
};
use bunsin_engine::{Backends, ProfileSecrets, Runtime};
use bunsin_llm::{ApiToken, HttpLlmClient, LlmConfig};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// The token for `key`, to be sent to `address`.
    ///
    /// A token the owner stored is used only for the origin it was entered for. Without a
    /// stored token, the server's environment variable for this profile and key is used.
    fn token(
        &self,
        profile: &str,
        key: &str,
        address: Option<&str>,
        secrets: &ProfileSecrets,
    ) -> Result<String, String> {
        match secrets.get(key) {
            Some(Ok(stored)) if address.and_then(origin_of).as_deref() == Some(stored.origin.as_str()) => {
                Ok(stored.value.clone())
            }
            Some(Ok(_)) => Err(format!(
                "the stored token `{key}` was entered for another address. Enter the token again for the new address"
            )),
            // A stored token that cannot be read is an error. The environment is not tried instead.
            Some(Err(error)) => Err(error.clone()),
            None => {
                let name = secret_env_name(profile, key)?;
                match (self.secrets)(&name) {
                    Some(value) if !value.is_empty() => Ok(value),
                    _ => Err(format!(
                        "no token is stored for `{key}` and the environment variable `{name}` is not set"
                    )),
                }
            }
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
        // Every way to store settings and every connect passes here. A token header such as `Host`
        // would change where an allowed address leads.
        if let Some(header) = &section.token_header {
            check_token_header(header).map_err(|e| format!("connector `{name}`: {e}"))?;
        }
        match &section.mcp {
            McpConfig::Stdio { .. } if !self.allow_stdio => {
                return Err(format!(
                    "connector `{name}` uses the stdio transport, which runs a command on the server. \
                     The server operator must start the server with --allow-stdio-connectors"
                ));
            }
            // Every way to store settings and every start of the command passes these checks.
            McpConfig::Stdio { command, env, .. } => {
                if !command.is_absolute() {
                    return Err(format!(
                        "connector `{name}`: the command must be an absolute path, got `{}`",
                        command.display()
                    ));
                }
                if let Some(twice) = section.secret_env.iter().find(|key| env.contains_key(*key)) {
                    return Err(format!(
                        "connector `{name}`: the variable `{twice}` is in both lists of environment variables"
                    ));
                }
                if let Some(bad) = env.keys().chain(&section.secret_env).find(|key| !valid_env_name(key)) {
                    return Err(format!(
                        "connector `{name}`: `{bad}` is not a name of an environment variable: \
                         use a-z, A-Z, 0-9 and `_`, and do not start with a digit"
                    ));
                }
            }
            McpConfig::Http { .. } if !section.secret_env.is_empty() => {
                return Err(format!(
                    "connector `{name}` has an address. Secret environment variables are for a command"
                ));
            }
            McpConfig::Http { url } => self
                .upstream
                .check(&format!("the address of connector `{name}`"), url)?,
        }
        Ok(section)
    }

    pub fn llm(&self, profile: &str, section: &Value, secrets: &ProfileSecrets) -> Result<HttpLlmClient, String> {
        let section = self.llm_section(section)?;
        let token = self.token(profile, &section.token_secret, Some(&section.base_url), secrets)?;
        let token = ApiToken::new(token);
        let config = LlmConfig {
            wire: section.wire,
            base_url: section.base_url,
            model: section.model,
            timeout: Duration::from_secs(section.timeout_secs),
            workspace_id: section.workspace_id,
        };
        HttpLlmClient::new(config, token).map_err(|e| format!("the LLM client cannot start: {e}"))
    }

    async fn connector(
        &self,
        profile: &str,
        name: &str,
        section: &Value,
        secrets: &ProfileSecrets,
    ) -> Result<McpConnector, String> {
        let section = self.connector_section(name, section)?;
        let address = match &section.mcp {
            McpConfig::Http { url } => Some(url.as_str()),
            McpConfig::Stdio { .. } => None,
        };
        let token = section
            .token_secret
            .as_deref()
            .map(|key| self.token(profile, key, address, secrets))
            .transpose()?;
        // A server that starts but never answers must not block the profile forever.
        let auth = token.as_deref().map(|token| TokenAuth {
            token,
            header: section.token_header.as_deref(),
        });
        let secret_env = stored_env(name, &section, secrets)?;
        let connect = McpConnector::connect_with_env(name, section.mcp, auth, &secret_env);
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
        Box::pin(async move { self.build_with_secrets(profile, config, &ProfileSecrets::new()).await })
    }

    /// Builds each client on its own. One that fails holds its reason and does not stop the others.
    fn build_with_secrets<'a>(
        &'a self,
        profile: &'a str,
        config: &'a ProfileConfig,
        secrets: &'a ProfileSecrets,
    ) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async move {
            let llm = self
                .llm(profile, &config.llm, secrets)
                .map(|client| Arc::new(client) as Arc<dyn LlmClient>);
            let mut connectors = BTreeMap::new();
            for (name, section) in &config.connectors {
                let connector = self.connector(profile, name, section, secrets).await;
                connectors.insert(name.clone(), connector.map(|c| Arc::new(c) as Arc<dyn Connector>));
            }
            Ok(Runtime { llm, connectors })
        })
    }

    fn has_server_secret(&self, profile: &str, key: &str) -> bool {
        secret_env_name(profile, key).is_ok_and(|name| (self.secrets)(&name).is_some_and(|value| !value.is_empty()))
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

/// The decrypted values of the secret environment variables of a command, by variable name.
/// They go into the environment of the program when it starts, and nowhere else.
fn stored_env(
    name: &str,
    section: &ConnectorSection,
    secrets: &ProfileSecrets,
) -> Result<BTreeMap<String, String>, String> {
    let mut env = BTreeMap::new();
    for variable in &section.secret_env {
        let value = match secrets.get(&env_secret_key(name, variable)) {
            Some(Ok(stored)) if stored.origin == env_secret_origin(name) => stored.value.clone(),
            Some(Ok(_)) => {
                return Err(format!(
                    "connector `{name}`: the stored value of `{variable}` was not entered for this connector"
                ));
            }
            Some(Err(error)) => return Err(error.clone()),
            None => {
                return Err(format!(
                    "connector `{name}`: no value is stored for the variable `{variable}`. Enter it in the window"
                ));
            }
        };
        env.insert(variable.clone(), value);
    }
    Ok(env)
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
        let shown = format!("{:?}", b.llm("kim", &llm_section(), &ProfileSecrets::new()).unwrap());
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
        let error = b.llm("lee", &llm_section(), &ProfileSecrets::new()).unwrap_err();
        assert!(
            error.contains("the environment variable `BUNSIN_SECRET_LEE__LLM` is not set"),
            "{error}"
        );
        // No key reaches a variable outside the profile's own names.
        for key in ["HOME", "KIM__LLM", "BUNSIN_SECRET_KIM__LLM"] {
            let mut section = llm_section();
            section["token_secret"] = json!(key);
            assert!(b.llm("lee", &section, &ProfileSecrets::new()).is_err(), "{key}");
        }
    }

    #[test]
    fn a_missing_or_empty_secret_names_the_variable() {
        for env in [&[][..], &[("BUNSIN_SECRET_KIM__LLM", "")][..]] {
            let error = backends(env, false)
                .llm("kim", &llm_section(), &ProfileSecrets::new())
                .unwrap_err();
            assert!(
                error.contains("the environment variable `BUNSIN_SECRET_KIM__LLM` is not set"),
                "{error}"
            );
        }
    }

    #[test]
    fn missing_and_invalid_llm_settings_are_reported() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "x")], false);
        assert_eq!(
            b.llm("kim", &Value::Null, &ProfileSecrets::new()).unwrap_err(),
            "the profile has no `llm` settings"
        );
        let unknown_wire = json!({"wire": "telepathy", "base_url": "u", "model": "m", "token_secret": "LLM"});
        assert!(
            b.llm("kim", &unknown_wire, &ProfileSecrets::new())
                .unwrap_err()
                .starts_with("the `llm` settings are invalid")
        );
        // A token written into the settings is refused, so it is never stored.
        let mut inline = llm_section();
        inline["token"] = json!("s3cret-value");
        assert!(
            b.llm("kim", &inline, &ProfileSecrets::new())
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
            .connector("kim", "slack", &stdio, &ProfileSecrets::new())
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
        let error = b
            .connector("kim", "slack", &relative, &ProfileSecrets::new())
            .await
            .err()
            .unwrap();
        // A relative command is refused before anything starts. The error names the connector.
        assert!(
            error.starts_with("connector `slack`: the command must be an absolute path"),
            "{error}"
        );

        // The operator allows this host, so the request is tried and fails at connect.
        let b = RealBackends::new(
            Arc::new(|name| (name == "BUNSIN_SECRET_KIM__SLACK").then(|| "xoxb-s3cret".to_owned())),
            true,
            UpstreamPolicy {
                allowed_hosts: vec!["127.0.0.1".into()],
            },
        );
        let dead = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_secret": "SLACK"});
        let error = b
            .connector("kim", "slack", &dead, &ProfileSecrets::new())
            .await
            .err()
            .unwrap();
        assert!(error.contains("connector `slack`"), "{error}");
        assert!(!error.contains("xoxb-s3cret"), "{error}");

        let missing = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}, "token_secret": "JIRA"});
        let error = b
            .connector("kim", "slack", &missing, &ProfileSecrets::new())
            .await
            .err()
            .unwrap();
        assert!(
            error.contains("the environment variable `BUNSIN_SECRET_KIM__JIRA` is not set"),
            "{error}"
        );
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
        assert!(b.llm("kim", &llm, &ProfileSecrets::new()).is_err());
        let error = b
            .connector("kim", "jira", &mcp, &ProfileSecrets::new())
            .await
            .err()
            .unwrap();
        assert!(error.contains("must start with https://"), "{error}");
    }

    fn stored(value: &str, origin: &str) -> ProfileSecrets {
        let secret = bunsin_engine::StoredSecret {
            value: value.into(),
            origin: origin.into(),
        };
        ProfileSecrets::from([("LLM".to_owned(), Ok(secret))])
    }

    #[test]
    fn a_stored_token_is_used_only_for_the_address_it_was_entered_for() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "from-the-environment")], false);
        let secrets = stored("stored-token", "https://llm.example:443");
        // Same origin, other path: the stored token is used.
        let mut same = llm_section();
        same["base_url"] = json!("https://llm.example/v2");
        assert!(b.llm("kim", &same, &secrets).is_ok());

        // Another host, port or scheme: refused, and the environment token is not used instead.
        for address in [
            "https://attacker.example",
            "https://llm.example:8443",
            "https://llm.example.attacker.example",
        ] {
            let mut moved = llm_section();
            moved["base_url"] = json!(address);
            let error = b.llm("kim", &moved, &secrets).unwrap_err();
            assert!(error.contains("entered for another address"), "{address}: {error}");
            assert!(
                !error.contains("stored-token") && !error.contains("from-the-environment"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_stored_token_that_cannot_be_read_is_an_error_and_not_a_fallback() {
        let b = backends(&[("BUNSIN_SECRET_KIM__LLM", "from-the-environment")], false);
        let unreadable = ProfileSecrets::from([(
            "LLM".to_owned(),
            Err("the stored token `LLM` cannot be read".to_owned()),
        )]);
        let error = b.llm("kim", &llm_section(), &unreadable).unwrap_err();
        assert!(error.contains("cannot be read"), "{error}");
    }

    #[test]
    fn the_server_reports_whether_it_supplies_a_secret() {
        let b = backends(
            &[("BUNSIN_SECRET_KIM__LLM", "x"), ("BUNSIN_SECRET_KIM__EMPTY", "")],
            false,
        );
        assert!(b.has_server_secret("kim", "LLM"));
        assert!(!b.has_server_secret("lee", "LLM"));
        assert!(!b.has_server_secret("kim", "EMPTY"));
        assert!(!b.has_server_secret("kim", "../HOME"));
    }

    #[tokio::test]
    async fn one_client_that_fails_does_not_stop_the_others() {
        let b = RealBackends::new(
            Arc::new(|name| (name == "BUNSIN_SECRET_KIM__LLM").then(|| "x".to_owned())),
            false,
            UpstreamPolicy {
                allowed_hosts: vec!["127.0.0.1".into(), "llm.example".into()],
            },
        );
        let dead = json!({"mcp": {"transport": "http", "url": "http://127.0.0.1:1/mcp"}});
        let runtime = b.build("kim", &config(llm_section(), &[("jira", dead)])).await.unwrap();
        assert!(runtime.llm.is_ok());
        let error = runtime.connector("jira").err().unwrap();
        assert!(error.contains("connector `jira` cannot connect"), "{error}");
        // A profile without LLM settings can still have working connectors checked.
        let runtime = b.build("kim", &config(Value::Null, &[])).await.unwrap();
        assert_eq!(
            runtime.llm.as_ref().err().map(String::as_str),
            Some("the profile has no `llm` settings")
        );
    }

    #[test]
    fn the_program_gets_each_secret_variable_under_its_name_and_nothing_else_does() {
        let section: ConnectorSection = serde_json::from_value(json!({
            "mcp": {"transport": "stdio", "command": "/opt/mcp/x", "args": [], "env": {"REGION": "eu"}},
            "secret_env": ["API_TOKEN", "other_key"]
        }))
        .unwrap();
        let entry = |connector: &str, variable: &str, value: &str| {
            let secret = bunsin_engine::StoredSecret {
                value: value.to_owned(),
                origin: env_secret_origin(connector),
            };
            (env_secret_key(connector, variable), Ok(secret))
        };
        let mut secrets: ProfileSecrets = [
            entry("local", "API_TOKEN", "value-one"),
            entry("local", "other_key", "value two\nwith a line"),
            // A variable of another connector is not given to this one.
            entry("other", "API_TOKEN", "not-for-local"),
        ]
        .into();
        let env = stored_env("local", &section, &secrets).unwrap();
        // The values are as they were entered. The plain variable is not in this map.
        let expected: BTreeMap<String, String> = [
            ("API_TOKEN".to_owned(), "value-one".to_owned()),
            ("other_key".to_owned(), "value two\nwith a line".to_owned()),
        ]
        .into();
        assert_eq!(env, expected);

        // A missing value stops the start, and the error names the variable and no value.
        secrets.remove(&env_secret_key("local", "other_key"));
        let error = stored_env("local", &section, &secrets).unwrap_err();
        assert!(
            error.contains("no value is stored for the variable `other_key`"),
            "{error}"
        );
        assert!(!error.contains("value-one"), "{error}");
        // A value that was stored for something else is refused.
        let token = bunsin_engine::StoredSecret {
            value: "a-token".to_owned(),
            origin: "https://mcp.example.com:443".to_owned(),
        };
        secrets.insert(env_secret_key("local", "other_key"), Ok(token));
        let error = stored_env("local", &section, &secrets).unwrap_err();
        assert!(error.contains("was not entered for this connector"), "{error}");
        assert!(!error.contains("a-token"), "{error}");
        // A value that cannot be decrypted is an error too.
        secrets.insert(env_secret_key("local", "other_key"), Err("cannot be opened".to_owned()));
        assert_eq!(stored_env("local", &section, &secrets).unwrap_err(), "cannot be opened");
    }

    #[test]
    fn secret_variables_are_checked_in_the_settings() {
        let b = backends(&[], true);
        let section = |mcp: Value, secret_env: Value| json!({"mcp": mcp, "secret_env": secret_env});
        let command = json!({"transport": "stdio", "command": "/opt/mcp/x", "args": [], "env": {"REGION": "eu"}});
        let ok = config(
            Value::Null,
            &[("local", section(command.clone(), json!(["API_TOKEN"])))],
        );
        assert_eq!(b.validate("kim", &ok), Ok(()));
        let twice = config(Value::Null, &[("local", section(command.clone(), json!(["REGION"])))]);
        assert!(b.validate("kim", &twice).unwrap_err().contains("in both lists"));
        let bad = config(Value::Null, &[("local", section(command, json!(["1BAD"])))]);
        assert!(b.validate("kim", &bad).unwrap_err().contains("environment variable"));
        let address = json!({"transport": "http", "url": "https://mcp.example.com/x"});
        let http = config(Value::Null, &[("wiki", section(address, json!(["API_TOKEN"])))]);
        assert!(b.validate("kim", &http).unwrap_err().contains("are for a command"));
    }
}
