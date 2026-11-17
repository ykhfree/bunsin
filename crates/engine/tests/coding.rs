//! Coding delegation in the engine: the hint, the approval it always needs, the start on the server's
//! coder, and the result that comes back as artifacts and a draft. The coder here is scripted.

mod common;

use bunsin_api::{ApiError, EngineApi, Outcome, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::coding::{CODING_START_TOOL, CodingConfig, CodingOutcome, CodingReport, TestOutcome};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::records::InboxBody;
use common::*;
use serde_json::json;

const EMAIL: &str = "lee.park@corp.example.com";

fn coding() -> CodingConfig {
    serde_json::from_value(json!({
        "backend": "command", "command": ["/bin/sh", "-c", "true"], "timeout_secs": 60,
        "repos": {"app": {"path": "/srv/secret-path/app", "test_command": ["/usr/bin/make", "test"]}}
    }))
    .unwrap()
}

fn rule(tier: Tier) -> ToolRule {
    ToolRule {
        class: ToolClass::Write,
        tier,
        outgoing_text: OutgoingText::None,
    }
}

fn with_coding(tier: Tier) -> ProfileConfig {
    let mut config = config();
    config.coding = Some(coding());
    config.policy.rules.insert(CODING_START_TOOL.into(), rule(tier));
    config
}

fn start(repo: &str) -> String {
    call(
        CODING_START_TOOL,
        json!({"repo": repo, "instructions": "Fix the login bug."}),
    )
}

/// Hands in an event that the agent answers with a coding call. Returns the inbox ID.
async fn propose_job(rig: &Rig, id: &str) -> i64 {
    rig.llm.reply(&start("app"), 10);
    match rig
        .engine
        .ingest("kim", event(id, "lee", "the login is broken"))
        .await
        .unwrap()
    {
        Outcome::PendingApproval { inbox_id } => inbox_id,
        other => panic!("not pending: {other:?}"),
    }
}

fn outcome(exit_code: Option<i32>) -> CodingOutcome {
    CodingOutcome {
        branch: "agent/b1-kim".into(),
        base_commit: "a".repeat(40),
        head_commit: "b".repeat(40),
        diff_stat: " src/login.rs | 4 ++--".into(),
        test: Some(TestOutcome {
            command: vec!["/usr/bin/make".into(), "test".into()],
            exit_code: Some(0),
            timed_out: false,
            output: format!("ok; mail {EMAIL}"),
        }),
        exit_code,
        signal: None,
        timed_out: false,
        summary: Some("Fixed it.".into()),
        transcript: format!("$ /bin/sh\nmail {EMAIL}"),
    }
}

#[tokio::test]
async fn the_hint_needs_the_settings_and_a_rule_and_names_only_the_repositories() {
    // A rule without settings, and settings without a rule, give no hint.
    let mut rule_only = config();
    rule_only
        .policy
        .rules
        .insert(CODING_START_TOOL.into(), rule(Tier::Approve));
    let mut settings_only = config();
    settings_only.coding = Some(coding());
    for config in [rule_only, settings_only] {
        let rig = rig_with(config).await;
        rig.llm.reply(&ignore(), 10);
        rig.engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap();
        assert!(!all_requests(&rig).contains(CODING_START_TOOL));
    }
    let rig = rig_with(with_coding(Tier::Approve)).await;
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap();
    let sent = all_requests(&rig);
    assert!(
        sent.contains(CODING_START_TOOL) && sent.contains("repositories: app"),
        "{sent}"
    );
    assert!(!sent.contains("secret-path") && !sent.contains("/bin/sh") && !sent.contains("make"));
}

#[tokio::test]
async fn an_autonomous_rule_still_asks_the_owner() {
    let rig = rig_with(with_coding(Tier::Autonomous)).await;
    let id = propose_job(&rig, "e1").await;
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Pending);
    assert!(rig.coder.started().is_empty());
}

#[tokio::test]
async fn an_approved_job_starts_and_its_result_becomes_artifacts_and_a_draft() {
    let rig = rig_with(with_coding(Tier::Approve)).await;
    let id = propose_job(&rig, "e1").await;
    assert_eq!(
        rig.engine.approve("kim", id, None).await.unwrap(),
        Outcome::Started { inbox_id: id }
    );
    let started = rig.coder.started();
    assert_eq!(started.len(), 1);
    assert_eq!(started[0].job_id, format!("b{id}-kim"));
    assert_eq!(started[0].repo_path, std::path::PathBuf::from("/srv/secret-path/app"));
    assert_eq!(started[0].base_ref, "HEAD");
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Executing);
    // Nothing is recorded before the job ends.
    assert!(rig.engine.background_tick().await.is_empty());
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Executing);

    rig.coder.finish(CodingReport {
        profile: "kim".into(),
        inbox_id: id,
        outcome: Ok(outcome(Some(0))),
    });
    assert!(rig.engine.background_tick().await.is_empty());
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Executed);
    let snapshot = rig.engine.snapshot("kim").await.unwrap();
    let kinds: Vec<&str> = snapshot
        .artifacts
        .iter()
        .filter(|a| a.inbox_id == Some(id))
        .map(|a| a.kind.as_str())
        .collect();
    assert!(
        kinds.contains(&"coding_result") && kinds.contains(&"coding_transcript"),
        "{kinds:?}"
    );
    let draft = &snapshot.inbox[0];
    assert_eq!(draft.state, ApprovalState::Draft);
    let InboxBody::Escalation { summary } = &draft.body else {
        panic!("not a draft: {:?}", draft.body);
    };
    assert!(
        summary.contains("Branch `agent/b1-kim`") && summary.contains("merge it yourself"),
        "{summary}"
    );
    // The output of the job is stored filtered.
    assert!(!rig.stores(EMAIL));
}

#[tokio::test]
async fn a_failed_job_fails_its_item_and_still_tells_the_owner() {
    let rig = rig_with(with_coding(Tier::Approve)).await;
    let first = propose_job(&rig, "e1").await;
    rig.engine.approve("kim", first, None).await.unwrap();
    let second = propose_job(&rig, "e2").await;
    rig.engine.approve("kim", second, None).await.unwrap();
    rig.coder.finish(CodingReport {
        profile: "kim".into(),
        inbox_id: first,
        outcome: Ok(outcome(Some(1))),
    });
    rig.coder.finish(CodingReport {
        profile: "kim".into(),
        inbox_id: second,
        outcome: Err("branch agent/b2-kim already exists".into()),
    });
    assert!(rig.engine.background_tick().await.is_empty());
    let store = rig.kim_store();
    assert_eq!(store.inbox_item(first).unwrap().state, ApprovalState::Failed);
    assert_eq!(store.inbox_item(second).unwrap().state, ApprovalState::Failed);
    let drafts: Vec<String> = rig
        .engine
        .snapshot("kim")
        .await
        .unwrap()
        .inbox
        .into_iter()
        .filter_map(|item| match item.body {
            InboxBody::Escalation { summary } => Some(summary),
            _ => None,
        })
        .collect();
    assert_eq!(drafts.len(), 2);
    assert!(drafts.iter().any(|text| text.contains("ended with exit code 1")));
    assert!(
        drafts
            .iter()
            .any(|text| text.contains("failed before it made a branch"))
    );
}

#[tokio::test]
async fn a_coder_that_refuses_fails_the_item() {
    let rig = rig_with(with_coding(Tier::Approve)).await;
    rig.coder.refuse("this server does not run coding jobs");
    let id = propose_job(&rig, "e1").await;
    match rig.engine.approve("kim", id, None).await.unwrap() {
        Outcome::ExecutionFailed { error, .. } => assert!(error.contains("does not run coding jobs"), "{error}"),
        other => panic!("not failed: {other:?}"),
    }
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Failed);
}

#[tokio::test]
async fn a_restart_while_a_job_runs_leaves_it_unknown_and_a_late_report_is_ignored() {
    let rig = rig_with(with_coding(Tier::Approve)).await;
    let id = propose_job(&rig, "e1").await;
    rig.engine.approve("kim", id, None).await.unwrap();
    let rig = rig.restart();
    rig.engine.mark_interrupted();
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Unknown);
    rig.coder.finish(CodingReport {
        profile: "kim".into(),
        inbox_id: id,
        outcome: Ok(outcome(Some(0))),
    });
    assert!(rig.engine.background_tick().await.is_empty());
    assert_eq!(rig.kim_store().inbox_item(id).unwrap().state, ApprovalState::Unknown);
    let audit = rig.kim_store().list_audit(100).unwrap();
    assert!(audit.iter().any(|entry| entry.kind == "coding_report_ignored"));
}

#[tokio::test]
async fn coding_rules_and_settings_are_checked() {
    let rig = rig().await;
    let refuse = |config: ProfileConfig| async {
        let result = rig.engine.set_profile_config("kim", config).await;
        assert!(matches!(result, Err(ApiError::Invalid(_))), "{result:?}");
    };
    let mut read = with_coding(Tier::Approve);
    read.policy.rules.insert(CODING_START_TOOL.into(), ToolRule::read());
    refuse(read).await;
    let mut text = with_coding(Tier::Approve);
    text.policy.rules.insert(
        CODING_START_TOOL.into(),
        ToolRule::write(ToolClass::Write, OutgoingText::Arg("instructions".into())),
    );
    refuse(text).await;
    let mut other = with_coding(Tier::Approve);
    other
        .policy
        .rules
        .insert("bunsin.coding/merge".into(), rule(Tier::Approve));
    refuse(other).await;
    let mut bad = with_coding(Tier::Approve);
    bad.coding.as_mut().unwrap().command = Some(vec!["sh".into()]);
    refuse(bad).await;
    rig.engine
        .set_profile_config("kim", with_coding(Tier::Approve))
        .await
        .unwrap();
}
