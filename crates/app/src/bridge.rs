//! Runs engine calls on tokio and hands the resulting state to the window.
//!
//! The window never changes its own copy of engine state. It sends a command
//! and shows the state that comes back.

use bunsin_api::{EngineApi, Outcome, ProfileSnapshot};
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
}

/// What the window shows. Every field comes from the engine.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UiState {
    pub profiles: Vec<ProfileInfo>,
    pub selected: Option<String>,
    pub snapshot: Option<ProfileSnapshot>,
    /// The failure of the last command, if it failed.
    pub error: Option<String>,
    /// False until the engine answers, and again when it stops answering.
    /// The window then shows no engine state, because that state may be out of date.
    pub connected: bool,
}

/// Runs one command, then reloads the state from the engine.
pub async fn apply(api: &dyn EngineApi, state: &mut UiState, command: UiCommand) {
    state.error = None;
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
    };
    if let Err(error) = result {
        state.error = Some(error);
    }
    reload(api, state).await;
}

async fn reload(api: &dyn EngineApi, state: &mut UiState) {
    state.snapshot = None;
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
        Some(profile) => match api.snapshot(profile).await {
            Ok(snapshot) => {
                state.snapshot = Some(snapshot);
                state.connected = true;
            }
            Err(error) => state.error = Some(error.to_string()),
        },
        None => state.connected = true,
    }
}

/// Starts the task that serves the window. It ends when either channel closes.
pub fn spawn_bridge(
    runtime: &tokio::runtime::Handle,
    api: Arc<dyn EngineApi>,
) -> (
    mpsc::UnboundedSender<UiCommand>,
    mpsc::UnboundedReceiver<UiState>,
) {
    let (command_tx, mut command_rx) = mpsc::unbounded_channel();
    let (state_tx, state_rx) = mpsc::unbounded_channel();
    runtime.spawn(async move {
        let mut state = UiState::default();
        while let Some(command) = command_rx.recv().await {
            apply(&*api, &mut state, command).await;
            if state_tx.send(state.clone()).is_err() {
                break;
            }
        }
    });
    (command_tx, state_rx)
}
