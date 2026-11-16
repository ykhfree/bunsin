//! Connectors to external services. `McpConnector` is an MCP client.
//! `ApiConnector` calls a REST API with fixed tools. `webhook` checks and reads inbound events.
//!
//! This crate does no permission checks. The engine checks the policy before it calls a tool.

pub mod api;
pub mod webhook;

pub use api::{ApiAuth, ApiConnector, ApiService};

use bunsin_core::ports::{BoxFuture, Connector, ConnectorError};
use bunsin_core::tool::ToolSpec;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

pub use bunsin_core::settings::McpConfig;

/// The token of an HTTP connector and the header that carries it.
#[derive(Clone, Copy)]
pub struct TokenAuth<'a> {
    pub token: &'a str,
    /// `None`: `Authorization: Bearer <token>`. `Some(name)`: `<name>: <token>`.
    pub header: Option<&'a str>,
}

impl<'a> TokenAuth<'a> {
    /// The token as `Authorization: Bearer <token>`.
    pub fn bearer(token: &'a str) -> Self {
        Self { token, header: None }
    }
}

/// A connected MCP client session.
pub struct McpConnector {
    name: String,
    config: McpConfig,
    client: RunningService<RoleClient, ()>,
}

impl McpConnector {
    /// Starts the transport and does the MCP handshake.
    /// `token` is the token for the HTTP transport. The stdio transport rejects a token.
    pub async fn connect(
        name: impl Into<String>,
        config: McpConfig,
        token: Option<TokenAuth<'_>>,
    ) -> Result<Self, ConnectorError> {
        Self::connect_with_env(name, config, token, &BTreeMap::new()).await
    }

    /// Like `connect`. A command gets `secret_env` as more environment variables.
    /// These values go to the child process only. They are not kept in the connector.
    pub async fn connect_with_env(
        name: impl Into<String>,
        config: McpConfig,
        token: Option<TokenAuth<'_>>,
        secret_env: &BTreeMap<String, String>,
    ) -> Result<Self, ConnectorError> {
        let client = match &config {
            McpConfig::Http { url } => {
                let mut transport = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                let mut headers = reqwest::header::HeaderMap::new();
                match token {
                    None => {}
                    // rmcp sends this value as `Authorization: Bearer <token>`.
                    Some(TokenAuth { token, header: None }) => transport = transport.auth_header(token),
                    // The named header carries the token as it is, on every request of this client.
                    Some(TokenAuth {
                        token,
                        header: Some(name),
                    }) => {
                        // The errors name the header. They never hold the token.
                        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
                            ConnectorError::Transport(format!("`{name}` is not a valid HTTP header name"))
                        })?;
                        let mut value = reqwest::header::HeaderValue::from_str(token).map_err(|_| {
                            ConnectorError::Transport("the token is not a valid HTTP header value".into())
                        })?;
                        value.set_sensitive(true);
                        headers.insert(name, value);
                    }
                }
                // A redirect could carry the token to another host, and could reach a host
                // that the server's address rules refused. The client therefore follows none.
                let http = reqwest::Client::builder()
                    .default_headers(headers)
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| ConnectorError::Transport(format!("the HTTP client cannot start: {e}")))?;
                ().serve(StreamableHttpClientTransport::with_client(http, transport))
                    .await
            }
            McpConfig::Stdio { command, args, env } => {
                if token.is_some() {
                    return Err(ConnectorError::Transport(
                        "a token applies only to the HTTP transport".into(),
                    ));
                }
                if !command.is_absolute() {
                    return Err(ConnectorError::Transport(format!(
                        "the stdio command must be an absolute path, got `{}`",
                        command.display()
                    )));
                }
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args).envs(env).envs(secret_env);
                let transport = TokioChildProcess::new(cmd)
                    .map_err(|e| ConnectorError::Transport(format!("cannot start `{}`: {e}", command.display())))?;
                ().serve(transport).await
            }
        }
        .map_err(|e| ConnectorError::Transport(e.to_string()))?;
        // The engine compares the live tool list with the approved one before each call.
        // A cached list would hide a change on the server, so the cache is off.
        client
            .set_response_cache_config(rmcp::ClientCacheConfig::disabled())
            .await;
        Ok(Self {
            name: name.into(),
            config,
            client,
        })
    }

    async fn list_all(&self) -> Result<Vec<ToolSpec>, ConnectorError> {
        let tools = self.client.list_all_tools().await.map_err(service_error)?;
        Ok(tools
            .into_iter()
            .map(|t| ToolSpec {
                name: t.name.into_owned(),
                description: t.description.map(|d| d.into_owned()).unwrap_or_default(),
                input_schema: Value::Object((*t.input_schema).clone()),
            })
            .collect())
    }

    async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, ConnectorError> {
        // List first, so that a tool the server does not list never reaches the server.
        if !self.list_all().await?.iter().any(|t| t.name == tool) {
            return Err(ConnectorError::UnknownTool(tool.to_owned()));
        }
        let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(args);
        let result = self.client.call_tool(params).await.map_err(service_error)?;
        tool_result(result)
    }
}

impl Connector for McpConnector {
    fn name(&self) -> &str {
        &self.name
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>> {
        Box::pin(self.list_all())
    }

    fn call_tool(&self, tool: &str, args: Map<String, Value>) -> BoxFuture<'_, Result<Value, ConnectorError>> {
        let tool = tool.to_owned();
        Box::pin(async move { self.call(&tool, args).await })
    }
}

impl fmt::Debug for McpConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpConnector")
            .field("name", &self.name)
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

/// A JSON-RPC error from the server is a failed call. Every other error is a transport problem.
fn service_error(error: ServiceError) -> ConnectorError {
    match error {
        ServiceError::McpError(e) => ConnectorError::Failed(e.message.into_owned()),
        other => ConnectorError::Transport(other.to_string()),
    }
}

/// Returns `structured_content` if present, otherwise the content blocks.
fn tool_result(result: CallToolResult) -> Result<Value, ConnectorError> {
    // Check the error flag first: an error result can also carry structured content.
    if result.is_error == Some(true) {
        let text: Vec<&str> = result
            .content
            .iter()
            .filter_map(|c| c.as_text())
            .map(|t| t.text.as_str())
            .collect();
        let message = match (text.is_empty(), &result.structured_content) {
            (true, Some(structured)) => structured.to_string(),
            _ => text.join("\n"),
        };
        return Err(ConnectorError::Failed(message));
    }
    match result.structured_content {
        Some(structured) => Ok(structured),
        None => serde_json::to_value(&result.content)
            .map_err(|e| ConnectorError::Transport(format!("cannot encode the content: {e}"))),
    }
}
