//! The `/.well-known/ard.json` body for one listener (§5.1, PLAN 4.6).

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

/// One profile with publishing on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedProfile {
    pub scope: Scope,
    pub listing: AgentListing,
}

/// A profile that the catalog leaves out because its entry breaks a MUST rule.
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
/// The catalog checks identifier uniqueness over all profiles, so both listeners refuse the same
/// duplicates. It reports only the refused profiles that the listener would list.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::validate;

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
            },
        }
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
}
