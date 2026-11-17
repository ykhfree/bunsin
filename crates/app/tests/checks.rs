//! Headless window tests of the notes and evaluation cards of the settings view: real view, real engine.

mod common;

use bunsin_api::{EngineApi, EvalSettingsUpdate, Expected};
use bunsin_app::shell::Shell;
use bunsin_core::eval::{EvalConfig, EvalPass};
use bunsin_core::wiki::NotesConfig;
use common::window::{label, on_screen, open_sized, runtime, value, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, App, Entity, TestAppContext, Window};

/// The settings page scrolls. The window holds the whole page, because a headless test cannot click
/// a control that is scrolled away.
const TALL: (f32, f32) = (1280., 3200.);

fn open_settings(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
) -> (AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open_sized(cx, rt, rig, TALL);
    on_screen(cx, window, |window, cx| window.click("tab-settings", cx));
    wait_until(cx, &shell, "the settings", |s| {
        s.state.notes_settings.is_some() && s.state.eval_settings.is_some()
    });
    (window, shell)
}

fn retype(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

fn wait_for_saves(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the save result", |s| s.state.part_saves == n);
}

#[gpui_kit::test]
fn the_notes_settings_are_saved_and_refused_like_the_settings_file(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        // The window says what auto-apply never does.
        let help = label(window, "auto-apply-help");
        assert!(
            help.contains("never applies a change from a pre-load, an inbound question or a webhook event"),
            "{help}"
        );
        assert!(help.contains("never overwrites a page that you wrote"), "{help}");
        // A check every few hours that checks nothing is refused by the server's rule.
        retype(window, cx, "notes-lint-hours", "24");
        window.click("save-notes-settings", cx);
    });
    wait_for_saves(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        assert!(
            label(window, "notes-settings-result").contains("needs `notes.stale_days` or `notes.lint_conflicts`"),
            "{}",
            label(window, "notes-settings-result")
        );
        // A field that holds no number is refused in the window. Nothing is sent.
        retype(window, cx, "notes-stale-days", "a month");
        window.click("save-notes-settings", cx);
        assert_eq!(
            label(window, "notes-settings-message"),
            "Enter a whole number of 1 or more, or leave the field empty."
        );
        retype(window, cx, "notes-stale-days", "30");
        window.click("notes-auto-apply", cx);
        window.click("notes-lint-conflicts", cx);
        assert_eq!(label(window, "notes-settings-changes"), "Not saved");
        window.click("save-notes-settings", cx);
    });
    wait_for_saves(cx, &shell, 2);
    assert_eq!(
        rt.block_on(rig.engine.notes_settings("kim")).unwrap(),
        NotesConfig {
            auto_apply: true,
            stale_days: Some(30),
            lint_every_hours: Some(24),
            lint_conflicts: true,
        }
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "notes-settings-result"), "Saved");
        assert_eq!(label(window, "notes-settings-changes"), "");
        assert_eq!(label(window, "notes-settings-message"), "");
    });
    // The other profile is untouched.
    assert_eq!(
        rt.block_on(rig.engine.notes_settings("lee")).unwrap(),
        NotesConfig::default()
    );
}

#[gpui_kit::test]
fn the_pass_mark_and_the_judge_are_saved_and_a_conflict_offers_save_anyway(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_settings(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "eval-settings-help").contains("changes no rate and no pass"));
        // Two of three marks are refused in the window.
        retype(window, cx, "pass-kind", "80");
        retype(window, cx, "pass-overlap", "40");
        window.click("save-eval-settings", cx);
        assert_eq!(
            label(window, "eval-settings-message"),
            "Enter three whole numbers from 0 to 100, or leave all three empty."
        );
        retype(window, cx, "pass-boundary", "100");
        window.click("eval-judge", cx);
    });
    // Someone else turns the judge on meanwhile.
    let theirs = EvalConfig {
        pass: None,
        judge: true,
    };
    let update = EvalSettingsUpdate {
        eval: theirs.clone(),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_eval_settings("kim", update)).unwrap();
    on_screen(cx, window, |window, cx| window.click("save-eval-settings", cx));
    wait_for_saves(cx, &shell, 1);
    assert_eq!(rt.block_on(rig.engine.eval_settings("kim")).unwrap(), theirs);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "eval-settings-conflict").contains("Nothing was saved"));
        assert_eq!(value(window, "pass-boundary"), "100");
        window.click("eval-settings-save-anyway", cx);
    });
    wait_for_saves(cx, &shell, 2);
    let mark = EvalPass {
        kind_percent: 80,
        overlap_percent: 40,
        boundary_percent: 100,
    };
    assert_eq!(
        rt.block_on(rig.engine.eval_settings("kim")).unwrap(),
        EvalConfig {
            pass: Some(mark),
            judge: true,
        }
    );
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("eval-settings-conflict").is_none());
        assert_eq!(label(window, "eval-settings-result"), "Saved");
    });
    // The learning view reads the new mark.
    assert_eq!(rt.block_on(rig.engine.eval_report("kim")).unwrap().pass, Some(mark));
}
