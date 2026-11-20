//! The built-in connector `bunsin.ard`: `search`, `explore` and `list` ask the profile's ARD
//! registries, and `resolve` reads the entries of a domain of the profile's list.
//!
//! The client follows no referral: a referral names a registry that the registry chose, not the owner.
//! Each search asks for `federation: none`. `resolve` takes only a domain of the owner's list, because
//! a read that fetches an address the owner does not control can carry read text out (D5).
//! The engine treats each tool as a read and quotes its result.

use crate::UpstreamPolicy;
use crate::fetch::Fetcher;
use bunsin_ard::{
    ARD_NAMESPACE, Accepted, Context, ExploreRequest, FacetRequest, Federation, ListFilter, ListRequest, Query,
    Resolution, ResultType, SearchClient, SearchError, SearchRequest, SearchResult, Seed,
};
use bunsin_core::ard::{
    ARD_CONNECTOR, ARD_EXPLORE, ARD_LIST, ARD_RESOLVE, ARD_SEARCH, ArdDomain, ArdRegistry, LIST_ORDERS,
    MAX_CONTEXT_PREFIXES, MAX_FACETS, MAX_FILTER_CHARS, MAX_FILTER_KEYS, MAX_FILTER_VALUES, MAX_PAGE_TOKEN_CHARS,
    MAX_SEARCH_CHARS, SEARCH_PAGE_SIZE, tool_specs,
};
use bunsin_core::ports::{BoxFuture, Connector, ConnectorError};
use bunsin_core::tool::ToolSpec;
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::time::Duration;
use url::Url;

/// How long one registry may take to answer.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(20);
/// The most entries, and the most refused entries, that one `resolve` shows.
const MAX_SHOWN_ENTRIES: usize = 20;
/// The most terms of other namespaces that one entry shows.
const MAX_SHOWN_TERMS: usize = 20;
/// The longest value that `resolve` shows, in characters of JSON.
const MAX_SHOWN_CHARS: usize = 500;
/// The core terms that `resolve` shows, with the local name of each IRI in the ARD namespace.
const SHOWN_CORE_TERMS: [(&str, &str); 10] = [
    ("identifier", "identifier"),
    ("displayName", "displayName"),
    ("type", "mediaType"),
    ("url", "url"),
    ("description", "description"),
    ("capabilities", "capabilities"),
    ("tags", "tags"),
    ("version", "version"),
    ("updatedAt", "updatedAt"),
    ("representativeQueries", "representativeQueries"),
];

pub struct ArdTools {
    client: SearchClient,
    registries: Vec<ArdRegistry>,
    domains: Vec<ArdDomain>,
    fetcher: Fetcher,
}

fn invalid(message: impl std::fmt::Display) -> ConnectorError {
    ConnectorError::Failed(format!("invalid arguments: {message}"))
}

/// Refuses an argument that `tool` does not take.
fn only(args: &Map<String, Value>, tool: &str, names: &[&str]) -> Result<(), ConnectorError> {
    match args.keys().find(|key| !names.contains(&key.as_str())) {
        Some(unknown) => Err(invalid(format!("`{tool}` takes no argument `{unknown}`"))),
        None => Ok(()),
    }
}

/// A string of 1 to `max` characters, after trimming.
fn text<'a>(args: &'a Map<String, Value>, name: &str, max: usize) -> Result<Option<&'a str>, ConnectorError> {
    match args.get(name) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty() && text.chars().count() <= max)
            .map(Some)
            .ok_or_else(|| invalid(format!("`{name}` must have 1 to {max} characters"))),
    }
}

/// A list of 1 to `max_items` strings of 1 to `max_chars` characters. A single string counts as a list.
fn strings(value: &Value, what: &str, max_items: usize, max_chars: usize) -> Result<Vec<String>, ConnectorError> {
    let error = || {
        invalid(format!(
            "{what} must be 1 to {max_items} strings of 1 to {max_chars} characters"
        ))
    };
    let items = match value {
        Value::Array(items) => items.iter().collect(),
        single => vec![single],
    };
    if items.is_empty() || items.len() > max_items {
        return Err(error());
    }
    items
        .into_iter()
        .map(|item| {
            item.as_str()
                .filter(|item| !item.is_empty() && item.chars().count() <= max_chars)
                .map(str::to_owned)
                .ok_or_else(error)
        })
        .collect()
}

/// The `filter` argument: term path to values (§5.3.1), with the values always as an array.
fn filter(args: &Map<String, Value>) -> Result<Option<Map<String, Value>>, ConnectorError> {
    let Some(value) = args.get("filter") else {
        return Ok(None);
    };
    let keys = value
        .as_object()
        .filter(|keys| (1..=MAX_FILTER_KEYS).contains(&keys.len()))
        .ok_or_else(|| invalid(format!("`filter` must be an object with 1 to {MAX_FILTER_KEYS} keys")))?;
    let mut checked = Map::new();
    for (key, values) in keys {
        if key.is_empty() || key.chars().count() > MAX_FILTER_CHARS {
            return Err(invalid(format!(
                "a `filter` key must have 1 to {MAX_FILTER_CHARS} characters"
            )));
        }
        let values = strings(
            values,
            &format!("the values of `filter.{key}`"),
            MAX_FILTER_VALUES,
            MAX_FILTER_CHARS,
        )?;
        checked.insert(key.clone(), json!(values));
    }
    Ok(Some(checked))
}

/// The `context` argument: prefix to IRI. The JSON-LD rules of a published entry apply.
fn context(args: &Map<String, Value>) -> Result<Option<Value>, ConnectorError> {
    let Some(value) = args.get("context") else {
        return Ok(None);
    };
    let prefixes = value
        .as_object()
        .filter(|p| (1..=MAX_CONTEXT_PREFIXES).contains(&p.len()))
        .ok_or_else(|| {
            invalid(format!(
                "`context` must be an object with 1 to {MAX_CONTEXT_PREFIXES} prefixes"
            ))
        })?;
    let strings_only = prefixes
        .values()
        .all(|iri| iri.as_str().is_some_and(|iri| iri.chars().count() <= MAX_FILTER_CHARS));
    if !strings_only {
        return Err(invalid(format!(
            "each `context` value must be an IRI of at most {MAX_FILTER_CHARS} characters"
        )));
    }
    Context::base().with(Some(value)).map_err(invalid)?;
    Ok(Some(value.clone()))
}

/// The terms of a result that the agent sees. Other terms are left out.
fn result(found: &SearchResult, ranked: bool) -> Value {
    let mut shown = json!({
        "identifier": found.identifier,
        "displayName": found.display_name,
        "type": found.media_type,
        "url": found.url,
        "description": found.description,
        "capabilities": found.capabilities,
    });
    if ranked {
        shown["score"] = json!(found.score);
        shown["source"] = json!(found.source);
    }
    shown
}

fn registry_error(error: SearchError) -> ConnectorError {
    match error {
        SearchError::Registry { status: 501, .. } => {
            ConnectorError::Failed("the registry does not offer this request (status 501)".into())
        }
        other => ConnectorError::Failed(other.to_string()),
    }
}

/// Cuts a text to `max` characters, and says so.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}[cut]", &text[..end]),
        None => text.to_owned(),
    }
}

/// A value as it is, or as cut JSON text if it is longer than [`MAX_SHOWN_CHARS`].
fn shown_value(value: &Value) -> Value {
    let text = value.to_string();
    match text.chars().count() > MAX_SHOWN_CHARS {
        true => Value::String(cut(&text, MAX_SHOWN_CHARS)),
        false => value.clone(),
    }
}

/// What the agent sees of an accepted entry: the core terms by their names, the publisher, the trust
/// identity marked as not verified, and terms of other namespaces by their IRI. An IRI key keeps a
/// publisher's prefix from passing as a core term.
fn shown_entry(accepted: &Accepted) -> Value {
    let mut shown = Map::new();
    for (name, local) in SHOWN_CORE_TERMS {
        if let Some(value) = accepted.terms.get(&format!("{ARD_NAMESPACE}{local}")) {
            shown.insert(name.to_owned(), shown_value(value));
        }
    }
    shown.insert("publisher".into(), json!(accepted.publisher));
    let identity = accepted
        .terms
        .get(&format!("{ARD_NAMESPACE}trustManifest"))
        .and_then(|manifest| manifest.get("identity"))
        .and_then(Value::as_str);
    if let Some(identity) = identity {
        shown.insert(
            "trustIdentity".into(),
            json!(format!("{} (not verified)", cut(identity, 200))),
        );
    }
    let others = accepted.terms.iter().filter(|(iri, _)| !iri.starts_with(ARD_NAMESPACE));
    for (iri, value) in others.take(MAX_SHOWN_TERMS) {
        shown.insert(cut(iri, MAX_FILTER_CHARS), shown_value(value));
    }
    Value::Object(shown)
}

/// The result of `resolve`. An identifier is shown once, from the first source that gave it.
/// If the root page or `robots.txt` could not be read, the sources are not all known, so the
/// resolution fails as a crawl does.
fn resolved(resolution: &Resolution) -> Result<Value, ConnectorError> {
    if let Some(error) = &resolution.discovery_error {
        return Err(ConnectorError::Failed(format!(
            "the resolution of `{}` is incomplete: {error}",
            resolution.domain
        )));
    }
    let mut seen = BTreeSet::new();
    let (mut entries, mut refused, mut not_read) = (Vec::new(), Vec::new(), Vec::new());
    let (mut entries_left_out, mut refused_left_out) = (0, 0);
    for source in &resolution.sources {
        let found = match &source.result {
            Ok(found) => found,
            Err(error) => {
                not_read.push(json!({"url": cut(source.url.as_str(), MAX_FILTER_CHARS), "error": error.to_string()}));
                continue;
            }
        };
        for accepted in &found.accepted {
            if !seen.insert(accepted.entry.identifier.to_ascii_lowercase()) {
                continue;
            }
            match entries.len() < MAX_SHOWN_ENTRIES {
                true => entries.push(shown_entry(accepted)),
                false => entries_left_out += 1,
            }
        }
        for rejected in &found.rejected {
            match refused.len() < MAX_SHOWN_ENTRIES {
                true => refused.push(json!({
                    "identifier": rejected.identifier.as_deref().map(|id| cut(id, MAX_FILTER_CHARS)),
                    "reason": cut(&rejected.reason.to_string(), MAX_SHOWN_CHARS),
                })),
                false => refused_left_out += 1,
            }
        }
    }
    let mut out = json!({
        "domain": resolution.domain,
        "entries": entries,
        "refused": refused,
        "sources_not_read": not_read,
    });
    if entries_left_out > 0 {
        out["entries_left_out"] = json!(entries_left_out);
    }
    if refused_left_out > 0 {
        out["refused_left_out"] = json!(refused_left_out);
    }
    Ok(out)
}

impl ArdTools {
    /// Makes the tools for registries and domains whose addresses the caller checked.
    pub fn new(registries: &[ArdRegistry], domains: &[ArdDomain], upstream: &UpstreamPolicy) -> Result<Self, String> {
        let urls = registries
            .iter()
            .map(|registry| Url::parse(&registry.url).map_err(|e| format!("ARD registry `{}`: {e}", registry.name)))
            .collect::<Result<Vec<_>, _>>()?;
        // Hop limit 0: no referral is followed.
        let client = SearchClient::new(&urls, 0, SEARCH_TIMEOUT).map_err(|e| e.to_string())?;
        Ok(Self {
            client,
            registries: registries.to_vec(),
            domains: domains.to_vec(),
            fetcher: Fetcher::new(upstream)?,
        })
    }

    /// The registry that `registry` names, or the first one. Returns its name and base URL.
    fn registry(&self, args: &Map<String, Value>) -> Result<(&str, Url), ConnectorError> {
        let registry = match args.get("registry") {
            None => self.registries.first(),
            Some(Value::String(name)) => self.registries.iter().find(|r| r.name == *name),
            Some(_) => None,
        }
        .ok_or_else(|| invalid("`registry` must name a registry of the profile"))?;
        let url = Url::parse(&registry.url).map_err(|e| ConnectorError::Failed(e.to_string()))?;
        Ok((&registry.name, url))
    }

    fn page_token(args: &Map<String, Value>) -> Result<Option<String>, ConnectorError> {
        Ok(text(args, "page_token", MAX_PAGE_TOKEN_CHARS)?.map(str::to_owned))
    }

    async fn search(&self, args: &Map<String, Value>) -> Result<Value, ConnectorError> {
        only(
            args,
            ARD_SEARCH,
            &["text", "registry", "filter", "context", "page_token"],
        )?;
        let text = text(args, "text", MAX_SEARCH_CHARS)?
            .ok_or_else(|| invalid(format!("`text` must have 1 to {MAX_SEARCH_CHARS} characters")))?;
        let (name, url) = self.registry(args)?;
        let request = SearchRequest {
            query: Query {
                context: context(args)?,
                text: text.to_owned(),
                filter: filter(args)?,
            },
            federation: Some(Federation::None),
            page_size: Some(SEARCH_PAGE_SIZE),
            page_token: Self::page_token(args)?,
        };
        let found = self.client.search(&url, &request).await.map_err(registry_error)?;
        // With hop limit 0 the first page is the only one.
        let page = found.pages.first().map(|page| &page.response);
        let results: Vec<Value> = page
            .map(|response| response.results.iter().map(|r| result(r, true)).collect())
            .unwrap_or_default();
        let token = page.and_then(|response| response.page_token.as_deref());
        Ok(json!({"registry": name, "results": results, "page_token": token}))
    }

    async fn explore(&self, args: &Map<String, Value>) -> Result<Value, ConnectorError> {
        only(args, ARD_EXPLORE, &["registry", "text", "filter", "context", "facets"])?;
        let facets = args.get("facets").unwrap_or(&Value::Null);
        let facets = strings(facets, "`facets`", MAX_FACETS, MAX_FILTER_CHARS)?;
        let (name, url) = self.registry(args)?;
        let query = Query {
            context: context(args)?,
            text: text(args, "text", MAX_SEARCH_CHARS)?.unwrap_or_default().to_owned(),
            filter: filter(args)?,
        };
        let request = ExploreRequest {
            query: (query != Query::default()).then_some(query),
            result_type: ResultType {
                facets: facets.into_iter().map(|field| FacetRequest { field }).collect(),
            },
        };
        let answer = self.client.explore(&url, &request).await.map_err(registry_error)?;
        Ok(json!({"registry": name, "facets": answer.facets}))
    }

    async fn list(&self, args: &Map<String, Value>) -> Result<Value, ConnectorError> {
        only(
            args,
            ARD_LIST,
            &[
                "registry",
                "type",
                "publisher",
                "display_name",
                "created_after",
                "updated_after",
                "order_by",
                "page_token",
            ],
        )?;
        let values = |name: &str| match args.get(name) {
            None => Ok(Vec::new()),
            Some(value) => strings(value, &format!("`{name}`"), MAX_FILTER_VALUES, MAX_FILTER_CHARS),
        };
        let time = |name: &str| -> Result<Option<String>, ConnectorError> {
            let Some(value) = text(args, name, 64)? else {
                return Ok(None);
            };
            match bunsin_core::time::parse_utc(value) {
                Some(_) => Ok(Some(value.to_owned())),
                None => Err(invalid(format!("`{name}` must be an RFC 3339 time"))),
            }
        };
        let filter = ListFilter {
            types: values("type")?,
            publishers: values("publisher")?,
            display_name: text(args, "display_name", MAX_FILTER_CHARS)?.map(str::to_owned),
            created_after: time("created_after")?,
            updated_after: time("updated_after")?,
        };
        let order_by = match args.get("order_by") {
            None => None,
            Some(Value::String(order)) if LIST_ORDERS.contains(&order.as_str()) => Some(order.clone()),
            Some(_) => return Err(invalid(format!("`order_by` must be one of {}", LIST_ORDERS.join(", ")))),
        };
        let (name, url) = self.registry(args)?;
        let request = ListRequest {
            filter: filter.expression().map_err(invalid)?,
            order_by,
            page_size: Some(SEARCH_PAGE_SIZE),
            page_token: Self::page_token(args)?,
        };
        let answer = self.client.list(&url, &request).await.map_err(registry_error)?;
        let items: Vec<Value> = answer.items.iter().map(|item| result(item, false)).collect();
        Ok(json!({"registry": name, "items": items, "total": answer.total, "page_token": answer.page_token}))
    }

    async fn resolve(&self, args: &Map<String, Value>) -> Result<Value, ConnectorError> {
        only(args, ARD_RESOLVE, &["domain"])?;
        let domain = match args.get("domain") {
            Some(Value::String(name)) => self.domains.iter().find(|d| d.domain() == name),
            _ => None,
        }
        .ok_or_else(|| invalid("`domain` must name a domain of the owner's list"))?;
        let seed = match domain.catalog() {
            None => Seed::domain(domain.domain()),
            Some(catalog) => {
                let catalog = Url::parse(catalog).map_err(|e| ConnectorError::Failed(e.to_string()))?;
                Seed::catalog(domain.domain(), catalog)
            }
        }
        .map_err(ConnectorError::Failed)?;
        let resolution = self.fetcher.resolve(&seed).await.map_err(ConnectorError::Failed)?;
        resolved(&resolution)
    }
}

impl Connector for ArdTools {
    fn name(&self) -> &str {
        ARD_CONNECTOR
    }

    fn list_tools(&self) -> BoxFuture<'_, Result<Vec<ToolSpec>, ConnectorError>> {
        let specs = tool_specs(&self.registries, &self.domains);
        Box::pin(async move { Ok(specs) })
    }

    fn call_tool(&self, tool: &str, args: Map<String, Value>) -> BoxFuture<'_, Result<Value, ConnectorError>> {
        let tool = tool.to_owned();
        Box::pin(async move {
            match tool.as_str() {
                ARD_SEARCH => self.search(&args).await,
                ARD_EXPLORE => self.explore(&args).await,
                ARD_LIST => self.list(&args).await,
                ARD_RESOLVE => self.resolve(&args).await,
                other => Err(ConnectorError::UnknownTool(other.to_owned())),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunsin_ard::{DiscoveryError, FetchError, Found, Source, accept_entry};

    fn resolution(discovery_error: Option<DiscoveryError>) -> Resolution {
        let entry = json!({"identifier": "urn:air:acme.com:kim", "displayName": "Kim", "type": "text/html",
                           "url": "https://acme.com/kim"});
        let found = Found {
            accepted: vec![accept_entry(&entry, "acme.com").unwrap()],
            rejected: Vec::new(),
        };
        Resolution {
            domain: "acme.com".into(),
            sources: vec![Source {
                url: Url::parse("https://acme.com/.well-known/ard.json").unwrap(),
                result: Ok(found),
            }],
            discovery_error,
        }
    }

    #[test]
    fn a_resolution_with_an_unread_page_is_a_readable_failure() {
        let shown = resolved(&resolution(None)).unwrap();
        assert_eq!(shown["entries"][0]["identifier"], "urn:air:acme.com:kim");
        let unread = DiscoveryError {
            url: Url::parse("https://acme.com/robots.txt").unwrap(),
            error: FetchError::Status(503),
        };
        let Err(ConnectorError::Failed(text)) = resolved(&resolution(Some(unread))) else {
            panic!("an unread robots.txt must fail the resolution");
        };
        assert_eq!(
            text,
            "the resolution of `acme.com` is incomplete: cannot read `https://acme.com/robots.txt`, \
             which may announce entry sources: the server answered status 503"
        );
    }
}
