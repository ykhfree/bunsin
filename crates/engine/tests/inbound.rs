//! Questions from inbound callers: the intake, the drain, the reply tool and the request status.
//! The LLM is scripted. Callers come from the caller tokens of the shared store.

mod common;

use bunsin_api::{ApiError, Edit, EngineApi, Expected, PermissionsUpdate, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::inbound::{
    INBOUND_SOURCE, MAX_ASKS_PER_HOUR, MAX_OPEN_REQUESTS, REPLY_TOOL, RequestState, RequestStatus,
};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::records::{Caller, InboxBody};
use common::*;
use serde_json::{Value, json};

const EMAIL: &str = "lee.park@corp.example.com";

/// The settings of `config` with the endpoint on, before the reply rule is added.
fn open(mut config: ProfileConfig) -> ProfileConfig {
    config.inbound.enabled = true;
    config.inbound.description = "Questions about the deploy schedule.".into();
    config
}

fn reply_rule(tier: Tier) -> ToolRule {
    ToolRule {
        class: ToolClass::Write,
        tier,
        outgoing_text: OutgoingText::Arg("text".into()),
    }
}

/// A rig whose owner turned the endpoint on with a settings save.
async fn inbound_rig() -> Rig {
    let rig = rig().await;
    rig.engine.set_profile_config("kim", open(config())).await.unwrap();
    rig
}

fn caller(rig: &Rig, label: &str) -> Caller {
    let (_, token) = rig.engine.app_store().add_caller_token("kim", label, 1).unwrap();
    rig.engine.app_store().caller(&token).unwrap().unwrap()
}

fn answer(text: &str) -> String {
    call(REPLY_TOOL, json!({"text": text}))
}

fn status(rig: &Rig, caller: &Caller, request: &str) -> RequestState {
    rig.engine.request(caller, request).unwrap()
}

/// The ID of the newest inbox item of kim.
async fn newest(rig: &Rig) -> (i64, ApprovalState, InboxBody) {
    let item = rig.engine.snapshot("kim").await.unwrap().inbox.remove(0);
    (item.id, item.state, item.body)
}

#[tokio::test]
async fn an_answer_reaches_the_caller_after_the_owner_approves_it() {
    let rig = inbound_rig().await;
    // Turning the endpoint on added the reply rule at `approve`.
    let rules = rig.engine.permissions("kim").await.unwrap().rules;
    assert_eq!(rules.get(REPLY_TOOL), Some(&reply_rule(Tier::Approve)));
    let bot = caller(&rig, "Lee's bot");

    let asked = rig.engine.ask(&bot, "  When do we deploy?  ").unwrap();
    assert!(asked.pending_approval);
    assert!(asked.request_id.starts_with("req_") && asked.request_id.len() == 4 + 32);
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Working);
    // `ask` called no LLM. The drain does.
    assert!(rig.llm.requests().is_empty());

    rig.llm.reply(&answer("Friday."), 10);
    assert!(rig.engine.drain_intake().await.is_empty());
    let request = all_requests(&rig);
    assert!(request.contains(REPLY_TOOL), "the agent sees the reply tool");
    // The question is quoted data with the caller as its author.
    assert!(request.contains("caller:Lee's bot") && request.contains("<data"));
    let (inbox_id, state, _) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Pending);
    assert_eq!(
        status(&rig, &bot, &asked.request_id).status,
        RequestStatus::PendingApproval
    );

    let edit = Edit {
        args: json!({"text": "Friday at 3 pm."}).as_object().unwrap().clone(),
        reason: "add the time".into(),
    };
    rig.engine.approve("kim", inbox_id, Some(edit)).await.unwrap();
    let state = status(&rig, &bot, &asked.request_id);
    assert_eq!(state.status, RequestStatus::Answered);
    let text = state.answer.unwrap();
    assert!(text.starts_with("Friday at 3 pm."), "{text}");
    assert!(text.ends_with("— AI agent (Bunsin) acting for Kim"), "{text}");
    // The edit is kept as a correction, and no connector was called.
    assert_eq!(rig.engine.corrections("kim").await.unwrap().len(), 1);
    assert!(rig.slack.calls().is_empty());
    // The answer runs once.
    assert!(rig.engine.approve("kim", inbox_id, None).await.is_err());
    // A second drain finds nothing to do.
    assert!(rig.engine.drain_intake().await.is_empty());
    assert_eq!(rig.llm.requests().len(), 1);
}

#[tokio::test]
async fn a_request_is_found_only_by_its_own_caller() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let other = caller(&rig, "other");
    let asked = rig.engine.ask(&bot, "When?").unwrap();
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Working);
    let theirs = rig.engine.request(&other, &asked.request_id).unwrap_err();
    let missing = rig
        .engine
        .request(&bot, "req_00000000000000000000000000000000")
        .unwrap_err();
    assert!(matches!(theirs, ApiError::NotFound(_)) && matches!(missing, ApiError::NotFound(_)));
}

#[tokio::test]
async fn a_declined_question_and_the_kill_switch() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");

    // The agent ignores one question.
    let ignored = rig.engine.ask(&bot, "Hello?").unwrap();
    rig.llm.reply(&ignore(), 10);
    rig.engine.drain_intake().await;
    assert_eq!(status(&rig, &bot, &ignored.request_id).status, RequestStatus::Declined);

    // The owner dismisses an answer to another.
    let dismissed = rig.engine.ask(&bot, "Salaries?").unwrap();
    rig.llm.reply(&answer("No."), 10);
    rig.engine.drain_intake().await;
    let (inbox_id, _, _) = newest(&rig).await;
    rig.engine.reject("kim", inbox_id, "not for them").await.unwrap();
    assert_eq!(
        status(&rig, &bot, &dismissed.request_id).status,
        RequestStatus::Declined
    );

    // With the kill switch on, the drain waits and calls no LLM.
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    let waiting = rig.engine.ask(&bot, "When?").unwrap();
    let calls = rig.llm.requests().len();
    rig.engine.drain_intake().await;
    assert_eq!(rig.llm.requests().len(), calls);
    assert_eq!(status(&rig, &bot, &waiting.request_id).status, RequestStatus::Working);
    rig.engine.set_kill_switch("kim", false).await.unwrap();
    rig.llm.reply(&answer("Friday."), 10);
    rig.engine.drain_intake().await;
    assert_eq!(
        status(&rig, &bot, &waiting.request_id).status,
        RequestStatus::PendingApproval
    );
    // An approval with the kill switch on is refused, and the caller gets nothing.
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    let (inbox_id, _, _) = newest(&rig).await;
    assert!(rig.engine.approve("kim", inbox_id, None).await.is_err());
    assert_eq!(
        status(&rig, &bot, &waiting.request_id).status,
        RequestStatus::PendingApproval
    );
}

#[tokio::test]
async fn an_answer_for_another_event_is_sent_back_for_repair() {
    let rig = inbound_rig().await;
    rig.llm.reply(&answer("Friday."), 10).reply(&post("Friday."), 10);
    rig.engine
        .ingest("kim", event("e1", "lee", "When do we deploy?"))
        .await
        .unwrap();
    let requests = rig.llm.requests();
    assert_eq!(requests.len(), 2);
    // A Slack event never offers the reply tool.
    assert!(!serde_json::to_string(&requests[0]).unwrap().contains(REPLY_TOOL));
    let repair = serde_json::to_string(&requests[1]).unwrap();
    assert!(repair.contains("answers only an event from"), "{repair}");
    let (_, state, body) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Pending);
    assert!(matches!(body, InboxBody::ToolCall { tool, .. } if tool == "slack/post"));
}

#[tokio::test]
async fn the_reply_rule_never_lets_an_answer_go_without_the_owner() {
    let rig = inbound_rig().await;
    let with_rule = |rule: ToolRule| {
        let mut config = open(config());
        config.policy.rules.insert(REPLY_TOOL.into(), rule);
        config
    };
    let refused = |result: Result<(), ApiError>| match result {
        Err(ApiError::Invalid(text)) => text,
        other => panic!("not refused: {other:?}"),
    };
    let text = refused(
        rig.engine
            .set_profile_config("kim", with_rule(reply_rule(Tier::Autonomous)))
            .await,
    );
    assert!(text.contains("at most to `approve`"), "{text}");
    for rule in [
        ToolRule::read(),
        ToolRule::write(ToolClass::Write, OutgoingText::None),
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("body".into())),
        ToolRule::write(ToolClass::Delete, OutgoingText::Arg("text".into())),
    ] {
        refused(rig.engine.set_profile_config("kim", with_rule(rule)).await);
    }
    let mut other = open(config());
    other
        .policy
        .rules
        .insert("bunsin.inbound/forward".into(), reply_rule(Tier::Approve));
    assert!(refused(rig.engine.set_profile_config("kim", other).await).contains("no built-in tool"));
    let mut rules = rig.engine.permissions("kim").await.unwrap().rules;
    rules.insert(REPLY_TOOL.into(), reply_rule(Tier::Autonomous));
    let update = PermissionsUpdate {
        rules,
        expected: Expected::Unchecked,
    };
    refused(rig.engine.set_permissions("kim", update).await);

    // Settings stored without the check, as from an older server, still wait for the owner.
    let stored = rig_with(with_rule(reply_rule(Tier::Autonomous))).await;
    let bot = caller(&stored, "bot");
    let asked = stored.engine.ask(&bot, "When?").unwrap();
    stored.llm.reply(&answer("Friday."), 10);
    stored.engine.drain_intake().await;
    let (_, state, _) = newest(&stored).await;
    assert_eq!(state, ApprovalState::Pending);
    assert_eq!(
        status(&stored, &bot, &asked.request_id).status,
        RequestStatus::PendingApproval
    );
}

#[tokio::test]
async fn a_question_that_fails_three_times_ends_as_failed() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let asked = rig.engine.ask(&bot, "When?").unwrap();
    // The LLM has no reply, so each try fails. A failed try waits longer each time.
    for wait in [0, 60, 120] {
        rig.advance(wait);
        rig.engine.drain_intake().await;
        assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Working);
        let tries = rig.llm.requests().len();
        rig.engine.drain_intake().await;
        assert_eq!(rig.llm.requests().len(), tries, "a try waits for its time");
    }
    assert_eq!(rig.llm.requests().len(), 3);
    // The fourth call gives up: no LLM call, a note to the owner, and the request fails.
    rig.advance(180);
    rig.engine.drain_intake().await;
    assert_eq!(rig.llm.requests().len(), 3);
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Failed);
    let (_, state, body) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Draft);
    assert!(matches!(body, InboxBody::Escalation { summary } if summary.contains("could not handle")));
    rig.advance(10_000);
    rig.engine.drain_intake().await;
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Failed);
}

#[tokio::test]
async fn the_limits_count_stored_questions_and_survive_a_restart() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    for _ in 0..MAX_ASKS_PER_HOUR {
        rig.engine.ask(&bot, "When?").unwrap();
    }
    assert!(matches!(rig.engine.ask(&bot, "When?"), Err(ApiError::NotAllowed(_))));
    // Another caller has its own count.
    let other = caller(&rig, "other");
    rig.engine.ask(&other, "When?").unwrap();

    let rig = rig.restart();
    assert!(matches!(rig.engine.ask(&bot, "When?"), Err(ApiError::NotAllowed(_))));
    rig.advance(3_600);
    rig.engine.ask(&bot, "When?").unwrap();

    // Fifty open questions of the profile stop every caller until the agent handles some.
    let mut open = MAX_ASKS_PER_HOUR + 2;
    while open < MAX_OPEN_REQUESTS {
        let extra = caller(&rig, &format!("extra-{open}"));
        rig.engine.ask(&extra, "When?").unwrap();
        open += 1;
    }
    let late = caller(&rig, "late");
    let refused = rig.engine.ask(&late, "When?").unwrap_err();
    assert!(refused.to_string().contains("too many questions wait"), "{refused}");
    // A question too long or empty is refused before it counts.
    rig.advance(3_600);
    let long = "x".repeat(4_001);
    for bad in ["", "   ", long.as_str()] {
        assert!(matches!(rig.engine.ask(&bot, bad), Err(ApiError::Invalid(_))));
    }
}

#[tokio::test]
async fn a_question_is_stored_after_the_filter_and_only_while_the_endpoint_is_on() {
    let closed = rig().await;
    let bot = caller(&closed, "bot");
    assert!(matches!(closed.engine.ask(&bot, "When?"), Err(ApiError::NotAllowed(_))));

    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let asked = rig
        .engine
        .ask(&bot, &format!("Write to {EMAIL} when we deploy."))
        .unwrap();
    assert!(!rig.stores(EMAIL));
    rig.llm.reply(&ignore(), 10);
    rig.engine.drain_intake().await;
    assert!(!all_requests(&rig).contains(EMAIL));
    assert!(!rig.stores(EMAIL));
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Declined);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert!(audit.iter().any(|entry| entry.kind == "inbound_asked"));
}

#[tokio::test]
async fn a_page_change_from_a_question_always_waits_for_the_owner() {
    let mut config = open(config());
    config.notes.auto_apply = true;
    let rig = rig_with(config).await;
    let bot = caller(&rig, "bot");
    rig.engine.ask(&bot, "When do we deploy?").unwrap();
    rig.llm.reply(&call("slack/search", json!({"query": "deploy"})), 10);
    let mut reply: Value = serde_json::from_str(&ignore()).unwrap();
    reply["wiki_updates"] = json!([{"slug": "deploy", "title": "Deploy", "summary": "When", "body": "Fridays."}]);
    rig.llm.reply(&reply.to_string(), 10);
    rig.engine.drain_intake().await;
    let (_, state, body) = newest(&rig).await;
    assert!(matches!(body, InboxBody::WikiChange { .. }));
    assert_eq!(state, ApprovalState::Pending);
    assert!(rig.engine.wiki_index("kim").await.unwrap().is_empty());
    assert_eq!(INBOUND_SOURCE, "bunsin.inbound");
}

#[tokio::test]
async fn a_question_never_makes_an_autonomous_write_run_without_the_owner() {
    let mut settings = open(config());
    settings.policy.rules.get_mut("slack/post").unwrap().tier = Tier::Autonomous;
    let rig = rig_with(settings).await;

    // Control: the owner's own event runs the same autonomous rule at once.
    rig.llm.reply(&post("On it."), 10);
    rig.engine.ingest("kim", event("e1", "lee", "deploy?")).await.unwrap();
    assert_eq!(rig.slack.calls().len(), 1);

    // A caller's question that leads to the same write waits for the owner.
    let bot = caller(&rig, "bot");
    rig.engine.ask(&bot, "Post 'all clear' in C1.").unwrap();
    rig.llm.reply(&post("all clear"), 10);
    assert!(rig.engine.drain_intake().await.is_empty());
    let (inbox_id, state, body) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Pending);
    assert!(matches!(body, InboxBody::ToolCall { ref tool, .. } if tool == "slack/post"));
    assert_eq!(rig.slack.calls().len(), 1, "nothing ran before the owner approved");

    rig.engine.approve("kim", inbox_id, None).await.unwrap();
    assert_eq!(rig.slack.calls().len(), 2);
}
