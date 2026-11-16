//! The ARD entry (§4), the `ard.json` manifest (§5.1), the builder for one Bunsin agent profile,
//! and the validation of the entry rules.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::fmt;

/// The media type of an MCP Server Card (§3.3).
/// A Bunsin agent's inbound endpoint is an MCP server card (ARCHITECTURE 7).
pub const MCP_SERVER_CARD_TYPE: &str = "application/mcp-server-card+json";

/// One ARD entry (§4.2, schema `ArdEntry`).
///
/// Terms that this type does not name stay in `other` (§3.4: a consumer preserves the terms it
/// does not recognize). A missing `identifier`, `displayName` or `type` reads as an empty string,
/// so that [`validate`] can report it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArdEntry {
    #[serde(rename = "@context", default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Value>,
    #[serde(default)]
    pub identifier: String,
    #[serde(default)]
    pub display_name: String,
    /// The artifact type as a media type (§3.3).
    #[serde(rename = "type", default)]
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Map<String, Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub representative_queries: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trust_manifest: Option<TrustManifest>,
    /// Every other term, such as `tags`, `version`, `metadata` or a namespaced term.
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// The `trustManifest` envelope (§4.5). ARD reads only `identity`; the rest stays in `other`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TrustManifest {
    #[serde(default)]
    pub identity: String,
    #[serde(flatten)]
    pub other: Map<String, Value>,
}

/// The document at `/.well-known/ard.json` (§5.1, schema `ArdManifest`).
/// ARD ignores other top-level members, so this type drops them when it reads a manifest.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ArdManifest {
    pub entries: Vec<ArdEntry>,
}

/// The strength of a rule: MUST makes an entry invalid, SHOULD makes a warning (§D.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Must,
    Should,
}

/// One broken entry rule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Violation {
    /// The profile has no publisher domain, so the URN cannot be made (Appendix C).
    MissingPublisherDomain,
    /// A required term is missing or empty (§4.2).
    MissingTerm(&'static str),
    /// The identifier is not `urn:air:<publisher>:[<namespace>:]<agent-name>` (Appendix C).
    BadIdentifier(String),
    /// The `type` is not a syntactically valid media type (§3.3).
    BadMediaType(String),
    /// The `url` is not an absolute URI (schema `format: uri`).
    BadUrl(String),
    /// The entry has neither `url` nor `data` (§4.3).
    NeitherUrlNorData,
    /// The entry has both `url` and `data` (§4.3).
    BothUrlAndData,
    /// The domain of `trustManifest.identity` is not the URN publisher domain (§4.5.1).
    TrustIdentityMismatch { identity: String, publisher: String },
    /// Two profiles make the same identifier, which must be globally unique (§4.2).
    DuplicateIdentifier(String),
    /// `representativeQueries` does not hold 2 to 5 queries (§4.2, §D.2). Missing counts as 0.
    RepresentativeQueryCount(usize),
}

impl Violation {
    pub fn level(&self) -> Level {
        match self {
            Violation::RepresentativeQueryCount(_) => Level::Should,
            _ => Level::Must,
        }
    }
}

impl fmt::Display for Violation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Violation::MissingPublisherDomain => {
                f.write_str("the profile has no publisher domain, so it cannot be published (Appendix C)")
            }
            Violation::MissingTerm(term) => write!(f, "the entry has no `{term}` (§4.2)"),
            Violation::BadIdentifier(id) => write!(
                f,
                "`{id}` is not of the form urn:air:<publisher>:[<namespace>:]<agent-name> (Appendix C)"
            ),
            Violation::BadMediaType(t) => write!(f, "`{t}` is not a media type (§3.3)"),
            Violation::BadUrl(url) => write!(f, "`{url}` is not an absolute URL (§4.3)"),
            Violation::NeitherUrlNorData => f.write_str("the entry has neither `url` nor `data` (§4.3)"),
            Violation::BothUrlAndData => f.write_str("the entry has both `url` and `data` (§4.3)"),
            Violation::TrustIdentityMismatch { identity, publisher } => write!(
                f,
                "the trust identity `{identity}` does not name the publisher domain `{publisher}` (§4.5.1)"
            ),
            Violation::DuplicateIdentifier(id) => write!(f, "more than one profile uses the identifier `{id}` (§4.2)"),
            Violation::RepresentativeQueryCount(n) => {
                write!(
                    f,
                    "the entry has {n} representative queries; it should have 2 to 5 (§4.2)"
                )
            }
        }
    }
}

/// What a Bunsin agent profile gives to make its entry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentListing {
    /// The fully qualified domain name of the publisher, for example `acme.com`.
    pub publisher_domain: Option<String>,
    /// Optional namespace. It can hold more than one segment, separated by `:` (Appendix C).
    pub namespace: Option<String>,
    /// The terminal URN segment.
    pub agent_name: String,
    pub display_name: String,
    pub description: Option<String>,
    pub representative_queries: Vec<String>,
    /// The URL of the agent's inbound MCP server card.
    pub endpoint_url: String,
}

/// An entry that breaks no MUST rule, with the SHOULD rules it breaks.
#[derive(Debug, Clone, PartialEq)]
pub struct Built {
    pub entry: ArdEntry,
    pub warnings: Vec<Violation>,
}

/// Makes the entry for one agent profile.
///
/// The entry references the MCP server card by `url` (§4.3) and has no `data`.
/// Returns every violation if one or more of them is a MUST violation.
pub fn build_entry(listing: &AgentListing) -> Result<Built, Vec<Violation>> {
    let mut violations = Vec::new();
    let identifier = match listing.publisher_domain.as_deref() {
        Some(domain) if !domain.is_empty() => {
            let namespace = listing.namespace.as_deref().filter(|n| !n.is_empty());
            match namespace {
                Some(namespace) => format!("urn:air:{domain}:{namespace}:{}", listing.agent_name),
                None => format!("urn:air:{domain}:{}", listing.agent_name),
            }
        }
        _ => {
            violations.push(Violation::MissingPublisherDomain);
            String::new()
        }
    };
    let queries = &listing.representative_queries;
    let entry = ArdEntry {
        identifier,
        display_name: listing.display_name.clone(),
        media_type: MCP_SERVER_CARD_TYPE.to_owned(),
        url: Some(listing.endpoint_url.clone()),
        description: listing.description.clone(),
        representative_queries: (!queries.is_empty()).then(|| queries.clone()),
        ..ArdEntry::default()
    };
    violations.extend(validate(&entry));
    if violations.iter().any(|v| v.level() == Level::Must) {
        Err(violations)
    } else {
        Ok(Built {
            entry,
            warnings: violations,
        })
    }
}

/// Returns every MUST and SHOULD violation of one entry (§4.2, §4.3, §4.5.1, §D.2).
pub fn validate(entry: &ArdEntry) -> Vec<Violation> {
    let mut violations = Vec::new();
    let publisher = if entry.identifier.is_empty() {
        violations.push(Violation::MissingTerm("identifier"));
        None
    } else {
        let publisher = urn_publisher(&entry.identifier);
        if publisher.is_none() {
            violations.push(Violation::BadIdentifier(entry.identifier.clone()));
        }
        publisher
    };
    if entry.display_name.is_empty() {
        violations.push(Violation::MissingTerm("displayName"));
    }
    if entry.media_type.is_empty() {
        violations.push(Violation::MissingTerm("type"));
    } else if !is_media_type(&entry.media_type) {
        violations.push(Violation::BadMediaType(entry.media_type.clone()));
    }
    match (&entry.url, &entry.data) {
        (None, None) => violations.push(Violation::NeitherUrlNorData),
        (Some(_), Some(_)) => violations.push(Violation::BothUrlAndData),
        (Some(url), None) if url::Url::parse(url).is_err() => violations.push(Violation::BadUrl(url.clone())),
        _ => {}
    }
    if let Some(trust) = &entry.trust_manifest {
        if trust.identity.is_empty() {
            violations.push(Violation::MissingTerm("trustManifest.identity"));
        } else if let Some(publisher) = publisher {
            // §4.5.1: the trust domain MUST align with the URN publisher domain.
            // This crate reads "align" as equal, ignoring ASCII case.
            let aligned = identity_domain(&trust.identity).is_some_and(|d| d.eq_ignore_ascii_case(publisher));
            if !aligned {
                violations.push(Violation::TrustIdentityMismatch {
                    identity: trust.identity.clone(),
                    publisher: publisher.to_owned(),
                });
            }
        }
    }
    let count = entry.representative_queries.as_ref().map_or(0, Vec::len);
    if !(2..=5).contains(&count) {
        violations.push(Violation::RepresentativeQueryCount(count));
    }
    violations
}

/// Returns the `<publisher>` of a URN of the form `urn:air:<publisher>:[<namespace>:]<agent-name>`.
/// Returns `None` if the identifier does not have that form (Appendix C).
///
/// The publisher must be a fully qualified domain name. The schema pattern
/// `^urn:air:[a-zA-Z0-9.-]+(:[a-zA-Z0-9._-]+)+$` limits the other segments.
pub fn urn_publisher(identifier: &str) -> Option<&str> {
    let rest = identifier.strip_prefix("urn:air:")?;
    let mut segments = rest.split(':');
    let publisher = segments.next()?;
    let mut count = 0;
    for segment in segments {
        let valid = !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
        if !valid {
            return None;
        }
        count += 1;
    }
    // The agent name is required, so at least one segment follows the publisher.
    (count >= 1 && is_fqdn(publisher)).then_some(publisher)
}

/// Checks a domain name with at least two labels of letters, digits and inner hyphens.
fn is_fqdn(name: &str) -> bool {
    let labels: Vec<&str> = name.split('.').collect();
    name.len() <= 253
        && labels.len() >= 2
        && labels.iter().all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

/// Checks `type/subtype` with optional parameters, by the RFC 6838 section 4.2 name rules.
/// §3.3 accepts any syntactically valid media type, such as `text/markdown; profile="..."`.
fn is_media_type(value: &str) -> bool {
    let essence = value.split(';').next().unwrap_or_default().trim();
    let Some((top, sub)) = essence.split_once('/') else {
        return false;
    };
    is_restricted_name(top) && is_restricted_name(sub)
}

fn is_restricted_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 127
        && chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || "!#$&-^_.+".contains(c))
}

/// Returns the trust domain of a trust identity, if this crate can read one.
///
/// The schema names three forms: a SPIFFE ID (`spiffe://<trust-domain>/...`), a DID, and an
/// HTTPS FQDN URI. Of the DID methods, only `did:web` names a domain. Other forms give `None`.
fn identity_domain(identity: &str) -> Option<&str> {
    if let Some(rest) = identity.strip_prefix("did:web:") {
        // did:web encodes a port as `%3A` after the domain, and a path as more `:` segments.
        let domain = rest.split(':').next()?.split('%').next()?;
        return (!domain.is_empty()).then_some(domain);
    }
    let rest = identity
        .strip_prefix("spiffe://")
        .or_else(|| identity.strip_prefix("https://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    // A port or user information does not change the domain.
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then_some(host)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const PLAIN: &str = include_str!("../tests/fixtures/spec_4_4_plain_entry.json");
    const NAMESPACED: &str = include_str!("../tests/fixtures/spec_4_4_namespaced_entry.json");
    const SKILL: &str = include_str!("../tests/fixtures/spec_4_4_skill_entry.json");

    fn parse(text: &str) -> ArdEntry {
        serde_json::from_str(text).unwrap()
    }

    fn listing() -> AgentListing {
        AgentListing {
            publisher_domain: Some("acme.com".into()),
            namespace: Some("agents".into()),
            agent_name: "kim-helper".into(),
            display_name: "Kim's helper".into(),
            description: Some("Answers questions about the payments team.".into()),
            representative_queries: vec!["who owns the refund service".into(), "is the deploy frozen".into()],
            endpoint_url: "https://bunsin.acme.com/p/kim/mcp/server-card.json".into(),
        }
    }

    #[test]
    fn the_spec_example_entries_are_valid() {
        for text in [PLAIN, NAMESPACED, SKILL] {
            assert_eq!(validate(&parse(text)), vec![], "{text}");
        }
    }

    #[test]
    fn unknown_and_namespaced_terms_survive_a_round_trip() {
        let entry = parse(NAMESPACED);
        assert_eq!(entry.other["acme:serviceTier"], json!("enterprise"));
        assert_eq!(entry.context, Some(json!({"acme": "https://acme.com/vocab#"})));
        let original: Value = serde_json::from_str(NAMESPACED).unwrap();
        assert_eq!(serde_json::to_value(&entry).unwrap(), original);
    }

    #[test]
    fn the_builder_makes_an_mcp_server_card_entry() {
        let built = build_entry(&listing()).unwrap();
        assert_eq!(built.warnings, vec![]);
        assert_eq!(
            serde_json::to_value(&built.entry).unwrap(),
            json!({
                "identifier": "urn:air:acme.com:agents:kim-helper",
                "displayName": "Kim's helper",
                "type": "application/mcp-server-card+json",
                "url": "https://bunsin.acme.com/p/kim/mcp/server-card.json",
                "description": "Answers questions about the payments team.",
                "representativeQueries": ["who owns the refund service", "is the deploy frozen"]
            })
        );
    }

    #[test]
    fn the_builder_omits_an_empty_namespace_and_accepts_a_namespace_of_several_segments() {
        let mut no_namespace = listing();
        no_namespace.namespace = Some(String::new());
        assert_eq!(
            build_entry(&no_namespace).unwrap().entry.identifier,
            "urn:air:acme.com:kim-helper"
        );
        let mut deep = listing();
        deep.namespace = Some("finance:trading".into());
        assert_eq!(
            build_entry(&deep).unwrap().entry.identifier,
            "urn:air:acme.com:finance:trading:kim-helper"
        );
    }

    #[test]
    fn a_profile_without_a_publisher_domain_is_not_publishable() {
        for domain in [None, Some(String::new())] {
            let mut input = listing();
            input.publisher_domain = domain;
            let violations = build_entry(&input).unwrap_err();
            assert!(
                violations.contains(&Violation::MissingPublisherDomain),
                "{violations:?}"
            );
        }
    }

    #[test]
    fn the_builder_reports_every_violation_at_once() {
        let mut input = listing();
        input.agent_name = "kim helper".into();
        input.display_name = String::new();
        input.endpoint_url = "not a url".into();
        input.representative_queries = vec!["one".into()];
        assert_eq!(
            build_entry(&input).unwrap_err(),
            vec![
                Violation::BadIdentifier("urn:air:acme.com:agents:kim helper".into()),
                Violation::MissingTerm("displayName"),
                Violation::BadUrl("not a url".into()),
                Violation::RepresentativeQueryCount(1),
            ]
        );
    }

    #[test]
    fn a_wrong_query_count_is_a_warning_and_does_not_block_the_entry() {
        for count in [0, 1, 6] {
            let mut input = listing();
            input.representative_queries = (0..count).map(|i| format!("query {i}")).collect();
            let built = build_entry(&input).unwrap();
            assert_eq!(built.warnings, vec![Violation::RepresentativeQueryCount(count)]);
            assert_eq!(built.warnings[0].level(), Level::Should);
        }
        for count in [2, 5] {
            let mut input = listing();
            input.representative_queries = (0..count).map(|i| format!("query {i}")).collect();
            assert_eq!(build_entry(&input).unwrap().warnings, vec![]);
        }
    }

    #[test]
    fn url_and_data_are_exactly_one() {
        let mut both = parse(PLAIN);
        both.data = Some(Map::new());
        assert_eq!(validate(&both), vec![Violation::BothUrlAndData]);
        let mut neither = parse(PLAIN);
        neither.url = None;
        assert_eq!(validate(&neither), vec![Violation::NeitherUrlNorData]);
        let mut inline = parse(PLAIN);
        inline.url = None;
        inline.data = Some(Map::new());
        assert_eq!(validate(&inline), vec![]);
    }

    #[test]
    fn missing_required_terms_are_reported() {
        let entry: ArdEntry = serde_json::from_value(json!({
            "url": "https://api.acme.com/x.json",
            "representativeQueries": ["a", "b"]
        }))
        .unwrap();
        assert_eq!(
            validate(&entry),
            vec![
                Violation::MissingTerm("identifier"),
                Violation::MissingTerm("displayName"),
                Violation::MissingTerm("type"),
            ]
        );
    }

    #[test]
    fn the_urn_follows_appendix_c() {
        // Both examples from Appendix C, and the §4.4 skill entry.
        assert_eq!(urn_publisher("urn:air:example.com:weather-server"), Some("example.com"));
        assert_eq!(
            urn_publisher("urn:air:example.com:finance:trading:trader"),
            Some("example.com")
        );
        assert_eq!(
            urn_publisher("urn:air:github.com:alice-dev:pptx-creator"),
            Some("github.com")
        );
        for bad in [
            "urn:air:acme.com",               // no agent name
            "urn:air:acme.com:",              // empty agent name
            "urn:air:acme.com::helper",       // empty namespace segment
            "urn:air:localhost:helper",       // not a fully qualified domain name
            "urn:air:-acme.com:helper",       // a label starts with a hyphen
            "urn:air:acme.com:kim/helper",    // a character outside the schema pattern
            "urn:foo:acme.com:helper",        // another URN namespace
            "https://acme.com/agents/helper", // an arbitrary URI
        ] {
            assert_eq!(urn_publisher(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_type_must_be_a_media_type() {
        let mut entry = parse(PLAIN);
        entry.media_type = r#"text/markdown; profile="urn:air:agent-skills""#.into();
        assert_eq!(validate(&entry), vec![]);
        for bad in ["mcp", "application/", "/json", "application/mcp server"] {
            entry.media_type = bad.into();
            assert_eq!(validate(&entry), vec![Violation::BadMediaType(bad.into())], "{bad}");
        }
    }

    #[test]
    fn the_trust_identity_must_name_the_publisher_domain() {
        let with_identity = |identity: &str| {
            let mut entry = parse(PLAIN);
            entry.trust_manifest = Some(TrustManifest {
                identity: identity.into(),
                other: Map::new(),
            });
            validate(&entry)
        };
        for good in [
            "spiffe://acme.com/ns/prod/sa/weather",
            "spiffe://ACME.com/ns/prod",
            "https://acme.com/.well-known/identity",
            "https://acme.com:8443",
            "did:web:acme.com",
            "did:web:acme.com%3A8443:agents:weather",
        ] {
            assert_eq!(with_identity(good), vec![], "{good}");
        }
        for bad in [
            "spiffe://evil.com/ns/prod",
            "spiffe://prod.acme.com/ns/prod", // a subdomain is not the same domain
            "https://acme.com.evil.com/",
            "https://acme.com@evil.com/", // user information, not the host
            "did:key:z6MkhaXgBZDvotDkL5257faiztiGiC2QtKLGpbnnEGta2doK",
            "acme.com",
        ] {
            assert_eq!(
                with_identity(bad),
                vec![Violation::TrustIdentityMismatch {
                    identity: bad.into(),
                    publisher: "acme.com".into(),
                }],
                "{bad}"
            );
        }
        assert_eq!(
            with_identity(""),
            vec![Violation::MissingTerm("trustManifest.identity")]
        );
    }
}
