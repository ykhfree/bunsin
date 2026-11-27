//! Runs engine calls on tokio and hands the resulting state to the window.
//!
//! The window never changes its own copy of engine state. It sends a command
//! and shows the state that comes back.

use bunsin_api::{
    AdoptPrinciple, ApiConnectorUpdate, ApiError, ApiResult, ArdRegistriesUpdate, Autonomy, AutonomyThresholdsUpdate,
    CommandConnectorUpdate, Connections, ConnectorUpdate, Edit, EngineApi, EvalReport, EvalSettingsUpdate,
    InboundStatus, InboundUpdate, LlmTest, LlmUpdate, NewCallerToken, NotesSettingsUpdate, Outcome, Permissions,
    PermissionsUpdate, PollPreview, PollsUpdate, PreloadNotes, PrinciplesUpdate, PrivacyUpdate, ProfileSnapshot,
    ReplayCase, Resolution, ToolsReport, WebhooksUpdate, WikiPageUpdate,
};
use bunsin_core::ard::ArdRegistry;
use bunsin_core::candidates::{PrincipleCandidate, PrincipleSource};
use bunsin_core::eval::{EvalConfig, ReplayCaseRecord};
use bunsin_core::inbound::InboundConfig;
use bunsin_core::lint::LintFinding;
use bunsin_core::poll::PollConfig;
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::records::{CallerToken, CorrectionRecord, ProfileInfo};
use bunsin_core::tool::PinSet;
use bunsin_core::webhook::WebhookConfig;
use bunsin_core::wiki::{NotesConfig, WikiPage, WikiSummary};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::mpsc;

/// A command that names a profile is refused if that profile is no longer selected.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    Refresh,
    SelectProfile(String),
    Approve(i64),
    /// Approves a pending tool call with the owner's change of its text, and runs it.
    ApproveEdited {
        profile: String,
        id: i64,
        edit: Edit,
    },
    Reject(i64),
    /// Resolves an item whose call a stop cut. A retry runs a copy of the call.
    ResolveUnknown(i64, Resolution),
    /// Reports a problem with a call that ran without asking. Its tool is then held at approval.
    ReportAutonomous(i64),
    /// Clears the hold of a tool.
    ClearAutonomyHold {
        profile: String,
        tool: String,
    },
    SetKillSwitch(bool),
    SavePrivacy {
        profile: String,
        update: PrivacyUpdate,
    },
    /// Replaces the permission rules.
    SavePermissions {
        profile: String,
        update: PermissionsUpdate,
    },
    /// Replaces the owner's principles.
    SavePrinciples {
        profile: String,
        update: PrinciplesUpdate,
    },
    /// Replaces the polls.
    SavePolls {
        profile: String,
        update: PollsUpdate,
    },
    /// Runs saved poll number `index` once, without handling its events.
    TestPoll {
        profile: String,
        index: usize,
    },
    /// Saves the LLM endpoint. A typed token travels only in this command.
    SaveLlm {
        profile: String,
        update: LlmUpdate,
    },
    /// Saves one connector. A typed token travels only in this command.
    SaveConnector {
        profile: String,
        name: String,
        update: ConnectorUpdate,
    },
    /// Saves one connector that the server starts as a command.
    SaveCommandConnector {
        profile: String,
        name: String,
        update: CommandConnectorUpdate,
    },
    /// Saves one connector that calls a REST API.
    SaveApiConnector {
        profile: String,
        name: String,
        update: ApiConnectorUpdate,
    },
    RemoveConnector {
        profile: String,
        name: String,
    },
    /// Sends one test request to the saved LLM endpoint.
    TestLlm {
        profile: String,
    },
    /// Connects the connector and lists its tools.
    CheckConnector {
        profile: String,
        name: String,
    },
    /// Approves the tool list that was shown. `shown` holds the fingerprint of each tool.
    ApproveTools {
        profile: String,
        name: String,
        shown: PinSet,
    },
    /// Signs in to the MCP server of an OAuth connector. The bridge opens a one-time listener
    /// on 127.0.0.1 for the browser and then sends `StartSignIn`.
    SignIn {
        profile: String,
        name: String,
    },
    /// Starts the sign-in with the redirect address of the listener.
    StartSignIn {
        profile: String,
        name: String,
        redirect_uri: String,
    },
    /// Ends the sign-in with the address the browser went to, or with why none came.
    FinishSignIn {
        profile: String,
        name: String,
        callback: Result<String, String>,
    },
    /// Opens a page of the notes. The page is then reloaded with every refresh.
    OpenNote {
        profile: String,
        slug: String,
    },
    /// Stops loading the open page.
    CloseNote,
    /// Searches the notes. The search runs again with every refresh, until `ClearNoteSearch`.
    SearchNotes {
        profile: String,
        query: String,
    },
    /// Stops the search. The window then lists every page again.
    ClearNoteSearch,
    /// Loads the revisions of the open page with every refresh (`show`), or stops loading them.
    NoteHistory {
        profile: String,
        show: bool,
    },
    /// Stores the owner's text for a page. After a successful save that page is the open one.
    SaveNote {
        profile: String,
        slug: String,
        update: WikiPageUpdate,
    },
    DeleteNote {
        profile: String,
        slug: String,
    },
    /// Retires a correction: the agent no longer gets it.
    RetireCorrection {
        profile: String,
        id: i64,
    },
    /// Asks the LLM for principle candidates in the owner's texts.
    ExtractPrinciples {
        profile: String,
        source: PrincipleSource,
    },
    /// Appends a candidate, or the owner's edit of it, to the principles.
    AdoptPrinciple {
        profile: String,
        adoption: AdoptPrinciple,
    },
    DismissPrinciple {
        profile: String,
        id: i64,
    },
    DeleteReplayCase {
        profile: String,
        id: i64,
    },
    /// Stores past cases for the evaluation. The server filters personal data before it stores them.
    AddReplayCases {
        profile: String,
        cases: Vec<ReplayCase>,
    },
    /// Starts an evaluation run. The server replays the cases in the background.
    StartEval {
        profile: String,
    },
    /// Replaces the thresholds of the autonomy suggestions.
    SaveAutonomyThresholds {
        profile: String,
        update: AutonomyThresholdsUpdate,
    },
    /// Checks the notes now with the server's settings. A check changes no page.
    LintNotes {
        profile: String,
    },
    /// Dismisses a finding of a check of the notes.
    DismissFinding {
        profile: String,
        id: i64,
    },
    /// Calls one tool that reads. The agent proposes pages of the notes from the result.
    PreloadNotes {
        profile: String,
        request: PreloadNotes,
    },
    /// Creates a caller token for the inbound endpoint. The state then holds it until the next command.
    AddCaller {
        profile: String,
        label: String,
    },
    RevokeCaller {
        profile: String,
        id: i64,
    },
    /// Replaces the settings of the inbound endpoint and its ARD listing. Boxed: the settings are
    /// the largest of all commands.
    SaveInbound {
        profile: String,
        update: Box<InboundUpdate>,
    },
    /// Replaces the ARD registries.
    SaveArdRegistries {
        profile: String,
        update: ArdRegistriesUpdate,
    },
    /// Replaces the webhooks. A removed webhook loses its signing secret on the server.
    SaveWebhooks {
        profile: String,
        update: WebhooksUpdate,
    },
    /// Stores the signing secret of a webhook. A typed secret travels only in this command.
    SetWebhookSecret {
        profile: String,
        name: String,
        secret: Typed,
    },
    /// Replaces the settings of the notes.
    SaveNotesSettings {
        profile: String,
        update: NotesSettingsUpdate,
    },
    /// Replaces the evaluation settings.
    SaveEvalSettings {
        profile: String,
        update: EvalSettingsUpdate,
    },
}

/// A secret that the owner typed. Its debug form leaves the value out.
#[derive(Clone, PartialEq)]
pub struct Typed(pub String);

impl std::fmt::Debug for Typed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// A part of the settings that a card of the window saves by itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    Inbound,
    ArdRegistries,
    Webhooks,
    WebhookSecret,
    Notes,
    Eval,
}

/// The page of the notes that the window has open.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenNote {
    pub profile: String,
    pub slug: String,
    /// None: the server has no page with this name.
    pub page: Option<WikiPage>,
    /// Every stored revision of the page, oldest first, if the window shows them. A deletion is `None`.
    pub history: Option<Vec<Option<WikiPage>>>,
}

/// The search of the notes that the window shows.
#[derive(Debug, Clone, PartialEq)]
pub struct NoteSearch {
    pub profile: String,
    pub query: String,
    /// The matching pages, best first, or why the server refused the query.
    /// None until the next reload.
    pub found: Option<Result<Vec<WikiSummary>, String>>,
}

/// What the last notes command did.
#[derive(Debug, Clone, PartialEq)]
pub enum NoteResult {
    Saved {
        slug: String,
        result: Result<(), SaveError>,
    },
    Deleted {
        slug: String,
        result: Result<(), String>,
    },
}

/// What the last poll command did.
#[derive(Debug, Clone, PartialEq)]
pub enum PollResult {
    Saved(Result<(), SaveError>),
    /// The outer error is a failure of the test or of the server. The events are inside.
    Tested {
        index: usize,
        result: Result<PollPreview, String>,
    },
}

/// Why a save did nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum SaveError {
    /// The server holds another value than the one the form loaded.
    Conflict,
    /// Any other failure, as the text to show.
    Failed(String),
}

/// What the last connection command did. A save keeps a conflict apart from other failures.
/// Every other error is the text to show.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionResult {
    LlmSaved(Result<(), SaveError>),
    /// The outer error is a failure to reach the Bunsin server. The test result is inside.
    LlmTested(Result<LlmTest, String>),
    ConnectorSaved {
        name: String,
        result: Result<(), SaveError>,
    },
    ConnectorRemoved {
        name: String,
        result: Result<(), String>,
    },
    Checked {
        name: String,
        result: Result<ToolsReport, String>,
    },
    ToolsApproved {
        name: String,
        result: Result<(), String>,
    },
    /// The address of the page where the owner signs in.
    SignInStarted {
        name: String,
        result: Result<String, String>,
    },
    SignedIn {
        name: String,
        result: Result<(), String>,
    },
}

/// What the window shows. Every field comes from the engine. No access token and no signing secret
/// is ever in it, and a caller token only in `new_caller`, from its creation until the next command.
/// The arguments and the plain environment variables of a command connector are in it. The value
/// of a secret variable is not: the state holds only its name.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiState {
    pub profiles: Vec<ProfileInfo>,
    pub selected: Option<String>,
    pub snapshot: Option<ProfileSnapshot>,
    /// The privacy settings of the selected profile, as the server has them.
    pub privacy: Option<PrivacyConfig>,
    /// The result of the last command, if it was `SavePrivacy`. A failed save is shown here, not in `error`.
    pub privacy_result: Option<Result<(), SaveError>>,
    /// How many `SavePrivacy` commands the bridge has run. It changes once per save.
    pub privacy_saves: u64,
    /// The approved tools and the permission rules of the selected profile, as the server has them.
    pub permissions: Option<Permissions>,
    /// The result of the last command, if it was `SavePermissions`. A failed save is shown here, not in `error`.
    pub permissions_result: Option<Result<(), SaveError>>,
    /// How many `SavePermissions` commands the bridge has run.
    pub permission_saves: u64,
    /// The decisions about each write or delete tool, its hold and its suggestion, as the server has them.
    pub autonomy: Option<Autonomy>,
    /// The principles of the selected profile, as the server has them.
    pub principles: Option<Vec<String>>,
    /// The result of the last command, if it was `SavePrinciples`. A failed save is shown here, not in `error`.
    pub principles_result: Option<Result<(), SaveError>>,
    /// How many `SavePrinciples` commands the bridge has run.
    pub principle_saves: u64,
    /// The polls of the selected profile, as the server has them.
    pub polls: Option<Vec<PollConfig>>,
    /// The result of the last command, if it was a poll command. A failure is shown here, not in `error`.
    pub poll_result: Option<PollResult>,
    /// How many poll commands the bridge has run.
    pub poll_commands: u64,
    /// The LLM endpoint and the connectors of the selected profile, as the server has them.
    pub connections: Option<Connections>,
    /// The result of the last command, if it was a connection command. A failure is shown here, not in `error`.
    pub connection_result: Option<ConnectionResult>,
    /// How many connection commands the bridge has run. It changes once per command.
    pub connection_commands: u64,
    /// The index of the selected profile's notes: one entry per page.
    pub notes: Option<Vec<WikiSummary>>,
    /// The page the window has open, as the server has it.
    pub open_note: Option<OpenNote>,
    /// The search of the notes, if one is active.
    pub note_search: Option<NoteSearch>,
    /// The result of the last command, if it was `SaveNote` or `DeleteNote`. A failure is shown here, not in `error`.
    pub note_result: Option<NoteResult>,
    /// How many `SaveNote` and `DeleteNote` commands the bridge has run.
    pub note_commands: u64,
    /// The owner's corrections of the selected profile, newest first, as the server has them.
    pub corrections: Option<Vec<CorrectionRecord>>,
    /// The principle candidates of the selected profile, newest first.
    pub candidates: Option<Vec<PrincipleCandidate>>,
    /// How many candidates the last `ExtractPrinciples` stored, or why it failed. Not an `error`.
    pub extract_result: Option<Result<usize, String>>,
    /// The replay cases of the selected profile, oldest first.
    pub replay_cases: Option<Vec<ReplayCaseRecord>>,
    /// How many cases the last `AddReplayCases` stored, or why it failed. Not an `error`.
    pub cases_result: Option<Result<usize, String>>,
    /// How many `AddReplayCases` commands the bridge has run.
    pub case_adds: u64,
    /// The newest evaluation run of the selected profile.
    pub eval_report: Option<EvalReport>,
    /// The result of the last command, if it was `SaveAutonomyThresholds`. A failed save is shown here, not in `error`.
    pub thresholds_result: Option<Result<(), SaveError>>,
    /// How many `SaveAutonomyThresholds` commands the bridge has run.
    pub threshold_saves: u64,
    /// The findings of the last check of the notes that the owner has not dismissed.
    pub lint_findings: Option<Vec<LintFinding>>,
    /// How many findings the last `LintNotes` stored, or why it failed. Not an `error`.
    pub lint_result: Option<Result<usize, String>>,
    /// How many page changes the last `PreloadNotes` put into the inbox, or why it failed. Not an `error`.
    pub preload_result: Option<Result<usize, String>>,
    /// The caller tokens of the selected profile, revoked ones too.
    pub callers: Option<Vec<CallerToken>>,
    /// The token that the last command created. The server cannot show it again.
    pub new_caller: Option<NewCallerToken>,
    /// Where the inbound endpoint and the webhooks are served, and why the ARD catalogs leave the
    /// listing out. It never holds a signing secret, only whether one is stored.
    pub inbound_status: Option<InboundStatus>,
    /// The settings of the inbound endpoint and its ARD listing, as the server has them.
    pub inbound: Option<InboundConfig>,
    /// The ARD registries, as the server has them.
    pub ard_registries: Option<Vec<ArdRegistry>>,
    /// The webhooks by name, as the server has them.
    pub webhooks: Option<BTreeMap<String, WebhookConfig>>,
    /// The settings of the notes, as the server has them.
    pub notes_settings: Option<NotesConfig>,
    /// The evaluation settings, as the server has them.
    pub eval_settings: Option<EvalConfig>,
    /// The part and the result of the last command, if it saved one part of the settings.
    /// A failed save is shown in its card, not in `error`.
    pub part_result: Option<(Part, Result<(), SaveError>)>,
    /// How many commands that save a part the bridge has run.
    pub part_saves: u64,
    /// The failure of the last command, if it failed.
    pub error: Option<String>,
    /// False until the engine answers, and again when it stops answering.
    /// The window then shows no engine state, because that state may be out of date.
    pub connected: bool,
}

/// Refuses a command for `profile` if that profile is not the selected one.
fn check_selected(selected: Option<&str>, profile: &str) -> Result<(), String> {
    match selected {
        None => Err("no profile is selected".to_owned()),
        Some(selected) if selected != profile => Err("another profile is selected now, so nothing was done".to_owned()),
        Some(_) => Ok(()),
    }
}

/// Runs `call` if `profile` is still the selected profile. The call does nothing until it is awaited.
async fn for_profile<T>(
    selected: Option<&str>,
    profile: &str,
    call: impl Future<Output = ApiResult<T>>,
) -> Result<T, String> {
    check_selected(selected, profile)?;
    call.await.map_err(|e| e.to_string())
}

/// Like `for_profile`, for a save. A conflict is kept apart from the other failures.
async fn save_for(
    selected: Option<&str>,
    profile: &str,
    call: impl Future<Output = ApiResult<()>>,
) -> Result<(), SaveError> {
    check_selected(selected, profile).map_err(SaveError::Failed)?;
    call.await.map_err(|error| match error {
        ApiError::Conflict(_) => SaveError::Conflict,
        error => SaveError::Failed(error.to_string()),
    })
}

/// The result of an approval. A call that ran and failed, or got no answer, is a failure too.
fn approved(result: Result<Outcome, String>) -> Result<(), String> {
    match result? {
        Outcome::ExecutionFailed { error, .. } => Err(format!("the call failed: {error}")),
        Outcome::ExecutionUnknown { error, .. } => Err(format!("the outcome of the call is unknown: {error}")),
        _ => Ok(()),
    }
}

/// Runs one command, then reloads the state from the engine.
pub async fn apply(api: &dyn EngineApi, state: &mut UiState, command: UiCommand) {
    state.error = None;
    state.privacy_result = None;
    state.permissions_result = None;
    state.principles_result = None;
    state.poll_result = None;
    state.connection_result = None;
    state.note_result = None;
    state.extract_result = None;
    state.thresholds_result = None;
    state.lint_result = None;
    state.preload_result = None;
    state.cases_result = None;
    state.new_caller = None;
    state.part_result = None;
    let selected = state.selected.clone();
    let selected = selected.as_deref();
    let result = match (command, selected) {
        (UiCommand::Refresh, _) => Ok(()),
        (UiCommand::SelectProfile(id), _) => {
            state.selected = Some(id);
            Ok(())
        }
        (UiCommand::SavePermissions { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_permissions(&profile, update)).await;
            state.permissions_result = Some(result);
            state.permission_saves += 1;
            Ok(())
        }
        (UiCommand::SavePrinciples { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_principles(&profile, update)).await;
            state.principles_result = Some(result);
            state.principle_saves += 1;
            Ok(())
        }
        (UiCommand::SavePolls { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_polls(&profile, update)).await;
            state.poll_result = Some(PollResult::Saved(result));
            Ok(())
        }
        (UiCommand::TestPoll { profile, index }, _) => {
            let result = for_profile(selected, &profile, api.test_poll(&profile, index)).await;
            state.poll_result = Some(PollResult::Tested { index, result });
            Ok(())
        }
        (UiCommand::SavePrivacy { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_privacy(&profile, update)).await;
            state.privacy_result = Some(result);
            state.privacy_saves += 1;
            Ok(())
        }
        (UiCommand::SaveLlm { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_llm(&profile, update)).await;
            state.connection_result = Some(ConnectionResult::LlmSaved(result));
            Ok(())
        }
        (UiCommand::SaveConnector { profile, name, update }, _) => {
            let result = save_for(selected, &profile, api.set_connector(&profile, &name, update)).await;
            state.connection_result = Some(ConnectionResult::ConnectorSaved { name, result });
            Ok(())
        }
        (UiCommand::SaveCommandConnector { profile, name, update }, _) => {
            let result = save_for(selected, &profile, api.set_command_connector(&profile, &name, update)).await;
            state.connection_result = Some(ConnectionResult::ConnectorSaved { name, result });
            Ok(())
        }
        (UiCommand::SaveApiConnector { profile, name, update }, _) => {
            let result = save_for(selected, &profile, api.set_api_connector(&profile, &name, update)).await;
            state.connection_result = Some(ConnectionResult::ConnectorSaved { name, result });
            Ok(())
        }
        (UiCommand::RemoveConnector { profile, name }, _) => {
            let result = for_profile(selected, &profile, api.remove_connector(&profile, &name)).await;
            state.connection_result = Some(ConnectionResult::ConnectorRemoved { name, result });
            Ok(())
        }
        (UiCommand::TestLlm { profile }, _) => {
            let result = for_profile(selected, &profile, api.test_llm(&profile)).await;
            state.connection_result = Some(ConnectionResult::LlmTested(result));
            Ok(())
        }
        (UiCommand::CheckConnector { profile, name }, _) => {
            let result = for_profile(selected, &profile, api.connector_tools(&profile, &name)).await;
            state.connection_result = Some(ConnectionResult::Checked { name, result });
            Ok(())
        }
        (UiCommand::ApproveTools { profile, name, shown }, _) => {
            let result = for_profile(selected, &profile, api.approve_tools(&profile, &name, shown)).await;
            state.connection_result = Some(ConnectionResult::ToolsApproved { name, result });
            Ok(())
        }
        (
            UiCommand::StartSignIn {
                profile,
                name,
                redirect_uri,
            },
            _,
        ) => {
            let start = api.start_connector_oauth(&profile, &name, &redirect_uri);
            let result = for_profile(selected, &profile, start).await;
            let result = result.map(|started| started.authorization_url);
            state.connection_result = Some(ConnectionResult::SignInStarted { name, result });
            Ok(())
        }
        (
            UiCommand::FinishSignIn {
                profile,
                name,
                callback,
            },
            _,
        ) => {
            let result = match callback {
                Ok(callback) => {
                    let finish = api.finish_connector_oauth(&profile, &name, callback);
                    for_profile(selected, &profile, finish).await
                }
                Err(error) => Err(error),
            };
            state.connection_result = Some(ConnectionResult::SignedIn { name, result });
            Ok(())
        }
        // The loop of `spawn_bridge` turns `SignIn` into `StartSignIn`. A direct call has no listener.
        (UiCommand::SignIn { name, .. }, _) => {
            let result = Err("this window cannot receive a sign-in".to_owned());
            state.connection_result = Some(ConnectionResult::SignInStarted { name, result });
            Ok(())
        }
        (UiCommand::OpenNote { profile, slug }, _) => check_selected(selected, &profile).map(|()| {
            state.open_note = Some(OpenNote {
                profile,
                slug,
                page: None,
                history: None,
            });
        }),
        (UiCommand::NoteHistory { profile, show }, _) => check_selected(selected, &profile).map(|()| {
            if let Some(open) = &mut state.open_note {
                // The reload fills the list.
                open.history = show.then(Vec::new);
            }
        }),
        (UiCommand::CloseNote, _) => {
            state.open_note = None;
            Ok(())
        }
        (UiCommand::SearchNotes { profile, query }, _) => check_selected(selected, &profile).map(|()| {
            // The reload fills the result.
            state.note_search = Some(NoteSearch {
                profile,
                query,
                found: None,
            });
        }),
        (UiCommand::ClearNoteSearch, _) => {
            state.note_search = None;
            Ok(())
        }
        (UiCommand::SaveNote { profile, slug, update }, _) => {
            let call = async { api.set_wiki_page(&profile, &slug, update).await.map(|_| ()) };
            let result = save_for(selected, &profile, call).await;
            if result.is_ok() {
                // A history that was shown for this page stays shown, with the new revision.
                let history = state
                    .open_note
                    .as_ref()
                    .filter(|open| open.slug == slug)
                    .and_then(|open| open.history.as_ref())
                    .map(|_| Vec::new());
                state.open_note = Some(OpenNote {
                    profile,
                    slug: slug.clone(),
                    page: None,
                    history,
                });
            }
            state.note_result = Some(NoteResult::Saved { slug, result });
            Ok(())
        }
        (UiCommand::DeleteNote { profile, slug }, _) => {
            let result = for_profile(selected, &profile, api.delete_wiki_page(&profile, &slug)).await;
            if result.is_ok() {
                state.open_note = None;
            }
            state.note_result = Some(NoteResult::Deleted { slug, result });
            Ok(())
        }
        (UiCommand::SaveAutonomyThresholds { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_autonomy_thresholds(&profile, update)).await;
            state.thresholds_result = Some(result);
            state.threshold_saves += 1;
            Ok(())
        }
        (UiCommand::ExtractPrinciples { profile, source }, _) => {
            let result = for_profile(selected, &profile, api.extract_principles(&profile, source)).await;
            state.extract_result = Some(result.map(|found| found.len()));
            Ok(())
        }
        (UiCommand::LintNotes { profile }, _) => {
            let result = for_profile(selected, &profile, api.lint_notes(&profile)).await;
            state.lint_result = Some(result.map(|found| found.len()));
            Ok(())
        }
        (UiCommand::PreloadNotes { profile, request }, _) => {
            let result = for_profile(selected, &profile, api.preload_notes(&profile, request)).await;
            state.preload_result = Some(result.map(|filed| filed.len()));
            Ok(())
        }
        (UiCommand::AddReplayCases { profile, cases }, _) => {
            let result = for_profile(selected, &profile, api.add_replay_cases(&profile, cases)).await;
            state.cases_result = Some(result.map(|ids| ids.len()));
            state.case_adds += 1;
            Ok(())
        }
        (UiCommand::AddCaller { profile, label }, _) => {
            for_profile(selected, &profile, api.add_caller_token(&profile, &label))
                .await
                .map(|created| state.new_caller = Some(created))
        }
        (UiCommand::RevokeCaller { profile, id }, _) => {
            for_profile(selected, &profile, api.revoke_caller_token(&profile, id)).await
        }
        (UiCommand::SaveInbound { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_inbound_settings(&profile, *update)).await;
            state.part_result = Some((Part::Inbound, result));
            Ok(())
        }
        (UiCommand::SaveArdRegistries { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_ard_registries(&profile, update)).await;
            state.part_result = Some((Part::ArdRegistries, result));
            Ok(())
        }
        (UiCommand::SaveWebhooks { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_webhooks(&profile, update)).await;
            state.part_result = Some((Part::Webhooks, result));
            Ok(())
        }
        (UiCommand::SetWebhookSecret { profile, name, secret }, _) => {
            let call = api.set_webhook_secret(&profile, &name, secret.0);
            state.part_result = Some((Part::WebhookSecret, save_for(selected, &profile, call).await));
            Ok(())
        }
        (UiCommand::SaveNotesSettings { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_notes_settings(&profile, update)).await;
            state.part_result = Some((Part::Notes, result));
            Ok(())
        }
        (UiCommand::SaveEvalSettings { profile, update }, _) => {
            let result = save_for(selected, &profile, api.set_eval_settings(&profile, update)).await;
            state.part_result = Some((Part::Eval, result));
            Ok(())
        }
        (UiCommand::DismissFinding { profile, id }, _) => {
            for_profile(selected, &profile, api.dismiss_finding(&profile, id)).await
        }
        (UiCommand::RetireCorrection { profile, id }, _) => {
            for_profile(selected, &profile, api.retire_correction(&profile, id)).await
        }
        (UiCommand::AdoptPrinciple { profile, adoption }, _) => {
            for_profile(selected, &profile, api.adopt_principle(&profile, adoption)).await
        }
        (UiCommand::DismissPrinciple { profile, id }, _) => {
            for_profile(selected, &profile, api.dismiss_principle(&profile, id)).await
        }
        (UiCommand::DeleteReplayCase { profile, id }, _) => {
            for_profile(selected, &profile, api.delete_replay_case(&profile, id)).await
        }
        (UiCommand::StartEval { profile }, _) => for_profile(selected, &profile, api.start_eval(&profile))
            .await
            .map(|_| ()),
        (UiCommand::ClearAutonomyHold { profile, tool }, _) => {
            for_profile(selected, &profile, api.clear_autonomy_hold(&profile, &tool)).await
        }
        (UiCommand::ApproveEdited { profile, id, edit }, _) => {
            approved(for_profile(selected, &profile, api.approve(&profile, id, Some(edit))).await)
        }
        (_, None) => Err("no profile is selected".to_owned()),
        (UiCommand::Approve(id), Some(profile)) => {
            approved(api.approve(profile, id, None).await.map_err(|e| e.to_string()))
        }
        (UiCommand::Reject(id), Some(profile)) => api
            .reject(profile, id, "dismissed in the app")
            .await
            .map_err(|e| e.to_string()),
        (UiCommand::ResolveUnknown(id, resolution), Some(profile)) => {
            match api.resolve_unknown(profile, id, resolution).await {
                Ok(Some(outcome)) => approved(Ok(outcome)),
                Ok(None) => Ok(()),
                Err(error) => Err(error.to_string()),
            }
        }
        (UiCommand::ReportAutonomous(id), Some(profile)) => {
            api.report_autonomous(profile, id).await.map_err(|e| e.to_string())
        }
        (UiCommand::SetKillSwitch(on), Some(profile)) => api
            .set_kill_switch(profile, on)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string()),
    };
    // Every connection command sets a result, so this counts them.
    if state.connection_result.is_some() {
        state.connection_commands += 1;
    }
    if state.note_result.is_some() {
        state.note_commands += 1;
    }
    if state.poll_result.is_some() {
        state.poll_commands += 1;
    }
    if state.part_result.is_some() {
        state.part_saves += 1;
    }
    if let Err(error) = result {
        state.error = Some(error);
    }
    reload(api, state).await;
}

/// What the window shows of one profile.
struct Loaded {
    snapshot: ProfileSnapshot,
    privacy: PrivacyConfig,
    permissions: Permissions,
    autonomy: Autonomy,
    principles: Vec<String>,
    polls: Vec<PollConfig>,
    connections: Connections,
    notes: Vec<WikiSummary>,
    corrections: Vec<CorrectionRecord>,
    candidates: Vec<PrincipleCandidate>,
    replay_cases: Vec<ReplayCaseRecord>,
    eval_report: EvalReport,
    lint_findings: Vec<LintFinding>,
    callers: Vec<CallerToken>,
    inbound_status: InboundStatus,
    inbound: InboundConfig,
    ard_registries: Vec<ArdRegistry>,
    webhooks: BTreeMap<String, WebhookConfig>,
    notes_settings: NotesConfig,
    eval_settings: EvalConfig,
    /// The open page. None if no page is open or the server has no such page.
    page: Option<WikiPage>,
    /// The revisions of the open page, if the window shows them.
    history: Option<Vec<Option<WikiPage>>>,
    /// The result of the search, if one is active.
    found: Option<Result<Vec<WikiSummary>, String>>,
}

/// `open` is the open page and whether its history is shown. `query` is the search of the notes.
async fn load_profile(
    api: &dyn EngineApi,
    profile: &str,
    open: Option<(&str, bool)>,
    query: Option<&str>,
) -> ApiResult<Loaded> {
    Ok(Loaded {
        snapshot: api.snapshot(profile).await?,
        privacy: api.privacy(profile).await?,
        permissions: api.permissions(profile).await?,
        autonomy: api.autonomy(profile).await?,
        principles: api.principles(profile).await?,
        polls: api.polls(profile).await?,
        connections: api.connections(profile).await?,
        notes: api.wiki_index(profile).await?,
        corrections: api.corrections(profile).await?,
        candidates: api.principle_candidates(profile).await?,
        replay_cases: api.replay_cases(profile).await?,
        eval_report: api.eval_report(profile).await?,
        lint_findings: api.lint_findings(profile).await?,
        callers: api.caller_tokens(profile).await?,
        inbound_status: api.inbound_status(profile).await?,
        inbound: api.inbound_settings(profile).await?,
        ard_registries: api.ard_registries(profile).await?,
        webhooks: api.webhooks(profile).await?,
        notes_settings: api.notes_settings(profile).await?,
        eval_settings: api.eval_settings(profile).await?,
        history: match open {
            Some((slug, true)) => Some(api.wiki_history(profile, slug).await?),
            _ => None,
        },
        found: match query {
            None => None,
            // A refused query is not a failure of the connection.
            Some(query) => match api.wiki_search(profile, query).await {
                Ok(pages) => Some(Ok(pages)),
                Err(ApiError::Invalid(reason)) => Some(Err(reason)),
                Err(error) => return Err(error),
            },
        },
        page: match open {
            None => None,
            // A page that is gone is not a failure of the connection.
            Some((slug, _)) => match api.wiki_page(profile, slug).await {
                Ok(page) => Some(page),
                Err(ApiError::NotFound(_)) => None,
                Err(error) => return Err(error),
            },
        },
    })
}

async fn reload(api: &dyn EngineApi, state: &mut UiState) {
    state.snapshot = None;
    state.privacy = None;
    state.permissions = None;
    state.autonomy = None;
    state.principles = None;
    state.polls = None;
    state.connections = None;
    state.notes = None;
    state.corrections = None;
    state.candidates = None;
    state.replay_cases = None;
    state.eval_report = None;
    state.lint_findings = None;
    state.callers = None;
    state.inbound_status = None;
    state.inbound = None;
    state.ard_registries = None;
    state.webhooks = None;
    state.notes_settings = None;
    state.eval_settings = None;
    state.connected = false;
    match api.list_profiles().await {
        Ok(profiles) => state.profiles = profiles,
        Err(error) => {
            state.profiles.clear();
            state.error = Some(error.to_string());
            return;
        }
    }
    // Keep the selection if that profile still exists. Otherwise select the first one.
    let known = |id: &String| state.profiles.iter().any(|p| &p.id == id);
    if !state.selected.as_ref().is_some_and(known) {
        state.selected = state.profiles.first().map(|p| p.id.clone());
    }
    // A page stays open, and a search active, only while its profile is the selected one.
    if state.open_note.as_ref().map(|open| &open.profile) != state.selected.as_ref() {
        state.open_note = None;
    }
    if state.note_search.as_ref().map(|search| &search.profile) != state.selected.as_ref() {
        state.note_search = None;
    }
    let query = state.note_search.as_ref().map(|search| search.query.clone());
    let open = state
        .open_note
        .as_ref()
        .map(|open| (open.slug.clone(), open.history.is_some()));
    let open = open.as_ref().map(|(slug, history)| (slug.as_str(), *history));
    match &state.selected {
        // The window is connected only if every call succeeds.
        Some(profile) => match load_profile(api, profile, open, query.as_deref()).await {
            Ok(loaded) => {
                state.snapshot = Some(loaded.snapshot);
                state.privacy = Some(loaded.privacy);
                state.permissions = Some(loaded.permissions);
                state.autonomy = Some(loaded.autonomy);
                state.principles = Some(loaded.principles);
                state.polls = Some(loaded.polls);
                state.connections = Some(loaded.connections);
                state.notes = Some(loaded.notes);
                state.corrections = Some(loaded.corrections);
                state.candidates = Some(loaded.candidates);
                state.replay_cases = Some(loaded.replay_cases);
                state.eval_report = Some(loaded.eval_report);
                state.lint_findings = Some(loaded.lint_findings);
                state.callers = Some(loaded.callers);
                state.inbound_status = Some(loaded.inbound_status);
                state.inbound = Some(loaded.inbound);
                state.ard_registries = Some(loaded.ard_registries);
                state.webhooks = Some(loaded.webhooks);
                state.notes_settings = Some(loaded.notes_settings);
                state.eval_settings = Some(loaded.eval_settings);
                if let Some(open) = &mut state.open_note {
                    open.page = loaded.page;
                    open.history = loaded.history;
                }
                if let Some(search) = &mut state.note_search {
                    search.found = loaded.found;
                }
                state.connected = true;
            }
            Err(error) => state.error = Some(error.to_string()),
        },
        None => state.connected = true,
    }
}

/// Reloads the state without a command, to show what happened on the server meanwhile.
/// The failure of the last command stays on screen. A connection failure does not,
/// once the server answers again.
pub async fn refresh(api: &dyn EngineApi, state: &mut UiState) {
    let command_error = if state.connected { state.error.take() } else { None };
    state.error = None;
    reload(api, state).await;
    if state.error.is_none() {
        state.error = command_error;
    }
}

/// How often the window reloads by itself.
pub const REFRESH_EVERY: std::time::Duration = std::time::Duration::from_secs(5);

/// Opens the listener for the browser and starts the sign-in. A task waits for the browser
/// and sends `FinishSignIn`.
async fn sign_in(
    api: &dyn EngineApi,
    state: &mut UiState,
    profile: String,
    name: String,
    commands: &mpsc::WeakUnboundedSender<UiCommand>,
) {
    let (listener, redirect_uri) = match crate::sign_in::listen().await {
        Ok(listening) => listening,
        Err(error) => {
            let command = UiCommand::FinishSignIn {
                profile,
                name,
                callback: Err(error),
            };
            return apply(api, state, command).await;
        }
    };
    let start = UiCommand::StartSignIn {
        profile: profile.clone(),
        name: name.clone(),
        redirect_uri: redirect_uri.clone(),
    };
    apply(api, state, start).await;
    if !matches!(
        &state.connection_result,
        Some(ConnectionResult::SignInStarted { result: Ok(_), .. })
    ) {
        return;
    }
    let commands = commands.clone();
    tokio::spawn(async move {
        let callback = crate::sign_in::wait(listener, &redirect_uri).await;
        if let Some(commands) = commands.upgrade() {
            let _ = commands.send(UiCommand::FinishSignIn {
                profile,
                name,
                callback,
            });
        }
    });
}

/// Starts the task that serves the window. It reloads the state after each command and
/// every `refresh_every`. It ends when either channel closes.
pub fn spawn_bridge(
    runtime: &tokio::runtime::Handle,
    api: Arc<dyn EngineApi>,
    refresh_every: std::time::Duration,
) -> (mpsc::UnboundedSender<UiCommand>, mpsc::UnboundedReceiver<UiState>) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    // A sign-in sends its end back as a command. A weak sender lets the loop end with the window.
    let sign_ins = command_tx.downgrade();
    runtime.spawn(async move {
        let mut state = UiState::default();
        // The timer runs on tokio. A tokio timer must not be polled on the GPUI executor.
        let mut timer = tokio::time::interval(refresh_every);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let before = state.clone();
            tokio::select! {
                command = command_rx.recv() => match command {
                    Some(UiCommand::SignIn { profile, name }) => {
                        sign_in(&*api, &mut state, profile, name, &sign_ins).await;
                    }
                    Some(command) => apply(&*api, &mut state, command).await,
                    // The window closed or disconnected.
                    None => break,
                },
                _ = timer.tick() => {
                    refresh(&*api, &mut state).await;
                    // An unchanged state is not sent, so the window does not redraw for nothing.
                    if state == before {
                        continue;
                    }
                }
            }
            if state_tx.send(state.clone()).is_err() {
                break;
            }
        }
    });
    (command_tx, state_rx)
}
