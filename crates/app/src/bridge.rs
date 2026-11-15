//! Runs engine calls on tokio and hands the resulting state to the window.
//!
//! The window never changes its own copy of engine state. It sends a command
//! and shows the state that comes back.

use bunsin_api::{
    ApiError, ApiResult, CommandConnectorUpdate, Connections, ConnectorUpdate, EngineApi, LlmTest, LlmUpdate, Outcome,
    PrivacyUpdate, ProfileSnapshot, ToolsReport, WikiPageUpdate,
};
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::records::ProfileInfo;
use bunsin_core::tool::PinSet;
use bunsin_core::wiki::{WikiPage, WikiSummary};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A command that names a profile is refused if that profile is no longer selected.
#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    Refresh,
    SelectProfile(String),
    Approve(i64),
    Reject(i64),
    SetKillSwitch(bool),
    SavePrivacy {
        profile: String,
        update: PrivacyUpdate,
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
    /// Opens a page of the notes. The page is then reloaded with every refresh.
    OpenNote {
        profile: String,
        slug: String,
    },
    /// Stops loading the open page.
    CloseNote,
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
}

/// The page of the notes that the window has open.
#[derive(Debug, Clone, PartialEq)]
pub struct OpenNote {
    pub profile: String,
    pub slug: String,
    /// None: the server has no page with this name.
    pub page: Option<WikiPage>,
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
}

/// What the window shows. Every field comes from the engine. No token is ever in it.
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
    /// The result of the last command, if it was `SaveNote` or `DeleteNote`. A failure is shown here, not in `error`.
    pub note_result: Option<NoteResult>,
    /// How many `SaveNote` and `DeleteNote` commands the bridge has run.
    pub note_commands: u64,
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

/// Runs one command, then reloads the state from the engine.
pub async fn apply(api: &dyn EngineApi, state: &mut UiState, command: UiCommand) {
    state.error = None;
    state.privacy_result = None;
    state.connection_result = None;
    state.note_result = None;
    let selected = state.selected.clone();
    let selected = selected.as_deref();
    let result = match (command, selected) {
        (UiCommand::Refresh, _) => Ok(()),
        (UiCommand::SelectProfile(id), _) => {
            state.selected = Some(id);
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
        (UiCommand::OpenNote { profile, slug }, _) => check_selected(selected, &profile).map(|()| {
            state.open_note = Some(OpenNote {
                profile,
                slug,
                page: None,
            });
        }),
        (UiCommand::CloseNote, _) => {
            state.open_note = None;
            Ok(())
        }
        (UiCommand::SaveNote { profile, slug, update }, _) => {
            let call = async { api.set_wiki_page(&profile, &slug, update).await.map(|_| ()) };
            let result = save_for(selected, &profile, call).await;
            if result.is_ok() {
                state.open_note = Some(OpenNote {
                    profile,
                    slug: slug.clone(),
                    page: None,
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
        (_, None) => Err("no profile is selected".to_owned()),
        (UiCommand::Approve(id), Some(profile)) => match api.approve(profile, id, None).await {
            Ok(Outcome::ExecutionFailed { error, .. }) => Err(format!("the call failed: {error}")),
            Ok(_) => Ok(()),
            Err(error) => Err(error.to_string()),
        },
        (UiCommand::Reject(id), Some(profile)) => api
            .reject(profile, id, "dismissed in the app")
            .await
            .map_err(|e| e.to_string()),
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
    if let Err(error) = result {
        state.error = Some(error);
    }
    reload(api, state).await;
}

/// What the window shows of one profile.
struct Loaded {
    snapshot: ProfileSnapshot,
    privacy: PrivacyConfig,
    connections: Connections,
    notes: Vec<WikiSummary>,
    /// The open page. None if no page is open or the server has no such page.
    page: Option<WikiPage>,
}

async fn load_profile(api: &dyn EngineApi, profile: &str, open: Option<&str>) -> ApiResult<Loaded> {
    Ok(Loaded {
        snapshot: api.snapshot(profile).await?,
        privacy: api.privacy(profile).await?,
        connections: api.connections(profile).await?,
        notes: api.wiki_index(profile).await?,
        page: match open {
            None => None,
            // A page that is gone is not a failure of the connection.
            Some(slug) => match api.wiki_page(profile, slug).await {
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
    state.connections = None;
    state.notes = None;
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
    // A page stays open only while its profile is the selected one.
    if state.open_note.as_ref().map(|open| &open.profile) != state.selected.as_ref() {
        state.open_note = None;
    }
    let open = state.open_note.as_ref().map(|open| open.slug.clone());
    match &state.selected {
        // The window is connected only if every call succeeds.
        Some(profile) => match load_profile(api, profile, open.as_deref()).await {
            Ok(loaded) => {
                state.snapshot = Some(loaded.snapshot);
                state.privacy = Some(loaded.privacy);
                state.connections = Some(loaded.connections);
                state.notes = Some(loaded.notes);
                if let Some(open) = &mut state.open_note {
                    open.page = loaded.page;
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

/// Starts the task that serves the window. It reloads the state after each command and
/// every `refresh_every`. It ends when either channel closes.
pub fn spawn_bridge(
    runtime: &tokio::runtime::Handle,
    api: Arc<dyn EngineApi>,
    refresh_every: std::time::Duration,
) -> (mpsc::UnboundedSender<UiCommand>, mpsc::UnboundedReceiver<UiState>) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    runtime.spawn(async move {
        let mut state = UiState::default();
        // The timer runs on tokio. A tokio timer must not be polled on the GPUI executor.
        let mut timer = tokio::time::interval(refresh_every);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            let before = state.clone();
            tokio::select! {
                command = command_rx.recv() => match command {
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
