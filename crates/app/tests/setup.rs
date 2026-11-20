//! Headless window tests of the setup of a new agent: real view, real engine with scripted clients.

mod common;

use bunsin_api::{EngineApi, Expected, LlmUpdate, PermissionsUpdate, ProfileConfig, TokenChange};
use bunsin_app::shell::{Shell, View};
use bunsin_core::eval::RunState;
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::settings::Wire;
use bunsin_core::tool::ToolSpec;
use common::window::{label, on_screen, open_sized, runtime, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, App, Entity, SharedString, TestAppContext, Window};
use serde_json::json;

/// Tall enough for every control of a step: a headless test cannot click what is scrolled away.
const TALL: (f32, f32) = (1280., 2000.);
const TOKEN: &str = "sk-setup-token-2b81";

fn tools() -> Vec<ToolSpec> {
    let spec = |name: &str, description: &str, properties: serde_json::Value| ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": properties}),
    };
    vec![
        spec(
            "post",
            "Post a message",
            json!({"channel": {"type": "string"}, "text": {"type": "string"}}),
        ),
        spec("search", "Search messages", json!({"query": {"type": "string"}})),
    ]
}

/// Adds profile `mia` with `config` to the rig. The window selects `kim` first.
fn add_mia(rt: &tokio::runtime::Runtime, rig: &common::Rig, config: ProfileConfig) {
    rig.slack.set_tools(tools());
    rt.block_on(rig.engine.create_profile("mia", config)).unwrap();
}

fn fill(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

fn step(window: &Window) -> String {
    label(window, "setup-step")
}

fn view(cx: &mut TestAppContext, shell: &Entity<Shell>) -> View {
    shell.read_with(cx, |shell, _| shell.view)
}

/// Selects `mia` and waits until her state is shown.
fn select_mia(cx: &mut TestAppContext, window: AnyWindowHandle, shell: &Entity<Shell>) {
    on_screen(cx, window, |window, cx| window.click("profile-mia", cx));
    wait_until(cx, shell, "mia's state", |s| {
        s.state.selected.as_deref() == Some("mia") && s.state.permissions.is_some()
    });
}

fn click_in(window: &mut Window, cx: &mut App, scope: &str, id: &'static str) {
    window.within(SharedString::from(scope.to_owned())).click(id, cx);
}

fn wait_commands(cx: &mut TestAppContext, shell: &Entity<Shell>, n: u64) {
    wait_until(cx, shell, "the connection command", |s| {
        s.state.connection_commands == n
    });
}

#[gpui_kit::test]
fn a_new_profile_is_set_up_step_by_step_with_existing_calls(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    add_mia(&rt, &rig, ProfileConfig::new("Mia"));
    let (window, shell) = open_sized(cx, &rt, &rig, TALL);
    // `kim` has approved tools, so the window shows the inbox as usual.
    assert_eq!(view(cx, &shell), View::Inbox);
    select_mia(cx, window, &shell);
    assert_eq!(view(cx, &shell), View::Setup);

    // 1. The LLM endpoint. Next needs a test that works.
    on_screen(cx, window, |window, cx| {
        assert_eq!(step(window), "Step 1 of 7: LLM endpoint");
        window.click("setup-next", cx);
        assert_eq!(step(window), "Step 1 of 7: LLM endpoint");
        assert!(label(window, "setup-message").starts_with("Save the endpoint and press Test."));
        fill(window, cx, "llm-token", TOKEN);
        window.click("save-llm", cx);
    });
    wait_commands(cx, &shell, 1);
    let ignore = json!({"kind": "ignore", "rationale": "connection test", "summary": null, "tool": null,
        "args_json": null});
    rig.llm.reply(&ignore.to_string(), 10);
    on_screen(cx, window, |window, cx| window.click("test-llm", cx));
    wait_commands(cx, &shell, 2);
    on_screen(cx, window, |window, cx| {
        assert!(
            label(window, "llm-test").starts_with("Works: "),
            "{}",
            label(window, "llm-test")
        );
        assert_eq!(label(window, "setup-llm-state"), "The endpoint works. Next goes on.");
        window.click("setup-next", cx);
        assert_eq!(step(window), "Step 2 of 7: Connect services");
    });

    // 2. A connector, and the connection check.
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "connector-name", "slack");
        fill(window, cx, "connector-address", "https://mcp.example/slack");
        window.click("save-connector", cx);
    });
    wait_commands(cx, &shell, 3);
    on_screen(cx, window, |window, cx| {
        window.within("connector-list").within("slack").click("check", cx);
    });
    wait_commands(cx, &shell, 4);
    on_screen(cx, window, |window, cx| {
        let checked = window
            .within("connector-list")
            .within("slack")
            .find("tools")
            .label()
            .unwrap()
            .to_owned();
        assert!(checked.starts_with("works: 2 tool(s)"), "{checked}");
        window.click("setup-next", cx);
        assert_eq!(step(window), "Step 3 of 7: Tools and rules");
    });

    // 3. The tool list, its approval, and a rule for each tool.
    on_screen(cx, window, |window, cx| {
        click_in(window, cx, "setup-slack", "list-tools")
    });
    wait_commands(cx, &shell, 5);
    on_screen(cx, window, |window, cx| {
        let list = window.within("setup-slack");
        assert_eq!(list.find("tool-post").label(), Some("post"));
        assert_eq!(list.find("tool-search").label(), Some("search"));
        assert_eq!(list.find("approved-count").label(), Some("Approved tools: 0"));
        click_in(window, cx, "setup-slack", "approve-tools");
    });
    wait_until(cx, &shell, "the approved tools", |s| {
        s.state
            .permissions
            .as_ref()
            .is_some_and(|p| p.tools.get("slack").is_some_and(|t| t.len() == 2))
    });
    on_screen(cx, window, |window, cx| {
        // No class is chosen for a tool by its name or description.
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "The agent cannot use this tool."
        );
        assert_eq!(
            label(window, "perm-slack/search-state"),
            "The agent cannot use this tool."
        );
        window.click("perm-slack/search-read", cx);
        window.click("perm-slack/post-write", cx);
        // A new write starts at Approve. The setup offers no Draft and no Autonomous.
        assert!(window.try_find("perm-slack/post-tier-approve").is_some());
        assert!(window.try_find("perm-slack/post-tier-observe").is_some());
        assert!(window.try_find("perm-slack/post-tier-draft").is_none());
        assert!(window.try_find("perm-slack/post-tier-autonomous").is_none());
        // The text for other people is an explicit choice.
        window.click("save-permissions", cx);
        assert!(label(window, "permissions-message").starts_with("Choose where the text for other people is"));
        window.click("perm-slack/post-text-text", cx);
        window.click("save-permissions", cx);
    });
    wait_until(cx, &shell, "the rules", |s| s.state.permission_saves == 1);
    let rules = rt.block_on(rig.engine.permissions("mia")).unwrap().rules;
    assert_eq!(rules["slack/search"], ToolRule::read());
    assert_eq!(
        rules["slack/post"],
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into()))
    );
    assert_eq!(rules["slack/post"].tier, Tier::Approve);

    // 4. The knowledge step is optional. Skip calls nothing.
    on_screen(cx, window, |window, cx| {
        window.click("setup-next", cx);
        assert_eq!(step(window), "Step 4 of 7: Knowledge");
        assert!(window.try_find("preload-notes").is_some());
        window.click("setup-skip", cx);
        assert_eq!(step(window), "Step 5 of 7: Principles");
    });
    assert!(rig.slack.calls().is_empty());

    // 5. Principle candidates from a read of the owner's messages.
    rig.slack.set_result(
        "search",
        json!({"messages": [{"text": "Sure, I will check Jira first."}]}),
    );
    let found = json!({"candidates": [
        {"text": "Check Jira before you promise.", "evidence": "I will check Jira first."},
        {"text": "Use emoji.", "evidence": "Sure"}
    ]});
    rig.llm.reply(&found.to_string(), 10);
    on_screen(cx, window, |window, cx| {
        fill(window, cx, "read-tool", "slack/search");
        fill(window, cx, "read-args", r#"{"query": "from:mia"}"#);
        window.click("extract-read", cx);
    });
    wait_until(cx, &shell, "the extraction", |s| s.state.extract_result == Some(Ok(2)));
    assert_eq!(rig.slack.calls()[0].0, "search");
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "candidates-count"), "New candidates: 2");
        window.click("adopt-1", cx);
    });
    wait_until(cx, &shell, "the adoption", |s| {
        s.state.principles.as_ref().is_some_and(|p| p.len() == 1)
    });
    on_screen(cx, window, |window, cx| window.click("dismiss-2", cx));
    wait_until(cx, &shell, "the dismissal", |s| {
        s.state.candidates.as_ref().is_some_and(|c| {
            c.iter()
                .all(|c| c.state != bunsin_core::candidates::CandidateState::New)
        })
    });
    assert_eq!(
        rt.block_on(rig.engine.principles("mia")).unwrap(),
        ["Check Jira before you promise."]
    );

    // 6. A replay case, and the first evaluation with its score.
    on_screen(cx, window, |window, cx| {
        window.click("setup-next", cx);
        assert_eq!(step(window), "Step 6 of 7: Evaluation");
        window.click("add-cases", cx);
        assert!(label(window, "cases-result").starts_with("The cases are not a JSON array of cases"));
        window.within("setup-cases").click("input-state", cx);
        window.input(
            r#"[{"event": {"source": "slack", "external_id": "c1", "author": "lee", "text": "when do we deploy?"}, "actual_reply": "We deploy at 3 pm."}]"#,
            cx,
        );
        window.click("add-cases", cx);
    });
    wait_until(cx, &shell, "the cases", |s| s.state.case_adds == 1);
    let post = json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "slack/post",
        "args_json": json!({"channel": "C1", "text": "We deploy at 3 pm."}).to_string()});
    rig.llm.reply(&post.to_string(), 10);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "cases-result"), "1 case added.");
        assert_eq!(label(window, "cases-count"), "Replay cases: 1");
        window.click("start-eval", cx);
    });
    wait_until(cx, &shell, "the run", |s| {
        s.state.eval_report.as_ref().is_some_and(|r| r.run.is_some())
    });
    // The server's poll task replays the cases, then ends the run. This test has none, so it ticks here.
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
        assert!(
            label(window, "eval-rates").starts_with("Kind 100%"),
            "{}",
            label(window, "eval-rates")
        );
        window.click("setup-next", cx);
        assert_eq!(step(window), "Step 7 of 7: Finish");
    });

    // 7. The summary. Every write rule is at Observe or Approve, so the setup ends.
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "summary-llm"), "LLM endpoint: saved, and its test works");
        assert_eq!(label(window, "summary-connectors"), "Connectors: 1, approved tools: 2");
        assert_eq!(label(window, "summary-rules"), "Rules: 1 read, 1 write or delete");
        assert_eq!(label(window, "summary-principles"), "Principles: 1");
        assert!(label(window, "summary-eval").starts_with("Evaluation: Kind 100%"));
        assert_eq!(
            label(window, "rules-check"),
            "Every write and delete rule is at Observe or Approve."
        );
        window.click("setup-finish", cx);
    });
    assert_eq!(view(cx, &shell), View::Inbox);
    // The token never reached the window's state.
    let state = shell.read_with(cx, |s, _| format!("{:?}", s.state));
    assert!(!state.contains(TOKEN));
}

#[gpui_kit::test]
fn the_setup_can_be_left_and_resumed_where_it_was(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    add_mia(&rt, &rig, ProfileConfig::new("Mia"));
    // The LLM endpoint was saved before, for example in an earlier window.
    let llm = LlmUpdate {
        wire: Wire::OpenAiChat,
        base_url: "https://llm.example/v1".into(),
        model: "m".into(),
        timeout_secs: 30,
        token: TokenChange::Set(TOKEN.into()),
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_llm("mia", llm)).unwrap();
    let (window, shell) = open_sized(cx, &rt, &rig, TALL);
    select_mia(cx, window, &shell);
    assert_eq!(view(cx, &shell), View::Setup);
    on_screen(cx, window, |window, cx| {
        // The server shows the endpoint, so the setup starts at the first step it does not show.
        assert_eq!(step(window), "Step 2 of 7: Connect services");
        window.click("setup-skip", cx);
        assert_eq!(step(window), "Step 3 of 7: Tools and rules");
        window.click("setup-leave", cx);
    });
    assert_eq!(view(cx, &shell), View::Agent);

    // Another profile and back: the setup does not open by itself a second time.
    on_screen(cx, window, |window, cx| window.click("profile-kim", cx));
    wait_until(cx, &shell, "kim", |s| s.state.selected.as_deref() == Some("kim"));
    select_mia(cx, window, &shell);
    assert_eq!(view(cx, &shell), View::Agent);

    // The Agent view opens it again, at the step where it was left.
    on_screen(cx, window, |window, cx| window.click("open-setup", cx));
    assert_eq!(view(cx, &shell), View::Setup);
    on_screen(cx, window, |window, cx| {
        assert_eq!(step(window), "Step 3 of 7: Tools and rules");
        window.click("setup-back", cx);
        assert_eq!(step(window), "Step 2 of 7: Connect services");
        window.click("setup-back", cx);
        assert_eq!(step(window), "Step 1 of 7: LLM endpoint");
        assert!(window.try_find("setup-back").is_none());
    });
    // The setup stored nothing of its own: the server has only the endpoint saved before.
    let config = rt.block_on(rig.engine.profile_config("mia")).unwrap();
    assert!(config.connectors.is_empty() && config.policy.rules.is_empty());
}

#[gpui_kit::test]
fn the_setup_ends_only_when_no_write_rule_is_beyond_approve(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    // A profile with approved tools from the settings file: the setup does not open by itself.
    let mut config = ProfileConfig::new("Mia");
    config.approved_tools.insert("slack".into(), tools());
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    config.policy.rules.insert(
        "slack/post".into(),
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
    );
    add_mia(&rt, &rig, config);
    let mut rules = rt.block_on(rig.engine.permissions("mia")).unwrap().rules;
    rules.get_mut("slack/post").unwrap().tier = Tier::Autonomous;
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    rt.block_on(rig.engine.set_permissions("mia", update)).unwrap();

    let (window, shell) = open_sized(cx, &rt, &rig, TALL);
    select_mia(cx, window, &shell);
    assert_eq!(view(cx, &shell), View::Inbox);
    on_screen(cx, window, |window, cx| window.click("tab-agent", cx));
    on_screen(cx, window, |window, cx| window.click("open-setup", cx));
    on_screen(cx, window, |window, cx| {
        // Without an LLM endpoint, the setup starts at the first step. Every step can be skipped.
        assert_eq!(step(window), "Step 1 of 7: LLM endpoint");
        for _ in 0..6 {
            window.click("setup-skip", cx);
        }
        assert_eq!(step(window), "Step 7 of 7: Finish");
        assert_eq!(label(window, "summary-llm"), "LLM endpoint: none");
        assert_eq!(
            label(window, "rules-check"),
            "Not at Observe or Approve: slack/post (autonomous)"
        );
        window.click("setup-finish", cx);
        assert_eq!(
            label(window, "setup-message"),
            "Each write and delete rule must be at Observe or Approve before the setup ends."
        );
    });
    assert_eq!(view(cx, &shell), View::Setup);

    on_screen(cx, window, |window, cx| window.click("rules-to-approve", cx));
    wait_until(cx, &shell, "the rules", |s| s.state.permission_saves == 1);
    let rules = rt.block_on(rig.engine.permissions("mia")).unwrap().rules;
    assert_eq!(rules["slack/post"].tier, Tier::Approve);
    assert_eq!(rules["slack/search"], ToolRule::read());
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "rules-check"),
            "Every write and delete rule is at Observe or Approve."
        );
        window.click("setup-finish", cx);
    });
    assert_eq!(view(cx, &shell), View::Inbox);
}
