//! The plain settings types that the server, the command line and the app share.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// The HTTP API shape that an LLM endpoint speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Wire {
    /// OpenAI Chat Completions: `POST {base_url}/chat/completions`.
    #[serde(rename = "openai-chat")]
    OpenAiChat,
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
}

pub fn default_timeout_secs() -> u64 {
    120
}

/// One entry of the `connectors` section of the profile settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorSection {
    pub mcp: McpConfig,
    /// The key of the secret that holds the bearer token, if the MCP server needs one.
    pub token_secret: Option<String>,
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
