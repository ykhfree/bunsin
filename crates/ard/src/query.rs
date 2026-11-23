//! The registry side of the search API (§5.3): the requests of `POST /search`, `POST /explore` and
//! `GET /agents`, the filter keys, the filter match, the facet count, and the answer bodies.
//!
//! A request that breaks a rule gets a [`QueryError`]. The server answers it with 400 and
//! `INVALID_ARGUMENT` (Appendix B). The shapes follow `ard.openapi.yaml`: an object with
//! `additionalProperties: false` refuses a member it does not name.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};

use crate::jsonld::Context;
use crate::search::{Bucket, FacetResult};

/// The default page size of `/search` (§5.3.2).
pub const SEARCH_PAGE_SIZE: usize = 10;
/// The default page size of `/agents` (§5.3.4).
pub const LIST_PAGE_SIZE: usize = 20;
/// The largest page size. A larger request gets this size.
pub const MAX_PAGE_SIZE: usize = 100;
/// The largest offset that a page token can carry.
pub const MAX_OFFSET: usize = 1_000;
/// The most facets of one explore request.
pub const MAX_FACETS: usize = 10;
/// The default bucket count of a facet (§5.3.3).
pub const FACET_LIMIT: usize = 20;
/// The largest bucket count of a facet. A larger request gets this count.
pub const MAX_FACET_LIMIT: usize = 100;
/// The most keys of the filter of one `/search` or `/explore` request. Each key is matched
/// against every visible row, so the count bounds the work of one request.
pub const MAX_REQUEST_FILTER_KEYS: usize = 20;
/// The most values of one filter key.
pub const MAX_REQUEST_FILTER_VALUES: usize = 50;

/// The terms whose members a filter key can reach by a literal dot path (§5.3.1).
const OPAQUE_TERMS: [&str; 3] = ["trustManifest", "metadata", "data"];

/// True for `scheme://...`. A key such as `okf:taxonomy` is a prefix and a name, not an absolute IRI.
fn is_absolute_iri(key: &str) -> bool {
    let Some((scheme, rest)) = key.split_once(':') else {
        return false;
    };
    let mut chars = scheme.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
        && rest.starts_with("//")
}

/// Why a request cannot be answered. The text is for the caller.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct QueryError(pub String);

fn invalid<T>(message: impl Into<String>) -> Result<T, QueryError> {
    Err(QueryError(message.into()))
}

/// What a filter key or a facet field names (§5.3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterKey {
    /// The `<publisher>` of the identifier (Appendix C). The registry extracts it.
    Publisher,
    /// A top-level term, by IRI.
    Term(String),
    /// A literal JSON path under `trustManifest`, `metadata` or `data`.
    Path { term: String, path: Vec<String> },
}

/// Resolves a filter key or a facet field through the query's effective context: the base
/// context and `query.@context` (§5.3.1).
fn resolve_key(key: &str, context: &Context) -> Result<FilterKey, QueryError> {
    if key == "publisher" {
        return Ok(FilterKey::Publisher);
    }
    // An absolute IRI holds dots, so it is checked before dot paths.
    if is_absolute_iri(key) {
        return Ok(FilterKey::Term(key.to_owned()));
    }
    if let Some((head, rest)) = key.split_once('.') {
        let path: Vec<String> = rest.split('.').map(str::to_owned).collect();
        if !OPAQUE_TERMS.contains(&head) || path.iter().any(String::is_empty) {
            return invalid(format!(
                "`{key}`: a dot path must start with trustManifest., metadata. or data."
            ));
        }
        return Ok(FilterKey::Path {
            term: context.expand_key(head),
            path,
        });
    }
    let iri = context.expand_key(key);
    if iri.starts_with('@') {
        return invalid(format!("`{key}` is a JSON-LD keyword, not a term"));
    }
    // A compact IRI whose prefix nothing binds stays as it was written.
    if let Some((prefix, _)) = key.split_once(':')
        && iri == key
    {
        return invalid(format!(
            "`{key}` uses the prefix `{prefix}`, which query.@context does not bind"
        ));
    }
    Ok(FilterKey::Term(iri))
}

/// The values of one value: a string, a number or a boolean as text, each element of an array.
fn push_values(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(text) => out.push(text.clone()),
        Value::Number(_) | Value::Bool(_) => out.push(value.to_string()),
        Value::Array(items) => items.iter().for_each(|item| push_values(item, out)),
        Value::Null | Value::Object(_) => {}
    }
}

/// Follows a literal path. An array on the way stands for each of its elements.
fn walk<'a>(value: &'a Value, path: &[String], out: &mut Vec<&'a Value>) {
    let Some((first, rest)) = path.split_first() else {
        out.push(value);
        return;
    };
    match value {
        Value::Array(items) => items.iter().for_each(|item| walk(item, path, out)),
        Value::Object(map) => {
            if let Some(next) = map.get(first) {
                walk(next, rest, out);
            }
        }
        _ => {}
    }
}

/// The values at `key` of one entry, given its terms by IRI and its publisher.
pub fn values_at(key: &FilterKey, terms: &Map<String, Value>, publisher: &str) -> Vec<String> {
    let mut out = Vec::new();
    match key {
        FilterKey::Publisher => out.push(publisher.to_owned()),
        FilterKey::Term(iri) => {
            if let Some(value) = terms.get(iri) {
                push_values(value, &mut out);
            }
        }
        FilterKey::Path { term, path } => {
            let mut found = Vec::new();
            if let Some(value) = terms.get(term) {
                walk(value, path, &mut found);
            }
            found.into_iter().for_each(|value| push_values(value, &mut out));
        }
    }
    out
}

/// One filter constraint: the entry matches if one of its values at `key` is one of `values`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Constraint {
    /// The key as the request wrote it.
    pub key_text: String,
    pub key: FilterKey,
    pub values: Vec<String>,
}

/// The query object of `/search` and `/explore` (§5.3.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryModel {
    pub text: Option<String>,
    /// By key text. All must match.
    pub filter: Vec<Constraint>,
}

impl QueryModel {
    /// True if the entry satisfies every constraint. A publisher compares without ASCII case.
    pub fn matches(&self, terms: &Map<String, Value>, publisher: &str) -> bool {
        self.filter.iter().all(|constraint| {
            let found = values_at(&constraint.key, terms, publisher);
            constraint.values.iter().any(|wanted| match constraint.key {
                FilterKey::Publisher => found.iter().any(|value| value.eq_ignore_ascii_case(wanted)),
                _ => found.contains(wanted),
            })
        })
    }

    /// The same text for the same query, whatever the order of the filter keys. A page token holds
    /// its digest. It holds the resolved keys, so another `@context` makes another text.
    pub fn canonical(&self) -> String {
        let filter: Vec<(String, &[String])> = self
            .filter
            .iter()
            .map(|constraint| (format!("{:?}", constraint.key), constraint.values.as_slice()))
            .collect();
        serde_json::to_string(&(&self.text, filter)).expect("strings always serialize")
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawQuery {
    #[serde(rename = "@context", default)]
    context: Option<Value>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    filter: Option<Map<String, Value>>,
}

impl RawQuery {
    /// The query, and its effective context.
    fn parse(self) -> Result<(QueryModel, Context), QueryError> {
        let context = Context::base()
            .with(self.context.as_ref())
            .map_err(|e| QueryError(format!("`query.@context`: {e}")))?;
        let raw_filter = self.filter.unwrap_or_default();
        if raw_filter.len() > MAX_REQUEST_FILTER_KEYS {
            return invalid(format!("`query.filter` has more than {MAX_REQUEST_FILTER_KEYS} keys"));
        }
        let mut filter = Vec::new();
        for (key_text, value) in raw_filter {
            if value
                .as_array()
                .is_some_and(|items| items.len() > MAX_REQUEST_FILTER_VALUES)
            {
                return invalid(format!(
                    "`{key_text}` has more than {MAX_REQUEST_FILTER_VALUES} filter values"
                ));
            }
            let key = resolve_key(&key_text, &context)?;
            let values = match value {
                Value::String(text) => vec![text],
                Value::Array(items) if !items.is_empty() => items
                    .into_iter()
                    .map(|item| match item {
                        Value::String(text) => Ok(text),
                        _ => invalid(format!("`{key_text}`: every filter value must be a string")),
                    })
                    .collect::<Result<_, _>>()?,
                _ => {
                    return invalid(format!(
                        "`{key_text}`: a filter value is a string or an array of strings"
                    ));
                }
            };
            filter.push(Constraint { key_text, key, values });
        }
        filter.sort_by(|a, b| a.key_text.cmp(&b.key_text));
        let query = QueryModel {
            text: self.text,
            filter,
        };
        Ok((query, context))
    }
}

/// The federation modes (§5.4). The registry has no upstream, so all three search its own index.
#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum Federation {
    Auto,
    Referrals,
    None,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawSearch {
    query: RawQuery,
    /// Every mode searches the own index and returns no referrals (§5.4). An unknown mode is refused.
    #[serde(rename = "federation", default)]
    _federation: Option<Federation>,
    #[serde(default)]
    page_size: Option<i64>,
    #[serde(default)]
    page_token: Option<String>,
}

/// A `POST /search` request (§5.3.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// Its `text` is present.
    pub query: QueryModel,
    pub page_size: usize,
    pub page_token: Option<String>,
}

fn page_size(asked: Option<i64>, default: usize) -> Result<usize, QueryError> {
    match asked {
        None => Ok(default),
        Some(size) if size >= 1 => Ok((size as usize).min(MAX_PAGE_SIZE)),
        Some(_) => invalid("`pageSize` must be 1 or more"),
    }
}

fn body<'a, T: Deserialize<'a>>(bytes: &'a [u8], what: &str) -> Result<T, QueryError> {
    serde_json::from_slice(bytes).map_err(|e| QueryError(format!("the body is not a valid {what} request: {e}")))
}

/// Reads the body of `POST /search`.
pub fn parse_search(bytes: &[u8]) -> Result<SearchQuery, QueryError> {
    let raw: RawSearch = body(bytes, "search")?;
    let (query, _) = raw.query.parse()?;
    if query.text.is_none() {
        return invalid("`query.text` is required for search (§5.3.2)");
    }
    Ok(SearchQuery {
        query,
        page_size: page_size(raw.page_size, SEARCH_PAGE_SIZE)?,
        page_token: raw.page_token,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawFacet {
    field: String,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    min_count: Option<i64>,
}

/// `resultType` allows other members: future result shapes extend it (§5.3.3).
#[derive(Deserialize)]
struct RawResultType {
    facets: Vec<RawFacet>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RawExplore {
    #[serde(default)]
    query: Option<RawQuery>,
    result_type: RawResultType,
}

/// One facet of an explore request (§5.3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facet {
    /// The field as the request wrote it. The answer uses it as the key.
    pub field: String,
    pub key: FilterKey,
    pub limit: usize,
    pub min_count: u64,
}

/// A `POST /explore` request (§5.3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExploreQuery {
    pub query: QueryModel,
    pub facets: Vec<Facet>,
}

/// Reads the body of `POST /explore`.
pub fn parse_explore(bytes: &[u8]) -> Result<ExploreQuery, QueryError> {
    let raw: RawExplore = body(bytes, "explore")?;
    let (query, context) = match raw.query {
        Some(query) => query.parse()?,
        None => (QueryModel::default(), Context::base().clone()),
    };
    let count = raw.result_type.facets.len();
    if !(1..=MAX_FACETS).contains(&count) {
        return invalid(format!("`resultType.facets` needs 1 to {MAX_FACETS} facets"));
    }
    let mut facets: Vec<Facet> = Vec::new();
    for facet in raw.result_type.facets {
        if facets.iter().any(|known| known.field == facet.field) {
            return invalid(format!("the facet `{}` is asked twice", facet.field));
        }
        let limit = match facet.limit {
            None => FACET_LIMIT,
            Some(limit) if limit >= 1 => (limit as usize).min(MAX_FACET_LIMIT),
            Some(_) => return invalid("a facet `limit` must be 1 or more"),
        };
        let min_count = match facet.min_count {
            None => 1,
            Some(count) if count >= 0 => count as u64,
            Some(_) => return invalid("a facet `minCount` must not be negative"),
        };
        facets.push(Facet {
            key: resolve_key(&facet.field, &context)?,
            field: facet.field,
            limit,
            min_count,
        });
    }
    Ok(ExploreQuery { query, facets })
}

impl Facet {
    /// Counts the entries of each value at the key. An entry counts once per value. Buckets below
    /// `min_count` go; the rest are ordered by count, then value, and cut at `limit`. `otherCount`
    /// is the sum of the counts of the cut buckets, and absent if no bucket was cut.
    pub fn count<'a>(&self, entries: impl IntoIterator<Item = (&'a Map<String, Value>, &'a str)>) -> FacetResult {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for (terms, publisher) in entries {
            let values: BTreeSet<String> = values_at(&self.key, terms, publisher).into_iter().collect();
            for value in values {
                *counts.entry(value).or_default() += 1;
            }
        }
        let mut counted: Vec<(String, u64)> = counts
            .into_iter()
            .filter(|(_, count)| *count >= self.min_count)
            .collect();
        counted.sort_by(|(a_value, a), (b_value, b)| b.cmp(a).then_with(|| a_value.cmp(b_value)));
        let other: u64 = counted.iter().skip(self.limit).map(|(_, count)| count).sum();
        counted.truncate(self.limit);
        FacetResult {
            buckets: counted
                .into_iter()
                .map(|(value, count)| Bucket {
                    value,
                    count: Some(count),
                })
                .collect(),
            other_count: (other > 0).then_some(other),
        }
    }
}

/// The answer of `POST /search` (schema `SearchResponse`). It never has `referrals` (§5.4).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SearchAnswer {
    pub results: Vec<Map<String, Value>>,
    #[serde(rename = "pageToken", skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
}

/// The answer of `GET /agents` (schema `ListResponse`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListAnswer {
    pub items: Vec<Map<String, Value>>,
    pub total: u64,
    #[serde(rename = "pageToken", skip_serializing_if = "Option::is_none")]
    pub page_token: Option<String>,
}

/// One clause of the `GET /agents` filter (Appendix A). Values of one clause are alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListClause {
    /// Matches if the display name holds one of the texts, ignoring case.
    DisplayName(Vec<String>),
    Type(Vec<String>),
    PublisherId(Vec<String>),
    /// The time as the request wrote it. The registry reads it.
    CreatedAfter(String),
    UpdatedAfter(String),
}

/// The order of `GET /agents`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ListField {
    DisplayName,
    CreatedAt,
    UpdatedAt,
    #[default]
    Identifier,
}

/// A `GET /agents` request (§5.3.4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListQuery {
    /// All must match.
    pub clauses: Vec<ListClause>,
    pub order: ListField,
    pub descending: bool,
    pub page_size: usize,
    pub page_token: Option<String>,
    /// The `filter` and `orderBy` texts. A page token holds their digest.
    pub canonical: String,
}

/// Reads the filter of `GET /agents`. The grammar is the part that Appendix A and the OpenAPI
/// example both allow:
///
/// ```text
/// filter = clause *(" AND " clause)
/// clause = ("displayName" / "type" / "publisherId") " = " quoted
///        / ("createdAfter" / "updatedAfter") " > " quoted
/// quoted = "'" value *("," value) "'"
/// ```
///
/// Spaces around the operator and `AND` may repeat. A value holds no `'` and no `,`.
pub fn parse_list_filter(text: &str) -> Result<Vec<ListClause>, QueryError> {
    let wrong = || {
        QueryError(format!(
            "the filter `{text}` is not of the form field = 'value' AND ... (Appendix A)"
        ))
    };
    let mut clauses = Vec::new();
    let mut rest = text.trim();
    while !rest.is_empty() {
        let field_end = rest.find(|c: char| !c.is_ascii_alphabetic()).unwrap_or(rest.len());
        let (field, after) = rest.split_at(field_end);
        let after = after.trim_start();
        let operator = after.chars().next().ok_or_else(wrong)?;
        // The operator may be any character, so the slice steps over its UTF-8 length.
        let after = after[operator.len_utf8()..]
            .trim_start()
            .strip_prefix('\'')
            .ok_or_else(wrong)?;
        let close = after.find('\'').ok_or_else(wrong)?;
        let values: Vec<String> = after[..close].split(',').map(|v| v.trim().to_owned()).collect();
        if values.iter().any(String::is_empty) {
            return Err(wrong());
        }
        let single = |values: Vec<String>| match <[String; 1]>::try_from(values) {
            Ok([value]) => Ok(value),
            Err(_) => invalid(format!("`{field}` takes one time")),
        };
        clauses.push(match (field, operator) {
            ("displayName", '=') => ListClause::DisplayName(values),
            ("type", '=') => ListClause::Type(values),
            ("publisherId", '=') => ListClause::PublisherId(values),
            ("createdAfter", '>') => ListClause::CreatedAfter(single(values)?),
            ("updatedAfter", '>') => ListClause::UpdatedAfter(single(values)?),
            _ => {
                return invalid(format!(
                    "`{field} {operator}` is not a filter clause: use displayName, type or publisherId with =, \
                     createdAfter or updatedAfter with > (Appendix A)"
                ));
            }
        });
        rest = after[close + 1..].trim_start();
        if !rest.is_empty() {
            rest = rest
                .strip_prefix("AND")
                .filter(|next| next.starts_with(' '))
                .ok_or_else(wrong)?
                .trim_start();
            if rest.is_empty() {
                return Err(wrong());
            }
        }
    }
    Ok(clauses)
}

/// Reads `orderBy`: a field and an optional `ASC` or `DESC`.
fn parse_order(text: &str) -> Result<(ListField, bool), QueryError> {
    let words: Vec<&str> = text.split_whitespace().collect();
    let (field, direction) = match words.as_slice() {
        [field] => (*field, "ASC"),
        [field, direction] => (*field, *direction),
        _ => return invalid(format!("`orderBy` `{text}` is not a field and a direction")),
    };
    let field = match field {
        "displayName" | "name" => ListField::DisplayName,
        "createdAt" | "created_at" => ListField::CreatedAt,
        "updatedAt" | "updated_at" => ListField::UpdatedAt,
        "identifier" => ListField::Identifier,
        other => {
            return invalid(format!(
                "`orderBy` cannot use `{other}`: use displayName, createdAt, updatedAt or identifier"
            ));
        }
    };
    match direction.to_ascii_uppercase().as_str() {
        "ASC" => Ok((field, false)),
        "DESC" => Ok((field, true)),
        _ => invalid(format!("`orderBy` direction `{direction}` is not ASC or DESC")),
    }
}

/// Reads the decoded query parameters of `GET /agents`. A parameter that the operation does
/// not name, or a repeated one, is refused.
pub fn parse_list(pairs: impl IntoIterator<Item = (String, String)>) -> Result<ListQuery, QueryError> {
    let mut seen: BTreeMap<String, String> = BTreeMap::new();
    for (name, value) in pairs {
        if !["filter", "orderBy", "pageSize", "pageToken"].contains(&name.as_str()) {
            return invalid(format!("`{name}` is not a parameter of GET /agents"));
        }
        if seen.insert(name.clone(), value).is_some() {
            return invalid(format!("`{name}` is given twice"));
        }
    }
    let filter = seen.get("filter").cloned().unwrap_or_default();
    let order = seen.get("orderBy").cloned().unwrap_or_default();
    let (field, descending) = if order.trim().is_empty() {
        (ListField::Identifier, false)
    } else {
        parse_order(&order)?
    };
    let size = match seen.get("pageSize") {
        None => None,
        Some(text) => Some(
            text.parse::<i64>()
                .map_err(|_| QueryError(format!("`pageSize` `{text}` is not an integer")))?,
        ),
    };
    Ok(ListQuery {
        clauses: parse_list_filter(&filter)?,
        order: field,
        descending,
        page_size: page_size(size, LIST_PAGE_SIZE)?,
        page_token: seen.get("pageToken").cloned(),
        canonical: serde_json::to_string(&(filter, order)).expect("strings always serialize"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonld::{ARD_NAMESPACE, expand_terms};
    use serde_json::json;

    /// The terms of an entry, as the index stores them.
    fn terms_of(entry: Value) -> Map<String, Value> {
        expand_terms(entry.as_object().unwrap()).unwrap()
    }

    fn search(body: Value) -> Result<SearchQuery, QueryError> {
        parse_search(body.to_string().as_bytes())
    }

    fn explore(body: Value) -> Result<ExploreQuery, QueryError> {
        parse_explore(body.to_string().as_bytes())
    }

    fn ns(name: &str) -> String {
        format!("{ARD_NAMESPACE}{name}")
    }

    #[test]
    fn the_spec_requests_are_read() {
        // The request example of §5.3.2, and the query example of §5.3.1 without its bound prefix.
        let found = search(json!({
            "query": {"text": "find me a flight booking agent", "filter": {"type": ["application/a2a-agent-card+json"]}},
            "federation": "referrals",
            "pageSize": 5
        }))
        .unwrap();
        assert_eq!(found.page_size, 5);
        assert_eq!(found.query.text.as_deref(), Some("find me a flight booking agent"));
        assert_eq!(found.query.filter[0].key, FilterKey::Term(ns("mediaType")));
        let found = search(json!({"query": {
            "@context": {"okf": "https://openknowledgeformat.org/ns#"},
            "text": "x",
            "filter": {"tags": "finance", "trustManifest.attestations.type": ["SOC2-Type2"]}
        }}))
        .unwrap();
        assert_eq!(found.page_size, SEARCH_PAGE_SIZE);
        let keys: Vec<&FilterKey> = found.query.filter.iter().map(|c| &c.key).collect();
        assert_eq!(
            keys,
            [
                &FilterKey::Term(ns("tags")),
                &FilterKey::Path {
                    term: ns("trustManifest"),
                    path: vec!["attestations".into(), "type".into()]
                }
            ]
        );
        // The request example of §5.3.3.
        let found = explore(json!({
            "query": {"text": "currency conversion", "filter": {"trustManifest.attestations.type": ["SOC2-Type2"]}},
            "resultType": {"facets": [{"field": "type"}, {"field": "publisher", "limit": 50}]}
        }))
        .unwrap();
        assert_eq!(found.facets[0].key, FilterKey::Term(ns("mediaType")));
        assert_eq!((found.facets[0].limit, found.facets[0].min_count), (FACET_LIMIT, 1));
        assert_eq!(
            (found.facets[1].key.clone(), found.facets[1].limit),
            (FilterKey::Publisher, 50)
        );
    }

    #[test]
    fn unknown_members_and_unbound_prefixes_get_400() {
        let refused = |body: Value| search(body).unwrap_err().0;
        // Members that `additionalProperties: false` objects do not name.
        assert!(refused(json!({"query": {"text": "x"}, "limit": 3})).contains("unknown field `limit`"));
        assert!(refused(json!({"query": {"text": "x", "filters": {}}})).contains("unknown field `filters`"));
        assert!(
            explore(json!({"resultType": {"facets": [{"field": "type", "size": 3}]}}))
                .unwrap_err()
                .0
                .contains("unknown field `size`")
        );
        assert!(explore(json!({"resultType": {"facets": [{"field": "type"}]}, "x": 1})).is_err());
        // `resultType` itself is open for future result shapes.
        assert!(explore(json!({"resultType": {"facets": [{"field": "type"}], "counts": true}})).is_ok());
        // A prefix that nothing binds, and other dot paths.
        let text = refused(json!({"query": {"text": "x", "filter": {"okf:taxonomy": "us-gaap"}}}));
        assert!(text.contains("does not bind"), "{text}");
        for key in ["tags.first", "acme:tier.x", "trustManifest.", "metadata..x"] {
            let text = refused(json!({"query": {"text": "x", "filter": {key: "v"}}}));
            assert!(text.contains("dot path"), "{key}: {text}");
        }
        assert!(refused(json!({"query": {"text": "x", "filter": {"id": "v"}}})).contains("keyword"));
        // A context that the expansion does not support is a request error.
        for context in [
            json!(5),
            json!("https://schema.org"),
            json!({"type": "https://schema.org/type"}),
            json!({"@vocab": "https://evil.example/"}),
        ] {
            let text = refused(json!({"query": {"@context": context.clone(), "text": "x"}}));
            assert!(text.contains("query.@context"), "{context}: {text}");
        }
        // Other request rules.
        assert!(refused(json!({"query": {"filter": {}}})).contains("text"));
        assert!(refused(json!({"query": {"text": "x"}, "federation": "upstream"})).contains("unknown variant"));
        assert!(refused(json!({"query": {"text": "x"}, "pageSize": 0})).contains("pageSize"));
        assert!(refused(json!({"query": {"text": "x", "filter": {"tags": 5}}})).contains("string"));
        assert!(refused(json!({"query": {"text": "x", "filter": {"tags": []}}})).contains("string"));
        assert!(parse_search(b"{not json").is_err());
        for facets in [
            json!([]),
            json!(
                (0..11)
                    .map(|i| json!({"field": format!("ard:x{i}")}))
                    .collect::<Vec<_>>()
            ),
        ] {
            assert!(
                explore(json!({"resultType": {"facets": facets}}))
                    .unwrap_err()
                    .0
                    .contains("1 to 10")
            );
        }
        assert!(
            explore(json!({"resultType": {"facets": [{"field": "okf:x"}]}}))
                .unwrap_err()
                .0
                .contains("does not bind")
        );
        assert!(explore(json!({"resultType": {"facets": [{"field": "type"}, {"field": "type"}]}})).is_err());
    }

    #[test]
    fn a_filter_has_at_most_20_keys_and_50_values_a_key() {
        let keys = |count: usize| -> Map<String, Value> { (0..count).map(|n| (format!("k{n}"), json!("v"))).collect() };
        let values = |count: usize| json!((0..count).map(|n| format!("v{n}")).collect::<Vec<_>>());
        let facets = json!({"facets": [{"field": "type"}]});
        // Search and explore share the rule.
        let search_filter = |filter: Value| search(json!({"query": {"text": "x", "filter": filter}}));
        let explore_filter = |filter: Value| explore(json!({"query": {"filter": filter}, "resultType": facets}));
        for (filter, allowed) in [
            (json!(keys(20)), true),
            (json!(keys(21)), false),
            (json!({"tags": values(50), "type": values(50)}), true),
            (json!({"tags": values(51)}), false),
            (json!({"tags": "one", "type": values(51)}), false),
        ] {
            for outcome in [
                search_filter(filter.clone()).map(|_| ()),
                explore_filter(filter.clone()).map(|_| ()),
            ] {
                match (outcome, allowed) {
                    (Ok(()), true) => {}
                    (Err(error), false) => assert!(error.0.contains("more than"), "{error}"),
                    (outcome, _) => panic!("{filter}: {outcome:?}"),
                }
            }
        }
        assert_eq!(
            search_filter(json!(keys(21))).unwrap_err().0,
            "`query.filter` has more than 20 keys"
        );
        assert_eq!(
            explore_filter(json!({"tags": values(51)})).unwrap_err().0,
            "`tags` has more than 50 filter values"
        );
    }

    #[test]
    fn a_query_prefix_matches_an_entry_that_bound_the_same_iri_under_another_prefix() {
        // The entry binds the namespace as `openknowledge`, the query as `okf` (§5.3.1).
        let terms = terms_of(json!({
            "@context": ["https://agenticresourcediscovery.org/context/v1", {"openknowledge": "https://openknowledgeformat.org/ns#"}],
            "openknowledge:taxonomy": ["us-gaap", "ifrs"],
            "serviceTier": "gold"
        }));
        let matches = |context: Value, filter: Value| {
            search(json!({"query": {"@context": context, "text": "x", "filter": filter}}))
                .unwrap()
                .query
                .matches(&terms, "acme.com")
        };
        let okf = json!({"okf": "https://openknowledgeformat.org/ns#"});
        assert!(matches(okf.clone(), json!({"okf:taxonomy": "ifrs"})));
        assert!(!matches(okf.clone(), json!({"okf:taxonomy": "gaap"})));
        // The same IRI written out, and a prefix bound to another namespace.
        assert!(matches(
            json!([]),
            json!({"https://openknowledgeformat.org/ns#taxonomy": "us-gaap"})
        ));
        assert!(!matches(
            json!({"okf": "https://other.example/ns#"}),
            json!({"okf:taxonomy": "ifrs"})
        ));
        // A facet field resolves the same way.
        let found = explore(json!({"query": {"@context": okf}, "resultType": {"facets": [{"field": "okf:taxonomy"}]}}))
            .unwrap();
        assert_eq!(
            found.facets[0].key,
            FilterKey::Term("https://openknowledgeformat.org/ns#taxonomy".into())
        );
        // A plain name falls into the default namespace, as the entry's own key did.
        assert!(matches(json!([]), json!({"serviceTier": "gold"})));
        assert!(matches(json!([]), json!({"ard:serviceTier": "gold"})));
    }

    #[test]
    fn a_filter_matches_any_value_of_an_array_and_every_key() {
        let entry = json!({
            "type": "application/mcp-server-card+json",
            "tags": ["payments", "refunds"],
            "version": 2,
            "https://acme.com/vocab#tier": "gold",
            "trustManifest": {"identity": "spiffe://acme.com/x", "attestations": [{"type": "SOC2-Type2"}, {"type": "HIPAA"}]},
            "metadata": {"location": {"city": "Seoul"}}
        });
        let terms = terms_of(entry);
        let matches = |filter: Value| {
            search(json!({"query": {"text": "x", "filter": filter}}))
                .unwrap()
                .query
                .matches(&terms, "acme.com")
        };
        assert!(matches(json!({})));
        assert!(matches(json!({"tags": "refunds"})));
        assert!(matches(
            json!({"tags": ["hr", "payments"], "type": "application/mcp-server-card+json"})
        ));
        assert!(!matches(
            json!({"tags": "payments", "type": "application/a2a-agent-card+json"})
        ));
        assert!(matches(json!({"trustManifest.attestations.type": "HIPAA"})));
        assert!(!matches(json!({"trustManifest.attestations.type": "ISO"})));
        assert!(matches(json!({"metadata.location.city": "Seoul"})));
        assert!(matches(json!({"https://acme.com/vocab#tier": "gold"})));
        assert!(matches(json!({"version": "2"})));
        assert!(matches(json!({"publisher": "ACME.com"})));
        assert!(!matches(json!({"publisher": "evil.com"})));
        // A value compares exactly.
        assert!(!matches(json!({"tags": "Payments"})));
        assert!(!matches(json!({"description": "x"})));
    }

    #[test]
    fn the_canonical_text_ignores_the_order_of_filter_keys() {
        let one = search(json!({"query": {"text": "x", "filter": {"tags": "a", "type": "b"}}})).unwrap();
        let two = search(json!({"query": {"text": "x", "filter": {"type": "b", "tags": "a"}}, "pageSize": 3})).unwrap();
        let three = search(json!({"query": {"text": "y", "filter": {"type": "b", "tags": "a"}}})).unwrap();
        assert_eq!(one.query.canonical(), two.query.canonical());
        assert_ne!(one.query.canonical(), three.query.canonical());
    }

    #[test]
    fn a_facet_counts_each_entry_once_per_value_and_cuts_at_its_limit() {
        let rows: Vec<(Map<String, Value>, &str)> = [
            (json!({"tags": ["a", "b", "a"]}), "acme.com"),
            (json!({"tags": ["a"]}), "acme.com"),
            (json!({"tags": ["c"]}), "beta.com"),
            (json!({"tags": ["b", "d"]}), "beta.com"),
            (json!({}), "beta.com"),
        ]
        .into_iter()
        .map(|(entry, publisher)| (terms_of(entry), publisher))
        .collect();
        let facet = |field: &str, limit: usize, min_count: u64| Facet {
            field: field.into(),
            key: resolve_key(field, Context::base()).unwrap(),
            limit,
            min_count,
        };
        let count = |facet: Facet| facet.count(rows.iter().map(|(terms, publisher)| (terms, *publisher)));
        let bucket = |value: &str, count: u64| Bucket {
            value: value.into(),
            count: Some(count),
        };
        assert_eq!(
            count(facet("tags", 20, 1)),
            FacetResult {
                buckets: vec![bucket("a", 2), bucket("b", 2), bucket("c", 1), bucket("d", 1)],
                other_count: None,
            }
        );
        assert_eq!(
            count(facet("tags", 1, 1)),
            FacetResult {
                buckets: vec![bucket("a", 2)],
                other_count: Some(4),
            }
        );
        // A bucket below `minCount` is not cut: it is not there, so it adds nothing to `otherCount`.
        assert_eq!(
            count(facet("tags", 1, 2)),
            FacetResult {
                buckets: vec![bucket("a", 2)],
                other_count: Some(2),
            }
        );
        assert_eq!(
            count(facet("publisher", 20, 1)).buckets,
            [bucket("beta.com", 3), bucket("acme.com", 2)]
        );
    }

    fn list(pairs: &[(&str, &str)]) -> Result<ListQuery, QueryError> {
        parse_list(
            pairs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned())),
        )
    }

    #[test]
    fn the_list_filter_accepts_only_the_appendix_a_subset() {
        // The OpenAPI example, and a clause of each field.
        assert_eq!(
            parse_list_filter("type = 'application/mcp-server-card+json' AND createdAfter > '2026-01-01'").unwrap(),
            [
                ListClause::Type(vec!["application/mcp-server-card+json".into()]),
                ListClause::CreatedAfter("2026-01-01".into()),
            ]
        );
        assert_eq!(
            parse_list_filter(
                "displayName='Kim'  AND  publisherId = 'acme.com,beta.com' AND updatedAfter>'2026-11-20T00:00:00Z'"
            )
            .unwrap(),
            [
                ListClause::DisplayName(vec!["Kim".into()]),
                ListClause::PublisherId(vec!["acme.com".into(), "beta.com".into()]),
                ListClause::UpdatedAfter("2026-11-20T00:00:00Z".into()),
            ]
        );
        assert_eq!(parse_list_filter("  ").unwrap(), []);
        for wrong in [
            "type = application/json",                // no quotes
            "type = 'a' OR type = 'b'",               // OR is a comma inside the quotes
            "type = 'a' and publisherId = 'b'",       // AND is upper case
            "type = 'a' AND",                         // no clause after AND
            "type = 'a' publisherId = 'b'",           // no AND
            "type > 'a'",                             // > is for times
            "createdAfter = '2026-01-01'",            // = is for names
            "createdAfter > '2026-01-01,2026-02-01'", // one time only
            "name = 'Kim'",                           // not an Appendix A field
            "type = 'a,'",                            // an empty value
            "type = 'a",                              // an open quote
            "(type = 'a')",                           // no grouping
            "type ≥ 'a'",                             // an operator of more than one byte
            "é",                                      // a field of more than one byte, and nothing else
        ] {
            assert!(parse_list_filter(wrong).is_err(), "{wrong}");
        }
        // The other parameters.
        let found = list(&[
            ("filter", "type = 'a'"),
            ("orderBy", "updated_at desc"),
            ("pageSize", "500"),
        ])
        .unwrap();
        assert_eq!(
            (found.order, found.descending, found.page_size),
            (ListField::UpdatedAt, true, MAX_PAGE_SIZE)
        );
        let found = list(&[]).unwrap();
        assert_eq!(
            (found.order, found.descending, found.page_size),
            (ListField::Identifier, false, LIST_PAGE_SIZE)
        );
        assert_eq!(list(&[("orderBy", "name")]).unwrap().order, ListField::DisplayName);
        for wrong in [
            vec![("orderBy", "score DESC")],
            vec![("orderBy", "name UP")],
            vec![("pageSize", "0")],
            vec![("pageSize", "ten")],
            vec![("query", "x")],
            vec![("filter", "type = 'a'"), ("filter", "type = 'b'")],
        ] {
            assert!(list(&wrong).is_err(), "{wrong:?}");
        }
    }
}
