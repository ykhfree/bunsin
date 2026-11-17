//! ARD publishing: the `/.well-known/ard.json` catalog of each listener for callers, and the
//! owner's view of where the endpoint and the webhooks are served and why a catalog leaves the endpoint out.
//!
//! A listener lists only the profiles that it serves (`InboundConfig::served_on`). Each entry points
//! at the server card on the same listener. Only a publisher domain that the server operator allows
//! can be published.

use crate::{Engine, store_error};
use bunsin_api::http::{ARD_CATALOG, hook_path, inbound_mcp_path, server_card_path};
use bunsin_api::{ApiError, ApiResult, ArdStatus, InboundEndpoint, InboundStatus, ProfileConfig, WebhookStatus};
use bunsin_ard::{AgentListing, Catalog, Level, PublishedProfile, Scope, Violation, build_entry, catalog};
use bunsin_core::inbound::{ArdListing, InboundScope};
use bunsin_core::webhook::hook_secret_key;

/// A placeholder address. The settings check builds an entry before any listener is known.
const PLACEHOLDER_URL: &str = "https://placeholder.invalid/server-card";

/// A listener for callers: who reaches it, and the address that callers use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OuterListener {
    pub scope: InboundScope,
    /// For example `https://agents.example.com`, without a path.
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

/// What the profile gives to make its entry. The display name is the card title, or the agent name.
fn listing(config: &ProfileConfig, ard: &ArdListing, endpoint_url: String) -> AgentListing {
    let inbound = &config.inbound;
    AgentListing {
        publisher_domain: Some(ard.publisher.trim().to_ascii_lowercase()),
        namespace: ard.namespace.clone(),
        agent_name: ard.agent_name.clone(),
        display_name: inbound.title.clone().unwrap_or_else(|| ard.agent_name.clone()),
        description: Some(inbound.description.trim().to_owned()).filter(|d| !d.is_empty()),
        representative_queries: ard.representative_queries.clone(),
        endpoint_url,
    }
}

impl Engine {
    /// Records a listener for callers. The catalogs and `inbound_status` name its addresses.
    /// A second listener of the same scope replaces the first.
    pub fn add_outer_listener(&self, listener: OuterListener) {
        let mut outer = self.outer.lock().expect("engine lock");
        outer.retain(|known| known.scope != listener.scope);
        outer.push(listener);
    }

    fn outer_listener(&self, scope: InboundScope) -> Option<OuterListener> {
        let outer = self.outer.lock().expect("engine lock");
        outer.iter().find(|listener| listener.scope == scope).cloned()
    }

    /// True if the server operator allows `publisher`.
    fn publisher_allowed(&self, publisher: &str) -> bool {
        let publisher = publisher.trim().to_ascii_lowercase();
        self.backends.ard_publishers().contains(&publisher)
    }

    /// Refuses an ARD listing whose entry breaks a MUST rule of ARD. A SHOULD rule, such as the
    /// count of representative queries, only gives a warning in `inbound_status`.
    pub(crate) fn check_ard(config: &ProfileConfig) -> ApiResult<()> {
        let Some(ard) = &config.inbound.ard else {
            return Ok(());
        };
        build_entry(&listing(config, ard, PLACEHOLDER_URL.to_owned()))
            .map(|_| ())
            .map_err(|violations| {
                let must: Vec<String> = violations
                    .iter()
                    .filter(|v| v.level() == Level::Must)
                    .map(Violation::to_string)
                    .collect();
                ApiError::Invalid(format!("`inbound.ard` makes no valid ARD entry: {}", must.join("; ")))
            })
    }

    /// The catalog of the listener of `scope`: every profile that this listener serves and that has an
    /// ARD listing with an allowed publisher. Profiles that cannot be read are left out.
    pub fn ard_catalog(&self, scope: InboundScope) -> Catalog {
        let Some(listener) = self.outer_listener(scope) else {
            return catalog(&[], ard_scope(scope));
        };
        let mut published = Vec::new();
        for profile in self.app.list_profiles().unwrap_or_default() {
            let Ok(config) = self.store(&profile.id).and_then(|store| Self::config(&store)) else {
                continue;
            };
            let Some(ard) = &config.inbound.ard else { continue };
            // The internal listener serves every endpoint that is on. The catalog gets all of them, so
            // the duplicate check covers all profiles; its scope rule leaves out what `served_on` leaves out.
            if !config.inbound.served_on(InboundScope::Internal) || !self.publisher_allowed(&ard.publisher) {
                continue;
            }
            let card = listener.url_of(&server_card_path(&profile.id));
            published.push(PublishedProfile {
                scope: ard_scope(config.inbound.scope),
                listing: listing(&config, ard, card),
            });
        }
        catalog(&published, ard_scope(scope))
    }

    pub(crate) fn read_inbound_status(&self, profile: &str) -> ApiResult<InboundStatus> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let inbound = &config.inbound;
        let mut endpoints = Vec::new();
        for scope in [InboundScope::Internal, InboundScope::Public] {
            if let Some(listener) = self.outer_listener(scope).filter(|_| inbound.served_on(scope)) {
                endpoints.push(InboundEndpoint {
                    listener: scope,
                    mcp_url: listener.url_of(&inbound_mcp_path(profile)),
                    card_url: listener.url_of(&server_card_path(profile)),
                    catalog_url: listener.url_of(ARD_CATALOG),
                });
            }
        }
        let ard = inbound
            .ard
            .as_ref()
            .map(|ard| self.ard_status(&config, ard, &endpoints));
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
        })
    }

    fn ard_status(&self, config: &ProfileConfig, ard: &ArdListing, endpoints: &[InboundEndpoint]) -> ArdStatus {
        let built = build_entry(&listing(config, ard, PLACEHOLDER_URL.to_owned()));
        let mut status = ArdStatus {
            identifier: built.as_ref().ok().map(|b| b.entry.identifier.clone()),
            listed_on: Vec::new(),
            refused: Vec::new(),
            warnings: Vec::new(),
        };
        match &built {
            Ok(built) => status.warnings = built.warnings.iter().map(Violation::to_string).collect(),
            Err(violations) => status.refused = violations.iter().map(Violation::to_string).collect(),
        }
        if !self.publisher_allowed(&ard.publisher) {
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
        let Some(identifier) = status.identifier.clone() else {
            return status;
        };
        for endpoint in endpoints {
            let catalog = self.ard_catalog(endpoint.listener);
            if catalog.manifest.entries.iter().any(|e| e.identifier == identifier) {
                status.listed_on.push(endpoint.listener);
            }
            let duplicate = Violation::DuplicateIdentifier(identifier.clone());
            let refused = catalog.refused.iter().any(|r| r.violations.contains(&duplicate));
            if refused && !status.refused.contains(&duplicate.to_string()) {
                status.refused.push(duplicate.to_string());
            }
        }
        status
    }
}
