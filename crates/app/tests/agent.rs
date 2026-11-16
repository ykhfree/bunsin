//! Headless window tests of the agent view: real view, real engine.

mod common;

use bunsin_api::{EngineApi, Expected, PrinciplesUpdate};
use bunsin_app::shell::Shell;
use common::window::{REFRESH, label, on_screen, open, runtime, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{App, Entity, ScrollDelta, TestAppContext, Window, point, px};

/// Types `text` into the principle field and presses its button.
fn add(window: &mut Window, cx: &mut App, text: &str) {
    window.within("principle-text").click("input-state", cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
    window.click("add-principle", cx);
}

fn rows(window: &Window, count: usize) -> Vec<String> {
    (0..count)
        .map(|at| {
            window
                .find(format!("principle-{at}"))
                .label()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn wait_for_saves(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the save result", |s| s.state.principle_saves == n);
}

fn open_agent(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
) -> (gpui_kit::AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open(cx, rt, rig);
    on_screen(cx, window, |window, cx| window.click("tab-agent", cx));
    wait_until(cx, &shell, "the principles", |s| s.state.principles.is_some());
    (window, shell)
}

#[gpui_kit::test]
fn principles_are_added_ordered_changed_removed_and_saved(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_agent(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "principles-count"), "Principles: 0");
        assert!(label(window, "principles-help").contains("privacy filter does not change them"));
        add(window, cx, "Answer in two sentences.");
        add(window, cx, "Never promise a date.");
        add(window, cx, "Be kind.");
    });
    // A refresh does not drop the draft.
    std::thread::sleep(REFRESH * 4);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            rows(window, 3),
            ["Answer in two sentences.", "Never promise a date.", "Be kind."]
        );
        assert_eq!(label(window, "principles-changes"), "Not saved");
        // The first has no Up, the last has no Down.
        assert!(window.try_find("principle-up-0").is_none());
        assert!(window.try_find("principle-down-2").is_none());
        window.click("principle-up-1", cx);
        window.click("principle-remove-2", cx);
        window.click("principle-edit-1", cx);
    });
    on_screen(cx, window, |window, cx| {
        // While a principle is in the field, the list cannot change under it.
        assert!(window.try_find("principle-remove-0").is_none());
        assert_eq!(label(window, "add-principle"), "Change principle 2");
        add(window, cx, "Answer in one sentence.");
        window.click("save-principles", cx);
    });
    wait_for_saves(cx, &shell, 1);
    assert_eq!(
        rt.block_on(rig.engine.principles("kim")).unwrap(),
        ["Never promise a date.", "Answer in one sentence."]
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "principles-result"), "Saved");
        assert_eq!(label(window, "principles-changes"), "");
        assert_eq!(label(window, "principles-count"), "Principles: 2");
    });
}

#[gpui_kit::test]
fn unadded_text_a_server_refusal_and_a_conflict_save_nothing(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_agent(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("save-principles", cx);
        assert_eq!(
            label(window, "principles-message"),
            "Nothing changed, so nothing was saved."
        );
        window.within("principle-text").click("input-state", cx);
        window.input("Typed, not added.", cx);
        window.click("save-principles", cx);
        assert_eq!(
            label(window, "principles-message"),
            "Add the principle or clear its field first."
        );
        // The server checks the length.
        add(window, cx, &"x".repeat(1_001));
    });
    // The long principle pushes Save down. The view scrolls.
    on_screen(cx, window, |window, cx| {
        window.scroll("principles-count", ScrollDelta::Pixels(point(px(0.), px(-800.))), cx);
    });
    on_screen(cx, window, |window, cx| window.click("save-principles", cx));
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        let result = label(window, "principles-result");
        assert!(result.contains("principle 1 has 1001 characters"), "{result}");
        window.scroll("principles-result", ScrollDelta::Pixels(point(px(0.), px(800.))), cx);
    });
    on_screen(cx, window, |window, cx| {
        window.click("principle-remove-0", cx);
        add(window, cx, "Mine.");
    });
    // Another client saves first.
    let other = PrinciplesUpdate {
        principles: vec!["Theirs.".into()],
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_principles("kim", other)).unwrap();
    on_screen(cx, window, |window, cx| window.click("save-principles", cx));
    wait_for_saves(cx, &shell, 2);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "principles-conflict").contains("Nothing was saved"));
        // The draft stays for the owner's choice.
        assert_eq!(rows(window, 1), ["Mine."]);
        window.click("principles-save-anyway", cx);
    });
    wait_for_saves(cx, &shell, 3);
    assert_eq!(rt.block_on(rig.engine.principles("kim")).unwrap(), ["Mine."]);
}
