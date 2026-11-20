//! Questions from inbound callers: the intake, the drain, the reply tool and the request status.
//! The LLM is scripted. Callers come from the caller tokens of the shared store.

mod common;

use bunsin_api::{ApiError, Edit, EngineApi, Expected, PermissionsUpdate, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::event::InboundEvent;
use bunsin_core::inbound::{
    INBOUND_SOURCE, MAX_ASKS_PER_HOUR, MAX_OPEN_REQUESTS, REPLY_TOOL, RequestState, RequestStatus,
};
use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason, Usage};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::ports::{BoxFuture, LlmClient};
use bunsin_core::records::{Caller, InboxBody, InboxItem};
use bunsin_engine::{Backends, Engine, Runtime};
use common::*;
use serde_json::{Value, json};
use std::sync::Arc;

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

    // The agent ignores one question. The caller does not learn that choice: the owner decides.
    let ignored = rig.engine.ask(&bot, "Hello?").unwrap();
    rig.llm.reply(&ignore(), 10);
    rig.engine.drain_intake().await;
    assert_eq!(
        status(&rig, &bot, &ignored.request_id).status,
        RequestStatus::PendingApproval
    );
    let (inbox_id, state, body) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Draft);
    assert!(matches!(body, InboxBody::Escalation { ref summary } if summary.contains("no answer")));
    rig.engine.reject("kim", inbox_id, "not for them").await.unwrap();
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
async fn a_question_that_fails_three_times_waits_for_the_owner() {
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
    // The fourth call gives up: no LLM call and a note to the owner. The caller sees only
    // that the owner has not decided; the owner's dismissal declines the question.
    rig.advance(180);
    rig.engine.drain_intake().await;
    assert_eq!(rig.llm.requests().len(), 3);
    let pending = RequestStatus::PendingApproval;
    assert_eq!(status(&rig, &bot, &asked.request_id).status, pending);
    let (inbox_id, state, body) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Draft);
    assert!(matches!(body, InboxBody::Escalation { summary } if summary.contains("could not handle")));
    rig.advance(10_000);
    rig.engine.drain_intake().await;
    assert_eq!(rig.llm.requests().len(), 3);
    assert_eq!(status(&rig, &bot, &asked.request_id).status, pending);
    rig.engine.reject("kim", inbox_id, "").await.unwrap();
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Declined);
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
    assert_eq!(
        status(&rig, &bot, &asked.request_id).status,
        RequestStatus::PendingApproval
    );
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
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    let change = inbox
        .iter()
        .find(|item| matches!(item.body, InboxBody::WikiChange { .. }))
        .expect("the page change is in the inbox");
    assert_eq!(change.state, ApprovalState::Pending);
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

/// The inbox items of the event of a caller's request, oldest first.
fn request_items(rig: &Rig, request_id: &str) -> Vec<InboxItem> {
    let key = InboundEvent {
        source: INBOUND_SOURCE.into(),
        external_id: request_id.into(),
        thread: None,
        author: String::new(),
        text: String::new(),
        channel: Default::default(),
    }
    .dedupe_key();
    rig.kim_store().inbox_for_event(&key).unwrap()
}

/// The owner's dismissals of waiting calls, over all tools.
async fn rejections(rig: &Rig) -> u32 {
    let autonomy = rig.engine.autonomy("kim").await.unwrap();
    autonomy.tools.iter().map(|tool| tool.counts.rejected).sum()
}

async fn audit_kinds(engine: &Engine) -> Vec<String> {
    let audit = engine.snapshot("kim").await.unwrap().audit;
    audit.into_iter().map(|entry| entry.kind).collect()
}

#[tokio::test]
async fn cancel_withdraws_items_without_counting_a_rejection() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let asked = rig.engine.send_message(&bot, "When?", "m-1", None).unwrap();
    rig.llm.reply(&answer("Friday."), 10);
    rig.engine.drain_intake().await;
    let (inbox_id, state, _) = newest(&rig).await;
    assert_eq!(state, ApprovalState::Pending);
    rig.advance(30);

    let canceled = rig.engine.cancel_request(&bot, &asked.request_id).unwrap();
    assert_eq!(canceled.status, RequestStatus::Canceled);
    assert_eq!(canceled.status_at, 1_000_030);
    let items = request_items(&rig, &asked.request_id);
    assert_eq!(items.len(), 1);
    assert_eq!((items[0].id, items[0].state), (inbox_id, ApprovalState::Rejected));
    // The owner decided nothing, so nothing counts against the reply tool.
    assert_eq!(rejections(&rig).await, 0);
    assert!(audit_kinds(&rig.engine).await.contains(&"inbound_canceled".to_owned()));
    // Approving the withdrawn answer is refused, so the caller gets nothing.
    assert!(rig.engine.approve("kim", inbox_id, None).await.is_err());
    // A second cancel changes nothing and keeps the first time.
    rig.advance(30);
    assert_eq!(rig.engine.cancel_request(&bot, &asked.request_id).unwrap(), canceled);
    // `get_request` shows the cancel too.
    assert_eq!(status(&rig, &bot, &asked.request_id).status, RequestStatus::Canceled);

    // Control: the owner's own dismissal does count.
    let other = rig.engine.ask(&bot, "Salaries?").unwrap();
    rig.llm.reply(&answer("No."), 10);
    rig.engine.drain_intake().await;
    let (inbox_id, _, _) = newest(&rig).await;
    rig.engine.reject("kim", inbox_id, "no").await.unwrap();
    assert_eq!(rejections(&rig).await, 1);
    // A decided request is not canceled: the caller keeps seeing the decision.
    let declined = rig.engine.cancel_request(&bot, &other.request_id).unwrap();
    assert_eq!(declined.status, RequestStatus::Declined);
    // Another caller's request is not found.
    let stranger = caller(&rig, "stranger");
    let refused = rig.engine.cancel_request(&stranger, &other.request_id);
    assert!(matches!(refused, Err(ApiError::NotFound(_))), "{refused:?}");
}

#[tokio::test]
async fn a_canceled_question_is_not_handed_to_the_agent() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let asked = rig.engine.send_message(&bot, "When?", "m-1", None).unwrap();
    rig.engine.cancel_request(&bot, &asked.request_id).unwrap();
    rig.llm.reply(&answer("Friday."), 10);
    assert!(rig.engine.drain_intake().await.is_empty());
    assert!(
        rig.llm.requests().is_empty(),
        "the agent never saw the canceled question"
    );
    assert!(request_items(&rig, &asked.request_id).is_empty());
}

#[tokio::test]
async fn an_answer_approved_after_cancel_is_not_delivered() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let asked = rig.engine.send_message(&bot, "When?", "m-1", None).unwrap();
    rig.llm.reply(&answer("Friday."), 10);
    rig.engine.drain_intake().await;
    let (inbox_id, _, _) = newest(&rig).await;
    // The owner's approval moved the answer to `approved`, and its run has not started yet.
    let store = rig.kim_store();
    assert!(
        store
            .move_inbox_state(inbox_id, ApprovalState::Pending, ApprovalState::Approved, 1_000_000)
            .unwrap()
    );
    // The cancel cannot withdraw an approved item.
    rig.engine.cancel_request(&bot, &asked.request_id).unwrap();
    assert_eq!(store.inbox_item(inbox_id).unwrap().state, ApprovalState::Approved);
    // The run checks the cancel: the answer fails and goes nowhere.
    let interrupted = rig.engine.mark_interrupted();
    assert!(rig.engine.resume_approved(interrupted).await.is_empty());
    assert_eq!(store.inbox_item(inbox_id).unwrap().state, ApprovalState::Failed);
    let kinds = audit_kinds(&rig.engine).await;
    assert!(kinds.contains(&"inbound_reply_canceled".to_owned()), "{kinds:?}");
    assert!(!kinds.contains(&"inbound_answered".to_owned()), "{kinds:?}");
    let state = status(&rig, &bot, &asked.request_id);
    assert_eq!((state.status, state.answer), (RequestStatus::Canceled, None));
}

#[tokio::test]
async fn a_repeated_message_id_counts_once() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let first = rig.engine.send_message(&bot, "When?", "m-1", Some("ctx-1")).unwrap();
    assert_eq!(first.status, RequestStatus::Working);
    assert_eq!((first.context_id.as_str(), first.status_at), ("ctx-1", 1_000_000));
    // A resend returns the first request, also with other text, and stores nothing.
    rig.advance(5);
    let again = rig.engine.send_message(&bot, "When??", "m-1", None).unwrap();
    assert_eq!(again, first);
    assert_eq!(rig.engine.caller_requests(&bot).unwrap().len(), 1);
    // Without a context ID, the context is the request ID.
    let second = rig.engine.send_message(&bot, "Where?", "m-2", None).unwrap();
    assert_eq!(second.context_id, second.request_id);
    // Another caller with the same message ID gets its own request.
    let other = caller(&rig, "other");
    let theirs = rig.engine.send_message(&other, "When?", "m-1", None).unwrap();
    assert_ne!(theirs.request_id, first.request_id);

    // The hourly limit counts stored questions only, so resends never use it up.
    for n in 3..=MAX_ASKS_PER_HOUR {
        rig.engine.send_message(&bot, "When?", &format!("m-{n}"), None).unwrap();
    }
    assert!(matches!(
        rig.engine.send_message(&bot, "When?", "m-new", None),
        Err(ApiError::NotAllowed(_))
    ));
    // Over the limit, a resend still finds its first request.
    assert_eq!(rig.engine.send_message(&bot, "When?", "m-1", None).unwrap(), first);
    let asked = audit_kinds(&rig.engine)
        .await
        .into_iter()
        .filter(|kind| kind == "inbound_asked")
        .count();
    assert_eq!(asked as u32, MAX_ASKS_PER_HOUR + 1);
}

#[tokio::test]
async fn the_listed_requests_are_the_callers_own_with_the_time_of_the_owners_decision() {
    let rig = inbound_rig().await;
    let bot = caller(&rig, "bot");
    let over_mcp = rig.engine.ask(&bot, "When?").unwrap();
    rig.advance(10);
    let over_a2a = rig.engine.send_message(&bot, "Where?", "m-1", None).unwrap();
    let other = caller(&rig, "other");
    rig.engine.send_message(&other, "Who?", "m-1", None).unwrap();
    rig.advance(10);
    // The agent's work changes no time: both still show their arrival.
    rig.llm.reply(&answer("Friday."), 10).reply(&ignore(), 10);
    rig.engine.drain_intake().await;
    let listed = rig.engine.caller_requests(&bot).unwrap();
    let ids: Vec<&str> = listed.iter().map(|r| r.request_id.as_str()).collect();
    assert_eq!(ids, [over_a2a.request_id.as_str(), over_mcp.request_id.as_str()]);
    let found: Vec<(RequestStatus, i64)> = listed.iter().map(|r| (r.status, r.status_at)).collect();
    let pending = RequestStatus::PendingApproval;
    assert_eq!(found, [(pending, 1_000_010), (pending, 1_000_000)]);
    // The owner's approval is the time of the answer.
    rig.advance(100);
    let answer_item = request_items(&rig, &over_mcp.request_id)[0].id;
    rig.engine.approve("kim", answer_item, None).await.unwrap();
    let answered = rig.engine.caller_request(&bot, &over_mcp.request_id).unwrap();
    assert_eq!(
        (answered.status, answered.status_at),
        (RequestStatus::Answered, 1_000_120)
    );
}

/// An LLM that replies only after the test lets it, so the test can act while the agent works.
struct GatedLlm {
    reply: String,
    called: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl LlmClient for GatedLlm {
    fn complete(&self, _request: CompletionRequest) -> BoxFuture<'_, Result<CompletionResponse, LlmError>> {
        Box::pin(async move {
            self.called.notify_one();
            self.release.notified().await;
            Ok(CompletionResponse {
                content: self.reply.clone(),
                stop: StopReason::End,
                usage: Usage {
                    input_tokens: 10,
                    output_tokens: 0,
                },
            })
        })
    }
}

struct GatedBackends(Arc<GatedLlm>);

impl Backends for GatedBackends {
    fn build<'a>(&'a self, _profile: &'a str, _config: &'a ProfileConfig) -> BoxFuture<'a, Result<Runtime, String>> {
        let llm: Arc<dyn LlmClient> = self.0.clone();
        Box::pin(async move {
            Ok(Runtime {
                llm: Ok(llm),
                connectors: Default::default(),
            })
        })
    }
}

#[tokio::test]
async fn a_cancel_during_the_drain_withdraws_new_items() {
    // The agent answers in one case and ignores the question in the other. Neither leaves an item open.
    for reply in [answer("Friday."), ignore()] {
        let dir = tempfile::tempdir().unwrap();
        let llm = Arc::new(GatedLlm {
            reply,
            called: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let backends = Arc::new(GatedBackends(llm.clone()));
        let engine = Engine::open(dir.path(), backends, Arc::new(|| 1_000_000)).unwrap();
        engine.create_profile("kim", config()).await.unwrap();
        engine.set_profile_config("kim", open(config())).await.unwrap();
        let (_, token) = engine.app_store().add_caller_token("kim", "bot", 1).unwrap();
        let bot = engine.app_store().caller(&token).unwrap().unwrap();
        let asked = engine.send_message(&bot, "When?", "m-1", None).unwrap();

        let cancel = async {
            // The agent works on the question now.
            llm.called.notified().await;
            let canceled = engine.cancel_request(&bot, &asked.request_id).unwrap();
            llm.release.notify_one();
            canceled
        };
        let (failures, canceled) = tokio::join!(engine.drain_intake(), cancel);
        assert!(failures.is_empty(), "{failures:?}");
        assert_eq!(canceled.status, RequestStatus::Canceled);

        let inbox = engine.snapshot("kim").await.unwrap().inbox;
        let open_items: Vec<_> = inbox
            .iter()
            .filter(|item| matches!(item.state, ApprovalState::Pending | ApprovalState::Draft))
            .collect();
        assert!(open_items.is_empty(), "{open_items:?}");
        let kinds = audit_kinds(&engine).await;
        assert!(kinds.contains(&"inbound_canceled".to_owned()), "{kinds:?}");
        let state = engine.caller_request(&bot, &asked.request_id).unwrap();
        assert_eq!(state.status, RequestStatus::Canceled);
        // The question is not handed to the agent again.
        assert!(engine.drain_intake().await.is_empty());
    }
}
