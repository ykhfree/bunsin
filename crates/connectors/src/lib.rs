//! Connectors to external services. `McpConnector` is an MCP client.
//!
//! This crate does no permission checks. The engine checks the policy before it calls a tool.

use bunsin_core::ports::{BoxFuture, Connector, ConnectorError};
use bunsin_core::tool::ToolSpec;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::service::{RoleClient, RunningService, ServiceError};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Map, Value};
use std::fmt;

pub use bunsin_core::settings::McpConfig;

/// A connected MCP client session.
pub struct McpConnector {
    name: String,
    config: McpConfig,
    client: RunningService<RoleClient, ()>,
}

impl McpConnector {
    /// Starts the transport and does the MCP handshake.
    /// `token` is the bearer token for the HTTP transport. The stdio transport rejects a token.
    pub async fn connect(
        name: impl Into<String>,
        config: McpConfig,
        token: Option<&str>,
    ) -> Result<Self, ConnectorError> {
        let client = match &config {
            McpConfig::Http { url } => {
                let mut transport = StreamableHttpClientTransportConfig::with_uri(url.as_str());
                if let Some(token) = token {
                    // rmcp sends this value as `Authorization: Bearer <token>`.
                    transport = transport.auth_header(token);
                }
                // A redirect could carry the bearer token to another host, and could reach a host
                // that the server's address rules refused. The client therefore follows none.
                let http = reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .map_err(|e| ConnectorError::Transport(format!("the HTTP client cannot start: {e}")))?;
                ().serve(StreamableHttpClientTransport::with_client(http, transport))
                    .await
            }
            McpConfig::Stdio { command, args, env } => {
                if token.is_some() {
                    return Err(ConnectorError::Transport(
                        "a bearer token applies only to the HTTP transport".into(),
                    ));
                }
                if !command.is_absolute() {
                    return Err(ConnectorError::Transport(format!(
                        "the stdio command must be an absolute path, got `{}`",
                        command.display()
                    )));
                }
                let mut cmd = tokio::process::Command::new(command);
                cmd.args(args).envs(env);
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
