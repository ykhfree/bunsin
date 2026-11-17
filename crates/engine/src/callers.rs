//! Caller tokens: the owner gives one to each outside caller of the profile's inbound endpoint.
//! The tokens live in the shared store, because the server finds the profile from the token.

use crate::{Engine, store_error};
use bunsin_api::{ApiError, ApiResult, NewCallerToken};
use bunsin_core::records::CallerToken;
use serde_json::json;

/// The longest label of a caller token, in characters.
const MAX_LABEL_CHARS: usize = 60;

impl Engine {
    pub(crate) fn read_caller_tokens(&self, profile: &str) -> ApiResult<Vec<CallerToken>> {
        // The profile store is opened first, so an unknown profile is an error and not an empty list.
        self.store(profile)?;
        self.app.caller_tokens(profile).map_err(store_error)
    }

    pub(crate) fn new_caller_token(&self, profile: &str, label: &str) -> ApiResult<NewCallerToken> {
        let store = self.store(profile)?;
        let label = label.trim();
        // The label reaches the agent as the author of each question, so it is one short line.
        if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS || label.chars().any(char::is_control) {
            return Err(ApiError::Invalid(format!(
                "a caller label has 1 to {MAX_LABEL_CHARS} characters on one line"
            )));
        }
        let (id, token) = self
            .app
            .add_caller_token(profile, label, self.now())
            .map_err(store_error)?;
        self.audit(&store, "caller_token_added", None, json!({"id": id, "label": label}))?;
        Ok(NewCallerToken { id, token })
    }

    pub(crate) fn revoke_caller(&self, profile: &str, id: i64) -> ApiResult<()> {
        let store = self.store(profile)?;
        if !self
            .app
            .revoke_caller_token(profile, id, self.now())
            .map_err(store_error)?
        {
            return Err(ApiError::NotFound(format!("caller token {id} that is not revoked")));
        }
        self.audit(&store, "caller_token_revoked", None, json!({"id": id}))
    }
}
