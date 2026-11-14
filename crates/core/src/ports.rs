//! Interfaces that the engine calls. Implementations live in other crates.

use crate::llm::{CompletionRequest, CompletionResponse, LlmError};
use crate::tool::ToolSpec;
use serde_json::{Map, Value};
use std::future::Future;
use std::pin::Pin;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

pub trait LlmClient: Send + Sync {
    fn complete(&self, request: CompletionRequest) -> BoxFuture<'_, Result<CompletionResponse, LlmError>>;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectorError {
    #[error("the connector has no tool named `{0}`")]
    UnknownTool(String),
    #[error("the tool call failed: {0}")]
    Failed(String),
    #[error("transport failure: {0}")]
    Transport(String),
}

/// One external service (Slack, Jira, Confluence), reached by MCP or by its REST API.
pub trait Connector: Send + Sync {
    /// The connector name. Tool IDs are `<name>/<tool>`.
    fn name(&self) -> &str;
    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>>;
    fn call_tool(&self, tool: &str, args: Map<String, Value>) -> BoxFuture<'_, Result<Value, ConnectorError>>;
}
