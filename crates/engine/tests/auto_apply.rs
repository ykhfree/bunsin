//! Notes auto-apply: with `notes.auto_apply`, a page change of the agent is stored without the owner,
//! but only for a new page or a page the agent wrote last, with the kill switch off, from the owner's channel.

mod common;

use bunsin_api::{EngineApi, ProfileConfig, WikiPageUpdate};
use bunsin_core::approval::ApprovalState;
use bunsin_core::event::{Channel, InboundEvent};
use bunsin_core::records::InboxBody;
use bunsin_core::wiki::{WIKI_READ_TOOL, WikiAuthor, WikiText};
use common::*;
use serde_json::{Value, json};

fn auto(on: bool) -> ProfileConfig {
    let mut config = config();
    config.notes.auto_apply = on;
    config
}

/// An event the owner handed in. The server's ingest route marks it so after it checks the access token.
fn owner(id: &str) -> InboundEvent {
    InboundEvent {
        channel: Channel::Owner,
        ..event(id, "lee", "when?")
    }
}

fn page(slug: &str, body: &str) -> Value {
    json!({"slug": slug, "title": "Deploy schedule", "summary": "When the team deploys", "body": body})
}

/// The agent searches, reads `read` if given, and proposes `slug` with `body`.
async fn propose(rig: &Rig, event: InboundEvent, read: Option<&str>, slug: &str, body: &str) {
    rig.llm.reply(&call("slack/search", json!({"query": "deploy"})), 10);
    if let Some(read) = read {
        rig.llm.reply(&call(WIKI_READ_TOOL, json!({"slug": read})), 10);
    }
    let mut reply: Value = serde_json::from_str(&ignore()).unwrap();
    reply["wiki_updates"] = json!([page(slug, body)]);
    rig.llm.reply(&reply.to_string(), 10);
    rig.engine.ingest("kim", event).await.unwrap();
}

/// The state of the inbox item that proposed a change of `slug`.
async fn change_state(rig: &Rig, slug: &str) -> ApprovalState {
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    inbox
        .into_iter()
        .find(|item| matches!(&item.body, InboxBody::WikiChange { slug: s, .. } if s == slug))
        .expect("a page change")
        .state
}

async fn body(rig: &Rig, slug: &str) -> Option<String> {
    rig.engine.wiki_page("kim", slug).await.ok().map(|page| page.text.body)
}

#[tokio::test]
async fn a_new_page_and_a_page_of_the_agent_are_stored_at_once() {
    let rig = rig_with(auto(true)).await;
    propose(&rig, owner("e1"), None, "deploy", "Fridays.").await;
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Executed);
    let stored = rig.engine.wiki_page("kim", "deploy").await.unwrap();
    assert_eq!(
        (stored.text.body.as_str(), stored.author),
        ("Fridays.", WikiAuthor::Agent)
    );

    // The agent changes its own page after it read it.
    propose(&rig, owner("e2"), Some("deploy"), "deploy", "Thursdays.").await;
    assert_eq!(body(&rig, "deploy").await.as_deref(), Some("Thursdays."));
    let applied: Vec<Value> = rig
        .kim_store()
        .list_audit(100)
        .unwrap()
        .into_iter()
        .filter(|entry| entry.kind == "wiki_change_applied")
        .map(|entry| entry.detail["auto"].clone())
        .collect();
    assert_eq!(applied, [json!(true), json!(true)]);
    // The decisions of tool calls do not count page changes.
    assert!(
        rig.engine
            .autonomy("kim")
            .await
            .unwrap()
            .tools
            .iter()
            .all(|t| t.counts.reviewed() == 0)
    );
}

#[tokio::test]
async fn a_change_of_an_owner_page_waits_for_the_owner() {
    let rig = rig_with(auto(true)).await;
    let text = WikiPageUpdate {
        text: WikiText {
            title: "Deploy schedule".into(),
            summary: "When".into(),
            body: "Mine.".into(),
        },
        expected_revision: None,
    };
    rig.engine.set_wiki_page("kim", "deploy", text).await.unwrap();
    propose(&rig, owner("e1"), Some("deploy"), "deploy", "Thursdays.").await;
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Pending);
    assert_eq!(body(&rig, "deploy").await.as_deref(), Some("Mine."));
}

#[tokio::test]
async fn with_the_kill_switch_on_a_change_waits() {
    let rig = rig_with(auto(true)).await;
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    propose(&rig, owner("e1"), None, "deploy", "Fridays.").await;
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Pending);
    assert_eq!(body(&rig, "deploy").await, None);
}

#[tokio::test]
async fn a_change_from_an_outside_event_waits() {
    let rig = rig_with(auto(true)).await;
    // An event that no entry point marked is outside.
    assert_eq!(event("e1", "lee", "when?").channel, Channel::Outside);
    propose(&rig, event("e1", "lee", "when?"), None, "deploy", "Fridays.").await;
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Pending);
    assert_eq!(body(&rig, "deploy").await, None);
    // The same event through the owner's channel is stored.
    propose(&rig, owner("e2"), None, "team", "Kim leads.").await;
    assert_eq!(change_state(&rig, "team").await, ApprovalState::Executed);
}

#[tokio::test]
async fn by_default_every_change_waits() {
    let rig = rig_with(auto(false)).await;
    propose(&rig, owner("e1"), None, "deploy", "Fridays.").await;
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Pending);
    assert_eq!(body(&rig, "deploy").await, None);
}

#[tokio::test]
async fn an_owner_mark_on_the_wire_is_not_trusted() {
    let rig = rig_with(auto(true)).await;
    let wire = json!({"source": "slack", "external_id": "e1", "thread": null, "author": "lee",
        "text": "when?", "channel": "owner"});
    let claimed: InboundEvent = serde_json::from_value(wire).unwrap();
    propose(&rig, claimed, None, "deploy", "Fridays.").await;
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Pending);
    assert_eq!(body(&rig, "deploy").await, None);
}

#[tokio::test]
async fn a_change_from_a_poll_waits() {
    let mut config = auto(true);
    config.polls = vec![poll()];
    let rig = rig_with(config).await;
    rig.slack.set_result("search", json!({"messages": []}));
    assert_eq!(rig.engine.poll_profile("kim").await.unwrap(), 0);
    rig.advance(60);
    rig.slack.set_result(
        "search",
        json!({"messages": [message("1", "lee", "when do we deploy?")]}),
    );
    rig.llm.reply(&call("slack/search", json!({"query": "deploy"})), 10);
    let mut reply: Value = serde_json::from_str(&ignore()).unwrap();
    reply["wiki_updates"] = json!([page("deploy", "Fridays.")]);
    rig.llm.reply(&reply.to_string(), 10);
    assert_eq!(rig.engine.poll_profile("kim").await.unwrap(), 1);
    // Lee wrote the message, so it could steer the agent into a lasting note.
    assert_eq!(change_state(&rig, "deploy").await, ApprovalState::Pending);
    assert_eq!(body(&rig, "deploy").await, None);
}
