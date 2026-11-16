//! Headless tests of the permissions view: real view, real engine. The window has laptop size.

mod common;

use bunsin_api::{EngineApi, Expected, PermissionsUpdate};
use bunsin_app::shell::Shell;
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::poll::{PollConfig, PollMapping};
use bunsin_core::tool::ToolSpec;
use common::Rig;
use common::window::{REFRESH, on_screen, open, runtime, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, Entity, TestAppContext, Window};
use serde_json::json;
use std::collections::BTreeMap;

const PAGE_CONFLICT: &str = "The rules changed on the server since this view loaded them. Nothing was saved.";
const OFF: &str = "The agent cannot use this tool.";
const READ: &str = "Read: the agent calls it without asking, also while all changes are stopped.";

fn spec(name: &str, description: &str, properties: serde_json::Value) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        input_schema: json!({"type": "object", "properties": properties}),
    }
}

/// `post` has two text arguments and one that is not text. `search` has one. `remove` has one.
fn slack_tools() -> Vec<ToolSpec> {
    let text = json!({"type": "string"});
    vec![
        spec(
            "post",
            "Post a message",
            json!({"channel": text, "text": text, "broadcast": {"type": "boolean"}}),
        ),
        spec("search", "Search messages", json!({"query": text})),
        spec("remove", "Delete a message", json!({"ts": text})),
    ]
}

fn write(tier: Tier, text: OutgoingText) -> ToolRule {
    ToolRule {
        class: ToolClass::Write,
        tier,
        outgoing_text: text,
    }
}

fn text_in(arg: &str) -> OutgoingText {
    OutgoingText::Arg(arg.into())
}

/// Gives `kim` the approved tools `tools`. The rule of `slack/post` from the rig stays.
async fn approve(rig: &Rig, tools: Vec<ToolSpec>) {
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.approved_tools.insert("slack".into(), tools);
    rig.engine.set_profile_config("kim", config).await.unwrap();
}

/// Replaces `kim`'s rules as another client would.
async fn store(rig: &Rig, change: impl FnOnce(&mut BTreeMap<String, ToolRule>)) {
    let mut rules = rules(rig).await;
    change(&mut rules);
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    rig.engine.set_permissions("kim", update).await.unwrap();
}

async fn rules(rig: &Rig) -> BTreeMap<String, ToolRule> {
    rig.engine.permissions("kim").await.unwrap().rules
}

/// Opens the window on the permissions view.
fn open_permissions(
    cx: &mut TestAppContext,
    rt: &tokio::runtime::Runtime,
    rig: &Rig,
) -> (AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open(cx, rt, rig);
    on_screen(cx, window, |window, cx| window.click("tab-permissions", cx));
    (window, shell)
}

fn label(window: &Window, id: &str) -> String {
    window
        .try_find(id.to_owned())
        .and_then(|element| element.label().map(str::to_owned))
        .unwrap_or_default()
}

fn clicks(cx: &mut TestAppContext, window: AnyWindowHandle, ids: &[&str]) {
    for id in ids {
        on_screen(cx, window, |window, cx| window.click((*id).to_owned(), cx));
    }
}

fn saves(cx: &mut TestAppContext, shell: &Entity<Shell>) -> u64 {
    shell.read_with(cx, |s, _| s.state.permission_saves)
}

/// Clicks Save and waits for the server's answer.
fn save(cx: &mut TestAppContext, window: AnyWindowHandle, shell: &Entity<Shell>) {
    let before = saves(cx, shell);
    clicks(cx, window, &["save-permissions"]);
    wait_until(cx, shell, "the result of the save", |s| {
        s.state.permission_saves > before
    });
}

/// Lets several refreshes pass, for a check that nothing happened.
fn let_refreshes_pass(cx: &mut TestAppContext) {
    for _ in 0..4 {
        std::thread::sleep(REFRESH);
        cx.run_until_parked();
    }
}

#[gpui_kit::test]
fn the_view_shows_each_approved_tool_with_its_stored_rule(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let (window, _shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. The AI marker goes into `text`."
        );
        // A tool without a rule is off. Nothing is chosen for it from its name or description.
        assert_eq!(label(window, "perm-slack/search-state"), OFF);
        assert_eq!(label(window, "perm-slack/remove-state"), OFF);
        // Only the text arguments of the tool are offered for the AI marker.
        assert!(window.try_find("perm-slack/post-text-channel").is_some());
        assert!(window.try_find("perm-slack/post-text-text").is_some());
        assert!(window.try_find("perm-slack/post-text-broadcast").is_none());
        // A tool that is off has no step and no text choice.
        assert!(window.try_find("perm-slack/search-tier-approve").is_none());
        assert!(window.try_find("perm-slack/search-text-none").is_none());
        assert_eq!(label(window, "permissions-changes"), "");
        assert_eq!(label(window, "permissions-result"), "");
    });
}

#[gpui_kit::test]
fn a_click_changes_only_the_draft_and_save_stores_it(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let before = rt.block_on(rules(&rig));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    clicks(cx, window, &["perm-slack/search-read", "perm-slack/post-tier-draft"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "perm-slack/search-state"), READ);
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 2 rules\nslack/post: write, approve, text in `text` → write, draft, text in `text`\n\
             slack/search: off → read"
        );
    });
    // Nothing reaches the server before Save, also not with a refresh.
    let_refreshes_pass(cx);
    assert_eq!(rt.block_on(rules(&rig)), before);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "perm-slack/search-state"), READ);
    });

    save(cx, window, &shell);
    let stored = rt.block_on(rules(&rig));
    assert_eq!(stored["slack/search"], ToolRule::read());
    assert_eq!(stored["slack/post"], write(Tier::Draft, text_in("text")));
    assert_eq!(stored.len(), 2);
    let_refreshes_pass(cx);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-result"), "Saved");
        assert_eq!(label(window, "permissions-changes"), "");
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: you see the draft. It does not run. The AI marker goes into `text`."
        );
    });

    // A save of an unchanged form sends nothing.
    let sent = saves(cx, &shell);
    clicks(cx, window, &["save-permissions"]);
    let_refreshes_pass(cx);
    assert_eq!(saves(cx, &shell), sent);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "permissions-message"),
            "Nothing changed, so nothing was saved."
        );
    });

    // Off removes the rule.
    clicks(cx, window, &["perm-slack/search-off"]);
    save(cx, window, &shell);
    assert!(!rt.block_on(rules(&rig)).contains_key("slack/search"));
}

#[gpui_kit::test]
fn a_write_rule_is_not_saved_until_the_text_choice_is_made(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    // Only these two tools, so that both are on screen. The rule of `slack/post` stays, without a tool.
    let mut tools = slack_tools();
    tools.remove(0);
    rt.block_on(approve(&rig, tools));
    let before = rt.block_on(rules(&rig));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    clicks(cx, window, &["perm-slack/remove-write", "save-permissions"]);
    let_refreshes_pass(cx);
    assert_eq!(saves(cx, &shell), 0);
    assert_eq!(rt.block_on(rules(&rig)), before);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "permissions-message"),
            "Choose where the text for other people is, for: slack/remove."
        );
        assert_eq!(
            label(window, "perm-slack/remove-state"),
            "Write: it runs after you approve. Choose where its text for other people is."
        );
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 1 rule\nslack/remove: off → write, approve, text not chosen"
        );
    });

    // "No text" is a choice of the owner. It is stored as such.
    clicks(cx, window, &["perm-slack/remove-text-none"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-message"), "");
    });
    save(cx, window, &shell);
    assert_eq!(
        rt.block_on(rules(&rig))["slack/remove"],
        write(Tier::Approve, OutgoingText::None)
    );

    // A chosen argument is stored by its name.
    clicks(cx, window, &["perm-slack/search-write", "perm-slack/search-text-query"]);
    save(cx, window, &shell);
    assert_eq!(
        rt.block_on(rules(&rig))["slack/search"],
        write(Tier::Approve, text_in("query"))
    );
}

#[gpui_kit::test]
fn a_new_class_starts_at_approve_and_a_delete_is_never_offered_without_asking(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    rt.block_on(store(&rig, |rules| {
        rules.insert("slack/post".into(), write(Tier::Autonomous, text_in("text")));
    }));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs without asking. The AI marker goes into `text`."
        );
        assert!(window.try_find("perm-slack/post-tier-autonomous").is_some());
    });
    // "Without asking" does not follow the rule into the delete class.
    clicks(cx, window, &["perm-slack/post-delete"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Delete: it runs after you approve. The AI marker goes into `text`."
        );
        assert!(window.try_find("perm-slack/post-tier-autonomous").is_none());
    });
    save(cx, window, &shell);
    let delete = ToolRule {
        class: ToolClass::Delete,
        tier: Tier::Approve,
        outgoing_text: text_in("text"),
    };
    assert_eq!(rt.block_on(rules(&rig))["slack/post"], delete);

    // From Read, a write is a new rule: the step is Approve and the text is not chosen.
    clicks(cx, window, &["perm-slack/post-read", "perm-slack/post-write"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. Choose where its text for other people is."
        );
    });
}

#[gpui_kit::test]
fn stored_rules_that_the_buttons_cannot_make_are_shown_as_they_are(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let odd_delete = ToolRule {
        class: ToolClass::Delete,
        tier: Tier::Autonomous,
        outgoing_text: OutgoingText::None,
    };
    rt.block_on(store(&rig, |rules| {
        // An argument that is not at the top of the tool's input, a delete "without asking",
        // and a rule whose tool is not approved.
        rules.insert("slack/post".into(), write(Tier::Approve, text_in("blocks")));
        rules.insert("slack/remove".into(), odd_delete.clone());
        rules.insert("jira/create".into(), write(Tier::Draft, text_in("summary")));
    }));
    let before = rt.block_on(rules(&rig));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. The AI marker goes into `blocks`."
        );
        assert!(window.try_find("perm-slack/post-text-blocks").is_some());
        // The sentence says what the server does: it asks for every delete.
        assert_eq!(
            label(window, "perm-slack/remove-state"),
            "Delete: it runs after you approve. A delete always asks. It sends no text to other people."
        );
        assert!(window.try_find("perm-slack/remove-tier-autonomous").is_some());
        assert_eq!(
            label(window, "perm-jira/create-orphan"),
            "jira/create: write, draft, text in `summary`"
        );
        assert_eq!(label(window, "permissions-changes"), "");
    });

    // After the stored argument is removed and another one chosen, the stored one is still offered.
    clicks(
        cx,
        window,
        &["perm-slack/post-text-blocks", "perm-slack/post-text-channel"],
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 1 rule\nslack/post: write, approve, text in `blocks` → write, approve, text in `channel`"
        );
    });
    clicks(
        cx,
        window,
        &["perm-slack/post-text-channel", "perm-slack/post-text-blocks"],
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-changes"), "");
    });

    // A save of another tool keeps all three as they are.
    clicks(cx, window, &["perm-slack/search-read"]);
    save(cx, window, &shell);
    let mut expected = before;
    expected.insert("slack/search".into(), ToolRule::read());
    assert_eq!(rt.block_on(rules(&rig)), expected);

    clicks(cx, window, &["perm-jira/create-remove"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 1 rule\njira/create: write, draft, text in `summary` → off"
        );
    });
    save(cx, window, &shell);
    expected.remove("jira/create");
    assert_eq!(rt.block_on(rules(&rig)), expected);
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("perm-jira/create-orphan").is_none());
    });
}

#[gpui_kit::test]
fn a_save_against_changed_rules_asks_before_it_replaces_them(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    clicks(cx, window, &["perm-slack/search-read"]);
    // Another client stores a rule after the view loaded the rules.
    rt.block_on(store(&rig, |rules| {
        rules.insert("slack/post".into(), write(Tier::Draft, text_in("text")));
    }));
    let outside = rt.block_on(rules(&rig));
    wait_until(cx, &shell, "the server's new rules", |s| {
        s.state.permissions.as_ref().is_some_and(|p| p.rules == outside)
    });
    // The changed draft stays.
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "perm-slack/search-state"), READ);
        assert_eq!(label(window, "permissions-conflict"), "");
    });

    save(cx, window, &shell);
    assert_eq!(rt.block_on(rules(&rig)), outside);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-conflict"), PAGE_CONFLICT);
        assert_eq!(label(window, "perm-slack/search-state"), READ);
        assert_eq!(label(window, "permissions-result"), "");
    });

    // "Save anyway" stores the draft over the other client's rules.
    let before = saves(cx, &shell);
    clicks(cx, window, &["permissions-save-anyway"]);
    wait_until(cx, &shell, "the second save", |s| s.state.permission_saves > before);
    let stored = rt.block_on(rules(&rig));
    assert_eq!(stored["slack/search"], ToolRule::read());
    assert_eq!(stored["slack/post"], write(Tier::Approve, text_in("text")));
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-conflict"), "");
        assert_eq!(label(window, "permissions-result"), "Saved");
    });
}

#[gpui_kit::test]
fn load_the_servers_version_drops_the_draft(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    clicks(cx, window, &["perm-slack/search-read"]);
    rt.block_on(store(&rig, |rules| {
        rules.insert("slack/remove".into(), write(Tier::Draft, OutgoingText::None));
    }));
    let outside = rt.block_on(rules(&rig));
    save(cx, window, &shell);
    clicks(cx, window, &["permissions-load"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-conflict"), "");
        assert_eq!(label(window, "perm-slack/search-state"), OFF);
        assert_eq!(
            label(window, "perm-slack/remove-state"),
            "Write: you see the draft. It does not run. It sends no text to other people."
        );
        assert_eq!(label(window, "permissions-changes"), "");
    });
    assert_eq!(rt.block_on(rules(&rig)), outside);
}

#[gpui_kit::test]
fn an_unchanged_view_follows_the_server(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    rt.block_on(store(&rig, |rules| {
        rules.insert("slack/search".into(), ToolRule::read());
    }));
    wait_until(cx, &shell, "the server's new rules", |s| {
        s.state
            .permissions
            .as_ref()
            .is_some_and(|p| p.rules.contains_key("slack/search"))
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "perm-slack/search-state"), READ);
        assert_eq!(label(window, "permissions-changes"), "");
    });
}

#[gpui_kit::test]
fn a_refused_save_shows_the_reason_and_keeps_the_draft(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(async {
        // The profile polls with `slack/search`. A poll needs the read rule of its tool.
        let mut config = rig.engine.profile_config("kim").await.unwrap();
        config.approved_tools.insert("slack".into(), slack_tools());
        config.policy.rules.insert("slack/search".into(), ToolRule::read());
        config.polls.push(PollConfig {
            connector: "slack".into(),
            tool: "search".into(),
            args: json!({"query": "to:me"}).as_object().unwrap().clone(),
            every_secs: 60,
            mapping: PollMapping {
                items: "messages".into(),
                id: "ts".into(),
                text: "text".into(),
                author: "user".into(),
                thread: None,
            },
            skip_authors: vec!["kim".into()],
        });
        rig.engine.set_profile_config("kim", config).await.unwrap();
    });
    let before = rt.block_on(rules(&rig));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    clicks(cx, window, &["perm-slack/search-off"]);
    save(cx, window, &shell);
    assert_eq!(rt.block_on(rules(&rig)), before);
    // The reason stays on screen while the window refreshes.
    let_refreshes_pass(cx);
    on_screen(cx, window, |window, _| {
        let result = label(window, "permissions-result");
        assert!(result.contains("has no permission rule"), "{result}");
        assert_eq!(label(window, "perm-slack/search-state"), OFF);
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 1 rule\nslack/search: read → off"
        );
    });
}

#[gpui_kit::test]
fn save_stays_on_screen_above_a_long_list_of_tools(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let text = json!({"type": "string"});
    let tools: Vec<ToolSpec> = (0..30)
        .map(|i| spec(&format!("tool{i:02}"), "Does one thing", json!({"text": text})))
        .collect();
    rt.block_on(approve(&rig, tools));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    // The list is longer than the window. The first tool and Save can be clicked without a scroll.
    clicks(cx, window, &["perm-slack/tool00-read"]);
    on_screen(cx, window, |window, _| {
        // Save and the list of changes are not inside the list that scrolls.
        let list = window.within("permissions-list");
        assert!(list.try_find("perm-slack/tool29-read").is_some());
        assert!(list.try_find("save-permissions").is_none());
        assert!(list.try_find("permissions-changes").is_none());
        assert!(window.try_find("save-permissions").is_some());
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 1 rule\nslack/tool00: off → read"
        );
        // The rule of `slack/post` has no approved tool now. It is listed before the 30 tools.
        assert_eq!(
            label(window, "perm-slack/post-orphan"),
            "slack/post: write, approve, text in `text`"
        );
    });
    save(cx, window, &shell);
    assert_eq!(rt.block_on(rules(&rig))["slack/tool00"], ToolRule::read());
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-result"), "Saved");
    });
}

#[gpui_kit::test]
fn a_profile_without_approved_tools_says_what_to_do(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(async {
        let mut config = rig.engine.profile_config("kim").await.unwrap();
        config.approved_tools.clear();
        config.policy.rules.clear();
        rig.engine.set_profile_config("kim", config).await.unwrap();
    });
    let (window, _shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-empty"), "no approved tool");
        assert!(window.try_find("save-permissions").is_some());
    });
}

#[gpui_kit::test]
fn another_profile_shows_its_own_rules_and_drops_the_draft(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let kim = rt.block_on(rules(&rig));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    clicks(cx, window, &["perm-slack/search-read", "profile-lee"]);
    wait_until(cx, &shell, "lee's rules", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.permissions.is_some()
    });
    on_screen(cx, window, |window, _| {
        // Lee approved one tool, without typed arguments. Kim's tools and Kim's draft are not here.
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. The AI marker goes into `text`."
        );
        assert!(window.try_find("perm-slack/search-state").is_none());
        assert_eq!(label(window, "permissions-changes"), "");
    });
    // A save in lee's view reaches lee only.
    clicks(cx, window, &["perm-slack/post-tier-draft"]);
    save(cx, window, &shell);
    assert_eq!(rt.block_on(rules(&rig)), kim);
    let lee = rt.block_on(rig.engine.permissions("lee")).unwrap().rules;
    assert_eq!(lee["slack/post"], write(Tier::Draft, text_in("text")));

    // Back at kim, the draft from before is gone.
    clicks(cx, window, &["profile-kim"]);
    wait_until(cx, &shell, "kim's rules", |s| {
        s.state.selected.as_deref() == Some("kim")
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "perm-slack/search-state"), OFF);
        assert_eq!(label(window, "permissions-changes"), "");
    });
}

fn paths(list: &[&str]) -> OutgoingText {
    OutgoingText::Paths(list.iter().map(|path| (*path).to_owned()).collect())
}

#[gpui_kit::test]
fn the_owner_can_choose_several_text_arguments(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    // `text` is chosen. A click on `channel` adds it.
    clicks(cx, window, &["perm-slack/post-text-channel"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. The AI marker goes into `channel`, `text`."
        );
    });
    save(cx, window, &shell);
    assert_eq!(
        rt.block_on(rules(&rig))["slack/post"],
        write(Tier::Approve, paths(&["channel", "text"]))
    );

    // Removing both leaves no choice. The rule is not saved until the owner makes one.
    clicks(
        cx,
        window,
        &["perm-slack/post-text-channel", "perm-slack/post-text-text"],
    );
    let sent = saves(cx, &shell);
    clicks(cx, window, &["save-permissions"]);
    let_refreshes_pass(cx);
    assert_eq!(saves(cx, &shell), sent);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "permissions-message"),
            "Choose where the text for other people is, for: slack/post."
        );
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. Choose where its text for other people is."
        );
    });
    assert_eq!(
        rt.block_on(rules(&rig))["slack/post"],
        write(Tier::Approve, paths(&["channel", "text"]))
    );
}

#[gpui_kit::test]
fn a_stored_choice_with_a_place_inside_a_value_is_shown_and_kept(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(approve(&rig, slack_tools()));
    let nested = write(Tier::Approve, paths(&["text", "blocks.*.text.text"]));
    rt.block_on(store(&rig, |rules| {
        rules.insert("slack/post".into(), nested.clone());
    }));
    let (window, shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. The AI marker goes into `text`, `blocks.*.text.text`."
        );
        assert_eq!(
            label(window, "perm-slack/post-text-stored"),
            "`text`, `blocks.*.text.text`"
        );
    });
    // A save of another tool keeps the stored choice as it is.
    clicks(cx, window, &["perm-slack/search-read"]);
    save(cx, window, &shell);
    assert_eq!(rt.block_on(rules(&rig))["slack/post"], nested);

    // A click on an argument replaces the stored choice. Its button brings it back whole.
    clicks(cx, window, &["perm-slack/post-text-channel"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "permissions-changes"),
            "Not saved: 1 rule\nslack/post: write, approve, text in `text`, `blocks.*.text.text` → \
             write, approve, text in `channel`"
        );
    });
    clicks(cx, window, &["perm-slack/post-text-stored"]);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "permissions-changes"), "");
    });
}

/// Gives `kim` the thresholds of a suggestion.
async fn set_thresholds(rig: &Rig, min_reviewed: u32, min_unchanged_percent: u32) {
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.autonomy = Some(bunsin_core::autonomy::AutonomyThresholds {
        min_reviewed,
        min_unchanged_percent,
        window_days: 30,
    });
    rig.engine.set_profile_config("kim", config).await.unwrap();
}

/// Lets `slack/post` run without asking, and runs one post. Returns its inbox ID.
async fn autonomous_post(rig: &Rig) -> i64 {
    store(rig, |rules| {
        rules.get_mut("slack/post").unwrap().tier = Tier::Autonomous
    })
    .await;
    let call = json!({"kind": "tool_call", "rationale": "asked", "summary": null, "tool": "slack/post",
        "args_json": json!({"channel": "C1", "text": "Done."}).to_string()});
    rig.llm.reply(&call.to_string(), 10);
    let event = bunsin_core::event::InboundEvent {
        source: "slack".into(),
        external_id: "e-auto".into(),
        thread: None,
        author: "lee".into(),
        text: "is it done?".into(),
    };
    match rig.engine.ingest("kim", event).await.unwrap() {
        bunsin_api::Outcome::Executed { inbox_id } => inbox_id,
        other => panic!("expected an autonomous run, got {other:?}"),
    }
}

fn held(shell: &Shell, tool: &str) -> Option<bool> {
    let autonomy = shell.state.autonomy.as_ref()?;
    Some(autonomy.tools.iter().any(|entry| entry.tool == tool && entry.held))
}

#[gpui_kit::test]
fn the_view_shows_the_counts_and_a_suggestion_only_with_thresholds(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(rig.engine.approve("kim", 1, None)).unwrap();
    let (window, shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "perm-slack/post-counts"),
            "All days: 1 reviewed, 1 approved unchanged (100%), 0 edited, 0 dismissed. \
             Ran without asking: 0, failed: 0."
        );
        assert!(label(window, "permissions-thresholds").contains("Suggestions for Autonomous are off"));
        // Without the owner's thresholds nothing is suggested.
        assert!(window.try_find("perm-slack/post-suggested").is_none());
        assert!(window.try_find("perm-slack/post-held").is_none());
    });

    rt.block_on(set_thresholds(&rig, 1, 100));
    wait_until(cx, &shell, "the thresholds", |s| {
        s.state.autonomy.as_ref().is_some_and(|a| a.thresholds.is_some())
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "perm-slack/post-suggested"), "Suggested: Autonomous");
        assert!(label(window, "perm-slack/post-counts").starts_with("Last 30 days: 1 reviewed"));
        // The suggestion changes no step. Only Save does.
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs after you approve. The AI marker goes into `text`."
        );
    });
    assert_eq!(rt.block_on(rules(&rig))["slack/post"].tier, Tier::Approve);
    rt.block_on(set_thresholds(&rig, 2, 100));
    wait_until(cx, &shell, "the new thresholds", |s| {
        s.state
            .autonomy
            .as_ref()
            .is_some_and(|a| a.thresholds.is_some_and(|t| t.min_reviewed == 2))
    });
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("perm-slack/post-suggested").is_none());
    });
}

#[gpui_kit::test]
fn the_owner_sees_a_hold_and_clears_it(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(set_thresholds(&rig, 0, 0));
    let ran = rt.block_on(autonomous_post(&rig));
    rt.block_on(rig.engine.report_autonomous("kim", ran)).unwrap();
    let (window, shell) = open_permissions(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert!(label(window, "perm-slack/post-held").starts_with("Held at approval"));
        // The rule still says autonomous; the hold is not part of the rules.
        assert_eq!(
            label(window, "perm-slack/post-state"),
            "Write: it runs without asking. The AI marker goes into `text`."
        );
        assert!(window.try_find("perm-slack/post-suggested").is_none());
        // The clear button is in the list, the Save bar stays outside it.
        assert!(
            window
                .within("permissions-list")
                .try_find("perm-slack/post-clear-hold")
                .is_some()
        );
        assert!(window.within("permissions-list").try_find("save-permissions").is_none());
    });
    clicks(cx, window, &["perm-slack/post-clear-hold"]);
    wait_until(cx, &shell, "the cleared hold", |s| held(s, "slack/post") == Some(false));
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("perm-slack/post-held").is_none());
    });
    assert!(!rt.block_on(rig.engine.autonomy("kim")).unwrap().tools[0].held);
    // The clearing saved no rule.
    assert_eq!(saves(cx, &shell), 0);
}
