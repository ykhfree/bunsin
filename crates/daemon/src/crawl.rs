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
use bunsin_ard::Seed;
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

/// Checks the seeds before the server starts: their form, their count, and the address that each
/// one fetches first, by the server's rules for upstream addresses.
pub fn check_seeds(seeds: &[(String, InboundScope)], upstream: &UpstreamPolicy) -> Result<(), String> {
    if seeds.len() > MAX_SEEDS {
        return Err(format!(
            "there are {} registry sources; at most {MAX_SEEDS} are allowed",
            seeds.len()
        ));
    }
    for (text, _) in seeds {
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
pub async fn tick(engine: &Engine, fetcher: Option<&Fetcher>, stopped: &tokio::sync::watch::Receiver<bool>) {
    if let Err(error) = engine.refresh_own_catalogs() {
        eprintln!("the registry could not index the catalogs of this server: {error}");
    }
    let Some(fetcher) = fetcher else { return };
    let mut read = Vec::new();
    while !*stopped.borrow() {
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

/// Reads one seed and hands the outcome to the engine. A read succeeds if one entry source
/// could be read. Otherwise the first error is the failure: Bunsin text and an HTTP status only.
async fn crawl(engine: &Engine, fetcher: &Fetcher, source: i64, text: &str) -> bunsin_api::ApiResult<()> {
    let seed = match parse_seed(text) {
        Ok(seed) => seed,
        Err(error) => return engine.crawl_failed(source, &error),
    };
    let resolution = match fetcher.resolve(&seed).await {
        Ok(resolution) => resolution,
        Err(timeout) => return engine.crawl_failed(source, &timeout),
    };
    let mut found = Vec::new();
    let mut first_error = None;
    let mut read_any = false;
    for source in resolution.sources {
        match source.result {
            Ok(entries) => {
                read_any = true;
                found.extend(entries.accepted.into_iter().map(Ok));
                found.extend(entries.rejected.into_iter().map(Err));
            }
            Err(error) => {
                first_error.get_or_insert(error.to_string());
            }
        }
    }
    if read_any {
        engine.crawl_succeeded(source, found)
    } else {
        let error = first_error.unwrap_or_else(|| "the source announced no entry source".into());
        engine.crawl_failed(source, &error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    }
}
