mod common;

use bunsin_app::bridge::{UiCommand, UiState, apply, refresh, spawn_bridge};
use bunsin_core::approval::ApprovalState;

#[tokio::test]
async fn refresh_selects_the_first_profile_and_loads_it() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.selected.as_deref(), Some("kim"));
    assert_eq!(state.profiles.len(), 2);
    let snapshot = state.snapshot.unwrap();
    assert_eq!(snapshot.inbox.len(), 1);
    assert_eq!(snapshot.inbox[0].state, ApprovalState::Pending);
    assert_eq!(state.error, None);
}

#[tokio::test]
async fn selecting_another_profile_shows_only_its_data() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    assert_eq!(state.selected.as_deref(), Some("lee"));
    assert!(state.snapshot.unwrap().inbox.is_empty());
}

#[tokio::test]
async fn approve_runs_the_call_and_a_second_approve_reports_an_error() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, UiCommand::Approve(1)).await;
    assert_eq!(state.error, None);
    assert_eq!(state.snapshot.as_ref().unwrap().inbox[0].state, ApprovalState::Executed);

    apply(&*rig.engine, &mut state, UiCommand::Approve(1)).await;
    assert!(
        state.error.as_deref().is_some_and(|e| e.contains("not pending")),
        "{:?}",
        state.error
    );
    assert_eq!(rig.slack.calls().len(), 1);
}

#[tokio::test]
async fn a_failed_call_is_shown_as_an_error() {
    let rig = common::rig().await;
    rig.slack.fail_with("channel_not_found");
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, UiCommand::Approve(1)).await;
    assert!(
        state.error.as_deref().is_some_and(|e| e.contains("channel_not_found")),
        "{:?}",
        state.error
    );
    assert_eq!(state.snapshot.unwrap().inbox[0].state, ApprovalState::Failed);
}

#[tokio::test]
async fn the_kill_switch_state_comes_from_the_engine() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, UiCommand::SetKillSwitch(true)).await;
    assert!(state.snapshot.as_ref().unwrap().kill_switch);

    // With the switch on, approval is refused and the item stays pending.
    apply(&*rig.engine, &mut state, UiCommand::Approve(1)).await;
    assert!(state.error.is_some());
    assert_eq!(state.snapshot.as_ref().unwrap().inbox[0].state, ApprovalState::Pending);
    assert!(rig.slack.calls().is_empty());

    // The other profile has its own switch.
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    assert!(!state.snapshot.unwrap().kill_switch);
}

#[tokio::test]
async fn dismiss_rejects_the_item() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, UiCommand::Reject(1)).await;
    assert_eq!(state.error, None);
    assert_eq!(state.snapshot.unwrap().inbox[0].state, ApprovalState::Rejected);
    assert!(rig.slack.calls().is_empty());
}

#[tokio::test]
async fn a_command_without_any_profile_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    struct NoBackends;
    impl bunsin_engine::Backends for NoBackends {
        fn build<'a>(
            &'a self,
            _: &'a str,
            _: &'a bunsin_api::ProfileConfig,
        ) -> bunsin_core::ports::BoxFuture<'a, Result<bunsin_engine::Runtime, String>> {
            Box::pin(async { Err("unused".to_owned()) })
        }
    }
    let engine =
        bunsin_engine::Engine::open(dir.path(), std::sync::Arc::new(NoBackends), std::sync::Arc::new(|| 1)).unwrap();
    let mut state = UiState::default();
    apply(&engine, &mut state, UiCommand::SetKillSwitch(true)).await;
    assert_eq!(state.error.as_deref(), Some("no profile is selected"));
    assert!(state.profiles.is_empty());
    assert_eq!(state.snapshot, None);
}

#[tokio::test]
async fn a_server_that_stops_answering_clears_the_shown_state() {
    use bunsin_client::{RemoteConfig, RemoteEngine};
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert!(state.connected);
    assert!(state.snapshot.is_some());

    // Nothing listens on this port, like a server that went down.
    let dead = RemoteConfig {
        url: "http://127.0.0.1:1".into(),
        ca_cert_pem: None,
    };
    let dead = RemoteEngine::new(&dead, "bsn_test").unwrap();
    apply(&dead, &mut state, UiCommand::Approve(1)).await;
    assert!(!state.connected);
    // No stale "Running" state stays on screen.
    assert_eq!(state.snapshot, None);
    assert!(state.profiles.is_empty());
    assert!(
        state.error.as_deref().is_some_and(|e| e.contains("cannot be reached")),
        "{:?}",
        state.error
    );
    assert!(rig.slack.calls().is_empty());

    // When the server answers again, the state comes back.
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert!(state.connected);
    assert_eq!(state.snapshot.unwrap().inbox.len(), 1);
}

#[tokio::test]
async fn a_timed_refresh_keeps_the_last_command_error_but_not_a_connection_error() {
    use bunsin_client::{RemoteConfig, RemoteEngine};
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, UiCommand::Approve(99)).await;
    let command_error = state.error.clone();
    assert!(command_error.is_some());

    // The failure of the command stays on screen across a refresh.
    refresh(&*rig.engine, &mut state).await;
    assert_eq!(state.error, command_error);

    // A connection failure replaces it, and goes away when the server answers again.
    let dead = RemoteConfig {
        url: "http://127.0.0.1:1".into(),
        ca_cert_pem: None,
    };
    let dead = RemoteEngine::new(&dead, "bsn_test").unwrap();
    refresh(&dead, &mut state).await;
    assert!(!state.connected);
    assert!(state.error.as_deref().is_some_and(|e| e.contains("cannot be reached")));
    refresh(&*rig.engine, &mut state).await;
    assert!(state.connected);
    assert_eq!(state.error, None);
}

#[tokio::test]
async fn the_bridge_task_ends_when_the_window_drops_its_command_sender() {
    let rig = common::rig().await;
    let handle = tokio::runtime::Handle::current();
    let (commands, mut states) = spawn_bridge(&handle, rig.engine.clone(), std::time::Duration::from_millis(20));
    commands.send(UiCommand::Refresh).unwrap();
    assert!(states.recv().await.is_some());
    drop(commands);
    // The state channel closes only when the task has ended and dropped its sender.
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while states.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "the bridge task kept running");
}
