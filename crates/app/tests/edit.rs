//! Headless window tests of the edit of a waiting call: real view, real engine.

mod common;

use bunsin_api::EngineApi;
use bunsin_app::shell::Shell;
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use common::window::{label, on_screen, open, runtime, screen_texts, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{App, Entity, ScrollDelta, TestAppContext, Window, point, px};
use serde_json::json;

const MARKER: &str = "— AI agent (Bunsin) acting for Kim";

/// The text in text area `n` of the editor of item `id`.
fn edit_text(window: &mut Window, id: i64, n: usize) -> String {
    let area = window.within(format!("edit-text-{id}-{n}")).find("input-state");
    area.value().unwrap_or_default().to_owned()
}

/// Replaces the text in text area `n` of the editor of item `id`.
fn retype(window: &mut Window, cx: &mut App, id: i64, n: usize, text: &str) {
    window.within(format!("edit-text-{id}-{n}")).click("input-state", cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

fn state_of(shell: &Shell, id: i64) -> Option<&'static str> {
    let snapshot = shell.state.snapshot.as_ref()?;
    snapshot.inbox.iter().find(|i| i.id == id).map(|i| i.state.as_str())
}

fn wait_for_state(cx: &mut TestAppContext, shell: &Entity<Shell>, id: i64, state: &'static str) {
    wait_until(cx, shell, state, |s| state_of(s, id) == Some(state));
}

#[gpui_kit::test]
fn an_edited_text_is_sent_with_the_marker_and_kept_as_a_correction(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        // The editor opens on request.
        assert!(window.try_find("edit-text-1-0").is_none());
        window.click("edit-1", cx);
    });
    on_screen(cx, window, |window, cx| {
        // The text area holds the agent's text without the marker. The marker is fixed text.
        assert_eq!(edit_text(window, 1, 0), "We deploy at 3 pm.");
        assert_eq!(label(window, "edit-place-1-0"), "Text (text)");
        assert_eq!(
            label(window, "edit-marker-1"),
            format!("The server adds this line after the text: {MARKER}")
        );
        // Every argument is still on screen, the ones the edit does not change too.
        let detail = label(window, "detail-1");
        assert!(detail.contains("\"channel\": \"C1\""), "{detail}");
        // The editor has its own buttons. The plain approval is not offered beside it.
        assert!(window.try_find("approve-1").is_none());
        assert!(window.try_find("edit-1").is_none());
        retype(window, cx, 1, 0, "We deploy at 4 pm.");
        window.click("edit-reason-1", cx);
        window.input("the time moved", cx);
        window.click("approve-edited-1", cx);
    });
    wait_for_state(cx, &shell, 1, "executed");

    let calls = rig.slack.calls();
    assert_eq!(calls.len(), 1);
    let (tool, args) = &calls[0];
    assert_eq!(tool, "post");
    assert_eq!(args["text"], format!("We deploy at 4 pm.\n\n{MARKER}"));
    assert_eq!(args["channel"], "C1");
    let snapshot = rt.block_on(rig.engine.snapshot("kim")).unwrap();
    let edited = snapshot
        .audit
        .iter()
        .find(|a| a.kind == "edited")
        .expect("an edit entry");
    assert_eq!(edited.detail["reason"], "the time moved");
    assert!(snapshot.artifacts.iter().any(|a| a.kind == "edited_draft"));
    // The editor closed with the item.
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("edit-text-1-0").is_none());
        assert!(window.try_find("approve-edited-1").is_none());
    });
}

#[gpui_kit::test]
fn cancel_and_an_unchanged_text_send_no_edit(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| window.click("edit-1", cx));
    on_screen(cx, window, |window, cx| {
        retype(window, cx, 1, 0, "Something else.");
        window.click("cancel-edit-1", cx);
    });
    on_screen(cx, window, |window, cx| {
        // Cancel drops the typed text and changes nothing on the server.
        assert!(window.try_find("edit-text-1-0").is_none());
        assert!(window.find("approve-1").visible());
        // A new edit starts from the server's text again.
        window.click("edit-1", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(edit_text(window, 1, 0), "We deploy at 3 pm.");
        // An empty text is not sent.
        retype(window, cx, 1, 0, "  ");
        window.click("approve-edited-1", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "edit-message-1"),
            "A text is empty. Write it, or dismiss the item."
        );
        retype(window, cx, 1, 0, "We deploy at 3 pm.");
        window.click("approve-edited-1", cx);
    });
    wait_for_state(cx, &shell, 1, "executed");
    // The same text is a plain approval: no correction for the agent to learn from.
    assert_eq!(
        rig.slack.calls()[0].1["text"],
        format!("We deploy at 3 pm.\n\n{MARKER}")
    );
    let snapshot = rt.block_on(rig.engine.snapshot("kim")).unwrap();
    assert!(snapshot.audit.iter().all(|a| a.kind != "edited"));
    assert!(snapshot.audit.iter().any(|a| a.kind == "approved"));
}

/// Gives `kim` a rule for `slack/post` with `text`, and hands the agent an event that it answers
/// with a post of `args`. The post is pending item 2.
async fn pending_post(rig: &common::Rig, text: OutgoingText, args: serde_json::Value) {
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config
        .policy
        .rules
        .insert("slack/post".into(), ToolRule::write(ToolClass::Write, text));
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let proposal = json!({"kind": "tool_call", "rationale": "Lee asked", "summary": null,
        "tool": "slack/post", "args_json": args.to_string()});
    rig.llm.reply(&proposal.to_string(), 10);
    let event = InboundEvent {
        source: "slack".into(),
        external_id: "e-blocks".into(),
        thread: None,
        author: "lee".into(),
        text: "the plan?".into(),
        channel: Default::default(),
    };
    rig.engine.ingest("kim", event).await.unwrap();
    // Item 1 is out of the way, so item 2 is at the top of the inbox.
    rig.engine.reject("kim", 1, "not needed").await.unwrap();
}

#[gpui_kit::test]
fn each_place_of_text_has_its_own_area_and_goes_back_to_its_place(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let rule = OutgoingText::Paths(vec!["text".into(), "blocks.*.text".into()]);
    let args = json!({"channel": "C1", "text": "Plan", "blocks": [{"text": "Monday"}, {"text": "Tuesday"}]});
    rt.block_on(pending_post(&rig, rule, args));
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| window.click("edit-2", cx));
    on_screen(cx, window, |window, cx| {
        let places: Vec<String> = (0..3)
            .map(|n| {
                window
                    .find(format!("edit-place-2-{n}"))
                    .label()
                    .unwrap_or_default()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            places,
            ["Text at text", "Text at blocks.0.text", "Text at blocks.1.text"]
        );
        assert_eq!(edit_text(window, 2, 1), "Monday");
        assert_eq!(edit_text(window, 2, 2), "Tuesday");
        retype(window, cx, 2, 2, "Wednesday");
        // Three text areas make the card taller than the inbox at laptop size. The inbox scrolls.
        window.scroll("inbox-2", ScrollDelta::Pixels(point(px(0.), px(-300.))), cx);
    });
    on_screen(cx, window, |window, cx| window.click("approve-edited-2", cx));
    wait_for_state(cx, &shell, 2, "executed");
    let args = &rig.slack.calls()[0].1;
    // Every place has the marker once. The unchanged places keep their text.
    assert_eq!(args["text"], format!("Plan\n\n{MARKER}"));
    assert_eq!(args["blocks"][0]["text"], format!("Monday\n\n{MARKER}"));
    assert_eq!(args["blocks"][1]["text"], format!("Wednesday\n\n{MARKER}"));
    assert_eq!(args["channel"], "C1");
}

#[gpui_kit::test]
fn a_call_without_text_and_another_profile_offer_no_edit(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(pending_post(
        &rig,
        OutgoingText::None,
        json!({"channel": "C1", "text": "hi"}),
    ));
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        // The rule sends no text, so there is nothing for the owner to edit.
        assert!(window.find("approve-2").visible());
        assert!(window.try_find("edit-2").is_none());
    });

    // An open edit ends when another profile is selected.
    let mut config = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    config.policy.rules.insert(
        "slack/post".into(),
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
    );
    rt.block_on(rig.engine.set_profile_config("kim", config)).unwrap();
    wait_until(cx, &shell, "the edit button", |s| {
        s.state
            .permissions
            .as_ref()
            .is_some_and(|p| matches!(p.rules["slack/post"].outgoing_text, OutgoingText::Arg(_)))
    });
    on_screen(cx, window, |window, cx| window.click("edit-2", cx));
    on_screen(cx, window, |window, cx| {
        assert_eq!(edit_text(window, 2, 0), "hi");
        window.click("profile-lee", cx);
    });
    wait_until(cx, &shell, "the other profile", |s| {
        s.state.selected.as_deref() == Some("lee")
    });
    wait_until(cx, &shell, "the closed editor", |s| s.draft_editor.editing().is_none());
    on_screen(cx, window, |window, _| {
        assert!(screen_texts(window).iter().all(|text| text != "hi"));
    });
    // Nothing was sent or changed.
    assert!(rig.slack.calls().is_empty());
}

#[gpui_kit::test]
fn an_edit_ends_when_the_item_is_handled_elsewhere(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let (window, shell) = open(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        window.click("edit-1", cx);
    });
    on_screen(cx, window, |window, cx| retype(window, cx, 1, 0, "Typed here."));
    // The command line dismisses the item while the editor is open.
    rt.block_on(rig.engine.reject("kim", 1, "done elsewhere")).unwrap();
    wait_for_state(cx, &shell, 1, "rejected");
    wait_until(cx, &shell, "the closed editor", |s| s.draft_editor.editing().is_none());
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("approve-edited-1").is_none());
    });
    assert!(rig.slack.calls().is_empty());
}
