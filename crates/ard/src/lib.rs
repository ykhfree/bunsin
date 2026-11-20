//! Agentic Resource Discovery (ARD) for Bunsin.
//!
//! Bunsin takes two ARD roles (PLAN 4.6). It is a Publisher: it lists agent profiles in
//! `/.well-known/ard.json`. It is a Consumer: an agent searches the registries in its profile.
//! It is not a Registry.
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
mod search;

pub use catalog::{Catalog, PublishedProfile, Refused, Scope, catalog, dns_record};
pub use entry::{
    A2A_AGENT_CARD_TYPE, AgentListing, ArdEntry, ArdManifest, BASE_CONTEXT, Built, Level, MCP_SERVER_CARD_TYPE,
    Protocol, TrustManifest, Violation, build_entry, urn_publisher, validate,
};
pub use search::{
    DeclineReason, Declined, ErrorCode, Failed, Federated, Federation, MAX_ERROR_MESSAGE_BYTES, MAX_RESPONSE_BYTES,
    Page, Query, REGISTRY_TYPES, Referral, SearchClient, SearchError, SearchRequest, SearchResponse, SearchResult,
    search_endpoint,
};

/// The ARD specification version that this crate implements.
pub const SPEC_VERSION: &str = "v0.91";
