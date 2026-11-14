//! `EngineApi` over HTTPS: how the app and the command line reach a Bunsin server.

use bunsin_api::http::{ApproveBody, ApproveToolsBody, ErrorBody, KillSwitchBody, PROFILES, RejectBody, profile_path};
use bunsin_api::{
    ApiError, ApiResult, Connections, ConnectorUpdate, Edit, EngineApi, LlmUpdate, Outcome, PollPreview, ProfileConfig,
    ProfileSnapshot, ReplayCase, ReplayResult, ToolsReport,
};
use bunsin_core::event::InboundEvent;
use bunsin_core::ports::BoxFuture;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::records::ProfileInfo;
use bunsin_core::tool::PinSet;
use reqwest::header::{AUTHORIZATION, HeaderValue};
use reqwest::{Method, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt;
use std::time::Duration;

/// Longer than the server's two LLM calls for one event, so the client does not give up first.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Where the server is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConfig {
    /// For example `https://bunsin.example.com`. `http://` is accepted only for this machine.
    pub url: String,
    /// A PEM certificate to trust instead of the system roots, for a private CA.
    pub ca_cert_pem: Option<Vec<u8>>,
}

pub struct RemoteEngine {
    base: Url,
    auth: HeaderValue,
    http: reqwest::Client,
}

impl fmt::Debug for RemoteEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The access token is left out on purpose.
        f.debug_struct("RemoteEngine")
            .field("server", &self.base.as_str())
            .finish_non_exhaustive()
    }
}

/// True if `url` points at this machine. The host is compared as a whole, not by prefix.
fn is_loopback(url: &Url) -> bool {
    match url.host() {
        Some(url::Host::Domain(name)) => name == "localhost",
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

impl RemoteEngine {
    pub fn new(config: &RemoteConfig, token: &str) -> ApiResult<Self> {
        let invalid = |message: String| ApiError::Invalid(message);
        let base = Url::parse(config.url.trim()).map_err(|e| invalid(format!("the server address is invalid: {e}")))?;
        match base.scheme() {
            "https" => {}
            // The access token must not cross a network in clear text.
            "http" if is_loopback(&base) => {}
            "http" => {
                return Err(invalid("use https:// for a server on another machine".into()));
            }
            other => {
                return Err(invalid(format!(
                    "the server address must start with https://, not {other}://"
                )));
            }
        }
        if !base.username().is_empty() || base.password().is_some() || base.query().is_some() {
            return Err(invalid(
                "the server address must not contain a user name, password or query".into(),
            ));
        }
        if token.trim().is_empty() {
            return Err(ApiError::Unauthorized("no access token is set".into()));
        }
        let mut auth = HeaderValue::from_str(&format!("Bearer {}", token.trim()))
            .map_err(|_| ApiError::Unauthorized("the access token has characters that cannot be sent".into()))?;
        auth.set_sensitive(true);

        let mut builder = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .connect_timeout(CONNECT_TIMEOUT)
            // A redirect could send the access token to another host.
            .redirect(reqwest::redirect::Policy::none());
        if let Some(pem) = &config.ca_cert_pem {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| invalid(format!("the CA certificate cannot be read: {e}")))?;
            // Text without a certificate parses as an empty list, which would trust no server.
            if certs.is_empty() {
                return Err(invalid("the CA certificate file holds no certificate".into()));
            }
            builder = builder.tls_certs_only(certs);
        }
        let http = builder
            .build()
            .map_err(|e| ApiError::Internal(format!("the HTTP client cannot start: {e}")))?;
        Ok(Self { base, auth, http })
    }

    async fn call<B: Serialize + Sync, T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<&B>,
    ) -> ApiResult<T> {
        let text = self.send(method, path, body).await?;
        serde_json::from_str(&text).map_err(|e| ApiError::Unreachable(format!("the server reply cannot be read: {e}")))
    }

    /// Sends one request. Returns the reply body of a 2xx answer.
    async fn send<B: Serialize + Sync>(&self, method: Method, path: &str, body: Option<&B>) -> ApiResult<String> {
        let url = self
            .base
            .join(path)
            .map_err(|e| ApiError::Invalid(format!("the request path is invalid: {e}")))?;
        let mut request = self.http.request(method, url).header(AUTHORIZATION, self.auth.clone());
        if let Some(body) = body {
            request = request.json(body);
        }
        // `without_url` keeps the message short. The token is in a header and never in the message.
        let unreachable = |e: reqwest::Error| ApiError::Unreachable(e.without_url().to_string());
        let response = request.send().await.map_err(unreachable)?;
        let status = response.status();
        let text = response.text().await.map_err(unreachable)?;
        if status.is_success() {
            return Ok(text);
        }
        match serde_json::from_str::<ErrorBody>(&text) {
            Ok(body) => Err(body.error),
            // Not a Bunsin error: a proxy page, or another service at this address.
            Err(_) if status.as_u16() == 401 => {
                Err(ApiError::Unauthorized("the server refused the access token".into()))
            }
            Err(_) => Err(ApiError::Unreachable(format!(
                "the server answered HTTP {} without a Bunsin error",
                status.as_u16()
            ))),
        }
    }

    async fn send_only<B: Serialize + Sync>(&self, method: Method, path: &str, body: Option<&B>) -> ApiResult<()> {
        self.send(method, path, body).await.map(|_| ())
    }
}

const NO_BODY: Option<&()> = None;

impl EngineApi for RemoteEngine {
    fn list_profiles(&self) -> BoxFuture<'_, ApiResult<Vec<ProfileInfo>>> {
        Box::pin(self.call(Method::GET, PROFILES, NO_BODY))
    }

    fn profile_config<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileConfig>> {
        Box::pin(async move { self.call(Method::GET, &profile_path(profile, "config"), NO_BODY).await })
    }

    fn set_profile_config<'a>(&'a self, profile: &'a str, config: ProfileConfig) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            self.send_only(Method::PUT, &profile_path(profile, "config"), Some(&config))
                .await
        })
    }

    fn snapshot<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileSnapshot>> {
        Box::pin(async move {
            self.call(Method::GET, &profile_path(profile, "snapshot"), NO_BODY)
                .await
        })
    }

    fn ingest<'a>(&'a self, profile: &'a str, event: InboundEvent) -> BoxFuture<'a, ApiResult<Outcome>> {
        Box::pin(async move {
            self.call(Method::POST, &profile_path(profile, "events"), Some(&event))
                .await
        })
    }

    fn approve<'a>(&'a self, profile: &'a str, inbox_id: i64, edit: Option<Edit>) -> BoxFuture<'a, ApiResult<Outcome>> {
        Box::pin(async move {
            let path = profile_path(profile, &format!("inbox/{inbox_id}/approve"));
            self.call(Method::POST, &path, Some(&ApproveBody { edit })).await
        })
    }

    fn reject<'a>(&'a self, profile: &'a str, inbox_id: i64, reason: &'a str) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let path = profile_path(profile, &format!("inbox/{inbox_id}/reject"));
            self.send_only(
                Method::POST,
                &path,
                Some(&RejectBody {
                    reason: reason.to_owned(),
                }),
            )
            .await
        })
    }

    fn set_kill_switch<'a>(&'a self, profile: &'a str, on: bool) -> BoxFuture<'a, ApiResult<bool>> {
        Box::pin(async move {
            let path = profile_path(profile, "kill-switch");
            let reply: KillSwitchBody = self.call(Method::PUT, &path, Some(&KillSwitchBody { on })).await?;
            Ok(reply.on)
        })
    }

    fn replay<'a>(&'a self, profile: &'a str, case: ReplayCase) -> BoxFuture<'a, ApiResult<ReplayResult>> {
        Box::pin(async move {
            self.call(Method::POST, &profile_path(profile, "replay"), Some(&case))
                .await
        })
    }

    fn connector_tools<'a>(&'a self, profile: &'a str, connector: &'a str) -> BoxFuture<'a, ApiResult<ToolsReport>> {
        Box::pin(async move {
            let path = profile_path(profile, &format!("connectors/{connector}/tools"));
            self.call(Method::GET, &path, NO_BODY).await
        })
    }

    fn approve_tools<'a>(
        &'a self,
        profile: &'a str,
        connector: &'a str,
        shown: PinSet,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let path = profile_path(profile, &format!("connectors/{connector}/tools/approve"));
            self.send_only(Method::POST, &path, Some(&ApproveToolsBody { shown }))
                .await
        })
    }

    fn privacy<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<PrivacyConfig>> {
        Box::pin(async move { self.call(Method::GET, &profile_path(profile, "privacy"), NO_BODY).await })
    }

    fn set_privacy<'a>(&'a self, profile: &'a str, privacy: PrivacyConfig) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            self.send_only(Method::PUT, &profile_path(profile, "privacy"), Some(&privacy))
                .await
        })
    }

    fn test_poll<'a>(&'a self, profile: &'a str, index: usize) -> BoxFuture<'a, ApiResult<PollPreview>> {
        Box::pin(async move {
            self.call(
                Method::POST,
                &profile_path(profile, &format!("polls/{index}/test")),
                NO_BODY,
            )
            .await
        })
    }

    fn connections<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Connections>> {
        Box::pin(async move {
            self.call(Method::GET, &profile_path(profile, "connections"), NO_BODY)
                .await
        })
    }

    fn set_llm<'a>(&'a self, profile: &'a str, update: LlmUpdate) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            self.send_only(Method::PUT, &profile_path(profile, "connections/llm"), Some(&update))
                .await
        })
    }

    fn set_connector<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        update: ConnectorUpdate,
    ) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let path = profile_path(profile, &format!("connections/connectors/{name}"));
            self.send_only(Method::PUT, &path, Some(&update)).await
        })
    }

    fn remove_connector<'a>(&'a self, profile: &'a str, name: &'a str) -> BoxFuture<'a, ApiResult<()>> {
        Box::pin(async move {
            let path = profile_path(profile, &format!("connections/connectors/{name}"));
            self.send_only(Method::DELETE, &path, NO_BODY).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connect(url: &str, token: &str) -> ApiResult<RemoteEngine> {
        RemoteEngine::new(
            &RemoteConfig {
                url: url.to_owned(),
                ca_cert_pem: None,
            },
            token,
        )
    }

    #[test]
    fn https_and_local_http_addresses_are_accepted() {
        for url in [
            "https://bunsin.example.com",
            "https://10.0.0.5:8787/",
            "http://127.0.0.1:8787",
            "http://localhost:8787",
            "http://[::1]:8787",
        ] {
            assert!(connect(url, "bsn_x").is_ok(), "{url}");
        }
    }

    #[test]
    fn plain_http_to_another_machine_is_refused() {
        // Each host only looks like a local address.
        for url in [
            "http://bunsin.example.com",
            "http://10.0.0.5:8787",
            "http://localhost.example.com",
            "http://127.0.0.1.example.com",
            "http://example.com/127.0.0.1",
            "http://localhost@example.com",
        ] {
            let error = connect(url, "bsn_x").err().unwrap();
            assert!(matches!(error, ApiError::Invalid(_)), "{url}: {error:?}");
        }
    }

    #[test]
    fn other_bad_addresses_and_tokens_are_refused() {
        for url in [
            "",
            "bunsin.example.com",
            "ftp://bunsin.example.com",
            "https://user:pw@bunsin.example.com",
            "https://bunsin.example.com/?token=x",
        ] {
            assert!(matches!(connect(url, "bsn_x"), Err(ApiError::Invalid(_))), "{url}");
        }
        assert!(matches!(
            connect("https://bunsin.example.com", "  "),
            Err(ApiError::Unauthorized(_))
        ));
        assert!(matches!(
            connect("https://bunsin.example.com", "bad\ntoken"),
            Err(ApiError::Unauthorized(_))
        ));
        let bad_ca = RemoteConfig {
            url: "https://bunsin.example.com".into(),
            ca_cert_pem: Some(b"not a certificate".to_vec()),
        };
        assert!(matches!(RemoteEngine::new(&bad_ca, "bsn_x"), Err(ApiError::Invalid(_))));
    }

    #[test]
    fn the_token_is_not_in_the_debug_output() {
        let engine = connect("https://bunsin.example.com", "bsn_s3cret-token").unwrap();
        let shown = format!("{engine:?}");
        assert!(shown.contains("bunsin.example.com"), "{shown}");
        assert!(!shown.contains("s3cret"), "{shown}");
    }
}
