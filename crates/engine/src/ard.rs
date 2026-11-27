//! ARD publishing: the `/.well-known/ard.json` catalog of each listener for callers, and the
//! owner's view of where the endpoint and the webhooks are served and why a catalog leaves an entry out.
//!
//! A listener lists only the profiles that it serves (`InboundConfig::served_on`). A profile has an
//! MCP entry, and an A2A entry if A2A is on: a listener that lists the profile also serves its Agent
//! Card (`InboundConfig::a2a_served_on`). Each entry points at a card on the same listener. Only a
//! publisher domain that the server operator allows can be published.

use crate::{Engine, store_error};
use bunsin_api::http::{ARD_CATALOG, agent_card_path, hook_path, inbound_a2a_path, inbound_mcp_path, server_card_path};
use bunsin_api::{ApiError, ApiResult, ArdStatus, InboundEndpoint, InboundStatus, ProfileConfig, WebhookStatus};
use bunsin_ard::{
    AgentListing, Catalog, Level, Protocol, PublishedProfile, Scope, TrustManifest, Violation, build_entry, catalog,
    dns_record,
};
use bunsin_core::inbound::{ArdListing, InboundScope};
use bunsin_core::time::utc_text;
use bunsin_core::webhook::hook_secret_key;
use bunsin_store::ProfileStore;
use serde_json::Value;

/// A placeholder address. The settings check builds an entry before any listener is known.
const PLACEHOLDER_URL: &str = "https://placeholder.invalid/server-card";
/// The profile setting with the time of the last save that changed `inbound`, as RFC 3339 text.
/// It is not part of the settings, so it never changes what a form loaded.
const UPDATED_AT_KEY: &str = "ard_updated_at";

/// A listener for callers: who reaches it, and the address that callers use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterListener {
    pub scope: InboundScope,
    /// For example `https://agents.example.com`.
    pub url: String,
}

impl OuterListener {
    fn url_of(&self, path: &str) -> String {
        format!("{}{path}", self.url.trim_end_matches('/'))
    }
}

fn ard_scope(scope: InboundScope) -> Scope {
    match scope {
        InboundScope::Internal => Scope::Internal,
        InboundScope::Public => Scope::Public,
    }
}

/// The publisher domain as the identifier writes it.
fn publisher(ard: &ArdListing) -> String {
    ard.publisher.trim().to_ascii_lowercase()
}

/// The trust manifest of the entries. A stored manifest without a text `identity` gets an empty
/// identity, so that `validate` refuses the entries instead of listing them without the manifest.
fn trust_manifest(ard: &ArdListing) -> Option<TrustManifest> {
    ard.trust_manifest.as_ref().map(|manifest| {
        let mut other = manifest.clone();
        let identity = match other.remove("identity") {
            Some(Value::String(identity)) => identity,
            _ => String::new(),
        };
        TrustManifest { identity, other }
    })
}

/// What the profile gives to make its entries: the MCP entry, and the A2A entry if A2A is on.
/// `card` gives the URL of the card of each protocol. The display name is the card title, or the agent name.
fn listings(
    config: &ProfileConfig,
    ard: &ArdListing,
    updated_at: Option<String>,
    card: impl Fn(Protocol) -> String,
) -> Vec<AgentListing> {
    let inbound = &config.inbound;
    let mcp = AgentListing {
        protocol: Protocol::Mcp,
        publisher_domain: Some(publisher(ard)),
        namespace: ard.namespace.clone(),
        agent_name: ard.agent_name.clone(),
        display_name: inbound.title.clone().unwrap_or_else(|| ard.agent_name.clone()),
        description: Some(inbound.description.trim().to_owned()).filter(|d| !d.is_empty()),
        representative_queries: ard.representative_queries.clone(),
        endpoint_url: card(Protocol::Mcp),
        tags: ard.tags.clone(),
        capabilities: Vec::new(),
        // The same version as the server card: all crates share the workspace version.
        version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        updated_at,
        trust_manifest: trust_manifest(ard),
    };
    // The A2A entry names the skills of the card. Without skills of its own, the card has `ask`.
    let a2a = inbound.a2a.then(|| AgentListing {
        protocol: Protocol::A2a,
        endpoint_url: card(Protocol::A2a),
        capabilities: inbound.skills.iter().map(|skill| skill.id.clone()).collect(),
        ..mcp.clone()
    });
    std::iter::once(mcp).chain(a2a).collect()
}

fn placeholder(_: Protocol) -> String {
    PLACEHOLDER_URL.to_owned()
}

impl Engine {
    /// Records a listener for callers. The catalogs and `inbound_status` name its addresses.
    /// A second listener of the same scope replaces the first.
    pub fn add_outer_listener(&self, listener: OuterListener) {
        let mut outer = self.outer.lock().expect("engine lock");
        outer.retain(|known| known.scope != listener.scope);
        outer.push(listener);
    }

    pub(crate) fn outer_listener(&self, scope: InboundScope) -> Option<OuterListener> {
        let outer = self.outer.lock().expect("engine lock");
        outer.iter().find(|listener| listener.scope == scope).cloned()
    }

    /// True if the server operator allows `publisher`.
    fn publisher_allowed(&self, publisher: &str) -> bool {
        let publisher = publisher.trim().to_ascii_lowercase();
        self.backends.ard_publishers().contains(&publisher)
    }

    /// Refuses an ARD listing whose entries break a MUST rule of ARD, such as a trust identity of
    /// another domain (§4.5.1). A SHOULD rule, such as the count of representative queries, only
    /// gives a warning in `inbound_status`.
    pub(crate) fn check_ard(config: &ProfileConfig) -> ApiResult<()> {
        let Some(ard) = &config.inbound.ard else {
            return Ok(());
        };
        for listing in listings(config, ard, None, placeholder) {
            if let Err(violations) = build_entry(&listing) {
                let must: Vec<String> = violations
                    .iter()
                    .filter(|v| v.level() == Level::Must)
                    .map(Violation::to_string)
                    .collect();
                return Err(ApiError::Invalid(format!(
                    "`inbound.ard` makes no valid ARD entry: {}",
                    must.join("; ")
                )));
            }
        }
        Ok(())
    }

    /// Stores the time of a save that changed `inbound`. The catalogs write it as `updatedAt`.
    /// The registry then indexes the own catalogs again, so a profile that left a listener leaves
    /// its registry at once.
    pub(crate) fn stamp_inbound(
        &self,
        store: &ProfileStore,
        before: &ProfileConfig,
        after: &ProfileConfig,
    ) -> ApiResult<()> {
        if before.inbound == after.inbound {
            return Ok(());
        }
        store
            .set_setting(UPDATED_AT_KEY, &utc_text(self.now()))
            .map_err(store_error)?;
        // The save is done, so a failed refresh does not fail it. The registry task refreshes again
        // within a minute and reports its own failure.
        let _ = self.refresh_own_catalogs();
        Ok(())
    }

    /// The catalog of the listener of `scope`: the entries of every profile that this listener serves
    /// and that has an ARD listing with an allowed publisher. Profiles that cannot be read are left out.
    /// A listener that serves the registry lists it too, unless a profile already uses its identifier.
    pub fn ard_catalog(&self, scope: InboundScope) -> Catalog {
        let Some(listener) = self.outer_listener(scope) else {
            return catalog(&[], ard_scope(scope));
        };
        let mut published = Vec::new();
        for profile in self.app.list_profiles().unwrap_or_default() {
            let Ok(store) = self.store(&profile.id) else { continue };
            let (Ok(config), Ok(updated_at)) = (Self::config(&store), store.setting(UPDATED_AT_KEY)) else {
                continue;
            };
            let Some(ard) = &config.inbound.ard else { continue };
            // The internal listener serves every endpoint that is on. The catalog gets all of them, so
            // the duplicate check covers all profiles; its scope rule leaves out what `served_on` leaves out.
            if !config.inbound.served_on(InboundScope::Internal) || !self.publisher_allowed(&ard.publisher) {
                continue;
            }
            let card = |protocol| {
                listener.url_of(&match protocol {
                    Protocol::Mcp => server_card_path(&profile.id),
                    Protocol::A2a => agent_card_path(&profile.id),
                })
            };
            let scope = ard_scope(config.inbound.scope);
            published.extend(
                listings(&config, ard, updated_at, card)
                    .into_iter()
                    .map(|listing| PublishedProfile { scope, listing }),
            );
        }
        let mut out = catalog(&published, ard_scope(scope));
        if let Some(entry) = self.registry_entry(&listener)
            && !out
                .manifest
                .entries
                .iter()
                .any(|known| known.identifier == entry.identifier)
        {
            out.manifest.entries.push(entry);
        }
        out
    }

    pub(crate) fn read_inbound_status(&self, profile: &str) -> ApiResult<InboundStatus> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let inbound = &config.inbound;
        let mut endpoints = Vec::new();
        for scope in [InboundScope::Internal, InboundScope::Public] {
            if let Some(listener) = self.outer_listener(scope).filter(|_| inbound.served_on(scope)) {
                let a2a = inbound.a2a_served_on(scope);
                endpoints.push(InboundEndpoint {
                    listener: scope,
                    mcp_url: listener.url_of(&inbound_mcp_path(profile)),
                    card_url: listener.url_of(&server_card_path(profile)),
                    catalog_url: listener.url_of(ARD_CATALOG),
                    a2a_url: a2a.then(|| listener.url_of(&inbound_a2a_path(profile))),
                    agent_card_url: a2a.then(|| listener.url_of(&agent_card_path(profile))),
                });
            }
        }
        let (ard, dns_records) = match &inbound.ard {
            Some(listing) => {
                let ard = self.ard_statuses(&config, listing, &endpoints);
                let dns_records = self.dns_records(listing, &ard);
                (ard, dns_records)
            }
            None => (Vec::new(), Vec::new()),
        };
        let mut webhooks = Vec::new();
        for (name, hook) in &config.webhooks {
            let urls = [InboundScope::Internal, InboundScope::Public]
                .into_iter()
                .filter(|scope| hook.served_on(*scope))
                .filter_map(|scope| self.outer_listener(scope))
                .map(|listener| listener.url_of(&hook_path(profile, name)))
                .collect();
            webhooks.push(WebhookStatus {
                name: name.clone(),
                connector: hook.connector.clone(),
                urls,
                secret_stored: store.secret(&hook_secret_key(name)).map_err(store_error)?.is_some(),
            });
        }
        Ok(InboundStatus {
            enabled: inbound.enabled,
            scope: inbound.scope,
            endpoints,
            ard,
            webhooks,
            dns_records,
        })
    }

    /// The status of each entry of the listing, in the order MCP, A2A.
    fn ard_statuses(&self, config: &ProfileConfig, ard: &ArdListing, endpoints: &[InboundEndpoint]) -> Vec<ArdStatus> {
        let catalogs: Vec<(InboundScope, Catalog)> = endpoints
            .iter()
            .map(|endpoint| (endpoint.listener, self.ard_catalog(endpoint.listener)))
            .collect();
        let allowed = self.publisher_allowed(&ard.publisher);
        let mut statuses = Vec::new();
        for listing in listings(config, ard, None, placeholder) {
            let built = build_entry(&listing);
            let mut status = ArdStatus {
                identifier: built.as_ref().ok().map(|b| b.entry.identifier.clone()),
                media_type: listing.protocol.media_type().to_owned(),
                listed_on: Vec::new(),
                refused: Vec::new(),
                warnings: Vec::new(),
            };
            match &built {
                Ok(built) => status.warnings = built.warnings.iter().map(Violation::to_string).collect(),
                Err(violations) => status.refused = violations.iter().map(Violation::to_string).collect(),
            }
            if !allowed {
                status.refused.push(format!(
                    "the server operator does not allow the publisher domain `{}` (--ard-publisher-domain)",
                    ard.publisher
                ));
            }
            if endpoints.is_empty() {
                status
                    .refused
                    .push("no listener for callers serves the endpoint of this profile".into());
            }
            if let Some(identifier) = status.identifier.clone() {
                for (listener, catalog) in &catalogs {
                    if catalog.manifest.entries.iter().any(|e| e.identifier == identifier) {
                        status.listed_on.push(*listener);
                    }
                    let duplicate = Violation::DuplicateIdentifier(identifier.clone());
                    let refused = catalog.refused.iter().any(|r| r.violations.contains(&duplicate));
                    if refused && !status.refused.contains(&duplicate.to_string()) {
                        status.refused.push(duplicate.to_string());
                    }
                }
            }
            statuses.push(status);
        }
        statuses
    }

    /// The DNS text for each listener whose catalog lists an entry of this profile.
    fn dns_records(&self, ard: &ArdListing, statuses: &[ArdStatus]) -> Vec<String> {
        let mut records = Vec::new();
        for scope in [InboundScope::Internal, InboundScope::Public] {
            if !statuses.iter().any(|status| status.listed_on.contains(&scope)) {
                continue;
            }
            let Some(listener) = self.outer_listener(scope) else {
                continue;
            };
            let record = dns_record(&publisher(ard), &listener.url);
            // Two listeners behind one host and port need one record.
            if !records.contains(&record) {
                records.push(record);
            }
        }
        records
    }
}
