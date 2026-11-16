//! Narrow autonomy: the decisions about each tool's calls, their counts, and the hold.

mod common;

use bunsin_api::{ApiError, AutonomyThresholdsUpdate, Edit, EngineApi, Expected, Outcome, PermissionsUpdate};
use bunsin_core::approval::ApprovalState;
use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
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

/// A post that ran without asking.
async fn autonomous_run(rig: &Rig, id: &str) -> i64 {
    match ingest(rig, id, &post("Done.")).await {
        Outcome::Executed { inbox_id } => inbox_id,
        other => panic!("expected an autonomous run, got {other:?}"),
    }
}

#[tokio::test]
async fn a_report_holds_the_tool_and_its_next_call_waits() {
    let rig = rig().await;
    set_tier(&rig, Tier::Autonomous).await;
    let ran = autonomous_run(&rig, "e1").await;
    rig.engine.report_autonomous("kim", ran).await.unwrap();
    assert!(matches!(
        ingest(&rig, "e2", &post("Again.")).await,
        Outcome::PendingApproval { .. }
    ));
    assert_eq!(rig.slack.calls().len(), 1);
    // The step stays as the owner set it. Only the hold keeps the tool at approval.
    let rules = rig.engine.permissions("kim").await.unwrap().rules;
    assert_eq!(rules[POST].tier, Tier::Autonomous);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let demoted = audit.iter().find(|entry| entry.kind == "autonomy_demoted").unwrap();
    assert_eq!(
        demoted.detail,
        json!({"tool": POST, "inbox_id": ran, "cause": "reported"})
    );
    assert_eq!(audit_count(&rig, "autonomous_reported").await, 1);

    // An item is reported once.
    let again = rig.engine.report_autonomous("kim", ran).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
}

#[tokio::test]
async fn only_a_finished_autonomous_call_can_be_reported() {
    let rig = rig().await;
    let waiting = pending_post(&rig, "e1").await;
    let refused = rig.engine.report_autonomous("kim", waiting).await;
    assert!(matches!(refused, Err(ApiError::NotAllowed(ref e)) if e.contains("did not run without you")));
    rig.engine.approve("kim", waiting, None).await.unwrap();
    // The owner approved it, so it did not run without the owner.
    assert!(rig.engine.report_autonomous("kim", waiting).await.is_err());
    // An autonomous call that has not run yet cannot be reported.
    let body = InboxBody::ToolCall {
        tool: POST.into(),
        args: json!({"channel": "C1", "text": "x"}).as_object().unwrap().clone(),
    };
    let approved = rig
        .kim_store()
        .add_inbox_item("k", &body, "asked", &[], ApprovalState::Approved, 1)
        .unwrap();
    let refused = rig.engine.report_autonomous("kim", approved.id).await;
    assert!(matches!(refused, Err(ApiError::NotAllowed(ref e)) if e.contains("approved")));
    assert!(!rig.kim_store().is_held(POST).unwrap());
    assert!(matches!(
        rig.engine.report_autonomous("kim", 999).await,
        Err(ApiError::NotFound(_))
    ));
}

#[tokio::test]
async fn a_dismissed_call_of_an_autonomous_tool_makes_its_next_call_wait() {
    let rig = rig().await;
    let waiting = pending_post(&rig, "e1").await;
    set_tier(&rig, Tier::Autonomous).await;
    rig.engine.reject("kim", waiting, "wrong").await.unwrap();
    assert!(matches!(
        ingest(&rig, "e2", &post("Again.")).await,
        Outcome::PendingApproval { .. }
    ));
    assert!(rig.slack.calls().is_empty());
}

#[tokio::test]
async fn a_hold_survives_a_saved_settings_file_and_only_its_clearing_ends_it() {
    let rig = rig().await;
    set_tier(&rig, Tier::Autonomous).await;
    let ran = autonomous_run(&rig, "e1").await;
    // A settings file read before the report.
    let stale = rig.engine.profile_config("kim").await.unwrap();
    rig.engine.report_autonomous("kim", ran).await.unwrap();

    rig.engine.set_profile_config("kim", stale).await.unwrap();
    assert!(matches!(
        ingest(&rig, "e2", &post("Again.")).await,
        Outcome::PendingApproval { .. }
    ));
    // "Save anyway" of a permissions form with the step `autonomous`.
    set_tier(&rig, Tier::Autonomous).await;
    assert!(matches!(
        ingest(&rig, "e3", &post("Again.")).await,
        Outcome::PendingApproval { .. }
    ));
    assert!(rig.engine.autonomy("kim").await.unwrap().tools[0].held);

    rig.engine.clear_autonomy_hold("kim", POST).await.unwrap();
    autonomous_run(&rig, "e4").await;
    assert_eq!(audit_count(&rig, "autonomy_hold_cleared").await, 1);
    let again = rig.engine.clear_autonomy_hold("kim", POST).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(ref e)) if e.contains("not held")));
}

async fn set_thresholds(rig: &Rig, thresholds: AutonomyThresholds) {
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.autonomy = Some(thresholds);
    rig.engine.set_profile_config("kim", config).await.unwrap();
}

#[tokio::test]
async fn autonomy_lists_counts_holds_and_suggestions() {
    let rig = rig().await;
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.policy.rules.insert(
        "slack/remove".into(),
        ToolRule::write(ToolClass::Delete, OutgoingText::None),
    );
    rig.engine.set_profile_config("kim", config).await.unwrap();
    for id in ["e1", "e2", "e3"] {
        let waiting = pending_post(&rig, id).await;
        rig.engine.approve("kim", waiting, None).await.unwrap();
    }
    let waiting = pending_post(&rig, "e4").await;
    rig.engine.reject("kim", waiting, "no").await.unwrap();

    // Without thresholds nothing is suggested. The read rule is not listed.
    let autonomy = rig.engine.autonomy("kim").await.unwrap();
    assert_eq!(autonomy.thresholds, None);
    let tools: Vec<&str> = autonomy.tools.iter().map(|t| t.tool.as_str()).collect();
    assert_eq!(tools, ["slack/post", "slack/remove"]);
    let post_entry = &autonomy.tools[0];
    assert_eq!((post_entry.counts.approved, post_entry.counts.rejected), (3, 1));
    assert!(!post_entry.suggested && !post_entry.held);

    let thresholds = |min_unchanged_percent| AutonomyThresholds {
        min_reviewed: 4,
        min_unchanged_percent,
        window_days: 7,
    };
    set_thresholds(&rig, thresholds(76)).await;
    assert!(!rig.engine.autonomy("kim").await.unwrap().tools[0].suggested);
    set_thresholds(&rig, thresholds(75)).await;
    let autonomy = rig.engine.autonomy("kim").await.unwrap();
    assert!(autonomy.tools[0].suggested);
    assert!(!autonomy.tools[1].suggested);
    // The engine never raises the step itself.
    assert_eq!(
        rig.engine.permissions("kim").await.unwrap().rules[POST].tier,
        Tier::Approve
    );

    // Decisions older than the window do not count.
    rig.advance(8 * 24 * 60 * 60);
    let autonomy = rig.engine.autonomy("kim").await.unwrap();
    assert_eq!(autonomy.tools[0].counts.reviewed(), 0);
    assert!(!autonomy.tools[0].suggested);

    // Thresholds that count nothing are refused.
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.autonomy = Some(AutonomyThresholds {
        window_days: 0,
        ..thresholds(75)
    });
    let refused = rig.engine.set_profile_config("kim", config).await;
    assert!(matches!(refused, Err(ApiError::Invalid(ref e)) if e.contains("window_days")));
}

#[tokio::test]
async fn a_held_tool_is_listed_without_its_rule_and_not_suggested() {
    let rig = rig().await;
    set_tier(&rig, Tier::Autonomous).await;
    let ran = autonomous_run(&rig, "e1").await;
    rig.engine.report_autonomous("kim", ran).await.unwrap();
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.policy.rules.remove(POST);
    config.autonomy = Some(AutonomyThresholds {
        min_reviewed: 0,
        min_unchanged_percent: 0,
        window_days: 30,
    });
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let autonomy = rig.engine.autonomy("kim").await.unwrap();
    assert_eq!(autonomy.tools.len(), 1);
    assert_eq!(autonomy.tools[0].tool, POST);
    assert!(autonomy.tools[0].held && !autonomy.tools[0].suggested);
}

#[tokio::test]
async fn the_thresholds_are_saved_alone_with_their_check_and_a_conflict_check() {
    let rig = rig().await;
    assert_eq!(rig.engine.autonomy_thresholds("kim").await.unwrap(), None);
    let thresholds = AutonomyThresholds {
        min_reviewed: 20,
        min_unchanged_percent: 90,
        window_days: 30,
    };
    let update = |thresholds, expected| AutonomyThresholdsUpdate {
        thresholds,
        expected: Expected::Value(expected),
    };
    rig.engine
        .set_autonomy_thresholds("kim", update(Some(thresholds), None))
        .await
        .unwrap();
    assert_eq!(rig.engine.autonomy_thresholds("kim").await.unwrap(), Some(thresholds));
    assert_eq!(rig.engine.autonomy("kim").await.unwrap().thresholds, Some(thresholds));
    // The rest of the settings stays.
    let config = rig.engine.profile_config("kim").await.unwrap();
    assert_eq!(config.policy.rules.len(), 2);

    // The caller loaded no thresholds, but the server holds some now.
    let stale = rig.engine.set_autonomy_thresholds("kim", update(None, None)).await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    let empty = AutonomyThresholds {
        window_days: 0,
        ..thresholds
    };
    let refused = rig
        .engine
        .set_autonomy_thresholds("kim", update(Some(empty), Some(thresholds)))
        .await;
    assert!(
        matches!(refused, Err(ApiError::Invalid(ref e)) if e.contains("window_days")),
        "{refused:?}"
    );
    rig.engine
        .set_autonomy_thresholds("kim", update(None, Some(thresholds)))
        .await
        .unwrap();
    assert_eq!(rig.engine.autonomy_thresholds("kim").await.unwrap(), None);
    assert_eq!(audit_count(&rig, "autonomy_thresholds_changed").await, 2);
}
