//! The fetcher for addresses that third parties choose: ARD manifests, pages and `robots.txt`.
//!
//! A `rel="ard"` link or an `Agentmap` line names an address that the publisher chose, not the
//! owner or the operator. So every request and every redirect passes [`UpstreamPolicy::check`].
//! Without an operator host list, the name resolver also drops internal addresses. A public name
//! that resolves only to an internal address is refused, and no proxy sees the request.
//!
//! Limits: a body of [`bunsin_ard::MAX_RESPONSE_BYTES`] (1 MiB), [`REQUEST_TIMEOUT`] per request,
//! [`MAX_REDIRECTS`] redirects, and [`RESOLVE_TIMEOUT`] for one resolution.

use crate::{UpstreamPolicy, is_internal_ip};
use bunsin_ard::{Fetch, FetchError, Fetched, Resolution, Seed};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::SocketAddr;
use std::time::Duration;
use url::Url;

/// How long one request may take.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// How long one resolution may take, with all its requests.
pub const RESOLVE_TIMEOUT: Duration = Duration::from_secs(30);
/// The most redirects that one request follows.
pub const MAX_REDIRECTS: usize = 3;

pub struct Fetcher {
    http: reqwest::Client,
    upstream: UpstreamPolicy,
}

impl Fetcher {
    pub fn new(upstream: &UpstreamPolicy) -> Result<Self, String> {
        let policy = upstream.clone();
        let redirects = reqwest::redirect::Policy::custom(move |attempt| {
            // `previous` holds the first address and each redirect target so far.
            if attempt.previous().len() > MAX_REDIRECTS {
                return attempt.error(format!("more than {MAX_REDIRECTS} redirects"));
            }
            match policy.check("a redirect target", attempt.url().as_str()) {
                Ok(()) => attempt.follow(),
                Err(error) => attempt.error(error),
            }
        });
        let mut builder = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).redirect(redirects);
        if upstream.allowed_hosts.is_empty() {
            // A proxy resolves the name itself, so it would pass by the resolver.
            builder = builder.no_proxy().dns_resolver(PublicResolver);
        }
        let http = builder
            .build()
            .map_err(|e| format!("the ARD fetcher cannot start: {e}"))?;
        Ok(Self {
            http,
            upstream: upstream.clone(),
        })
    }

    /// Resolves `seed` within [`RESOLVE_TIMEOUT`].
    pub async fn resolve(&self, seed: &Seed) -> Result<Resolution, String> {
        tokio::time::timeout(RESOLVE_TIMEOUT, bunsin_ard::resolve(self, seed))
            .await
            .map_err(|_| {
                format!(
                    "the resolution of `{}` took more than {} s",
                    seed.binding(),
                    RESOLVE_TIMEOUT.as_secs()
                )
            })
    }
}

impl Fetch for Fetcher {
    fn get(&self, url: &Url) -> impl Future<Output = Result<Fetched, FetchError>> + Send {
        let checked = self.upstream.check("the address", url.as_str());
        let url = url.clone();
        async move {
            checked.map_err(FetchError::Refused)?;
            bunsin_ard::get_document(&self.http, &url).await
        }
    }
}

/// Resolves names with the system resolver and keeps only the addresses outside the server's network.
struct PublicResolver;

impl Resolve for PublicResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_owned();
        Box::pin(async move {
            let found = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let public = public_addresses(&host, found)?;
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

/// The public addresses among `found`. Fails if none is left.
fn public_addresses(host: &str, found: impl Iterator<Item = SocketAddr>) -> Result<Vec<SocketAddr>, String> {
    let public: Vec<SocketAddr> = found.filter(|address| !is_internal_ip(address.ip())).collect();
    if public.is_empty() {
        return Err(format!(
            "`{host}` has no public address; the server does not connect to its own network"
        ));
    }
    Ok(public)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn address(text: &str) -> SocketAddr {
        SocketAddr::new(text.parse().unwrap(), 0)
    }

    #[test]
    fn the_resolver_keeps_public_addresses_and_refuses_a_name_without_one() {
        let found = [
            "127.0.0.1",
            "93.184.216.34",
            "10.1.2.3",
            "::1",
            "2606:2800:220:1::1",
            "::ffff:192.168.0.1",
        ];
        let public = public_addresses("mixed.example", found.into_iter().map(address)).unwrap();
        assert_eq!(public, [address("93.184.216.34"), address("2606:2800:220:1::1")]);
        let internal = ["127.0.0.1", "169.254.169.254", "100.64.0.1", "fd00::1"];
        let error = public_addresses("inside.example", internal.into_iter().map(address)).unwrap_err();
        assert!(error.contains("`inside.example` has no public address"), "{error}");
        assert!(public_addresses("none.example", std::iter::empty()).is_err());
    }

    #[tokio::test]
    async fn the_resolver_refuses_a_name_of_this_machine() {
        let error = PublicResolver
            .resolve("localhost".parse().unwrap())
            .await
            .err()
            .expect("localhost has only internal addresses");
        assert!(error.to_string().contains("no public address"), "{error}");
    }

    /// A server whose path `/hop/<n>` redirects to `/hop/<n - 1>` and `/hop/0` answers. It counts requests.
    async fn hops() -> (String, Arc<AtomicUsize>) {
        let count = Arc::new(AtomicUsize::new(0));
        let counted = count.clone();
        let app = axum::Router::new().route(
            "/hop/{n}",
            axum::routing::get(move |axum::extract::Path(n): axum::extract::Path<u32>| {
                counted.fetch_add(1, Ordering::SeqCst);
                async move {
                    match n {
                        0 => (StatusCode::OK, [(header::LINK, "</x.json>; rel=\"ard\"")], "{}").into_response(),
                        n => (StatusCode::FOUND, [(header::LOCATION, format!("/hop/{}", n - 1))]).into_response(),
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        (base, count)
    }

    fn local() -> Fetcher {
        Fetcher::new(&UpstreamPolicy {
            allowed_hosts: vec!["127.0.0.1".into()],
        })
        .unwrap()
    }

    #[tokio::test]
    async fn three_redirects_are_followed_and_a_fourth_is_refused() {
        let (base, count) = hops().await;
        let fetched = local()
            .get(&Url::parse(&format!("{base}/hop/3")).unwrap())
            .await
            .unwrap();
        assert_eq!(fetched.url.path(), "/hop/0");
        assert_eq!(fetched.links, ["</x.json>; rel=\"ard\""]);
        assert_eq!(count.load(Ordering::SeqCst), 4);

        let error = local()
            .get(&Url::parse(&format!("{base}/hop/4")).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, FetchError::Refused(text) if text.contains("more than 3 redirects")),
            "{error:?}"
        );
        // The fourth redirect target got no request.
        assert_eq!(count.load(Ordering::SeqCst), 8);
    }

    #[tokio::test]
    async fn every_address_and_redirect_passes_the_address_rules() {
        let target_count = Arc::new(AtomicUsize::new(0));
        let counted = target_count.clone();
        let target = axum::Router::new().fallback(move || {
            counted.fetch_add(1, Ordering::SeqCst);
            async { "{}" }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move { axum::serve(listener, target).await });
        // The same server under a name that the operator did not allow.
        let elsewhere = format!("http://localhost:{port}/ard.json");
        let redirect = axum::Router::new().fallback(move || {
            let elsewhere = elsewhere.clone();
            async move { (StatusCode::TEMPORARY_REDIRECT, [(header::LOCATION, elsewhere)]) }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let start = format!("http://{}/start", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, redirect).await });

        let error = local().get(&Url::parse(&start).unwrap()).await.unwrap_err();
        assert!(
            matches!(&error, FetchError::Refused(text) if text.contains("`localhost`")),
            "{error:?}"
        );
        let direct = Url::parse(&format!("http://localhost:{port}/ard.json")).unwrap();
        assert!(matches!(local().get(&direct).await, Err(FetchError::Refused(_))));
        assert_eq!(target_count.load(Ordering::SeqCst), 0);

        // Without a host list, an internal address is refused before any request.
        let open = Fetcher::new(&UpstreamPolicy::default()).unwrap();
        let inside = Url::parse(&format!("https://127.0.0.1:{port}/")).unwrap();
        assert!(matches!(open.get(&inside).await, Err(FetchError::Refused(_))));
        assert_eq!(target_count.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_large_body_and_an_error_status_are_errors() {
        let app = axum::Router::new()
            .route(
                "/big",
                axum::routing::get(|| async { "x".repeat(bunsin_ard::MAX_RESPONSE_BYTES + 1) }),
            )
            .route("/gone", axum::routing::get(|| async { StatusCode::GONE }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        let get = |path: &str| {
            let url = Url::parse(&format!("{base}{path}")).unwrap();
            async move { local().get(&url).await }
        };
        assert_eq!(
            get("/big").await.unwrap_err(),
            FetchError::TooLarge(bunsin_ard::MAX_RESPONSE_BYTES)
        );
        assert_eq!(get("/gone").await.unwrap_err(), FetchError::Status(410));
    }
}
