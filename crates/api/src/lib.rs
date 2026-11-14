//! The interface between a user interface (GPUI app, CLI) and the engine.

pub mod http;

use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{DenyReason, Policy};
use bunsin_core::poll::PollConfig;
use bunsin_core::ports::BoxFuture;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::proposal::Proposal;
use bunsin_core::records::{ArtifactMeta, AuditEntry, InboxItem, ProfileInfo};
use bunsin_core::tool::{PinDiff, PinSet, ToolSpec};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

/// Everything that differs between two agents.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileConfig {
    /// The person the agent acts for.
    pub owner_name: String,
    /// Appended to every text the agent sends to other people.
    pub ai_marker: String,
    pub principles: Vec<String>,
    pub policy: Policy,
    /// The tools the user approved, by connector name, as the connector listed them then.
    pub approved_tools: BTreeMap<String, Vec<ToolSpec>>,
    pub max_output_tokens: u32,
    /// Token ceiling for the handling of one event.
    pub token_budget: u64,
    /// LLM endpoint settings. The backend that builds the client reads them.
    pub llm: Value,
    /// Connector settings by connector name. The backend that builds the connector reads them.
    pub connectors: BTreeMap<String, Value>,
    /// What is removed from text before it is stored or sent to the LLM.
    /// Settings from before this field existed get the default: every built-in kind on.
    #[serde(default)]
    pub privacy: PrivacyConfig,
    /// The searches the server repeats to find new events.
    #[serde(default)]
    pub polls: Vec<PollConfig>,
}

impl ProfileConfig {
    /// A profile that can do nothing until the user adds tools and rules.
    pub fn new(owner_name: &str) -> Self {
        Self {
            owner_name: owner_name.to_owned(),
            ai_marker: format!("— AI agent (Bunsin) acting for {owner_name}"),
            principles: Vec::new(),
            policy: Policy::default(),
            approved_tools: BTreeMap::new(),
            max_output_tokens: 4096,
            token_budget: 200_000,
            llm: Value::Null,
            connectors: BTreeMap::new(),
            privacy: PrivacyConfig::default(),
            polls: Vec::new(),
        }
    }
}

/// What the engine did with one event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    /// The same event was handled before.
    Duplicate,
    Ignored,
    Escalated {
        inbox_id: i64,
    },
    /// The proposal was saved. Nothing was shown or run.
    Recorded,
    Drafted {
        inbox_id: i64,
    },
    PendingApproval {
        inbox_id: i64,
    },
    Executed {
        inbox_id: i64,
    },
    /// The tool call ran and failed.
    ExecutionFailed {
        inbox_id: i64,
        error: String,
    },
    Denied {
        reason: DenyReason,
    },
    /// No usable proposal was produced.
    Failed {
        error: String,
    },
}

/// The user's change to a pending tool call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Edit {
    pub args: Map<String, Value>,
    /// Why the user changed the draft. The agent learns from it.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSnapshot {
    pub profile: ProfileInfo,
    /// The kill switch state as stored by the engine.
    pub kill_switch: bool,
    pub inbox: Vec<InboxItem>,
    pub audit: Vec<AuditEntry>,
    pub artifacts: Vec<ArtifactMeta>,
    /// One entry per poll in the settings, in the same order.
    #[serde(default)]
    pub polls: Vec<PollStatus>,
}

/// What the last run of one poll did.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollStatus {
    /// `<connector>/<tool>`.
    pub tool: String,
    /// Unix seconds. `None` if the poll has not run.
    pub last_run_at: Option<i64>,
    /// How many items the tool result held.
    pub items: u32,
    /// How many of them had an ID, text and an author. If `items` is above zero and this
    /// is zero, the mapping in the settings does not fit the result.
    pub usable: u32,
    pub new_events: u32,
    pub error: Option<String>,
}

/// The tools a connector lists now, compared with the approved ones.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolsReport {
    pub tools: Vec<ToolSpec>,
    /// Tools that are new, changed or gone since the owner approved the list.
    pub diff: PinDiff,
}

/// What a poll would hand to the agent. Nothing is stored and no event is handled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PollPreview {
    pub items: u32,
    /// The usable items after the privacy filter, without the skipped authors.
    pub events: Vec<InboundEvent>,
}

/// One past case for replay evaluation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayCase {
    pub event: InboundEvent,
    /// What the person really answered. The agent never sees it.
    pub actual_reply: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    pub proposal: Option<Proposal>,
    pub error: Option<String>,
    pub actual_reply: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum ApiError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("not allowed: {0}")]
    NotAllowed(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("internal error: {0}")]
    Internal(String),
    /// The access token is missing, unknown, revoked or for another profile.
    #[error("not authorized: {0}")]
    Unauthorized(String),
    /// The server did not answer. The request may or may not have run.
    #[error("the server cannot be reached: {0}")]
    Unreachable(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

/// What a profile owner can do. Profiles and access tokens are created on the
/// server by its operator, so they are not part of this interface.
pub trait EngineApi: Send + Sync {
    /// The profiles the caller may use.
    fn list_profiles(&self) -> BoxFuture<'_, ApiResult<Vec<ProfileInfo>>>;
    fn profile_config<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileConfig>>;
    fn set_profile_config<'a>(&'a self, profile: &'a str, config: ProfileConfig) -> BoxFuture<'a, ApiResult<()>>;
    fn snapshot<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileSnapshot>>;
    fn ingest<'a>(&'a self, profile: &'a str, event: InboundEvent) -> BoxFuture<'a, ApiResult<Outcome>>;
    /// Approves a pending tool call, with an optional edit, and runs it.
    fn approve<'a>(&'a self, profile: &'a str, inbox_id: i64, edit: Option<Edit>) -> BoxFuture<'a, ApiResult<Outcome>>;
    fn reject<'a>(&'a self, profile: &'a str, inbox_id: i64, reason: &'a str) -> BoxFuture<'a, ApiResult<()>>;
    /// Returns the state the engine stored.
    fn set_kill_switch<'a>(&'a self, profile: &'a str, on: bool) -> BoxFuture<'a, ApiResult<bool>>;
    fn replay<'a>(&'a self, profile: &'a str, case: ReplayCase) -> BoxFuture<'a, ApiResult<ReplayResult>>;
    /// Lists the tools of a connector as its server describes them now.
    fn connector_tools<'a>(&'a self, profile: &'a str, connector: &'a str) -> BoxFuture<'a, ApiResult<ToolsReport>>;
    /// Approves the tool list that was shown. `shown` holds the fingerprint of each tool.
    /// The call fails if the connector lists something else by now.
    fn approve_tools<'a>(&'a self, profile: &'a str, connector: &'a str, shown: PinSet)
    -> BoxFuture<'a, ApiResult<()>>;
    fn privacy<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<PrivacyConfig>>;
    /// Replaces only the privacy part of the settings.
    fn set_privacy<'a>(&'a self, profile: &'a str, privacy: PrivacyConfig) -> BoxFuture<'a, ApiResult<()>>;
    /// Runs poll number `index` of the settings once, without handling its events.
    fn test_poll<'a>(&'a self, profile: &'a str, index: usize) -> BoxFuture<'a, ApiResult<PollPreview>>;
}
