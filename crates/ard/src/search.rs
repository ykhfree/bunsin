//! The Consumer client of the registry API: Search (§5.3.2, §5.4), Explore (§5.3.3),
//! List (§5.3.4, Appendix A) and the error codes (Appendix B).
//!
//! Search results are data written by others. This module returns them as they are.
//! It does not act on their text, and no policy check may take `score` as input (§4.5.2, §5.3.2).

use crate::resolve::{FetchError, Fetched};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;
use url::Url;

/// The largest response body that the client reads, in bytes.
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;

/// The longest registry error message that the client keeps, in bytes.
pub const MAX_ERROR_MESSAGE_BYTES: usize = 2048;

/// The media types of a registry referral (schema `RegistryReferral.type`, §3.3).
pub const REGISTRY_TYPES: [&str; 2] = ["application/ai-registry+json", "application/ai-registry"];

/// The body of `POST /search` (§5.3.2).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRequest {
    pub query: Query,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub federation: Option<Federation>,
    /// The registry's default is 10 and its maximum is 100.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_size: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
}

/// The query object (§5.3.1). Search requires `text` (§5.3.2). Explore does not, so an
/// empty `text` is left out of the body.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Query {
    #[serde(rename = "@context", skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// Keys are term paths. Values are arrays or single values (§5.3.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<Map<String, Value>>,
}

/// How a registry federates a search (§5.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Federation {
    Auto,
    Referrals,
    None,
}

/// The body of a successful `POST /search` answer (§5.3.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
    #[serde(default)]
    pub referrals: Vec<Referral>,
    #[serde(default)]
    pub page_token: Option<String>,
}

/// One search result. Only `identifier` is required; the registry chooses the other terms (§5.3.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    pub identifier: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(rename = "type", default)]
    pub media_type: Option<String>,
    /// The address of the artifact, not of the ARD entry (§5.3.2).
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Option<Vec<String>>,
    /// Semantic relevance from 0 to 100. It is not a trust, compliance or safety judgment.
    /// No policy check may take it as input (§4.5.2, §5.3.2).
    #[serde(default)]
    pub score: Option<f64>,
    /// The registry that indexed the entry.
    #[serde(default)]
    pub source: Option<String>,
    /// Every other term the registry returned.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// A registry that a registry refers the client to (§5.3.2, schema `RegistryReferral`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Referral {
    pub identifier: String,
    pub display_name: String,
    #[serde(rename = "type")]
    pub media_type: String,
    /// The complete search endpoint. A client MUST NOT append `/search` to it (§5.3).
    pub url: String,
}

/// The body of `POST /explore` (§5.3.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<Query>,
    pub result_type: ResultType,
}

/// The shape of an Explore result. Facets are the only shape that §5.3.3 defines.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResultType {
    pub facets: Vec<FacetRequest>,
}

/// One facet to compute. The registry's defaults apply to `limit` and `minCount`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FacetRequest {
    /// A term path, with the syntax of a filter key (§5.3.1).
    pub field: String,
}

/// The body of a successful `POST /explore` answer (§5.3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExploreResponse {
    pub result_type: String,
    pub facets: BTreeMap<String, FacetResult>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FacetResult {
    pub buckets: Vec<Bucket>,
    /// The number of matching entries in the buckets past the limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub other_count: Option<u64>,
}

/// One facet value. A registry MAY leave out `count` (§5.3.3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Bucket {
    pub value: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
}

/// The query parameters of `GET /agents` (§5.3.4).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListRequest {
    /// An Appendix A expression, such as the one [`ListFilter::expression`] makes.
    pub filter: Option<String>,
    pub order_by: Option<String>,
    pub page_size: Option<u32>,
    pub page_token: Option<String>,
}

/// The constraints of a List filter (Appendix A). Across fields: AND. Within a field: OR.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListFilter {
    pub types: Vec<String>,
    pub publishers: Vec<String>,
    pub display_name: Option<String>,
    pub created_after: Option<String>,
    pub updated_after: Option<String>,
}

impl ListFilter {
    /// Makes the expression, such as `type = 'a,b' AND publisherId = 'acme.com'`.
    ///
    /// The syntax is the part that Appendix A and the OpenAPI example share: `=` or `>`,
    /// a quoted value, commas for OR, and ` AND `. A value with `'` or `,` cannot be quoted,
    /// so it is refused. Returns `None` if there is no constraint.
    pub fn expression(&self) -> Result<Option<String>, String> {
        let mut clauses = Vec::new();
        let mut add = |field: &str, operator: &str, values: &[String]| -> Result<(), String> {
            if values.is_empty() {
                return Ok(());
            }
            if let Some(bad) = values.iter().find(|v| v.is_empty() || v.contains(['\'', ','])) {
                return Err(format!("the `{field}` value `{bad}` is empty or has `'` or `,`"));
            }
            clauses.push(format!("{field} {operator} '{}'", values.join(",")));
            Ok(())
        };
        add("type", "=", &self.types)?;
        add("publisherId", "=", &self.publishers)?;
        add("displayName", "=", self.display_name.as_slice())?;
        add("createdAfter", ">", self.created_after.as_slice())?;
        add("updatedAfter", ">", self.updated_after.as_slice())?;
        Ok((!clauses.is_empty()).then(|| clauses.join(" AND ")))
    }
}

/// The body of a successful `GET /agents` answer (§5.3.4, schema `ListResponse`).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListResponse {
    /// Entries with the terms that the registry chose. They have no `score` or `source`.
    pub items: Vec<SearchResult>,
    #[serde(default)]
    pub total: Option<u64>,
    #[serde(default)]
    pub page_token: Option<String>,
}

/// The error codes of Appendix B.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ErrorCode {
    InvalidArgument,
    Unauthenticated,
    NotFound,
    RateLimitExceeded,
    InternalError,
    /// A code that Appendix B does not list.
    Other(String),
}

impl ErrorCode {
    fn from_code(code: &str) -> Self {
        match code {
            "INVALID_ARGUMENT" => ErrorCode::InvalidArgument,
            "UNAUTHENTICATED" => ErrorCode::Unauthenticated,
            "NOT_FOUND" => ErrorCode::NotFound,
            "RATE_LIMIT_EXCEEDED" => ErrorCode::RateLimitExceeded,
            "INTERNAL_ERROR" => ErrorCode::InternalError,
            other => ErrorCode::Other(other.to_owned()),
        }
    }

    /// The code that Appendix B pairs with an HTTP status, if any.
    fn from_status(status: u16) -> Option<Self> {
        match status {
            400 => Some(ErrorCode::InvalidArgument),
            401 => Some(ErrorCode::Unauthenticated),
            404 => Some(ErrorCode::NotFound),
            429 => Some(ErrorCode::RateLimitExceeded),
            500 => Some(ErrorCode::InternalError),
            _ => None,
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ErrorCode::InvalidArgument => "INVALID_ARGUMENT",
            ErrorCode::Unauthenticated => "UNAUTHENTICATED",
            ErrorCode::NotFound => "NOT_FOUND",
            ErrorCode::RateLimitExceeded => "RATE_LIMIT_EXCEEDED",
            ErrorCode::InternalError => "INTERNAL_ERROR",
            ErrorCode::Other(code) => code,
        })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The registry is not in the list that the client was made with.
    #[error("the registry `{0}` is not in the profile's registry list")]
    NotAllowed(String),
    #[error("the registry answered with redirect status {0}; the client does not follow redirects")]
    Redirected(u16),
    /// A non-success answer. `code` comes from the body's `errorCode`, or else from the status.
    /// `message` is registry text: it is data, not instruction.
    #[error("the registry answered status {status}{}: {message}", code.as_ref().map(|c| format!(" ({c})")).unwrap_or_default())]
    Registry {
        status: u16,
        code: Option<ErrorCode>,
        message: String,
    },
    #[error("cannot reach the registry: {0}")]
    Transport(String),
    #[error("the registry answer is not a valid search response: {0}")]
    InvalidResponse(String),
}

/// The answer of one registry in a federated search.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// The search endpoint that the client called.
    pub endpoint: Url,
    /// 0 for the first registry, 1 for a registry that it refers to, and so on.
    pub hop: u32,
    pub response: SearchResponse,
}

/// Why the client did not follow a referral.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeclineReason {
    /// The referral's `type` is not a registry type.
    NotARegistry,
    /// The referral's `url` is not the search endpoint of a registry in the client's list.
    NotAllowed,
    /// Following the referral would pass the client's hop limit.
    HopLimit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Declined {
    pub referral: Referral,
    pub reason: DeclineReason,
}

/// A referred registry that the client called without success.
#[derive(Debug)]
pub struct Failed {
    pub endpoint: Url,
    pub error: SearchError,
}

/// The outcome of a search and of the referrals that the client followed.
#[derive(Debug, Default)]
pub struct Federated {
    /// The first page is the first registry's answer. The others follow in breadth-first order.
    pub pages: Vec<Page>,
    pub declined: Vec<Declined>,
    pub failed: Vec<Failed>,
}

/// Returns the search endpoint of a registry base URL: the base with `/search` appended (§5.3).
/// Use it only for a base URL, never for a referral `url`.
pub fn search_endpoint(base: &Url) -> Url {
    endpoint(base, "search")
}

/// The base URL with one path segment appended.
fn endpoint(base: &Url, segment: &str) -> Url {
    let mut endpoint = base.clone();
    endpoint.set_path(&format!("{}/{segment}", base.path().trim_end_matches('/')));
    endpoint
}

/// A client for the registries of one profile.
#[derive(Debug, Clone)]
pub struct SearchClient {
    http: reqwest::Client,
    /// The base URLs of the allowed registries.
    registries: Vec<Url>,
    max_hops: u32,
}

impl SearchClient {
    /// Makes a client that calls only the registries with the base URLs in `registries`.
    /// `max_hops` limits the referral chain; 0 follows no referral.
    pub fn new(registries: &[Url], max_hops: u32, timeout: Duration) -> Result<Self, SearchError> {
        // A redirect could take the request to a host that is not in the registry list.
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| SearchError::Transport(error_chain(&e)))?;
        Ok(Self {
            http,
            registries: registries.to_vec(),
            max_hops,
        })
    }

    /// The endpoint `segment` of `registry`, if the registry is in the list.
    fn allowed(&self, registry: &Url, segment: &str) -> Result<Url, SearchError> {
        let target = endpoint(registry, segment);
        if self.registries.iter().any(|base| endpoint(base, segment) == target) {
            Ok(target)
        } else {
            Err(SearchError::NotAllowed(registry.to_string()))
        }
    }

    /// Sends `request` to `POST <registry>/explore`. A registry without Explore answers 501.
    pub async fn explore(&self, registry: &Url, request: &ExploreRequest) -> Result<ExploreResponse, SearchError> {
        let endpoint = self.allowed(registry, "explore")?;
        self.send(self.post(&endpoint, request)).await
    }

    /// Sends `GET <registry>/agents` with the parameters of `request`.
    pub async fn list(&self, registry: &Url, request: &ListRequest) -> Result<ListResponse, SearchError> {
        let mut endpoint = self.allowed(registry, "agents")?;
        let page_size = request.page_size.map(|n| n.to_string());
        let pairs = [
            ("filter", &request.filter),
            ("orderBy", &request.order_by),
            ("pageSize", &page_size),
            ("pageToken", &request.page_token),
        ];
        for (name, value) in pairs {
            if let Some(value) = value {
                endpoint.query_pairs_mut().append_pair(name, value);
            }
        }
        let request = self
            .http
            .get(endpoint)
            .header(reqwest::header::ACCEPT, "application/json");
        self.send(request).await
    }

    /// Sends `request` to `POST <registry>/search`, then follows the referrals in the answers.
    ///
    /// The client follows a referral only to its exact `url` (§5.3), only if that `url` is the
    /// search endpoint of an allowed registry, and only within the hop limit. It calls each
    /// endpoint once. A referred registry gets the request without `pageToken`, because a page
    /// token belongs to the registry that issued it.
    ///
    /// Fails if `registry` is not allowed or the first registry fails. A failed referred
    /// registry goes into [`Federated::failed`].
    pub async fn search(&self, registry: &Url, request: &SearchRequest) -> Result<Federated, SearchError> {
        let first = self.allowed(registry, "search")?;
        let response = self.send(self.post(&first, request)).await?;
        let referred_request = SearchRequest {
            page_token: None,
            ..request.clone()
        };
        let mut visited = vec![first.clone()];
        let mut out = Federated {
            pages: vec![Page {
                endpoint: first,
                hop: 0,
                response,
            }],
            ..Federated::default()
        };
        let mut next = 0;
        while next < out.pages.len() {
            let hop = out.pages[next].hop + 1;
            let referrals = out.pages[next].response.referrals.clone();
            next += 1;
            for referral in referrals {
                let endpoint = match self.check(&referral, hop, &visited) {
                    Ok(Some(endpoint)) => endpoint,
                    Ok(None) => continue,
                    Err(reason) => {
                        out.declined.push(Declined { referral, reason });
                        continue;
                    }
                };
                visited.push(endpoint.clone());
                match self.send(self.post(&endpoint, &referred_request)).await {
                    Ok(response) => out.pages.push(Page {
                        endpoint,
                        hop,
                        response,
                    }),
                    Err(error) => out.failed.push(Failed { endpoint, error }),
                }
            }
        }
        Ok(out)
    }

    /// Returns the endpoint to call for `referral` at `hop`, `None` if the client called it
    /// already, or the reason to decline it.
    fn check(&self, referral: &Referral, hop: u32, visited: &[Url]) -> Result<Option<Url>, DeclineReason> {
        if !REGISTRY_TYPES.contains(&referral.media_type.as_str()) {
            return Err(DeclineReason::NotARegistry);
        }
        // The referral url is already the complete search endpoint. It is used as it is.
        let endpoint = Url::parse(&referral.url)
            .ok()
            .filter(|url| self.registries.iter().any(|base| search_endpoint(base) == *url))
            .ok_or(DeclineReason::NotAllowed)?;
        if visited.contains(&endpoint) {
            return Ok(None);
        }
        if hop > self.max_hops {
            return Err(DeclineReason::HopLimit);
        }
        Ok(Some(endpoint))
    }

    fn post(&self, endpoint: &Url, body: &impl Serialize) -> reqwest::RequestBuilder {
        let body = serde_json::to_string(body).expect("a request body always serializes");
        self.http
            .post(endpoint.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::ACCEPT, "application/json")
            .body(body)
    }

    /// Sends a request and reads a JSON answer of type `R`, or the registry's error.
    async fn send<R: DeserializeOwned>(&self, request: reqwest::RequestBuilder) -> Result<R, SearchError> {
        let response = request
            .send()
            .await
            .map_err(|e| SearchError::Transport(error_chain(&e)))?;
        let status = response.status();
        if status.is_redirection() {
            return Err(SearchError::Redirected(status.as_u16()));
        }
        let body = read_body(response).await.map_err(|error| match error {
            FetchError::Transport(text) => SearchError::Transport(text),
            other => SearchError::InvalidResponse(other.to_string()),
        })?;
        if !status.is_success() {
            return Err(registry_error(status.as_u16(), &body));
        }
        serde_json::from_slice(&body).map_err(|e| SearchError::InvalidResponse(e.to_string()))
    }
}

/// Sends `GET url` with `client` and reads the answer as an entry source (§5.1).
///
/// The client sets the address and redirect rules: a redirect it refuses is
/// [`FetchError::Refused`]. A status other than success is an error. The body limit is
/// [`MAX_RESPONSE_BYTES`].
pub async fn get_document(client: &reqwest::Client, url: &Url) -> Result<Fetched, FetchError> {
    let response = client
        .get(url.clone())
        .header(reqwest::header::ACCEPT, "application/json, text/html;q=0.9, */*;q=0.1")
        .send()
        .await
        .map_err(|e| match e.is_redirect() {
            true => FetchError::Refused(error_chain(&e)),
            false => FetchError::Transport(error_chain(&e)),
        })?;
    let status = response.status();
    if !status.is_success() {
        return Err(FetchError::Status(status.as_u16()));
    }
    let url = response.url().clone();
    let links = response
        .headers()
        .get_all(reqwest::header::LINK)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(str::to_owned)
        .collect();
    let body = read_body(response).await?;
    Ok(Fetched { url, links, body })
}

/// Reads the body, and fails if it is larger than [`MAX_RESPONSE_BYTES`].
async fn read_body(mut response: reqwest::Response) -> Result<Vec<u8>, FetchError> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| FetchError::Transport(error_chain(&e)))?
    {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(FetchError::TooLarge(MAX_RESPONSE_BYTES));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Reads an error body `{"errorCode", "message"}` (schema `Error`, Appendix B).
fn registry_error(status: u16, body: &[u8]) -> SearchError {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase")]
    struct ErrorBody {
        error_code: Option<String>,
        message: Option<String>,
    }
    let parsed: ErrorBody = serde_json::from_slice(body).unwrap_or_default();
    let code = parsed
        .error_code
        .map(|c| ErrorCode::from_code(&c))
        .or_else(|| ErrorCode::from_status(status));
    let mut message = parsed.message.unwrap_or_default();
    if message.len() > MAX_ERROR_MESSAGE_BYTES {
        let mut end = MAX_ERROR_MESSAGE_BYTES;
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        message.truncate(end);
    }
    SearchError::Registry { status, code, message }
}

fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_spec_search_response_parses() {
        let text = include_str!("../tests/fixtures/spec_5_3_2_search_response.json");
        let response: SearchResponse = serde_json::from_str(text).unwrap();
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].identifier, "urn:air:acme.com:agent:assistant");
        assert_eq!(response.results[0].score, Some(95.0));
        assert_eq!(response.results[1].capabilities, Some(vec!["WeatherTool".to_owned()]));
        assert_eq!(response.referrals[0].url, "https://finder.nlweb.ai/search");
        assert_eq!(response.page_token.as_deref(), Some("eyJwYWdlIjogMn0="));
    }

    #[test]
    fn a_result_needs_only_an_identifier_and_keeps_other_terms() {
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [{"identifier": "urn:air:acme.com:x", "acme:tier": "gold"}]
        }))
        .unwrap();
        assert_eq!(response.referrals, vec![]);
        assert_eq!(response.results[0].other["acme:tier"], json!("gold"));
        assert_eq!(response.results[0].score, None);
    }

    #[test]
    fn the_request_matches_the_spec_example() {
        let request = SearchRequest {
            query: Query {
                text: "find me a flight booking agent".into(),
                filter: Some(Map::from_iter([(
                    "type".to_owned(),
                    json!(["application/a2a-agent-card+json"]),
                )])),
                ..Query::default()
            },
            federation: Some(Federation::Referrals),
            page_size: Some(5),
            page_token: None,
        };
        // The request example of §5.3.2.
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "query": {
                    "text": "find me a flight booking agent",
                    "filter": {"type": ["application/a2a-agent-card+json"]}
                },
                "federation": "referrals",
                "pageSize": 5
            })
        );
    }

    #[test]
    fn the_list_filter_joins_fields_with_and_and_values_with_commas() {
        let filter = ListFilter {
            types: vec![
                "application/mcp-server-card+json".into(),
                "application/a2a-agent-card+json".into(),
            ],
            publishers: vec!["acme.com".into()],
            display_name: Some("Expense Bot".into()),
            created_after: Some("2026-01-01T00:00:00Z".into()),
            updated_after: None,
        };
        assert_eq!(
            filter.expression().unwrap().unwrap(),
            "type = 'application/mcp-server-card+json,application/a2a-agent-card+json' \
             AND publisherId = 'acme.com' AND displayName = 'Expense Bot' AND createdAfter > '2026-01-01T00:00:00Z'"
        );
        assert_eq!(ListFilter::default().expression(), Ok(None));
        // A quote would end the value early, and a comma would add an OR value.
        for bad in [
            ListFilter {
                display_name: Some("x' OR type = 'y".into()),
                ..ListFilter::default()
            },
            ListFilter {
                publishers: vec!["acme.com,evil.example".into()],
                ..ListFilter::default()
            },
            ListFilter {
                types: vec![String::new()],
                ..ListFilter::default()
            },
        ] {
            assert!(bad.expression().is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_explore_request_leaves_out_an_empty_text_and_query() {
        let facets = ResultType {
            facets: vec![FacetRequest { field: "type".into() }],
        };
        let without = ExploreRequest {
            query: None,
            result_type: facets.clone(),
        };
        assert_eq!(
            serde_json::to_value(&without).unwrap(),
            json!({"resultType": {"facets": [{"field": "type"}]}})
        );
        let with = ExploreRequest {
            query: Some(Query {
                filter: Some(Map::from_iter([("tags".to_owned(), json!(["finance"]))])),
                ..Query::default()
            }),
            result_type: facets,
        };
        assert_eq!(
            serde_json::to_value(&with).unwrap(),
            json!({"query": {"filter": {"tags": ["finance"]}}, "resultType": {"facets": [{"field": "type"}]}})
        );
        // The answer example of §5.3.3 parses. A bucket without a count is accepted.
        let answer: ExploreResponse = serde_json::from_value(json!({
            "resultType": "facets",
            "facets": {
                "type": {"buckets": [{"value": "application/mcp-server-card+json", "count": 1247}], "otherCount": 23},
                "publisher": {"buckets": [{"value": "acme.com"}]}
            }
        }))
        .unwrap();
        assert_eq!(answer.facets["type"].other_count, Some(23));
        assert_eq!(answer.facets["publisher"].buckets[0].count, None);
    }

    #[test]
    fn the_search_endpoint_appends_one_path_segment() {
        for (base, endpoint) in [
            ("https://registry.acme.com", "https://registry.acme.com/search"),
            (
                "https://registry.acme.com/api/v1/",
                "https://registry.acme.com/api/v1/search",
            ),
            (
                "https://registry.acme.com/api/v1",
                "https://registry.acme.com/api/v1/search",
            ),
        ] {
            assert_eq!(search_endpoint(&Url::parse(base).unwrap()).as_str(), endpoint);
        }
    }

    #[test]
    fn an_error_body_gives_the_code_and_a_missing_body_gives_the_status_code() {
        let error = registry_error(400, br#"{"errorCode":"INVALID_ARGUMENT","message":"bad filter"}"#);
        assert!(matches!(
            &error,
            SearchError::Registry { status: 400, code: Some(ErrorCode::InvalidArgument), message } if message == "bad filter"
        ));
        assert_eq!(
            error.to_string(),
            "the registry answered status 400 (INVALID_ARGUMENT): bad filter"
        );
        assert!(matches!(
            registry_error(429, b""),
            SearchError::Registry {
                code: Some(ErrorCode::RateLimitExceeded),
                ..
            }
        ));
        assert!(matches!(
            registry_error(503, b"down"),
            SearchError::Registry { code: None, .. }
        ));
        assert!(matches!(
            registry_error(500, br#"{"errorCode":"QUOTA","message":"x"}"#),
            SearchError::Registry { code: Some(ErrorCode::Other(c)), .. } if c == "QUOTA"
        ));
    }

    #[test]
    fn a_long_error_message_is_cut_on_a_character_boundary() {
        let long = "가".repeat(MAX_ERROR_MESSAGE_BYTES);
        let body = json!({"errorCode": "INTERNAL_ERROR", "message": long}).to_string();
        let SearchError::Registry { message, .. } = registry_error(500, body.as_bytes()) else {
            panic!("not a registry error");
        };
        assert!(message.len() <= MAX_ERROR_MESSAGE_BYTES && message.len() > MAX_ERROR_MESSAGE_BYTES - 3);
        assert!(message.chars().all(|c| c == '가'));
    }
}
