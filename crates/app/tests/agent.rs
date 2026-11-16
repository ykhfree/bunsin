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

/// Lets `kim`'s agent read `slack/search` and gives the tool a result with three messages.
async fn searchable(rig: &common::Rig) {
    let tools = vec![
        bunsin_testkit::tool("post", "Post a message"),
        bunsin_testkit::tool("search", "Search messages"),
    ];
    rig.slack.set_tools(tools.clone());
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config
        .policy
        .rules
        .insert("slack/search".into(), bunsin_core::policy::ToolRule::read());
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let messages = serde_json::json!({"messages": [
        {"ts": "1.1", "user": "lee", "text": "When do we deploy?"},
        {"ts": "1.2", "user": "kim", "text": "My own message."},
        {"ts": "1.3", "user": "park", "thread_ts": "1.0", "text": "Mail park@example.com"}
    ]});
    rig.slack.set_result("search", messages);
}

/// Replaces the text of a single-line field.
fn retype(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

/// Replaces the text of the text area inside `wrapper`.
fn retype_area(window: &mut Window, cx: &mut App, wrapper: &'static str, text: &str) {
    window.within(wrapper).click("input-state", cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

/// Fills the poll fields with a search of `tool`.
fn fill_poll(window: &mut Window, cx: &mut App, tool: &str, every: &str) {
    retype(window, cx, "poll-connector", "slack");
    retype(window, cx, "poll-tool", tool);
    retype(window, cx, "poll-every", every);
    retype_area(window, cx, "poll-args", "{\"query\": \"to:me\"}");
    retype(window, cx, "poll-items", "messages");
    retype(window, cx, "poll-id", "ts");
    retype(window, cx, "poll-text", "text");
    retype(window, cx, "poll-author", "user");
    retype(window, cx, "poll-thread", "thread_ts");
    retype_area(window, cx, "poll-skip", "kim\n");
}

fn wait_for_poll_commands(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the poll command", |s| s.state.poll_commands == n);
}

/// Opens the agent view in a window tall enough for both cards: a headless test cannot
/// click a control that is scrolled out of view. The inbox tests keep the laptop size.
fn open_tall(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &common::Rig,
) -> (gpui_kit::AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = common::window::open_sized(cx, rt, rig, (1280., 2000.));
    on_screen(cx, window, |window, cx| window.click("tab-agent", cx));
    wait_until(cx, &shell, "the polls", |s| s.state.polls.is_some());
    (window, shell)
}

#[gpui_kit::test]
fn a_poll_is_added_saved_tested_and_changed(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(searchable(&rig));
    let (window, shell) = open_tall(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "polls-count"), "Polls: 0");
        // The window checks what it can parse. The server checks the rest.
        fill_poll(window, cx, "search", "a minute");
        window.click("add-poll", cx);
        assert_eq!(
            label(window, "polls-message"),
            "Enter the interval as a whole number of seconds."
        );
        retype(window, cx, "poll-every", "60");
        retype_area(window, cx, "poll-args", "[\"to:me\"]");
        window.click("add-poll", cx);
        assert!(label(window, "polls-message").contains("must be a JSON object"));
        retype_area(window, cx, "poll-args", "{\"query\": \"to:me\"}");
        window.click("add-poll", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "poll-0"), "slack/search every 60 s");
        assert_eq!(label(window, "poll-args-0"), r#"{"query":"to:me"}"#);
        assert_eq!(
            label(window, "poll-mapping-0"),
            "items: messages, id: ts, text: text, author: user, thread: thread_ts"
        );
        assert_eq!(label(window, "poll-skip-0"), "skips: kim");
        // A draft is not tested: the server has other polls.
        window.click("poll-test-0", cx);
        assert_eq!(
            label(window, "polls-message"),
            "Save the list first. Test runs the saved polls."
        );
        window.click("save-polls", cx);
    });
    wait_for_poll_commands(cx, &shell, 1);
    let stored = rt.block_on(rig.engine.polls("kim")).unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].tool_id(), "slack/search");
    assert_eq!(stored[0].mapping.thread.as_deref(), Some("thread_ts"));
    assert_eq!(stored[0].skip_authors, ["kim"]);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "polls-result"), "Saved");
        assert_eq!(label(window, "poll-status-0"), "Not run yet");
        window.click("poll-test-0", cx);
    });
    wait_for_poll_commands(cx, &shell, 2);
    on_screen(cx, window, |window, _| {
        let test = label(window, "poll-test-result-0");
        assert!(
            test.starts_with("Test: 3 items in the result, 2 events for the agent"),
            "{test}"
        );
        assert!(test.contains("- lee (ID 1.1): When do we deploy?"), "{test}");
        // The skipped author is not an event. The text passed the privacy filter.
        assert!(!test.contains("My own message."), "{test}");
        assert!(
            test.contains("- park (ID 1.3, thread 1.0): Mail [REDACTED:EMAIL]"),
            "{test}"
        );
    });
    // The window shows the last run that the server's timer made.
    rt.block_on(rig.engine.poll_profile("kim")).unwrap();
    wait_until(cx, &shell, "the poll status", |s| {
        s.state
            .snapshot
            .as_ref()
            .is_some_and(|snap| snap.polls[0].last_run_at.is_some())
    });
    on_screen(cx, window, |window, cx| {
        let status = label(window, "poll-status-0");
        assert!(status.ends_with("3 items, 2 usable, 0 new events"), "{status}");
        window.click("poll-edit-0", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "add-poll"), "Change poll 1");
        // The fields hold the saved poll.
        assert_eq!(window.find("poll-every").value(), Some("60"));
        retype(window, cx, "poll-every", "120");
        window.click("add-poll", cx);
        window.click("save-polls", cx);
    });
    wait_for_poll_commands(cx, &shell, 3);
    assert_eq!(rt.block_on(rig.engine.polls("kim")).unwrap()[0].every_secs, 120);
}

#[gpui_kit::test]
fn the_server_reason_and_a_conflict_are_shown_and_nothing_is_saved(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(searchable(&rig));
    let (window, shell) = open_tall(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        // `post` writes. A poll runs without the owner, so the server refuses it.
        fill_poll(window, cx, "post", "60");
        window.click("add-poll", cx);
        window.click("save-polls", cx);
    });
    wait_for_poll_commands(cx, &shell, 1);
    on_screen(cx, window, |window, cx| {
        let result = label(window, "polls-result");
        assert!(result.contains("needs a rule with the class `read`"), "{result}");
        // The draft stays for a fix.
        assert_eq!(label(window, "poll-0"), "slack/post every 60 s");
        window.click("poll-edit-0", cx);
        retype(window, cx, "poll-tool", "search");
        window.click("add-poll", cx);
        // Text in the fields would not be saved.
        retype(window, cx, "poll-tool", "typed");
        window.click("save-polls", cx);
        assert_eq!(
            label(window, "polls-message"),
            "Add the poll or clear its fields first."
        );
        // An empty input does not delete selected text. Backspace does.
        window.click("poll-tool", cx);
        window.press("cmd-a", cx);
        window.press("backspace", cx);
    });
    assert!(rt.block_on(rig.engine.polls("kim")).unwrap().is_empty());
    // Another client saves a poll first.
    let mut theirs = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    let poll = serde_json::json!({
        "connector": "slack", "tool": "search", "args": {}, "every_secs": 300,
        "mapping": {"items": "messages", "id": "ts", "text": "text", "author": "user", "thread": null},
        "skip_authors": []
    });
    theirs.polls = vec![serde_json::from_value(poll).unwrap()];
    rt.block_on(rig.engine.set_profile_config("kim", theirs)).unwrap();
    on_screen(cx, window, |window, cx| window.click("save-polls", cx));
    wait_for_poll_commands(cx, &shell, 2);
    on_screen(cx, window, |window, cx| {
        assert!(label(window, "polls-conflict").contains("Nothing was saved"));
        window.click("polls-load", cx);
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "poll-0"), "slack/search every 300 s");
        assert_eq!(label(window, "polls-changes"), "");
    });
}
