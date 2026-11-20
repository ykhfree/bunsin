//! The ARD registry of the engine: the index of the server's own catalogs, the ranking, and what
//! each listener sees.

use bunsin_api::http::server_card_path;
use bunsin_api::{EngineApi, Expected, InboundUpdate, ProfileConfig};
use bunsin_ard::{ExploreQuery, ListQuery, SearchQuery, accept_entry, parse_explore, parse_list, parse_search};
use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope};
use bunsin_core::ports::BoxFuture;
use bunsin_engine::{Backends, Engine, OuterListener, Runtime};
use bunsin_store::{RegistryStore, SourceKind};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

const INTERNAL_URL: &str = "https://agents.corp.acme.com";
const PUBLIC_URL: &str = "https://agents.acme.com";

/// Backends that allow two publisher domains and build nothing.
struct Publishers;

impl Backends for Publishers {
    fn build<'a>(&'a self, _profile: &'a str, _config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async { Err("not needed".to_owned()) })
    }

    fn ard_publishers(&self) -> Vec<String> {
        vec!["acme.com".into(), "corp.acme.com".into()]
    }
}

struct Rig {
    dir: tempfile::TempDir,
    engine: Engine,
    clock: Arc<AtomicI64>,
}

impl Rig {
    /// An engine with both listeners and the registry on for `registry`.
    fn new(registry: InboundScope) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let time = clock.clone();
        let engine = Engine::open(
            dir.path(),
            Arc::new(Publishers),
            Arc::new(move || time.load(Ordering::SeqCst)),
        )
        .unwrap();
        for (scope, url) in [
            (InboundScope::Internal, INTERNAL_URL),
            (InboundScope::Public, PUBLIC_URL),
        ] {
            engine.add_outer_listener(OuterListener { scope, url: url.into() });
        }
        engine.serve_registry(registry).unwrap();
        Rig { dir, engine, clock }
    }

    /// Adds a profile whose endpoint is on for `scope` and listed under acme.com.
    async fn publish(&self, name: &str, scope: InboundScope, description: &str, queries: &[&str]) {
        let mut config = ProfileConfig::new(name);
        config.inbound = InboundConfig {
            enabled: true,
            scope,
            description: description.into(),
            ard: Some(ArdListing {
                publisher: "acme.com".into(),
                agent_name: name.into(),
                representative_queries: queries.iter().map(|q| (*q).to_owned()).collect(),
                ..ArdListing::default()
            }),
            ..InboundConfig::default()
        };
        self.engine.create_profile(name, config).await.unwrap();
    }

    fn refresh(&self) {
        self.engine.refresh_own_catalogs().unwrap();
    }

    fn search(&self, listener: InboundScope, body: Value) -> Value {
        let request: SearchQuery = parse_search(body.to_string().as_bytes()).unwrap();
        json!(self.engine.registry_search(listener, &request).unwrap())
    }

    fn explore(&self, listener: InboundScope, body: Value) -> Value {
        let request: ExploreQuery = parse_explore(body.to_string().as_bytes()).unwrap();
        json!(self.engine.registry_explore(listener, &request).unwrap())
    }

    fn list(&self, listener: InboundScope, pairs: &[(&str, &str)]) -> Value {
        let pairs = pairs.iter().map(|(n, v)| ((*n).to_owned(), (*v).to_owned()));
        let request: ListQuery = parse_list(pairs).unwrap();
        json!(self.engine.registry_list(listener, &request).unwrap())
    }
}

fn text(text: &str) -> Value {
    json!({"query": {"text": text}})
}

fn identifiers(answer: &Value, member: &str) -> Vec<String> {
    answer[member]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["identifier"].as_str().unwrap().to_owned())
        .collect()
}

fn scores(answer: &Value) -> Vec<(String, Value)> {
    answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| (item["identifier"].as_str().unwrap().to_owned(), item["score"].clone()))
        .collect()
}

const ALL_FACETS: [&str; 2] = ["publisher", "type"];

fn facets(rig: &Rig, listener: InboundScope, query: Value) -> Value {
    let facets: Vec<Value> = ALL_FACETS.iter().map(|field| json!({"field": field})).collect();
    rig.explore(listener, json!({"query": query, "resultType": {"facets": facets}}))
}

#[tokio::test]
async fn the_public_listener_never_sees_internal_rows() {
    let rig = Rig::new(InboundScope::Public);
    rig.publish(
        "kim",
        InboundScope::Internal,
        "Payroll exceptions",
        &["who approves payroll exceptions", "where is the payroll calendar"],
    )
    .await;
    rig.publish(
        "lee",
        InboundScope::Public,
        "Shipping delays",
        &["why is my parcel late", "track a shipping delay"],
    )
    .await;
    rig.refresh();

    // The internal listener sees both profiles; the public one sees the public profile and its own entry.
    let internal = rig.list(InboundScope::Internal, &[]);
    assert_eq!(
        identifiers(&internal, "items"),
        [
            "urn:air:acme.com:kim",
            "urn:air:acme.com:lee",
            "urn:air:corp.acme.com:registry:bunsin"
        ]
    );
    let public = rig.list(InboundScope::Public, &[]);
    assert_eq!(
        identifiers(&public, "items"),
        ["urn:air:acme.com:lee", "urn:air:acme.com:registry:bunsin"]
    );
    assert_eq!(public["total"], 2);
    assert!(!public.to_string().contains("ayroll"), "{public}");

    // Search, filters, facets and the list filter find nothing of the internal profile.
    assert_eq!(
        identifiers(
            &rig.search(InboundScope::Internal, text("payroll exceptions")),
            "results"
        ),
        ["urn:air:acme.com:kim"]
    );
    assert_eq!(
        rig.search(InboundScope::Public, text("payroll exceptions"))["results"],
        json!([])
    );
    let filtered = json!({"query": {"text": "payroll", "filter": {"identifier": "urn:air:acme.com:kim"}}});
    assert_eq!(rig.search(InboundScope::Public, filtered)["results"], json!([]));
    assert_eq!(
        facets(&rig, InboundScope::Public, json!({}))["facets"]["publisher"]["buckets"],
        json!([{"value": "acme.com", "count": 2}])
    );
    assert_eq!(
        facets(&rig, InboundScope::Internal, json!({}))["facets"]["publisher"]["buckets"],
        json!([{"value": "acme.com", "count": 2}, {"value": "corp.acme.com", "count": 1}])
    );
    assert_eq!(
        rig.list(InboundScope::Public, &[("filter", "displayName = 'kim'")])["total"],
        0
    );
    assert_eq!(
        rig.list(InboundScope::Internal, &[("filter", "displayName = 'kim'")])["total"],
        1
    );
}

#[tokio::test]
async fn an_internal_row_changes_no_public_score_count_or_total() {
    let rig = Rig::new(InboundScope::Public);
    for (name, topic) in [
        ("lee", "parcel tracking"),
        ("park", "invoice disputes"),
        ("choi", "office keys"),
        ("jung", "laptop repairs"),
    ] {
        rig.publish(
            name,
            InboundScope::Public,
            &format!("Questions about {topic}"),
            &[topic, "general help"],
        )
        .await;
    }
    rig.refresh();
    // `zebra` is in no public row. A public caller probes for it next to a public word.
    let probe = text("parcel zebra");
    let answers = |rig: &Rig| {
        (
            rig.search(InboundScope::Public, probe.clone()),
            // Scores that depend on N and df: one rare term in each row, one common term in four.
            rig.search(InboundScope::Public, text("parcel invoice questions")),
            facets(rig, InboundScope::Public, probe["query"].clone()),
            facets(rig, InboundScope::Public, json!({})),
            rig.list(InboundScope::Public, &[]),
        )
    };
    let before = answers(&rig);
    assert_eq!(scores(&before.0), [("urn:air:acme.com:lee".to_owned(), json!(100))]);

    // An internal profile holds the probe word, the public word and the common words.
    rig.publish(
        "kim",
        InboundScope::Internal,
        "Zebra parcel questions",
        &["general help with zebra parcels", "parcel zebra"],
    )
    .await;
    rig.refresh();
    assert_eq!(answers(&rig), before);
    // The internal listener does count the new row: there the public row no longer matches every term.
    let internal = scores(&rig.search(InboundScope::Internal, probe));
    assert_eq!(internal[0], ("urn:air:acme.com:kim".to_owned(), json!(100)));
    let lee = internal.iter().find(|(id, _)| id == "urn:air:acme.com:lee").unwrap();
    assert!(lee.1.as_u64().unwrap() < 100, "{internal:?}");
}

#[tokio::test]
async fn scores_are_integers_and_a_full_match_scores_100() {
    let rig = Rig::new(InboundScope::Internal);
    rig.publish(
        "kim",
        InboundScope::Internal,
        "Refund approvals for travel",
        &["approve a travel refund", "refund status"],
    )
    .await;
    rig.publish(
        "lee",
        InboundScope::Internal,
        "Travel bookings",
        &["book travel to Seoul", "change a booking"],
    )
    .await;
    rig.publish(
        "park",
        InboundScope::Internal,
        "Office plants",
        &["water the plants", "order plants"],
    )
    .await;
    rig.refresh();
    let answer = rig.search(InboundScope::Internal, text("travel refund approvals"));
    let found = scores(&answer);
    assert_eq!(found.len(), 2, "{answer}");
    assert_eq!(found[0], ("urn:air:acme.com:kim".to_owned(), json!(100)));
    assert_eq!(found[1].0, "urn:air:acme.com:lee");
    let partial = found[1].1.as_u64().expect("an integer score");
    assert!((1..100).contains(&partial), "{partial}");
    for item in answer["results"].as_array().unwrap() {
        // A result names its registry and leaves out the queries that serve the index.
        assert_eq!(item["source"], INTERNAL_URL);
        assert!(item.get("representativeQueries").is_none(), "{item}");
        assert!(item["score"].is_u64(), "{item}");
    }
    // A term that no visible row holds does not lower the best score.
    assert_eq!(
        scores(&rig.search(InboundScope::Internal, text("refund approvals nonexistentword")))[0].1,
        json!(100)
    );
    // Without usable terms nothing matches.
    assert_eq!(rig.search(InboundScope::Internal, text("a an"))["results"], json!([]));
}

#[tokio::test]
async fn explore_and_search_share_the_cutoff() {
    let rig = Rig::new(InboundScope::Internal);
    // Nine rows on the internal listener, with its own entry: `questions` is in eight of them.
    let names = ["kim", "lee", "park", "choi", "jung", "kang", "yoon", "lim"];
    for (index, name) in names.iter().enumerate() {
        let topic = if index == 0 {
            "refund questions"
        } else {
            "team questions"
        };
        rig.publish(name, InboundScope::Internal, topic, &[topic, "help desk"])
            .await;
    }
    rig.refresh();
    let total_count = |answer: &Value| -> u64 {
        answer["facets"]["type"]["buckets"]
            .as_array()
            .unwrap()
            .iter()
            .map(|bucket| bucket["count"].as_u64().unwrap())
            .sum()
    };
    for (query, expected) in [
        (json!({"text": "refund questions"}), 1),
        // A common word alone is below the cutoff in both.
        (json!({"text": "questions"}), 0),
        (json!({"text": "help desk"}), 0),
        // A filter narrows both the same way.
        (json!({"text": "refund", "filter": {"publisher": "beta.com"}}), 0),
        (
            json!({"text": "refund", "filter": {"type": "application/mcp-server-card+json"}}),
            1,
        ),
    ] {
        let searched = rig.search(InboundScope::Internal, json!({"query": query.clone(), "pageSize": 100}));
        let explored = facets(&rig, InboundScope::Internal, query.clone());
        assert_eq!(searched["results"].as_array().unwrap().len(), expected, "{query}");
        assert_eq!(total_count(&explored), expected as u64, "{query}");
    }
    // Without text, Explore covers every visible row.
    assert_eq!(total_count(&facets(&rig, InboundScope::Internal, json!({}))), 9);
}

#[tokio::test]
async fn self_entries_name_the_card_of_their_listener() {
    let rig = Rig::new(InboundScope::Public);
    rig.publish(
        "kim",
        InboundScope::Internal,
        "Payroll",
        &["payroll dates", "payroll errors"],
    )
    .await;
    rig.publish(
        "lee",
        InboundScope::Public,
        "Shipping",
        &["shipping dates", "shipping errors"],
    )
    .await;
    rig.refresh();
    let urls = |listener| -> Vec<(String, String)> {
        let answer = rig.list(listener, &[]);
        answer["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                (
                    item["identifier"].as_str().unwrap().to_owned(),
                    item["url"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let card = |base: &str, profile: &str| format!("{base}{}", server_card_path(profile));
    // Each listener's rows point at the cards of the same listener, and its registry entry at itself.
    // The registry entry takes the longest allowed domain of the listener host.
    assert_eq!(
        urls(InboundScope::Internal),
        [
            ("urn:air:acme.com:kim".to_owned(), card(INTERNAL_URL, "kim")),
            ("urn:air:acme.com:lee".to_owned(), card(INTERNAL_URL, "lee")),
            (
                "urn:air:corp.acme.com:registry:bunsin".to_owned(),
                INTERNAL_URL.to_owned()
            ),
        ]
    );
    assert_eq!(
        urls(InboundScope::Public),
        [
            ("urn:air:acme.com:lee".to_owned(), card(PUBLIC_URL, "lee")),
            ("urn:air:acme.com:registry:bunsin".to_owned(), PUBLIC_URL.to_owned()),
        ]
    );
    let registry = rig
        .engine
        .ard_catalog(InboundScope::Public)
        .manifest
        .entries
        .into_iter()
        .find(|entry| entry.identifier == "urn:air:acme.com:registry:bunsin")
        .unwrap();
    assert_eq!(registry.media_type, "application/ai-registry+json");
    assert_eq!(registry.url.as_deref(), Some(PUBLIC_URL));
    assert_eq!(
        bunsin_ard::validate(&registry)
            .iter()
            .filter(|v| v.level() == bunsin_ard::Level::Must)
            .count(),
        0
    );
}

#[tokio::test]
async fn a_listener_without_the_registry_or_a_fitting_domain_lists_no_registry() {
    let rig = Rig::new(InboundScope::Internal);
    let has_registry = |listener| {
        rig.engine
            .ard_catalog(listener)
            .manifest
            .entries
            .iter()
            .any(|entry| entry.media_type == "application/ai-registry+json")
    };
    assert!(has_registry(InboundScope::Internal));
    assert!(!has_registry(InboundScope::Public));
    assert_eq!(rig.engine.registry_publisher(InboundScope::Public), None);
    assert!(!rig.engine.registry_serves(InboundScope::Public));
    // A host under no allowed domain gets no registry entry.
    rig.engine.add_outer_listener(OuterListener {
        scope: InboundScope::Internal,
        url: "https://agents.example.org".into(),
    });
    assert!(!has_registry(InboundScope::Internal));
    assert_eq!(rig.engine.registry_publisher(InboundScope::Internal), None);
    // The public listener refuses registry requests when only the internal one serves it.
    let request = parse_search(text("x").to_string().as_bytes()).unwrap();
    assert!(rig.engine.registry_search(InboundScope::Public, &request).is_err());
}

#[tokio::test]
async fn a_refresh_keeps_the_first_time_and_moves_the_change_time_only_on_an_edit() {
    let rig = Rig::new(InboundScope::Internal);
    rig.publish(
        "kim",
        InboundScope::Internal,
        "Payroll",
        &["payroll dates", "payroll errors"],
    )
    .await;
    rig.publish(
        "lee",
        InboundScope::Internal,
        "Shipping",
        &["shipping dates", "shipping errors"],
    )
    .await;
    rig.refresh();
    // 1,000,000 is 1970-01-12T13:46:40Z. Later refreshes happen after the cut below.
    let cut = "1970-01-13T00:00:00Z";
    rig.clock.store(2_000_000, Ordering::SeqCst);
    rig.refresh();
    let after = |field: &str| rig.list(InboundScope::Internal, &[("filter", &format!("{field} > '{cut}'"))]);
    assert_eq!(after("createdAfter")["total"], 0);
    assert_eq!(after("updatedAfter")["total"], 0);

    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.inbound.description = "Payroll and bonuses".into();
    rig.engine.set_profile_config("kim", config).await.unwrap();
    rig.refresh();
    assert_eq!(after("createdAfter")["total"], 0);
    assert_eq!(identifiers(&after("updatedAfter"), "items"), ["urn:air:acme.com:kim"]);
    // The order by change time puts the edited entry last, and DESC first.
    let ordered = rig.list(InboundScope::Internal, &[("orderBy", "updatedAt DESC")]);
    assert_eq!(identifiers(&ordered, "items")[0], "urn:air:acme.com:kim");
}

#[tokio::test]
async fn pages_follow_their_tokens_and_a_token_of_another_query_is_refused() {
    let rig = Rig::new(InboundScope::Internal);
    for name in ["kim", "lee", "park", "choi", "jung"] {
        rig.publish(
            name,
            InboundScope::Internal,
            &format!("{name} desk"),
            &["escalation desk", "general help"],
        )
        .await;
    }
    rig.refresh();
    // Five profiles and the registry's own entry, two a page.
    let first = rig.list(InboundScope::Internal, &[("pageSize", "2")]);
    assert_eq!(first["total"], 6);
    let mut seen = identifiers(&first, "items");
    let mut token = first["pageToken"].as_str().unwrap().to_owned();
    loop {
        let next = rig.list(InboundScope::Internal, &[("pageSize", "2"), ("pageToken", &token)]);
        seen.extend(identifiers(&next, "items"));
        match next["pageToken"].as_str() {
            Some(more) => token = more.to_owned(),
            None => break,
        }
    }
    let mut sorted = seen.clone();
    sorted.sort();
    assert_eq!(seen, sorted);
    assert_eq!(seen.len(), 6);
    // The token belongs to the request that made it.
    let other = parse_list([
        ("filter".to_owned(), "type = 'x'".to_owned()),
        ("pageToken".to_owned(), token),
    ])
    .unwrap();
    assert!(matches!(
        rig.engine.registry_list(InboundScope::Internal, &other),
        Err(bunsin_api::ApiError::Invalid(_))
    ));
    // A search page token works the same way.
    let body = json!({"query": {"text": "kim park lee"}, "pageSize": 1});
    let page = rig.search(InboundScope::Internal, body.clone());
    let token = page["pageToken"].as_str().unwrap();
    let mut next = body.clone();
    next["pageToken"] = json!(token);
    let second = rig.search(InboundScope::Internal, next);
    assert_ne!(identifiers(&page, "results"), identifiers(&second, "results"));
}

/// A crawled entry of the publisher in `identifier`.
fn crawled(identifier: &str) -> Value {
    json!({"identifier": identifier, "displayName": "Crawled", "type": "text/html", "url": "https://crawled.test/"})
}

/// Reads the next due seed as a crawl does: `entries` gets the seed's domain and gives what its sources hold.
fn crawl(rig: &Rig, entries: impl Fn(&str) -> Vec<Value>) {
    let (source, domain) = rig.engine.due_registry_seed().unwrap().expect("a seed is due");
    let found = entries(&domain).iter().map(|raw| accept_entry(raw, &domain)).collect();
    rig.engine.crawl_succeeded(source, found).unwrap();
}

#[tokio::test]
async fn a_request_reads_one_state_of_the_index_while_the_own_catalogs_refresh() {
    let rig = Rig::new(InboundScope::Internal);
    for name in ["kim", "lee", "park", "choi"] {
        rig.publish(
            name,
            InboundScope::Internal,
            &format!("{name} refund desk"),
            &["refund desk", "general help"],
        )
        .await;
    }
    rig.refresh();
    let search = parse_search(text("kim refund").to_string().as_bytes()).unwrap();
    let list = parse_list(Vec::new()).unwrap();
    let answers = || {
        (
            rig.engine
                .registry_search(InboundScope::Internal, &search)
                .map(|a| json!(a)),
            rig.engine
                .registry_list(InboundScope::Internal, &list)
                .map(|a| json!(a)),
        )
    };
    let (Ok(searched), Ok(listed)) = answers() else {
        panic!("the first answers failed");
    };
    assert_eq!(identifiers(&searched, "results")[0], "urn:air:acme.com:kim");
    assert_eq!(listed["total"], 5);
    // Each refresh replaces the rows of the own catalog with new row IDs.
    let done = AtomicBool::new(false);
    let mut wrong = Vec::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(Ordering::SeqCst) {
                rig.refresh();
            }
        });
        for _ in 0..500 {
            match answers() {
                (Ok(s), Ok(l)) if s == searched && l == listed => {}
                other => wrong.push(format!("{other:?}")),
            }
        }
        done.store(true, Ordering::SeqCst);
    });
    assert!(
        wrong.is_empty(),
        "{} of 500 answers were wrong; the first: {}",
        wrong.len(),
        wrong[0]
    );
}

#[tokio::test]
async fn the_own_catalogs_take_no_room_of_the_seeds() {
    let rig = Rig::new(InboundScope::Internal);
    rig.publish(
        "kim",
        InboundScope::Internal,
        "Payroll",
        &["payroll dates", "payroll errors"],
    )
    .await;
    rig.refresh();
    let seeds: Vec<(String, InboundScope)> = (0..21)
        .map(|n| (format!("s{n}.test"), InboundScope::Internal))
        .collect();
    rig.engine.set_registry_seeds(&seeds).unwrap();
    for _ in &seeds {
        crawl(&rig, |domain| {
            (0..500).map(|n| crawled(&format!("urn:air:{domain}:e{n}"))).collect()
        });
    }
    // The seeds alone fill the 10,000 entries of the index; the own rows do not count. The 21st seed finds no room.
    let status = RegistryStore::open(rig.dir.path()).unwrap().status().unwrap();
    let seeded: u64 = status
        .iter()
        .filter(|source| source.kind == SourceKind::Seed)
        .map(|source| source.entries)
        .sum();
    assert_eq!(seeded, 10_000);
    let last = status.iter().find(|source| source.url == "s20.test").unwrap();
    assert_eq!(
        (last.entries, last.refusals.clone()),
        (0, vec![("index_full".to_owned(), 500)])
    );

    // With the index full, a new profile and the registry's own entry are still listed.
    rig.publish(
        "lee",
        InboundScope::Internal,
        "Shipping",
        &["shipping dates", "shipping errors"],
    )
    .await;
    rig.refresh();
    let own = rig.list(
        InboundScope::Internal,
        &[("filter", "publisherId = 'acme.com,corp.acme.com'")],
    );
    assert_eq!(
        identifiers(&own, "items"),
        [
            "urn:air:acme.com:kim",
            "urn:air:acme.com:lee",
            "urn:air:corp.acme.com:registry:bunsin"
        ]
    );
    assert_eq!(rig.list(InboundScope::Internal, &[])["total"], 10_003);
    let status = RegistryStore::open(rig.dir.path()).unwrap().status().unwrap();
    let own = status.iter().find(|source| source.url == "self:internal").unwrap();
    assert_eq!((own.entries, own.refusals.clone()), (3, vec![]));
}

#[tokio::test]
async fn a_save_that_changes_inbound_updates_the_registry_at_once() {
    let rig = Rig::new(InboundScope::Public);
    for name in ["kim", "lee", "park"] {
        rig.publish(
            name,
            InboundScope::Public,
            &format!("{name} desk"),
            &["help desk", "general help"],
        )
        .await;
    }
    rig.refresh();
    let listed = |listener| identifiers(&rig.list(listener, &[]), "items");
    assert_eq!(
        listed(InboundScope::Public),
        [
            "urn:air:acme.com:kim",
            "urn:air:acme.com:lee",
            "urn:air:acme.com:park",
            "urn:air:acme.com:registry:bunsin"
        ]
    );
    // No refresh runs between the saves and the checks below.
    // A settings save moves kim to the internal listener.
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.inbound.scope = InboundScope::Internal;
    rig.engine.set_profile_config("kim", config).await.unwrap();
    assert_eq!(
        listed(InboundScope::Public),
        [
            "urn:air:acme.com:lee",
            "urn:air:acme.com:park",
            "urn:air:acme.com:registry:bunsin"
        ]
    );
    assert!(listed(InboundScope::Internal).contains(&"urn:air:acme.com:kim".to_owned()));
    // A save of the inbound settings removes lee's listing.
    let mut inbound = rig.engine.inbound_settings("lee").await.unwrap();
    inbound.ard = None;
    let update = InboundUpdate {
        inbound,
        expected: Expected::Unchecked,
    };
    rig.engine.set_inbound_settings("lee", update).await.unwrap();
    assert_eq!(
        listed(InboundScope::Public),
        ["urn:air:acme.com:park", "urn:air:acme.com:registry:bunsin"]
    );
    // A settings save turns park's endpoint off. A listing needs the endpoint on, so it goes too.
    let mut config = rig.engine.profile_config("park").await.unwrap();
    config.inbound.enabled = false;
    config.inbound.ard = None;
    rig.engine.set_profile_config("park", config).await.unwrap();
    assert_eq!(listed(InboundScope::Public), ["urn:air:acme.com:registry:bunsin"]);
    assert_eq!(
        listed(InboundScope::Internal),
        ["urn:air:acme.com:kim", "urn:air:corp.acme.com:registry:bunsin"]
    );
}

#[tokio::test]
async fn a_crawled_entry_shows_no_score_or_source_of_its_own() {
    let rig = Rig::new(InboundScope::Internal);
    rig.engine
        .set_registry_seeds(&[("beta.test".to_owned(), InboundScope::Internal)])
        .unwrap();
    crawl(&rig, |domain| {
        let mut entry = crawled(&format!("urn:air:{domain}:refunds"));
        entry["description"] = json!("Refund desk");
        entry["score"] = json!(7);
        entry["source"] = json!("https://evil.example/");
        vec![entry]
    });
    let listed = rig.list(InboundScope::Internal, &[("filter", "publisherId = 'beta.test'")]);
    let item = &listed["items"][0];
    assert_eq!(item["identifier"], "urn:air:beta.test:refunds", "{listed}");
    assert!(item.get("score").is_none() && item.get("source").is_none(), "{item}");
    // A search result carries the registry's own score and source.
    let found = rig.search(InboundScope::Internal, text("refund desk"));
    let result = &found["results"][0];
    assert_eq!(result["identifier"], "urn:air:beta.test:refunds", "{found}");
    assert_eq!(
        (result["score"].clone(), result["source"].clone()),
        (json!(100), json!(INTERNAL_URL))
    );
}
