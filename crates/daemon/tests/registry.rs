//! The ARD registry on the outer listeners, over HTTP, and the operator command `registry-status`.

use bunsin_api::ProfileConfig;
use bunsin_core::inbound::{ArdListing, InboundConfig, InboundScope};
use bunsin_core::ports::BoxFuture;
use bunsin_daemon::server::{OuterConfig, Reach, Running, ServeConfig, serve_outer, start};
use bunsin_engine::{Backends, Engine, OuterListener, Runtime};
use bunsin_store::{NewEntry, Refusal, RegistryStore, SourceKind};
use reqwest::StatusCode;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use tokio::sync::Notify;

const PUBLIC_URL: &str = "https://agents.acme.com";
const INTERNAL_URL: &str = "https://agents.corp.acme.com:8443";

/// Backends that allow the publisher domain acme.com and build nothing.
struct Publishers;

impl Backends for Publishers {
    fn build<'a>(&'a self, _profile: &'a str, _config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        Box::pin(async { Err("not needed".to_owned()) })
    }

    fn ard_publishers(&self) -> Vec<String> {
        vec!["acme.com".into()]
    }
}

fn outer(reach: Reach, url: &str) -> OuterConfig {
    OuterConfig {
        a2a_root_profile: None,
        reach,
        listen: "127.0.0.1:0".parse().unwrap(),
        url: url.parse().unwrap(),
        tls: None,
    }
}

struct Rig {
    _dir: tempfile::TempDir,
    engine: Arc<Engine>,
    internal: Running,
    public: Running,
}

/// An engine with two published profiles, the registry on for `registry`, and both listeners.
async fn rig(registry: Option<InboundScope>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), Arc::new(Publishers), Arc::new(|| 1_000_000)).unwrap();
    for (name, scope, topic) in [
        ("kim", InboundScope::Internal, "payroll exceptions"),
        ("lee", InboundScope::Public, "parcel tracking"),
    ] {
        let mut config = ProfileConfig::new(name);
        config.inbound = InboundConfig {
            enabled: true,
            scope,
            description: format!("Questions about {topic}"),
            ard: Some(ArdListing {
                publisher: "acme.com".into(),
                agent_name: name.into(),
                representative_queries: vec![topic.into(), "general help".into()],
                ..ArdListing::default()
            }),
            ..InboundConfig::default()
        };
        engine.create_profile(name, config).await.unwrap();
    }
    if let Some(scope) = registry {
        engine.serve_registry(scope).unwrap();
    }
    let engine = Arc::new(engine);
    let wake = Arc::new(Notify::new());
    let internal = serve_outer(engine.clone(), outer(Reach::Internal, INTERNAL_URL), wake.clone())
        .await
        .unwrap();
    let public = serve_outer(engine.clone(), outer(Reach::Public, PUBLIC_URL), wake)
        .await
        .unwrap();
    let (_stop, stopped) = tokio::sync::watch::channel(false);
    bunsin_daemon::crawl::tick(&engine, None, &stopped).await;
    Rig {
        _dir: dir,
        engine,
        internal,
        public,
    }
}

async fn post(listener: &Running, path: &str, body: String) -> reqwest::Response {
    reqwest::Client::new()
        .post(format!("{}{path}", listener.url()))
        .header("content-type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

/// `GET /agents` with the parameters encoded, so a `+` in a media type stays a `+`.
async fn agents(listener: &Running, pairs: &[(&str, &str)]) -> reqwest::Response {
    let mut url = url::Url::parse(&format!("{}/agents", listener.url())).unwrap();
    if !pairs.is_empty() {
        url.query_pairs_mut().extend_pairs(pairs);
    }
    reqwest::get(url).await.unwrap()
}

async fn body(response: reqwest::Response) -> (StatusCode, Value) {
    let status = response.status();
    (status, response.json().await.unwrap())
}

/// The response schemas of `ard.openapi.yaml` (components.schemas), transcribed to JSON. The
/// reference `./ard-entry.schema.json#/$defs/EntryFields` points into the copy of the entry schema.
fn openapi_errors(component: &str, instance: &Value) -> Vec<String> {
    let entry: Value = serde_json::from_str(include_str!("../../ard/tests/fixtures/ard-entry.schema.json")).unwrap();
    let mut defs: Map<String, Value> = entry["$defs"].as_object().unwrap().clone();
    let components = json!({
        "SearchResponse": {
            "type": "object",
            "required": ["results"],
            "properties": {
                "results": {"type": "array", "items": {"$ref": "#/$defs/SearchResultItem"}},
                "referrals": {"type": "array", "items": {"$ref": "#/$defs/RegistryReferral"}},
                "pageToken": {"type": "string"}
            },
            "additionalProperties": false
        },
        "SearchResultItem": {
            "allOf": [{"$ref": "#/$defs/EntryFields"}],
            "type": "object",
            "required": ["identifier", "score", "source"],
            "properties": {
                "score": {"type": "integer", "minimum": 0, "maximum": 100},
                "source": {"type": "string", "format": "uri"}
            }
        },
        "RegistryReferral": {
            "type": "object",
            "required": ["identifier", "displayName", "type", "url"],
            "properties": {
                "identifier": {"type": "string"},
                "displayName": {"type": "string"},
                "type": {"type": "string", "enum": ["application/ai-registry", "application/ai-registry+json"]},
                "url": {"type": "string", "format": "uri"}
            }
        },
        "ExploreResponse": {
            "type": "object",
            "required": ["resultType", "facets"],
            "properties": {
                "resultType": {"type": "string", "enum": ["facets"]},
                "facets": {"type": "object", "additionalProperties": {"$ref": "#/$defs/ExploreFacetResult"}}
            },
            "additionalProperties": false
        },
        "ExploreFacetResult": {
            "type": "object",
            "required": ["buckets"],
            "properties": {
                "buckets": {"type": "array", "items": {"$ref": "#/$defs/ExploreFacetBucket"}},
                "otherCount": {"type": "integer"}
            },
            "additionalProperties": false
        },
        "ExploreFacetBucket": {
            "type": "object",
            "required": ["value", "count"],
            "properties": {"value": {"type": "string"}, "count": {"type": "integer"}},
            "additionalProperties": false
        },
        "ListResponse": {
            "type": "object",
            "required": ["items"],
            "properties": {
                "items": {"type": "array", "items": {"allOf": [{"$ref": "#/$defs/EntryFields"}], "required": ["identifier"]}},
                "total": {"type": "integer"},
                "pageToken": {"type": "string"}
            }
        },
        "Error": {
            "type": "object",
            "required": ["errorCode", "message"],
            "properties": {"errorCode": {"type": "string"}, "message": {"type": "string"}}
        }
    });
    defs.extend(components.as_object().unwrap().clone());
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": defs,
        "$ref": format!("#/$defs/{component}")
    });
    let validator = jsonschema::options()
        .should_validate_formats(true)
        .build(&schema)
        .unwrap();
    validator.iter_errors(instance).map(|error| error.to_string()).collect()
}

#[tokio::test]
async fn answers_follow_the_openapi_shapes() {
    let rig = rig(Some(InboundScope::Public)).await;
    let none = Vec::<String>::new();

    let search = json!({"query": {"text": "parcel tracking help"}, "federation": "auto", "pageSize": 1});
    let (status, answer) = body(post(&rig.internal, "/search", search.to_string()).await).await;
    assert_eq!(status, StatusCode::OK);
    // Without results the shape check would pass for nothing.
    assert_eq!(answer["results"].as_array().unwrap().len(), 1, "{answer}");
    assert!(answer["pageToken"].is_string(), "{answer}");
    assert!(answer.get("referrals").is_none(), "{answer}");
    assert_eq!(openapi_errors("SearchResponse", &answer), none);

    let explore = json!({"query": {"text": "parcel"}, "resultType": {"facets": [{"field": "type"}, {"field": "publisher", "limit": 1, "minCount": 0}]}});
    let (status, answer) = body(post(&rig.public, "/explore", explore.to_string()).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["facets"]["type"]["buckets"][0]["count"], 1, "{answer}");
    assert_eq!(openapi_errors("ExploreResponse", &answer), none);
    let all = json!({"resultType": {"facets": [{"field": "type", "limit": 1}]}});
    let (_, answer) = body(post(&rig.internal, "/explore", all.to_string()).await).await;
    assert_eq!(answer["facets"]["type"]["otherCount"], 1, "{answer}");
    assert_eq!(openapi_errors("ExploreResponse", &answer), none);

    let (status, answer) = body(
        agents(
            &rig.internal,
            &[
                ("filter", "type = 'application/mcp-server-card+json'"),
                ("pageSize", "1"),
            ],
        )
        .await,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (answer["items"].as_array().unwrap().len(), answer["total"].clone()),
        (1, json!(2)),
        "{answer}"
    );
    assert_eq!(openapi_errors("ListResponse", &answer), none);

    let (status, answer) = body(post(&rig.public, "/search", "{}".into()).await).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(answer["errorCode"], "INVALID_ARGUMENT");
    assert_eq!(openapi_errors("Error", &answer), none);

    // The check finds a broken shape: a score past 100, a missing source, an unknown member.
    let broken = json!({"results": [{"identifier": "urn:air:acme.com:x", "score": 101}], "next": 1});
    assert_eq!(openapi_errors("SearchResponse", &broken).len(), 3);
}

#[tokio::test]
async fn the_routes_exist_only_on_listeners_that_serve_the_registry() {
    let search = json!({"query": {"text": "parcel"}}).to_string();
    for (registry, internal, public) in [
        (None, StatusCode::NOT_FOUND, StatusCode::NOT_FOUND),
        (Some(InboundScope::Internal), StatusCode::OK, StatusCode::NOT_FOUND),
        (Some(InboundScope::Public), StatusCode::OK, StatusCode::OK),
    ] {
        let rig = rig(registry).await;
        for (listener, expected) in [(&rig.internal, internal), (&rig.public, public)] {
            assert_eq!(
                post(listener, "/search", search.clone()).await.status(),
                expected,
                "{registry:?}"
            );
            assert_eq!(agents(listener, &[]).await.status(), expected, "{registry:?}");
        }
        // The catalog of a listener lists the registry exactly where it is served.
        for (listener, expected) in [(&rig.internal, internal), (&rig.public, public)] {
            let catalog: Value = reqwest::get(format!("{}/.well-known/ard.json", listener.url()))
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            let listed = catalog["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["type"] == "application/ai-registry+json");
            assert_eq!(listed, expected == StatusCode::OK, "{registry:?} {catalog}");
        }
    }
}

#[tokio::test]
async fn the_public_registry_entry_and_its_dns_record_name_the_listener() {
    let rig = rig(Some(InboundScope::Public)).await;
    let catalog: Value = reqwest::get(format!("{}/.well-known/ard.json", rig.public.url()))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let entry = catalog["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["type"] == "application/ai-registry+json")
        .unwrap()
        .clone();
    assert_eq!(entry["identifier"], "urn:air:acme.com:registry:bunsin");
    // A client appends /search to this base URL, and the listener answers there.
    assert_eq!(entry["url"], "https://agents.acme.com/");
    let found = body(
        post(
            &rig.public,
            "/search",
            json!({"query": {"text": "registry"}}).to_string(),
        )
        .await,
    )
    .await
    .1;
    assert_eq!(
        found["results"][0]["identifier"], "urn:air:acme.com:registry:bunsin",
        "{found}"
    );
    assert_eq!(found["results"][0]["source"], "https://agents.acme.com/");

    let configs = [outer(Reach::Internal, INTERNAL_URL), outer(Reach::Public, PUBLIC_URL)];
    assert_eq!(
        bunsin_daemon::registry::svcb_records(&rig.engine, &configs),
        [
            "_search._agents.acme.com. 3600 IN SVCB 1 agents.corp.acme.com. port=8443 ; the registry of the internal listener",
            "_search._agents.acme.com. 3600 IN SVCB 1 agents.acme.com. ; the registry of the public listener",
        ]
    );
    // A listener address with a path cannot be named by a record.
    rig.engine.add_outer_listener(OuterListener {
        scope: InboundScope::Public,
        url: "https://acme.com/registry".into(),
    });
    let with_path = [outer(Reach::Public, "https://acme.com/registry")];
    let lines = bunsin_daemon::registry::svcb_records(&rig.engine, &with_path);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].starts_with("warning: --public-url has a path"), "{lines:?}");
}

#[tokio::test]
async fn requests_that_break_a_rule_get_400_and_every_answer_allows_any_origin() {
    let rig = rig(Some(InboundScope::Public)).await;
    let large = json!({"query": {"text": "x".repeat(70 * 1024)}}).to_string();
    // A filter of 21 keys, and a key of 51 values.
    let keys: Map<String, Value> = (0..21).map(|n| (format!("k{n}"), json!("v"))).collect();
    let values: Vec<String> = (0..51).map(|n| format!("v{n}")).collect();
    let facets = json!({"facets": [{"field": "type"}]});
    for (path, request) in [
        ("/search", json!({"query": {"text": "x", "filter": keys}}).to_string()),
        (
            "/search",
            json!({"query": {"text": "x", "filter": {"tags": values}}}).to_string(),
        ),
        (
            "/explore",
            json!({"query": {"filter": keys}, "resultType": facets}).to_string(),
        ),
        (
            "/explore",
            json!({"query": {"filter": {"tags": values}}, "resultType": facets}).to_string(),
        ),
        ("/search", large),
        ("/search", "not json".to_owned()),
        (
            "/search",
            json!({"query": {"text": "x"}, "federation": "upstream"}).to_string(),
        ),
        (
            "/search",
            json!({"query": {"text": "x", "filter": {"okf:taxonomy": "a"}}}).to_string(),
        ),
        ("/explore", json!({"resultType": {"facets": []}}).to_string()),
        (
            "/explore",
            json!({"resultType": {"facets": [{"field": "type"}]}, "extra": 1}).to_string(),
        ),
    ] {
        let response = post(&rig.public, path, request.clone()).await;
        assert_eq!(response.headers()["access-control-allow-origin"], "*");
        let (status, answer) = body(response).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "{path} {}",
            &request[..request.len().min(80)]
        );
        assert_eq!(answer["errorCode"], "INVALID_ARGUMENT", "{answer}");
        assert!(!answer["message"].as_str().unwrap().is_empty());
    }
    for pairs in [
        vec![("filter", "type = 'a' OR type = 'b'")],
        vec![("filter", "createdAfter > 'yesterday'")],
        vec![("orderBy", "score")],
        vec![("pageToken", "made-up")],
        vec![("limit", "5")],
        vec![("filter", "é")],
        vec![("filter", "type ≥ '배포'")],
    ] {
        let (status, answer) = body(agents(&rig.public, &pairs).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{pairs:?}");
        assert_eq!(answer["errorCode"], "INVALID_ARGUMENT", "{pairs:?}");
    }
    // Every federation mode searches the own index and gives no referrals.
    for mode in ["auto", "referrals", "none"] {
        let request = json!({"query": {"text": "parcel"}, "federation": mode}).to_string();
        let (status, answer) = body(post(&rig.public, "/search", request).await).await;
        assert_eq!(status, StatusCode::OK, "{mode}");
        assert_eq!(answer["results"][0]["identifier"], "urn:air:acme.com:lee", "{mode}");
        assert!(answer.get("referrals").is_none(), "{mode}");
    }
    // A browser asks before a JSON POST from another origin.
    let preflight = reqwest::Client::new()
        .request(reqwest::Method::OPTIONS, format!("{}/search", rig.public.url()))
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(preflight.headers()["access-control-allow-origin"], "*");
    assert_eq!(preflight.headers()["access-control-allow-methods"], "GET, POST");
    // The OpenAPI example of the list filter, with a date and a media type that holds `+`.
    let example = [(
        "filter",
        "type = 'application/mcp-server-card+json' AND createdAfter > '1970-01-01'",
    )];
    let (status, answer) = body(agents(&rig.public, &example).await).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["total"], 1, "{answer}");
}

#[tokio::test]
async fn a_listener_answers_600_requests_a_minute() {
    let rig = rig(Some(InboundScope::Public)).await;
    let client = reqwest::Client::new();
    let url = format!("{}/agents", rig.public.url());
    for index in 0..bunsin_daemon::registry::MAX_REQUESTS_PER_MINUTE {
        let status = client.get(&url).send().await.unwrap().status();
        assert_eq!(status, StatusCode::OK, "request {index}");
    }
    let response = client.get(&url).send().await.unwrap();
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    let (status, answer) = body(response).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(answer["errorCode"], "RATE_LIMIT_EXCEEDED");
    // The other listener counts its own requests.
    assert_eq!(agents(&rig.internal, &[]).await.status(), StatusCode::OK);
}

#[tokio::test]
async fn a_started_server_indexes_its_catalogs_at_once_and_stops_the_task() {
    let dir = tempfile::tempdir().unwrap();
    let engine = Engine::open(dir.path(), Arc::new(Publishers), Arc::new(|| 1_000_000)).unwrap();
    engine.serve_registry(InboundScope::Internal).unwrap();
    let engine = Arc::new(engine);
    let control = ServeConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        tls: None,
    };
    let started = start(engine, control, vec![outer(Reach::Internal, INTERNAL_URL)])
        .await
        .unwrap();
    let listener = &started.outer[0];
    // The first tick runs when the task starts. It indexes the registry's own entry.
    let mut total = json!(0);
    for _ in 0..100 {
        total = body(agents(listener, &[]).await).await.1["total"].clone();
        if total == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(total, 1);
    // The stop ends the registry task with the others; it does not wait for the next tick.
    tokio::time::timeout(std::time::Duration::from_secs(10), started.stop())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn the_serve_flag_turns_the_registry_on_and_prints_its_dns_record() {
    use std::io::{BufRead, BufReader, Read};
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_bunsind"))
        .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
        .arg(dir.path())
        .args([
            "--internal-listen",
            "127.0.0.1:0",
            "--internal-url",
            "https://agents.acme.com",
        ])
        .args(["--ard-publisher-domain", "acme.com", "--ard-registry", "internal"])
        .env_remove("BUNSIN_DATA_DIR")
        .env_remove("BUNSIN_MASTER_KEY_FILE")
        // No test may touch the real Keychain.
        .env("BUNSIN_KEY_STORE", "file")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let control = lines.next().unwrap().unwrap();
    assert!(control.starts_with("listening on "), "{control}");
    let internal = lines.next().unwrap().unwrap();
    let address = internal.strip_prefix("internal listener on ").unwrap().to_owned();
    let mut total = json!(0);
    for _ in 0..100 {
        let answer: Value = reqwest::get(format!("{address}/agents"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        total = answer["total"].clone();
        if total == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    // The first tick indexed the registry's own entry.
    assert_eq!(total, 1);
    std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(child.wait().unwrap().success());
    let mut stderr = String::new();
    child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
    assert!(
        stderr.contains(
            "_search._agents.acme.com. 3600 IN SVCB 1 agents.acme.com. ; the registry of the internal listener\n"
        ),
        "{stderr}"
    );
}

fn entry(identifier: &str) -> NewEntry {
    NewEntry {
        identifier: identifier.into(),
        publisher: "acme.com".into(),
        display_name: "x".into(),
        media_type: "application/mcp-server-card+json".into(),
        entry: json!({"identifier": identifier}),
        terms: Map::new(),
        search_text: "x".into(),
    }
}

#[test]
fn registry_status_lists_sources_and_counts() {
    let dir = tempfile::tempdir().unwrap();
    let run = || {
        std::process::Command::new(env!("CARGO_BIN_EXE_bunsind"))
            .args(["registry-status", "--data-dir"])
            .arg(dir.path())
            .env_remove("BUNSIN_DATA_DIR")
            .output()
            .unwrap()
    };
    let output = run();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "no sources: a server with --ard-registry fills the index\n"
    );
    {
        let store = RegistryStore::open(dir.path()).unwrap();
        let own = store
            .source("self:public", SourceKind::SelfCatalog, InboundScope::Public)
            .unwrap();
        store
            .replace(
                own,
                &[entry("urn:air:acme.com:kim"), entry("urn:air:acme.com:lee")],
                &[],
                1_767_225_599,
            )
            .unwrap();
        // A seed source as the crawl writes it: one read with refusals, then two failures.
        let seed = store
            .source("beta.com", SourceKind::Seed, InboundScope::Internal)
            .unwrap();
        let refusal = |identifier: &str, reason: &str| Refusal {
            identifier: identifier.into(),
            reason: reason.into(),
        };
        store
            .replace(
                seed,
                &[entry("urn:air:beta.com:a")],
                &[
                    refusal("urn:air:evil.com:a", "other_publisher"),
                    refusal("urn:air:evil.com:b", "other_publisher"),
                    refusal("x", "invalid"),
                ],
                0,
            )
            .unwrap();
        store.record_failure(seed, "HTTP 503").unwrap();
        store.record_failure(seed, "the answer is larger than 1 MiB").unwrap();
    }
    let output = run();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "source\tself:public\tself\tpublic\t2 entries\tlast ok 2025-12-31T23:59:59Z\tnext -\t0 failures\t-\n\
         source\tbeta.com\tseed\tinternal\t1 entries\tlast ok 1970-01-01T00:00:00Z\tnext 1970-01-01T00:00:00Z\t2 failures\tthe answer is larger than 1 MiB\n\
         refused\tbeta.com\tinvalid\t1\n\
         refused\tbeta.com\tother_publisher\t2\n"
    );
}

// --- The crawl of the operator's seeds ---

#[test]
fn a_bad_registry_source_stops_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let serve = |options: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_bunsind"))
            .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
            .arg(dir.path())
            .args(options)
            .env_remove("BUNSIN_DATA_DIR")
            .env("BUNSIN_KEY_STORE", "file")
            .output()
            .unwrap();
        assert!(!output.status.success(), "{options:?} started");
        String::from_utf8(output.stderr).unwrap()
    };
    for (options, expected) in [
        (
            &[
                "--ard-registry",
                "internal",
                "--ard-registry-source",
                "acme.com=http://cdn.example/ard.json",
            ][..],
            "must start with https://",
        ),
        (
            &["--ard-registry", "internal", "--ard-registry-source", "Acme.com"],
            "`Acme.com`",
        ),
        (
            &["--ard-registry", "internal", "--ard-registry-public-source", "acme.com"],
            "needs --ard-registry public",
        ),
        (&["--ard-registry-source", "acme.com"], "--ard-registry"),
    ] {
        let error = serve(options);
        assert!(error.contains(expected), "{options:?}: {error}");
    }
    // The refused starts read no key and wrote no index.
    assert!(!dir.path().join("master.key").exists());
    assert!(!dir.path().join("registry.db").exists());
}

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

/// A web server on this machine. It answers each path of its pages with a media type and a body.
/// While `failing` is set, every answer is 503. `asked` counts the requests.
struct Web {
    base: String,
    failing: Arc<AtomicBool>,
    asked: Arc<AtomicUsize>,
}

async fn web(pages: Vec<(&'static str, &'static str, String)>) -> Web {
    let failing = Arc::new(AtomicBool::new(false));
    let asked = Arc::new(AtomicUsize::new(0));
    let (fail, count) = (failing.clone(), asked.clone());
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        count.fetch_add(1, Ordering::SeqCst);
        let found = pages.iter().find(|(path, _, _)| *path == uri.path()).cloned();
        let failing = fail.load(Ordering::SeqCst);
        async move {
            use axum::response::IntoResponse;
            match found {
                _ if failing => StatusCode::SERVICE_UNAVAILABLE.into_response(),
                Some((_, media_type, body)) => ([(axum::http::header::CONTENT_TYPE, media_type)], body).into_response(),
                None => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await });
    Web { base, failing, asked }
}

/// A registry on the internal listener that crawls `seeds`, with a clock the test sets.
struct Crawler {
    dir: tempfile::TempDir,
    engine: Engine,
    clock: Arc<AtomicI64>,
    fetcher: bunsin_daemon::fetch::Fetcher,
}

const START: i64 = 1_000_000;
const HOUR: i64 = 3_600;

impl Crawler {
    fn new(seeds: &[String]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let clock = Arc::new(AtomicI64::new(START));
        let time = clock.clone();
        let engine = Engine::open(
            dir.path(),
            Arc::new(Publishers),
            Arc::new(move || time.load(Ordering::SeqCst)),
        )
        .unwrap();
        // A host under no allowed publisher domain, so the registry lists no entry of its own.
        engine.add_outer_listener(OuterListener {
            scope: InboundScope::Internal,
            url: "https://registry.example.org".into(),
        });
        engine.serve_registry(InboundScope::Internal).unwrap();
        let seeds: Vec<(String, InboundScope)> = seeds
            .iter()
            .map(|seed| (seed.clone(), InboundScope::Internal))
            .collect();
        // Test servers listen on this machine, so the operator allows its address, as on a real server.
        let upstream = bunsin_daemon::UpstreamPolicy {
            allowed_hosts: vec!["127.0.0.1".into()],
        };
        bunsin_daemon::crawl::check_seeds(&seeds, &upstream).unwrap();
        engine.set_registry_seeds(&seeds).unwrap();
        let fetcher = bunsin_daemon::fetch::Fetcher::new(&upstream).unwrap();
        Crawler {
            dir,
            engine,
            clock,
            fetcher,
        }
    }

    async fn tick(&self) {
        let (_stop, stopped) = tokio::sync::watch::channel(false);
        bunsin_daemon::crawl::tick(&self.engine, Some(&self.fetcher), &stopped).await;
    }

    fn identifiers(&self) -> Vec<String> {
        let request = bunsin_ard::parse_list(Vec::new()).unwrap();
        let answer = json!(self.engine.registry_list(InboundScope::Internal, &request).unwrap());
        answer["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["identifier"].as_str().unwrap().to_owned())
            .collect()
    }

    fn search(&self, body: Value) -> Value {
        let request = bunsin_ard::parse_search(body.to_string().as_bytes()).unwrap();
        json!(self.engine.registry_search(InboundScope::Internal, &request).unwrap())
    }

    /// The seed sources, as `registry-status` reads them.
    fn seeds(&self) -> Vec<bunsin_store::SourceStatus> {
        let store = RegistryStore::open(self.dir.path()).unwrap();
        let all = store.status().unwrap();
        all.into_iter()
            .filter(|source| source.kind == SourceKind::Seed)
            .collect()
    }
}

fn crawled_entry(identifier: &str, name: &str) -> Value {
    json!({
        "identifier": identifier,
        "displayName": name,
        "type": "application/mcp-server-card+json",
        "url": "https://agents.acme.test/agents/x/mcp/server-card",
        "description": format!("{name} answers questions"),
        "representativeQueries": ["where is my parcel", "who signs refunds"]
    })
}

/// The server as the operator runs it: an internal seed reaches only the internal listener,
/// a public seed both listeners.
#[tokio::test(flavor = "multi_thread")]
async fn each_seed_flag_reaches_the_listeners_of_its_scope() {
    use std::io::{BufRead, BufReader};
    let site = web(vec![
        (
            "/a.json",
            "application/json",
            json!({"entries": [crawled_entry("urn:air:a.test:inner", "Inner desk")]}).to_string(),
        ),
        (
            "/b.json",
            "application/json",
            json!({"entries": [crawled_entry("urn:air:b.test:outer", "Outer desk")]}).to_string(),
        ),
    ])
    .await;
    let dir = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_bunsind"))
        .args(["serve", "--listen", "127.0.0.1:0", "--data-dir"])
        .arg(dir.path())
        .args([
            "--internal-listen",
            "127.0.0.1:0",
            "--internal-url",
            "https://agents.corp.example",
        ])
        .args([
            "--public-listen",
            "127.0.0.1:0",
            "--public-url",
            "https://agents.example.org",
        ])
        .args(["--ard-registry", "public", "--allow-upstream-host", "127.0.0.1"])
        .arg("--ard-registry-source")
        .arg(format!("a.test={}/a.json", site.base))
        .arg("--ard-registry-public-source")
        .arg(format!("b.test={}/b.json", site.base))
        .env_remove("BUNSIN_DATA_DIR")
        .env_remove("BUNSIN_MASTER_KEY_FILE")
        .env("BUNSIN_KEY_STORE", "file")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut next_address = |word: &str| {
        let line = lines.next().unwrap().unwrap();
        line.strip_prefix(word).unwrap_or_else(|| panic!("{line}")).to_owned()
    };
    next_address("listening on ");
    let internal = next_address("internal listener on ");
    let public = next_address("public listener on ");
    let listed = |address: String| async move {
        let answer: Value = reqwest::get(format!("{address}/agents"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        answer["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["identifier"].as_str().unwrap().to_owned())
            .collect::<Vec<String>>()
    };
    let mut seen = (Vec::new(), Vec::new());
    for _ in 0..200 {
        seen = (listed(internal.clone()).await, listed(public.clone()).await);
        if seen.0.len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    std::process::Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(seen.0, ["urn:air:a.test:inner", "urn:air:b.test:outer"]);
    assert_eq!(seen.1, ["urn:air:b.test:outer"]);
}

#[tokio::test]
async fn a_crawl_indexes_a_manifest_and_in_page_markup() {
    let manifest = json!({"entries": [crawled_entry("urn:air:acme.test:desk", "Help desk")]});
    let markup = json!({
        "@context": ["https://agenticresourcediscovery.org/context/v1", {"acme": "https://acme.test/vocab#"}],
        "identifier": "urn:air:acme.test:payroll",
        "displayName": "Payroll helper",
        "type": "application/a2a-agent-card+json",
        "url": "https://agents.acme.test/payroll.json",
        "acme:tier": "gold"
    });
    let page = format!(
        r#"<html><head><script type="application/ld+json">{markup}</script></head><body>Payroll</body></html>"#
    );
    let site = web(vec![
        ("/ard.json", "application/json", manifest.to_string()),
        ("/payroll.html", "text/html", page),
    ])
    .await;
    let crawler = Crawler::new(&[
        format!("acme.test={}/ard.json", site.base),
        format!("acme.test={}/payroll.html", site.base),
    ]);
    crawler.tick().await;
    assert_eq!(
        crawler.identifiers(),
        ["urn:air:acme.test:desk", "urn:air:acme.test:payroll"]
    );
    // The in-page entry is found by its text, and by its namespaced term under the query's own prefix.
    let found = crawler.search(json!({"query": {"text": "payroll helper"}}));
    assert_eq!(
        found["results"][0]["identifier"], "urn:air:acme.test:payroll",
        "{found}"
    );
    assert_eq!(found["results"][0]["source"], "https://registry.example.org");
    let filtered = |tier: &str| {
        crawler.search(json!({"query": {
            "@context": {"t": "https://acme.test/vocab#"}, "text": "payroll helper", "filter": {"t:tier": tier}
        }}))["results"]
            .as_array()
            .unwrap()
            .len()
    };
    assert_eq!((filtered("gold"), filtered("silver")), (1, 0));
    // Both seeds were read once and are due again in an hour. A tick before then fetches nothing.
    let seeds = crawler.seeds();
    assert_eq!(seeds.len(), 2);
    for seed in &seeds {
        assert_eq!(
            (seed.entries, seed.last_ok_at, seed.next_fetch_at),
            (1, Some(START), START + HOUR)
        );
    }
    assert_eq!(site.asked.load(Ordering::SeqCst), 2);
    crawler.clock.store(START + HOUR - 1, Ordering::SeqCst);
    crawler.tick().await;
    assert_eq!(site.asked.load(Ordering::SeqCst), 2);
    crawler.clock.store(START + HOUR, Ordering::SeqCst);
    crawler.tick().await;
    assert_eq!(site.asked.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn a_maintenance_page_is_a_failed_read_not_an_empty_catalog() {
    let manifest = json!({"entries": [crawled_entry("urn:air:acme.test:desk", "Help desk")]});
    let site = web(vec![
        ("/ard.json", "application/json", manifest.to_string()),
        ("/down.html", "text/html", "<html><body>Back soon</body></html>".into()),
    ])
    .await;
    let crawler = Crawler::new(&[
        format!("acme.test={}/ard.json", site.base),
        format!("acme.test={}/down.html", site.base),
    ]);
    crawler.tick().await;
    assert_eq!(crawler.identifiers(), ["urn:air:acme.test:desk"]);
    let down = crawler
        .seeds()
        .into_iter()
        .find(|seed| seed.url.ends_with("/down.html"))
        .unwrap();
    // A failed read keeps the seed's entries for seven days; an empty success would remove them.
    assert_eq!(down.failures, 1);
    assert_eq!(down.last_ok_at, None);
    assert_eq!(down.last_error.as_deref(), Some("the page holds no ARD entries"));
}

#[tokio::test]
async fn a_source_with_more_than_1000_candidates_is_a_failed_read() {
    let empty = |count: usize| json!({"entries": vec![json!({}); count]}).to_string();
    let site = web(vec![
        ("/many.json", "application/json", empty(1_001)),
        ("/most.json", "application/json", empty(1_000)),
    ])
    .await;
    let crawler = Crawler::new(&[
        format!("acme.test={}/many.json", site.base),
        format!("acme.test={}/most.json", site.base),
    ]);
    crawler.tick().await;
    let seeds = crawler.seeds();
    // Past the limit nothing is read, so no refusal rows are written and earlier entries would stay.
    let many = &seeds[0];
    assert_eq!(
        (many.failures, many.last_ok_at, many.refusals.clone()),
        (1, None, vec![])
    );
    assert_eq!(
        many.last_error.as_deref(),
        Some("the source lists more than 1000 entries")
    );
    // At the limit each candidate is read, and each empty object is refused.
    let most = &seeds[1];
    assert_eq!((most.failures, most.last_ok_at), (0, Some(START)));
    assert_eq!(most.refusals, [("invalid".to_owned(), 1_000)]);
}

#[tokio::test]
async fn refused_entries_are_reported_by_reason() {
    let mut large = crawled_entry("urn:air:acme.test:large", "Large");
    large["description"] = json!("x".repeat(70 * 1024));
    let mut invalid = crawled_entry("urn:air:acme.test:nameless", "Nameless");
    invalid.as_object_mut().unwrap().remove("displayName");
    let manifest = json!({"entries": [
        crawled_entry("urn:air:acme.test:desk", "Help desk"),
        crawled_entry("urn:air:other.test:desk", "Someone else's desk"),
        invalid,
        large,
        crawled_entry("urn:air:acme.test:desk", "The same identifier again"),
    ]});
    let site = web(vec![("/ard.json", "application/json", manifest.to_string())]).await;
    let crawler = Crawler::new(&[format!("acme.test={}/ard.json", site.base)]);
    crawler.tick().await;
    assert_eq!(crawler.identifiers(), ["urn:air:acme.test:desk"]);
    // The operator's command shows the refusals by reason.
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bunsind"))
        .args(["registry-status", "--data-dir"])
        .arg(crawler.dir.path())
        .env_remove("BUNSIN_DATA_DIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let text = String::from_utf8(output.stdout).unwrap();
    let seed = format!("acme.test={}/ard.json", site.base);
    for line in [
        format!("source\t{seed}\tseed\tinternal\t1 entries\t"),
        format!("refused\t{seed}\tduplicate\t1\n"),
        format!("refused\t{seed}\tinvalid\t1\n"),
        format!("refused\t{seed}\tother_publisher\t1\n"),
        format!("refused\t{seed}\ttoo_large\t1\n"),
    ] {
        assert!(text.contains(&line), "{line:?} in {text}");
    }
}

#[tokio::test]
async fn a_failing_source_keeps_its_entries_for_seven_days() {
    let manifest = json!({"entries": [crawled_entry("urn:air:acme.test:desk", "Help desk")]});
    let site = web(vec![("/ard.json", "application/json", manifest.to_string())]).await;
    let crawler = Crawler::new(&[format!("acme.test={}/ard.json", site.base)]);
    crawler.tick().await;
    assert_eq!(crawler.identifiers(), ["urn:air:acme.test:desk"]);

    site.failing.store(true, Ordering::SeqCst);
    // Each tick runs when the seed is due. Record the wait after each failure and the entries kept.
    let mut seen = Vec::new();
    for _ in 0..20 {
        let due = crawler.seeds()[0].next_fetch_at;
        crawler.clock.store(due, Ordering::SeqCst);
        crawler.tick().await;
        let seed = crawler.seeds().remove(0);
        seen.push(((due - START) / HOUR, (seed.next_fetch_at - due) / HOUR, seed.entries));
        if seed.entries == 0 {
            break;
        }
    }
    // (hours since the last success, the wait until the next read, entries).
    assert_eq!(
        seen,
        [
            (1, 1, 1),
            (2, 2, 1),
            (4, 4, 1),
            (8, 8, 1),
            (16, 16, 1),
            (32, 24, 1),
            (56, 24, 1),
            (80, 24, 1),
            (104, 24, 1),
            (128, 24, 1),
            (152, 24, 1),
            // Seven days are 168 hours. The first failure after them removes the entries.
            (176, 24, 0),
        ]
    );
    let seed = crawler.seeds().remove(0);
    assert_eq!(seed.failures, 12);
    assert_eq!(seed.last_error.as_deref(), Some("the server answered status 503"));
    // A success brings the entries back and ends the waiting.
    site.failing.store(false, Ordering::SeqCst);
    crawler.clock.store(seed.next_fetch_at, Ordering::SeqCst);
    crawler.tick().await;
    let seed = crawler.seeds().remove(0);
    assert_eq!((seed.entries, seed.failures), (1, 0));
    assert_eq!(seed.next_fetch_at, seed.last_ok_at.unwrap() + HOUR);
}

#[tokio::test]
async fn a_catalog_seed_binds_its_named_domain_not_the_host() {
    // Bunsin A publishes an agent under acme.test on a listener of this machine.
    let dir = tempfile::tempdir().unwrap();
    let real = bunsin_daemon::RealBackends::new(Arc::new(|_| None), false, bunsin_daemon::UpstreamPolicy::default())
        .with_ard_publishers(&["acme.test".into()])
        .unwrap();
    struct Publisher(bunsin_daemon::RealBackends);
    impl Backends for Publisher {
        fn build<'a>(&'a self, _: &'a str, _: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
            Box::pin(async { Err("not needed".to_owned()) })
        }
        fn ard_publishers(&self) -> Vec<String> {
            self.0.ard_publishers()
        }
    }
    let a = Arc::new(Engine::open(dir.path(), Arc::new(Publisher(real)), Arc::new(|| START)).unwrap());
    let mut config = ProfileConfig::new("kim");
    config.inbound = InboundConfig {
        enabled: true,
        description: "Questions about parcels".into(),
        ard: Some(ArdListing {
            publisher: "acme.test".into(),
            agent_name: "kim".into(),
            representative_queries: vec!["where is my parcel".into(), "who signs refunds".into()],
            ..ArdListing::default()
        }),
        ..InboundConfig::default()
    };
    a.create_profile("kim", config).await.unwrap();
    let listener = serve_outer(a, outer(Reach::Internal, "http://127.0.0.1"), Arc::new(Notify::new()))
        .await
        .unwrap();
    let catalog = format!("{}/.well-known/ard.json", listener.url());

    // The registry reads the same catalog for acme.test and for other.test.
    let crawler = Crawler::new(&[format!("acme.test={catalog}"), format!("other.test={catalog}")]);
    crawler.tick().await;
    assert_eq!(crawler.identifiers(), ["urn:air:acme.test:kim"]);
    let seeds = crawler.seeds();
    assert_eq!((seeds[0].entries, seeds[0].refusals.clone()), (1, vec![]));
    assert_eq!(
        (seeds[1].entries, seeds[1].refusals.clone()),
        (0, vec![("other_publisher".to_owned(), 1)])
    );
}
