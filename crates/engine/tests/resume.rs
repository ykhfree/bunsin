//! A start after a stop: cut calls become `unknown`, approved calls run once or go back to the owner.

mod common;

use bunsin_api::{ApiError, EngineApi, Outcome, Resolution};
use bunsin_core::approval::ApprovalState;
use bunsin_core::policy::Tier;
use bunsin_core::records::InboxItem;
use bunsin_testkit::tool;
use common::*;
use serde_json::json;

/// A pending post that the owner sees in the inbox.
async fn pending_post(rig: &Rig, id: &str) -> i64 {
    rig.llm.reply(&post("At 3 pm."), 10);
    match rig.engine.ingest("kim", event(id, "lee", "when?")).await.unwrap() {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("expected a pending item, got {other:?}"),
    }
}

/// Moves an item as a request did before the stop cut it.
fn force_state(rig: &Rig, inbox_id: i64, from: ApprovalState, to: ApprovalState) {
    assert!(rig.kim_store().move_inbox_state(inbox_id, from, to, 1).unwrap());
}

/// An item the policy approved without the owner, as `handle_tool_call` stores it.
fn autonomous_post(rig: &Rig) -> i64 {
    let body = bunsin_core::records::InboxBody::ToolCall {
        tool: "slack/post".into(),
        args: json!({"channel": "C1", "text": format!("Done. {MARKER}")})
            .as_object()
            .unwrap()
            .clone(),
    };
    rig.kim_store()
        .add_inbox_item("k-auto", &body, "asked", &[], ApprovalState::Approved, 1)
        .unwrap()
        .id
}

async fn item(rig: &Rig, inbox_id: i64) -> InboxItem {
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    snapshot.inbox.into_iter().find(|i| i.id == inbox_id).unwrap()
}

async fn audit_kinds(rig: &Rig) -> Vec<String> {
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    snapshot.audit.into_iter().rev().map(|e| e.kind).collect()
}

async fn start(rig: &Rig) {
    let interrupted = rig.engine.mark_interrupted();
    assert!(interrupted.failures.is_empty());
    let failures = rig.engine.resume_approved(interrupted).await;
    assert!(failures.is_empty(), "{failures:?}");
}

async fn set_rule_tier(rig: &Rig, tier: Tier) {
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.policy.rules.get_mut("slack/post").unwrap().tier = tier;
    rig.engine.set_profile_config("kim", config).await.unwrap();
}

#[tokio::test]
async fn a_cut_call_becomes_unknown_and_is_never_run_again() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    force_state(&rig, id, ApprovalState::Pending, ApprovalState::Approved);
    force_state(&rig, id, ApprovalState::Approved, ApprovalState::Executing);
    let rig = rig.restart();
    start(&rig).await;

    assert_eq!(item(&rig, id).await.state, ApprovalState::Unknown);
    assert!(rig.slack.calls().is_empty());
    assert!(audit_kinds(&rig).await.contains(&"outcome_unknown".to_owned()));
    // A second start leaves it unknown and still runs nothing.
    let rig = rig.restart();
    start(&rig).await;
    assert_eq!(item(&rig, id).await.state, ApprovalState::Unknown);
    assert!(rig.slack.calls().is_empty());
    // The owner cannot approve it again.
    assert!(rig.engine.approve("kim", id, None).await.is_err());
    assert!(rig.slack.calls().is_empty());
}

#[tokio::test]
async fn an_approved_call_that_never_started_runs_once() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    force_state(&rig, id, ApprovalState::Pending, ApprovalState::Approved);
    let rig = rig.restart();
    start(&rig).await;

    assert_eq!(item(&rig, id).await.state, ApprovalState::Executed);
    assert_eq!(rig.slack.calls().len(), 1);
    let kinds = audit_kinds(&rig).await;
    let resumed = kinds.iter().position(|k| k == "resumed").unwrap();
    let started = kinds.iter().position(|k| k == "execution_started").unwrap();
    let executed = kinds.iter().position(|k| k == "executed").unwrap();
    assert!(resumed < started && started < executed, "{kinds:?}");
    // The next start finds nothing to run.
    let rig = rig.restart();
    start(&rig).await;
    assert_eq!(rig.slack.calls().len(), 1);
}

#[tokio::test]
async fn the_kill_switch_sends_an_approved_call_back_to_the_owner_as_a_copy() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    force_state(&rig, id, ApprovalState::Pending, ApprovalState::Approved);
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    let rig = rig.restart();
    start(&rig).await;

    assert!(rig.slack.calls().is_empty());
    assert_eq!(item(&rig, id).await.state, ApprovalState::Failed);
    let copy = rig
        .engine
        .snapshot("kim")
        .await
        .unwrap()
        .inbox
        .into_iter()
        .find(|i| i.retry_of == Some(id))
        .unwrap();
    assert_eq!(copy.state, ApprovalState::Pending);
    assert_eq!(copy.body, item(&rig, id).await.body);
    assert!(audit_kinds(&rig).await.contains(&"resume_abandoned".to_owned()));

    // The approval of the copy checks the policy again.
    assert!(rig.engine.approve("kim", copy.id, None).await.is_err());
    rig.engine.set_kill_switch("kim", false).await.unwrap();
    let outcome = rig.engine.approve("kim", copy.id, None).await.unwrap();
    assert_eq!(outcome, Outcome::Executed { inbox_id: copy.id });
    assert_eq!(rig.slack.calls().len(), 1);
}

#[tokio::test]
async fn a_changed_tool_sends_an_approved_call_back_to_the_owner() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    force_state(&rig, id, ApprovalState::Pending, ApprovalState::Approved);
    // The MCP server now describes `post` differently than when the owner approved it.
    rig.slack
        .set_tools(vec![tool("search", "Search messages"), tool("post", "Post anywhere")]);
    let rig = rig.restart();
    start(&rig).await;

    assert!(rig.slack.calls().is_empty());
    assert_eq!(item(&rig, id).await.state, ApprovalState::Failed);
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    assert!(
        inbox
            .iter()
            .any(|i| i.retry_of == Some(id) && i.state == ApprovalState::Pending)
    );
}

#[tokio::test]
async fn an_autonomous_call_runs_only_while_its_rule_is_still_autonomous() {
    let rig = rig().await;
    set_rule_tier(&rig, Tier::Autonomous).await;
    let runs = autonomous_post(&rig);
    let rig = rig.restart();
    start(&rig).await;
    assert_eq!(item(&rig, runs).await.state, ApprovalState::Executed);
    assert_eq!(rig.slack.calls().len(), 1);

    // The owner lowered the rule to `approve`: the policy no longer runs it alone.
    set_rule_tier(&rig, Tier::Approve).await;
    let waits = autonomous_post(&rig);
    let rig = rig.restart();
    start(&rig).await;
    assert_eq!(rig.slack.calls().len(), 1);
    assert_eq!(item(&rig, waits).await.state, ApprovalState::Failed);
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let copy = inbox.iter().find(|i| i.retry_of == Some(waits)).unwrap();
    assert_eq!((copy.state, copy.autonomous), (ApprovalState::Pending, false));
}

#[tokio::test]
async fn a_removed_rule_sends_an_owner_approved_call_back() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    force_state(&rig, id, ApprovalState::Pending, ApprovalState::Approved);
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.policy.rules.remove("slack/post");
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let rig = rig.restart();
    start(&rig).await;
    assert!(rig.slack.calls().is_empty());
    assert_eq!(item(&rig, id).await.state, ApprovalState::Failed);
}

#[tokio::test]
async fn an_item_approved_after_the_check_is_left_to_its_own_request() {
    let rig = rig().await;
    let before = pending_post(&rig, "e1").await;
    force_state(&rig, before, ApprovalState::Pending, ApprovalState::Approved);
    let rig = rig.restart();
    let interrupted = rig.engine.mark_interrupted();
    // A request after the start approves another item. Its own request runs it, not the resume.
    let after = autonomous_post(&rig);
    let failures = rig.engine.resume_approved(interrupted).await;
    assert!(failures.is_empty(), "{failures:?}");

    assert_eq!(item(&rig, before).await.state, ApprovalState::Executed);
    let after = item(&rig, after).await;
    assert_eq!(after.state, ApprovalState::Approved);
    assert_eq!(rig.slack.calls().len(), 1);
}

/// A rig after a restart, with one item that the stop cut while it ran.
async fn unknown_item() -> (Rig, i64) {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    force_state(&rig, id, ApprovalState::Pending, ApprovalState::Approved);
    force_state(&rig, id, ApprovalState::Approved, ApprovalState::Executing);
    let rig = rig.restart();
    start(&rig).await;
    assert_eq!(item(&rig, id).await.state, ApprovalState::Unknown);
    (rig, id)
}

#[tokio::test]
async fn done_and_drop_close_an_unknown_item_without_a_call() {
    for (resolution, state) in [
        (Resolution::Done, ApprovalState::Executed),
        (Resolution::Drop, ApprovalState::Failed),
    ] {
        let (rig, id) = unknown_item().await;
        let outcome = rig.engine.resolve_unknown("kim", id, resolution).await.unwrap();
        assert_eq!(outcome, None);
        assert_eq!(item(&rig, id).await.state, state, "{resolution:?}");
        assert!(rig.slack.calls().is_empty());
        assert_eq!(rig.engine.snapshot("kim").await.unwrap().inbox.len(), 1);
        assert!(audit_kinds(&rig).await.contains(&"unknown_resolved".to_owned()));
        // It is resolved once.
        let again = rig.engine.resolve_unknown("kim", id, Resolution::Retry).await;
        assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
        assert!(rig.slack.calls().is_empty());
    }
}

#[tokio::test]
async fn a_retry_runs_a_copy_once_under_a_new_id() {
    let (rig, id) = unknown_item().await;
    let outcome = rig.engine.resolve_unknown("kim", id, Resolution::Retry).await.unwrap();
    let Some(Outcome::Executed { inbox_id: copy }) = outcome else {
        panic!("expected an executed copy, got {outcome:?}")
    };
    assert_ne!(copy, id);
    assert_eq!(item(&rig, id).await.state, ApprovalState::Failed);
    let copy = item(&rig, copy).await;
    assert_eq!((copy.state, copy.retry_of), (ApprovalState::Executed, Some(id)));
    assert_eq!(rig.slack.calls().len(), 1);
    assert_eq!(rig.slack.calls()[0].1["text"], json!(format!("At 3 pm.\n\n{MARKER}")));
    // A second retry of the same item finds it failed and sends nothing.
    let again = rig.engine.resolve_unknown("kim", id, Resolution::Retry).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
    assert_eq!(rig.slack.calls().len(), 1);
}

#[tokio::test]
async fn a_retry_that_the_policy_refuses_leaves_a_pending_copy() {
    let (rig, id) = unknown_item().await;
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    let refused = rig.engine.resolve_unknown("kim", id, Resolution::Retry).await;
    let Err(ApiError::NotAllowed(error)) = refused else {
        panic!("expected a refusal, got {refused:?}")
    };
    assert!(rig.slack.calls().is_empty());
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let copy = inbox.iter().find(|i| i.retry_of == Some(id)).unwrap();
    assert_eq!(copy.state, ApprovalState::Pending);
    assert!(error.contains(&format!("item {}", copy.id)), "{error}");
    assert_eq!(item(&rig, id).await.state, ApprovalState::Failed);
}

#[tokio::test]
async fn only_an_unknown_item_can_be_resolved() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    for resolution in [Resolution::Done, Resolution::Retry, Resolution::Drop] {
        let result = rig.engine.resolve_unknown("kim", id, resolution).await;
        assert!(
            matches!(&result, Err(ApiError::NotAllowed(e)) if e.contains("pending")),
            "{resolution:?}: {result:?}"
        );
    }
    assert_eq!(item(&rig, id).await.state, ApprovalState::Pending);
    assert_eq!(rig.engine.snapshot("kim").await.unwrap().inbox.len(), 1);
    assert!(rig.slack.calls().is_empty());
}
