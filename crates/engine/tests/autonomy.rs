//! Narrow autonomy: the decisions about each tool's calls, their counts, and the hold.

mod common;

use bunsin_api::{Edit, EngineApi, Expected, Outcome, PermissionsUpdate};
use bunsin_core::approval::ApprovalState;
use bunsin_core::policy::Tier;
use bunsin_core::records::{DecisionCounts, InboxBody};
use common::*;
use serde_json::json;

const POST: &str = "slack/post";

async fn ingest(rig: &Rig, id: &str, reply: &str) -> Outcome {
    rig.llm.reply(reply, 10);
    rig.engine.ingest("kim", event(id, "lee", "when?")).await.unwrap()
}

/// A post that waits for the owner.
async fn pending_post(rig: &Rig, id: &str) -> i64 {
    match ingest(rig, id, &post("At 3 pm.")).await {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("expected a pending item, got {other:?}"),
    }
}

/// Sets the step of `slack/post` as the owner does in the permissions view.
async fn set_tier(rig: &Rig, tier: Tier) {
    let mut rules = rig.engine.permissions("kim").await.unwrap().rules;
    rules.get_mut(POST).unwrap().tier = tier;
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    rig.engine.set_permissions("kim", update).await.unwrap();
}

fn counts(rig: &Rig) -> DecisionCounts {
    rig.kim_store()
        .decision_counts(0)
        .unwrap()
        .get(POST)
        .copied()
        .unwrap_or_default()
}

async fn audit_count(rig: &Rig, kind: &str) -> usize {
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    audit.iter().filter(|entry| entry.kind == kind).count()
}

#[tokio::test]
async fn approvals_edits_rejections_and_autonomous_runs_are_counted() {
    let rig = rig().await;
    let approved = pending_post(&rig, "e1").await;
    rig.engine.approve("kim", approved, None).await.unwrap();
    let edited = pending_post(&rig, "e2").await;
    let edit = Edit {
        args: json!({"channel": "C1", "text": "At 4 pm."})
            .as_object()
            .unwrap()
            .clone(),
        reason: "moved".into(),
    };
    rig.engine.approve("kim", edited, Some(edit)).await.unwrap();
    let rejected = pending_post(&rig, "e3").await;
    rig.engine.reject("kim", rejected, "not needed").await.unwrap();
    set_tier(&rig, Tier::Autonomous).await;
    assert!(matches!(
        ingest(&rig, "e4", &post("Done.")).await,
        Outcome::Executed { .. }
    ));

    let expected = DecisionCounts {
        approved: 1,
        edited: 1,
        rejected: 1,
        autonomous: 1,
        autonomous_failed: 0,
    };
    assert_eq!(counts(&rig), expected);
    // A dismissal of an approve-step call holds nothing.
    assert!(!rig.kim_store().is_held(POST).unwrap());

    // A failed autonomous call counts as a failure.
    rig.slack.fail_with("slack is down");
    assert!(matches!(
        ingest(&rig, "e5", &post("Done again.")).await,
        Outcome::ExecutionFailed { .. }
    ));
    let after = counts(&rig);
    assert_eq!((after.autonomous, after.autonomous_failed), (2, 1));
}

#[tokio::test]
async fn drafts_notes_escalations_and_copies_are_not_counted() {
    let rig = rig().await;
    // A dismissed draft never ran and was never asked about.
    set_tier(&rig, Tier::Draft).await;
    let Outcome::Drafted { inbox_id: draft } = ingest(&rig, "e1", &post("At 3 pm.")).await else {
        panic!("expected a draft")
    };
    rig.engine.reject("kim", draft, "no").await.unwrap();
    // An escalation is no call.
    let escalate = json!({"kind": "escalate", "rationale": "money", "summary": "Lee asks about pay.",
        "tool": null, "args_json": null});
    let Outcome::Escalated { inbox_id: escalation } = ingest(&rig, "e2", &escalate.to_string()).await else {
        panic!("expected an escalation")
    };
    rig.engine.reject("kim", escalation, "seen").await.unwrap();
    // A page change is no call either.
    rig.slack
        .set_result("search", json!({"messages": [{"text": "We deploy on Fridays."}]}));
    let page = json!({"slug": "deploy", "title": "Deploy", "summary": "When", "body": "Fridays."});
    let mut reply: serde_json::Value = serde_json::from_str(&ignore()).unwrap();
    reply["wiki_updates"] = json!([page]);
    rig.llm
        .reply(&call("slack/search", json!({"query": "deploy"})), 10)
        .reply(&reply.to_string(), 10);
    rig.engine.ingest("kim", event("e3", "lee", "deploy?")).await.unwrap();
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let note = inbox
        .iter()
        .find(|item| matches!(item.body, InboxBody::WikiChange { .. }))
        .unwrap()
        .id;
    rig.engine.approve("kim", note, None).await.unwrap();
    assert!(rig.kim_store().decision_counts(0).unwrap().is_empty());

    // A copy repeats an item whose approval is counted already.
    set_tier(&rig, Tier::Approve).await;
    let first = pending_post(&rig, "e4").await;
    rig.engine.approve("kim", first, None).await.unwrap();
    let copy = rig
        .kim_store()
        .fail_with_pending_copy(first, ApprovalState::Executed, 1)
        .unwrap()
        .unwrap();
    rig.engine.approve("kim", copy.id, None).await.unwrap();
    assert_eq!(counts(&rig).approved, 1);
}

#[tokio::test]
async fn dismissing_a_call_of_an_autonomous_tool_holds_it_once() {
    let rig = rig().await;
    // Two calls wait from before the owner let the tool run without asking.
    let first = pending_post(&rig, "e1").await;
    let second = pending_post(&rig, "e2").await;
    let approved = pending_post(&rig, "e3").await;
    rig.engine.approve("kim", approved, None).await.unwrap();
    set_tier(&rig, Tier::Autonomous).await;

    rig.engine.reject("kim", first, "wrong").await.unwrap();
    assert!(rig.kim_store().is_held(POST).unwrap());
    assert_eq!(audit_count(&rig, "autonomy_demoted").await, 1);
    // The hold starts the counts again.
    assert_eq!(counts(&rig), DecisionCounts::default());

    // A dismissal while the tool is held writes no second hold, but it is counted.
    rig.engine.reject("kim", second, "wrong too").await.unwrap();
    assert_eq!(audit_count(&rig, "autonomy_demoted").await, 1);
    assert_eq!(counts(&rig).rejected, 1);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let demoted = audit.iter().find(|entry| entry.kind == "autonomy_demoted").unwrap();
    assert_eq!(
        demoted.detail,
        json!({"tool": POST, "inbox_id": first, "cause": "rejected"})
    );
}
