//! Narrow autonomy: what the owner and the policy decided about each tool's calls, and holds.
//!
//! The engine records every decision about a call in an append-only table. The counts show the
//! owner how often a tool's drafts were approved without a change. The engine never raises a step.
//! It only holds a tool at approval after the owner turns down one of its autonomous calls.
//! The hold lives outside the settings, so a saved settings file cannot undo it.

use crate::{Engine, store_error};
use bunsin_api::{ApiError, ApiResult, Autonomy, ToolAutonomy};
use bunsin_core::approval::ApprovalState;
use bunsin_core::autonomy::suggest;
use bunsin_core::policy::{Tier, ToolClass};
use bunsin_core::records::{DecisionKind, InboxBody, InboxItem};
use bunsin_store::ProfileStore;
use serde_json::json;
use std::collections::BTreeSet;

const DAY_SECS: i64 = 24 * 60 * 60;

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

    /// The counts, holds and suggestions of every write or delete rule and every held tool.
    pub(crate) fn read_autonomy(&self, profile: &str) -> ApiResult<Autonomy> {
        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        let thresholds = config.autonomy;
        let since = thresholds.map_or(i64::MIN, |t| self.now() - i64::from(t.window_days) * DAY_SECS);
        let counts = store.decision_counts(since).map_err(store_error)?;
        let held = store.held_tools().map_err(store_error)?;
        let rules = &config.policy.rules;
        let writes = rules
            .iter()
            .filter(|(_, rule)| rule.class != ToolClass::Read)
            .map(|(tool, _)| tool);
        let tools: BTreeSet<&String> = writes.chain(&held).collect();
        let tools = tools
            .into_iter()
            .map(|tool| {
                let counts = counts.get(tool).copied().unwrap_or_default();
                let held = held.contains(tool);
                let suggested = rules
                    .get(tool)
                    .is_some_and(|rule| suggest(thresholds.as_ref(), tool, rule, &counts, held));
                ToolAutonomy {
                    tool: tool.clone(),
                    counts,
                    held,
                    suggested,
                }
            })
            .collect();
        Ok(Autonomy { thresholds, tools })
    }

    /// Records the owner's report of a problem with a call that ran without the owner,
    /// and holds its tool at approval.
    pub(crate) fn report(&self, profile: &str, inbox_id: i64) -> ApiResult<()> {
        let store = self.store(profile)?;
        let item = store.inbox_item(inbox_id).map_err(store_error)?;
        let InboxBody::ToolCall { tool, .. } = &item.body else {
            return Err(ApiError::NotAllowed("only a tool call can be reported".into()));
        };
        if !item.autonomous {
            return Err(ApiError::NotAllowed(
                "this call did not run without you; dismiss a waiting call instead".into(),
            ));
        }
        if !matches!(item.state, ApprovalState::Executed | ApprovalState::Failed) {
            return Err(ApiError::NotAllowed(format!(
                "the item is {}, not executed or failed",
                item.state.as_str()
            )));
        }
        if !store.add_report(self.now(), inbox_id, tool).map_err(store_error)? {
            return Err(ApiError::NotAllowed(format!("item {inbox_id} was reported before")));
        }
        let detail = json!({"inbox_id": inbox_id, "tool": tool});
        self.audit(&store, "autonomous_reported", Some(&item.event_key), detail)?;
        self.demote(&store, &item, tool, "reported")
    }

    /// Clears the hold of a tool. Only the owner does this.
    pub(crate) fn clear_hold(&self, profile: &str, tool: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        if !store.clear_hold(self.now(), tool).map_err(store_error)? {
            return Err(ApiError::NotAllowed(format!("`{tool}` is not held")));
        }
        self.audit(&store, "autonomy_hold_cleared", None, json!({"tool": tool}))
    }
}
