//! The built-in connector `bunsin.ard` with its one tool `search`: a search of the profile's ARD registries.
//!
//! The client follows no referral: a referral names a registry that the registry chose, not the owner.
//! Each search asks for `federation: none`. The engine treats the tool as a read and quotes its result.

use bunsin_ard::{Federation, Query, SearchClient, SearchRequest, SearchResult};
use bunsin_core::ard::{ARD_CONNECTOR, ARD_SEARCH, ArdRegistry, MAX_SEARCH_CHARS, SEARCH_PAGE_SIZE, search_spec};
use bunsin_core::ports::{BoxFuture, Connector, ConnectorError};
use bunsin_core::tool::ToolSpec;
use serde_json::{Map, Value, json};
use std::time::Duration;
use url::Url;

/// How long one registry may take to answer.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);

pub struct ArdSearch {
    client: SearchClient,
    registries: Vec<ArdRegistry>,
}

impl ArdSearch {
    /// Makes the search for registries whose addresses the caller checked.
    pub fn new(registries: &[ArdRegistry]) -> Result<Self, String> {
        let urls = registries
            .iter()
            .map(|registry| Url::parse(&registry.url).map_err(|e| format!("ARD registry `{}`: {e}", registry.name)))
            .collect::<Result<Vec<_>, _>>()?;
        // Hop limit 0: no referral is followed.
        let client = SearchClient::new(&urls, 0, SEARCH_TIMEOUT).map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            registries: registries.to_vec(),
        })
    }

    async fn search(&self, args: &Map<String, Value>) -> Result<Value, ConnectorError> {
        let invalid = |message: String| ConnectorError::Failed(format!("invalid arguments: {message}"));
        if let Some(unknown) = args.keys().find(|key| !["text", "registry"].contains(&key.as_str())) {
            return Err(invalid(format!("`search` takes no argument `{unknown}`")));
        }
        let text = args
            .get("text")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= MAX_SEARCH_CHARS)
            .ok_or_else(|| invalid(format!("`text` must have 1 to {MAX_SEARCH_CHARS} characters")))?;
        let registry = match args.get("registry") {
            None => self.registries.first(),
            Some(Value::String(name)) => self.registries.iter().find(|r| r.name == *name),
            Some(_) => None,
        }
        .ok_or_else(|| invalid("`registry` must name a registry of the profile".into()))?;
        let request = SearchRequest {
            query: Query {
                text: text.to_owned(),
                ..Query::default()
            },
            federation: Some(Federation::None),
            page_size: Some(SEARCH_PAGE_SIZE),
            page_token: None,
        };
        let url = Url::parse(&registry.url).map_err(|e| ConnectorError::Failed(e.to_string()))?;
        let found = self
            .client
            .search(&url, &request)
            .await
            .map_err(|e| ConnectorError::Failed(e.to_string()))?;
        // With hop limit 0 the first page is the only one.
        let results: Vec<Value> = found
            .pages
            .first()
            .map(|page| page.response.results.iter().map(result).collect())
            .unwrap_or_default();
        Ok(json!({"registry": registry.name, "results": results}))
    }
}

/// The terms of a result that the agent sees. Other terms are left out.
fn result(found: &SearchResult) -> Value {
    json!({
        "identifier": found.identifier,
        "displayName": found.display_name,
        "type": found.media_type,
        "url": found.url,
        "description": found.description,
        "capabilities": found.capabilities,
        "score": found.score,
        "source": found.source,
    })
}

impl Connector for ArdSearch {
    fn name(&self) -> &str {
        ARD_CONNECTOR
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>> {
        let spec = search_spec(&self.registries);
        Box::pin(async move { Ok(vec![spec]) })
    }

    fn call_tool(&self, tool: &str, args: Map<String, Value>) -> BoxFuture<'_, Result<Value, ConnectorError>> {
        let tool = tool.to_owned();
        Box::pin(async move {
            match tool.as_str() {
                ARD_SEARCH => self.search(&args).await,
                other => Err(ConnectorError::UnknownTool(other.to_owned())),
            }
        })
    }
}
