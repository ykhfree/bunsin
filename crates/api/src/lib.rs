//! The interface between a user interface (GPUI app, CLI) and the engine.

use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{DenyReason, Policy};
use bunsin_core::ports::BoxFuture;
use bunsin_core::proposal::Proposal;
use bunsin_core::records::{ArtifactMeta, AuditEntry, InboxItem, ProfileInfo};
use bunsin_core::tool::ToolSpec;
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
}

pub type ApiResult<T> = Result<T, ApiError>;

pub trait EngineApi: Send + Sync {
    fn list_profiles(&self) -> BoxFuture<'_, ApiResult<Vec<ProfileInfo>>>;
    fn create_profile<'a>(
        &'a self,
        id: &'a str,
        config: ProfileConfig,
    ) -> BoxFuture<'a, ApiResult<ProfileInfo>>;
    fn profile_config<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileConfig>>;
    fn set_profile_config<'a>(
        &'a self,
        profile: &'a str,
        config: ProfileConfig,
    ) -> BoxFuture<'a, ApiResult<()>>;
    fn snapshot<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<ProfileSnapshot>>;
    fn ingest<'a>(
        &'a self,
        profile: &'a str,
        event: InboundEvent,
    ) -> BoxFuture<'a, ApiResult<Outcome>>;
    /// Approves a pending tool call, with an optional edit, and runs it.
    fn approve<'a>(
        &'a self,
        profile: &'a str,
        inbox_id: i64,
        edit: Option<Edit>,
    ) -> BoxFuture<'a, ApiResult<Outcome>>;
    fn reject<'a>(
        &'a self,
        profile: &'a str,
        inbox_id: i64,
        reason: &'a str,
    ) -> BoxFuture<'a, ApiResult<()>>;
    /// Returns the state the engine stored.
    fn set_kill_switch<'a>(&'a self, profile: &'a str, on: bool) -> BoxFuture<'a, ApiResult<bool>>;
    fn replay<'a>(
        &'a self,
        profile: &'a str,
        case: ReplayCase,
    ) -> BoxFuture<'a, ApiResult<ReplayResult>>;
}
