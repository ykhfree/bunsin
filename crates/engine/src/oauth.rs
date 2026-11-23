//! OAuth sign-in of MCP connectors.
//!
//! The backends talk to the authorization server. The engine checks the connector, seals the
//! credentials under `OAUTH_<NAME>` and binds them to the origin of the MCP server. The tokens
//! are never in the settings, the log or a reply.

use crate::secrets::{SecretBox, StoredSecret};
use crate::{Clock, Engine, connections::connector_section, internal, store_error};
use bunsin_api::{ApiError, ApiResult, OAuthConnection, OAuthStarted, OAuthState, ProfileConfig};
use bunsin_core::settings::{ConnectorSection, oauth_secret_key, oauth_secret_origin, origin_of};
use bunsin_store::ProfileStore;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

/// Stores what a connector learns while it runs: new OAuth credentials after a refresh, or that
/// the authorization server refused them. The engine gives one writer to each profile's runtime.
pub trait SecretWriter: Send + Sync {
    /// Seals and stores new OAuth credentials of `connector`, for its MCP server at `origin`.
    fn save_oauth(&self, connector: &str, origin: &str, credentials: &str) -> Result<(), String>;

    /// Removes the OAuth credentials of `connector` if they are still `credentials`, and logs
    /// that the owner must sign in again.
    fn oauth_sign_in_needed(&self, connector: &str, credentials: &str) -> Result<(), String>;
}

/// The credentials of a finished sign-in, as the backends return them.
pub struct OAuthSignIn {
    /// The credentials as the backends store them. They hold the tokens.
    pub credentials: String,
    /// The origin of the MCP server that the sign-in was for.
    pub origin: String,
}

impl std::fmt::Debug for OAuthSignIn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthSignIn")
            .field("credentials", &"[REDACTED]")
            .field("origin", &self.origin)
            .finish()
    }
}

/// The writer of one profile.
pub(crate) struct ProfileWriter {
    profile: String,
    store: Arc<ProfileStore>,
    secret_box: SecretBox,
    clock: Clock,
    /// Shared by every writer of the engine. It keeps the check and the removal of
    /// `oauth_sign_in_needed` apart from a save of new credentials.
    lock: Arc<Mutex<()>>,
}

impl ProfileWriter {
    fn seal(&self, connector: &str, origin: &str, credentials: &str) -> Result<(), String> {
        let key = oauth_secret_key(connector);
        let secret = StoredSecret {
            value: credentials.to_owned(),
            origin: oauth_secret_origin(connector, origin),
        };
        let sealed = self.secret_box.seal(&self.profile, &key, &secret)?;
        self.store
            .put_secret(&key, &sealed, (self.clock)())
            .map_err(|e| e.to_string())
    }
}

impl SecretWriter for ProfileWriter {
    fn save_oauth(&self, connector: &str, origin: &str, credentials: &str) -> Result<(), String> {
        let _held = self.lock.lock().expect("writer lock");
        self.seal(connector, origin, credentials)
    }

    fn oauth_sign_in_needed(&self, connector: &str, credentials: &str) -> Result<(), String> {
        let _held = self.lock.lock().expect("writer lock");
        let key = oauth_secret_key(connector);
        // A sign-in that finished meanwhile stored other credentials. Those stay.
        let Some(sealed) = self.store.secret(&key).map_err(|e| e.to_string())? else {
            return Ok(());
        };
        let stored = self.secret_box.open(&self.profile, &key, &sealed)?;
        if stored.value != credentials {
            return Ok(());
        }
        self.store.delete_secret(&key).map_err(|e| e.to_string())?;
        self.store
            .append_audit(
                (self.clock)(),
                "oauth_sign_in_needed",
                None,
                &json!({"target": connector}),
            )
            .map_err(|e| e.to_string())
    }
}

/// True if the stored `credentials` can still give an access token at `now` (seconds since 1970):
/// they hold a token response, and its access token has not expired or a refresh token can renew it.
/// A token without its lifetime or its time of receipt counts as valid, as the connector uses it.
///
/// The credentials are the JSON that the backends store (`StoredCredentials` of rmcp). Only the
/// fields read here matter.
fn gives_a_token(credentials: &str, now: i64) -> bool {
    let Ok(stored) = serde_json::from_str::<Value>(credentials) else {
        return false;
    };
    let tokens = &stored["token_response"];
    if !tokens.is_object() {
        return false;
    }
    if tokens.get("refresh_token").is_some_and(|token| !token.is_null()) {
        return true;
    }
    match (tokens["expires_in"].as_i64(), stored["token_received_at"].as_i64()) {
        (Some(lifetime), Some(received)) => received.saturating_add(lifetime) > now,
        _ => true,
    }
}

/// The settings of OAuth connector `name` and the origin of its MCP server.
fn oauth_section<'a>(config: &'a ProfileConfig, name: &str) -> ApiResult<(&'a Value, String)> {
    let value = config
        .connectors
        .get(name)
        .ok_or_else(|| ApiError::NotFound(format!("connector `{name}`")))?;
    let section = connector_section(name, value)?;
    let origin = match (&section.oauth, section.kind.address().and_then(origin_of)) {
        (Some(_), Some(origin)) => origin,
        _ => {
            return Err(ApiError::Invalid(format!(
                "connector `{name}` does not sign in with OAuth"
            )));
        }
    };
    Ok((value, origin))
}

impl Engine {
    pub(crate) fn writer(&self, profile: &str, store: Arc<ProfileStore>) -> Arc<dyn SecretWriter> {
        Arc::new(ProfileWriter {
            profile: profile.to_owned(),
            store,
            secret_box: self.secret_box.clone(),
            clock: self.clock.clone(),
            lock: self.oauth_writes.clone(),
        })
    }

    /// The OAuth state of a connector. Only credentials for the present origin count, and only
    /// while they can still give an access token.
    pub(crate) fn oauth_connection(
        &self,
        profile: &str,
        store: &ProfileStore,
        name: &str,
        section: &ConnectorSection,
    ) -> ApiResult<Option<OAuthConnection>> {
        let Some(config) = &section.oauth else {
            return Ok(None);
        };
        let wanted = section
            .kind
            .address()
            .and_then(origin_of)
            .map(|origin| oauth_secret_origin(name, &origin));
        let key = oauth_secret_key(name);
        let sealed = store.secret(&key).map_err(store_error)?;
        let usable = match sealed {
            Some(sealed) if Some(&sealed.origin) == wanted.as_ref() => {
                // Credentials that this server cannot open give no token either.
                let opened = self.secret_box.open(profile, &key, &sealed);
                opened.is_ok_and(|stored| gives_a_token(&stored.value, (self.clock)()))
            }
            _ => false,
        };
        let state = if usable {
            OAuthState::SignedIn
        } else {
            OAuthState::SignInNeeded
        };
        Ok(Some(OAuthConnection {
            config: config.clone(),
            state,
        }))
    }

    pub(crate) async fn start_oauth(&self, profile: &str, name: &str, redirect_uri: &str) -> ApiResult<OAuthStarted> {
        let config = Self::config(&*self.store(profile)?)?;
        let (section, _) = oauth_section(&config, name)?;
        let authorization_url = self
            .backends
            .oauth_start(profile, name, section, redirect_uri)
            .await
            .map_err(ApiError::Invalid)?;
        Ok(OAuthStarted { authorization_url })
    }

    pub(crate) async fn finish_oauth(&self, profile: &str, name: &str, callback_url: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        // The connector must still sign in with OAuth when the sign-in ends.
        oauth_section(&Self::config(&store)?, name)?;
        let signed_in = self
            .backends
            .oauth_finish(profile, name, callback_url)
            .await
            .map_err(ApiError::Invalid)?;
        // The address can change while the owner signs in. The tokens are for the old server.
        let (_, origin) = oauth_section(&Self::config(&store)?, name)?;
        if origin != signed_in.origin {
            return Err(ApiError::Invalid(format!(
                "the address of connector `{name}` changed during the sign-in; sign in again"
            )));
        }
        self.writer(profile, store.clone())
            .save_oauth(name, &origin, &signed_in.credentials)
            .map_err(internal)?;
        // The cached connector may hold no credentials or old ones.
        self.runtimes.lock().expect("engine lock").remove(profile);
        self.audit(&store, "oauth_signed_in", None, json!({"target": name}))
    }
}

#[cfg(test)]
mod tests {
    use super::gives_a_token;
    use serde_json::json;

    /// Stored credentials as rmcp writes them, with `tokens` as the token response.
    fn stored(tokens: serde_json::Value, received_at: Option<i64>) -> String {
        json!({"client_id": "c1", "token_response": tokens, "granted_scopes": [], "token_received_at": received_at})
            .to_string()
    }

    #[test]
    fn an_expired_access_token_gives_none_without_a_refresh_token() {
        let lasting = json!({"access_token": "a", "token_type": "bearer", "expires_in": 3600});
        let renewable = json!({"access_token": "a", "token_type": "bearer", "expires_in": 3600, "refresh_token": "r"});
        // Received at 1000, so the access token expires at 4600.
        assert!(gives_a_token(&stored(lasting.clone(), Some(1_000)), 4_599));
        assert!(!gives_a_token(&stored(lasting.clone(), Some(1_000)), 4_600));
        assert!(!gives_a_token(&stored(lasting, Some(1_000)), 100_000));
        // A refresh token renews an expired access token.
        assert!(gives_a_token(&stored(renewable, Some(1_000)), 100_000));
    }

    #[test]
    fn a_token_without_its_lifetime_counts_and_credentials_without_tokens_do_not() {
        let endless = json!({"access_token": "a", "token_type": "bearer"});
        assert!(gives_a_token(&stored(endless, Some(1_000)), i64::MAX));
        let unknown_receipt = json!({"access_token": "a", "token_type": "bearer", "expires_in": 60});
        assert!(gives_a_token(&stored(unknown_receipt, None), i64::MAX));
        let null_refresh =
            json!({"access_token": "a", "token_type": "bearer", "expires_in": 60, "refresh_token": null});
        assert!(!gives_a_token(&stored(null_refresh, Some(1_000)), 2_000));
        // A registered client that never got tokens, and text that is not credentials.
        assert!(!gives_a_token(&stored(serde_json::Value::Null, Some(1_000)), 0));
        assert!(!gives_a_token("not json", 0));
    }
}
