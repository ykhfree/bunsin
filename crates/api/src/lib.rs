//! The interface between a user interface (GPUI app, CLI) and the engine.

pub mod http;

use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::candidates::{PrincipleCandidate, PrincipleSource};
use bunsin_core::eval::{CaseScore, EvalConfig, EvalPass, EvalResult, EvalRun, EvalSummary, Expect, ReplayCaseRecord};
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{DenyReason, Policy, ToolRule};
use bunsin_core::poll::PollConfig;
use bunsin_core::ports::BoxFuture;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::proposal::Proposal;
use bunsin_core::records::{
    ArtifactMeta, AuditEntry, CorrectionRecord, DecisionCounts, InboxItem, LlmUsage, ProfileInfo,
};
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
    /// When the window suggests that a tool run without asking. `None`: it suggests nothing.
    #[serde(default)]
    pub autonomy: Option<AutonomyThresholds>,
    /// The owner's pass mark for replay evaluation.
    #[serde(default)]
    pub eval: EvalConfig,
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
            autonomy: None,
            eval: EvalConfig::default(),
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

/// How the owner resolves a tool call whose outcome is unknown, because a stop cut it while it ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    /// The call reached its tool. The item becomes `executed`.
    Done,
    /// The item becomes `failed`, and a copy of the call runs now as a new item.
    /// If the first call reached its tool, the tool gets the call twice.
    Retry,
    /// The item becomes `failed`. Nothing runs.
    Drop,
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

/// The permission table of a profile, with the tools that a rule can be for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Permissions {
    /// The tools the owner approved, by connector name, as each connector listed them then.
    pub tools: BTreeMap<String, Vec<ToolSpec>>,
    /// The rules by tool ID: `<connector>/<tool>`. A tool without a rule cannot be used.
    pub rules: BTreeMap<String, ToolRule>,
    /// The text that the server adds at each place of outgoing text that a rule names.
    #[serde(default)]
    pub ai_marker: String,
}

/// The decisions about the calls of each write or delete tool, with the holds and the suggestions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Autonomy {
    /// The owner's thresholds. `None`: no tool is suggested, and the counts cover all days.
    pub thresholds: Option<AutonomyThresholds>,
    /// By tool ID: every write or delete rule, and every held tool.
    pub tools: Vec<ToolAutonomy>,
}

/// The decisions about one tool's calls since its last hold, within the days of the thresholds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolAutonomy {
    pub tool: String,
    pub counts: DecisionCounts,
    /// True while the owner's hold keeps the tool at approval, whatever its step says.
    pub held: bool,
    /// True if the counts meet the thresholds and the owner could raise the step to `autonomous`.
    /// Only the owner raises it, with `set_permissions`.
    pub suggested: bool,
}

/// A new permission table, with the table the caller loaded before it changed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionsUpdate {
    /// Every rule. A rule that is not in here is removed.
    pub rules: BTreeMap<String, ToolRule>,
    #[serde(default)]
    pub expected: Expected<BTreeMap<String, ToolRule>>,
}

/// The owner's new principles, with the list the caller loaded before it changed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrinciplesUpdate {
    /// Every principle, in the order of the prompt. A principle that is not in here is removed.
    pub principles: Vec<String>,
    #[serde(default)]
    pub expected: Expected<Vec<String>>,
}

/// The owner's adoption of a principle candidate: one text appended to the principles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptPrinciple {
    pub id: i64,
    /// The owner's edit of the candidate's text. `None` adopts the text as it is.
    #[serde(default)]
    pub text: Option<String>,
    /// The principles the caller loaded. The adoption is refused if the server holds others.
    #[serde(default)]
    pub expected: Expected<Vec<String>>,
}

/// New polls, with the list the caller loaded before it changed it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PollsUpdate {
    /// Every poll. A poll that is not in here is removed.
    pub polls: Vec<PollConfig>,
    #[serde(default)]
    pub expected: Expected<Vec<PollConfig>>,
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
    /// The header that carries the token as it is. `None`: `Authorization: Bearer <token>`.
    #[serde(default)]
    pub token_header: Option<String>,
    /// The names of the secret environment variables of a command. Their values are never sent.
    #[serde(default)]
    pub secret_env: Vec<String>,
    /// The names in `secret_env` for which the server has no value. A settings file can name
    /// a variable, but only the window gives it a value.
    #[serde(default)]
    pub secret_env_missing: Vec<String>,
}

/// What to do with the value of one secret environment variable when a command is saved.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretChange {
    /// Keep the stored value. Refused if no value is stored.
    Keep,
    Set(String),
}

impl std::fmt::Debug for SecretChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The value is left out on purpose.
        f.write_str(match self {
            SecretChange::Keep => "Keep",
            SecretChange::Set(_) => "Set([REDACTED])",
        })
    }
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

/// A connector that is reached over HTTP. A connector that runs a command has `CommandConnectorUpdate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectorUpdate {
    pub url: String,
    pub token: TokenChange,
    /// The header that carries the token as it is, for example `X-API-Key`.
    /// `None`: the token goes as `Authorization: Bearer <token>`.
    #[serde(default)]
    pub token_header: Option<String>,
    /// The connector the caller loaded, or `None` for a new connector.
    #[serde(default)]
    pub expected: Expected<Option<ConnectorConnection>>,
}

/// A connector that the server starts as a command (the stdio transport).
/// The server stores it only if its operator allows such connectors.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandConnectorUpdate {
    /// The absolute path of the program.
    pub command: std::path::PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables of the program. They are stored as plain text.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Environment variables whose values the server encrypts and does not show again.
    /// A stored variable that is not named here is removed with its value.
    #[serde(default)]
    pub secret_env: BTreeMap<String, SecretChange>,
    /// The connector the caller loaded, or `None` for a new connector.
    #[serde(default)]
    pub expected: Expected<Option<ConnectorConnection>>,
}

/// What one test request to the LLM endpoint did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LlmTest {
    /// True if the endpoint answered with a reply the agent can use.
    pub works: bool,
    /// What the endpoint answered, or why the request failed. It never holds the token.
    pub detail: String,
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
    /// What the owner expects the agent to do. The default is a reply.
    #[serde(default)]
    pub expect: Expect,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayResult {
    pub proposal: Option<Proposal>,
    pub error: Option<String>,
    pub actual_reply: String,
    /// The score of the proposal, computed without an LLM. `None` if there is no proposal.
    #[serde(default)]
    pub score: Option<CaseScore>,
}

/// The newest evaluation run, with its results and rates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalReport {
    /// `None` if no run was started.
    pub run: Option<EvalRun>,
    /// The cases the run has not replayed yet. 0 when it is done.
    pub remaining: u32,
    pub results: Vec<EvalResult>,
    pub summary: EvalSummary,
    /// The owner's pass mark, from the settings.
    pub pass: Option<EvalPass>,
    /// Whether the finished run meets the pass mark. `None` while it runs or without a mark.
    pub passed: Option<bool>,
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
    /// Saves a connector that the server starts as a command.
    fn set_command_connector<'a>(
        &'a self,
        profile: &'a str,
        name: &'a str,
        update: CommandConnectorUpdate,
    ) -> BoxFuture<'a, ApiResult<()>>;
    /// Removes a connector and its stored token.
    fn remove_connector<'a>(&'a self, profile: &'a str, name: &'a str) -> BoxFuture<'a, ApiResult<()>>;
    /// The agent's notes: one line per page.
    /// The stored corrections, also the retired ones. Newest first, at most 200.
    fn corrections<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<CorrectionRecord>>>;
    /// Retires a correction: it stays stored, but no prompt gets it. A retired correction cannot be retired again.
    fn retire_correction<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>>;
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
    /// The permission table and the approved tools. It connects to nothing.
    fn permissions<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Permissions>>;
    /// Replaces the permission table.
    fn set_permissions<'a>(&'a self, profile: &'a str, update: PermissionsUpdate) -> BoxFuture<'a, ApiResult<()>>;
    /// The owner's principles, in the order of the prompt.
    fn principles<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<String>>>;
    /// Replaces the principles. Each must have 1 to 1,000 characters; at most 50 are allowed.
    fn set_principles<'a>(&'a self, profile: &'a str, update: PrinciplesUpdate) -> BoxFuture<'a, ApiResult<()>>;
    /// The searches the server repeats to find new events.
    fn polls<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<PollConfig>>>;
    /// Replaces the polls. The tool of each poll needs a `read` rule and must be in the approved list.
    fn set_polls<'a>(&'a self, profile: &'a str, update: PollsUpdate) -> BoxFuture<'a, ApiResult<()>>;
    /// Runs poll number `index` of the settings once, without handling its events.
    fn test_poll<'a>(&'a self, profile: &'a str, index: usize) -> BoxFuture<'a, ApiResult<PollPreview>>;
    /// Sends one request to the saved LLM endpoint and says whether the reply has the form the agent needs.
    /// The request holds no data of the profile. It costs tokens at the endpoint.
    fn test_llm<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<LlmTest>>;
    /// The LLM calls of the last 90 days, added up by UTC day and purpose. Newest day first.
    fn llm_usage<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<LlmUsage>>>;
    /// Resolves an `unknown` item. A retry approves a copy of the call with a new inbox ID and
    /// returns the outcome of that run. The approval checks the policy; if it refuses, the copy
    /// stays pending. Done and drop return `None`.
    fn resolve_unknown<'a>(
        &'a self,
        profile: &'a str,
        inbox_id: i64,
        resolution: Resolution,
    ) -> BoxFuture<'a, ApiResult<Option<Outcome>>>;
    /// The decisions about each write or delete tool's calls, its hold and whether it is suggested.
    fn autonomy<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Autonomy>>;
    /// Reports a problem with a call that ran without the owner. It holds the tool at approval.
    /// Only an autonomous tool call that is `executed` or `failed` can be reported, and only once.
    fn report_autonomous<'a>(&'a self, profile: &'a str, inbox_id: i64) -> BoxFuture<'a, ApiResult<()>>;
    /// Clears the hold of a tool. Its calls then follow its step again.
    fn clear_autonomy_hold<'a>(&'a self, profile: &'a str, tool: &'a str) -> BoxFuture<'a, ApiResult<()>>;
    /// Stores replay cases after the privacy filter. Returns their IDs, in the order of `cases`.
    fn add_replay_cases<'a>(&'a self, profile: &'a str, cases: Vec<ReplayCase>) -> BoxFuture<'a, ApiResult<Vec<i64>>>;
    /// The stored replay cases, oldest first, at most 200.
    fn replay_cases<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<ReplayCaseRecord>>>;
    /// Deletes a replay case. The results of earlier runs stay.
    fn delete_replay_case<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>>;
    /// Starts an evaluation run of every stored case and returns its ID. The server replays a few
    /// cases at a time in the background, also while the kill switch is on: a replay sends nothing.
    /// Refused while a run is running or if there is no case.
    fn start_eval<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<i64>>;
    /// The newest evaluation run and its results.
    fn eval_report<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<EvalReport>>;
    /// Asks the LLM for principles in the owner's texts of `source`, which it reads as quoted data after
    /// the privacy filter. Stores and returns the candidates that pass the checks. None becomes a principle.
    fn extract_principles<'a>(
        &'a self,
        profile: &'a str,
        source: PrincipleSource,
    ) -> BoxFuture<'a, ApiResult<Vec<PrincipleCandidate>>>;
    /// Every candidate, newest first, at most 200.
    fn principle_candidates<'a>(&'a self, profile: &'a str) -> BoxFuture<'a, ApiResult<Vec<PrincipleCandidate>>>;
    /// Appends the text of a new candidate, or the owner's edit of it, to the principles. The save has the
    /// same checks as `set_principles`.
    fn adopt_principle<'a>(&'a self, profile: &'a str, adoption: AdoptPrinciple) -> BoxFuture<'a, ApiResult<()>>;
    /// Dismisses a new candidate. A later extraction does not propose its text again.
    fn dismiss_principle<'a>(&'a self, profile: &'a str, id: i64) -> BoxFuture<'a, ApiResult<()>>;
}
