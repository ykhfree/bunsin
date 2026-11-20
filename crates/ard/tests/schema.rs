//! The catalog that `catalog(..)` builds, checked against the JSON schema of the ARD specification.
//! The schema is a copy; `fixtures/README.md` names its source.

use bunsin_ard::{AgentListing, ArdEntry, Level, Protocol, PublishedProfile, Scope, TrustManifest, catalog, validate};
use serde_json::{Value, json};

const SCHEMA: &str = include_str!("fixtures/ard-entry.schema.json");

/// The schema errors of `instance` against one definition of the schema: `ArdEntry`, `ArdManifest`
/// or `TrustManifest`.
/// Formats such as `uri` are checked too.
fn schema_errors(definition: &str, instance: &Value) -> Vec<String> {
    let mut schema: Value = serde_json::from_str(SCHEMA).unwrap();
    schema["$ref"] = json!(format!("#/$defs/{definition}"));
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    validator.iter_errors(instance).map(|error| error.to_string()).collect()
}

fn listing(agent_name: &str, queries: &[&str]) -> AgentListing {
    AgentListing {
        publisher_domain: Some("acme.com".into()),
        namespace: None,
        agent_name: agent_name.into(),
        display_name: agent_name.into(),
        description: None,
        representative_queries: queries.iter().map(|q| (*q).to_owned()).collect(),
        endpoint_url: format!("https://bunsin.acme.com/agents/{agent_name}/mcp/server-card"),
        version: Some("0.1.0".into()),
        ..AgentListing::default()
    }
}

/// A trust manifest with every member that ARD names.
fn full_manifest(identity: &str) -> TrustManifest {
    let other = json!({
        "identityType": "spiffe",
        "trustSchema": {"identifier": "acme-trust", "version": "1", "governanceUri": "https://acme.com/trust",
                        "verificationMethods": ["x509-svid"]},
        "attestations": [{"type": "SOC2-Type2", "uri": "https://acme.com/soc2.pdf", "mediaType": "application/pdf",
                          "digest": "sha256:0f"}],
        "provenance": [{"relation": "publishedFrom", "sourceId": "urn:air:acme.com:team:kim"}],
        "signature": "eyJhbGciOiJFUzI1NiJ9"
    });
    TrustManifest {
        identity: identity.into(),
        other: other.as_object().unwrap().clone(),
    }
}

/// Profiles that use every field of a listing, and the edge values that the builder still lists.
fn profiles() -> Vec<PublishedProfile> {
    let mut namespaced = listing("kim", &["When do we deploy?", "Who reviews a release?"]);
    namespaced.namespace = Some("team".into());
    namespaced.display_name = "김 비서 (Kim's assistant)".into();
    namespaced.description = Some("Questions about deploys and reviews.".into());
    namespaced.tags = vec!["deploys".into(), "릴리스".into()];
    namespaced.updated_at = Some("2026-11-20T08:15:00Z".into());
    namespaced.trust_manifest = Some(full_manifest("spiffe://acme.com/ns/team/kim"));
    let mut odd_name = listing("lee_bot.v2-beta", &["a", "b", "c", "d", "e"]);
    odd_name.endpoint_url = "http://127.0.0.1:8787/agents/lee/mcp/server-card".into();
    let mut sub_domain = listing("park", &["Is the build green?", "Who is on call?"]);
    sub_domain.publisher_domain = Some("ops.eu-1.acme.co.kr".into());
    sub_domain.trust_manifest = Some(TrustManifest {
        identity: "did:web:ops.eu-1.acme.co.kr".into(),
        ..TrustManifest::default()
    });
    // The A2A entries of two profiles, as the engine makes them.
    let a2a = |of: &AgentListing| AgentListing {
        protocol: Protocol::A2a,
        endpoint_url: format!("https://bunsin.acme.com/agents/{}/a2a/agent-card.json", of.agent_name),
        ..of.clone()
    };
    let namespaced_a2a = a2a(&namespaced);
    let sub_domain_a2a = a2a(&sub_domain);
    // A count of queries outside 2 to 5 is a warning, so these entries are listed.
    let one_query = listing("choi", &["Where are the release notes?"]);
    let no_query = listing("jung", &[]);
    [
        (namespaced, Scope::Internal),
        (namespaced_a2a, Scope::Internal),
        (odd_name, Scope::Public),
        (sub_domain, Scope::Public),
        (sub_domain_a2a, Scope::Public),
        (one_query, Scope::Internal),
        (no_query, Scope::Public),
    ]
    .into_iter()
    .map(|(listing, scope)| PublishedProfile { scope, listing })
    .collect()
}

#[test]
fn every_entry_of_both_catalogs_conforms_to_the_schema() {
    let profiles = profiles();
    // Entries, and the entries with a trust manifest.
    for (listener, expected, trusted) in [(Scope::Internal, 7, 4), (Scope::Public, 4, 2)] {
        let catalog = catalog(&profiles, listener);
        assert!(catalog.refused.is_empty(), "{:?}", catalog.refused);
        let body: Value = serde_json::from_str(&catalog.body()).unwrap();
        let entries = body["entries"].as_array().unwrap();
        // Without entries the check below would pass for nothing.
        assert_eq!(entries.len(), expected);
        assert_eq!(schema_errors("ArdManifest", &body), Vec::<String>::new());
        for entry in entries {
            assert_eq!(schema_errors("ArdEntry", entry), Vec::<String>::new(), "{entry}");
        }
        // The schema names the property `TrustManifest`, so the `ArdEntry` check above skips
        // `trustManifest`. Each one is checked against its own definition.
        let manifests: Vec<&Value> = entries.iter().filter_map(|e| e.get("trustManifest")).collect();
        assert_eq!(manifests.len(), trusted, "{body}");
        for manifest in manifests {
            assert_eq!(
                schema_errors("TrustManifest", manifest),
                Vec::<String>::new(),
                "{manifest}"
            );
        }
    }
}

#[test]
fn the_catalog_carries_the_descriptive_terms_in_their_schema_formats() {
    let body: Value = serde_json::from_str(&catalog(&profiles(), Scope::Internal).body()).unwrap();
    let kim = &body["entries"][0];
    assert_eq!(kim["@context"], "https://agenticresourcediscovery.org/context/v1");
    assert_eq!(kim["tags"], json!(["deploys", "릴리스"]));
    assert_eq!(kim["updatedAt"], "2026-11-20T08:15:00Z");
    assert_eq!(kim["version"], "0.1.0");
    assert_eq!(kim["capabilities"], json!(["ask", "get_request"]));
    let kim_a2a = &body["entries"][1];
    assert_eq!(kim_a2a["identifier"], "urn:air:acme.com:team:a2a:kim");
    assert_eq!(kim_a2a["capabilities"], json!(["ask"]));
    assert_eq!(kim_a2a["trustManifest"], kim["trustManifest"]);
    // `updatedAt` has `format: date-time`, so text that is not one fails the schema.
    let mut bad_time = kim.clone();
    bad_time["updatedAt"] = json!("20 Nov 2026");
    assert!(!schema_errors("ArdEntry", &bad_time).is_empty());
}

#[test]
fn a_trust_manifest_that_breaks_its_definition_fails_the_schema() {
    let good = serde_json::to_value(full_manifest("spiffe://acme.com/ns/team/kim")).unwrap();
    assert_eq!(schema_errors("TrustManifest", &good), Vec::<String>::new());
    let mut cases: Vec<(&str, Value)> = Vec::new();
    let mut no_identity = good.clone();
    no_identity.as_object_mut().unwrap().remove("identity");
    cases.push(("no identity", no_identity));
    let mut no_uri = good.clone();
    no_uri["attestations"][0].as_object_mut().unwrap().remove("uri");
    cases.push(("an attestation without uri", no_uri));
    let mut relative = good.clone();
    relative["trustSchema"]["governanceUri"] = json!("trust policy");
    cases.push(("a governance URI that is not a URI", relative));
    let mut no_source = good.clone();
    no_source["provenance"][0].as_object_mut().unwrap().remove("sourceId");
    cases.push(("a provenance link without sourceId", no_source));
    for (what, manifest) in cases {
        assert!(
            !schema_errors("TrustManifest", &manifest).is_empty(),
            "the schema takes {what}"
        );
    }
}

#[test]
fn an_empty_catalog_conforms_to_the_schema() {
    let body: Value = serde_json::from_str(&catalog(&[], Scope::Public).body()).unwrap();
    assert_eq!(body, json!({"entries": []}));
    assert_eq!(schema_errors("ArdManifest", &body), Vec::<String>::new());
}

/// A listed entry, as JSON, that the tests below break in one place each.
fn listed_entry() -> Value {
    let catalog = catalog(&profiles(), Scope::Internal);
    serde_json::to_value(&catalog.manifest.entries[0]).unwrap()
}

#[test]
fn entries_that_break_a_must_rule_fail_the_schema_and_the_crate_check_alike() {
    let mut cases: Vec<(&str, Value)> = Vec::new();
    let mut both = listed_entry();
    both["data"] = json!({"name": "kim"});
    cases.push(("both url and data", both));
    let mut neither = listed_entry();
    neither.as_object_mut().unwrap().remove("url");
    cases.push(("neither url nor data", neither));
    let mut no_type = listed_entry();
    no_type.as_object_mut().unwrap().remove("type");
    cases.push(("no type", no_type));
    let mut no_name = listed_entry();
    no_name.as_object_mut().unwrap().remove("displayName");
    cases.push(("no display name", no_name));
    for identifier in [
        "urn:air:acme.com",
        "urn:air:acme.com:kim/helper",
        "urn:agent:acme.com:kim",
    ] {
        let mut bad = listed_entry();
        bad["identifier"] = json!(identifier);
        cases.push((identifier, bad));
    }
    let mut relative = listed_entry();
    relative["url"] = json!("/agents/kim/mcp/server-card");
    cases.push(("a relative url", relative));

    for (what, entry) in cases {
        assert!(!schema_errors("ArdEntry", &entry).is_empty(), "the schema takes {what}");
        // A field that the crate reads as empty counts as missing, as in the schema.
        let parsed: ArdEntry = serde_json::from_value(entry).unwrap();
        let must = validate(&parsed)
            .into_iter()
            .any(|violation| violation.level() == Level::Must);
        assert!(must, "the crate takes {what}");
    }
}
