//! Headless window tests: real view, real engine, scripted LLM and connector.

mod common;

use bunsin_api::{EngineApi, PrivacyUpdate};
use bunsin_app::shell::Shell;
use bunsin_core::event::InboundEvent;
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use common::window::{REFRESH, label, on_screen, open, open_settings, runtime, value, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, App, AppContext, Entity, ScrollDelta, TestAppContext, Window, point, px};

/// Types a label and a pattern into the Add fields. Does not press Add.
fn type_pattern(window: &mut Window, cx: &mut App, label: &str, pattern: &str) {
    window.click("new-label", cx);
    window.input(label, cx);
    window.click("new-pattern", cx);
    window.input(pattern, cx);
}

fn pattern(label: &str, pattern: &str) -> CustomPattern {
    CustomPattern {
        label: label.into(),
        pattern: pattern.into(),
    }
}

/// The four built-in toggles, by ID.
const TOGGLES: [&str; 4] = [
    "privacy-email",
    "privacy-phone",
    "privacy-resident-number",
    "privacy-card-number",
];

#[gpui_kit::test]
fn the_window_shows_engine_state_and_sends_commands(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open(cx, &rt, &rig);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("status").label(), Some("Running"));
        assert_eq!(window.find("profile-kim").label(), Some("● Kim (kim)"));
        assert_eq!(window.find("profile-lee").label(), Some("Lee (lee)"));
        assert_eq!(window.find("inbox-count").label(), Some("Approval inbox: 1"));
        assert_eq!(window.find("inbox-1").label(), Some("pending · slack/post"));
        // The destination is on screen before the user approves, not only the message text.
        let detail = window.find("detail-1").label().unwrap_or_default().to_owned();
        assert!(detail.contains("\"channel\": \"C1\""), "{detail}");
        assert!(detail.contains("We deploy at 3 pm."), "{detail}");
        window.click("approve-1", cx);
    })
    .unwrap();

    wait_until(cx, &shell, "the executed item", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.inbox[0].state.as_str() == "executed")
    });
    assert_eq!(rig.slack.calls().len(), 1);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("inbox-1").label(), Some("executed · slack/post"));
        // An executed item offers no buttons.
        assert!(window.try_find("approve-1").is_none());
        assert!(window.try_find("reject-1").is_none());
        window.click("kill-switch", cx);
    })
    .unwrap();

    wait_until(cx, &shell, "the kill switch", |s| {
        s.state.snapshot.as_ref().is_some_and(|snap| snap.kill_switch)
    });
    // The engine stored the switch. The window did not only change its own label.
    assert!(rt.block_on(rig.engine.snapshot("kim")).unwrap().kill_switch);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("status").label(),
            Some("Stopped: the agent makes no changes")
        );
        window.click("profile-lee", cx);
    })
    .unwrap();

    wait_until(cx, &shell, "the other profile", |s| {
        s.state.selected.as_deref() == Some("lee")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("profile-lee").label(), Some("● Lee (lee)"));
        assert_eq!(window.find("status").label(), Some("Running"));
        assert_eq!(window.find("inbox-count").label(), Some("Approval inbox: 0"));
        assert!(window.try_find("inbox-1").is_none());
    })
    .unwrap();
}

#[gpui_kit::test]
fn the_window_shows_a_new_item_without_a_click(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (_window, shell) = open(cx, &rt, &rig);
    let inbox = |s: &Shell| s.state.snapshot.as_ref().map_or(0, |snapshot| snapshot.inbox.len());
    assert_eq!(shell.read_with(cx, |s, _| inbox(s)), 1);

    // An event reaches the server while the window is only open.
    rt.block_on(common::ingest_second_event(&rig));
    wait_until(cx, &shell, "the new item", |s| inbox(s) == 2);
}

/// Waits until the window has received the state after the `n`th save.
fn wait_for_saves(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the save result", |s| s.state.privacy_saves == n);
}

#[gpui_kit::test]
fn the_privacy_section_shows_the_server_settings(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        // The defaults: every built-in kind on, no custom pattern, nothing to save.
        for id in TOGGLES {
            assert_eq!(window.find(id).checked(), Some(true), "{id}");
        }
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");
        assert!(window.try_find("pattern-label-0").is_none());
        assert_eq!(label(window, "privacy-changes"), "");
        assert_eq!(label(window, "privacy-result"), "");
        let help = label(window, "privacy-help");
        for text in [
            "removed before the agent sees it",
            "does not find names or postal addresses",
            "Rust regex crate",
        ] {
            assert!(help.contains(text), "{help}");
        }
    });

    // Settings that the server already has are shown, not the defaults.
    let stored = PrivacyConfig {
        card_number: false,
        custom: vec![pattern("project", r"PRJ-\d{4}")],
        ..PrivacyConfig::default()
    };
    rt.block_on(
        rig.engine
            .set_privacy("lee", bunsin_api::PrivacyUpdate::unchecked(stored)),
    )
    .unwrap();
    on_screen(cx, window, |window, cx| window.click("profile-lee", cx));
    wait_until(cx, &shell, "lee's settings", |s| {
        s.state.privacy.as_ref().is_some_and(|p| !p.card_number)
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(window.find("privacy-card-number").checked(), Some(false));
        assert_eq!(window.find("privacy-email").checked(), Some(true));
        assert_eq!(label(window, "pattern-label-0"), "project");
        assert_eq!(label(window, "pattern-regex-0"), r"PRJ-\d{4}");
        assert_eq!(label(window, "privacy-changes"), "");
    });
}

#[gpui_kit::test]
fn saved_settings_are_the_ones_the_server_uses(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("privacy-email", cx);
        assert_eq!(window.find("privacy-email").checked(), Some(false));
        type_pattern(window, cx, "project", r"PRJ-\d{4}");
        window.click("add-pattern", cx);
        assert_eq!(label(window, "pattern-label-0"), "project");
        assert_eq!(label(window, "pattern-regex-0"), r"PRJ-\d{4}");
        // Add empties the fields for the next pattern.
        assert_eq!(value(window, "new-label"), "");
        assert_eq!(value(window, "new-pattern"), "");
        assert_eq!(label(window, "privacy-changes"), "Not saved");
        assert_eq!(label(window, "privacy-result"), "");
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 1);

    let expected = PrivacyConfig {
        email: false,
        custom: vec![pattern("project", r"PRJ-\d{4}")],
        ..PrivacyConfig::default()
    };
    assert_eq!(rt.block_on(rig.engine.privacy("kim")).unwrap(), expected);
    assert_eq!(
        rt.block_on(rig.engine.privacy("lee")).unwrap(),
        PrivacyConfig::default()
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "privacy-result"), "Saved");
        assert_eq!(label(window, "privacy-changes"), "");
        assert_eq!(window.find("privacy-email").checked(), Some(false));
        assert_eq!(label(window, "pattern-label-0"), "project");
    });

    // A new event goes through the saved filter before the LLM sees it.
    let proposal = serde_json::json!({
        "kind": "tool_call", "rationale": "Lee asked for the status", "summary": null,
        "tool": "slack/post",
        "args_json": serde_json::json!({"channel": "C1", "text": "Not yet."}).to_string()
    });
    rig.llm.reply(&proposal.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "e-privacy".into(),
        thread: None,
        author: "lee".into(),
        text: "Is PRJ-1234 done? Mail lee@example.com".into(),
    };
    rt.block_on(rig.engine.ingest("kim", event)).unwrap();
    let request = serde_json::to_string(rig.llm.requests().last().unwrap()).unwrap();
    assert!(!request.contains("PRJ-1234"), "{request}");
    assert!(request.contains("[REDACTED:project]"), "{request}");
    // Email filtering is off now, so the address reaches the LLM.
    assert!(request.contains("lee@example.com"), "{request}");
}

#[gpui_kit::test]
fn remove_and_save_deletes_the_pattern_on_the_server(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let stored = PrivacyConfig {
        custom: vec![pattern("first", "AAA"), pattern("second", "BBB")],
        ..PrivacyConfig::default()
    };
    rt.block_on(
        rig.engine
            .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(stored)),
    )
    .unwrap();
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 2");
        window.click("remove-pattern-0", cx);
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 1");
        assert_eq!(label(window, "pattern-label-0"), "second");
        assert!(window.try_find("pattern-label-1").is_none());
        assert_eq!(label(window, "privacy-changes"), "Not saved");
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 1);
    let saved = rt.block_on(rig.engine.privacy("kim")).unwrap();
    assert_eq!(saved.custom, vec![pattern("second", "BBB")]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "privacy-result"), "Saved");
    });
}

#[gpui_kit::test]
fn a_refused_pattern_shows_the_server_error_and_keeps_the_form(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("privacy-phone", cx);
        type_pattern(window, cx, "bad", "(unclosed");
        window.click("add-pattern", cx);
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, _| {
        let result = label(window, "privacy-result");
        assert!(
            result.contains("the pattern of `bad` is not a valid regular expression"),
            "{result}"
        );
        assert_eq!(label(window, "privacy-changes"), "Not saved");
        assert_eq!(label(window, "pattern-label-0"), "bad");
        assert_eq!(label(window, "pattern-regex-0"), "(unclosed");
        assert_eq!(window.find("privacy-phone").checked(), Some(false));
        // A refusal that is not a conflict offers no "Save anyway".
        for id in ["privacy-conflict", "privacy-load", "privacy-save-anyway"] {
            assert!(window.try_find(id).is_none(), "{id}");
        }
    });
    assert_eq!(
        rt.block_on(rig.engine.privacy("kim")).unwrap(),
        PrivacyConfig::default()
    );
}

#[gpui_kit::test]
fn a_refresh_does_not_overwrite_the_form_being_edited(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("privacy-phone", cx);
        type_pattern(window, cx, "half typed", "NOT-ADDED");
    });

    // Another client changes the server's settings, so a refresh brings a new state.
    let other = PrivacyConfig {
        card_number: false,
        ..PrivacyConfig::default()
    };
    rt.block_on(
        rig.engine
            .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(other)),
    )
    .unwrap();
    wait_until(cx, &shell, "the refreshed settings", |s| {
        s.state.privacy.as_ref().is_some_and(|p| !p.card_number)
    });
    // Let more refreshes run.
    std::thread::sleep(REFRESH * 4);
    cx.run_until_parked();

    on_screen(cx, window, |window, _| {
        assert_eq!(value(window, "new-label"), "half typed");
        assert_eq!(value(window, "new-pattern"), "NOT-ADDED");
        assert_eq!(window.find("privacy-phone").checked(), Some(false));
        // The form keeps the user's value, not the server's new one.
        assert_eq!(window.find("privacy-card-number").checked(), Some(true));
        assert_eq!(label(window, "privacy-changes"), "Not saved");
    });
}

#[gpui_kit::test]
fn edits_after_a_successful_save_survive_a_refresh(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| window.click("save-privacy", cx));
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "privacy-result"), "Saved");
        window.click("privacy-phone", cx);
        // The form now differs from the server, so it is not "Saved".
        assert_eq!(label(window, "privacy-result"), "");
        assert_eq!(label(window, "privacy-changes"), "Not saved");
    });

    // A new event changes the state. The state still carries the earlier save result.
    rt.block_on(common::ingest_second_event(&rig));
    wait_until(cx, &shell, "the new item", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| snapshot.inbox.len() == 2)
    });
    assert_eq!(shell.read_with(cx, |s, _| s.state.privacy_result.clone()), Some(Ok(())));
    on_screen(cx, window, |window, _| {
        assert_eq!(window.find("privacy-phone").checked(), Some(false));
        assert_eq!(label(window, "privacy-changes"), "Not saved");
        assert_eq!(label(window, "privacy-result"), "");
    });
}

#[gpui_kit::test]
fn another_profile_discards_the_draft_and_saves_only_to_itself(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("privacy-phone", cx);
        type_pattern(window, cx, "kim only", "KIM-[0-9]+");
        window.click("add-pattern", cx);
        type_pattern(window, cx, "left over", "LEFT");
        window.click("profile-lee", cx);
    });
    wait_until(cx, &shell, "lee's settings", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.privacy.is_some()
    });
    on_screen(cx, window, |window, cx| {
        for id in TOGGLES {
            assert_eq!(window.find(id).checked(), Some(true), "{id}");
        }
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");
        assert!(window.try_find("pattern-label-0").is_none());
        assert_eq!(value(window, "new-label"), "");
        assert_eq!(value(window, "new-pattern"), "");
        assert_eq!(label(window, "privacy-changes"), "");
        type_pattern(window, cx, "lee only", "LEE-[0-9]+");
        window.click("add-pattern", cx);
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 1);
    let lee = PrivacyConfig {
        custom: vec![pattern("lee only", "LEE-[0-9]+")],
        ..PrivacyConfig::default()
    };
    assert_eq!(rt.block_on(rig.engine.privacy("lee")).unwrap(), lee);
    assert_eq!(
        rt.block_on(rig.engine.privacy("kim")).unwrap(),
        PrivacyConfig::default()
    );
}

#[gpui_kit::test]
fn add_with_an_empty_field_adds_no_row(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, _shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("add-pattern", cx);
        assert_eq!(label(window, "privacy-message"), "Enter a label and a pattern.");
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");

        // A label alone is not enough. The typed label stays in its field.
        window.click("new-label", cx);
        window.input("only a label", cx);
        window.click("add-pattern", cx);
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");
        assert_eq!(value(window, "new-label"), "only a label");

        // A label of spaces with a pattern is not enough either.
        window.click("new-label", cx);
        window.press("cmd-a", cx);
        window.input("   ", cx);
        window.click("new-pattern", cx);
        window.input("X", cx);
        window.click("add-pattern", cx);
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");
        assert!(window.try_find("pattern-label-0").is_none());
        assert_eq!(label(window, "privacy-message"), "Enter a label and a pattern.");
        // Nothing changed, so there is nothing to save.
        assert_eq!(label(window, "privacy-changes"), "");
    });
}

#[gpui_kit::test]
fn save_is_refused_while_the_add_fields_hold_text(cx: &mut TestAppContext) {
    const UNADDED: &str = "Add the pattern or clear the fields first";
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| window.click("save-privacy", cx));
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "privacy-result"), "Saved");
        // A pattern typed without Add.
        window.click("new-pattern", cx);
        window.input("NOT-ADDED", cx);
        window.click("save-privacy", cx);
        assert_eq!(label(window, "privacy-message"), UNADDED);
        // The earlier "Saved" must not stand next to text that was not stored.
        assert_eq!(label(window, "privacy-result"), "");

        // A label alone is refused too.
        window.press("cmd-a", cx);
        window.press("backspace", cx);
        window.click("new-label", cx);
        window.input("only a label", cx);
        window.click("save-privacy", cx);
        assert_eq!(label(window, "privacy-message"), UNADDED);
    });
    std::thread::sleep(REFRESH * 2);
    cx.run_until_parked();
    assert_eq!(shell.read_with(cx, |s, _| s.state.privacy_saves), 1);
    assert_eq!(
        rt.block_on(rig.engine.privacy("kim")).unwrap(),
        PrivacyConfig::default()
    );

    // After Add, the same text is saved.
    on_screen(cx, window, |window, cx| {
        window.click("new-pattern", cx);
        window.input("ADDED", cx);
        window.click("add-pattern", cx);
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 2);
    let saved = rt.block_on(rig.engine.privacy("kim")).unwrap();
    assert_eq!(saved.custom, vec![pattern("only a label", "ADDED")]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "privacy-message"), "");
        assert_eq!(label(window, "privacy-result"), "Saved");
    });
}

/// The privacy settings that another client saves in the conflict tests.
fn outside() -> PrivacyConfig {
    PrivacyConfig {
        card_number: false,
        custom: vec![pattern("outside", "OUT-[0-9]+")],
        ..PrivacyConfig::default()
    }
}

/// Saves `privacy` for kim from outside the window and waits until a refresh shows it.
fn change_privacy_from_outside(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
    shell: &Entity<Shell>,
    privacy: PrivacyConfig,
) {
    rt.block_on(rig.engine.set_privacy("kim", PrivacyUpdate::unchecked(privacy.clone())))
        .unwrap();
    wait_until(cx, shell, "the outside change", |s| {
        s.state.privacy.as_ref() == Some(&privacy)
    });
}

/// Opens the form, turns phone filtering off, lets another client save `outside()`, and saves.
/// The server refuses the save. Returns the window with the warning on screen.
fn refused_privacy_save(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
) -> (AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open_settings(cx, rt, rig);
    on_screen(cx, window, |window, cx| window.click("privacy-phone", cx));
    change_privacy_from_outside(cx, rt, rig, &shell, outside());
    on_screen(cx, window, |window, cx| {
        // The edited draft did not follow the server.
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");
        assert_eq!(label(window, "privacy-changes"), "Not saved");
        assert!(window.try_find("privacy-conflict").is_none());
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "privacy-conflict"), CONFLICT);
        assert!(window.try_find("privacy-load").is_some());
        assert!(window.try_find("privacy-save-anyway").is_some());
        // The conflict is not a plain error line.
        assert_eq!(label(window, "privacy-result"), "");
        // The draft stays in the form.
        assert_eq!(window.find("privacy-phone").checked(), Some(false));
        assert_eq!(window.find("privacy-card-number").checked(), Some(true));
        assert_eq!(label(window, "pattern-count"), "Custom patterns: 0");
    });
    assert_eq!(shell.read_with(cx, |s, _| s.state.error.clone()), None);
    assert_eq!(rt.block_on(rig.engine.privacy("kim")).unwrap(), outside());
    (window, shell)
}

const CONFLICT: &str = "The settings changed on the server since this form loaded them. Nothing was saved.";

#[gpui_kit::test]
fn a_privacy_save_over_an_outside_change_warns_and_save_anyway_overwrites(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = refused_privacy_save(cx, &rt, &rig);

    // The warning outlasts the timed refreshes.
    std::thread::sleep(REFRESH * 3);
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "privacy-conflict"), CONFLICT);
        window.click("privacy-save-anyway", cx);
    });
    wait_for_saves(cx, &shell, 2);
    let mine = PrivacyConfig {
        phone: false,
        ..PrivacyConfig::default()
    };
    assert_eq!(rt.block_on(rig.engine.privacy("kim")).unwrap(), mine);
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("privacy-conflict").is_none());
        assert_eq!(label(window, "privacy-result"), "Saved");
        assert_eq!(label(window, "privacy-changes"), "");
    });
}

#[gpui_kit::test]
fn load_after_a_conflict_shows_the_server_settings_and_drops_the_draft(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = refused_privacy_save(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("privacy-load", cx);
        assert!(window.try_find("privacy-conflict").is_none());
        assert_eq!(window.find("privacy-phone").checked(), Some(true));
        assert_eq!(window.find("privacy-card-number").checked(), Some(false));
        assert_eq!(label(window, "pattern-label-0"), "outside");
        assert_eq!(label(window, "privacy-changes"), "");
    });
    // The warning does not come back with a refresh.
    std::thread::sleep(REFRESH * 3);
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("privacy-conflict").is_none());
        // A save without edits now expects the loaded settings and changes nothing.
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 2);
    assert_eq!(rt.block_on(rig.engine.privacy("kim")).unwrap(), outside());
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("privacy-conflict").is_none());
        assert_eq!(label(window, "privacy-result"), "Saved");
    });
}

#[gpui_kit::test]
fn an_unchanged_privacy_form_follows_the_server_and_an_edited_one_does_not(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    change_privacy_from_outside(cx, &rt, &rig, &shell, outside());
    on_screen(cx, window, |window, cx| {
        assert_eq!(window.find("privacy-card-number").checked(), Some(false));
        assert_eq!(label(window, "pattern-label-0"), "outside");
        assert_eq!(label(window, "privacy-changes"), "");
        assert!(window.try_find("privacy-conflict").is_none());
        // Now the user edits the form.
        window.click("privacy-phone", cx);
    });

    let again = PrivacyConfig {
        email: false,
        ..outside()
    };
    change_privacy_from_outside(cx, &rt, &rig, &shell, again.clone());
    std::thread::sleep(REFRESH * 3);
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| {
        assert_eq!(window.find("privacy-email").checked(), Some(true));
        assert_eq!(window.find("privacy-phone").checked(), Some(false));
        assert_eq!(label(window, "privacy-changes"), "Not saved");
        // The base is the first outside change. The server holds the second one now.
        window.click("save-privacy", cx);
    });
    wait_for_saves(cx, &shell, 1);
    // The second outside change came after the edit, so the save is refused.
    assert_eq!(rt.block_on(rig.engine.privacy("kim")).unwrap(), again);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "privacy-conflict"), CONFLICT);
    });
}

#[gpui_kit::test]
fn an_item_shows_the_reads_it_is_based_on_and_can_be_approved(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(common::ingest_event_with_a_read(&rig));
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "inbox-count"), "Approval inbox: 2");
        assert_eq!(
            label(window, "basis-2"),
            r#"Read: slack/search {"query":"deploy date"}"#
        );
        // The item without reads shows no basis.
        assert!(window.try_find("basis-1").is_none());
        window.click("approve-2", cx);
    });
    wait_until(cx, &shell, "the executed item", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.inbox.iter().any(|i| i.id == 2 && i.state.as_str() == "executed"))
    });
    let (tool, args) = rig.slack.calls().last().cloned().unwrap();
    assert_eq!(tool, "post");
    assert!(args["text"].as_str().unwrap().starts_with("Friday."), "{args:?}");
    // The first item is still pending, below the other one. Two items are taller than the inbox
    // at laptop size, so the inbox scrolls to its buttons.
    on_screen(cx, window, |window, cx| {
        window.scroll("inbox-1", ScrollDelta::Pixels(point(px(0.), px(-200.))), cx);
        window.click("approve-1", cx);
    });
    wait_until(cx, &shell, "the first item executed", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.inbox.iter().all(|i| i.state.as_str() == "executed"))
    });
}

#[gpui_kit::test]
fn a_note_is_approved_in_the_inbox_and_stored_on_the_server(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(common::ingest_event_with_a_note(&rig));
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "inbox-count"), "Approval inbox: 3");
        // The note is told from an action by its title and by its button.
        assert_eq!(label(window, "inbox-2"), "pending · note `deploy-schedule` (new page)");
        let detail = label(window, "detail-2");
        assert!(detail.contains("LAST-LINE-OF-THE-PAGE"), "{detail}");
        assert!(detail.contains("- slack/search:"), "{detail}");
        // The long page scrolls in its own box. The arguments of an action have no such box.
        let page = window.find("detail-box-2").bounds().size.height;
        assert!(page <= px(220.), "{page:?}");
        assert!(window.try_find("detail-box-3").is_none());
        assert_eq!(label(window, "approve-2"), "Approve and keep");
        assert_eq!(label(window, "approve-3"), "Approve and run");
        window.scroll("inbox-2", ScrollDelta::Pixels(point(px(0.), px(-300.))), cx);
    });
    on_screen(cx, window, |window, cx| {
        // The title and the buttons of the note are on screen together.
        assert!(window.find("inbox-2").visible());
        window.click("approve-2", cx);
    });
    wait_until(cx, &shell, "the stored note", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.inbox.iter().any(|i| i.id == 2 && i.state.as_str() == "executed"))
    });
    let page = rt.block_on(rig.engine.wiki_page("kim", "deploy-schedule")).unwrap();
    assert!(page.text.body.ends_with("LAST-LINE-OF-THE-PAGE"));
    // Approving the note posted nothing.
    assert!(rig.slack.calls().iter().all(|(tool, _)| tool == "search"));
    // The action that came with it is still waiting.
    let snapshot = rt.block_on(rig.engine.snapshot("kim")).unwrap();
    assert!(
        snapshot
            .inbox
            .iter()
            .any(|i| i.id == 3 && i.state.as_str() == "pending")
    );
}
