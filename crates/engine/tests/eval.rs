//! Replay evaluation: stored cases, background runs, scores without an LLM, and the owner's pass mark.

mod common;

use bunsin_api::{ApiError, EngineApi, EvalReport, ProfileConfig, ReplayCase};
use bunsin_core::eval::{DraftKind, EvalPass, Expect, RunState};
use bunsin_core::llm::LlmError;
use common::*;
use serde_json::json;

const EMAIL: &str = "lee.park@corp.example.com";
const PHONE: &str = "010-9876-5432";

fn case(id: &str, text: &str, actual_reply: &str, expect: Expect) -> ReplayCase {
    ReplayCase {
        event: event(id, "lee", text),
        actual_reply: actual_reply.into(),
        expect,
    }
}

fn escalate() -> String {
    json!({"kind": "escalate", "rationale": "money", "summary": "Lee asks about pay.", "tool": null,
        "args_json": null})
    .to_string()
}

/// Runs background ticks until the newest run is done. Fails after `max` ticks.
async fn run_to_end(rig: &Rig, max: usize) -> EvalReport {
    for _ in 0..max {
        assert!(rig.engine.background_tick().await.is_empty());
        let report = rig.engine.eval_report("kim").await.unwrap();
        if report.run.as_ref().is_some_and(|run| run.state == RunState::Done) {
            return report;
        }
    }
    panic!("the run did not end in {max} ticks")
}

async fn set_pass(rig: &Rig, pass: EvalPass) {
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.eval.pass = Some(pass);
    rig.engine.set_profile_config("kim", config).await.unwrap();
}

#[tokio::test]
async fn a_run_scores_kinds_overlap_boundary_and_reads_a_few_cases_per_tick() {
    let rig = rig().await;
    let cases = vec![
        case("c1", "when do we deploy?", "We deploy at 3 pm.", Expect::Reply),
        case("c2", "is the build green?", "Yes, it is green.", Expect::Reply),
        case("c3", "can I get a raise?", "", Expect::Escalate),
        case("c4", "fyi: lunch is here", "", Expect::Ignore),
        case("c5", "who owns the notes?", "Mina.", Expect::Reply),
    ];
    let ids = rig.engine.add_replay_cases("kim", cases).await.unwrap();
    assert_eq!(ids.len(), 5);
    rig.llm
        .reply(&post("We deploy at 3 pm."), 10)
        .reply(&escalate(), 10)
        .reply(&escalate(), 10)
        .reply(&call("slack/search", json!({"query": "lunch"})), 10)
        .push(Err(LlmError::Http {
            status: 500,
            body: format!("quota of {EMAIL} is used up"),
        }));
    set_pass(
        &rig,
        EvalPass {
            kind_percent: 50,
            overlap_percent: 50,
            boundary_percent: 100,
        },
    )
    .await;
    let run = rig.engine.start_eval("kim").await.unwrap();

    // A tick replays three cases.
    assert!(rig.engine.background_tick().await.is_empty());
    let report = rig.engine.eval_report("kim").await.unwrap();
    assert_eq!((report.results.len(), report.remaining, report.passed), (3, 2, None));
    assert_eq!(rig.llm.requests().len(), 3);

    let report = run_to_end(&rig, 3).await;
    assert_eq!(report.run.as_ref().unwrap().id, run);
    let kinds: Vec<Option<DraftKind>> = report
        .results
        .iter()
        .map(|result| result.score.map(|score| score.kind))
        .collect();
    use DraftKind::*;
    assert_eq!(kinds, [Some(Reply), Some(Escalate), Some(Escalate), Some(Read), None]);
    assert_eq!(report.results[0].draft, "We deploy at 3 pm.");
    assert_eq!(report.results[0].score.unwrap().overlap_permille, Some(1000));
    let s = &report.summary;
    assert_eq!((s.cases, s.errors), (5, 1));
    assert_eq!(s.kind_percent, Some(50));
    assert_eq!(s.overlap_percent, Some(50));
    assert_eq!(s.boundary_percent, Some(100));
    assert_eq!(s.over_escalation_percent, Some(50));
    assert_eq!(s.read_percent, Some(25));
    // Every rate meets the mark, but a failed case keeps the run from passing.
    assert_eq!(report.passed, Some(false));

    // The failure is stored after the privacy filter.
    let error = report.results[4].error.as_deref().unwrap();
    assert!(error.contains("[REDACTED:EMAIL]"), "{error}");
    assert!(!rig.stores(EMAIL));
    // A replay runs no tool and calls the LLM once per case.
    assert!(rig.slack.calls().is_empty());
    assert_eq!(rig.llm.requests().len(), 5);
}

#[tokio::test]
async fn a_run_goes_on_while_the_kill_switch_is_on_and_passes_the_owners_mark() {
    let rig = rig().await;
    let cases = vec![
        case("c1", "when do we deploy?", "We deploy at 3 pm.", Expect::Reply),
        case("c2", "can I get a raise?", "", Expect::Escalate),
    ];
    rig.engine.add_replay_cases("kim", cases).await.unwrap();
    rig.llm.reply(&post("We deploy at 3 pm."), 10).reply(&escalate(), 10);
    set_pass(
        &rig,
        EvalPass {
            kind_percent: 100,
            overlap_percent: 100,
            boundary_percent: 100,
        },
    )
    .await;
    rig.engine.set_kill_switch("kim", true).await.unwrap();
    rig.engine.start_eval("kim").await.unwrap();
    let report = run_to_end(&rig, 2).await;
    assert_eq!(report.passed, Some(true), "{report:?}");
    assert_eq!(report.remaining, 0);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert!(audit.iter().any(|entry| entry.kind == "eval_finished"));
}

#[tokio::test]
async fn cases_are_filtered_before_they_are_stored() {
    let rig = rig().await;
    let personal = ReplayCase {
        event: event("c1", EMAIL, &format!("Call me at {PHONE}.")),
        actual_reply: format!("I mailed {EMAIL}."),
        expect: Expect::Reply,
    };
    rig.engine.add_replay_cases("kim", vec![personal]).await.unwrap();
    for value in [EMAIL, PHONE] {
        assert!(!rig.stores(value), "{value} was written to disk");
    }
    let stored = &rig.engine.replay_cases("kim").await.unwrap()[0];
    assert_eq!(stored.event.text, "Call me at [REDACTED:PHONE].");
    assert_eq!(stored.event.author, "[REDACTED:EMAIL]");
    assert_eq!(stored.actual_reply, "I mailed [REDACTED:EMAIL].");
    assert_eq!(stored.expect, Expect::Reply);
}

#[tokio::test]
async fn one_run_at_a_time_and_a_deleted_case_keeps_its_result() {
    let rig = rig().await;
    let refused = rig.engine.start_eval("kim").await;
    assert!(
        matches!(refused, Err(ApiError::NotAllowed(ref e)) if e.contains("no replay case")),
        "{refused:?}"
    );
    let report = rig.engine.eval_report("kim").await.unwrap();
    assert_eq!((report.run, report.results.len()), (None, 0));

    let ids = rig
        .engine
        .add_replay_cases("kim", vec![case("c1", "when?", "At 3 pm.", Expect::Reply)])
        .await
        .unwrap();
    rig.llm.reply(&post("At 4 pm."), 10);
    rig.engine.start_eval("kim").await.unwrap();
    let again = rig.engine.start_eval("kim").await;
    assert!(
        matches!(again, Err(ApiError::NotAllowed(ref e)) if e.contains("running")),
        "{again:?}"
    );
    let report = run_to_end(&rig, 2).await;
    // No pass mark: the report shows the rates only.
    assert_eq!((report.pass, report.passed), (None, None));

    rig.engine.delete_replay_case("kim", ids[0]).await.unwrap();
    let gone = rig.engine.delete_replay_case("kim", ids[0]).await;
    assert!(matches!(gone, Err(ApiError::NotFound(_))), "{gone:?}");
    assert_eq!(rig.engine.eval_report("kim").await.unwrap(), report);
    assert!(rig.engine.replay_cases("kim").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_single_replay_returns_its_score() {
    let rig = rig().await;
    rig.llm.reply(&post("We deploy at 3 pm."), 10);
    let result = rig
        .engine
        .replay(
            "kim",
            case("c1", "when do we deploy?", "we deploy at 3 PM.", Expect::Reply),
        )
        .await
        .unwrap();
    let score = result.score.unwrap();
    assert_eq!(
        (score.kind, score.kind_ok, score.overlap_permille),
        (DraftKind::Reply, true, Some(1000))
    );
    // The single replay stores no case and no run.
    assert!(rig.engine.replay_cases("kim").await.unwrap().is_empty());
    assert_eq!(rig.engine.eval_report("kim").await.unwrap().run, None);
}

#[tokio::test]
async fn the_pass_mark_is_checked_and_old_settings_still_load() {
    let rig = rig().await;
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config.eval.pass = Some(EvalPass {
        kind_percent: 80,
        overlap_percent: 101,
        boundary_percent: 90,
    });
    let refused = rig.engine.set_profile_config("kim", config).await;
    assert!(
        matches!(refused, Err(ApiError::Invalid(ref e)) if e.contains("overlap_percent")),
        "{refused:?}"
    );

    let mut old = serde_json::to_value(ProfileConfig::new("Kim")).unwrap();
    old.as_object_mut().unwrap().remove("eval");
    let loaded: ProfileConfig = serde_json::from_value(old).unwrap();
    assert_eq!(loaded.eval.pass, None);
}
