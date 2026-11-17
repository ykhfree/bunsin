//! The catalog that `catalog(..)` builds, checked against the JSON schema of the ARD specification.
//! The schema is a copy; `fixtures/README.md` names its source.

use bunsin_ard::{AgentListing, ArdEntry, Level, PublishedProfile, Scope, catalog, validate};
use serde_json::{Value, json};

const SCHEMA: &str = include_str!("fixtures/ard-entry.schema.json");

/// The schema errors of `instance` against one definition of the schema: `ArdEntry` or `ArdManifest`.
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
    }
}

/// Profiles that use every field of a listing, and the edge values that the builder still lists.
fn profiles() -> Vec<PublishedProfile> {
    let mut namespaced = listing("kim", &["When do we deploy?", "Who reviews a release?"]);
    namespaced.namespace = Some("team".into());
    namespaced.display_name = "김 비서 (Kim's assistant)".into();
    namespaced.description = Some("Questions about deploys and reviews.".into());
    let mut odd_name = listing("lee_bot.v2-beta", &["a", "b", "c", "d", "e"]);
    odd_name.endpoint_url = "http://127.0.0.1:8787/agents/lee/mcp/server-card".into();
    let mut sub_domain = listing("park", &["Is the build green?", "Who is on call?"]);
    sub_domain.publisher_domain = Some("ops.eu-1.acme.co.kr".into());
    // A count of queries outside 2 to 5 is a warning, so these entries are listed.
    let one_query = listing("choi", &["Where are the release notes?"]);
    let no_query = listing("jung", &[]);
    [
        (namespaced, Scope::Internal),
        (odd_name, Scope::Public),
        (sub_domain, Scope::Public),
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
    for (listener, expected) in [(Scope::Internal, 5), (Scope::Public, 3)] {
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
