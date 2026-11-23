//! The `/.well-known/ard.json` body for one listener, and the DNS record that points at it (§5.1, PLAN 4.6).

use crate::entry::{AgentListing, ArdEntry, ArdManifest, Violation, build_entry};
use std::collections::HashMap;

/// Where an entry is listed, and which entries a listener serves.
///
/// A profile with `Internal` scope is listed only on the internal listener.
/// A profile with `Public` scope is listed on both listeners.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Scope {
    #[default]
    Internal,
    Public,
}

/// One entry of a profile with publishing on. A profile with A2A on gives two: one for each protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedProfile {
    pub scope: Scope,
    pub listing: AgentListing,
}

/// An entry that the catalog leaves out because it breaks a MUST rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub agent_name: String,
    pub violations: Vec<Violation>,
}

/// The manifest for one listener, and the profiles of that listener that it leaves out.
#[derive(Debug, Clone, PartialEq)]
pub struct Catalog {
    pub manifest: ArdManifest,
    pub refused: Vec<Refused>,
}

impl Catalog {
    /// The JSON body to serve at `/.well-known/ard.json`.
    pub fn body(&self) -> String {
        serde_json::to_string(&self.manifest).expect("an ARD manifest always serializes")
    }
}

/// Makes the catalog for the listener with scope `listener`.
///
/// The catalog checks identifier uniqueness over all entries of all profiles, so both listeners refuse
/// the same duplicates, also an A2A identifier that equals an MCP identifier. It reports only the
/// refused entries that the listener would list.
pub fn catalog(profiles: &[PublishedProfile], listener: Scope) -> Catalog {
    let built: Vec<(&PublishedProfile, Result<ArdEntry, Vec<Violation>>)> = profiles
        .iter()
        .map(|profile| (profile, build_entry(&profile.listing).map(|b| b.entry)))
        .collect();
    let mut uses: HashMap<&str, usize> = HashMap::new();
    for (_, result) in &built {
        if let Ok(entry) = result {
            *uses.entry(entry.identifier.as_str()).or_default() += 1;
        }
    }
    let mut out = Catalog {
        manifest: ArdManifest::default(),
        refused: Vec::new(),
    };
    for (profile, result) in &built {
        if listener == Scope::Public && profile.scope != Scope::Public {
            continue;
        }
        let refusal = match result {
            Ok(entry) if uses[entry.identifier.as_str()] > 1 => {
                vec![Violation::DuplicateIdentifier(entry.identifier.clone())]
            }
            Ok(entry) => {
                out.manifest.entries.push(entry.clone());
                continue;
            }
            Err(violations) => violations.clone(),
        };
        out.refused.push(Refused {
            agent_name: profile.listing.agent_name.clone(),
            violations: refusal,
        });
    }
    out
}

/// The DNS text that points at the catalog of a listener (§5.1): an SVCB record named
/// `_entries._agents.<publisher>.` whose target is the listener host, with its port if not 443.
///
/// SVCB holds a host and a port, not a path. So a listener URL with a path, or with an IP address
/// as host, gets a zone-file comment that says why it has no record.
pub fn dns_record(publisher: &str, listener_url: &str) -> String {
    let none = |why: String| format!("; no _entries._agents record for {listener_url}: {why}");
    let url = match url::Url::parse(listener_url) {
        Ok(url) => url,
        Err(error) => return none(format!("the URL cannot be read ({error})")),
    };
    let host = match url.host() {
        Some(url::Host::Domain(host)) => host,
        Some(_) => return none("the SVCB target must be a host name, not an IP address".into()),
        None => return none("the URL has no host".into()),
    };
    if url.path() != "/" {
        return none(format!("SVCB names a host and a port, not the path {}", url.path()));
    }
    let port = match url.port_or_known_default() {
        Some(443) | None => String::new(),
        Some(port) => format!(" port={port}"),
    };
    format!("_entries._agents.{publisher}. 3600 IN SVCB 1 {host}.{port}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::{Protocol, TrustManifest, validate};

    fn profile(name: &str, scope: Scope) -> PublishedProfile {
        PublishedProfile {
            scope,
            listing: AgentListing {
                publisher_domain: Some("acme.com".into()),
                namespace: None,
                agent_name: name.into(),
                display_name: name.into(),
                description: None,
                representative_queries: vec!["first question".into(), "second question".into()],
                endpoint_url: format!("https://bunsin.acme.com/{name}/server-card.json"),
                ..AgentListing::default()
            },
        }
    }

    /// The A2A entry of the same profile, as the engine makes it.
    fn a2a(profile: &PublishedProfile) -> PublishedProfile {
        let mut a2a = profile.clone();
        a2a.listing.protocol = Protocol::A2a;
        a2a.listing.endpoint_url = format!(
            "https://bunsin.acme.com/agents/{}/a2a/agent-card.json",
            profile.listing.agent_name
        );
        a2a
    }

    fn names(catalog: &Catalog) -> Vec<&str> {
        catalog.manifest.entries.iter().map(|e| e.identifier.as_str()).collect()
    }

    #[test]
    fn the_internal_listener_lists_internal_and_public_profiles() {
        let profiles = [profile("inner", Scope::Internal), profile("outer", Scope::Public)];
        let internal = catalog(&profiles, Scope::Internal);
        assert_eq!(names(&internal), ["urn:air:acme.com:inner", "urn:air:acme.com:outer"]);
        assert_eq!(internal.refused, vec![]);
    }

    #[test]
    fn the_public_listener_lists_only_public_profiles() {
        let profiles = [profile("inner", Scope::Internal), profile("outer", Scope::Public)];
        let public = catalog(&profiles, Scope::Public);
        assert_eq!(names(&public), ["urn:air:acme.com:outer"]);
        assert!(!public.body().contains("inner"), "{}", public.body());
    }

    #[test]
    fn the_default_scope_is_internal() {
        assert_eq!(Scope::default(), Scope::Internal);
    }

    #[test]
    fn a_profile_without_a_domain_is_refused_and_the_others_stay() {
        let mut lost = profile("lost", Scope::Public);
        lost.listing.publisher_domain = None;
        let profiles = [lost, profile("ok", Scope::Public)];
        let public = catalog(&profiles, Scope::Public);
        assert_eq!(names(&public), ["urn:air:acme.com:ok"]);
        assert_eq!(public.refused.len(), 1);
        assert_eq!(public.refused[0].agent_name, "lost");
        assert!(
            public.refused[0]
                .violations
                .contains(&Violation::MissingPublisherDomain)
        );
    }

    #[test]
    fn the_public_listener_does_not_report_internal_refusals() {
        let mut lost = profile("lost", Scope::Internal);
        lost.listing.publisher_domain = None;
        let profiles = [lost];
        assert_eq!(catalog(&profiles, Scope::Public).refused, vec![]);
        assert_eq!(catalog(&profiles, Scope::Internal).refused.len(), 1);
    }

    #[test]
    fn duplicate_identifiers_are_refused_on_both_listeners() {
        let profiles = [
            profile("same", Scope::Internal),
            profile("same", Scope::Public),
            profile("other", Scope::Public),
        ];
        let duplicate = Violation::DuplicateIdentifier("urn:air:acme.com:same".into());
        let internal = catalog(&profiles, Scope::Internal);
        assert_eq!(names(&internal), ["urn:air:acme.com:other"]);
        assert_eq!(internal.refused.len(), 2);
        let public = catalog(&profiles, Scope::Public);
        assert_eq!(names(&public), ["urn:air:acme.com:other"]);
        assert_eq!(
            public.refused,
            vec![Refused {
                agent_name: "same".into(),
                violations: vec![duplicate],
            }]
        );
    }

    #[test]
    fn the_body_reads_back_as_a_manifest_of_valid_entries() {
        let profiles = [profile("one", Scope::Public), profile("two", Scope::Public)];
        let body = catalog(&profiles, Scope::Internal).body();
        let manifest: ArdManifest = serde_json::from_str(&body).unwrap();
        assert_eq!(manifest.entries.len(), 2);
        for entry in &manifest.entries {
            assert_eq!(validate(entry), vec![]);
        }
    }

    #[test]
    fn no_profiles_make_an_empty_entries_array() {
        assert_eq!(catalog(&[], Scope::Public).body(), r#"{"entries":[]}"#);
    }

    #[test]
    fn a_profile_with_a2a_has_two_entries_on_the_listeners_that_list_it() {
        let inner = profile("inner", Scope::Internal);
        let outer = profile("outer", Scope::Public);
        let profiles = [inner.clone(), a2a(&inner), outer.clone(), a2a(&outer)];
        assert_eq!(
            names(&catalog(&profiles, Scope::Internal)),
            [
                "urn:air:acme.com:inner",
                "urn:air:acme.com:a2a:inner",
                "urn:air:acme.com:outer",
                "urn:air:acme.com:a2a:outer"
            ]
        );
        let public = catalog(&profiles, Scope::Public);
        assert_eq!(names(&public), ["urn:air:acme.com:outer", "urn:air:acme.com:a2a:outer"]);
        let types: Vec<&str> = public.manifest.entries.iter().map(|e| e.media_type.as_str()).collect();
        assert_eq!(
            types,
            ["application/mcp-server-card+json", "application/a2a-agent-card+json"]
        );
    }

    #[test]
    fn an_a2a_identifier_that_equals_an_mcp_identifier_refuses_both_holders() {
        let kim = profile("kim", Scope::Public);
        // Lee chose the namespace `a2a`, so Lee's MCP entry gets the identifier of Kim's A2A entry.
        let mut lee = profile("kim", Scope::Public);
        lee.listing.namespace = Some("a2a".into());
        let profiles = [kim.clone(), a2a(&kim), lee];
        let public = catalog(&profiles, Scope::Public);
        // Kim's MCP entry stays. The two holders of the shared identifier leave.
        assert_eq!(names(&public), ["urn:air:acme.com:kim"]);
        let duplicate = Violation::DuplicateIdentifier("urn:air:acme.com:a2a:kim".into());
        assert_eq!(public.refused.len(), 2);
        assert!(public.refused.iter().all(|r| r.violations == [duplicate.clone()]));
    }

    #[test]
    fn a_trust_manifest_of_another_domain_refuses_both_entries() {
        let mut kim = profile("kim", Scope::Public);
        kim.listing.trust_manifest = Some(TrustManifest {
            identity: "spiffe://evil.com/ns/agents/kim".into(),
            ..TrustManifest::default()
        });
        let mut lee = profile("lee", Scope::Public);
        lee.listing.trust_manifest = Some(TrustManifest {
            identity: "did:web:acme.com".into(),
            ..TrustManifest::default()
        });
        let profiles = [kim.clone(), a2a(&kim), lee.clone(), a2a(&lee)];
        let public = catalog(&profiles, Scope::Public);
        assert_eq!(names(&public), ["urn:air:acme.com:lee", "urn:air:acme.com:a2a:lee"]);
        assert_eq!(public.refused.len(), 2);
        for refused in &public.refused {
            assert_eq!(refused.agent_name, "kim");
            assert!(
                matches!(refused.violations[..], [Violation::TrustIdentityMismatch { .. }]),
                "{:?}",
                refused.violations
            );
        }
        // The listed entries carry the manifest.
        for entry in &public.manifest.entries {
            assert_eq!(entry.trust_manifest.as_ref().unwrap().identity, "did:web:acme.com");
        }
    }

    #[test]
    fn the_dns_record_names_the_publisher_and_the_listener_host() {
        assert_eq!(
            dns_record("acme.com", "https://agents.acme.com"),
            "_entries._agents.acme.com. 3600 IN SVCB 1 agents.acme.com."
        );
        assert_eq!(
            dns_record("acme.com", "https://agents.acme.com/"),
            "_entries._agents.acme.com. 3600 IN SVCB 1 agents.acme.com."
        );
        assert_eq!(
            dns_record("acme.com", "https://agents.acme.com:8443"),
            "_entries._agents.acme.com. 3600 IN SVCB 1 agents.acme.com. port=8443"
        );
        assert_eq!(
            dns_record("acme.com", "http://localhost:8788"),
            "_entries._agents.acme.com. 3600 IN SVCB 1 localhost. port=8788"
        );
    }

    #[test]
    fn a_listener_url_with_a_path_or_an_ip_address_gets_a_comment_and_no_record() {
        for (url, why) in [
            ("https://acme.com/agents", "not the path /agents"),
            ("https://acme.com/agents/", "not the path /agents/"),
            ("http://127.0.0.1:8788", "not an IP address"),
            ("https://[::1]", "not an IP address"),
            ("not a url", "cannot be read"),
        ] {
            let text = dns_record("acme.com", url);
            assert!(text.starts_with("; no _entries._agents record for "), "{text}");
            assert!(text.contains(why), "{text}");
            assert!(!text.contains("SVCB 1"), "{text}");
        }
    }
}
