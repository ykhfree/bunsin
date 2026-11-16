//! Headless window tests of the learning view: real view, real engine.

mod common;

use bunsin_api::{AutonomyThresholdsUpdate, Edit, EngineApi, Expected, ReplayCase};
use bunsin_app::shell::Shell;
use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::candidates::CandidateState;
use bunsin_core::eval::{Expect, RunState};
use bunsin_core::event::InboundEvent;
use common::window::{label, on_screen, open_sized, runtime, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, App, Entity, TestAppContext, Window};
use serde_json::json;

/// Tall enough that all four cards are on screen: a headless test cannot click what is scrolled away.
const TALL: (f32, f32) = (1280., 1800.);

fn open_learning(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
) -> (AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open_sized(cx, rt, rig, TALL);
    on_screen(cx, window, |window, cx| window.click("tab-learning", cx));
    wait_until(cx, &shell, "the learning data", |s| {
        s.state.corrections.is_some() && s.state.eval_report.is_some()
    });
    (window, shell)
}

fn retype(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

/// The owner edits the waiting post of the rig (item 1), which stores correction 1.
fn edit_the_waiting_post(rt: &tokio::runtime::Runtime, rig: &common::Rig) {
    let edit = Edit {
        args: json!({"channel": "C1", "text": "At 4 pm."})
            .as_object()
            .unwrap()
            .clone(),
        reason: "it moved".into(),
    };
    rt.block_on(rig.engine.approve("kim", 1, Some(edit))).unwrap();
}

#[gpui_kit::test]
fn a_correction_is_listed_and_retired(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    edit_the_waiting_post(&rt, &rig);
    let (window, shell) = open_learning(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "corrections-count"), "Corrections: 1");
        assert_eq!(
            label(window, "correction-1"),
            "#1 slack/post: We deploy at 3 pm. → At 4 pm."
        );
        window.click("retire-1", cx);
    });
    wait_until(cx, &shell, "the retirement", |s| {
        s.state.corrections.as_ref().is_some_and(|c| c[0].retired_at.is_some())
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "correction-1"),
            "#1 slack/post: We deploy at 3 pm. → At 4 pm. (retired)"
        );
        assert!(window.try_find("retire-1").is_none());
    });
}

#[gpui_kit::test]
fn candidates_are_found_then_adopted_edited_or_dismissed(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    edit_the_waiting_post(&rt, &rig);
    let found = json!({"candidates": [
        {"text": "Answer briefly.", "evidence": "At 4 pm."},
        {"text": "Confirm the time.", "evidence": "it moved"},
        {"text": "Use emoji.", "evidence": "At 4 pm."}
    ]});
    rig.llm.reply(&found.to_string(), 10);
    let (window, shell) = open_learning(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "candidates-count"), "New candidates: 0");
        window.click("extract-corrections", cx);
    });
    wait_until(cx, &shell, "the extraction", |s| s.state.extract_result == Some(Ok(3)));
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "extract-result"), "3 new candidates.");
        assert_eq!(label(window, "candidates-count"), "New candidates: 3");
        assert_eq!(label(window, "candidate-2"), "Confirm the time.");
        assert_eq!(label(window, "candidate-evidence-2"), "“it moved”");
        window.click("adopt-1", cx);
    });
    wait_until(cx, &shell, "the adoption", |s| {
        s.state.principles.as_deref() == Some(&["Answer briefly.".to_owned()][..])
    });
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("adopt-1").is_none());
        window.click("edit-candidate-2", cx);
    });
    on_screen(cx, window, |window, cx| {
        // The other candidates keep their buttons while one is edited.
        assert!(window.try_find("dismiss-3").is_some());
        window.within("candidate-text").click("input-state", cx);
        window.press("cmd-a", cx);
        window.input("Confirm the time in Jira.", cx);
        window.click("adopt-edit", cx);
    });
    wait_until(cx, &shell, "the adoption of the edit", |s| {
        s.state.principles.as_ref().is_some_and(|p| p.len() == 2)
    });
    on_screen(cx, window, |window, cx| window.click("dismiss-3", cx));
    wait_until(cx, &shell, "the dismissal", |s| {
        s.state
            .candidates
            .as_ref()
            .is_some_and(|c| c.iter().all(|c| c.state != CandidateState::New))
    });
    assert_eq!(
        rt.block_on(rig.engine.principles("kim")).unwrap(),
        ["Answer briefly.", "Confirm the time in Jira."]
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "candidates-count"), "New candidates: 0");
    });
}

#[gpui_kit::test]
fn an_evaluation_runs_from_the_view_and_a_case_is_deleted(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let case = |id: &str, text: &str, reply: &str, expect| ReplayCase {
        event: InboundEvent {
            source: "slack".into(),
            external_id: id.into(),
            thread: None,
            author: "lee".into(),
            text: text.into(),
        },
        actual_reply: reply.into(),
        expect,
    };
    let cases = vec![
        case("c1", "when do we deploy?", "We deploy at 3 pm.", Expect::Reply),
        case("c2", "can I get a raise?", "", Expect::Escalate),
    ];
    rt.block_on(rig.engine.add_replay_cases("kim", cases)).unwrap();
    let post = json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "slack/post",
        "args_json": json!({"channel": "C1", "text": "We deploy at 3 pm."}).to_string()});
    let escalate = json!({"kind": "escalate", "rationale": "money", "summary": "Lee asks about pay.",
        "tool": null, "args_json": null});
    rig.llm.reply(&post.to_string(), 10).reply(&escalate.to_string(), 10);

    let (window, shell) = open_learning(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "cases-count"), "Replay cases: 2");
        assert_eq!(label(window, "case-1"), "#1 reply: when do we deploy?");
        assert_eq!(label(window, "eval-state"), "No run yet.");
        window.click("start-eval", cx);
    });
    wait_until(cx, &shell, "the run", |s| {
        s.state.eval_report.as_ref().is_some_and(|r| r.run.is_some())
    });
    on_screen(cx, window, |window, _| {
        assert!(label(window, "eval-state").starts_with("Run 1: 2 of 2 cases left."));
    });
    // The server's poll task replays the cases. This test has none, so it ticks here.
    for _ in 0..2 {
        assert!(rt.block_on(rig.engine.background_tick()).is_empty());
    }
    wait_until(cx, &shell, "the end of the run", |s| {
        s.state
            .eval_report
            .as_ref()
            .and_then(|r| r.run.as_ref())
            .is_some_and(|run| run.state == RunState::Done)
    });
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "eval-state").starts_with("Run 1: done at "));
        assert_eq!(
            label(window, "eval-rates"),
            "Kind 100% · Overlap 100% · Boundary 100% · Over-escalation 0% · Read 0% · Failed 0 of 2"
        );
        assert_eq!(label(window, "result-1"), "#1 reply → reply (ok), overlap 100%");
        assert_eq!(label(window, "result-2"), "#2 escalate → escalate (ok)");
        assert_eq!(
            label(window, "eval-pass"),
            "No pass mark. Set eval.pass in the settings file."
        );
        window.click("delete-case-2", cx);
    });
    wait_until(cx, &shell, "the deletion", |s| {
        s.state.replay_cases.as_ref().is_some_and(|c| c.len() == 1)
    });
    // The report of the run keeps the result of the deleted case.
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "cases-count"), "Replay cases: 1");
        assert_eq!(label(window, "result-2"), "#2 escalate → escalate (ok)");
    });
}

#[gpui_kit::test]
fn the_thresholds_are_checked_saved_kept_on_a_conflict_and_turned_off(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open_learning(cx, &rt, &rig);
    let saves = |cx: &mut TestAppContext, n: u64| {
        wait_until(cx, &shell, "the save result", |s| s.state.threshold_saves == n);
    };
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "thresholds-now"), "Now off: nothing is suggested.");
        retype(window, cx, "min-reviewed", "5");
        retype(window, cx, "min-unchanged", "80");
        retype(window, cx, "window-days", "0");
        window.click("save-thresholds", cx);
    });
    saves(cx, 1);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "thresholds-result").contains("window_days"));
        // The window checks the numbers itself and sends nothing.
        retype(window, cx, "window-days", "a month");
        window.click("save-thresholds", cx);
        assert_eq!(
            label(window, "thresholds-message"),
            "Enter three whole numbers, or turn the suggestions off."
        );
        retype(window, cx, "window-days", "30");
        window.click("save-thresholds", cx);
    });
    saves(cx, 2);
    let saved = AutonomyThresholds {
        min_reviewed: 5,
        min_unchanged_percent: 80,
        window_days: 30,
    };
    assert_eq!(rt.block_on(rig.engine.autonomy_thresholds("kim")).unwrap(), Some(saved));
    wait_until(cx, &shell, "the new thresholds", |s| {
        s.state.autonomy.as_ref().is_some_and(|a| a.thresholds == Some(saved))
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "thresholds-result"), "Saved");
        assert_eq!(
            label(window, "thresholds-now"),
            "Now: 5 reviewed calls, 80% unchanged, last 30 days"
        );
        retype(window, cx, "min-reviewed", "6");
    });
    // Another client changes the thresholds meanwhile.
    let other = AutonomyThresholds {
        min_reviewed: 9,
        min_unchanged_percent: 90,
        window_days: 9,
    };
    let update = AutonomyThresholdsUpdate {
        thresholds: Some(other),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_autonomy_thresholds("kim", update)).unwrap();
    on_screen(cx, window, |window, cx| window.click("save-thresholds", cx));
    saves(cx, 3);
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("thresholds-conflict").is_some());
        // The changed field stays.
        assert_eq!(window.find("min-reviewed").value(), Some("6"));
        window.click("thresholds-save-anyway", cx);
    });
    saves(cx, 4);
    let anyway = AutonomyThresholds {
        min_reviewed: 6,
        ..saved
    };
    assert_eq!(
        rt.block_on(rig.engine.autonomy_thresholds("kim")).unwrap(),
        Some(anyway)
    );
    wait_until(cx, &shell, "the thresholds saved anyway", |s| {
        s.state.autonomy.as_ref().is_some_and(|a| a.thresholds == Some(anyway))
    });
    on_screen(cx, window, |window, cx| {
        assert!(window.try_find("thresholds-conflict").is_none());
        window.click("thresholds-off", cx);
    });
    saves(cx, 5);
    assert_eq!(rt.block_on(rig.engine.autonomy_thresholds("kim")).unwrap(), None);
}
