mod common;

use bunsin_api::{Connections, ConnectorUpdate, EngineApi, LlmUpdate, TokenChange, TokenSource};
use bunsin_app::bridge::{ConnectionResult, UiCommand, UiState, apply, refresh, spawn_bridge};
use bunsin_core::approval::ApprovalState;
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use bunsin_core::settings::Wire;
use bunsin_core::tool::{PinSet, pin};
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

const TOKEN: &str = "sk-llm-token-7f3a9c";

fn llm(base_url: &str, token: TokenChange) -> LlmUpdate {
    LlmUpdate {
        wire: Wire::OpenAiChat,
        base_url: base_url.into(),
        model: "m1".into(),
        timeout_secs: 120,
        token,
    }
}

fn connector(url: &str, token: TokenChange) -> ConnectorUpdate {
    ConnectorUpdate { url: url.into(), token }
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
            ConnectionResult::LlmSaved(Err(error))
            | ConnectionResult::ConnectorSaved { result: Err(error), .. }
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
