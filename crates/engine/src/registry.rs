//! The ARD registry (ARCHITECTURE 18): its index in `registry.db`, the ranking and the pages of
//! `/search`, `/explore` and `/agents`, and its own entry in the catalog of each listener that serves it.
//!
//! Every count uses only the rows that the asking listener sees: `N`, `df`, the best score, the
//! cutoff, the identifier de-duplication, the facet counts and `total`. Otherwise a public caller
//! could learn from a score whether an internal entry holds a word.

use crate::ard::OuterListener;
use crate::{Engine, internal, store_error};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bunsin_api::{ApiError, ApiResult};
use bunsin_ard::{
    ArdEntry, ExploreAnswer, ExploreQuery, Level, ListAnswer, ListClause, ListField, ListQuery, MAX_OFFSET, QueryModel,
    REGISTRY_TYPES, SearchAnswer, SearchQuery, entry_terms, urn_publisher, validate,
};
use bunsin_core::inbound::{InboundScope, reaches};
use bunsin_core::search::{query_terms, rank_scored};
use bunsin_core::time::parse_utc;
use bunsin_store::{IndexedRow, NewEntry, Refusal, RegistryStore, SourceKind};
use serde_json::{Map, Value, json};
use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

/// The largest entry that the index stores, in bytes of JSON.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// The most entries of one source.
pub const MAX_SOURCE_ENTRIES: usize = 500;
/// The most entries of the whole index.
pub const MAX_ENTRIES: usize = 10_000;

/// The terms whose text the full-text index holds.
const SEARCHED_TERMS: [&str; 5] = [
    "displayName",
    "description",
    "representativeQueries",
    "capabilities",
    "tags",
];

/// The registry that the server operator turned on.
pub(crate) struct Registry {
    /// The widest listener scope that serves it: `internal`, or `public` for both listeners.
    scope: InboundScope,
    store: RegistryStore,
}

/// The source name of the catalog of a listener.
fn self_source(listener: InboundScope) -> &'static str {
    match listener {
        InboundScope::Internal => "self:internal",
        InboundScope::Public => "self:public",
    }
}

/// The host of a listener address such as `https://agents.acme.com:8443/`, in lower case.
fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let host = rest.split(['/', ':', '?', '#']).next()?;
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// The text of the searched terms of an entry, one value a line.
fn search_text(entry: &Map<String, Value>) -> String {
    let mut lines = Vec::new();
    for term in SEARCHED_TERMS {
        match entry.get(term) {
            Some(Value::String(text)) => lines.push(text.clone()),
            Some(Value::Array(items)) => lines.extend(items.iter().filter_map(Value::as_str).map(str::to_owned)),
            _ => {}
        }
    }
    lines.join("\n")
}

/// An entry as an answer shows it: without `representativeQueries`, which serve the index (§5.3.2).
fn shown(body: Value) -> Map<String, Value> {
    let mut item = match body {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    item.remove("representativeQueries");
    item
}

/// The page token of the page at `offset`: base64url of the offset and a digest of the request.
fn page_token(kind: &str, canonical: &str, offset: usize) -> String {
    let digest = bunsin_core::sha256_hex(format!("{kind}\n{canonical}").as_bytes());
    URL_SAFE_NO_PAD.encode(format!("{offset}.{}", &digest[..16]))
}

/// The offset of a page token. A token of another request, or of an offset past [`MAX_OFFSET`], is refused.
fn page_offset(kind: &str, canonical: &str, token: Option<&str>) -> ApiResult<usize> {
    let Some(token) = token else { return Ok(0) };
    let refused = || ApiError::Invalid("`pageToken` does not belong to this request".into());
    let text = URL_SAFE_NO_PAD.decode(token).map_err(|_| refused())?;
    let text = String::from_utf8(text).map_err(|_| refused())?;
    let offset: usize = text
        .split_once('.')
        .and_then(|(n, _)| n.parse().ok())
        .ok_or_else(refused)?;
    if offset > MAX_OFFSET || page_token(kind, canonical, offset) != token {
        return Err(refused());
    }
    Ok(offset)
}

/// The rows of one page, and the token of the next page if there is one within [`MAX_OFFSET`].
fn page<T>(rows: Vec<T>, offset: usize, size: usize, next: impl Fn(usize) -> String) -> (Vec<T>, Option<String>) {
    let following = offset + size;
    let token = (following < rows.len() && following <= MAX_OFFSET).then(|| next(following));
    (rows.into_iter().skip(offset).take(size).collect(), token)
}

/// A time of a list filter: RFC 3339, or a date that stands for its first second in UTC.
fn filter_time(text: &str) -> ApiResult<i64> {
    parse_utc(text)
        .or_else(|| {
            (text.len() == 10)
                .then(|| parse_utc(&format!("{text}T00:00:00Z")))
                .flatten()
        })
        .ok_or_else(|| ApiError::Invalid(format!("`{text}` is not an ISO 8601 date or time")))
}

impl Engine {
    /// Turns the registry on for the listeners that `scope` reaches. It opens `registry.db` in the
    /// data directory. The server operator does this once, before the listeners start.
    pub fn serve_registry(&self, scope: InboundScope) -> ApiResult<()> {
        let store = RegistryStore::open(self.app.root()).map_err(store_error)?;
        self.registry
            .set(Registry { scope, store })
            .map_err(|_| internal("the registry is already on"))
    }

    /// True if the listener of `listener` serves the registry.
    pub fn registry_serves(&self, listener: InboundScope) -> bool {
        self.registry
            .get()
            .is_some_and(|registry| reaches(registry.scope, listener))
    }

    fn registry_for(&self, listener: InboundScope) -> ApiResult<&Registry> {
        self.registry
            .get()
            .filter(|registry| reaches(registry.scope, listener))
            .ok_or_else(|| ApiError::NotFound("this listener serves no registry".into()))
    }

    /// The publisher domain of the registry's own entry on the listener of `listener`: the longest
    /// `--ard-publisher-domain` that is the listener host or a label suffix of it. `None` if the
    /// listener serves no registry or no domain fits.
    pub fn registry_publisher(&self, listener: InboundScope) -> Option<String> {
        if !self.registry_serves(listener) {
            return None;
        }
        let host = host_of(&self.outer_listener(listener)?.url)?;
        self.backends
            .ard_publishers()
            .into_iter()
            .filter(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
            .max_by_key(String::len)
    }

    /// The registry's own entry for the catalog of `listener` (§5.3): its base URL is the listener address.
    pub(crate) fn registry_entry(&self, listener: &OuterListener) -> Option<ArdEntry> {
        let domain = self.registry_publisher(listener.scope)?;
        Some(ArdEntry {
            context: Some(Value::from(bunsin_ard::BASE_CONTEXT)),
            identifier: format!("urn:air:{domain}:registry:bunsin"),
            display_name: "Bunsin ARD registry".into(),
            media_type: REGISTRY_TYPES[0].into(),
            url: Some(listener.url.clone()),
            description: Some("Searches the ARD entries that this server indexes.".into()),
            ..ArdEntry::default()
        })
    }

    /// Replaces the rows of each listener's own catalog with its current catalog. The registry task
    /// calls this on every tick. A listener that serves no registry is left out.
    pub fn refresh_own_catalogs(&self) -> ApiResult<()> {
        let Some(registry) = self.registry.get() else {
            return Ok(());
        };
        for listener in [InboundScope::Internal, InboundScope::Public] {
            if !reaches(registry.scope, listener) {
                continue;
            }
            let source = registry
                .store
                .source(self_source(listener), SourceKind::SelfCatalog, listener)
                .map_err(store_error)?;
            let entries = self
                .ard_catalog(listener)
                .manifest
                .entries
                .iter()
                .map(|entry| serde_json::to_value(entry).map_err(internal))
                .collect::<ApiResult<Vec<Value>>>()?;
            self.index_source(registry, source, &entries)?;
        }
        Ok(())
    }

    /// Stores the entries of one source in place of its earlier ones. An entry that breaks a MUST
    /// rule, is too large, or passes a limit is refused with a short reason. The caller checked
    /// the binding of the publisher domain, if the source needs one.
    fn index_source(&self, registry: &Registry, source: i64, entries: &[Value]) -> ApiResult<()> {
        let room = MAX_ENTRIES.saturating_sub(registry.store.entries_outside(source).map_err(store_error)?);
        let mut stored: Vec<NewEntry> = Vec::new();
        let mut refused: Vec<Refusal> = Vec::new();
        for value in entries {
            let identifier = value["identifier"].as_str().unwrap_or_default().to_owned();
            let checked = match (serde_json::from_value::<ArdEntry>(value.clone()), value.as_object()) {
                _ if value.to_string().len() > MAX_ENTRY_BYTES => Err("too_large"),
                (Ok(entry), Some(map)) if validate(&entry).iter().all(|v| v.level() != Level::Must) => Ok((entry, map)),
                _ => Err("invalid"),
            };
            let limit = if stored.iter().any(|known| known.identifier == identifier) {
                Some("duplicate")
            } else if stored.len() >= MAX_SOURCE_ENTRIES {
                Some("source_full")
            } else if stored.len() >= room {
                Some("index_full")
            } else {
                None
            };
            match (checked, limit) {
                (Ok((entry, map)), None) => stored.push(NewEntry {
                    publisher: urn_publisher(&entry.identifier)
                        .unwrap_or_default()
                        .to_ascii_lowercase(),
                    identifier,
                    display_name: entry.display_name,
                    media_type: entry.media_type,
                    terms: entry_terms(map),
                    search_text: search_text(map),
                    entry: value.clone(),
                }),
                (Err(reason), _) | (Ok(_), Some(reason)) => refused.push(Refusal {
                    identifier,
                    reason: reason.into(),
                }),
            }
        }
        registry
            .store
            .replace(source, &stored, &refused, self.now())
            .map_err(store_error)
    }

    /// The base URL of the listener of `listener`. Results name it as their `source`.
    fn registry_url(&self, listener: InboundScope) -> ApiResult<String> {
        self.outer_listener(listener)
            .map(|known| known.url)
            .ok_or_else(|| internal("the listener has no address"))
    }

    /// The rows that the query matches, with their score if it has text, best first (§5.3.3).
    /// Search and Explore share it, so they share the cutoff of `search::rank_scored`.
    fn matched(
        &self,
        registry: &Registry,
        listener: InboundScope,
        query: &QueryModel,
    ) -> ApiResult<Vec<(IndexedRow, Option<u8>)>> {
        let rows = registry.store.visible(listener).map_err(store_error)?;
        let ranked = match &query.text {
            None => rows.into_iter().map(|row| (row, None)).collect(),
            Some(text) => {
                let terms = query_terms(text);
                let ids: BTreeSet<i64> = rows.iter().map(|row| row.id).collect();
                // The SQL already keeps to the listener's rows. This also drops hidden duplicates.
                let hits: Vec<BTreeSet<i64>> = registry
                    .store
                    .term_hits(listener, &terms)
                    .map_err(store_error)?
                    .into_iter()
                    .map(|hit| hit.intersection(&ids).copied().collect())
                    .collect();
                let total = rows.len();
                let mut by_id: HashMap<i64, IndexedRow> = rows.into_iter().map(|row| (row.id, row)).collect();
                let order = rank_scored(total, &hits, |id| by_id[id].identifier.clone());
                order
                    .into_iter()
                    .filter_map(|(id, score)| by_id.remove(&id).map(|row| (row, Some(score))))
                    .collect::<Vec<_>>()
            }
        };
        Ok(ranked
            .into_iter()
            .filter(|(row, _)| query.matches(&row.terms, &row.publisher))
            .collect())
    }

    /// `POST /search` on the listener of `listener` (§5.3.2).
    pub fn registry_search(&self, listener: InboundScope, request: &SearchQuery) -> ApiResult<SearchAnswer> {
        let registry = self.registry_for(listener)?;
        let canonical = request.query.canonical();
        let offset = page_offset("search", &canonical, request.page_token.as_deref())?;
        let matched = self.matched(registry, listener, &request.query)?;
        let (rows, page_token) = page(matched, offset, request.page_size, |next| {
            page_token("search", &canonical, next)
        });
        let ids: Vec<i64> = rows.iter().map(|(row, _)| row.id).collect();
        let bodies = registry.store.bodies(&ids).map_err(store_error)?;
        let source = self.registry_url(listener)?;
        let results = bodies
            .into_iter()
            .zip(&rows)
            .map(|(body, (_, score))| {
                let mut item = shown(body);
                let score = score.expect("a search has text, so every matched row has a score");
                item.insert("score".into(), json!(score));
                item.insert("source".into(), json!(source));
                item
            })
            .collect();
        Ok(SearchAnswer { results, page_token })
    }

    /// `POST /explore` on the listener of `listener` (§5.3.3). The facets cover the whole matched set.
    pub fn registry_explore(&self, listener: InboundScope, request: &ExploreQuery) -> ApiResult<ExploreAnswer> {
        let registry = self.registry_for(listener)?;
        let matched = self.matched(registry, listener, &request.query)?;
        let facets = request
            .facets
            .iter()
            .map(|facet| {
                let rows = matched.iter().map(|(row, _)| (&row.terms, row.publisher.as_str()));
                (facet.field.clone(), facet.count(rows))
            })
            .collect();
        Ok(ExploreAnswer {
            result_type: "facets",
            facets,
        })
    }

    /// `GET /agents` on the listener of `listener` (§5.3.4). `createdAfter` compares with the time
    /// the registry first stored the entry, and `updatedAfter` with the time its stored entry changed.
    pub fn registry_list(&self, listener: InboundScope, request: &ListQuery) -> ApiResult<ListAnswer> {
        let registry = self.registry_for(listener)?;
        let offset = page_offset("agents", &request.canonical, request.page_token.as_deref())?;
        // The times are read once, so a wrong time is refused even if no row is visible.
        let times: Vec<Option<i64>> = request
            .clauses
            .iter()
            .map(|clause| match clause {
                ListClause::CreatedAfter(text) | ListClause::UpdatedAfter(text) => filter_time(text).map(Some),
                _ => Ok(None),
            })
            .collect::<ApiResult<_>>()?;
        let keep = |row: &IndexedRow| {
            request.clauses.iter().zip(&times).all(|(clause, time)| match clause {
                ListClause::DisplayName(texts) => {
                    let name = row.display_name.to_lowercase();
                    texts.iter().any(|text| name.contains(&text.to_lowercase()))
                }
                ListClause::Type(types) => types.contains(&row.media_type),
                ListClause::PublisherId(domains) => {
                    domains.iter().any(|domain| domain.eq_ignore_ascii_case(&row.publisher))
                }
                ListClause::CreatedAfter(_) => time.is_some_and(|after| row.first_seen_at > after),
                ListClause::UpdatedAfter(_) => time.is_some_and(|after| row.changed_at > after),
            })
        };
        let mut rows: Vec<IndexedRow> = registry
            .store
            .visible(listener)
            .map_err(store_error)?
            .into_iter()
            .filter(keep)
            .collect();
        let by_field = |a: &IndexedRow, b: &IndexedRow| -> Ordering {
            match request.order {
                ListField::DisplayName => a.display_name.to_lowercase().cmp(&b.display_name.to_lowercase()),
                ListField::CreatedAt => a.first_seen_at.cmp(&b.first_seen_at),
                ListField::UpdatedAt => a.changed_at.cmp(&b.changed_at),
                ListField::Identifier => Ordering::Equal,
            }
        };
        rows.sort_by(|a, b| {
            let order = by_field(a, b).then_with(|| a.identifier.cmp(&b.identifier));
            if request.descending { order.reverse() } else { order }
        });
        let total = rows.len() as u64;
        let (rows, page_token) = page(rows, offset, request.page_size, |next| {
            page_token("agents", &request.canonical, next)
        });
        let ids: Vec<i64> = rows.iter().map(|row| row.id).collect();
        let items = registry
            .store
            .bodies(&ids)
            .map_err(store_error)?
            .into_iter()
            .map(shown)
            .collect();
        Ok(ListAnswer {
            items,
            total,
            page_token,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_page_token_belongs_to_its_request_and_offset() {
        let token = page_token("search", "q", 10);
        assert_eq!(page_offset("search", "q", Some(&token)).unwrap(), 10);
        assert_eq!(page_offset("search", "q", None).unwrap(), 0);
        for (kind, canonical, token) in [
            ("search", "other", token.clone()),
            ("agents", "q", token.clone()),
            ("search", "q", "not-base64!".to_owned()),
            ("search", "q", URL_SAFE_NO_PAD.encode("10.0000000000000000")),
            ("search", "q", page_token("search", "q", MAX_OFFSET + 1)),
        ] {
            assert!(
                matches!(page_offset(kind, canonical, Some(&token)), Err(ApiError::Invalid(_))),
                "{kind} {canonical} {token}"
            );
        }
    }

    #[test]
    fn a_page_has_a_next_token_only_while_rows_remain_within_the_offset_limit() {
        let rows: Vec<usize> = (0..25).collect();
        let next = |offset: usize| offset.to_string();
        assert_eq!(page(rows.clone(), 0, 10, next), ((0..10).collect(), Some("10".into())));
        assert_eq!(page(rows.clone(), 20, 10, next), ((20..25).collect(), None));
        assert_eq!(page(rows.clone(), 15, 10, next), ((15..25).collect(), None));
        let many: Vec<usize> = (0..2_000).collect();
        assert_eq!(page(many.clone(), 990, 10, next).1, Some("1000".into()));
        assert_eq!(page(many, 1_000, 10, next).1, None);
    }

    #[test]
    fn a_list_time_is_rfc_3339_or_a_date() {
        assert_eq!(
            filter_time("2026-01-01").unwrap(),
            parse_utc("2026-01-01T00:00:00Z").unwrap()
        );
        assert_eq!(
            filter_time("2026-01-01T09:00:00+09:00").unwrap(),
            parse_utc("2026-01-01T00:00:00Z").unwrap()
        );
        for wrong in ["2026-1-1", "yesterday", "2026-02-30"] {
            assert!(filter_time(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_host_of_a_listener_address_is_its_lower_case_name() {
        assert_eq!(
            host_of("https://Agents.ACME.com:8443/").as_deref(),
            Some("agents.acme.com")
        );
        assert_eq!(host_of("https://agents.acme.com").as_deref(), Some("agents.acme.com"));
        assert_eq!(host_of("no address"), None);
    }
}
