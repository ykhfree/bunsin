//! Agentic Resource Discovery (ARD) for Bunsin.
//!
//! Bunsin takes three ARD roles (PLAN 4.6). It is a Publisher: it lists agent profiles in
//! `/.well-known/ard.json`. It is a Consumer: an agent searches the registries in its profile.
//! It is a Registry if the server operator turns it on: it indexes the server's own catalogs and
//! answers `/search`, `/explore` and `/agents` (§5.3). Crawling other sources is phase 2, not yet.
//!
//! The source is the ARD specification text, version [`SPEC_VERSION`], and its schema files
//! `ard-entry.schema.json` and `ard.openapi.yaml`. Comments cite the sections as `§n`.
//! If the specification body and a guide page or a schema file differ, this crate follows the specification body.
//! One difference is left: the body names the trust envelope `trustManifest`, and `ard-entry.schema.json`
//! names it `TrustManifest`. The crate reads and writes `trustManifest`. The schema allows other members,
//! so its `ArdEntry` check takes `trustManifest` without checking it. The catalog writes the trust
//! manifest that the owner gives, and does not verify its signature (§4.5.2).
//!
//! The test `tests/schema.rs` checks every entry of a catalog against a copy of `ard-entry.schema.json`,
//! and each `trustManifest` against its `TrustManifest` definition.

mod catalog;
mod entry;
mod html;
mod jsonld;
mod query;
mod resolve;
mod search;

pub use catalog::{Catalog, PublishedProfile, Refused, Scope, catalog, dns_record};
pub use entry::{
    A2A_AGENT_CARD_TYPE, AgentListing, ArdEntry, ArdManifest, BASE_CONTEXT, Built, Level, MCP_SERVER_CARD_TYPE,
    Protocol, TrustManifest, Violation, build_entry, urn_publisher, validate,
};
pub use query::{
    Constraint, ExploreAnswer, ExploreQuery, Facet, FacetAnswer, FilterKey, LIST_PAGE_SIZE, ListAnswer, ListClause,
    ListField, ListQuery, MAX_OFFSET, MAX_PAGE_SIZE, QueryError, QueryModel, SEARCH_PAGE_SIZE, SearchAnswer,
    SearchQuery, base_term_iri, entry_terms, parse_explore, parse_list, parse_list_filter, parse_search, values_at,
};
pub use search::{
    DeclineReason, Declined, ErrorCode, Failed, Federated, Federation, MAX_ERROR_MESSAGE_BYTES, MAX_RESPONSE_BYTES,
    Page, Query, REGISTRY_TYPES, Referral, SearchClient, SearchError, SearchRequest, SearchResponse, SearchResult,
    search_endpoint,
};

pub use search::{
    Bucket, ExploreRequest, ExploreResponse, FacetRequest, FacetResult, ListFilter, ListRequest, ListResponse,
    ResultType, get_document,
};

// The Consumer's resolution of a domain, shared with a crawl.
pub use html::{MAX_BLOCK_BYTES, MAX_BLOCKS, Markup, markup_entries, read_markup};
pub use jsonld::{ARD_NAMESPACE, BASE_CONTEXT_URL, Context, UnsupportedContext, expand_terms};
pub use resolve::{
    Accepted, Fetch, FetchError, Fetched, Found, MAX_ANNOUNCED, MAX_ENTRY_BYTES, RejectReason, Rejected, Resolution,
    Seed, Source, SourceError, accept_entry, resolve,
};

/// The ARD specification version that this crate implements.
pub const SPEC_VERSION: &str = "v0.91";
