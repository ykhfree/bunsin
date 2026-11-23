//! Scripted stand-ins for the LLM and for connectors. For tests only.

use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason, Usage};
use bunsin_core::ports::{BoxFuture, Connector, ConnectorError, LlmClient};
use bunsin_core::tool::ToolSpec;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::sync::Mutex;

/// Returns scripted replies in order and records every request.
#[derive(Default)]
pub struct FakeLlm {
    replies: Mutex<VecDeque<Result<CompletionResponse, LlmError>>>,
    requests: Mutex<Vec<CompletionRequest>>,
}

impl FakeLlm {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a normal reply that used `tokens` tokens.
    pub fn reply(&self, content: &str, tokens: u64) -> &Self {
        self.push(Ok(CompletionResponse {
            content: content.to_owned(),
            stop: StopReason::End,
            usage: Usage {
                input_tokens: tokens,
                output_tokens: 0,
            },
        }))
    }

    pub fn push(&self, reply: Result<CompletionResponse, LlmError>) -> &Self {
        self.replies.lock().unwrap().push_back(reply);
        self
    }

    pub fn requests(&self) -> Vec<CompletionRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl LlmClient for FakeLlm {
    fn complete(&self, request: CompletionRequest) -> BoxFuture<'_, Result<CompletionResponse, LlmError>> {
        self.requests.lock().unwrap().push(request);
        let reply = self.replies.lock().unwrap().pop_front();
        Box::pin(async move { reply.unwrap_or_else(|| Err(LlmError::Transport("FakeLlm has no reply queued".into()))) })
    }
}

/// A connector with a tool list that tests can change, and a record of every call.
pub struct FakeConnector {
    name: String,
    tools: Mutex<Vec<ToolSpec>>,
    calls: Mutex<Vec<(String, Map<String, Value>)>>,
    failure: Mutex<Option<ConnectorError>>,
    results: Mutex<std::collections::BTreeMap<String, Value>>,
}

pub fn tool(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: json!({"type": "object"}),
    }
}

impl FakeConnector {
    pub fn new(name: &str, tools: Vec<ToolSpec>) -> Self {
        Self {
            name: name.to_owned(),
            tools: Mutex::new(tools),
            calls: Mutex::new(Vec::new()),
            failure: Mutex::new(None),
            results: Mutex::new(Default::default()),
        }
    }

    pub fn set_tools(&self, tools: Vec<ToolSpec>) {
        *self.tools.lock().unwrap() = tools;
    }

    /// Makes later calls of `tool` return `result`.
    pub fn set_result(&self, tool: &str, result: Value) {
        self.results.lock().unwrap().insert(tool.to_owned(), result);
    }

    /// Makes every later call fail with `message`.
    pub fn fail_with(&self, message: &str) {
        self.fail_with_error(ConnectorError::Failed(message.to_owned()));
    }

    /// Makes every later call end with `error`.
    pub fn fail_with_error(&self, error: ConnectorError) {
        *self.failure.lock().unwrap() = Some(error);
    }

    pub fn calls(&self) -> Vec<(String, Map<String, Value>)> {
        self.calls.lock().unwrap().clone()
    }
}

impl Connector for FakeConnector {
    fn name(&self) -> &str {
        &self.name
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>> {
        let tools = self.tools.lock().unwrap().clone();
        Box::pin(async move { Ok(tools) })
    }

    fn call_tool(&self, tool: &str, args: Map<String, Value>) -> BoxFuture<'_, Result<Value, ConnectorError>> {
        self.calls.lock().unwrap().push((tool.to_owned(), args.clone()));
        let result = match self.failure.lock().unwrap().clone() {
            Some(error) => Err(error),
            None => match self.results.lock().unwrap().get(tool) {
                Some(result) => Ok(result.clone()),
                None => Ok(json!({"ok": true, "tool": tool, "echo": args})),
            },
        };
        Box::pin(async move { result })
    }
}
