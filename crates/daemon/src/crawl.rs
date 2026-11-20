//! The registry task keeps the index of the ARD registry up to date (ARCHITECTURE 18).
//!
//! Each tick replaces the rows of the server's own catalogs. Then it reads the operator's seeds that
//! are due, one at a time, with the resolution of `bunsin_ard::resolve` through [`Fetcher`]. The engine
//! stores what a read found and schedules the next read.
//!
//! The entry sources that one resolution fetches stay under their seed: they have no rows of their
//! own. One read covers all of them within one time limit, and they share the seed's binding domain.

use crate::UpstreamPolicy;
use crate::fetch::Fetcher;
use bunsin_ard::{Accepted, Rejected, Resolution, Seed};
use bunsin_core::inbound::InboundScope;
use bunsin_engine::Engine;
use std::time::Duration;
use url::Url;

/// How often the registry task runs.
pub const CRAWL_TICK: Duration = Duration::from_secs(60);
/// The most seeds that the operator may name.
pub const MAX_SEEDS: usize = 200;

/// Reads the value of a seed flag: `<domain>`, or `<domain>=<URL>` for one catalog address that
/// speaks for the domain. The domain is the binding domain in both forms.
pub fn parse_seed(text: &str) -> Result<Seed, String> {
    match text.split_once('=') {
        None => Seed::domain(text),
        Some((domain, address)) => {
            let url = Url::parse(address).map_err(|e| format!("`{address}` is not an address: {e}"))?;
            Seed::catalog(domain, url)
        }
    }
}

/// Checks the seeds before the server starts: their form, their count, a seed given twice, and the
/// address that each one fetches first, by the server's rules for upstream addresses.
pub fn check_seeds(seeds: &[(String, InboundScope)], upstream: &UpstreamPolicy) -> Result<(), String> {
    if seeds.len() > MAX_SEEDS {
        return Err(format!(
            "there are {} registry sources; at most {MAX_SEEDS} are allowed",
            seeds.len()
        ));
    }
    for (index, (text, _)) in seeds.iter().enumerate() {
        if seeds[..index].iter().any(|(earlier, _)| earlier == text) {
            return Err(format!("the registry source `{text}` is given twice"));
        }
        let seed = parse_seed(text).map_err(|e| format!("the registry source `{text}` cannot be used: {e}"))?;
        let first = match seed.catalog_url() {
            Some(url) => url.to_string(),
            None => format!("https://{}/", seed.binding()),
        };
        upstream.check(&format!("the registry source `{text}`"), &first)?;
    }
    Ok(())
}

/// One run of the registry task. A failure is written to stderr and does not stop the task.
/// Without a fetcher, only the own catalogs are indexed. `stopped` ends the run between two seeds.
/// A run starts no new seed after [`CRAWL_TICK`], so the own catalogs are indexed again soon;
/// the seeds left stay due for the next run.
pub async fn tick(engine: &Engine, fetcher: Option<&Fetcher>, stopped: &tokio::sync::watch::Receiver<bool>) {
    let began = std::time::Instant::now();
    if let Err(error) = engine.refresh_own_catalogs() {
        eprintln!("the registry could not index the catalogs of this server: {error}");
    }
    let Some(fetcher) = fetcher else { return };
    let mut read = Vec::new();
    while !*stopped.borrow() && began.elapsed() < CRAWL_TICK {
        let (source, text) = match engine.due_registry_seed() {
            Ok(Some(due)) => due,
            Ok(None) => break,
            Err(error) => {
                eprintln!("the registry could not find the next source: {error}");
                break;
            }
        };
        // A seed whose schedule could not be stored would be due again at once.
        if read.contains(&source) {
            break;
        }
        read.push(source);
        if let Err(error) = crawl(engine, fetcher, source, &text).await {
            eprintln!("the registry could not store the read of `{text}`: {error}");
        }
    }
}

/// Reads one seed and hands the outcome to the engine.
async fn crawl(engine: &Engine, fetcher: &Fetcher, source: i64, text: &str) -> bunsin_api::ApiResult<()> {
    let seed = match parse_seed(text) {
        Ok(seed) => seed,
        Err(error) => return engine.crawl_failed(source, &error),
    };
    match fetcher.resolve(&seed).await.and_then(read_entries) {
        Ok(found) => engine.crawl_succeeded(source, found),
        Err(error) => engine.crawl_failed(source, &error),
    }
}

/// The entries of a resolution. It succeeds only if the root page and `robots.txt` could be read
/// or were absent, and every entry source could be read. So a page or a source that is down for a
/// while does not remove entries. Otherwise the first error is the failure: Bunsin text and an
/// HTTP status only.
fn read_entries(resolution: Resolution) -> Result<Vec<Result<Accepted, Rejected>>, String> {
    if let Some(error) = resolution.discovery_error {
        return Err(error.to_string());
    }
    if resolution.sources.is_empty() {
        return Err("the source announced no entry source".into());
    }
    let mut found = Vec::new();
    for read in resolution.sources {
        let entries = read.result.map_err(|error| error.to_string())?;
        found.extend(entries.accepted.into_iter().map(Ok));
        found.extend(entries.rejected.into_iter().map(Err));
    }
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunsin_ard::{DiscoveryError, FetchError, Found, Source, SourceError, accept_entry};
    use serde_json::json;

    fn source(url: &str, result: Result<Found, SourceError>) -> Source {
        Source {
            url: Url::parse(url).unwrap(),
            result,
        }
    }

    /// A read source with one accepted and one refused entry.
    fn read(name: &str) -> Source {
        let entry = |identifier: String| {
            accept_entry(
                &json!({"identifier": identifier, "displayName": name, "type": "text/html", "url": "https://acme.com/"}),
                "acme.com",
            )
        };
        let found = Found {
            accepted: vec![entry(format!("urn:air:acme.com:{name}")).unwrap()],
            rejected: vec![entry(format!("urn:air:other.com:{name}")).unwrap_err()],
        };
        source(&format!("https://acme.com/{name}.json"), Ok(found))
    }

    fn resolution(sources: Vec<Source>, discovery_error: Option<DiscoveryError>) -> Resolution {
        Resolution {
            domain: "acme.com".into(),
            sources,
            discovery_error,
        }
    }

    #[test]
    fn a_read_takes_every_source_or_fails_with_the_first_error() {
        let found = read_entries(resolution(vec![read("a"), read("b")], None)).unwrap();
        let identifiers: Vec<(bool, String)> = found
            .iter()
            .map(|outcome| match outcome {
                Ok(accepted) => (true, accepted.entry.identifier.clone()),
                Err(rejected) => (false, rejected.identifier.clone().unwrap()),
            })
            .collect();
        assert_eq!(
            identifiers,
            [
                (true, "urn:air:acme.com:a".to_owned()),
                (false, "urn:air:other.com:a".to_owned()),
                (true, "urn:air:acme.com:b".to_owned()),
                (false, "urn:air:other.com:b".to_owned()),
            ]
        );
        // A source that fails fails the read, wherever it is.
        let down = source(
            "https://acme.com/down.json",
            Err(SourceError::Fetch(FetchError::Status(503))),
        );
        let error = read_entries(resolution(vec![read("a"), down, read("b")], None)).unwrap_err();
        assert_eq!(error, "the server answered status 503");
        let many = source("https://acme.com/many.json", Err(SourceError::TooManyEntries));
        let error = read_entries(resolution(vec![many], None)).unwrap_err();
        assert_eq!(error, "the source lists more than 1000 entries");
        assert_eq!(
            read_entries(resolution(Vec::new(), None)).unwrap_err(),
            "the source announced no entry source"
        );
    }

    #[test]
    fn a_page_that_announces_sources_and_cannot_be_read_fails_the_read() {
        // The well-known source was read, but the root page did not answer: its links are unknown.
        let unread = DiscoveryError {
            url: Url::parse("https://acme.com/").unwrap(),
            error: FetchError::Transport("connection reset".into()),
        };
        let error = read_entries(resolution(vec![read("well-known")], Some(unread))).unwrap_err();
        assert_eq!(
            error,
            "cannot read `https://acme.com/`, which may announce entry sources: cannot reach the address: connection reset"
        );
        // The error also stands when no source was found.
        let unread = DiscoveryError {
            url: Url::parse("https://acme.com/robots.txt").unwrap(),
            error: FetchError::Status(500),
        };
        let error = read_entries(resolution(Vec::new(), Some(unread))).unwrap_err();
        assert!(
            error.starts_with("cannot read `https://acme.com/robots.txt`"),
            "{error}"
        );
    }

    #[test]
    fn a_seed_is_a_domain_or_a_domain_with_one_catalog_address() {
        let seed = parse_seed("acme.com").unwrap();
        assert_eq!((seed.binding(), seed.catalog_url()), ("acme.com", None));
        let seed = parse_seed("acme.com=https://agents.acme.com/.well-known/ard.json").unwrap();
        assert_eq!(seed.binding(), "acme.com");
        assert_eq!(
            seed.catalog_url().map(Url::as_str),
            Some("https://agents.acme.com/.well-known/ard.json")
        );
        for wrong in [
            "https://acme.com/ard.json",
            "Acme.com",
            "acme.com=not a url",
            "=https://acme.com/",
            "",
        ] {
            assert!(parse_seed(wrong).is_err(), "{wrong}");
        }
    }

    #[test]
    fn the_seeds_pass_the_upstream_rules_and_the_count_before_the_start() {
        let open = UpstreamPolicy::default();
        let internal = |text: &str| vec![(text.to_owned(), InboundScope::Internal)];
        assert!(check_seeds(&internal("acme.com"), &open).is_ok());
        assert!(check_seeds(&internal("acme.com=https://cdn.example.net/ard.json"), &open).is_ok());
        for refused in [
            "acme.com=http://cdn.example.net/ard.json",
            "acme.com=https://127.0.0.1/ard.json",
            "localhost",
        ] {
            let error = check_seeds(&internal(refused), &open).unwrap_err();
            assert!(error.contains(&format!("`{refused}`")), "{error}");
        }
        // With a host list, only the listed hosts pass, also over http.
        let listed = UpstreamPolicy {
            allowed_hosts: vec!["127.0.0.1".into()],
        };
        assert!(check_seeds(&internal("acme.test=http://127.0.0.1:8080/ard.json"), &listed).is_ok());
        assert!(check_seeds(&internal("acme.com"), &listed).is_err());
        let many: Vec<(String, InboundScope)> = (0..=MAX_SEEDS)
            .map(|n| (format!("d{n}.example"), InboundScope::Public))
            .collect();
        assert!(check_seeds(&many, &open).unwrap_err().contains("at most 200"));
        assert!(check_seeds(&many[..MAX_SEEDS], &open).is_ok());
        // The same seed twice, in one flag or in both, is refused.
        let twice = [
            ("acme.com".to_owned(), InboundScope::Internal),
            ("acme.com".to_owned(), InboundScope::Public),
        ];
        assert!(check_seeds(&twice, &open).unwrap_err().contains("given twice"));
    }
}
