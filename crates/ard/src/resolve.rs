//! The Consumer's resolution of a domain's entries (§5.1), and the acceptance of one entry.
//!
//! This module does no HTTP itself. The caller passes a [`Fetch`], which owns the network rules:
//! address checks on every redirect, time limits and the body limit. So the resolution order and
//! the acceptance rules can be tested without a network, and a crawl can reuse them.
//!
//! Resolution of a domain, in this order:
//! 1. `https://<domain>/.well-known/ard.json` (§5.1 MUST).
//! 2. `https://<domain>/`: its `Link` headers and `<link rel="ard">` tags (§5.1 MUST honour).
//! 3. `https://<domain>/robots.txt`: its `Agentmap:` lines.
//! 4. The addresses that 2 and 3 announced, at most [`MAX_ANNOUNCED`]. Their own links are not followed.
//!
//! All steps run: step 1 does not replace step 2. A JSON body is a manifest. Any other body is read
//! as a page with JSON-LD blocks. Not consulted: `/.well-known/ai-catalog.json` (§5.1 MAY) and DNS,
//! whose records have no parameters that name an entry source.
//!
//! A 404 or 410 means that nothing is there. So a domain may publish only through links or
//! `Agentmap` lines (§5.1): a missing `ard.json` is no source. Any other error of the root page or
//! `robots.txt` leaves the announced sources unknown, and [`Resolution::discovery_error`] holds it.
//!
//! An entry is accepted only if its URN publisher is the binding domain: the domain that was
//! resolved, or the domain that the owner gave with a catalog address. It is not the host that
//! served the entry, so a link to a CDN works. A parent domain is not accepted: without a public
//! suffix list, `evil.github.io` could claim `urn:air:github.io:...`. The binding takes the place
//! of the defense of §4.5.1, with the domain that TLS confirmed.

use crate::entry::{ArdEntry, Level, Violation, urn_publisher, validate};
use crate::html::{markup_entries, read_markup};
use crate::jsonld::{UnsupportedContext, expand_terms};
use serde_json::{Map, Value};
use std::fmt;
use url::Url;

/// The largest entry that is accepted, in bytes of JSON.
pub const MAX_ENTRY_BYTES: usize = 64 * 1024;
/// The most announced addresses that one resolution fetches.
pub const MAX_ANNOUNCED: usize = 10;
/// The most entries that one source may list, twice the 500 entries that the registry keeps of a
/// source. More is a source error, so a source cannot make an unbounded count of refusals.
pub const MAX_SOURCE_CANDIDATES: usize = 1_000;

/// What one fetch got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    /// The address after redirects. Relative links resolve against it.
    pub url: Url,
    /// The values of the `Link` response headers.
    pub links: Vec<String>,
    pub body: Vec<u8>,
}

/// Why a fetch failed. The text is Bunsin's own; it holds no text of the answer.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// An address rule refused the address or a redirect target.
    #[error("{0}")]
    Refused(String),
    /// The answer had a status other than success.
    #[error("the server answered status {0}")]
    Status(u16),
    #[error("the body is larger than {0} bytes")]
    TooLarge(usize),
    #[error("cannot reach the address: {0}")]
    Transport(String),
}

/// Gets one address with `GET`. The implementation sets the network rules.
pub trait Fetch: Sync {
    fn get(&self, url: &Url) -> impl Future<Output = Result<Fetched, FetchError>> + Send;
}

/// What to resolve, and the domain that its entries must name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seed {
    domain: String,
    catalog: Option<Url>,
}

impl Seed {
    /// A domain to resolve in the order of this module.
    pub fn domain(domain: &str) -> Result<Seed, String> {
        Ok(Seed {
            domain: binding_domain(domain)?,
            catalog: None,
        })
    }

    /// One catalog address that speaks for `domain`. Nothing else is fetched.
    pub fn catalog(domain: &str, catalog: Url) -> Result<Seed, String> {
        Ok(Seed {
            domain: binding_domain(domain)?,
            catalog: Some(catalog),
        })
    }

    /// The binding domain.
    pub fn binding(&self) -> &str {
        &self.domain
    }

    pub fn catalog_url(&self) -> Option<&Url> {
        self.catalog.as_ref()
    }

    fn origin(&self) -> Url {
        Url::parse(&format!("https://{}/", self.domain)).expect("a checked domain makes a URL")
    }
}

/// Checks that `domain` is a host name alone, in lower case.
fn binding_domain(domain: &str) -> Result<String, String> {
    let url = Url::parse(&format!("https://{domain}/")).map_err(|_| format!("`{domain}` is not a domain"))?;
    match url.host() {
        Some(url::Host::Domain(host)) if host == domain && url.path() == "/" => Ok(domain.to_owned()),
        _ => Err(format!("`{domain}` is not a domain in lower case")),
    }
}

/// The outcome of a resolution.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolution {
    /// The binding domain.
    pub domain: String,
    /// Each entry source that was fetched, in fetch order. The root page and `robots.txt` only
    /// announce sources, so they are not here. A well-known `ard.json` that answered 404 or 410 is
    /// not here either.
    pub sources: Vec<Source>,
    /// The first error of the root page or `robots.txt`, other than 404 or 410. With it, the
    /// sources that the page announces are unknown, so `sources` may be incomplete.
    pub discovery_error: Option<DiscoveryError>,
}

/// A page that announces entry sources could not be read.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("cannot read `{url}`, which may announce entry sources: {error}")]
pub struct DiscoveryError {
    pub url: Url,
    pub error: FetchError,
}

/// True if the error says that nothing is at the address: 404 or 410.
fn nothing_there(error: &FetchError) -> bool {
    matches!(error, FetchError::Status(404 | 410))
}

/// One fetched entry source.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// The address that was asked for.
    pub url: Url,
    pub result: Result<Found, SourceError>,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SourceError {
    #[error(transparent)]
    Fetch(#[from] FetchError),
    #[error("the JSON body is not an ARD manifest with an `entries` array")]
    NotAManifest,
    /// A page that is no manifest and holds no entry markup, for example a maintenance page.
    #[error("the page holds no ARD entries")]
    NoEntries,
    #[error("the source lists more than {MAX_SOURCE_CANDIDATES} entries")]
    TooManyEntries,
}

/// The entries of one source.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Found {
    pub accepted: Vec<Accepted>,
    pub rejected: Vec<Rejected>,
}

/// An entry that passed [`accept_entry`].
#[derive(Debug, Clone, PartialEq)]
pub struct Accepted {
    pub entry: ArdEntry,
    /// Each top-level term by its IRI, with the value as published.
    pub terms: Map<String, Value>,
    /// The URN publisher, as written.
    pub publisher: String,
}

/// An entry that [`accept_entry`] refused.
#[derive(Debug, Clone, PartialEq)]
pub struct Rejected {
    /// The `identifier` string, if the entry has one. It is the publisher's text.
    pub identifier: Option<String>,
    pub reason: RejectReason,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RejectReason {
    /// The entry is larger than [`MAX_ENTRY_BYTES`].
    TooLarge,
    NotAnObject,
    /// A term has a JSON type that the entry model does not take.
    Malformed(String),
    UnsupportedContext(UnsupportedContext),
    /// The MUST rules that the entry breaks.
    Invalid(Vec<Violation>),
    /// The URN publisher is not the binding domain.
    OtherPublisher {
        publisher: String,
        domain: String,
    },
}

impl fmt::Display for RejectReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RejectReason::TooLarge => write!(f, "the entry is larger than {MAX_ENTRY_BYTES} bytes"),
            RejectReason::NotAnObject => f.write_str("the entry is not a JSON object"),
            RejectReason::Malformed(error) => write!(f, "a term has the wrong JSON type: {error}"),
            RejectReason::UnsupportedContext(error) => error.fmt(f),
            RejectReason::Invalid(violations) => {
                let texts: Vec<String> = violations.iter().map(Violation::to_string).collect();
                f.write_str(&texts.join("; "))
            }
            RejectReason::OtherPublisher { publisher, domain } => write!(
                f,
                "the publisher `{publisher}` is not the domain `{domain}` that this source speaks for"
            ),
        }
    }
}

/// Accepts one entry for the binding `domain`, or says why not.
///
/// The rules: at most [`MAX_ENTRY_BYTES`], a supported `@context`, no MUST violation of
/// [`validate`], and a URN publisher equal to `domain`, ignoring ASCII case. SHOULD violations
/// do not refuse an entry.
pub fn accept_entry(raw: &Value, domain: &str) -> Result<Accepted, Rejected> {
    let identifier = raw.get("identifier").and_then(Value::as_str).map(str::to_owned);
    let reject = |reason| Rejected {
        identifier: identifier.clone(),
        reason,
    };
    let size = serde_json::to_vec(raw).map_or(usize::MAX, |bytes| bytes.len());
    if size > MAX_ENTRY_BYTES {
        return Err(reject(RejectReason::TooLarge));
    }
    let Value::Object(node) = raw else {
        return Err(reject(RejectReason::NotAnObject));
    };
    let terms = expand_terms(node).map_err(|e| reject(RejectReason::UnsupportedContext(e)))?;
    let entry: ArdEntry =
        serde_json::from_value(raw.clone()).map_err(|e| reject(RejectReason::Malformed(e.to_string())))?;
    let broken: Vec<Violation> = validate(&entry)
        .into_iter()
        .filter(|v| v.level() == Level::Must)
        .collect();
    if !broken.is_empty() {
        return Err(reject(RejectReason::Invalid(broken)));
    }
    let publisher = urn_publisher(&entry.identifier)
        .expect("validate refuses an identifier without a publisher")
        .to_owned();
    if !publisher.eq_ignore_ascii_case(domain) {
        return Err(reject(RejectReason::OtherPublisher {
            publisher,
            domain: domain.to_owned(),
        }));
    }
    Ok(Accepted {
        entry,
        terms,
        publisher,
    })
}

/// Resolves a seed. A source or a page that fails is in the result with its error; it stops nothing.
pub async fn resolve<F: Fetch>(fetch: &F, seed: &Seed) -> Resolution {
    let domain = seed.binding();
    let mut sources = Vec::new();
    let mut discovery_error = None;
    if let Some(catalog) = seed.catalog_url() {
        sources.push(source(fetch, catalog, domain).await);
        return Resolution {
            domain: domain.to_owned(),
            sources,
            discovery_error,
        };
    }
    let origin = seed.origin();
    let well_known = origin.join(".well-known/ard.json").expect("a fixed path joins");
    let first = source(fetch, &well_known, domain).await;
    if !matches!(&first.result, Err(SourceError::Fetch(error)) if nothing_there(error)) {
        sources.push(first);
    }

    let mut announced = Vec::new();
    if let Some(page) = announcer(fetch, &origin, &mut discovery_error).await {
        let markup = read_markup(&String::from_utf8_lossy(&page.body));
        for target in ard_link_targets(&page.links).into_iter().chain(markup.links) {
            announced.extend(page.url.join(&target).ok());
        }
    }
    let robots = origin.join("robots.txt").expect("a fixed path joins");
    if let Some(file) = announcer(fetch, &robots, &mut discovery_error).await {
        for target in agentmap_targets(&String::from_utf8_lossy(&file.body)) {
            announced.extend(file.url.join(target).ok());
        }
    }
    let mut fetched = vec![well_known];
    for mut url in announced {
        url.set_fragment(None);
        if fetched.len() > MAX_ANNOUNCED || fetched.contains(&url) || !["http", "https"].contains(&url.scheme()) {
            continue;
        }
        sources.push(source(fetch, &url, domain).await);
        fetched.push(url);
    }
    Resolution {
        domain: domain.to_owned(),
        sources,
        discovery_error,
    }
}

/// Gets a page that announces sources. A 404 or 410 announces nothing. Another error goes into
/// `error`, unless an earlier page already put one there.
async fn announcer<F: Fetch>(fetch: &F, url: &Url, error: &mut Option<DiscoveryError>) -> Option<Fetched> {
    match fetch.get(url).await {
        Ok(page) => Some(page),
        Err(failed) => {
            if !nothing_there(&failed) {
                error.get_or_insert(DiscoveryError {
                    url: url.clone(),
                    error: failed,
                });
            }
            None
        }
    }
}

async fn source<F: Fetch>(fetch: &F, url: &Url, domain: &str) -> Source {
    let result = match fetch.get(url).await {
        Ok(fetched) => read_document(&fetched.body, domain),
        Err(error) => Err(error.into()),
    };
    Source {
        url: url.clone(),
        result,
    }
}

/// Reads a manifest, or the entries of a page. A body that starts with `{` is a manifest.
fn read_document(body: &[u8], domain: &str) -> Result<Found, SourceError> {
    let text = String::from_utf8_lossy(body);
    let text = text.trim_start_matches(['\u{feff}', ' ', '\t', '\r', '\n']);
    let candidates = if text.starts_with('{') {
        // Each entry is read alone, so that one bad entry does not refuse the others.
        let manifest: Value = serde_json::from_str(text).map_err(|_| SourceError::NotAManifest)?;
        match manifest.get("entries") {
            Some(Value::Array(entries)) => entries.clone(),
            _ => return Err(SourceError::NotAManifest),
        }
    } else {
        // A manifest may list no entries on purpose; a page without entry markup is not a source.
        let entries = markup_entries(&read_markup(text).blocks);
        if entries.is_empty() {
            return Err(SourceError::NoEntries);
        }
        entries
    };
    if candidates.len() > MAX_SOURCE_CANDIDATES {
        return Err(SourceError::TooManyEntries);
    }
    let mut found = Found::default();
    for candidate in &candidates {
        match accept_entry(candidate, domain) {
            Ok(accepted) => found.accepted.push(accepted),
            Err(rejected) => found.rejected.push(rejected),
        }
    }
    Ok(found)
}

/// The targets of `Link` header values whose `rel` holds the token `ard` (RFC 8288).
fn ard_link_targets(values: &[String]) -> Vec<String> {
    let mut targets = Vec::new();
    for value in values {
        let mut rest = value.as_str();
        loop {
            rest = rest.trim_start_matches([' ', '\t', ',']);
            let Some(after) = rest.strip_prefix('<') else { break };
            let Some(end) = after.find('>') else { break };
            let target = after[..end].trim();
            rest = &after[end + 1..];
            // The parameters run to the next comma outside quotes.
            let mut quoted = false;
            let end = rest
                .char_indices()
                .find(|&(_, c)| {
                    if c == '"' {
                        quoted = !quoted;
                    }
                    c == ',' && !quoted
                })
                .map_or(rest.len(), |(i, _)| i);
            let params = &rest[..end];
            rest = &rest[end..];
            let ard = params
                .split(';')
                .filter_map(|p| p.split_once('='))
                .any(|(name, value)| {
                    name.trim().eq_ignore_ascii_case("rel")
                        && value
                            .trim()
                            .trim_matches('"')
                            .split_ascii_whitespace()
                            .any(|t| t.eq_ignore_ascii_case("ard"))
                });
            if ard {
                targets.push(target.to_owned());
            }
        }
    }
    targets
}

/// The values of the `Agentmap:` lines of a `robots.txt`. A `#` starts a comment.
fn agentmap_targets(robots: &str) -> Vec<&str> {
    robots
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next().unwrap_or_default();
            let (field, value) = line.split_once(':')?;
            let value = value.trim();
            (field.trim().eq_ignore_ascii_case("agentmap") && !value.is_empty()).then_some(value)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MAX_RESPONSE_BYTES;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Answers by address. An address that is not here answers 404.
    #[derive(Default)]
    struct Web {
        pages: BTreeMap<String, Result<Fetched, FetchError>>,
        asked: Mutex<Vec<String>>,
    }

    impl Web {
        fn page(mut self, url: &str, body: &str) -> Self {
            self.pages.insert(url.into(), Ok(fetched(url, body)));
            self
        }

        fn with(mut self, url: &str, answer: Fetched) -> Self {
            self.pages.insert(url.into(), Ok(answer));
            self
        }

        fn failing(mut self, url: &str, error: FetchError) -> Self {
            self.pages.insert(url.into(), Err(error));
            self
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    fn fetched(url: &str, body: &str) -> Fetched {
        Fetched {
            url: Url::parse(url).unwrap(),
            links: Vec::new(),
            body: body.as_bytes().to_vec(),
        }
    }

    impl Fetch for Web {
        fn get(&self, url: &Url) -> impl Future<Output = Result<Fetched, FetchError>> + Send {
            self.asked.lock().unwrap().push(url.to_string());
            let answer = self
                .pages
                .get(url.as_str())
                .cloned()
                .unwrap_or(Err(FetchError::Status(404)));
            async move { answer }
        }
    }

    fn entry(identifier: &str) -> Value {
        json!({
            "identifier": identifier,
            "displayName": "Kim",
            "type": "application/mcp-server-card+json",
            "url": "https://agents.acme.com/agents/kim/mcp",
            "representativeQueries": ["what is due", "who approves"]
        })
    }

    fn manifest(identifiers: &[&str]) -> String {
        json!({"entries": identifiers.iter().map(|id| entry(id)).collect::<Vec<_>>()}).to_string()
    }

    fn accepted(resolution: &Resolution) -> Vec<String> {
        let sources = resolution.sources.iter().filter_map(|s| s.result.as_ref().ok());
        sources
            .flat_map(|found| found.accepted.iter().map(|a| a.entry.identifier.clone()))
            .collect()
    }

    fn rejected(resolution: &Resolution) -> Vec<(String, String)> {
        let sources = resolution.sources.iter().filter_map(|s| s.result.as_ref().ok());
        sources
            .flat_map(|found| found.rejected.iter())
            .map(|r| (r.identifier.clone().unwrap_or_default(), r.reason.to_string()))
            .collect()
    }

    fn run<T>(future: impl Future<Output = T>) -> T {
        // The fake web answers at once, so a plain poll loop is enough.
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
        }
    }

    #[test]
    fn the_binding_domain_is_the_resolved_domain_not_the_fetched_host() {
        // acme.com announces a catalog on a CDN. Its entries name acme.com, and one names the CDN.
        let cdn = "https://cdn.example.net/acme/ard.json";
        let web = Web::default()
            .with(
                "https://acme.com/",
                Fetched {
                    links: vec![format!("<{cdn}>; rel=\"ard\"")],
                    ..fetched("https://acme.com/", "<html></html>")
                },
            )
            .page(cdn, &manifest(&["urn:air:acme.com:kim", "urn:air:cdn.example.net:x"]));
        let resolution = run(resolve(&web, &Seed::domain("acme.com").unwrap()));
        assert_eq!(resolution.domain, "acme.com");
        assert_eq!(accepted(&resolution), ["urn:air:acme.com:kim"]);
        let refused = rejected(&resolution);
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].0, "urn:air:cdn.example.net:x");
        assert!(
            refused[0].1.contains("is not the domain `acme.com`"),
            "{}",
            refused[0].1
        );

        // A subdomain cannot speak for its parent domain. The case of the publisher does not matter.
        let web = Web::default().page(
            "https://evil.github.io/.well-known/ard.json",
            &manifest(&["urn:air:github.io:pages", "urn:air:Evil.GitHub.io:mine"]),
        );
        let resolution = run(resolve(&web, &Seed::domain("evil.github.io").unwrap()));
        assert_eq!(accepted(&resolution), ["urn:air:Evil.GitHub.io:mine"]);
        assert_eq!(rejected(&resolution)[0].0, "urn:air:github.io:pages");

        // A catalog seed binds the domain that the owner gave, not the host of the catalog.
        let listener = "https://agents.acme.com/.well-known/ard.json";
        let web = Web::default().page(listener, &manifest(&["urn:air:acme.com:kim"]));
        let url = Url::parse(listener).unwrap();
        let by_domain = run(resolve(&web, &Seed::catalog("acme.com", url.clone()).unwrap()));
        assert_eq!(accepted(&by_domain), ["urn:air:acme.com:kim"]);
        let by_host = run(resolve(&web, &Seed::catalog("agents.acme.com", url).unwrap()));
        assert!(accepted(&by_host).is_empty());
        assert_eq!(rejected(&by_host)[0].0, "urn:air:acme.com:kim");
        // A catalog seed fetches its one address and nothing else.
        assert_eq!(web.asked(), [listener, listener]);
    }

    #[test]
    fn a_domain_is_resolved_by_the_well_known_manifest_links_and_agentmaps() {
        let page = format!(
            r#"<html><head><link rel="ard" href="/page-link.json">
            <script type="application/ld+json">{}</script></head></html>"#,
            json!({"@context": "https://agenticresourcediscovery.org/context/v1", "identifier": "urn:air:acme.com:root-markup",
                   "displayName": "Root", "type": "text/html", "url": "https://acme.com/"})
        );
        let markup_page = format!(
            r#"<html><head><link rel="ard" href="/not-followed.json"></head><body>
            <script type="application/ld+json">{}</script></body></html>"#,
            json!([{"identifier": "urn:air:acme.com:in-page", "displayName": "In page",
                    "type": "application/a2a-agent-card+json", "url": "https://acme.com/card.json"}])
        );
        let web = Web::default()
            .page("https://acme.com/.well-known/ard.json", &manifest(&["urn:air:acme.com:well-known"]))
            .with(
                "https://acme.com/",
                Fetched {
                    // The root redirected. Relative links resolve against the final address.
                    url: Url::parse("https://www.acme.com/home/").unwrap(),
                    links: vec![
                        "<https://acme.com/style.css>; rel=preload, <header.json>; rel=\"alternate ARD\"".into(),
                        "</.well-known/ard.json#x>; rel=ard".into(),
                    ],
                    body: page.into_bytes(),
                },
            )
            .page(
                "https://acme.com/robots.txt",
                "User-agent: *\nDisallow:\n# Agentmap: https://acme.com/commented.json\nAGENTMAP: /markup.html\nSitemap: /sitemap.xml\n",
            )
            .page("https://www.acme.com/home/header.json", &manifest(&["urn:air:acme.com:header"]))
            .page("https://www.acme.com/page-link.json", &manifest(&["urn:air:acme.com:page-link"]))
            .page("https://acme.com/markup.html", &markup_page);
        let resolution = run(resolve(&web, &Seed::domain("acme.com").unwrap()));
        assert_eq!(
            accepted(&resolution),
            [
                "urn:air:acme.com:well-known",
                "urn:air:acme.com:header",
                "urn:air:acme.com:page-link",
                "urn:air:acme.com:in-page"
            ]
        );
        let urls: Vec<&str> = resolution.sources.iter().map(|s| s.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://acme.com/.well-known/ard.json",
                "https://www.acme.com/home/header.json",
                // The Link header names the well-known address of the redirected host: a new address.
                "https://www.acme.com/.well-known/ard.json",
                "https://www.acme.com/page-link.json",
                "https://acme.com/markup.html"
            ]
        );
        assert_eq!(
            resolution.sources[2].result,
            Err(SourceError::Fetch(FetchError::Status(404)))
        );
        // The root page's own JSON-LD, the comment in robots.txt and the links of an announced page
        // are not entry sources.
        let asked = web.asked();
        assert!(
            !asked
                .iter()
                .any(|u| u.contains("commented") || u.contains("not-followed")),
            "{asked:?}"
        );
        assert!(!accepted(&resolution).contains(&"urn:air:acme.com:root-markup".to_owned()));
    }

    #[test]
    fn at_most_ten_announced_addresses_are_fetched_once_each() {
        let robots: String = (0..15)
            .map(|n| format!("Agentmap: https://acme.com/{}.json\n", n % 12))
            .chain(["Agentmap: https://acme.com/.well-known/ard.json\n".into()])
            .chain(["Agentmap: mailto:a@acme.com\n".into()])
            .collect();
        let web = Web::default().page("https://acme.com/robots.txt", &robots);
        let resolution = run(resolve(&web, &Seed::domain("acme.com").unwrap()));
        let urls: Vec<String> = resolution.sources.iter().map(|s| s.url.to_string()).collect();
        // The well-known address answered 404, so it is no source. An Agentmap line names it again; it is not asked twice.
        let expected: Vec<String> = (0..10).map(|n| format!("https://acme.com/{n}.json")).collect();
        assert_eq!(urls, expected);
        let well_known = web
            .asked()
            .iter()
            .filter(|u| u.ends_with("/.well-known/ard.json"))
            .count();
        assert_eq!(well_known, 1);
    }

    #[test]
    fn a_domain_without_ard_json_publishes_through_links_and_agentmaps() {
        let seed = Seed::domain("acme.com").unwrap();
        let linked = || {
            Web::default()
                .with(
                    "https://acme.com/",
                    Fetched {
                        links: vec!["</linked.json>; rel=ard".into()],
                        ..fetched("https://acme.com/", "<html></html>")
                    },
                )
                .page("https://acme.com/linked.json", &manifest(&["urn:air:acme.com:linked"]))
        };
        // 404 and 410 of the well-known address mean that nothing is there.
        for missing in [404, 410] {
            let web = linked().failing("https://acme.com/.well-known/ard.json", FetchError::Status(missing));
            let resolution = run(resolve(&web, &seed));
            let urls: Vec<&str> = resolution.sources.iter().map(|s| s.url.as_str()).collect();
            assert_eq!(urls, ["https://acme.com/linked.json"], "{missing}");
            assert_eq!(accepted(&resolution), ["urn:air:acme.com:linked"]);
            assert_eq!(resolution.discovery_error, None);
        }
        let web = Web::default()
            .page("https://acme.com/robots.txt", "Agentmap: /mapped.json\n")
            .page("https://acme.com/mapped.json", &manifest(&["urn:air:acme.com:mapped"]));
        assert_eq!(accepted(&run(resolve(&web, &seed))), ["urn:air:acme.com:mapped"]);
        // Another error of the well-known address stays a failed source.
        let web = linked().failing("https://acme.com/.well-known/ard.json", FetchError::Status(503));
        let resolution = run(resolve(&web, &seed));
        assert_eq!(resolution.sources.len(), 2);
        assert_eq!(
            resolution.sources[0].result,
            Err(SourceError::Fetch(FetchError::Status(503)))
        );
        // A domain that publishes nothing has no source at all.
        assert_eq!(run(resolve(&Web::default(), &seed)).sources, []);
        // A catalog address and an announced address that answer 404 stay failed sources.
        let catalog = Url::parse("https://cdn.example.net/acme.json").unwrap();
        let resolution = run(resolve(&Web::default(), &Seed::catalog("acme.com", catalog).unwrap()));
        assert_eq!(
            resolution.sources[0].result,
            Err(SourceError::Fetch(FetchError::Status(404)))
        );
        let web = Web::default().page("https://acme.com/robots.txt", "Agentmap: /gone.json\n");
        let resolution = run(resolve(&web, &seed));
        assert_eq!(resolution.sources[0].url.as_str(), "https://acme.com/gone.json");
        assert_eq!(
            resolution.sources[0].result,
            Err(SourceError::Fetch(FetchError::Status(404)))
        );
    }

    #[test]
    fn a_root_page_or_robots_txt_that_cannot_be_read_leaves_the_sources_unknown() {
        let seed = Seed::domain("acme.com").unwrap();
        let well_known = || {
            Web::default().page(
                "https://acme.com/.well-known/ard.json",
                &manifest(&["urn:air:acme.com:well-known"]),
            )
        };
        let transport = FetchError::Transport("connection reset".into());
        for (page, error) in [
            ("https://acme.com/", transport.clone()),
            ("https://acme.com/", FetchError::Status(503)),
            ("https://acme.com/robots.txt", FetchError::Status(500)),
            ("https://acme.com/robots.txt", FetchError::Status(429)),
            ("https://acme.com/robots.txt", FetchError::Status(403)),
            ("https://acme.com/", FetchError::TooLarge(MAX_RESPONSE_BYTES)),
            (
                "https://acme.com/",
                FetchError::Refused("a redirect target is refused".into()),
            ),
        ] {
            let resolution = run(resolve(&well_known().failing(page, error.clone()), &seed));
            // The well-known source was read, but what the page announces is unknown.
            assert_eq!(accepted(&resolution), ["urn:air:acme.com:well-known"]);
            let expected = DiscoveryError {
                url: Url::parse(page).unwrap(),
                error,
            };
            assert_eq!(resolution.discovery_error.as_ref(), Some(&expected), "{page}");
        }
        // The first error is kept; the other page is still read.
        let web = well_known()
            .failing("https://acme.com/", FetchError::Status(502))
            .failing("https://acme.com/robots.txt", transport);
        let resolution = run(resolve(&web, &seed));
        assert_eq!(
            resolution.discovery_error.unwrap().to_string(),
            "cannot read `https://acme.com/`, which may announce entry sources: the server answered status 502"
        );
        assert!(web.asked().contains(&"https://acme.com/robots.txt".to_owned()));
        // 404 and 410 announce nothing, which is no error.
        let web = well_known()
            .failing("https://acme.com/", FetchError::Status(410))
            .failing("https://acme.com/robots.txt", FetchError::Status(404));
        assert_eq!(run(resolve(&web, &seed)).discovery_error, None);
    }

    #[test]
    fn a_source_may_list_at_most_1000_entries() {
        let catalog = |body: String| {
            let web = Web::default().page("https://agents.acme.com/catalog", &body);
            let url = Url::parse("https://agents.acme.com/catalog").unwrap();
            run(resolve(&web, &Seed::catalog("acme.com", url).unwrap()))
                .sources
                .remove(0)
                .result
        };
        let manifest = |count: usize| json!({"entries": vec![json!({}); count]}).to_string();
        assert_eq!(catalog(manifest(1_001)), Err(SourceError::TooManyEntries));
        assert_eq!(catalog(manifest(1_000)).unwrap().rejected.len(), 1_000);
        // One JSON-LD block of a page can hold as many candidates.
        let in_page = |count: usize| {
            let block = vec![json!({"identifier": "x", "displayName": "x", "type": "x"}); count];
            format!(
                r#"<html><script type="application/ld+json">{}</script></html>"#,
                json!(block)
            )
        };
        assert_eq!(catalog(in_page(1_001)), Err(SourceError::TooManyEntries));
        assert_eq!(catalog(in_page(1_000)).unwrap().rejected.len(), 1_000);
        assert_eq!(
            SourceError::TooManyEntries.to_string(),
            "the source lists more than 1000 entries"
        );
    }

    #[test]
    fn a_json_body_without_entries_is_not_a_manifest() {
        let web = Web::default().page("https://acme.com/.well-known/ard.json", r#"{"agents": []}"#);
        let resolution = run(resolve(&web, &Seed::domain("acme.com").unwrap()));
        assert_eq!(resolution.sources[0].result, Err(SourceError::NotAManifest));
    }

    #[test]
    fn a_page_without_entries_is_no_source_but_an_empty_manifest_is() {
        let catalog = |body: &str| {
            let web = Web::default().page("https://agents.acme.com/catalog", body);
            let url = Url::parse("https://agents.acme.com/catalog").unwrap();
            run(resolve(&web, &Seed::catalog("acme.com", url).unwrap()))
                .sources
                .remove(0)
                .result
        };
        // A maintenance page answers 200 but lists nothing: it must not read as an empty catalog.
        let maintenance = "<html><head><title>Down</title></head><body>Back soon</body></html>";
        assert_eq!(catalog(maintenance), Err(SourceError::NoEntries));
        // A manifest says on purpose that it lists nothing.
        assert_eq!(catalog(r#"{"entries": []}"#), Ok(Found::default()));
    }

    #[test]
    fn a_seed_needs_a_plain_domain() {
        assert!(Seed::domain("acme.com").is_ok());
        for bad in ["Acme.com", "acme.com/x", "127.0.0.1", "a b.com", "acme.com:8443", ""] {
            assert!(Seed::domain(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn each_acceptance_rule_refuses_with_its_own_reason() {
        let reason = |raw: Value| accept_entry(&raw, "acme.com").unwrap_err().reason;
        let good = entry("urn:air:acme.com:kim");
        let with = |key: &str, value: Value| {
            let mut raw = good.clone();
            raw[key] = value;
            raw
        };
        assert_eq!(
            reason(with("description", json!("x".repeat(MAX_ENTRY_BYTES)))),
            RejectReason::TooLarge
        );
        assert_eq!(reason(json!(["urn:air:acme.com:kim"])), RejectReason::NotAnObject);
        assert!(matches!(
            reason(with("representativeQueries", json!("one string"))),
            RejectReason::Malformed(_)
        ));
        assert!(matches!(
            reason(with("@context", json!("https://schema.org"))),
            RejectReason::UnsupportedContext(_)
        ));
        // A base term spelled another way would pass by the trust and url-or-data rules.
        for (key, value) in [
            ("ard:trustManifest", json!({"identity": "spiffe://google.com/x"})),
            ("ard:data", json!({"a": 1})),
        ] {
            assert!(
                matches!(reason(with(key, value)), RejectReason::UnsupportedContext(_)),
                "{key}"
            );
        }
        let RejectReason::Invalid(broken) = reason(with("data", json!({"a": 1}))) else {
            panic!("not invalid");
        };
        assert_eq!(broken, [Violation::BothUrlAndData]);
        assert!(matches!(
            reason(with("identifier", json!("urn:air:acme.co:kim"))),
            RejectReason::OtherPublisher { .. }
        ));
        // A SHOULD rule does not refuse: one representative query is accepted.
        let accepted = accept_entry(&with("representativeQueries", json!(["one"])), "acme.com").unwrap();
        assert_eq!(accepted.publisher, "acme.com");
        assert_eq!(
            accepted.terms["https://agenticresourcediscovery.org/ns#displayName"],
            json!("Kim")
        );
        // A rejected entry keeps its identifier for the report.
        let rejected = accept_entry(&with("data", json!({})), "acme.com").unwrap_err();
        assert_eq!(rejected.identifier.as_deref(), Some("urn:air:acme.com:kim"));
    }

    #[test]
    fn link_headers_and_agentmap_lines_are_parsed() {
        let links = [
            r#"<https://a.example/x>; rel="next", <https://a.example/ard.json>; title="a, b; c"; rel="ARD""#.to_owned(),
            "<one.json>;rel=ard,<two.json>; rel=\"preload ard\"".to_owned(),
            "garbage".to_owned(),
        ];
        assert_eq!(
            ard_link_targets(&links),
            ["https://a.example/ard.json", "one.json", "two.json"]
        );
        let robots = "agentmap:https://a.example/1.json\nAgentmap :  /2.json # note\nAgentmapx: /3\n#Agentmap: /4\n";
        assert_eq!(agentmap_targets(robots), ["https://a.example/1.json", "/2.json"]);
    }
}
