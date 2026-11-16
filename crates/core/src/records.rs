//! Rows that the store keeps and the UI shows.

use crate::approval::ApprovalState;
use crate::context::Correction;
use crate::wiki::{WikiSource, WikiText};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileInfo {
    pub id: String,
    pub display_name: String,
}

/// What an inbox item asks the user to look at.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InboxBody {
    /// A tool call the agent wants to make. `tool` is a tool ID.
    ToolCall { tool: String, args: Map<String, Value> },
    /// An event the user must handle.
    Escalation { summary: String },
    /// A change to a page of the agent's notes. Approving it stores the page.
    WikiChange {
        slug: String,
        text: WikiText,
        sources: Vec<WikiSource>,
        /// The revision of the page the change was written against. 0 for a new page.
        base_revision: u64,
        /// The text the change replaces, for a comparison. `None` for a new page.
        previous: Option<WikiText>,
    },
}

/// One read the agent made before it wrote a proposal.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReadNote {
    /// A tool ID: `<connector>/<tool>`.
    pub tool: String,
    pub args: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: i64,
    pub event_key: String,
    pub body: InboxBody,
    pub rationale: String,
    /// The reads the proposal is based on, in order. The owner sees them before deciding.
    #[serde(default)]
    pub basis: Vec<ReadNote>,
    pub state: ApprovalState,
    pub created_at: i64,
    pub updated_at: i64,
    /// True if the policy approved the item without the owner (tier `autonomous`).
    #[serde(default)]
    pub autonomous: bool,
    /// The item that this one repeats, after a stop cut it or kept it from running.
    #[serde(default)]
    pub retry_of: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub id: i64,
    pub at: i64,
    /// A short machine-readable name, for example `policy_decision`.
    pub kind: String,
    pub event_key: Option<String>,
    pub detail: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMeta {
    pub id: i64,
    /// For example `llm_reply`, `proposal`, `tool_result`.
    pub kind: String,
    pub sha256: String,
    pub size: u64,
    pub created_at: i64,
    pub event_key: Option<String>,
    pub inbox_id: Option<i64>,
}

/// The LLM calls of one day and one purpose, added up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmUsage {
    /// The UTC date, for example `2026-11-17`.
    pub day: String,
    /// What the calls were for, for example `event`, `replay` or `llm_test`.
    pub purpose: String,
    pub calls: u64,
    /// Calls that did not end with a whole reply: errors, refusals and replies cut at the output limit.
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The time of all calls together, in milliseconds.
    pub latency_ms: u64,
}

/// A stored correction, as the owner reviews it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorrectionRecord {
    pub id: i64,
    /// The inbox item whose draft the owner edited.
    pub inbox_id: i64,
    #[serde(flatten)]
    pub correction: Correction,
    pub created_at: i64,
    /// Unix seconds. A retired correction stays stored, but no prompt gets it.
    pub retired_at: Option<i64>,
}

/// One decision about a call of a tool. The store keeps them in an append-only table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionKind {
    /// The owner approved a waiting call without a change.
    Approved,
    /// The owner changed the text of a waiting call and approved it.
    Edited,
    /// The owner dismissed a waiting call.
    Rejected,
    /// The policy ran a call without the owner.
    Autonomous,
    /// The owner reported a problem with a call that ran without the owner.
    Reported,
    /// The tool is held at approval: its calls ask the owner whatever the step says.
    Demoted,
    /// The owner cleared the hold of the tool.
    HoldCleared,
}

impl DecisionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DecisionKind::Approved => "approved",
            DecisionKind::Edited => "edited",
            DecisionKind::Rejected => "rejected",
            DecisionKind::Autonomous => "autonomous",
            DecisionKind::Reported => "reported",
            DecisionKind::Demoted => "demoted",
            DecisionKind::HoldCleared => "hold_cleared",
        }
    }
}

/// The decisions about the calls of one tool, counted since its last hold began.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionCounts {
    /// Waiting calls that the owner approved without a change.
    pub approved: u32,
    /// Waiting calls that the owner changed and approved.
    pub edited: u32,
    /// Waiting calls that the owner dismissed.
    pub rejected: u32,
    /// Calls that ran without the owner.
    pub autonomous: u32,
    /// Of the calls that ran without the owner, the ones that failed.
    pub autonomous_failed: u32,
}

impl DecisionCounts {
    /// The waiting calls that the owner decided about.
    pub fn reviewed(&self) -> u32 {
        self.approved + self.edited + self.rejected
    }

    /// The share of reviewed calls approved without a change, rounded down. `None` without a review.
    pub fn unchanged_percent(&self) -> Option<u32> {
        let reviewed = self.reviewed();
        (reviewed > 0).then(|| self.approved * 100 / reviewed)
    }
}
