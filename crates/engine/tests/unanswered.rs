//! A write that gets no answer, for example at the connector's time limit, may have reached its
//! tool. It becomes `unknown` and never runs again by itself. A read that gets no answer stays data.

mod common;

use bunsin_api::{ApiError, EngineApi, Expected, Outcome, PermissionsUpdate, Resolution};
use bunsin_core::approval::ApprovalState;
use bunsin_core::policy::Tier;
use bunsin_core::ports::ConnectorError;
use common::*;
use serde_json::json;

const TIMED_OUT: &str = "the call of `post` did not end in 120 s and was cancelled";

fn timed_out() -> ConnectorError {
    ConnectorError::Unanswered(TIMED_OUT.into())
}

async fn pending_post(rig: &Rig, id: &str) -> i64 {
    rig.llm.reply(&post("At 3 pm."), 10);
    match rig.engine.ingest("kim", event(id, "lee", "when?")).await.unwrap() {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("expected a pending item, got {other:?}"),
    }
}

async fn state(rig: &Rig, inbox_id: i64) -> ApprovalState {
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    snapshot.inbox.into_iter().find(|i| i.id == inbox_id).unwrap().state
}

async fn audit_kinds(rig: &Rig) -> Vec<String> {
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    snapshot.audit.into_iter().map(|e| e.kind).collect()
}

fn posts(rig: &Rig) -> usize {
    rig.slack.calls().iter().filter(|(tool, _)| tool == "post").count()
}

#[tokio::test]
async fn an_approved_write_without_an_answer_becomes_unknown_and_is_not_sent_again() {
    let rig = rig().await;
    let id = pending_post(&rig, "e1").await;
    rig.slack.fail_with_error(timed_out());

    let outcome = rig.engine.approve("kim", id, None).await.unwrap();
    match &outcome {
        Outcome::ExecutionUnknown { inbox_id, error } => {
            assert_eq!(*inbox_id, id);
            assert!(error.contains(TIMED_OUT), "{error}");
        }
        other => panic!("expected an unknown outcome, got {other:?}"),
    }
    assert_eq!(state(&rig, id).await, ApprovalState::Unknown);
    assert_eq!(posts(&rig), 1);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let entry = audit.iter().find(|a| a.kind == "outcome_unknown").unwrap();
    assert_eq!(entry.detail["inbox_id"], id);
    assert_eq!(entry.detail["tool"], "slack/post");
    assert!(entry.detail["error"].as_str().unwrap().contains(TIMED_OUT));
    assert!(!audit_kinds(&rig).await.contains(&"execution_failed".to_owned()));

    // A second approval is refused. The call is not sent a second time.
    assert!(matches!(
        rig.engine.approve("kim", id, None).await,
        Err(ApiError::NotAllowed(_))
    ));
    // A start after a stop does not run it either.
    let interrupted = rig.engine.mark_interrupted();
    assert!(rig.engine.resume_approved(interrupted).await.is_empty());
    assert_eq!(posts(&rig), 1);
    assert_eq!(state(&rig, id).await, ApprovalState::Unknown);

    // Only the owner resolves it.
    rig.engine.resolve_unknown("kim", id, Resolution::Done).await.unwrap();
    assert_eq!(state(&rig, id).await, ApprovalState::Executed);
    assert_eq!(posts(&rig), 1);
}

#[tokio::test]
async fn only_an_unanswered_call_is_unknown_every_other_error_fails() {
    let cases = [
        // The server refused the call cleanly, for example with HTTP 400.
        (
            ConnectorError::Failed("HTTP 400: channel_not_found".into()),
            ApprovalState::Failed,
        ),
        // The request never reached the server.
        (
            ConnectorError::Transport("the request failed: connection refused".into()),
            ApprovalState::Failed,
        ),
        (ConnectorError::UnknownTool("post".into()), ApprovalState::Failed),
        (timed_out(), ApprovalState::Unknown),
    ];
    for (error, expected) in cases {
        let rig = rig().await;
        let id = pending_post(&rig, "e1").await;
        rig.slack.fail_with_error(error.clone());
        let outcome = rig.engine.approve("kim", id, None).await.unwrap();
        let text = error.to_string();
        match (&outcome, expected) {
            (Outcome::ExecutionFailed { error, .. }, ApprovalState::Failed) => assert_eq!(error, &text),
            (Outcome::ExecutionUnknown { error, .. }, ApprovalState::Unknown) => assert_eq!(error, &text),
            _ => panic!("{text}: expected {expected:?}, got {outcome:?}"),
        }
        assert_eq!(state(&rig, id).await, expected, "{text}");
        assert_eq!(posts(&rig), 1, "{text}");
    }
}

#[tokio::test]
async fn an_autonomous_write_without_an_answer_is_unknown_and_the_event_is_not_retried() {
    let rig = rig().await;
    let mut rules = rig.engine.permissions("kim").await.unwrap().rules;
    rules.get_mut("slack/post").unwrap().tier = Tier::Autonomous;
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    rig.engine.set_permissions("kim", update).await.unwrap();
    rig.slack.fail_with_error(timed_out());

    rig.llm.reply(&post("Done."), 10);
    let outcome = rig.engine.ingest("kim", event("e1", "lee", "when?")).await.unwrap();
    let Outcome::ExecutionUnknown { inbox_id, .. } = outcome else {
        panic!("expected an unknown outcome, got {outcome:?}")
    };
    assert_eq!(state(&rig, inbox_id).await, ApprovalState::Unknown);

    // The same event again is a duplicate. The LLM is not asked and the call is not sent.
    let again = rig.engine.ingest("kim", event("e1", "lee", "when?")).await.unwrap();
    assert!(matches!(again, Outcome::Duplicate), "{again:?}");
    assert_eq!(posts(&rig), 1);
    assert_eq!(rig.llm.requests().len(), 1);
}

#[tokio::test]
async fn a_read_without_an_answer_goes_back_to_the_llm_as_data() {
    let rig = rig().await;
    rig.slack.fail_with_error(ConnectorError::Unanswered(
        "the call of `search` did not end in 120 s".into(),
    ));
    let escalate = json!({"kind": "escalate", "rationale": "search is slow", "summary": "I could not search.",
        "tool": null, "args_json": null});
    rig.llm
        .reply(&call("slack/search", json!({"query": "deploy"})), 10)
        .reply(&escalate.to_string(), 10);

    let outcome = rig.engine.ingest("kim", event("e1", "lee", "status?")).await.unwrap();

    assert!(matches!(outcome, Outcome::Escalated { .. }), "{outcome:?}");
    assert!(
        rig.llm.requests()[1].messages[2]
            .content
            .contains("The tool call failed: no answer: the call of `search` did not end in 120 s")
    );
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert!(audit.iter().any(|a| a.kind == "read" && a.detail["ok"] == false));
    assert!(!audit.iter().any(|a| a.kind == "outcome_unknown"));
    assert!(
        rig.engine
            .snapshot("kim")
            .await
            .unwrap()
            .inbox
            .iter()
            .all(|i| i.state != ApprovalState::Unknown)
    );
}
