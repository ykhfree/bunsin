//! The polls as the window reads and replaces them.

mod common;

use bunsin_api::{ApiError, EngineApi, Expected, PollsUpdate};
use bunsin_core::poll::PollConfig;
use common::*;
use serde_json::json;

fn update(polls: Vec<PollConfig>, expected: Expected<Vec<PollConfig>>) -> PollsUpdate {
    PollsUpdate { polls, expected }
}

#[tokio::test]
async fn polls_are_replaced_as_a_list_and_run_by_the_server() {
    let rig = rig().await;
    let loaded = rig.engine.polls("kim").await.unwrap();
    assert!(loaded.is_empty());
    let before = rig.engine.profile_config("kim").await.unwrap();

    rig.engine
        .set_polls("kim", update(vec![poll()], Expected::Value(loaded)))
        .await
        .unwrap();
    assert_eq!(rig.engine.polls("kim").await.unwrap(), [poll()]);
    let after = rig.engine.profile_config("kim").await.unwrap();
    assert_eq!(
        (&after.policy, &after.principles, &after.approved_tools),
        (&before.policy, &before.principles, &before.approved_tools)
    );
    // The audit entry counts. The arguments of a search can name people, so they are not in it.
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let entry = audit.iter().find(|a| a.kind == "polls_changed").unwrap();
    assert_eq!(entry.detail, json!({"before": 0, "after": 1}));

    // The saved poll is the one that the test and the server's timer run.
    rig.slack.set_result(
        "search",
        json!({"messages": [message("1", "lee", "hi"), message("2", "kim", "mine")]}),
    );
    let preview = rig.engine.test_poll("kim", 0).await.unwrap();
    assert_eq!((preview.items, preview.events.len()), (2, 1));
    rig.engine.poll_profile("kim").await.unwrap();
    let status = &rig.engine.snapshot("kim").await.unwrap().polls[0];
    assert_eq!((status.items, status.usable), (2, 1));
}

#[tokio::test]
async fn a_poll_that_the_server_would_not_run_is_refused_and_nothing_is_stored() {
    let rig = rig().await;
    let with = |change: fn(&mut PollConfig)| {
        let mut poll = poll();
        change(&mut poll);
        poll
    };
    let refused = [
        // A poll runs without the owner. Its tool must only read.
        (with(|p| p.tool = "post".into()), "needs a rule with the class `read`"),
        (with(|p| p.tool = "unknown".into()), "has no permission rule"),
        (with(|p| p.every_secs = 29), "the shortest interval is 30 s"),
        (with(|p| p.mapping.author = " ".into()), "no `mapping.author` path"),
    ];
    for (poll, reason) in refused {
        let result = rig
            .engine
            .set_polls("kim", update(vec![poll], Expected::Unchecked))
            .await;
        assert!(
            matches!(&result, Err(ApiError::Invalid(e)) if e.contains(reason)),
            "{result:?}"
        );
    }

    // A read rule for a tool that the owner never approved is not enough.
    let mut config = rig.engine.profile_config("kim").await.unwrap();
    config
        .policy
        .rules
        .insert("slack/history".into(), bunsin_core::policy::ToolRule::read());
    rig.engine.set_profile_config("kim", config).await.unwrap();
    let unapproved = with(|p| p.tool = "history".into());
    let result = rig
        .engine
        .set_polls("kim", update(vec![unapproved], Expected::Unchecked))
        .await;
    assert!(
        matches!(&result, Err(ApiError::Invalid(e)) if e.contains("not in the approved list")),
        "{result:?}"
    );
    assert!(rig.engine.polls("kim").await.unwrap().is_empty());

    // A save that started from an older list is a conflict.
    rig.engine
        .set_polls("kim", update(vec![poll()], Expected::Unchecked))
        .await
        .unwrap();
    let stale = rig
        .engine
        .set_polls("kim", update(Vec::new(), Expected::Value(Vec::new())))
        .await;
    assert!(matches!(stale, Err(ApiError::Conflict(_))), "{stale:?}");
    assert_eq!(rig.engine.polls("kim").await.unwrap(), [poll()]);
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    assert_eq!(audit.iter().filter(|a| a.kind == "polls_changed").count(), 1);
}
