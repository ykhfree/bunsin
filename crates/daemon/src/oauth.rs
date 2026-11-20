//! OAuth sign-in for MCP connectors at an address, with the `auth` client of rmcp.
//!
//! Every request of the flow (metadata discovery, client registration, token, refresh) passes
//! [`UpstreamPolicy`] before it is sent, and the client follows no redirect.
//! A started sign-in waits in memory for 10 minutes. A profile has at most 3 waiting.
//! A restart of the server forgets them, so the owner starts again.

use crate::UpstreamPolicy;
use axum::http::Response;
use bunsin_connectors::McpConnector;
use bunsin_core::settings::OAuthConfig;
use bunsin_engine::{OAuthSignIn, SecretWriter};
use rmcp::transport::auth::{
    AuthError, AuthorizationManager, AuthorizationRequest, AuthorizationSession, CredentialStore, OAuthHttpClient,
    OAuthHttpClientFuture, OAuthHttpRequest, StoredCredentials,
};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use url::Url;

/// How long a started sign-in waits for its callback.
pub const SIGN_IN_TIME: Duration = Duration::from_secs(10 * 60);
/// How many sign-ins of one profile can wait at a time.
pub const MAX_WAITING: usize = 3;
/// The time limit of one OAuth request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The largest answer of the authorization server that the client reads.
const MAX_ANSWER_BYTES: usize = 1024 * 1024;
/// The client name that dynamic client registration sends.
const CLIENT_NAME: &str = "Bunsin";

/// Sends the HTTP requests of the OAuth flow. Each address must pass the upstream policy first.
struct PolicyHttp {
    http: reqwest::Client,
    upstream: UpstreamPolicy,
    connector: String,
    /// The last refusal of the policy. rmcp drops the cause of a failed token request, so the
    /// error text comes from here.
    refusal: Refusal,
}

/// The last refusal of the upstream policy in one OAuth flow.
#[derive(Clone, Default)]
struct Refusal(Arc<Mutex<Option<String>>>);

impl Refusal {
    /// The text of `error`, or the refusal of the policy that caused it.
    fn explain(&self, error: &(dyn std::error::Error + 'static)) -> String {
        match self.0.lock().expect("refusal lock").clone() {
            Some(refusal) => refusal,
            None => failure(error),
        }
    }
}

impl OAuthHttpClient for PolicyHttp {
    fn execute(&self, request: OAuthHttpRequest) -> OAuthHttpClientFuture<'_> {
        Box::pin(async move {
            // The client follows no redirect, whatever the request asks. rmcp follows a redirect
            // of discovery to the same origin itself, and that request comes here again.
            let request = request.request;
            let what = format!("an OAuth address of connector `{}`", self.connector);
            if let Err(refusal) = self.upstream.check(&what, &request.uri().to_string()) {
                *self.refusal.0.lock().expect("refusal lock") = Some(refusal.clone());
                return Err(refusal.into());
            }
            let mut response = self.http.execute(reqwest::Request::try_from(request)?).await?;
            let mut answer = Response::builder().status(response.status());
            for (name, value) in response.headers() {
                answer = answer.header(name, value);
            }
            let mut body = Vec::new();
            while let Some(chunk) = response.chunk().await? {
                if body.len() + chunk.len() > MAX_ANSWER_BYTES {
                    return Err("the answer of the authorization server is larger than 1 MiB".into());
                }
                body.extend_from_slice(&chunk);
            }
            Ok(answer.body(body)?)
        })
    }
}

/// An auth manager for the MCP server at `url` whose requests obey `upstream`.
async fn manager(
    connector: &str,
    url: &str,
    upstream: &UpstreamPolicy,
    store: Credentials,
    refusal: &Refusal,
) -> Result<AuthorizationManager, String> {
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_TIMEOUT)
        .build()
        .map_err(|e| format!("the HTTP client cannot start: {e}"))?;
    let client = PolicyHttp {
        http,
        upstream: upstream.clone(),
        connector: connector.to_owned(),
        refusal: refusal.clone(),
    };
    let mut manager = AuthorizationManager::new_with_oauth_http_client(url, Arc::new(client))
        .await
        .map_err(|e| refusal.explain(&e))?;
    manager.set_credential_store(store);
    Ok(manager)
}

/// The text of an error with its causes. A refusal of the upstream policy is a cause.
fn failure(error: &(dyn std::error::Error + 'static)) -> String {
    let mut text = error.to_string();
    let mut cause = error.source();
    while let Some(next) = cause {
        text.push_str(": ");
        text.push_str(&next.to_string());
        cause = next.source();
    }
    text
}

/// Where a runtime store writes changed credentials.
#[derive(Clone)]
struct Sink {
    writer: Arc<dyn SecretWriter>,
    connector: String,
    /// The origin of the MCP server.
    origin: String,
}

/// The OAuth credentials of one connector, as the JSON text that the engine seals.
/// The store of a sign-in keeps them in memory. The store of a runtime also writes each change
/// through the engine.
#[derive(Clone)]
struct Credentials {
    current: Arc<Mutex<Option<String>>>,
    sink: Option<Sink>,
}

impl Credentials {
    fn text(&self) -> Option<String> {
        self.current.lock().expect("credentials lock").clone()
    }

    /// The authorization server refused the credentials. The engine removes them.
    fn refused(&self) -> Result<(), AuthError> {
        if let (Some(sink), Some(text)) = (&self.sink, self.text()) {
            sink.writer
                .oauth_sign_in_needed(&sink.connector, &text)
                .map_err(AuthError::CredentialStoreError)?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl CredentialStore for Credentials {
    async fn load(&self) -> Result<Option<StoredCredentials>, AuthError> {
        self.text()
            .map(|text| {
                serde_json::from_str(&text)
                    .map_err(|_| AuthError::CredentialStoreError("the stored credentials cannot be read".into()))
            })
            .transpose()
    }

    async fn save(&self, credentials: StoredCredentials) -> Result<(), AuthError> {
        let text = serde_json::to_string(&credentials)
            .map_err(|_| AuthError::CredentialStoreError("the credentials cannot be written".into()))?;
        if let Some(sink) = &self.sink {
            sink.writer
                .save_oauth(&sink.connector, &sink.origin, &text)
                .map_err(AuthError::CredentialStoreError)?;
        }
        *self.current.lock().expect("credentials lock") = Some(text);
        Ok(())
    }

    /// rmcp clears the credentials when the authorization server changed its issuer.
    async fn clear(&self) -> Result<(), AuthError> {
        self.refused()?;
        *self.current.lock().expect("credentials lock") = None;
        Ok(())
    }

    /// The credentials that rmcp passes are the ones this store loaded, so the stored text is compared.
    async fn on_refresh_token_rejected(&self, _credentials: &StoredCredentials) -> Result<(), AuthError> {
        self.refused()
    }
}

/// Connects to the MCP server at `url` with sealed credentials. A refresh goes through `writer`.
pub(crate) async fn connect(
    connector: &str,
    url: &str,
    origin: &str,
    upstream: &UpstreamPolicy,
    credentials: String,
    writer: Option<Arc<dyn SecretWriter>>,
) -> Result<McpConnector, String> {
    let store = Credentials {
        current: Arc::new(Mutex::new(Some(credentials))),
        sink: writer.map(|writer| Sink {
            writer,
            connector: connector.to_owned(),
            origin: origin.to_owned(),
        }),
    };
    let refusal = Refusal::default();
    let mut manager = manager(connector, url, upstream, store, &refusal).await?;
    if !manager.initialize_from_store().await.map_err(|e| refusal.explain(&e))? {
        return Err(format!("connector `{connector}` needs a new sign-in: {SIGN_IN_HINT}"));
    }
    // An expired access token without a refresh token, or a refused refresh, would fail the first
    // request with a text that does not say what to do. The token is therefore asked for first.
    match manager.get_access_token().await {
        Ok(_) => {}
        Err(AuthError::AuthorizationRequired) => {
            return Err(format!("connector `{connector}` needs a new sign-in: {SIGN_IN_HINT}"));
        }
        Err(error) => return Err(refusal.explain(&error)),
    }
    McpConnector::connect_oauth(connector, url, manager)
        .await
        .map_err(|e| e.to_string())
}

/// How the owner signs in. Error texts name it.
pub const SIGN_IN_HINT: &str = "use Sign in in the window, or `bunsind connector-oauth`";

/// A sign-in that waits for the browser.
struct Waiting {
    profile: String,
    connector: String,
    /// The `state` value of the authorization request.
    state: String,
    redirect_uri: Url,
    /// The origin of the MCP server.
    origin: String,
    started: Instant,
    session: AuthorizationSession,
    /// The store that the session saves the tokens into.
    credentials: Credentials,
    refusal: Refusal,
}

/// The sign-ins that wait for their callback.
#[derive(Default)]
pub struct SignIns(Mutex<Vec<Waiting>>);

/// Refuses a redirect address that is not on this computer (RFC 8252, section 7.3).
fn loopback(redirect_uri: &str) -> Result<Url, String> {
    let refused = || {
        format!(
            "the redirect address `{redirect_uri}` must be a loopback address with http, \
             for example http://127.0.0.1:8976/callback"
        )
    };
    let url = Url::parse(redirect_uri).map_err(|_| refused())?;
    let plain = url.username().is_empty() && url.password().is_none() && url.fragment().is_none();
    if url.scheme() != "http" || !on_this_computer(&url) || !plain {
        return Err(refused());
    }
    Ok(url)
}

fn on_this_computer(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => false,
    }
}

/// Refuses an authorization address that the owner's browser must not open. The MCP server's
/// metadata sets this address, and the window opens it, so a scheme such as `smb:` or `file:`
/// would start a program on the owner's computer. Only https, or http on a loopback host, passes.
fn browser_address(address: &str) -> Result<Url, String> {
    let url = Url::parse(address).map_err(|_| "the authorization address is not a URL".to_owned())?;
    let web = url.scheme() == "https" || (url.scheme() == "http" && on_this_computer(&url));
    if !web {
        return Err(format!(
            "the authorization server gave an address with the scheme `{}`; only https is allowed",
            url.scheme()
        ));
    }
    Ok(url)
}

/// The value of query parameter `name` of `url`.
fn query(url: &Url, name: &str) -> Option<String> {
    url.query_pairs()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

impl SignIns {
    /// Drops the sign-ins that waited too long.
    fn waiting(&self) -> std::sync::MutexGuard<'_, Vec<Waiting>> {
        let mut waiting = self.0.lock().expect("sign-in lock");
        waiting.retain(|sign_in| sign_in.started.elapsed() < SIGN_IN_TIME);
        waiting
    }

    fn check_room(&self, profile: &str) -> Result<(), String> {
        if self.waiting().iter().filter(|w| w.profile == profile).count() >= MAX_WAITING {
            return Err(format!(
                "{MAX_WAITING} sign-ins of this profile wait already. Finish one, or wait 10 minutes"
            ));
        }
        Ok(())
    }

    /// Discovers the authorization server, registers a client if needed, and returns the
    /// address of the page where the owner signs in.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn start(
        &self,
        profile: &str,
        connector: &str,
        url: &str,
        origin: String,
        oauth: &OAuthConfig,
        redirect_uri: &str,
        upstream: &UpstreamPolicy,
    ) -> Result<String, String> {
        let redirect = loopback(redirect_uri)?;
        self.check_room(profile)?;
        let credentials = Credentials {
            current: Arc::new(Mutex::new(None)),
            sink: None,
        };
        let refusal = Refusal::default();
        let mut manager = manager(connector, url, upstream, credentials.clone(), &refusal).await?;
        let metadata = manager.resolve_metadata().await.map_err(|e| refusal.explain(&e))?;
        manager.set_metadata(metadata.metadata);
        let mut request = AuthorizationRequest::new(redirect.as_str()).with_client_name(CLIENT_NAME);
        if let Some(client_id) = &oauth.client_id {
            request = request.with_preregistered_client(client_id);
        }
        if !oauth.scopes.is_empty() {
            request = request.with_scopes(oauth.scopes.iter().cloned());
        }
        let session = AuthorizationSession::new(manager, request)
            .await
            .map_err(|(_, e)| refusal.explain(&e))?;
        let address = session.get_authorization_url().to_owned();
        let url = browser_address(&address)?;
        let state = query(&url, "state").ok_or("the authorization address has no `state`")?;
        // Another start may have filled the room during the requests above.
        self.check_room(profile)?;
        self.waiting().push(Waiting {
            profile: profile.to_owned(),
            connector: connector.to_owned(),
            state,
            redirect_uri: redirect,
            origin,
            started: Instant::now(),
            session,
            credentials,
            refusal,
        });
        Ok(address)
    }

    /// Exchanges the code of `callback_url` for tokens. The callback must carry the `state`
    /// of a waiting sign-in of this profile and connector, at that sign-in's redirect address.
    pub(crate) async fn finish(
        &self,
        profile: &str,
        connector: &str,
        callback_url: &str,
    ) -> Result<OAuthSignIn, String> {
        let unknown = "no sign-in waits for this address: it was used, it waited more than 10 minutes, \
                       or it belongs to another connector. Start the sign-in again";
        let callback =
            Url::parse(callback_url.trim()).map_err(|_| "the address after the sign-in is not an address")?;
        let state = query(&callback, "state").ok_or("the address after the sign-in has no `state`")?;
        let waiting = {
            let mut waiting = self.waiting();
            let found = waiting
                .iter()
                .position(|w| w.profile == profile && w.connector == connector && w.state == state);
            // One use only: a refusal, a wrong code or a failed exchange ends the sign-in too.
            waiting.remove(found.ok_or(unknown)?)
        };
        if let Some(error) = query(&callback, "error") {
            return Err(format!("the authorization server did not give access: {error}"));
        }
        let expected = &waiting.redirect_uri;
        let same_place = callback.scheme() == expected.scheme()
            && callback.host() == expected.host()
            && callback.port_or_known_default() == expected.port_or_known_default()
            && callback.path() == expected.path();
        if !same_place {
            return Err(format!(
                "the address after the sign-in must start with the redirect address {}",
                expected.as_str()
            ));
        }
        waiting
            .session
            .handle_callback_url(callback.as_str())
            .await
            .map_err(|e| format!("the sign-in failed: {}", waiting.refusal.explain(&e)))?;
        let credentials = waiting
            .credentials
            .text()
            .ok_or("the authorization server gave no tokens")?;
        Ok(OAuthSignIn {
            credentials,
            origin: waiting.origin,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_loopback_address_with_http_is_a_redirect_address() {
        for good in [
            "http://127.0.0.1:8976/callback",
            "http://[::1]:5000/cb",
            "http://localhost:7000/",
            "http://127.0.0.1/callback?x=1",
        ] {
            assert!(loopback(good).is_ok(), "{good}");
        }
        for bad in [
            "https://127.0.0.1:8976/callback",
            "http://10.0.0.5:8976/callback",
            "http://example.com/callback",
            "http://localhost.example.com/cb",
            "http://user@127.0.0.1:8976/callback",
            "http://127.0.0.1:8976/callback#frag",
            "127.0.0.1:8976/callback",
            "",
        ] {
            assert!(loopback(bad).unwrap_err().contains("loopback"), "{bad}");
        }
    }

    #[test]
    fn the_browser_opens_only_a_web_address() {
        for good in [
            "https://auth.example.com/authorize?state=s",
            "http://127.0.0.1:9000/authorize?state=s",
            "http://localhost/authorize",
        ] {
            assert!(browser_address(good).is_ok(), "{good}");
        }
        for (bad, scheme) in [
            ("smb://attacker.example.com/share?state=s", "smb"),
            ("file:///Applications/Calculator.app?state=s", "file"),
            ("ssh://attacker.example.com", "ssh"),
            ("javascript:alert(1)", "javascript"),
            ("http://auth.example.com/authorize?state=s", "http"),
            ("http://localhost.example.com/authorize", "http"),
        ] {
            let error = browser_address(bad).unwrap_err();
            assert!(error.contains(&format!("`{scheme}`")), "{bad}: {error}");
        }
        assert!(browser_address("not a url").is_err());
    }
}
