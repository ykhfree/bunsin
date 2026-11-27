//! ARD publishing in the engine: the A2A entry, `updatedAt`, the trust manifest and the owner's status.
//! A real engine on a temporary directory. The operator allows the publisher domain `acme.com`.

use bunsin_api::{ApiError, EngineApi, Expected, InboundUpdate, NotesSettingsUpdate, ProfileConfig};
use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope, InboundSkill};
use bunsin_core::ports::BoxFuture;
use bunsin_core::time::utc_text;
use bunsin_engine::{Backends, Engine, OuterListener, Runtime};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// The ARD paths need no LLM and no connector.
struct Operator;

impl Backends for Operator {
    fn build<'a>(&'a self, _: &'a str, _: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async { Err("these tests build no runtime".to_owned()) })
    }

    fn ard_publishers(&self) -> Vec<String> {
        vec!["acme.com".into()]
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    clock: Arc<AtomicI64>,
    engine: Engine,
}

const START: i64 = 1_700_000_000;

/// An engine with profile `kim` and an internal listener at `https://agents.acme.com`.
async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let clock = Arc::new(AtomicI64::new(START));
    let time = clock.clone();
    let engine = Engine::open(
        dir.path(),
        Arc::new(Operator),
        Arc::new(move || time.load(Ordering::SeqCst)),
    )
    .unwrap();
    engine.create_profile("kim", ProfileConfig::new("Kim")).await.unwrap();
    engine.add_outer_listener(OuterListener {
        scope: InboundScope::Internal,
        url: "https://agents.acme.com".into(),
    });
    Rig {
        _dir: dir,
        clock,
        engine,
    }
}

fn inbound(agent: &str, namespace: Option<&str>, a2a: bool) -> InboundConfig {
    InboundConfig {
        enabled: true,
        description: "Questions about the deploy schedule.".into(),
        a2a,
        ard: Some(ArdListing {
            publisher: "acme.com".into(),
            namespace: namespace.map(str::to_owned),
            agent_name: agent.into(),
            representative_queries: vec!["When do we deploy?".into(), "Who reviews?".into()],
            ..ArdListing::default()
        }),
        ..InboundConfig::default()
    }
}

async fn save(rig: &Rig, inbound: InboundConfig) -> Result<(), ApiError> {
    let update = InboundUpdate {
        inbound,
        expected: Expected::Unchecked,
    };
    rig.engine.set_inbound_settings("kim", update).await
}

/// The entries of the internal catalog as JSON.
fn entries(rig: &Rig) -> Vec<Value> {
    let body: Value = serde_json::from_str(&rig.engine.ard_catalog(InboundScope::Internal).body()).unwrap();
    body["entries"].as_array().unwrap().clone()
}

fn identifiers(rig: &Rig) -> Vec<String> {
    entries(rig)
        .iter()
        .map(|entry| entry["identifier"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn a2a_on_lists_two_entries_and_a_duplicate_a2a_identifier_refuses_both() {
    let rig = rig().await;
    save(&rig, inbound("kim", None, true)).await.unwrap();
    let listed = entries(&rig);
    assert_eq!(identifiers(&rig), ["urn:air:acme.com:kim", "urn:air:acme.com:a2a:kim"]);
    assert_eq!(listed[1]["type"], "application/a2a-agent-card+json");
    assert_eq!(
        listed[1]["url"],
        "https://agents.acme.com/agents/kim/a2a/agent-card.json"
    );
    assert_eq!(listed[1]["displayName"], "kim (A2A)");
    let status = rig.engine.inbound_status("kim").await.unwrap();
    assert_eq!(
        status.endpoints[0].a2a_url.as_deref(),
        Some("https://agents.acme.com/agents/kim/a2a")
    );
    assert_eq!(
        status.endpoints[0].agent_card_url.as_deref(),
        Some("https://agents.acme.com/agents/kim/a2a/agent-card.json")
    );
    let kinds: Vec<(&str, &[InboundScope])> = status
        .ard
        .iter()
        .map(|entry| (entry.media_type.as_str(), entry.listed_on.as_slice()))
        .collect();
    assert_eq!(
        kinds,
        [
            ("application/mcp-server-card+json", &[InboundScope::Internal][..]),
            ("application/a2a-agent-card+json", &[InboundScope::Internal][..]),
        ]
    );

    // Lee's MCP entry takes the identifier of Kim's A2A entry. Both holders leave every catalog.
    let mut lee = ProfileConfig::new("Lee");
    lee.inbound = inbound("kim", Some("a2a"), false);
    rig.engine.create_profile("lee", lee).await.unwrap();
    assert_eq!(identifiers(&rig), ["urn:air:acme.com:kim"]);
    let duplicate = "more than one profile uses the identifier `urn:air:acme.com:a2a:kim` (§4.2)";
    let kim = rig.engine.inbound_status("kim").await.unwrap().ard;
    assert_eq!(kim[0].listed_on, [InboundScope::Internal]);
    assert_eq!(kim[0].refused, Vec::<String>::new());
    assert!(kim[1].listed_on.is_empty());
    assert_eq!(kim[1].refused, [duplicate]);
    let lee = rig.engine.inbound_status("lee").await.unwrap().ard;
    assert_eq!(lee.len(), 1, "lee has A2A off");
    assert_eq!(lee[0].refused, [duplicate]);

    // A2A off: Kim's A2A entry and its addresses go, so Lee's MCP entry holds the identifier alone.
    save(&rig, inbound("kim", None, false)).await.unwrap();
    let status = rig.engine.inbound_status("kim").await.unwrap();
    assert_eq!(status.ard.len(), 1);
    assert_eq!(status.endpoints[0].a2a_url, None);
    assert_eq!(identifiers(&rig), ["urn:air:acme.com:kim", "urn:air:acme.com:a2a:kim"]);
    let lee = &entries(&rig)[1];
    assert_eq!(lee["type"], "application/mcp-server-card+json");
    assert_eq!(lee["url"], "https://agents.acme.com/agents/lee/mcp/server-card");
}

#[tokio::test]
async fn the_a2a_entry_names_the_skills_of_the_card_and_the_mcp_entry_keeps_its_tools() {
    let rig = rig().await;
    save(&rig, inbound("kim", None, true)).await.unwrap();
    let capabilities = |rig: &Rig| -> Vec<Value> { entries(rig).iter().map(|e| e["capabilities"].clone()).collect() };
    // Without skills of its own, the card has `ask`.
    assert_eq!(capabilities(&rig), [json!(["ask", "get_request"]), json!(["ask"])]);
    let skill = |id: &str| InboundSkill {
        id: id.into(),
        name: format!("Skill {id}"),
        description: "Does one thing. The owner approves it first.".into(),
        tags: vec!["work".into()],
        examples: Vec::new(),
    };
    let mut config = inbound("kim", None, true);
    config.skills = vec![skill("file-ticket"), skill("ask")];
    save(&rig, config.clone()).await.unwrap();
    assert_eq!(
        capabilities(&rig),
        [json!(["ask", "get_request"]), json!(["file-ticket", "ask"])]
    );
    // A skill that breaks a rule is refused on save, and the stored skills stay.
    config.skills.push(skill("file-ticket"));
    let error = save(&rig, config).await.unwrap_err();
    assert!(error.to_string().contains("the ID `file-ticket`"), "{error}");
    assert_eq!(rig.engine.inbound_settings("kim").await.unwrap().skills.len(), 2);
}

#[tokio::test]
async fn updated_at_changes_only_when_inbound_changes() {
    let rig = rig().await;
    // A profile that the operator created with a listing has no save time, so no `updatedAt`.
    let mut park = ProfileConfig::new("Park");
    park.inbound = inbound("park", None, false);
    rig.engine.create_profile("park", park).await.unwrap();
    assert!(entries(&rig)[0].get("updatedAt").is_none());

    let updated_at = |rig: &Rig| -> Vec<Value> {
        entries(rig)
            .into_iter()
            .filter(|entry| entry["identifier"].as_str().unwrap().ends_with(":kim"))
            .map(|entry| entry["updatedAt"].clone())
            .collect()
    };
    save(&rig, inbound("kim", None, true)).await.unwrap();
    let first = json!(utc_text(START));
    assert_eq!(updated_at(&rig), [first.clone(), first.clone()]);

    // Saves that leave `inbound` as it is keep the time: the same inbound part again, another
    // part, and the whole settings with another change.
    rig.clock.fetch_add(60, Ordering::SeqCst);
    save(&rig, inbound("kim", None, true)).await.unwrap();
    let notes = rig.engine.notes_settings("kim").await.unwrap();
    let update = NotesSettingsUpdate {
        notes,
        expected: Expected::Unchecked,
    };
    rig.engine.set_notes_settings("kim", update).await.unwrap();
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.owner_name = "Kim Lee".into();
    rig.engine.set_profile_config("kim", config.clone()).await.unwrap();
    assert_eq!(updated_at(&rig), [first.clone(), first]);

    // A change of `inbound` through either save path writes the new time.
    rig.clock.fetch_add(60, Ordering::SeqCst);
    config.inbound.title = Some("Kim's agent".into());
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let second = json!(utc_text(START + 120));
    assert_eq!(updated_at(&rig), [second.clone(), second]);
    rig.clock.fetch_add(60, Ordering::SeqCst);
    let mut tagged = inbound("kim", None, true);
    tagged.ard.as_mut().unwrap().tags = vec!["deploys".into()];
    save(&rig, tagged).await.unwrap();
    let third = json!(utc_text(START + 180));
    assert_eq!(updated_at(&rig), [third.clone(), third]);
    // A refused save stores nothing, also no time.
    rig.clock.fetch_add(60, Ordering::SeqCst);
    let mut refused = inbound("kim", None, true);
    refused.ard.as_mut().unwrap().tags = vec!["two\nlines".into()];
    assert!(save(&rig, refused).await.is_err());
    assert_eq!(
        updated_at(&rig),
        [json!(utc_text(START + 180)), json!(utc_text(START + 180))]
    );
}

#[tokio::test]
async fn the_trust_manifest_is_checked_on_save_and_listed_on_both_entries() {
    let rig = rig().await;
    let with_manifest = |manifest: Value| {
        let mut config = inbound("kim", None, true);
        config.ard.as_mut().unwrap().trust_manifest = Some(manifest.as_object().unwrap().clone());
        config
    };
    // Another domain breaks the publisher binding (§4.5.1). A missing identity breaks the shape check.
    for (manifest, why) in [
        (json!({"identity": "spiffe://evil.com/ns/kim"}), "§4.5.1"),
        (json!({"identity": "did:key:z6Mkh"}), "§4.5.1"),
        (json!({"identityType": "spiffe"}), "inbound.ard.trust_manifest"),
    ] {
        let error = save(&rig, with_manifest(manifest.clone())).await.unwrap_err();
        assert!(
            matches!(&error, ApiError::Invalid(m) if m.contains(why)),
            "{manifest}: {error:?}"
        );
    }
    assert_eq!(
        rig.engine.inbound_settings("kim").await.unwrap(),
        InboundConfig::default()
    );

    let manifest = json!({"identity": "spiffe://acme.com/ns/agents/kim",
        "attestations": [{"type": "SOC2-Type2", "uri": "https://acme.com/soc2"}]});
    save(&rig, with_manifest(manifest.clone())).await.unwrap();
    for entry in entries(&rig) {
        assert_eq!(entry["trustManifest"], manifest);
    }
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let changed = audit.iter().find(|a| a.kind == "inbound_settings_changed").unwrap();
    let detail = serde_json::to_string(&changed.detail).unwrap();
    assert!(detail.contains(r#""trust_manifest":true"#), "{detail}");
    // The audit holds no word of the manifest.
    assert!(!detail.contains("spiffe") && !detail.contains("SOC2"), "{detail}");

    // Settings that the operator stored without the check, with an identity that is no text, are
    // refused by the catalog on both entries, and the owner reads why.
    let mut lee = ProfileConfig::new("Lee");
    lee.inbound = inbound("lee", None, true);
    lee.inbound.ard.as_mut().unwrap().trust_manifest = Some(json!({"identity": 7}).as_object().unwrap().clone());
    rig.engine.create_profile("lee", lee).await.unwrap();
    assert!(identifiers(&rig).iter().all(|id| !id.ends_with(":lee")));
    let lee = rig.engine.inbound_status("lee").await.unwrap().ard;
    assert_eq!(lee.len(), 2);
    for entry in &lee {
        assert!(entry.listed_on.is_empty());
        assert!(
            entry.refused.iter().any(|r| r.contains("trustManifest.identity")),
            "{:?}",
            entry.refused
        );
    }
}

#[tokio::test]
async fn the_dns_text_names_each_listener_that_lists_an_entry() {
    let rig = rig().await;
    // No listing, no record.
    assert!(rig.engine.inbound_status("kim").await.unwrap().dns_records.is_empty());
    save(&rig, inbound("kim", None, false)).await.unwrap();
    assert_eq!(
        rig.engine.inbound_status("kim").await.unwrap().dns_records,
        ["_entries._agents.acme.com. 3600 IN SVCB 1 agents.acme.com."]
    );
    // An internal profile is not on the public listener, so that listener gets no record.
    rig.engine.add_outer_listener(OuterListener {
        scope: InboundScope::Public,
        url: "https://edge.acme.com:8443".into(),
    });
    assert_eq!(rig.engine.inbound_status("kim").await.unwrap().dns_records.len(), 1);
    let mut public = inbound("kim", None, false);
    public.scope = InboundScope::Public;
    save(&rig, public).await.unwrap();
    assert_eq!(
        rig.engine.inbound_status("kim").await.unwrap().dns_records,
        [
            "_entries._agents.acme.com. 3600 IN SVCB 1 agents.acme.com.",
            "_entries._agents.acme.com. 3600 IN SVCB 1 edge.acme.com. port=8443"
        ]
    );
    // A publisher that the operator does not allow lists nothing, so there is no record.
    let mut other = rig.engine.profile_config("kim").await.unwrap();
    other.inbound.ard.as_mut().unwrap().publisher = "other.example".into();
    let text = serde_json::to_string(&other).unwrap();
    rig.engine
        .create_profile("other", serde_json::from_str(&text).unwrap())
        .await
        .unwrap();
    assert!(rig.engine.inbound_status("other").await.unwrap().dns_records.is_empty());
}
