mod common;

use bunsin_api::EngineApi;
use bunsin_app::bridge::{UiCommand, UiState, apply, refresh, spawn_bridge};
use bunsin_core::approval::ApprovalState;
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};

/// An engine with no profile.
fn empty_engine(dir: &tempfile::TempDir) -> bunsin_engine::Engine {
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
    bunsin_engine::Engine::open(dir.path(), std::sync::Arc::new(NoBackends), std::sync::Arc::new(|| 1)).unwrap()
}

fn with_pattern(label: &str, pattern: &str) -> PrivacyConfig {
    PrivacyConfig {
        phone: false,
        custom: vec![CustomPattern {
            label: label.into(),
            pattern: pattern.into(),
        }],
        ..PrivacyConfig::default()
    }
}

fn save(profile: &str, privacy: PrivacyConfig) -> UiCommand {
    UiCommand::SavePrivacy {
        profile: profile.into(),
        privacy,
    }
}

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
    let engine = empty_engine(&dir);
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
    assert!(state.privacy.is_some());

    // Nothing listens on this port, like a server that went down.
    let dead = RemoteConfig {
        url: "http://127.0.0.1:1".into(),
        ca_cert_pem: None,
    };
    let dead = RemoteEngine::new(&dead, "bsn_test").unwrap();
    apply(&dead, &mut state, UiCommand::Approve(1)).await;
    assert!(!state.connected);
    // No stale "Running" state or privacy settings stay on screen.
    assert_eq!(state.snapshot, None);
    assert_eq!(state.privacy, None);
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

#[tokio::test]
async fn the_state_holds_the_privacy_settings_of_the_selected_profile() {
    let rig = common::rig().await;
    let lee = with_pattern("lee", "LEE-[0-9]+");
    rig.engine.set_privacy("lee", lee.clone()).await.unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.privacy, Some(PrivacyConfig::default()));
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    assert_eq!(state.privacy, Some(lee));

    // A timed refresh shows a change that another client made.
    let changed = with_pattern("other", "X+");
    rig.engine.set_privacy("lee", changed.clone()).await.unwrap();
    refresh(&*rig.engine, &mut state).await;
    assert_eq!(state.privacy, Some(changed));
}

#[tokio::test]
async fn save_privacy_writes_only_the_named_profile_and_reports_success() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let form = with_pattern("project", "PRJ-[0-9]{4}");
    apply(&*rig.engine, &mut state, save("kim", form.clone())).await;
    assert_eq!(state.privacy_result, Some(Ok(())));
    assert_eq!(state.privacy_saves, 1);
    assert_eq!(state.error, None);
    assert_eq!(state.privacy, Some(form.clone()));
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), form);
    assert_eq!(rig.engine.privacy("lee").await.unwrap(), PrivacyConfig::default());

    // A timed refresh keeps the result. The next command clears it, but not the count.
    refresh(&*rig.engine, &mut state).await;
    assert_eq!(state.privacy_result, Some(Ok(())));
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.privacy_result, None);
    assert_eq!(state.privacy_saves, 1);
}

#[tokio::test]
async fn save_privacy_writes_the_selected_profile_even_if_it_is_not_the_first() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    let form = with_pattern("lee", "LEE-[0-9]+");
    apply(&*rig.engine, &mut state, save("lee", form.clone())).await;
    assert_eq!(state.privacy_result, Some(Ok(())));
    assert_eq!(rig.engine.privacy("lee").await.unwrap(), form);
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), PrivacyConfig::default());
}

#[tokio::test]
async fn a_refused_save_shows_the_server_error_and_changes_nothing() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, save("kim", with_pattern("bad", "(unclosed"))).await;
    let error = match &state.privacy_result {
        Some(Err(error)) => error.clone(),
        other => panic!("expected an error, got {other:?}"),
    };
    assert!(
        error.contains("the pattern of `bad` is not a valid regular expression"),
        "{error}"
    );
    // The privacy section shows the failed save. The window's error line does not.
    assert_eq!(state.error, None);
    assert_eq!(state.privacy_saves, 1);
    assert_eq!(state.privacy, Some(PrivacyConfig::default()));
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), PrivacyConfig::default());
}

#[tokio::test]
async fn save_privacy_for_a_profile_that_is_no_longer_selected_is_refused() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    apply(&*rig.engine, &mut state, save("kim", with_pattern("kim", "KIM"))).await;
    assert!(
        matches!(&state.privacy_result, Some(Err(e)) if e.contains("another profile is selected")),
        "{:?}",
        state.privacy_result
    );
    for profile in ["kim", "lee"] {
        assert_eq!(rig.engine.privacy(profile).await.unwrap(), PrivacyConfig::default());
    }
}

#[tokio::test]
async fn save_privacy_without_any_profile_reports_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = empty_engine(&dir);
    let mut state = UiState::default();
    apply(&engine, &mut state, save("kim", PrivacyConfig::default())).await;
    assert_eq!(state.privacy_result, Some(Err("no profile is selected".to_owned())));
    assert_eq!(state.privacy_saves, 1);
    assert_eq!(state.error, None);
    assert_eq!(state.privacy, None);
}
