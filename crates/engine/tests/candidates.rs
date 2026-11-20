//! Principle candidates: the LLM proposes rules from the owner's texts, and only the owner adopts them.

mod common;

use bunsin_api::{AdoptPrinciple, ApiError, EngineApi, Expected, PrinciplesUpdate, ReplayCase};
use bunsin_core::candidates::{CandidateState, PrincipleSource};
use bunsin_core::context::Correction;
use bunsin_core::eval::Expect;
use bunsin_core::llm::LlmError;
use common::*;
use serde_json::json;

const EMAIL: &str = "lee.park@corp.example.com";

fn candidates(list: &[(&str, &str)]) -> String {
    let list: Vec<_> = list
        .iter()
        .map(|(text, evidence)| json!({"text": text, "evidence": evidence}))
        .collect();
    json!({ "candidates": list }).to_string()
}

fn correction(edited: &str, reason: &str) -> Correction {
    Correction {
        tool: "slack/post".into(),
        source: "slack".into(),
        task: "when?".into(),
        original: "A long draft.".into(),
        edited: edited.into(),
        reason: reason.into(),
    }
}

fn read(tool: &str) -> PrincipleSource {
    PrincipleSource::Read {
        tool: tool.into(),
        args: json!({"query": "from:kim"}).as_object().unwrap().clone(),
    }
}

/// The text of the last LLM request.
fn last_request(rig: &Rig) -> String {
    rig.llm.requests().last().unwrap().messages[0].content.clone()
}

#[tokio::test]
async fn each_source_gives_the_llm_the_owners_texts_as_quoted_data() {
    let rig = rig().await;
    let store = rig.kim_store();
    store.add_correction(1, &correction("Short.", "too long"), 5).unwrap();
    let retired = store.add_correction(2, &correction("Old style.", "old"), 6).unwrap();
    store.retire_correction(retired, 7).unwrap();

    rig.llm.reply(&candidates(&[("Answer in one line.", "Short.")]), 10);
    let found = rig
        .engine
        .extract_principles("kim", PrincipleSource::Corrections)
        .await
        .unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].text.as_str(), found[0].source.as_str(), found[0].state),
        ("Answer in one line.", "corrections", CandidateState::New)
    );
    let asked = last_request(&rig);
    assert!(
        asked.contains("<data source=\"owner-text\">\nShort.\n</data>"),
        "{asked}"
    );
    assert!(asked.contains("too long") && !asked.contains("Old style."), "{asked}");

    let case = ReplayCase {
        event: event("c1", "lee", "when do we deploy?"),
        actual_reply: "Thursday, and I will confirm it in Jira.".into(),
        expect: Expect::Reply,
    };
    rig.engine.add_replay_cases("kim", vec![case]).await.unwrap();
    rig.llm
        .reply(&candidates(&[("Confirm dates in Jira.", "confirm it in Jira")]), 10);
    let found = rig
        .engine
        .extract_principles("kim", PrincipleSource::ReplayCases)
        .await
        .unwrap();
    assert_eq!((found.len(), found[0].source.as_str()), (1, "replay_cases"));

    rig.slack.set_result(
        "search",
        json!({"messages": [{"text": format!("I never promise dates. Mail {EMAIL}.")}]}),
    );
    rig.llm
        .reply(&candidates(&[("Never promise a date.", "I never promise dates.")]), 10);
    let found = rig
        .engine
        .extract_principles("kim", read("slack/search"))
        .await
        .unwrap();
    assert_eq!((found.len(), found[0].source.as_str()), (1, "read:slack/search"));
    let calls = rig.slack.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].0.as_str(), &calls[0].1["query"]),
        ("search", &json!("from:kim"))
    );
    let asked = last_request(&rig);
    assert!(
        asked.contains("I never promise dates.") && !asked.contains(EMAIL),
        "{asked}"
    );
    assert!(!rig.stores(EMAIL));

    // Only a tool with a read rule is read.
    for tool in ["slack/post", "slack/unknown"] {
        let refused = rig.engine.extract_principles("kim", read(tool)).await;
        assert!(
            matches!(refused, Err(ApiError::NotAllowed(ref e)) if e.contains("read")),
            "{refused:?}"
        );
    }
    assert_eq!(rig.slack.calls().len(), 1);
    assert_eq!(rig.llm.requests().len(), 3);
    let usage = rig.engine.llm_usage("kim").await.unwrap();
    assert!(
        usage.iter().any(|u| u.purpose == "principles" && u.calls == 3),
        "{usage:?}"
    );
}

#[tokio::test]
async fn the_engine_drops_long_redacted_unquoted_and_known_candidates() {
    let mut config = config();
    config.principles = vec!["Be polite.".into()];
    let rig = rig_with(config).await;
    rig.kim_store()
        .add_correction(1, &correction("Short and polite.", "too long"), 5)
        .unwrap();
    let longest = "가".repeat(300);
    let longer = "가".repeat(301);
    rig.llm.reply(
        &candidates(&[
            ("Keep it short.", "Short and polite."),
            (&longest, "too long"),
            (&longer, "too long"),
            (&format!("Mail {EMAIL} first."), "too long"),
            ("Call [REDACTED:PHONE] first.", "too long"),
            ("Use emoji.", "with emoji"),
            ("Be polite.", "polite"),
        ]),
        10,
    );
    let found = rig
        .engine
        .extract_principles("kim", PrincipleSource::Corrections)
        .await
        .unwrap();
    let texts: Vec<&str> = found.iter().map(|c| c.text.as_str()).collect();
    assert_eq!(texts, ["Keep it short.", longest.as_str()]);
    assert!(!rig.stores(EMAIL));
    // Nothing became a principle.
    assert_eq!(rig.engine.principles("kim").await.unwrap(), ["Be polite."]);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let entry = audit.iter().find(|e| e.kind == "principles_extracted").unwrap();
    assert_eq!(entry.detail, json!({"source": "corrections", "proposed": 7, "kept": 2}));
}

#[tokio::test]
async fn an_adoption_appends_one_text_and_a_candidate_is_decided_once() {
    let mut config = config();
    config.principles = vec!["Be polite.".into()];
    let rig = rig_with(config).await;
    let store = rig.kim_store();
    let first = store
        .add_principle_candidate("Ask first.", "ask me", "corrections", 5)
        .unwrap();
    let second = store
        .add_principle_candidate("Be brief.", "short", "corrections", 5)
        .unwrap();
    let third = store
        .add_principle_candidate("Use emoji.", "smile", "corrections", 5)
        .unwrap();
    let adopt = |id, text: Option<&str>, expected: &[&str]| AdoptPrinciple {
        id,
        text: text.map(str::to_owned),
        expected: Expected::Value(expected.iter().map(|p| p.to_string()).collect()),
    };

    rig.engine
        .adopt_principle("kim", adopt(first, None, &["Be polite."]))
        .await
        .unwrap();
    assert_eq!(
        rig.engine.principles("kim").await.unwrap(),
        ["Be polite.", "Ask first."]
    );
    let again = rig
        .engine
        .adopt_principle("kim", adopt(first, None, &["Be polite.", "Ask first."]))
        .await;
    assert!(
        matches!(again, Err(ApiError::NotAllowed(ref e)) if e.contains("adopted")),
        "{again:?}"
    );
    let dismiss = rig.engine.dismiss_principle("kim", first).await;
    assert!(matches!(dismiss, Err(ApiError::NotAllowed(_))), "{dismiss:?}");

    // The form loaded the principles before the first adoption.
    let stale = rig
        .engine
        .adopt_principle("kim", adopt(second, None, &["Be polite."]))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    assert_eq!(store.principle_candidate(second).unwrap().state, CandidateState::New);
    rig.engine
        .adopt_principle(
            "kim",
            adopt(second, Some("  Be brief, but kind. "), &["Be polite.", "Ask first."]),
        )
        .await
        .unwrap();
    assert_eq!(
        rig.engine.principles("kim").await.unwrap(),
        ["Be polite.", "Ask first.", "Be brief, but kind."]
    );

    rig.engine.dismiss_principle("kim", third).await.unwrap();
    assert_eq!(
        store.principle_candidate(third).unwrap().state,
        CandidateState::Dismissed
    );
    // A dismissed text is not proposed again.
    store
        .add_correction(1, &correction("No emoji.", "smile less"), 5)
        .unwrap();
    rig.llm.reply(&candidates(&[("Use emoji.", "smile less")]), 10);
    let found = rig
        .engine
        .extract_principles("kim", PrincipleSource::Corrections)
        .await
        .unwrap();
    assert!(found.is_empty(), "{found:?}");

    // A save that the limits refuse leaves the candidate new.
    let full = PrinciplesUpdate {
        principles: (0..50).map(|n| format!("Rule {n}.")).collect(),
        expected: Expected::Unchecked,
    };
    rig.engine.set_principles("kim", full).await.unwrap();
    let fourth = store
        .add_principle_candidate("One more.", "more", "corrections", 5)
        .unwrap();
    let refused = rig
        .engine
        .adopt_principle(
            "kim",
            AdoptPrinciple {
                id: fourth,
                text: None,
                expected: Expected::Unchecked,
            },
        )
        .await;
    assert!(
        matches!(refused, Err(ApiError::Invalid(ref e)) if e.contains("at most 50")),
        "{refused:?}"
    );
    assert_eq!(store.principle_candidate(fourth).unwrap().state, CandidateState::New);
    assert_eq!(rig.engine.principles("kim").await.unwrap().len(), 50);

    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let adopted: Vec<_> = audit
        .iter()
        .filter(|e| e.kind == "principle_adopted")
        .map(|e| e.detail["edited"].clone())
        .collect();
    // Newest first.
    assert_eq!(adopted, [json!(true), json!(false)]);
}

#[tokio::test]
async fn an_extraction_without_texts_or_with_a_failed_llm_is_refused() {
    let rig = rig().await;
    let empty = rig.engine.extract_principles("kim", PrincipleSource::Corrections).await;
    assert!(
        matches!(empty, Err(ApiError::NotAllowed(ref e)) if e.contains("no text")),
        "{empty:?}"
    );
    assert!(rig.llm.requests().is_empty());

    rig.kim_store()
        .add_correction(1, &correction("Short.", "long"), 5)
        .unwrap();
    rig.llm.push(Err(LlmError::Http {
        status: 500,
        body: format!("quota of {EMAIL} is used up"),
    }));
    let failed = rig.engine.extract_principles("kim", PrincipleSource::Corrections).await;
    let Err(ApiError::NotAllowed(error)) = failed else {
        panic!("expected a refusal, got {failed:?}")
    };
    assert!(error.contains("[REDACTED:EMAIL]") && !error.contains(EMAIL), "{error}");
    rig.llm.reply("not a list", 10);
    let invalid = rig.engine.extract_principles("kim", PrincipleSource::Corrections).await;
    assert!(
        matches!(invalid, Err(ApiError::NotAllowed(ref e)) if e.contains("not a list")),
        "{invalid:?}"
    );
    assert!(rig.engine.principle_candidates("kim").await.unwrap().is_empty());
}
