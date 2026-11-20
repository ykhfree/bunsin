//! Corrections: stored filtered and without the AI marker, and put only into the prompts of
//! similar live events. A replay gets none.

mod common;

use bunsin_api::{ApiError, Edit, EngineApi, Outcome, ReplayCase};
use bunsin_core::policy::{OutgoingText, ToolClass, ToolRule};
use common::*;
use serde_json::{Value, json};

fn args(value: Value) -> serde_json::Map<String, Value> {
    value.as_object().unwrap().clone()
}

/// The agent proposes `proposed` for an event with `task`. The owner replaces it with `edited` and approves.
async fn correct(rig: &Rig, id: &str, task: &str, proposed: Value, edited: Value, reason: &str) {
    rig.llm.reply(&call("slack/post", proposed), 10);
    let Outcome::PendingApproval { inbox_id } = rig.engine.ingest("kim", event(id, "lee", task)).await.unwrap() else {
        panic!("expected a pending item")
    };
    let edit = Edit {
        args: args(edited),
        reason: reason.into(),
    };
    rig.engine.approve("kim", inbox_id, Some(edit)).await.unwrap();
}

async fn correct_text(rig: &Rig, id: &str, task: &str, edited: &str, reason: &str) {
    let proposed = json!({"channel": "C1", "text": format!("Draft for {id}.")});
    correct(
        rig,
        id,
        task,
        proposed,
        json!({"channel": "C1", "text": edited}),
        reason,
    )
    .await;
}

/// The user part of the last LLM request.
fn last_prompt(rig: &Rig) -> String {
    rig.llm.requests().last().unwrap().messages[0].content.clone()
}

async fn ask(rig: &Rig, id: &str, text: &str) -> String {
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", event(id, "lee", text)).await.unwrap();
    last_prompt(rig)
}

#[tokio::test]
async fn only_the_corrections_of_similar_tasks_reach_the_prompt() {
    let rig = rig().await;
    correct_text(
        &rig,
        "e1",
        "금요일 배포 일정을 알려 주세요",
        "목요일입니다.",
        "deploy moved",
    )
    .await;
    correct_text(&rig, "e2", "점심 메뉴 추천해 주세요", "국수", "lunch").await;
    correct_text(&rig, "e3", "회의실 예약해 주세요", "3층", "meeting room").await;
    correct_text(&rig, "e4", "the office wifi password", "Ask IT", "wifi").await;

    // `배포` has two syllables and is found with LIKE.
    let prompt = ask(&rig, "e5", "배포는 언제인가요?").await;
    assert_eq!(prompt.matches("<data source=\"correction\">").count(), 1, "{prompt}");
    assert!(prompt.contains("draft: Draft for e1.\nedit: 목요일입니다.\nreason: deploy moved"));
    // `주세요` is in three of four tasks. It finds nothing alone.
    let prompt = ask(&rig, "e6", "알려 주세요").await;
    assert_eq!(prompt.matches("<data source=\"correction\">").count(), 1, "{prompt}");
    assert!(prompt.contains("deploy moved"));
    let prompt = ask(&rig, "e7", "회의실 wifi").await;
    assert!(prompt.contains("reason: meeting room") && prompt.contains("reason: wifi"));
    assert!(!prompt.contains("deploy moved") && !prompt.contains("lunch"));
    // An unrelated event gets no corrections at all.
    let prompt = ask(&rig, "e8", "weather tomorrow").await;
    assert!(!prompt.contains("Past corrections"), "{prompt}");
}

#[tokio::test]
async fn a_correction_holds_the_call_and_its_event_without_the_marker() {
    let rig = rig().await;
    correct_text(
        &rig,
        "e1",
        "who owns the release notes?",
        "Mina owns them.",
        "name the owner",
    )
    .await;
    // The owner's text is sent with the marker.
    assert_eq!(rig.slack.calls()[0].1["text"], format!("Mina owns them.\n\n{MARKER}"));

    let records = rig.engine.corrections("kim").await.unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    assert_eq!(record.inbox_id, inbox[0].id);
    assert_eq!(record.retired_at, None);
    let c = &record.correction;
    assert_eq!(
        (c.tool.as_str(), c.source.as_str(), c.task.as_str()),
        ("slack/post", "slack", "who owns the release notes?")
    );
    assert_eq!(
        (c.original.as_str(), c.edited.as_str(), c.reason.as_str()),
        ("Draft for e1.", "Mina owns them.", "name the owner")
    );
}

#[tokio::test]
async fn a_rule_with_several_places_stores_their_texts_joined() {
    let mut config = config();
    config.policy.rules.insert(
        "slack/post".into(),
        ToolRule::write(
            ToolClass::Write,
            OutgoingText::Paths(vec!["text".into(), "blocks.*.text".into()]),
        ),
    );
    let rig = rig_with(config).await;
    let proposed = json!({"channel": "C1", "text": "Fallback", "blocks": [{"text": "First"}, {"text": "Second"}]});
    let edited = json!({"channel": "C1", "text": "Short", "blocks": [{"text": "One"}, {"text": "Two"}]});
    correct(&rig, "e1", "the deploy plan", proposed, edited, "shorter").await;
    let c = &rig.engine.corrections("kim").await.unwrap()[0].correction;
    assert_eq!(c.original, "Fallback\n\nFirst\n\nSecond");
    assert_eq!(c.edited, "Short\n\nOne\n\nTwo");
    assert!(!c.original.contains(MARKER) && !c.edited.contains(MARKER));
}

#[tokio::test]
async fn a_retired_correction_reaches_no_prompt() {
    let rig = rig().await;
    correct_text(&rig, "e1", "금요일 배포 일정", "목요일", "deploy moved").await;
    assert!(ask(&rig, "e2", "금요일 배포").await.contains("deploy moved"));

    let id = rig.engine.corrections("kim").await.unwrap()[0].id;
    rig.engine.retire_correction("kim", id).await.unwrap();
    assert!(!ask(&rig, "e3", "금요일 배포").await.contains("Past corrections"));
    let record = &rig.engine.corrections("kim").await.unwrap()[0];
    assert!(record.retired_at.is_some());

    let again = rig.engine.retire_correction("kim", id).await;
    assert!(matches!(again, Err(ApiError::NotAllowed(_))), "{again:?}");
    let missing = rig.engine.retire_correction("kim", id + 100).await;
    assert!(matches!(missing, Err(ApiError::NotFound(_))), "{missing:?}");
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let retired: Vec<_> = audit.iter().filter(|e| e.kind == "correction_retired").collect();
    assert_eq!(retired.len(), 1);
    assert_eq!(retired[0].detail, json!({"id": id}));
}

#[tokio::test]
async fn a_replay_gets_no_correction_even_one_with_the_real_reply() {
    let rig = rig().await;
    // The owner's edit is the real reply of the case that is replayed later.
    correct_text(
        &rig,
        "e1",
        "who owns the release notes?",
        "Mina owns them.",
        "name the owner",
    )
    .await;
    assert!(
        ask(&rig, "e2", "who owns the release notes?")
            .await
            .contains("Mina owns them.")
    );

    rig.llm.reply(&ignore(), 10);
    let case = ReplayCase {
        event: event("old", "lee", "who owns the release notes?"),
        actual_reply: "Mina owns them.".into(),
        expect: bunsin_core::eval::Expect::Reply,
    };
    rig.engine.replay("kim", case).await.unwrap();
    let replayed = serde_json::to_string(rig.llm.requests().last().unwrap()).unwrap();
    assert!(!replayed.contains("Past corrections"), "{replayed}");
    assert!(!replayed.contains("Mina owns them."));
}
