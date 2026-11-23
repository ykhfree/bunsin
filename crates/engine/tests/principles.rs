//! The owner's principles as the window reads and replaces them.

mod common;

use bunsin_api::{ApiError, EngineApi, Expected, PrinciplesUpdate};
use common::*;

fn update(principles: &[&str], expected: Expected<Vec<String>>) -> PrinciplesUpdate {
    PrinciplesUpdate {
        principles: principles.iter().map(|p| (*p).to_owned()).collect(),
        expected,
    }
}

#[tokio::test]
async fn principles_are_replaced_as_a_list_and_reach_the_prompt_as_written() {
    let rig = rig().await;
    assert!(rig.engine.principles("kim").await.unwrap().is_empty());
    let before = rig.engine.profile_config("kim").await.unwrap();

    // The owner's own words are not filtered, also an address of the owner.
    let mine = ["  Answer in two sentences.\n", "Reply from kim@example.com only."];
    rig.engine
        .set_principles("kim", update(&mine, Expected::Value(Vec::new())))
        .await
        .unwrap();
    let stored = rig.engine.principles("kim").await.unwrap();
    assert_eq!(stored, ["Answer in two sentences.", "Reply from kim@example.com only."]);
    // The other settings are as they were.
    let after = rig.engine.profile_config("kim").await.unwrap();
    assert_eq!(
        (&after.policy, &after.polls, &after.privacy),
        (&before.policy, &before.polls, &before.privacy)
    );

    // The audit entry counts. It does not hold the text.
    let reordered = [
        "Reply from kim@example.com only.",
        "Answer in two sentences.",
        "Be kind.",
    ];
    rig.engine
        .set_principles("kim", update(&reordered, Expected::Value(stored)))
        .await
        .unwrap();
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    let entry = snapshot.audit.iter().find(|a| a.kind == "principles_changed").unwrap();
    assert_eq!(
        entry.detail,
        serde_json::json!({"before": 2, "after": 3, "added": 1, "removed": 0})
    );

    // The next prompt has the principles in their order.
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", event("e1", "lee", "hello")).await.unwrap();
    let system = rig.llm.requests()[0].system.clone();
    let first = system
        .find("Reply from kim@example.com only.")
        .expect("the first principle");
    let second = system.find("Answer in two sentences.").expect("the second principle");
    assert!(first < second, "{system}");
}

#[tokio::test]
async fn a_stale_or_invalid_list_is_refused_and_nothing_is_stored() {
    let rig = rig().await;
    let loaded = rig.engine.principles("kim").await.unwrap();
    rig.engine
        .set_principles("kim", update(&["Answer briefly."], Expected::Unchecked))
        .await
        .unwrap();
    // A save that started from the list before is a conflict.
    let stale = rig
        .engine
        .set_principles("kim", update(&["Something else."], Expected::Value(loaded)))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");

    let too_many: Vec<&str> = vec!["Be brief."; 51];
    let long = "x".repeat(1_001);
    let refused = [
        (vec!["Fine.", "   "], "principle 2 is empty"),
        (too_many, "at most 50 principles"),
        (vec![long.as_str()], "principle 1 has 1001 characters"),
    ];
    for (list, reason) in refused {
        let result = rig
            .engine
            .set_principles("kim", update(&list, Expected::Unchecked))
            .await;
        assert!(
            matches!(&result, Err(ApiError::Invalid(e)) if e.contains(reason)),
            "{result:?}"
        );
    }
    assert_eq!(rig.engine.principles("kim").await.unwrap(), ["Answer briefly."]);
    let changes = rig.engine.snapshot("kim").await.unwrap().audit;
    assert_eq!(changes.iter().filter(|a| a.kind == "principles_changed").count(), 1);
}
