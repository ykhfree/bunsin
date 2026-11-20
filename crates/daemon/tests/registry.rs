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
    bunsin_daemon::crawl::tick(&engine);
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
    for (path, request) in [
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
        // A seed source as phase 2 will write it: one read with refusals, then two failures.
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
                    refusal("urn:air:evil.com:a", "binding"),
                    refusal("urn:air:evil.com:b", "binding"),
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
        "source\tself:public\tself\tpublic\t2 entries\tlast ok 2025-12-31T23:59:59Z\t0 failures\t-\n\
         source\tbeta.com\tseed\tinternal\t1 entries\tlast ok 1970-01-01T00:00:00Z\t2 failures\tthe answer is larger than 1 MiB\n\
         refused\tbeta.com\tbinding\t2\n\
         refused\tbeta.com\tinvalid\t1\n"
    );
}
