//! `ApiConnector` calls the REST API of Slack, Jira Cloud or Confluence Cloud directly.
//!
//! The tools are fixed in this file. Each has a JSON input schema,
//! so the engine can pin and check them like MCP tools.
//! The endpoint shapes follow the official API docs. Nobody ran this code against the real services.
//! Only local stub servers in the tests exercised it.

use base64::Engine;
use bunsin_core::ports::{BoxFuture, Connector, ConnectorError};
use bunsin_core::tool::ToolSpec;
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue};
use serde_json::{Map, Value, json};
use std::fmt;

/// The base URL of the Slack Web API.
pub const SLACK_BASE_URL: &str = "https://slack.com/api";

/// The longest API error detail that goes into an error text, in characters.
const MAX_DETAIL_CHARS: usize = 300;

/// The service that an `ApiConnector` calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiService {
    /// Base URL: `SLACK_BASE_URL`.
    Slack,
    /// Base URL: the site, for example `https://your-domain.atlassian.net`.
    Jira,
    /// Base URL: the site, for example `https://your-domain.atlassian.net`. The paths add `/wiki`.
    Confluence,
}

/// The credentials of an API connector.
#[derive(Clone)]
pub enum ApiAuth {
    /// `Authorization: Bearer <token>`. Slack accepts only this.
    Bearer(String),
    /// `Authorization: Basic <base64(email:token)>`, for an Atlassian API token.
    Basic { email: String, token: String },
}

/// A connector that calls a REST API with a fixed set of tools.
pub struct ApiConnector {
    name: String,
    service: ApiService,
    base: Url,
    /// The full `Authorization` header value. It is marked sensitive.
    authorization: HeaderValue,
    /// Texts that must not appear in an error: the token, and for basic auth the encoded pair.
    secrets: Vec<String>,
    http: reqwest::Client,
}

impl ApiConnector {
    /// Checks the base URL and the credentials. It sends no request.
    pub fn new(
        name: impl Into<String>,
        service: ApiService,
        base_url: &str,
        auth: ApiAuth,
    ) -> Result<Self, ConnectorError> {
        let base = Url::parse(base_url)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.query().is_none())
            .ok_or_else(|| ConnectorError::Transport(format!("`{base_url}` is not an http or https base URL")))?;
        let (authorization, secrets) = match auth {
            ApiAuth::Bearer(token) => (format!("Bearer {token}"), vec![token]),
            ApiAuth::Basic { .. } if service == ApiService::Slack => {
                return Err(ConnectorError::Transport("Slack accepts only a bearer token".into()));
            }
            ApiAuth::Basic { email, token } => {
                // The encoded pair holds the token too, so an error must not show it either.
                let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{email}:{token}"));
                (format!("Basic {encoded}"), vec![token, encoded])
            }
        };
        let mut header = HeaderValue::from_str(&authorization)
            .map_err(|_| ConnectorError::Transport("the token is not a valid HTTP header value".into()))?;
        header.set_sensitive(true);
        // A redirect could carry the token to another host, and could reach a host
        // that the server's address rules refused. The client therefore follows none.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ConnectorError::Transport(format!("the HTTP client cannot start: {e}")))?;
        Ok(Self {
            name: name.into(),
            service,
            base,
            authorization: header,
            secrets,
            http,
        })
    }

    /// The base URL with `segments` added to its path.
    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("new() accepts only http and https URLs, which have a path")
            .pop_if_empty()
            .extend(segments);
        url
    }

    async fn call(&self, tool: &str, args: Map<String, Value>) -> Result<Value, ConnectorError> {
        let spec = tools(self.service)
            .into_iter()
            .find(|t| t.name == tool)
            .ok_or_else(|| ConnectorError::UnknownTool(tool.to_owned()))?;
        let args = Args::new(&spec, &args)?;
        let request = match (self.service, tool) {
            (ApiService::Slack, "search_messages") => {
                let mut url = self.url(&["search.messages"]);
                let mut query = url.query_pairs_mut();
                query.append_pair("query", args.text("query")?);
                if let Some(count) = args.count("count", 100)? {
                    query.append_pair("count", &count.to_string());
                }
                drop(query);
                self.http.get(url)
            }
            (ApiService::Slack, "get_thread") => {
                let mut url = self.url(&["conversations.replies"]);
                let mut query = url.query_pairs_mut();
                query.append_pair("channel", args.text("channel")?);
                query.append_pair("ts", args.text("ts")?);
                if let Some(limit) = args.count("limit", 1000)? {
                    query.append_pair("limit", &limit.to_string());
                }
                drop(query);
                self.http.get(url)
            }
            (ApiService::Slack, "post_message") => {
                let mut body = json!({"channel": args.text("channel")?, "text": args.text("text")?});
                if let Some(thread_ts) = args.optional_text("thread_ts")? {
                    body["thread_ts"] = json!(thread_ts);
                }
                self.json(self.http.post(self.url(&["chat.postMessage"])), &body)
            }
            (ApiService::Jira, "search_issues") => {
                let mut body = json!({
                    "jql": args.text("jql")?,
                    "fields": ["summary", "status", "assignee", "reporter", "priority", "issuetype", "updated"],
                });
                if let Some(max) = args.count("max_results", 100)? {
                    body["maxResults"] = json!(max);
                }
                self.json(self.http.post(self.url(&["rest", "api", "3", "search", "jql"])), &body)
            }
            (ApiService::Jira, "get_issue") => {
                self.http
                    .get(self.url(&["rest", "api", "3", "issue", args.path_id("issue")?]))
            }
            (ApiService::Jira, "add_comment") => {
                let url = self.url(&["rest", "api", "3", "issue", args.path_id("issue")?, "comment"]);
                self.json(self.http.post(url), &json!({"body": adf_document(args.text("text")?)}))
            }
            (ApiService::Confluence, "search_pages") => {
                let mut url = self.url(&["wiki", "rest", "api", "search"]);
                let mut query = url.query_pairs_mut();
                query.append_pair("cql", args.text("cql")?);
                if let Some(limit) = args.count("limit", 100)? {
                    query.append_pair("limit", &limit.to_string());
                }
                drop(query);
                self.http.get(url)
            }
            (ApiService::Confluence, "get_page") => {
                let mut url = self.url(&["wiki", "api", "v2", "pages", args.path_id("page_id")?]);
                url.query_pairs_mut().append_pair("body-format", "storage");
                self.http.get(url)
            }
            (ApiService::Confluence, "create_page") => {
                let mut body = json!({
                    "spaceId": args.text("space_id")?,
                    "status": "current",
                    "title": args.text("title")?,
                    "body": {"representation": "storage", "value": storage_paragraphs(args.text("text")?)},
                });
                if let Some(parent) = args.optional_text("parent_id")? {
                    body["parentId"] = json!(parent);
                }
                self.json(self.http.post(self.url(&["wiki", "api", "v2", "pages"])), &body)
            }
            _ => unreachable!("every tool in tools() has a request above"),
        };
        self.send(request).await
    }

    fn json(&self, request: reqwest::RequestBuilder, body: &Value) -> reqwest::RequestBuilder {
        request
            .header(CONTENT_TYPE, "application/json; charset=utf-8")
            .body(body.to_string())
    }

    async fn send(&self, request: reqwest::RequestBuilder) -> Result<Value, ConnectorError> {
        let response = request
            .header(AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|e| ConnectorError::Transport(format!("the request failed: {e}")))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(ConnectorError::Transport(format!(
                "the server answered with a redirect (HTTP {}); the connector follows no redirects",
                status.as_u16()
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| ConnectorError::Transport(format!("cannot read the response: {e}")))?;
        let body: Option<Value> = serde_json::from_slice(&bytes).ok();
        if !status.is_success() {
            let detail = body
                .as_ref()
                .map(|body| self.error_detail(body))
                .filter(|detail| !detail.is_empty())
                .unwrap_or_else(|| "no error detail".into());
            return Err(ConnectorError::Failed(format!("HTTP {}: {detail}", status.as_u16())));
        }
        let body = body.ok_or_else(|| ConnectorError::Transport("the response is not JSON".into()))?;
        // The Slack Web API answers HTTP 200 also for a failed call. `ok` tells the result.
        if self.service == ApiService::Slack && body.get("ok") != Some(&Value::Bool(true)) {
            return Err(ConnectorError::Failed(self.error_detail(&body)));
        }
        Ok(body)
    }

    /// The error text of an API error body.
    fn error_detail(&self, body: &Value) -> String {
        let mut parts: Vec<String> = Vec::new();
        let mut push = |value: &Value| {
            if let Some(text) = value.as_str().filter(|t| !t.is_empty()) {
                parts.push(text.to_owned());
            }
        };
        match self.service {
            // Slack: {"ok": false, "error": "missing_scope", "needed": "search:read"}.
            ApiService::Slack => {
                push(body.get("error").unwrap_or(&Value::Null));
                if let Some(needed) = body.get("needed").and_then(Value::as_str) {
                    parts.push(format!("needed: {needed}"));
                }
            }
            // Jira: {"errorMessages": [..], "errors": {field: message}}.
            // Confluence v2: {"errors": [{"title": .., "detail": ..}]}. Confluence v1: {"message": ..}.
            ApiService::Jira | ApiService::Confluence => {
                body.get("errorMessages")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .for_each(&mut push);
                match body.get("errors") {
                    Some(Value::Object(fields)) => fields.values().for_each(&mut push),
                    Some(Value::Array(errors)) => {
                        for error in errors {
                            push(error.get("title").unwrap_or(&Value::Null));
                            push(error.get("detail").unwrap_or(&Value::Null));
                        }
                    }
                    _ => {}
                }
                push(body.get("message").unwrap_or(&Value::Null));
            }
        }
        let detail = parts.join("; ");
        match detail.char_indices().nth(MAX_DETAIL_CHARS) {
            Some((end, _)) => format!("{}...", &detail[..end]),
            None => detail,
        }
    }

    /// Removes the secrets from an error text.
    /// A server can echo a request header in its error body.
    fn redact(&self, error: ConnectorError) -> ConnectorError {
        let clean = |mut text: String| {
            for secret in self.secrets.iter().filter(|s| !s.is_empty()) {
                text = text.replace(secret.as_str(), "[redacted]");
            }
            text
        };
        match error {
            ConnectorError::UnknownTool(text) => ConnectorError::UnknownTool(clean(text)),
            ConnectorError::Failed(text) => ConnectorError::Failed(clean(text)),
            ConnectorError::Transport(text) => ConnectorError::Transport(clean(text)),
        }
    }
}

impl Connector for ApiConnector {
    fn name(&self) -> &str {
        &self.name
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>> {
        let tools = tools(self.service);
        Box::pin(async move { Ok(tools) })
    }

    fn call_tool(&self, tool: &str, args: Map<String, Value>) -> BoxFuture<'_, Result<Value, ConnectorError>> {
        let tool = tool.to_owned();
        Box::pin(async move { self.call(&tool, args).await.map_err(|e| self.redact(e)) })
    }
}

impl fmt::Debug for ApiConnector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApiConnector")
            .field("name", &self.name)
            .field("service", &self.service)
            .field("base", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

/// One tool argument in a schema.
struct Param {
    name: &'static str,
    kind: &'static str,
    description: &'static str,
    required: bool,
}

const fn param(name: &'static str, kind: &'static str, description: &'static str, required: bool) -> Param {
    Param {
        name,
        kind,
        description,
        required,
    }
}

fn spec(name: &str, description: &str, params: &[Param]) -> ToolSpec {
    let properties: Map<String, Value> = params
        .iter()
        .map(|p| {
            let mut schema = json!({"type": p.kind, "description": p.description});
            if p.kind == "integer" {
                schema["minimum"] = json!(1);
            }
            (p.name.to_owned(), schema)
        })
        .collect();
    let required: Vec<&str> = params.iter().filter(|p| p.required).map(|p| p.name).collect();
    ToolSpec {
        name: name.to_owned(),
        description: description.to_owned(),
        input_schema: json!({
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false,
        }),
    }
}

/// The fixed tools of a service. Reads are search and get. Writes add content. No tool deletes.
pub fn tools(service: ApiService) -> Vec<ToolSpec> {
    match service {
        ApiService::Slack => vec![
            spec(
                "search_messages",
                "Searches messages with Slack search syntax (search.messages). Needs a user token.",
                &[
                    param(
                        "query",
                        "string",
                        "Slack search query, for example `to:me deploy`",
                        true,
                    ),
                    param("count", "integer", "Results per page, at most 100", false),
                ],
            ),
            spec(
                "get_thread",
                "Reads a message and its thread replies (conversations.replies).",
                &[
                    param("channel", "string", "Channel ID, for example C123ABC456", true),
                    param("ts", "string", "The `ts` of the thread's parent message", true),
                    param("limit", "integer", "Messages to return, at most 1000", false),
                ],
            ),
            spec(
                "post_message",
                "Posts a message to a channel, or a reply in a thread (chat.postMessage).",
                &[
                    param("channel", "string", "Channel ID, for example C123ABC456", true),
                    param("text", "string", "The message text", true),
                    param(
                        "thread_ts",
                        "string",
                        "The `ts` of the parent message, to reply in its thread",
                        false,
                    ),
                ],
            ),
        ],
        ApiService::Jira => vec![
            spec(
                "search_issues",
                "Searches issues with JQL (POST /rest/api/3/search/jql).",
                &[
                    param(
                        "jql",
                        "string",
                        "JQL query, for example `assignee = currentUser()`",
                        true,
                    ),
                    param("max_results", "integer", "Issues to return, at most 100", false),
                ],
            ),
            spec(
                "get_issue",
                "Reads one issue (GET /rest/api/3/issue/{issue}).",
                &[param("issue", "string", "Issue key or ID, for example PROJ-123", true)],
            ),
            spec(
                "add_comment",
                "Adds a plain text comment to an issue (POST /rest/api/3/issue/{issue}/comment).",
                &[
                    param("issue", "string", "Issue key or ID, for example PROJ-123", true),
                    param(
                        "text",
                        "string",
                        "The comment text. Each line becomes a paragraph.",
                        true,
                    ),
                ],
            ),
        ],
        ApiService::Confluence => vec![
            spec(
                "search_pages",
                "Searches content with CQL (GET /wiki/rest/api/search).",
                &[
                    param(
                        "cql",
                        "string",
                        "CQL query, for example `type = page and text ~ \"deploy\"`",
                        true,
                    ),
                    param("limit", "integer", "Results to return, at most 100", false),
                ],
            ),
            spec(
                "get_page",
                "Reads one page with its body in storage format (GET /wiki/api/v2/pages/{id}).",
                &[param("page_id", "string", "The numeric page ID", true)],
            ),
            spec(
                "create_page",
                "Creates a page from plain text (POST /wiki/api/v2/pages).",
                &[
                    param("space_id", "string", "The numeric space ID", true),
                    param("title", "string", "The page title", true),
                    param("text", "string", "The page text. Each line becomes a paragraph.", true),
                    param("parent_id", "string", "The numeric ID of the parent page", false),
                ],
            ),
        ],
    }
}

/// The arguments of one call, checked against the tool's schema.
struct Args<'a>(&'a Map<String, Value>);

impl<'a> Args<'a> {
    /// Rejects an argument that the schema does not name, and a missing required argument.
    fn new(spec: &ToolSpec, args: &'a Map<String, Value>) -> Result<Self, ConnectorError> {
        let schema = &spec.input_schema;
        let properties = schema["properties"].as_object().expect("spec() writes an object");
        if let Some(unknown) = args.keys().find(|key| !properties.contains_key(*key)) {
            return Err(invalid(format!("`{}` takes no argument `{unknown}`", spec.name)));
        }
        let required = schema["required"].as_array().expect("spec() writes an array");
        if let Some(missing) = required
            .iter()
            .filter_map(Value::as_str)
            .find(|name| !args.contains_key(*name))
        {
            return Err(invalid(format!("`{}` needs the argument `{missing}`", spec.name)));
        }
        Ok(Self(args))
    }

    fn optional_text(&self, name: &str) -> Result<Option<&'a str>, ConnectorError> {
        match self.0.get(name) {
            None => Ok(None),
            Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text)),
            Some(_) => Err(invalid(format!("`{name}` must be a text that is not empty"))),
        }
    }

    /// A required text. `Args::new` already checked that it is present.
    fn text(&self, name: &str) -> Result<&'a str, ConnectorError> {
        self.optional_text(name)?
            .ok_or_else(|| invalid(format!("`{name}` must be a text that is not empty")))
    }

    /// A required ID that goes into the URL path. Only letters, digits, `-` and `_` are allowed,
    /// so that the ID cannot change the path.
    fn path_id(&self, name: &str) -> Result<&'a str, ConnectorError> {
        let id = self.text(name)?;
        if id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            Ok(id)
        } else {
            Err(invalid(format!("`{name}` may hold only letters, digits, `-` and `_`")))
        }
    }

    fn count(&self, name: &str, max: u64) -> Result<Option<u64>, ConnectorError> {
        match self.0.get(name) {
            None => Ok(None),
            Some(value) => value
                .as_u64()
                .filter(|n| (1..=max).contains(n))
                .map(Some)
                .ok_or_else(|| invalid(format!("`{name}` must be a whole number from 1 to {max}"))),
        }
    }
}

fn invalid(message: String) -> ConnectorError {
    ConnectorError::Failed(format!("invalid arguments: {message}"))
}

/// Plain text as an Atlassian Document Format document. Each line becomes a paragraph.
/// ADF rejects an empty text node, so an empty line becomes an empty paragraph.
fn adf_document(text: &str) -> Value {
    let paragraphs: Vec<Value> = text
        .lines()
        .map(|line| match line {
            "" => json!({"type": "paragraph"}),
            line => json!({"type": "paragraph", "content": [{"type": "text", "text": line}]}),
        })
        .collect();
    json!({"type": "doc", "version": 1, "content": paragraphs})
}

/// Plain text as Confluence storage format. Each line becomes an escaped `<p>` element,
/// so the text cannot add markup or macros.
fn storage_paragraphs(text: &str) -> String {
    text.lines()
        .map(|line| {
            let mut escaped = String::with_capacity(line.len());
            for c in line.chars() {
                match c {
                    '&' => escaped.push_str("&amp;"),
                    '<' => escaped.push_str("&lt;"),
                    '>' => escaped.push_str("&gt;"),
                    '"' => escaped.push_str("&quot;"),
                    '\'' => escaped.push_str("&#39;"),
                    c => escaped.push(c),
                }
            }
            format!("<p>{escaped}</p>")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adf_keeps_lines_as_paragraphs_and_has_no_empty_text_node() {
        let doc = adf_document("first\n\nthird");
        let content = doc["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["content"][0]["text"], "first");
        assert_eq!(content[1], json!({"type": "paragraph"}));
        assert_eq!(content[2]["content"][0]["text"], "third");
        assert_eq!(doc["version"], 1);
    }

    #[test]
    fn storage_text_is_escaped() {
        assert_eq!(
            storage_paragraphs("a <ac:macro> & \"b\"\nc's"),
            "<p>a &lt;ac:macro&gt; &amp; &quot;b&quot;</p><p>c&#39;s</p>"
        );
    }

    #[test]
    fn every_tool_has_a_closed_object_schema_and_no_tool_deletes() {
        for service in [ApiService::Slack, ApiService::Jira, ApiService::Confluence] {
            let tools = tools(service);
            assert_eq!(tools.len(), 3);
            for tool in &tools {
                assert_eq!(tool.input_schema["type"], "object");
                assert_eq!(tool.input_schema["additionalProperties"], false);
                assert!(!tool.name.contains("delete"), "{}", tool.name);
                // Every required name is a property.
                for name in tool.input_schema["required"].as_array().unwrap() {
                    assert!(tool.input_schema["properties"].get(name.as_str().unwrap()).is_some());
                }
            }
        }
    }

    #[test]
    fn error_detail_is_cut_at_a_char_boundary() {
        let connector = ApiConnector::new(
            "jira",
            ApiService::Jira,
            "https://x.example",
            ApiAuth::Bearer("t".into()),
        )
        .unwrap();
        let long = "가".repeat(MAX_DETAIL_CHARS + 10);
        let detail = connector.error_detail(&json!({"errorMessages": [long]}));
        assert_eq!(detail.chars().count(), MAX_DETAIL_CHARS + 3);
        assert!(detail.ends_with("..."));
    }
}
