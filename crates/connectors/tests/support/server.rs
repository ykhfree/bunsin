//! A test MCP server. The integration tests serve it over HTTP. The stdio example serves it on stdio.

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, JsonObject, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Tools per `tools/list` page. The server lists 5 tools, so a full listing takes 3 pages.
pub const PAGE_SIZE: usize = 2;

/// One `tools/call` as the server received it.
pub type Call = (String, Option<JsonObject>);

#[derive(Clone, Default)]
pub struct TestServer {
    /// Every `tools/call` request, recorded before the server looks at the tool name.
    pub calls: Arc<Mutex<Vec<Call>>>,
    /// The number of `tools/list` requests.
    pub list_requests: Arc<Mutex<usize>>,
    /// While true, a call does not answer until the client cancels it.
    pub hang: Arc<Mutex<bool>>,
    /// While true, a tool list does not answer until the client cancels it.
    pub hang_list: Arc<Mutex<bool>>,
    /// The number of calls and tool lists that the client cancelled.
    pub cancelled: Arc<Mutex<usize>>,
}

fn object(value: Value) -> JsonObject {
    match value {
        Value::Object(map) => map,
        other => panic!("a schema must be a JSON object, got {other}"),
    }
}

pub fn echo_schema() -> JsonObject {
    object(json!({
        "type": "object",
        "properties": {"text": {"type": "string"}, "count": {"type": "integer"}},
        "required": ["text"]
    }))
}

/// The tools in listing order.
pub fn tools() -> Vec<Tool> {
    let empty = || object(json!({"type": "object"}));
    vec![
        Tool::new("echo", "Returns the arguments as structured content", echo_schema()),
        Tool::new_with_raw("plain", None, empty()),
        Tool::new("fail", "Always returns a tool error result", empty()),
        Tool::new("reject", "Always answers with a JSON-RPC error", empty()),
        Tool::new("whoami", "Returns the process arguments and BUNSIN_TEST_VALUE", empty()),
    ]
}

impl ServerHandler for TestServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        *self.list_requests.lock().unwrap() += 1;
        let hang = *self.hang_list.lock().unwrap();
        if hang {
            context.ct.cancelled().await;
            *self.cancelled.lock().unwrap() += 1;
            return Err(McpError::internal_error("cancelled", None));
        }
        let start = match request.and_then(|r| r.cursor) {
            Some(cursor) => cursor
                .parse::<usize>()
                .map_err(|_| McpError::invalid_params("bad cursor", None))?,
            None => 0,
        };
        let all = tools();
        let end = (start + PAGE_SIZE).min(all.len());
        let mut result = ListToolsResult::with_all_items(all[start..end].to_vec());
        result.next_cursor = (end < all.len()).then(|| end.to_string());
        Ok(result)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        self.calls
            .lock()
            .unwrap()
            .push((request.name.to_string(), request.arguments.clone()));
        let hang = *self.hang.lock().unwrap();
        if hang {
            context.ct.cancelled().await;
            *self.cancelled.lock().unwrap() += 1;
            return Err(McpError::internal_error("cancelled", None));
        }
        let result = match request.name.as_ref() {
            "echo" => CallToolResult::structured(Value::Object(request.arguments.unwrap_or_default())),
            "plain" => CallToolResult::success(vec![ContentBlock::text("first"), ContentBlock::text("second")]),
            "fail" => {
                // An error result with both text and structured content.
                // With `{"no_text": true}` the result has structured content only.
                let no_text = request
                    .arguments
                    .as_ref()
                    .is_some_and(|a| a.get("no_text") == Some(&Value::Bool(true)));
                let text = if no_text {
                    vec![]
                } else {
                    vec![ContentBlock::text("quota exceeded")]
                };
                let mut result = CallToolResult::error(text);
                result.structured_content = Some(json!({"code": "quota"}));
                result
            }
            "reject" => return Err(McpError::invalid_params("channel is missing", None)),
            "whoami" => CallToolResult::structured(json!({
                "args": std::env::args().skip(1).collect::<Vec<_>>(),
                "value": std::env::var("BUNSIN_TEST_VALUE").ok(),
            })),
            other => return Err(McpError::invalid_params(format!("no tool `{other}`"), None)),
        };
        Ok(result.into())
    }
}
