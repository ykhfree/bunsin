//! The permission table as the window reads and replaces it.

mod common;

use bunsin_api::{ApiError, EngineApi, Expected, Outcome, PermissionsUpdate};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use common::*;
use serde_json::json;

fn write(text: OutgoingText) -> ToolRule {
    ToolRule::write(ToolClass::Write, text)
}

fn text_in(arg: &str) -> OutgoingText {
    OutgoingText::Arg(arg.into())
}

async fn replace(rig: &Rig, change: impl FnOnce(&mut std::collections::BTreeMap<String, ToolRule>)) {
    let mut rules = rig.engine.permissions("kim").await.unwrap().rules;
    change(&mut rules);
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    rig.engine.set_permissions("kim", update).await.unwrap();
}

#[tokio::test]
async fn the_table_is_read_with_the_approved_tools_and_replaced_as_a_whole() {
    let rig = rig().await;
    let before = rig.engine.permissions("kim").await.unwrap();
    assert_eq!(before.tools["slack"], slack_tools());
    assert_eq!(before.rules["slack/search"], ToolRule::read());
    assert_eq!(before.rules["slack/post"], write(text_in("text")));
    // Reading the table connects to nothing.
    assert!(rig.slack.calls().is_empty());

    // A rule that is left out is removed. A changed one is stored as it is.
    let mut rules = before.rules.clone();
    rules.remove("slack/search");
    rules.insert(
        "slack/post".into(),
        ToolRule {
            class: ToolClass::Write,
            tier: Tier::Draft,
            outgoing_text: text_in("text"),
        },
    );
    let update = PermissionsUpdate {
        rules: rules.clone(),
        expected: Expected::Value(before.rules.clone()),
    };
    rig.engine.set_permissions("kim", update).await.unwrap();
    assert_eq!(rig.engine.permissions("kim").await.unwrap().rules, rules);
    // The other settings are as they were.
    assert_eq!(
        rig.engine.profile_config("kim").await.unwrap().approved_tools["slack"],
        slack_tools()
    );

    // The log names each changed tool with its rule before and after.
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert_eq!(audit[0].kind, "permissions_changed");
    assert_eq!(
        audit[0].detail,
        json!({"changes": [
            {"tool": "slack/post",
             "from": {"class": "write", "tier": "approve", "outgoing_text": {"arg": "text"}},
             "to": {"class": "write", "tier": "draft", "outgoing_text": {"arg": "text"}}},
            {"tool": "slack/search",
             "from": {"class": "read", "tier": "autonomous", "outgoing_text": "none"},
             "to": null}
        ]})
    );

    // A save that started from the earlier table is refused, and nothing changes.
    let stale = PermissionsUpdate {
        rules: before.rules.clone(),
        expected: Expected::Value(before.rules),
    };
    let error = rig.engine.set_permissions("kim", stale).await.unwrap_err();
    assert!(matches!(error, ApiError::Conflict(_)), "{error:?}");
    assert_eq!(rig.engine.permissions("kim").await.unwrap().rules, rules);
}

#[tokio::test]
async fn a_table_that_takes_the_read_rule_from_a_poll_is_refused() {
    let mut config = config();
    config.polls.push(poll());
    let rig = rig_with(config).await;
    let before = rig.engine.permissions("kim").await.unwrap().rules;
    let mut rules = before.clone();
    rules.remove("slack/search");
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    let error = rig.engine.set_permissions("kim", update).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("has no permission rule")),
        "{error:?}"
    );
    assert_eq!(rig.engine.permissions("kim").await.unwrap().rules, before);
}

/// Handles an event for which the agent proposes a post. Returns the ID of the waiting item.
async fn waiting_post(rig: &Rig) -> i64 {
    rig.llm.reply(&post("At 3 pm."), 10);
    match rig.engine.ingest("kim", event("e1", "lee", "when?")).await.unwrap() {
        Outcome::PendingApproval { inbox_id, .. } => inbox_id,
        other => panic!("expected a pending item, got {other:?}"),
    }
}

#[tokio::test]
async fn a_waiting_item_gets_the_ai_marker_of_the_rule_that_is_in_force_at_approval() {
    let rig = rig().await;
    // The owner first said that the tool sends no text to other people.
    replace(&rig, |rules| {
        drop(rules.insert("slack/post".into(), write(OutgoingText::None)))
    })
    .await;
    let item = waiting_post(&rig).await;
    // Then the owner corrects the rule. The waiting item was made without the marker.
    replace(&rig, |rules| {
        drop(rules.insert("slack/post".into(), write(text_in("text"))))
    })
    .await;
    rig.engine.approve("kim", item, None).await.unwrap();
    let (_, args) = rig.slack.calls().last().cloned().unwrap();
    let sent = args["text"].as_str().unwrap();
    assert!(
        sent.starts_with("At 3 pm.") && sent.contains("AI agent (Bunsin) acting for Kim"),
        "{sent}"
    );
}

#[tokio::test]
async fn a_waiting_item_is_not_sent_if_the_rule_now_names_text_that_it_does_not_have() {
    let rig = rig().await;
    let item = waiting_post(&rig).await;
    // The rule now names an argument that this call does not have. The marker cannot be added.
    replace(&rig, |rules| {
        drop(rules.insert("slack/post".into(), write(text_in("body"))))
    })
    .await;
    let error = rig.engine.approve("kim", item, None).await.unwrap_err();
    assert!(
        matches!(&error, ApiError::Invalid(m) if m.contains("`body`")),
        "{error:?}"
    );
    assert!(rig.slack.calls().is_empty());
}

#[tokio::test]
async fn a_waiting_item_is_not_run_after_its_tool_lost_its_rule() {
    let rig = rig().await;
    let item = waiting_post(&rig).await;
    replace(&rig, |rules| drop(rules.remove("slack/post"))).await;
    let error = rig.engine.approve("kim", item, None).await.unwrap_err();
    assert!(matches!(error, ApiError::NotAllowed(_)), "{error:?}");
    assert!(rig.slack.calls().is_empty());
}

fn paths(list: &[&str]) -> OutgoingText {
    OutgoingText::Paths(list.iter().map(|path| (*path).to_owned()).collect())
}

fn marked(text: &str) -> String {
    format!("{text}\n\n{MARKER}")
}

/// A post with a fallback text and two blocks of text.
fn post_with_blocks(blocks: &[&str]) -> serde_json::Value {
    let blocks: Vec<_> = blocks
        .iter()
        .map(|text| json!({"type": "section", "text": {"type": "mrkdwn", "text": text}}))
        .collect();
    json!({"channel": "C1", "text": "Fallback", "blocks": blocks})
}

#[tokio::test]
async fn every_place_of_text_gets_the_marker_at_proposal_and_again_at_approval() {
    let rig = rig().await;
    replace(&rig, |rules| {
        drop(rules.insert("slack/post".into(), write(paths(&["text", "blocks.*.text.text"]))))
    })
    .await;
    rig.llm
        .reply(&call("slack/post", post_with_blocks(&["One", "Two"])), 10);
    let item = match rig.engine.ingest("kim", event("e1", "lee", "news?")).await.unwrap() {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("expected a pending item, got {other:?}"),
    };
    // The inbox shows the marks before the owner approves.
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    let bunsin_core::records::InboxBody::ToolCall { args, .. } = &snapshot.inbox[0].body else {
        panic!("expected a tool call")
    };
    assert_eq!(args["text"], marked("Fallback"));
    assert_eq!(args["blocks"][0]["text"]["text"], marked("One"));
    assert_eq!(args["blocks"][1]["text"]["text"], marked("Two"));

    // The owner removes the marks and adds a block. Each place is marked again.
    let edited = post_with_blocks(&["Only this", "And this"]);
    let edit = bunsin_api::Edit {
        args: edited.as_object().unwrap().clone(),
        reason: "shorter".into(),
    };
    rig.engine.approve("kim", item, Some(edit)).await.unwrap();
    let (_, sent) = rig.slack.calls().last().cloned().unwrap();
    assert_eq!(sent["text"], marked("Fallback"));
    assert_eq!(sent["blocks"][0]["text"]["text"], marked("Only this"));
    assert_eq!(sent["blocks"][1]["text"]["text"], marked("And this"));
    assert_eq!(sent["blocks"][1]["text"]["type"], "mrkdwn");
}

#[tokio::test]
async fn a_call_without_one_of_the_places_is_not_queued() {
    let rig = rig().await;
    replace(&rig, |rules| {
        drop(rules.insert("slack/post".into(), write(paths(&["text", "blocks.*.text.text"]))))
    })
    .await;
    // The second block has its text in another place than the rule names.
    let mut args = post_with_blocks(&["One"]);
    args["blocks"]
        .as_array_mut()
        .unwrap()
        .push(json!({"type": "context", "elements": [{"type": "mrkdwn", "text": "unmarked"}]}));
    rig.llm.reply(&call("slack/post", args), 10);
    let outcome = rig.engine.ingest("kim", event("e1", "lee", "news?")).await.unwrap();
    assert!(
        matches!(&outcome, Outcome::Failed { error } if error.contains("`blocks.*.text.text`")),
        "{outcome:?}"
    );
    assert!(rig.engine.snapshot("kim").await.unwrap().inbox.is_empty());
    assert!(rig.slack.calls().is_empty());
}

#[tokio::test]
async fn a_rule_that_now_names_more_places_covers_a_waiting_item() {
    let rig = rig().await;
    // The item is made while the rule names only `text`.
    rig.llm.reply(&call("slack/post", post_with_blocks(&["One"])), 10);
    let item = match rig.engine.ingest("kim", event("e1", "lee", "news?")).await.unwrap() {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("expected a pending item, got {other:?}"),
    };
    replace(&rig, |rules| {
        drop(rules.insert("slack/post".into(), write(paths(&["text", "blocks.*.text.text"]))))
    })
    .await;
    rig.engine.approve("kim", item, None).await.unwrap();
    let (_, sent) = rig.slack.calls().last().cloned().unwrap();
    // `text` keeps one marker. The block gets its own.
    assert_eq!(sent["text"], marked("Fallback"));
    assert_eq!(sent["blocks"][0]["text"]["text"], marked("One"));
}

#[tokio::test]
async fn a_rule_whose_paths_name_no_place_is_refused() {
    let rig = rig().await;
    let before = rig.engine.permissions("kim").await.unwrap().rules;
    for bad in [paths(&[]), paths(&["text", "blocks..text"]), paths(&["*.text"])] {
        let mut rules = before.clone();
        rules.insert("slack/post".into(), write(bad.clone()));
        let update = PermissionsUpdate {
            rules,
            expected: Expected::Unchecked,
        };
        let error = rig.engine.set_permissions("kim", update).await.unwrap_err();
        assert!(
            matches!(&error, ApiError::Invalid(m) if m.contains("the rule of `slack/post`")),
            "{bad:?}: {error:?}"
        );
    }
    // The whole settings file is checked the same way.
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.policy.rules.insert("slack/post".into(), write(paths(&[])));
    assert!(matches!(
        rig.engine.set_profile_config("kim", config).await,
        Err(ApiError::Invalid(_))
    ));
    assert_eq!(rig.engine.permissions("kim").await.unwrap().rules, before);
}
