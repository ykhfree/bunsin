//! The plain settings types that the server, the command line and the app share.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// The HTTP API shape that an LLM endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wire {
    /// OpenAI Chat Completions: `POST {base_url}/chat/completions`.
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    /// OpenAI Responses: `POST {base_url}/responses`.
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// Anthropic Messages: `POST {base_url}/v1/messages`.
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
}

/// How to reach one MCP server. It never holds a token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "transport", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpConfig {
    /// Streamable HTTP. The bearer token is passed separately at connect time.
    Http { url: String },
    /// A child process that speaks MCP on stdin and stdout.
    /// `command` must be an absolute path, because a macOS GUI app has a minimal PATH.
    /// The child inherits the parent environment, plus `env`.
    Stdio {
        command: PathBuf,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: BTreeMap<String, String>,
    },
}

/// The address of the Claude API. Source: the API overview of the Claude docs
/// (https://platform.claude.com/docs/en/api/overview), read on 2026-11-15.
pub const ANTHROPIC_BASE_URL: &str = "https://api.anthropic.com";
/// The model the Claude docs say to start with (models overview, read on 2026-11-15).
pub const ANTHROPIC_DEFAULT_MODEL: &str = "claude-opus-5-5";

/// The `llm` section of the profile settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlmSection {
    pub wire: Wire,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// The key of the secret that holds the endpoint token.
    pub token_secret: String,
    /// The workspace ID for the Claude API (`anthropic-workspace-id`). The API needs it
    /// with a key that is not scoped to one workspace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

pub fn default_timeout_secs() -> u64 {
    120
}

/// The base URL of the Slack Web API.
pub const SLACK_BASE_URL: &str = "https://slack.com/api";

/// The service of a REST API connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiService {
    /// Base URL: `SLACK_BASE_URL`.
    Slack,
    /// Base URL: the site, for example `https://your-domain.atlassian.net`.
    Jira,
    /// Base URL: the site, for example `https://your-domain.atlassian.net`. The paths add `/wiki`.
    Confluence,
}

/// How a REST API connector sends its token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiAuthMethod {
    /// `Authorization: Bearer <token>`.
    #[default]
    Bearer,
    /// `Authorization: Basic <base64(email:token)>`, for an Atlassian API token.
    Basic,
}

/// How to reach a REST API. The token is not in here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApiConfig {
    pub service: ApiService,
    pub base_url: String,
    #[serde(default)]
    pub auth: ApiAuthMethod,
    /// The account email of basic auth. Bearer auth has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

impl ApiConfig {
    /// Refuses an auth method that the service or the email does not fit.
    pub fn check(&self) -> Result<(), String> {
        let email = self.email.as_deref().map(str::trim).unwrap_or_default();
        match self.auth {
            ApiAuthMethod::Basic if self.service == ApiService::Slack => {
                Err("Slack accepts only a bearer token".into())
            }
            ApiAuthMethod::Basic if email.is_empty() => Err("basic auth needs the account email".into()),
            // Basic auth joins the email and the token with `:`.
            ApiAuthMethod::Basic if email.contains(':') || email.chars().any(char::is_whitespace) => {
                Err(format!("`{email}` is not an email address"))
            }
            ApiAuthMethod::Bearer if self.email.is_some() => Err("bearer auth has no email".into()),
            _ => Ok(()),
        }
    }
}

/// What a connector reaches: an MCP server or a REST API.
/// In the settings it is the key `mcp` or the key `api`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectorKind {
    Mcp(McpConfig),
    Api(ApiConfig),
}

impl ConnectorKind {
    /// What the connector does, for messages: "has an address", "runs a command on the server"
    /// or "calls a REST API". Two connectors are of the same kind if this text is the same.
    pub fn describe(&self) -> &'static str {
        match self {
            ConnectorKind::Mcp(McpConfig::Http { .. }) => "has an address",
            ConnectorKind::Mcp(McpConfig::Stdio { .. }) => "runs a command on the server",
            ConnectorKind::Api(_) => "calls a REST API",
        }
    }

    /// The address that a stored token is bound to. A command has none.
    pub fn address(&self) -> Option<&str> {
        match self {
            ConnectorKind::Mcp(McpConfig::Http { url }) => Some(url),
            ConnectorKind::Mcp(McpConfig::Stdio { .. }) => None,
            ConnectorKind::Api(api) => Some(&api.base_url),
        }
    }
}

/// One entry of the `connectors` section of the profile settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StoredConnector")]
pub struct ConnectorSection {
    #[serde(flatten)]
    pub kind: ConnectorKind,
    /// The key of the secret that holds the token, if the server needs one.
    pub token_secret: Option<String>,
    /// The HTTP header that carries the token as it is, for example `X-API-Key`.
    /// `None`: the token goes as `Authorization: Bearer <token>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_header: Option<String>,
    /// The names of environment variables of a command whose values the server keeps encrypted.
    /// The values are not in the settings. A connector with an address has none.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub secret_env: BTreeSet<String>,
}

/// The stored form of a connector. It refuses unknown fields, so a token written into the
/// settings, or an old field, is never accepted.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredConnector {
    #[serde(default)]
    mcp: Option<McpConfig>,
    #[serde(default)]
    api: Option<ApiConfig>,
    #[serde(default)]
    token_secret: Option<String>,
    #[serde(default)]
    token_header: Option<String>,
    #[serde(default)]
    secret_env: BTreeSet<String>,
}

impl TryFrom<StoredConnector> for ConnectorSection {
    type Error = String;

    fn try_from(stored: StoredConnector) -> Result<Self, String> {
        let kind = match (stored.mcp, stored.api) {
            (Some(mcp), None) => ConnectorKind::Mcp(mcp),
            (None, Some(api)) => ConnectorKind::Api(api),
            (Some(_), Some(_)) => return Err("a connector has `mcp` or `api`, not both".into()),
            (None, None) => return Err("a connector needs `mcp` or `api`".into()),
        };
        Ok(Self {
            kind,
            token_secret: stored.token_secret,
            token_header: stored.token_header,
            secret_env: stored.secret_env,
        })
    }
}

/// True if `name` can name an environment variable: a letter or `_`, then letters, digits and `_`,
/// at most 64 characters.
pub fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    name.len() <= 64
        && bytes.next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Every key of a secret environment variable starts with this. No token key that the server
/// chooses does: those start with `MCP_` or are `LLM`.
pub const ENV_SECRET_PREFIX: &str = "ENV_";

/// The secret key of one secret environment variable of a connector.
/// The variable name is written as hex, because a key has only upper-case letters and digits
/// in groups. The last group is the variable and the groups before it are the connector, so
/// two variables never give the same key.
pub fn env_secret_key(connector: &str, variable: &str) -> String {
    let hex: String = variable.bytes().map(|byte| format!("{byte:02X}")).collect();
    format!(
        "{ENV_SECRET_PREFIX}{}_{hex}",
        connector.to_ascii_uppercase().replace('-', "_")
    )
}

/// What a secret environment variable is stored for, in the place of a token's address.
/// It is not an address, so the value is never accepted as the token of an address.
pub fn env_secret_origin(connector: &str) -> String {
    format!("environment of connector {connector}")
}

/// Headers that the HTTP client or the MCP transport sets itself. A token must not replace them.
const RESERVED_HEADERS: [&str; 12] = [
    "host",
    "content-length",
    "content-type",
    "accept",
    "connection",
    "transfer-encoding",
    "upgrade",
    "te",
    "trailer",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

/// Checks the name of a header that carries a connector's token:
/// 1 to 64 of a-z, A-Z, 0-9, `-` and `_`, and not a header that the transport sets.
pub fn check_token_header(name: &str) -> Result<(), String> {
    let valid = (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !valid {
        return Err(format!(
            "the header name `{name}` is invalid: use 1 to 64 of a-z, A-Z, 0-9, `-` and `_`"
        ));
    }
    if RESERVED_HEADERS.contains(&name.to_ascii_lowercase().as_str()) {
        return Err(format!(
            "the header `{name}` is set by the connection itself and cannot carry a token"
        ));
    }
    Ok(())
}

/// The secret key of the LLM token when the server chooses the name.
pub const LLM_SECRET_KEY: &str = "LLM";

/// True if `name` can be a connector name: 1 to 32 of a-z, 0-9 and `-`.
/// A connector name is part of every tool ID (`<connector>/<tool>`) and of a secret key.
pub fn valid_connector_name(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// The secret key of a connector's token when the server chooses the name.
/// Two connector names never give the same key: `-` becomes `_`, and a name has no `_`.
pub fn connector_secret_key(name: &str) -> String {
    format!("MCP_{}", name.to_ascii_uppercase().replace('-', "_"))
}

/// The scheme, host and port of an address, for example `https://mcp.example.com:443`.
/// A stored token is bound to the origin it was entered for.
pub fn origin_of(address: &str) -> Option<String> {
    // The part before the path: `scheme://host[:port]`. No URL crate is needed for this.
    let (scheme, rest) = address.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // An address with user information is refused elsewhere. It has no stable origin here.
    if scheme.is_empty() || authority.is_empty() || authority.contains('@') {
        return None;
    }
    let scheme = scheme.to_ascii_lowercase();
    let authority = authority.to_ascii_lowercase();
    let has_port = authority
        .rsplit_once(':')
        .is_some_and(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    let default_port = match scheme.as_str() {
        "https" => "443",
        "http" => "80",
        _ => return None,
    };
    Some(if has_port {
        format!("{scheme}://{authority}")
    } else {
        format!("{scheme}://{authority}:{default_port}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_ignore_the_path_and_spell_out_the_port() {
        assert_eq!(
            origin_of("https://mcp.example.com/slack").as_deref(),
            Some("https://mcp.example.com:443")
        );
        assert_eq!(
            origin_of("https://MCP.Example.com:443/x?y#z").as_deref(),
            Some("https://mcp.example.com:443")
        );
        assert_eq!(
            origin_of("http://10.0.0.5:9000").as_deref(),
            Some("http://10.0.0.5:9000")
        );
        assert_eq!(origin_of("https://[::1]/x").as_deref(), Some("https://[::1]:443"));
        assert_eq!(origin_of("https://[::1]:8443").as_deref(), Some("https://[::1]:8443"));
    }

    #[test]
    fn another_host_port_or_scheme_is_another_origin() {
        let base = origin_of("https://mcp.example.com/slack");
        for other in [
            "https://mcp.example.com.evil.example/slack",
            "https://evil.example/mcp.example.com",
            "https://mcp.example.com:8443/slack",
            "http://mcp.example.com/slack",
            "https://mcp.example.com@evil.example/slack",
        ] {
            assert_ne!(origin_of(other), base, "{other}");
        }
        for bad in [
            "",
            "mcp.example.com",
            "ftp://mcp.example.com",
            "https://",
            "https://user@host",
        ] {
            assert_eq!(origin_of(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_connector_is_either_an_mcp_server_or_a_rest_api() {
        use serde_json::json;
        let parse = |value: serde_json::Value| serde_json::from_value::<ConnectorSection>(value);
        // Settings from before REST API connectors existed still load, and are written back the same.
        let stored = json!({"mcp": {"transport": "http", "url": "https://mcp.example/x"}, "token_secret": "MCP_X"});
        let section = parse(stored.clone()).unwrap();
        assert!(matches!(section.kind, ConnectorKind::Mcp(McpConfig::Http { .. })));
        assert_eq!(serde_json::to_value(&section).unwrap(), stored);

        let api = json!({"api": {"service": "jira", "base_url": "https://acme.atlassian.net", "auth": "basic",
            "email": "kim@acme.com"}, "token_secret": "MCP_JIRA"});
        let section = parse(api.clone()).unwrap();
        let ConnectorKind::Api(config) = &section.kind else {
            panic!("expected an API connector, got {section:?}")
        };
        assert_eq!((config.service, config.auth), (ApiService::Jira, ApiAuthMethod::Basic));
        assert_eq!(section.kind.address(), Some("https://acme.atlassian.net"));
        assert_eq!(serde_json::to_value(&section).unwrap(), api);
        // `auth` defaults to bearer.
        let slack =
            parse(json!({"api": {"service": "slack", "base_url": SLACK_BASE_URL}, "token_secret": "MCP_S"})).unwrap();
        assert!(matches!(&slack.kind, ConnectorKind::Api(c) if c.auth == ApiAuthMethod::Bearer));

        let both = json!({"mcp": {"transport": "http", "url": "https://x"}, "api": {"service": "slack", "base_url": SLACK_BASE_URL}});
        assert!(parse(both).unwrap_err().to_string().contains("not both"));
        assert!(
            parse(json!({"token_secret": "X"}))
                .unwrap_err()
                .to_string()
                .contains("needs")
        );
        // A token in the settings is refused, also inside `api`.
        let inline = json!({"api": {"service": "slack", "base_url": SLACK_BASE_URL, "token": "xoxb-1"}});
        assert!(parse(inline).is_err());
        assert!(parse(json!({"mcp": {"transport": "http", "url": "https://x"}, "token_env": "HOME"})).is_err());
    }

    #[test]
    fn the_auth_method_must_fit_the_service_and_the_email() {
        let config = |service, auth, email: Option<&str>| ApiConfig {
            service,
            base_url: SLACK_BASE_URL.into(),
            auth,
            email: email.map(str::to_owned),
        };
        use ApiAuthMethod::{Basic, Bearer};
        assert_eq!(config(ApiService::Slack, Bearer, None).check(), Ok(()));
        assert_eq!(config(ApiService::Jira, Basic, Some("kim@acme.com")).check(), Ok(()));
        assert_eq!(config(ApiService::Confluence, Bearer, None).check(), Ok(()));
        for (bad, why) in [
            (config(ApiService::Slack, Basic, Some("kim@acme.com")), "only a bearer"),
            (config(ApiService::Jira, Basic, None), "needs the account email"),
            (config(ApiService::Jira, Basic, Some("  ")), "needs the account email"),
            (config(ApiService::Jira, Basic, Some("kim:x@acme.com")), "not an email"),
            (config(ApiService::Jira, Bearer, Some("kim@acme.com")), "has no email"),
        ] {
            let error = bad.check().unwrap_err();
            assert!(error.contains(why), "{bad:?}: {error}");
        }
    }

    #[test]
    fn connector_names_and_their_secret_keys() {
        for name in ["slack", "jira-cloud", "c1"] {
            assert!(valid_connector_name(name), "{name}");
        }
        let long = "a".repeat(33);
        for name in ["", "Slack", "my slack", "a/b", "a_b", "../x", long.as_str()] {
            assert!(!valid_connector_name(name), "{name:?}");
        }
        assert_eq!(connector_secret_key("jira-cloud"), "MCP_JIRA_CLOUD");
        assert_ne!(connector_secret_key("a-b"), connector_secret_key("ab"));
    }
}
