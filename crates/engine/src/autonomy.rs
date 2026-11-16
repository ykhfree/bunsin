//! Narrow autonomy: what the owner and the policy decided about each tool's calls, and holds.
//!
//! The engine records every decision about a call in an append-only table. The counts show the
//! owner how often a tool's drafts were approved without a change. The engine never raises a step.
//! It only holds a tool at approval after the owner turns down one of its autonomous calls.
//! The hold lives outside the settings, so a saved settings file cannot undo it.

use crate::{Engine, store_error};
use bunsin_api::ApiResult;
use bunsin_core::policy::{Tier, ToolClass};
use bunsin_core::records::{DecisionKind, InboxItem};
use bunsin_store::ProfileStore;
use serde_json::json;

impl Engine {
    /// Records a decision about the call of `item`. A copy repeats an earlier item, whose decision
    /// is recorded already, so an owner's review of a copy is not counted again.
    pub(crate) fn record_decision(
        &self,
        store: &ProfileStore,
        item: &InboxItem,
        tool: &str,
        kind: DecisionKind,
    ) -> ApiResult<()> {
        let review = matches!(
            kind,
            DecisionKind::Approved | DecisionKind::Edited | DecisionKind::Rejected
        );
        if review && item.retry_of.is_some() {
            return Ok(());
        }
        store
            .add_decision(self.now(), Some(item.id), tool, kind)
            .map_err(store_error)
    }

    /// Holds `tool` at approval because the owner turned down `item`, if the tool is not held yet.
    /// `cause` names what the owner did.
    pub(crate) fn demote(&self, store: &ProfileStore, item: &InboxItem, tool: &str, cause: &str) -> ApiResult<()> {
        if store.demote(self.now(), item.id, tool).map_err(store_error)? {
            let detail = json!({"tool": tool, "inbox_id": item.id, "cause": cause});
            self.audit(store, "autonomy_demoted", Some(&item.event_key), detail)?;
        }
        Ok(())
    }

    /// Records the owner's dismissal of a waiting call. If the tool's rule lets it run without
    /// the owner, the dismissal also holds the tool at approval.
    pub(crate) fn record_rejection(&self, store: &ProfileStore, item: &InboxItem, tool: &str) -> ApiResult<()> {
        self.record_decision(store, item, tool, DecisionKind::Rejected)?;
        let config = Self::config(store)?;
        let autonomous = config
            .policy
            .rules
            .get(tool)
            .is_some_and(|rule| rule.class == ToolClass::Write && rule.tier == Tier::Autonomous);
        if autonomous {
            self.demote(store, item, tool, "rejected")?;
        }
        Ok(())
    }
}
