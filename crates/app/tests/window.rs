//! Headless window tests: real view, real engine, scripted LLM and connector.

mod common;

use bunsin_api::EngineApi;
use bunsin_app::bridge::spawn_bridge;
use bunsin_app::shell::Shell;
use bunsin_core::event::InboundEvent;
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, App, AppContext, Bounds, Entity, Point, TestAppContext, Window, WindowBounds, WindowOptions, px,
    size,
};
use std::time::{Duration, Instant};

/// The bridge refresh interval in these tests. It is short, so a wait can outlast it.
const REFRESH: Duration = Duration::from_millis(50);

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap()
}

/// Waits in real time, because the engine runs on tokio threads.
fn wait_until(cx: &mut TestAppContext, shell: &Entity<Shell>, what: &str, done: impl Fn(&Shell) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        cx.run_until_parked();
        if shell.read_with(cx, |shell, _| done(shell)) {
            return;
        }
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Opens the main window on the engine of `rig` and waits for the first snapshot.
fn open(cx: &mut TestAppContext, rt: &tokio::runtime::Runtime, rig: &common::Rig) -> (AnyWindowHandle, Entity<Shell>) {
    // The engine wakes the view from tokio threads, as in the real app. Tell the test scheduler.
    cx.executor().allow_parking();
    let (commands, states) = spawn_bridge(rt.handle(), rig.engine.clone(), REFRESH);
    cx.update(gpui_kit::init);
    let (window, shell) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1100.), px(900.)),
        };
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| Shell::new(states, commands, window, cx))
        })
        .unwrap()
    });
    wait_until(cx, &shell, "the first snapshot", |s| s.state.snapshot.is_some());
    (window, shell)
}

/// Runs `f` on a freshly drawn frame of the window.
fn on_screen<R>(cx: &mut TestAppContext, window: AnyWindowHandle, f: impl FnOnce(&mut Window, &mut App) -> R) -> R {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        f(window, cx)
    })
    .unwrap()
}

fn label(window: &Window, id: &'static str) -> String {
    window.find(id).label().unwrap_or_default().to_owned()
}

fn value(window: &Window, id: &'static str) -> String {
    window.find(id).value().unwrap_or_default().to_owned()
}

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
    let (window, shell) = open(cx, &rt, &rig);
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
    rt.block_on(rig.engine.set_privacy("lee", stored)).unwrap();
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
    let (window, shell) = open(cx, &rt, &rig);
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
    rt.block_on(rig.engine.set_privacy("kim", stored)).unwrap();
    let (window, shell) = open(cx, &rt, &rig);
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
    let (window, shell) = open(cx, &rt, &rig);
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
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("privacy-phone", cx);
        type_pattern(window, cx, "half typed", "NOT-ADDED");
    });

    // Another client changes the server's settings, so a refresh brings a new state.
    let other = PrivacyConfig {
        card_number: false,
        ..PrivacyConfig::default()
    };
    rt.block_on(rig.engine.set_privacy("kim", other)).unwrap();
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
    let (window, shell) = open(cx, &rt, &rig);
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
    let (window, shell) = open(cx, &rt, &rig);
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
    let (window, _shell) = open(cx, &rt, &rig);
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
