//! Runs engine calls on tokio and hands the resulting state to the window.
//!
//! The window never changes its own copy of engine state. It sends a command
//! and shows the state that comes back.

use bunsin_api::{ApiResult, EngineApi, Outcome, ProfileSnapshot};
use bunsin_core::privacy::PrivacyConfig;
use bunsin_core::records::ProfileInfo;
use std::sync::Arc;
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum UiCommand {
    Refresh,
    SelectProfile(String),
    Approve(i64),
    Reject(i64),
    SetKillSwitch(bool),
    /// Saves the privacy settings of `profile`. It is refused if `profile` is no longer selected.
    SavePrivacy {
        profile: String,
        privacy: PrivacyConfig,
    },
}

/// What the window shows. Every field comes from the engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiState {
    pub profiles: Vec<ProfileInfo>,
    pub selected: Option<String>,
    pub snapshot: Option<ProfileSnapshot>,
    /// The privacy settings of the selected profile, as the server has them.
    pub privacy: Option<PrivacyConfig>,
    /// The result of the last command, if it was `SavePrivacy`. A failed save is shown here, not in `error`.
    pub privacy_result: Option<Result<(), String>>,
    /// How many `SavePrivacy` commands the bridge has run. It changes once per save.
    pub privacy_saves: u64,
    /// The failure of the last command, if it failed.
    pub error: Option<String>,
    /// False until the engine answers, and again when it stops answering.
    /// The window then shows no engine state, because that state may be out of date.
    pub connected: bool,
}

/// Runs one command, then reloads the state from the engine.
pub async fn apply(api: &dyn EngineApi, state: &mut UiState, command: UiCommand) {
    state.error = None;
    state.privacy_result = None;
    let save = matches!(command, UiCommand::SavePrivacy { .. });
    let profile = state.selected.clone();
    let result = match (command, profile.as_deref()) {
        (UiCommand::Refresh, _) => Ok(()),
        (UiCommand::SelectProfile(id), _) => {
            state.selected = Some(id);
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
        (UiCommand::SavePrivacy { profile, .. }, Some(selected)) if profile != selected => {
            Err("another profile is selected now, so nothing was saved".to_owned())
        }
        (UiCommand::SavePrivacy { profile, privacy }, Some(_)) => {
            api.set_privacy(&profile, privacy).await.map_err(|e| e.to_string())
        }
    };
    if save {
        state.privacy_result = Some(result);
        state.privacy_saves += 1;
    } else if let Err(error) = result {
        state.error = Some(error);
    }
    reload(api, state).await;
}

async fn load_profile(api: &dyn EngineApi, profile: &str) -> ApiResult<(ProfileSnapshot, PrivacyConfig)> {
    Ok((api.snapshot(profile).await?, api.privacy(profile).await?))
}

async fn reload(api: &dyn EngineApi, state: &mut UiState) {
    state.snapshot = None;
    state.privacy = None;
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
    match &state.selected {
        // The window is connected only if both calls succeed.
        Some(profile) => match load_profile(api, profile).await {
            Ok((snapshot, privacy)) => {
                state.snapshot = Some(snapshot);
                state.privacy = Some(privacy);
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
