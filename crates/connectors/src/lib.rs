//! Connectors to external services. `McpConnector` is an MCP client, with a token or with OAuth.
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
use rmcp::transport::auth::{AuthClient, AuthorizationManager};
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{StreamableHttpClientTransport, TokioChildProcess};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

pub use bunsin_core::settings::McpConfig;

/// How long a tool call of an MCP connector may take, with the tool list that comes first.
/// A tool list alone, as the engine asks for it before each call, has the same limit.
/// Tools can be slow, so the limit is generous. A request that runs out is cancelled on the server.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(120);

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
    /// The time limit of one tool call or tool list. `CALL_TIMEOUT` unless changed.
    call_timeout: Duration,
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
                let http = http_client(headers)?;
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
        Ok(Self::started(name.into(), config, client).await)
    }

    /// Connects to the MCP server at `url` with the OAuth credentials of `auth`.
    /// `auth` sends the access token and refreshes it when it expires.
    pub async fn connect_oauth(
        name: impl Into<String>,
        url: &str,
        auth: AuthorizationManager,
    ) -> Result<Self, ConnectorError> {
        let transport = StreamableHttpClientTransportConfig::with_uri(url);
        let http = AuthClient::new(http_client(reqwest::header::HeaderMap::new())?, auth);
        let client =
            ().serve(StreamableHttpClientTransport::with_client(http, transport))
                .await
                .map_err(|e| ConnectorError::Transport(e.to_string()))?;
        let config = McpConfig::Http { url: url.to_owned() };
        Ok(Self::started(name.into(), config, client).await)
    }

    async fn started(name: String, config: McpConfig, client: RunningService<RoleClient, ()>) -> Self {
        // The engine compares the live tool list with the approved one before each call.
        // A cached list would hide a change on the server, so the cache is off.
        client
            .set_response_cache_config(rmcp::ClientCacheConfig::disabled())
            .await;
        Self {
            name,
            config,
            client,
            call_timeout: CALL_TIMEOUT,
        }
    }

    /// The same connector with another time limit for each tool call and tool list.
    pub fn with_call_timeout(mut self, limit: Duration) -> Self {
        self.call_timeout = limit;
        self
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

    /// The list and the call share one time limit. The end of the limit has two meanings.
    /// During the list, the call was not sent. During the call, the server may have done it.
    async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, ConnectorError> {
        let deadline = tokio::time::Instant::now() + self.call_timeout;
        let limit = self.call_timeout.as_secs_f64();
        // List first, so that a tool the server does not list never reaches the server.
        let tools = tokio::time::timeout_at(deadline, self.list_all()).await.map_err(|_| {
            ConnectorError::Transport(format!(
                "the tool list before the call of `{tool}` did not end in {limit} s; the call was not sent"
            ))
        })??;
        if !tools.iter().any(|t| t.name == tool) {
            return Err(ConnectorError::UnknownTool(tool.to_owned()));
        }
        let params = CallToolRequestParams::new(tool.to_owned()).with_arguments(args);
        let result = tokio::time::timeout_at(deadline, self.client.call_tool(params))
            .await
            .map_err(|_| {
                ConnectorError::Unanswered(format!(
                    "the call of `{tool}` did not end in {limit} s and was cancelled; the server may have done it anyway"
                ))
            })?
            .map_err(service_error)?;
        tool_result(result)
    }
}

impl Connector for McpConnector {
    fn name(&self) -> &str {
        &self.name
    }

    /// The engine lists the tools before each call, so a list that never ends would hold the event too.
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>> {
        Box::pin(async move {
            match tokio::time::timeout(self.call_timeout, self.list_all()).await {
                Ok(result) => result,
                Err(_) => Err(ConnectorError::Transport(format!(
                    "the tool list did not end in {} s and was cancelled",
                    self.call_timeout.as_secs_f64()
                ))),
            }
        })
    }

    /// A server that never answers must not hold the event that called the tool. At the time
    /// limit the request is dropped, and rmcp tells the server that it is cancelled.
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

/// The HTTP client of an MCP server at an address, with `headers` on every request.
fn http_client(headers: reqwest::header::HeaderMap) -> Result<reqwest::Client, ConnectorError> {
    // A redirect could carry the token to another host, and could reach a host
    // that the server's address rules refused. The client therefore follows none.
    reqwest::Client::builder()
        .default_headers(headers)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|e| ConnectorError::Transport(format!("the HTTP client cannot start: {e}")))
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
