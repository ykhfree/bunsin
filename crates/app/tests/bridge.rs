mod common;

use bunsin_api::{
    Connections, ConnectorUpdate, EngineApi, Expected, LlmUpdate, PrivacyUpdate, TokenChange, TokenSource,
    WikiPageUpdate,
};
use bunsin_app::bridge::{ConnectionResult, NoteResult, SaveError, UiCommand, UiState, apply, refresh, spawn_bridge};
use bunsin_core::approval::ApprovalState;
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use bunsin_core::settings::Wire;
use bunsin_core::tool::{PinSet, pin};
use bunsin_core::wiki::WikiText;
use bunsin_testkit::tool;

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
        update: PrivacyUpdate::unchecked(privacy),
    }
}

/// The text of a failed save. Panics on a success or a conflict.
fn failure(result: &Option<Result<(), SaveError>>) -> String {
    match result {
        Some(Err(SaveError::Failed(error))) => error.clone(),
        other => panic!("expected a failure, got {other:?}"),
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
    rig.engine
        .set_privacy("lee", bunsin_api::PrivacyUpdate::unchecked(lee.clone()))
        .await
        .unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.privacy, Some(PrivacyConfig::default()));
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    assert_eq!(state.privacy, Some(lee));

    // A timed refresh shows a change that another client made.
    let changed = with_pattern("other", "X+");
    rig.engine
        .set_privacy("lee", bunsin_api::PrivacyUpdate::unchecked(changed.clone()))
        .await
        .unwrap();
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
    let error = failure(&state.privacy_result);
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
    let error = failure(&state.privacy_result);
    assert!(error.contains("another profile is selected"), "{error}");
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
    assert_eq!(failure(&state.privacy_result), "no profile is selected");
    assert_eq!(state.privacy_saves, 1);
    assert_eq!(state.error, None);
    assert_eq!(state.privacy, None);
}

const TOKEN: &str = "sk-llm-token-7f3a9c";

fn llm(base_url: &str, token: TokenChange) -> LlmUpdate {
    LlmUpdate {
        wire: Wire::OpenAiChat,
        base_url: base_url.into(),
        model: "m1".into(),
        timeout_secs: 120,
        token,
        expected: bunsin_api::Expected::Unchecked,
    }
}

fn connector(url: &str, token: TokenChange) -> ConnectorUpdate {
    ConnectorUpdate {
        url: url.into(),
        token,
        token_header: None,
        expected: bunsin_api::Expected::Unchecked,
    }
}

/// One of each connection command for `profile`, on connector `slack`.
fn connection_commands(profile: &str) -> Vec<UiCommand> {
    let profile = profile.to_owned();
    let name = "slack".to_owned();
    vec![
        UiCommand::SaveLlm {
            profile: profile.clone(),
            update: llm("https://llm.example", TokenChange::Set(TOKEN.into())),
        },
        UiCommand::SaveConnector {
            profile: profile.clone(),
            name: name.clone(),
            update: connector("https://mcp.example/slack", TokenChange::Keep),
        },
        UiCommand::RemoveConnector {
            profile: profile.clone(),
            name: name.clone(),
        },
        UiCommand::CheckConnector {
            profile: profile.clone(),
            name: name.clone(),
        },
        UiCommand::ApproveTools {
            profile,
            name,
            shown: PinSet::new(),
        },
    ]
}

/// The error of a connection command result.
fn error_of(result: &Option<ConnectionResult>) -> String {
    match result {
        Some(
            ConnectionResult::LlmSaved(Err(SaveError::Failed(error)))
            | ConnectionResult::ConnectorSaved {
                result: Err(SaveError::Failed(error)),
                ..
            }
            | ConnectionResult::ConnectorRemoved { result: Err(error), .. }
            | ConnectionResult::Checked { result: Err(error), .. }
            | ConnectionResult::ToolsApproved { result: Err(error), .. },
        ) => error.clone(),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn no_connections() -> Connections {
    Connections {
        llm: None,
        connectors: Default::default(),
    }
}

#[tokio::test]
async fn the_state_holds_the_connections_of_the_selected_profile() {
    let rig = common::rig().await;
    rig.engine
        .set_llm("lee", llm("https://llm.example/v1", TokenChange::Set(TOKEN.into())))
        .await
        .unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.connections, Some(no_connections()));
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    let lee = state.connections.clone().unwrap().llm.unwrap();
    assert_eq!(
        (lee.base_url.as_str(), lee.token),
        ("https://llm.example/v1", TokenSource::Stored)
    );

    // A timed refresh shows a change that another client made.
    rig.engine
        .set_connector("lee", "jira", connector("https://mcp.example/jira", TokenChange::Keep))
        .await
        .unwrap();
    refresh(&*rig.engine, &mut state).await;
    assert!(state.connections.unwrap().connectors.contains_key("jira"));
}

#[tokio::test]
async fn save_llm_writes_only_the_named_profile_and_keeps_the_token_out_of_the_state() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let save = UiCommand::SaveLlm {
        profile: "kim".into(),
        update: llm("https://llm.example/v1", TokenChange::Set(TOKEN.into())),
    };
    apply(&*rig.engine, &mut state, save).await;
    // The state holds the address, but never the token, in any field.
    let text = format!("{state:?}");
    assert!(text.contains("https://llm.example/v1"), "{text}");
    assert!(!text.contains(TOKEN), "{text}");
    assert_eq!(state.connection_result, Some(ConnectionResult::LlmSaved(Ok(()))));
    assert_eq!(state.connection_commands, 1);
    assert_eq!(state.error, None);
    let shown = state.connections.as_ref().unwrap().llm.clone().unwrap();
    assert_eq!((shown.wire, shown.token), (Wire::OpenAiChat, TokenSource::Stored));
    let kim = rig.engine.connections("kim").await.unwrap().llm.unwrap();
    assert_eq!(kim.token, TokenSource::Stored);
    assert_eq!(rig.engine.connections("lee").await.unwrap(), no_connections());

    // A timed refresh keeps the result. The next command clears it, but not the count.
    refresh(&*rig.engine, &mut state).await;
    assert_eq!(state.connection_result, Some(ConnectionResult::LlmSaved(Ok(()))));
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.connection_result, None);
    assert_eq!(state.connection_commands, 1);
}

#[tokio::test]
async fn refused_connection_saves_show_the_server_error_and_change_nothing() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let save = UiCommand::SaveLlm {
        profile: "kim".into(),
        update: llm("http://llm.example", TokenChange::Set(TOKEN.into())),
    };
    apply(&*rig.engine, &mut state, save).await;
    let error = error_of(&state.connection_result);
    assert!(error.contains("must start with https://"), "{error}");
    // The connections section shows the failed save. The window's error line does not.
    assert_eq!(state.error, None);

    let save = UiCommand::SaveConnector {
        profile: "kim".into(),
        name: "Bad Name".into(),
        update: connector("https://mcp.example/x", TokenChange::Keep),
    };
    apply(&*rig.engine, &mut state, save).await;
    let error = error_of(&state.connection_result);
    assert!(error.contains("the connector name `Bad Name` is invalid"), "{error}");

    let remove = UiCommand::RemoveConnector {
        profile: "kim".into(),
        name: "jira".into(),
    };
    apply(&*rig.engine, &mut state, remove).await;
    assert!(error_of(&state.connection_result).contains("not found"));
    assert_eq!(state.connection_commands, 3);
    assert_eq!(rig.engine.connections("kim").await.unwrap(), no_connections());
    assert_eq!(state.connections, Some(no_connections()));
}

#[tokio::test]
async fn save_connector_and_remove_connector_report_what_they_did() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let save = UiCommand::SaveConnector {
        profile: "kim".into(),
        name: "jira".into(),
        update: connector("https://mcp.example/jira", TokenChange::Set(TOKEN.into())),
    };
    apply(&*rig.engine, &mut state, save).await;
    assert_eq!(
        state.connection_result,
        Some(ConnectionResult::ConnectorSaved {
            name: "jira".into(),
            result: Ok(())
        })
    );
    let jira = rig.engine.connections("kim").await.unwrap().connectors["jira"].clone();
    assert_eq!(jira.token, TokenSource::Stored);
    assert_eq!(state.connections.as_ref().unwrap().connectors["jira"], jira);
    assert!(!format!("{state:?}").contains(TOKEN));

    let remove = UiCommand::RemoveConnector {
        profile: "kim".into(),
        name: "jira".into(),
    };
    apply(&*rig.engine, &mut state, remove).await;
    assert_eq!(
        state.connection_result,
        Some(ConnectionResult::ConnectorRemoved {
            name: "jira".into(),
            result: Ok(())
        })
    );
    assert_eq!(state.connections, Some(no_connections()));
    assert_eq!(rig.engine.connections("kim").await.unwrap(), no_connections());
}

#[tokio::test]
async fn check_lists_the_tools_and_approve_takes_the_shown_list() {
    let rig = common::rig().await;
    for name in ["slack", "jira"] {
        let url = format!("https://mcp.example/{name}");
        rig.engine
            .set_connector("kim", name, connector(&url, TokenChange::Keep))
            .await
            .unwrap();
    }
    rig.slack
        .set_tools(vec![tool("post", "Post a message"), tool("search", "Search")]);
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let check = |name: &str| UiCommand::CheckConnector {
        profile: "kim".into(),
        name: name.into(),
    };

    apply(&*rig.engine, &mut state, check("slack")).await;
    let report = match &state.connection_result {
        Some(ConnectionResult::Checked {
            name,
            result: Ok(report),
        }) if name == "slack" => report.clone(),
        other => panic!("expected a report, got {other:?}"),
    };
    assert_eq!(report.tools.len(), 2);
    assert_eq!(report.diff.added, ["search"]);

    // A list other than the live one is refused.
    let mut stale = pin(&report.tools);
    stale.remove("search");
    let approve = |shown: PinSet| UiCommand::ApproveTools {
        profile: "kim".into(),
        name: "slack".into(),
        shown,
    };
    apply(&*rig.engine, &mut state, approve(stale)).await;
    let error = error_of(&state.connection_result);
    assert!(error.contains("lists other tools now than the ones shown"), "{error}");
    assert!(
        !rig.engine
            .connector_tools("kim", "slack")
            .await
            .unwrap()
            .diff
            .is_clean()
    );

    apply(&*rig.engine, &mut state, approve(pin(&report.tools))).await;
    assert_eq!(
        state.connection_result,
        Some(ConnectionResult::ToolsApproved {
            name: "slack".into(),
            result: Ok(())
        })
    );
    assert!(
        rig.engine
            .connector_tools("kim", "slack")
            .await
            .unwrap()
            .diff
            .is_clean()
    );

    // A connector that cannot connect reports why.
    apply(&*rig.engine, &mut state, check("jira")).await;
    let error = error_of(&state.connection_result);
    assert!(error.contains(common::REFUSED), "{error}");
    assert_eq!(state.error, None);
    assert_eq!(state.connection_commands, 4);
}

#[tokio::test]
async fn connection_commands_for_a_profile_that_is_no_longer_selected_are_refused() {
    let rig = common::rig().await;
    rig.engine
        .set_connector(
            "kim",
            "slack",
            connector("https://mcp.example/slack", TokenChange::Keep),
        )
        .await
        .unwrap();
    let before = rig.engine.profile_config("kim").await.unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    for command in connection_commands("kim") {
        apply(&*rig.engine, &mut state, command.clone()).await;
        let error = error_of(&state.connection_result);
        assert!(error.contains("another profile is selected"), "{command:?}: {error}");
    }
    assert_eq!(state.connection_commands, 5);
    assert_eq!(rig.engine.profile_config("kim").await.unwrap(), before);
    assert_eq!(rig.engine.connections("lee").await.unwrap(), no_connections());
}

#[tokio::test]
async fn connection_commands_without_any_profile_report_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let engine = empty_engine(&dir);
    let mut state = UiState::default();
    for command in connection_commands("kim") {
        apply(&engine, &mut state, command.clone()).await;
        assert_eq!(
            error_of(&state.connection_result),
            "no profile is selected",
            "{command:?}"
        );
        assert_eq!(state.error, None);
    }
    assert_eq!(state.connection_commands, 5);
    assert_eq!(state.connections, None);
}

#[tokio::test]
async fn a_privacy_save_against_a_changed_server_is_a_conflict_not_an_error() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let loaded = state.privacy.clone().unwrap();
    let outside = with_pattern("outside", "OUT");
    rig.engine
        .set_privacy("kim", PrivacyUpdate::unchecked(outside.clone()))
        .await
        .unwrap();

    let mine = with_pattern("mine", "MINE");
    let checked = |expected| UiCommand::SavePrivacy {
        profile: "kim".into(),
        update: PrivacyUpdate {
            privacy: mine.clone(),
            expected,
        },
    };
    apply(&*rig.engine, &mut state, checked(Expected::Value(loaded))).await;
    assert_eq!(state.privacy_result, Some(Err(SaveError::Conflict)));
    assert_eq!(state.error, None);
    assert_eq!(state.privacy_saves, 1);
    assert_eq!(state.privacy, Some(outside.clone()));
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), outside);

    // A save that expects the server's value now goes through.
    apply(&*rig.engine, &mut state, checked(Expected::Value(outside))).await;
    assert_eq!(state.privacy_result, Some(Ok(())));
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), mine);
}

#[tokio::test]
async fn an_llm_save_against_a_changed_server_is_a_conflict_not_an_error() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    // The form loaded "no endpoint". Another client saves one.
    rig.engine
        .set_llm("kim", llm("https://outside.example", TokenChange::Keep))
        .await
        .unwrap();
    let save = UiCommand::SaveLlm {
        profile: "kim".into(),
        update: LlmUpdate {
            expected: Expected::Value(None),
            ..llm("https://mine.example", TokenChange::Set(TOKEN.into()))
        },
    };
    apply(&*rig.engine, &mut state, save).await;
    assert_eq!(
        state.connection_result,
        Some(ConnectionResult::LlmSaved(Err(SaveError::Conflict)))
    );
    assert_eq!(state.error, None);
    let kept = rig.engine.connections("kim").await.unwrap().llm.unwrap();
    assert_eq!(
        (kept.base_url.as_str(), kept.token),
        ("https://outside.example", TokenSource::None)
    );
    assert!(!format!("{state:?}").contains(TOKEN));

    // A failure that is not a conflict stays a failure with its text.
    let save = UiCommand::SaveLlm {
        profile: "kim".into(),
        update: llm("http://plain.example", TokenChange::Keep),
    };
    apply(&*rig.engine, &mut state, save).await;
    assert!(error_of(&state.connection_result).contains("must start with https://"));
}

#[tokio::test]
async fn a_connector_save_against_a_changed_server_is_a_conflict_not_an_error() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    // Two drafts of a new connector `jira`. The other client saves first.
    rig.engine
        .set_connector(
            "kim",
            "jira",
            ConnectorUpdate {
                token_header: None,
                expected: Expected::Value(None),
                ..connector("https://first.example/jira", TokenChange::Keep)
            },
        )
        .await
        .unwrap();
    let save = UiCommand::SaveConnector {
        profile: "kim".into(),
        name: "jira".into(),
        update: ConnectorUpdate {
            token_header: None,
            expected: Expected::Value(None),
            ..connector("https://second.example/jira", TokenChange::Keep)
        },
    };
    apply(&*rig.engine, &mut state, save).await;
    assert_eq!(
        state.connection_result,
        Some(ConnectionResult::ConnectorSaved {
            name: "jira".into(),
            result: Err(SaveError::Conflict),
        })
    );
    assert_eq!(state.error, None);
    let jira = &state.connections.as_ref().unwrap().connectors["jira"];
    assert_eq!(
        jira.mcp,
        bunsin_core::settings::McpConfig::Http {
            url: "https://first.example/jira".into()
        }
    );
}

fn note(body: &str, expected_revision: Option<u64>) -> WikiPageUpdate {
    WikiPageUpdate {
        text: WikiText {
            title: "Team".into(),
            summary: "Who does what".into(),
            body: body.into(),
        },
        expected_revision,
    }
}

fn open_note(profile: &str, slug: &str) -> UiCommand {
    UiCommand::OpenNote {
        profile: profile.into(),
        slug: slug.into(),
    }
}

#[tokio::test]
async fn the_state_holds_the_index_and_the_open_page_of_the_selected_profile() {
    let rig = common::rig().await;
    rig.engine
        .set_wiki_page("kim", "team", note("Kim leads.", None))
        .await
        .unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.notes.as_ref().unwrap()[0].slug, "team");
    assert_eq!(state.open_note, None);

    apply(&*rig.engine, &mut state, open_note("kim", "team")).await;
    let open = state.open_note.clone().unwrap();
    assert_eq!((open.profile.as_str(), open.slug.as_str()), ("kim", "team"));
    assert_eq!(open.page.unwrap().text.body, "Kim leads.");

    // A refresh brings the page as the server has it now.
    rig.engine
        .set_wiki_page("kim", "team", note("Lee leads.", None))
        .await
        .unwrap();
    refresh(&*rig.engine, &mut state).await;
    let page = state.open_note.clone().unwrap().page.unwrap();
    assert_eq!((page.text.body.as_str(), page.revision), ("Lee leads.", 2));

    // The page of one profile is never shown for another.
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("lee".into())).await;
    assert_eq!(state.open_note, None);
    assert_eq!(state.notes, Some(vec![]));
    apply(&*rig.engine, &mut state, UiCommand::SelectProfile("kim".into())).await;
    assert_eq!(state.open_note, None);
}

#[tokio::test]
async fn a_missing_page_is_not_a_lost_connection() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    apply(&*rig.engine, &mut state, open_note("kim", "no-such-page")).await;
    assert!(state.connected);
    assert_eq!(state.error, None);
    assert!(state.snapshot.is_some());
    let open = state.open_note.clone().unwrap();
    assert_eq!((open.slug.as_str(), open.page), ("no-such-page", None));
}

#[tokio::test]
async fn a_notes_command_for_an_unselected_profile_does_nothing() {
    let rig = common::rig().await;
    rig.engine
        .set_wiki_page("lee", "team", note("Lee leads.", None))
        .await
        .unwrap();
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.selected.as_deref(), Some("kim"));

    apply(&*rig.engine, &mut state, open_note("lee", "team")).await;
    assert_eq!(state.open_note, None);
    assert!(state.error.clone().unwrap().contains("another profile"));

    let save = UiCommand::SaveNote {
        profile: "lee".into(),
        slug: "team".into(),
        update: note("Overwritten.", None),
    };
    apply(&*rig.engine, &mut state, save).await;
    assert!(matches!(
        &state.note_result,
        Some(NoteResult::Saved { result: Err(SaveError::Failed(error)), .. }) if error.contains("another profile")
    ));
    let delete = UiCommand::DeleteNote {
        profile: "lee".into(),
        slug: "team".into(),
    };
    apply(&*rig.engine, &mut state, delete).await;
    assert!(matches!(
        &state.note_result,
        Some(NoteResult::Deleted { result: Err(_), .. })
    ));
    assert_eq!(state.note_commands, 2);
    assert_eq!(
        rig.engine.wiki_page("lee", "team").await.unwrap().text.body,
        "Lee leads."
    );
}

#[tokio::test]
async fn a_save_opens_the_page_and_a_conflict_or_a_delete_does_not() {
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let save = |update| UiCommand::SaveNote {
        profile: "kim".into(),
        slug: "team".into(),
        update,
    };
    apply(&*rig.engine, &mut state, save(note("Kim leads.", Some(0)))).await;
    assert!(matches!(
        &state.note_result,
        Some(NoteResult::Saved { result: Ok(()), .. })
    ));
    assert_eq!(state.open_note.clone().unwrap().page.unwrap().revision, 1);
    assert_eq!(state.notes.as_ref().unwrap().len(), 1);

    // "The page must not exist yet" is refused now, and the stored text stays.
    apply(&*rig.engine, &mut state, UiCommand::CloseNote).await;
    apply(&*rig.engine, &mut state, save(note("Second.", Some(0)))).await;
    assert!(matches!(
        &state.note_result,
        Some(NoteResult::Saved {
            result: Err(SaveError::Conflict),
            ..
        })
    ));
    assert_eq!(state.open_note, None);
    assert_eq!(
        rig.engine.wiki_page("kim", "team").await.unwrap().text.body,
        "Kim leads."
    );
    // The result of a notes command does not stay for the next command.
    apply(&*rig.engine, &mut state, open_note("kim", "team")).await;
    assert_eq!(state.note_result, None);
    assert_eq!(state.note_commands, 2);

    let delete = UiCommand::DeleteNote {
        profile: "kim".into(),
        slug: "team".into(),
    };
    apply(&*rig.engine, &mut state, delete).await;
    assert!(matches!(
        &state.note_result,
        Some(NoteResult::Deleted { result: Ok(()), .. })
    ));
    assert_eq!(state.open_note, None);
    assert_eq!(state.notes, Some(vec![]));
}

#[tokio::test]
async fn a_permissions_save_reports_its_own_result_and_reloads_the_rules() {
    use bunsin_api::PermissionsUpdate;
    use bunsin_core::policy::ToolRule;
    let rig = common::rig().await;
    let mut state = UiState::default();
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    let loaded = state.permissions.clone().unwrap();
    assert_eq!(loaded, rig.engine.permissions("kim").await.unwrap());

    let mut rules = loaded.rules.clone();
    rules.insert("slack/search".into(), ToolRule::read());
    let save = |profile: &str, expected| UiCommand::SavePermissions {
        profile: profile.into(),
        update: PermissionsUpdate {
            rules: rules.clone(),
            expected,
        },
    };
    // A save for a profile that is not the selected one reaches no profile.
    apply(&*rig.engine, &mut state, save("lee", Expected::Unchecked)).await;
    assert!(failure(&state.permissions_result).contains("another profile is selected"));
    assert_eq!(state.permission_saves, 1);
    assert_eq!(rig.engine.permissions("lee").await.unwrap().rules, loaded.rules);

    apply(
        &*rig.engine,
        &mut state,
        save("kim", Expected::Value(loaded.rules.clone())),
    )
    .await;
    assert_eq!(state.permissions_result, Some(Ok(())));
    assert_eq!(state.permission_saves, 2);
    assert_eq!(state.permissions.clone().unwrap().rules, rules);
    assert_eq!(rig.engine.permissions("kim").await.unwrap().rules, rules);

    // The same save again started from rules that the server no longer has.
    apply(&*rig.engine, &mut state, save("kim", Expected::Value(loaded.rules))).await;
    assert_eq!(state.permissions_result, Some(Err(SaveError::Conflict)));
    assert_eq!(state.error, None);

    // The result belongs to its command only.
    apply(&*rig.engine, &mut state, UiCommand::Refresh).await;
    assert_eq!(state.permissions_result, None);
    assert_eq!(state.permission_saves, 3);
}
