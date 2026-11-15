//! The interface between a user interface (GPUI app, CLI) and the engine.

pub mod http;

use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{DenyReason, Policy};
use bunsin_core::poll::PollConfig;
use bunsin_core::ports::BoxFuture;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::proposal::Proposal;
use bunsin_core::records::{ArtifactMeta, AuditEntry, InboxItem, ProfileInfo};
use bunsin_core::settings::{McpConfig, Wire};
use bunsin_core::tool::{PinDiff, PinSet, ToolSpec};
use bunsin_core::wiki::{WikiPage, WikiSummary, WikiText};
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
            // Room for the model's thinking and the JSON reply.
            max_output_tokens: 16_000,
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

/// Where the token of a connection comes from. The token itself is never sent back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenSource {
    /// The owner entered it. The server keeps it encrypted.
    Stored,
    /// The server operator set it in the server's environment.
    ServerEnvironment,
    None,
}

/// What the caller believes the server holds now. A save with `Value` is refused with
/// `ApiError::Conflict` if the server holds something else, so that one person's change
/// is not overwritten without notice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expected<T> {
    /// Save without a check.
    #[default]
    Unchecked,
    /// Save only if the server still holds this value.
    Value(T),
}

/// New privacy settings, with what the caller loaded before it changed them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrivacyUpdate {
    pub privacy: PrivacyConfig,
    #[serde(default)]
    pub expected: Expected<PrivacyConfig>,
}

impl PrivacyUpdate {
    pub fn unchecked(privacy: PrivacyConfig) -> Self {
        Self {
            privacy,
            expected: Expected::Unchecked,
        }
    }
}

/// What to do with the token when a connection is saved.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenChange {
    /// Keep the token as it is. Refused if the address moved to another host.
    Keep,
    Set(String),
    Clear,
}

impl std::fmt::Debug for TokenChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The token value is left out on purpose.
        f.write_str(match self {
            TokenChange::Keep => "Keep",
            TokenChange::Set(_) => "Set([REDACTED])",
            TokenChange::Clear => "Clear",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmConnection {
    pub wire: Wire,
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
    pub token: TokenSource,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorConnection {
    pub mcp: McpConfig,
    pub token: TokenSource,
}

/// The LLM endpoint and the connectors of a profile, without any token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connections {
    pub llm: Option<LlmConnection>,
    pub connectors: BTreeMap<String, ConnectorConnection>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmUpdate {
    pub wire: Wire,
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
    pub token: TokenChange,
    /// The LLM connection the caller loaded, or `None` if there was none.
    #[serde(default)]
    pub expected: Expected<Option<LlmConnection>>,
}

/// A connector that is reached over HTTP. Other transports are set by the server operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorUpdate {
    pub url: String,
    pub token: TokenChange,
    /// The connector the caller loaded, or `None` for a new connector.
    #[serde(default)]
    pub expected: Expected<Option<ConnectorConnection>>,
}

/// The owner's new text for a page of the agent's notes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WikiPageUpdate {
    #[serde(flatten)]
    pub text: WikiText,
    /// The revision the owner edited. `Some(0)` means "the page must not exist yet".
    /// `None` saves without a check.
    #[serde(default)]
    pub expected_revision: Option<u64>,
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
    /// The server holds another value than the caller expected. Nothing was saved.
    #[error("the settings changed on the server: {0}")]
    Conflict(String),
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
    fn set_privacy<'a>(&'a self, profile: &'a str, update: PrivacyUpdate) -> BoxFuture<'a, ApiResult<()>>;
    /// The LLM endpoint and the connectors, with where each token comes from.
    fn connections<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Connections>>;
    /// Saves the LLM endpoint and its token in one step.
    fn set_llm<'a>(&'a self, profile: &'a str, update: LlmUpdate) -> BoxFuture<'a, ApiResult<()>>;
    /// Saves one connector and its token in one step. Check it with `connector_tools`.
    fn set_connector<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        update: ConnectorUpdate,
    ) -> BoxFuture<'a, ApiResult<()>>;
    /// Removes a connector and its stored token.
    fn remove_connector<'a>(&'a self, profile: &'a str, name: &'a str) -> BoxFuture<'a, ApiResult<()>>;
    /// The agent's notes: one line per page.
    fn wiki_index<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<WikiSummary>>>;
    fn wiki_page<'a>(&'a self, profile: &'a str, slug: &'a str) -> BoxFuture<'a, ApiResult<WikiPage>>;
    /// Stores the owner's text for a page. The owner's text replaces the agent's. The sources of the page stay.
    fn set_wiki_page<'a>(
        &'a self,
        profile: &'a str,
        slug: &'a str,
        update: WikiPageUpdate,
    ) -> BoxFuture<'a, ApiResult<WikiPage>>;
    fn delete_wiki_page<'a>(&'a self, profile: &'a str, slug: &'a str) -> BoxFuture<'a, ApiResult<()>>;
    /// Every stored revision of a page, oldest first. A deletion is `None`.
    /// To restore an old text, save it again with `set_wiki_page`.
    fn wiki_history<'a>(&'a self, profile: &'a str, slug: &'a str) -> BoxFuture<'a, ApiResult<Vec<Option<WikiPage>>>>;
    /// Runs poll number `index` of the settings once, without handling its events.
    fn test_poll<'a>(&'a self, profile: &'a str, index: usize) -> BoxFuture<'a, ApiResult<PollPreview>>;
}
