//! What a start does with tool calls that a stop cut or kept from running.
//!
//! A call that was `executing` may have reached its tool. It is never run again: it becomes
//! `unknown`, and only the owner resolves it. An `approved` call that never started runs now,
//! but only if the policy allows it at this time. Otherwise a pending copy asks the owner again.
//! The owner resolves an `unknown` item with `resolve_unknown`.

use crate::{Engine, store_error};
use bunsin_api::{ApiError, ApiResult, Outcome, Resolution};
use bunsin_core::approval::ApprovalState;
use bunsin_core::policy::Decision;
use bunsin_core::records::InboxBody;
use serde_json::json;

/// What `mark_interrupted` found. `resume_approved` takes it.
pub struct Interrupted {
    /// The approved tool calls that waited when the engine started: profile and inbox ID.
    approved: Vec<(String, i64)>,
    /// The profiles whose store could not be checked, with the reason.
    pub failures: Vec<(String, ApiError)>,
}

impl Engine {
    /// Runs before the server takes requests, while no call runs. Moves every `executing`
    /// item to `unknown` and lists the `approved` tool calls. A profile that fails does not
    /// stop the others.
    pub fn mark_interrupted(&self) -> Interrupted {
        let mut interrupted = Interrupted {
            approved: Vec::new(),
            failures: Vec::new(),
        };
        let profiles = match self.app.list_profiles() {
            Ok(profiles) => profiles,
            Err(error) => {
                interrupted
                    .failures
                    .push(("all profiles".to_owned(), store_error(error)));
                return interrupted;
            }
        };
        for profile in profiles {
            match self.mark_profile_interrupted(&profile.id) {
                Ok(ids) => interrupted
                    .approved
                    .extend(ids.into_iter().map(|id| (profile.id.clone(), id))),
                Err(error) => interrupted.failures.push((profile.id, error)),
            }
        }
        interrupted
    }

    fn mark_profile_interrupted(&self, profile: &str) -> ApiResult<Vec<i64>> {
        let store = self.store(profile)?;
        for item in store.inbox_in_state(ApprovalState::Executing).map_err(store_error)? {
            if store
                .move_inbox_state(item.id, ApprovalState::Executing, ApprovalState::Unknown, self.now())
                .map_err(store_error)?
            {
                let tool = match &item.body {
                    InboxBody::ToolCall { tool, .. } => Some(tool),
                    _ => None,
                };
                let detail = json!({"inbox_id": item.id, "tool": tool});
                self.audit(&store, "outcome_unknown", Some(&item.event_key), detail)?;
            }
        }
        let approved = store.inbox_in_state(ApprovalState::Approved).map_err(store_error)?;
        Ok(approved
            .into_iter()
            .filter(|item| matches!(item.body, InboxBody::ToolCall { .. }))
            .map(|item| item.id)
            .collect())
    }

    /// Runs after the server takes requests. Handles each approved call that `mark_interrupted`
    /// listed, and no other item. Returns the items that failed, with the reason.
    pub async fn resume_approved(&self, interrupted: Interrupted) -> Vec<(String, ApiError)> {
        let mut failures = Vec::new();
        for (profile, inbox_id) in interrupted.approved {
            if let Err(error) = self.resume_item(&profile, inbox_id).await {
                failures.push((profile, error));
            }
        }
        failures
    }

    /// Runs an approved call if the policy allows it now. The owner's approval counts if the
    /// policy still needs one; an autonomous item needs `Execute`. Otherwise the item fails and
    /// a pending copy asks the owner, whose approval checks the policy again.
    async fn resume_item(&self, profile: &str, inbox_id: i64) -> ApiResult<()> {
        let store = self.store(profile)?;
        let item = store.inbox_item(inbox_id).map_err(store_error)?;
        // A request since the start may have handled it.
        if item.state != ApprovalState::Approved {
            return Ok(());
        }
        let InboxBody::ToolCall { tool, .. } = &item.body else {
            return Ok(());
        };
        let key = Some(item.event_key.as_str());
        let blocked = match self.ctx(profile).await {
            Ok(ctx) => {
                let decision = self.decide(&ctx, tool).await?;
                let runs = match decision {
                    Decision::Execute => true,
                    Decision::NeedsApproval => !item.autonomous,
                    _ => false,
                };
                if runs {
                    self.audit(&store, "resumed", key, json!({"inbox_id": inbox_id}))?;
                    self.execute(&ctx, &item).await?;
                    return Ok(());
                }
                json!({"decision": decision})
            }
            // Without the clients the policy cannot be checked. The owner decides later.
            Err(error) => json!({"error": error.to_string()}),
        };
        if let Some(copy) = store
            .fail_with_pending_copy(inbox_id, ApprovalState::Approved, self.now())
            .map_err(store_error)?
        {
            let detail = json!({"inbox_id": inbox_id, "copy": copy.id, "blocked": blocked});
            self.audit(&store, "resume_abandoned", key, detail)?;
        }
        Ok(())
    }

    /// Resolves an `unknown` item as the owner says. Only an `unknown` item can be resolved.
    pub(crate) async fn resolve_unknown_item(
        &self,
        profile: &str,
        inbox_id: i64,
        resolution: Resolution,
    ) -> ApiResult<Option<Outcome>> {
        let store = self.store(profile)?;
        let item = store.inbox_item(inbox_id).map_err(store_error)?;
        if item.state != ApprovalState::Unknown {
            return Err(ApiError::NotAllowed(format!(
                "the item is {}, not unknown",
                item.state.as_str()
            )));
        }
        let handled = || ApiError::NotAllowed("the item was already handled".into());
        let key = Some(item.event_key.as_str());
        let to = match resolution {
            Resolution::Done => ApprovalState::Executed,
            Resolution::Drop => ApprovalState::Failed,
            Resolution::Retry => {
                // The move and the copy are one step, so two retries cannot make two copies.
                let copy = store
                    .fail_with_pending_copy(inbox_id, ApprovalState::Unknown, self.now())
                    .map_err(store_error)?
                    .ok_or_else(handled)?;
                let detail = json!({"inbox_id": inbox_id, "resolution": resolution, "copy": copy.id});
                self.audit(&store, "unknown_resolved", key, detail)?;
                return match self.approve_item(profile, copy.id, None).await {
                    Ok(outcome) => Ok(Some(outcome)),
                    Err(ApiError::NotAllowed(error)) => Err(ApiError::NotAllowed(format!(
                        "{error}; the copy waits in the inbox as item {}",
                        copy.id
                    ))),
                    Err(error) => Err(error),
                };
            }
        };
        if !store
            .move_inbox_state(inbox_id, ApprovalState::Unknown, to, self.now())
            .map_err(store_error)?
        {
            return Err(handled());
        }
        let detail = json!({"inbox_id": inbox_id, "resolution": resolution});
        self.audit(&store, "unknown_resolved", key, detail)?;
        Ok(None)
    }
}
