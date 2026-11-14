//! Rows that the store keeps and the UI shows.

use crate::approval::ApprovalState;
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
