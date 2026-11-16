//! The LLM call log: one row per call, with its job, purpose, tokens and how it ended.

mod common;

use bunsin_api::{EngineApi, Outcome, ReplayCase};
use bunsin_core::llm::{CompletionResponse, LlmError, StopReason, Usage};
use bunsin_store::{AppStore, LlmCall};
use common::*;

fn calls(rig: &Rig) -> Vec<LlmCall> {
    let store = AppStore::open(rig.dir.path()).unwrap().open_profile("kim").unwrap();
    let mut calls = store.llm_calls(100).unwrap();
    calls.reverse();
    calls
}

#[tokio::test]
async fn a_repaired_reply_leaves_two_rows_with_the_same_job() {
    let rig = rig().await;
    rig.llm.reply("Sure, I will post it.", 40).reply(&ignore(), 25);
    let event = event("e1", "lee", "please answer");
    let key = event.dedupe_key();
    assert_eq!(rig.engine.ingest("kim", event).await.unwrap(), Outcome::Ignored);

    let calls = calls(&rig);
    assert_eq!(calls.len(), 2, "{calls:?}");
    for (call, tokens) in calls.iter().zip([40, 25]) {
        assert_eq!(call.job.as_deref(), Some(key.as_str()));
        assert_eq!((call.purpose.as_str(), call.outcome.as_str()), ("event", "end"));
        assert_eq!((call.input_tokens, call.at), (tokens, 1_000_000));
    }
}

#[tokio::test]
async fn a_failed_call_leaves_a_row_with_the_kind_of_error() {
    let rig = rig().await;
    rig.llm
        .push(Err(LlmError::Transport("connection refused".into())))
        .push(Err(LlmError::Http {
            status: 500,
            body: "upstream said kim@example.com".into(),
        }));
    let first = rig.engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap();
    assert!(matches!(first, Outcome::Failed { .. }), "{first:?}");
    let second = rig.engine.ingest("kim", event("e2", "lee", "hi")).await.unwrap();
    assert!(matches!(second, Outcome::Failed { .. }), "{second:?}");

    let calls = calls(&rig);
    let kinds: Vec<_> = calls
        .iter()
        .map(|c| (c.outcome.as_str(), c.input_tokens, c.output_tokens))
        .collect();
    assert_eq!(kinds, [("transport_error", 0, 0), ("http_error", 0, 0)]);
    let usage = rig.engine.llm_usage("kim").await.unwrap();
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!((usage[0].calls, usage[0].failed), (2, 2));
}

#[tokio::test]
async fn each_purpose_is_logged_and_added_up_apart() {
    let rig = rig().await;
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap();
    rig.llm.push(Ok(CompletionResponse {
        content: "{".into(),
        stop: StopReason::MaxTokens,
        usage: Usage {
            input_tokens: 7,
            output_tokens: 3,
        },
    }));
    let case = ReplayCase {
        event: event("old-1", "lee", "when is the deploy?"),
        actual_reply: "Friday.".into(),
    };
    rig.engine.replay("kim", case.clone()).await.unwrap();
    rig.llm.reply(&ignore(), 5);
    assert!(rig.engine.test_llm("kim").await.unwrap().works);

    let calls = calls(&rig);
    let rows: Vec<_> = calls
        .iter()
        .map(|c| (c.purpose.as_str(), c.job.clone(), c.outcome.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("event", Some(event("e1", "lee", "hi").dedupe_key()), "end"),
            ("replay", Some(case.event.dedupe_key()), "max_tokens"),
            ("llm_test", None, "end"),
        ]
    );

    let usage = rig.engine.llm_usage("kim").await.unwrap();
    let rows: Vec<_> = usage
        .iter()
        .map(|u| (u.purpose.as_str(), u.calls, u.failed, u.input_tokens, u.output_tokens))
        .collect();
    assert_eq!(
        rows,
        [("event", 1, 0, 10, 0), ("llm_test", 1, 0, 5, 0), ("replay", 1, 1, 7, 3)]
    );
    // The test clock is at 1,000,000 s: 1970-01-12 UTC.
    assert!(usage.iter().all(|u| u.day == "1970-01-12"), "{usage:?}");
}

#[tokio::test]
async fn usage_leaves_out_calls_older_than_90_days() {
    let rig = rig().await;
    rig.llm.reply(&ignore(), 10).reply(&ignore(), 20);
    rig.engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap();
    rig.advance(90 * 24 * 60 * 60 + 1);
    rig.engine.ingest("kim", event("e2", "lee", "hi")).await.unwrap();
    let usage = rig.engine.llm_usage("kim").await.unwrap();
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!((usage[0].calls, usage[0].input_tokens), (1, 20));
}
