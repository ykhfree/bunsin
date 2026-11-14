//! The privacy filter in the pipeline: removed text reaches neither the LLM nor the disk.

mod common;

use bunsin_api::{ApiError, EngineApi, Outcome, ReplayCase};
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use bunsin_store::AppStore;
use common::*;
use serde_json::json;

const EMAIL: &str = "lee.park@corp.example.com";
const PHONE: &str = "010-9876-5432";

fn personal_event(id: &str) -> bunsin_core::event::InboundEvent {
    event(id, EMAIL, &format!("Call me at {PHONE} or mail {EMAIL} about PROJ-12."))
}

#[tokio::test]
async fn personal_data_reaches_neither_the_llm_nor_the_disk() {
    let rig = rig().await;
    // The first reply is invalid, so a repair request is sent too.
    rig.llm.reply("not json", 10).reply(&post("I will call you."), 10);
    let outcome = rig.engine.ingest("kim", personal_event("e1")).await.unwrap();
    assert!(matches!(outcome, Outcome::PendingApproval { .. }), "{outcome:?}");

    let requests = all_requests(&rig);
    assert_eq!(rig.llm.requests().len(), 2);
    for value in [EMAIL, PHONE, "lee.park", "9876"] {
        assert!(!requests.contains(value), "{value} was sent to the LLM");
        assert!(!rig.stores(value), "{value} was written to disk");
    }
    // The rest of the text is still there for the agent.
    assert!(requests.contains("about PROJ-12"));
    assert!(requests.contains("[REDACTED:PHONE]") && requests.contains("[REDACTED:EMAIL]"));
}

#[tokio::test]
async fn the_audit_log_counts_what_was_removed() {
    let rig = rig().await;
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", personal_event("e1")).await.unwrap();
    let audit = rig.engine.snapshot("kim").await.unwrap().audit;
    let received = audit.iter().find(|entry| entry.kind == "event_received").unwrap();
    // The author and the text each held the address once.
    assert_eq!(received.detail["redacted"], json!({"EMAIL": 2, "PHONE": 1}));
    assert_eq!(received.detail["author"], "[REDACTED:EMAIL]");
}

#[tokio::test]
async fn a_custom_pattern_set_through_the_privacy_call_is_applied() {
    let rig = rig().await;
    let mut privacy = rig.engine.privacy("kim").await.unwrap();
    assert_eq!(privacy, PrivacyConfig::default());
    privacy.custom.push(CustomPattern {
        label: "employee id".into(),
        pattern: r"\bEMP-\d{6}\b".into(),
    });
    rig.engine
        .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(privacy.clone()))
        .await
        .unwrap();
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), privacy);

    rig.llm.reply(&ignore(), 10);
    rig.engine
        .ingest("kim", event("e1", "lee", "EMP-004211 needs access"))
        .await
        .unwrap();
    let requests = all_requests(&rig);
    assert!(!requests.contains("EMP-004211"));
    assert!(requests.contains("[REDACTED:employee id] needs access"));
    assert!(!rig.stores("EMP-004211"));
}

#[tokio::test]
async fn a_kind_that_is_turned_off_is_not_removed() {
    let rig = rig().await;
    let privacy = PrivacyConfig {
        phone: false,
        ..PrivacyConfig::default()
    };
    rig.engine
        .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(privacy))
        .await
        .unwrap();
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", personal_event("e1")).await.unwrap();
    let requests = all_requests(&rig);
    assert!(requests.contains(PHONE));
    assert!(!requests.contains(EMAIL));
}

#[tokio::test]
async fn the_privacy_call_changes_only_the_privacy_settings() {
    let rig = rig().await;
    let mut before = rig.engine.profile_config("kim").await.unwrap();
    before.principles = vec!["Answer briefly.".into()];
    before.polls = vec![poll()];
    rig.engine.set_profile_config("kim", before.clone()).await.unwrap();

    let privacy = PrivacyConfig {
        email: false,
        ..PrivacyConfig::default()
    };
    rig.engine
        .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(privacy.clone()))
        .await
        .unwrap();
    let after = rig.engine.profile_config("kim").await.unwrap();
    assert_eq!(after.privacy, privacy);
    before.privacy = privacy;
    assert_eq!(after, before);
}

#[tokio::test]
async fn a_bad_pattern_is_refused_and_the_old_settings_stay() {
    let rig = rig().await;
    for pattern in ["(unclosed", "x*", ""] {
        let mut privacy = PrivacyConfig::default();
        privacy.custom.push(CustomPattern {
            label: "bad".into(),
            pattern: pattern.into(),
        });
        let error = rig
            .engine
            .set_privacy("kim", bunsin_api::PrivacyUpdate::unchecked(privacy.clone()))
            .await
            .unwrap_err();
        assert!(matches!(error, ApiError::Invalid(_)), "{pattern:?}: {error:?}");
        // The same check applies when the whole settings are replaced.
        let mut config = rig.engine.profile_config("kim").await.unwrap();
        config.privacy = privacy;
        assert!(matches!(
            rig.engine.set_profile_config("kim", config).await,
            Err(ApiError::Invalid(_))
        ));
    }
    assert_eq!(rig.engine.privacy("kim").await.unwrap(), PrivacyConfig::default());
}

#[tokio::test]
async fn broken_stored_settings_stop_the_event_before_the_llm_and_the_disk() {
    let rig = rig().await;
    // Settings that the checks would refuse, written past them.
    let mut config = serde_json::to_value(rig.engine.profile_config("kim").await.unwrap()).unwrap();
    config["privacy"]["custom"] = json!([{"label": "bad", "pattern": "(unclosed"}]);
    let store = AppStore::open(rig.dir.path()).unwrap().open_profile("kim").unwrap();
    store.set_setting("config", &config.to_string()).unwrap();

    rig.llm.reply(&ignore(), 10);
    let result = rig.engine.ingest("kim", personal_event("e1")).await;
    assert!(
        matches!(result, Err(ApiError::Invalid(ref m)) if m.contains("privacy settings")),
        "{result:?}"
    );
    assert!(rig.llm.requests().is_empty());
    assert!(!rig.stores(PHONE));
}

#[tokio::test]
async fn settings_from_before_the_filter_existed_filter_everything() {
    let rig = rig().await;
    let mut config = serde_json::to_value(rig.engine.profile_config("kim").await.unwrap()).unwrap();
    let fields = config.as_object_mut().unwrap();
    fields.remove("privacy");
    fields.remove("polls");
    let store = AppStore::open(rig.dir.path()).unwrap().open_profile("kim").unwrap();
    store.set_setting("config", &config.to_string()).unwrap();

    assert_eq!(rig.engine.privacy("kim").await.unwrap(), PrivacyConfig::default());
    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", personal_event("e1")).await.unwrap();
    assert!(!all_requests(&rig).contains(EMAIL));
}

#[tokio::test]
async fn a_placeholder_in_tool_arguments_is_sent_back_for_repair_and_never_posted() {
    let rig = rig().await;
    rig.llm
        .reply(&post("Mail [REDACTED:EMAIL] for details."), 10)
        .reply(&post("I will send the details."), 10);
    let outcome = rig.engine.ingest("kim", personal_event("e1")).await.unwrap();
    let Outcome::PendingApproval { inbox_id } = outcome else {
        panic!("expected a pending item, got {outcome:?}")
    };
    assert!(rig.llm.requests()[1].messages[2].content.contains("placeholder"));
    rig.engine.approve("kim", inbox_id, None).await.unwrap();
    assert_eq!(
        rig.slack.calls()[0].1["text"],
        format!("I will send the details.\n\n{MARKER}")
    );

    // Two replies with a placeholder end the try without a call.
    rig.llm
        .reply(&post("Call [REDACTED:PHONE]."), 10)
        .reply(&post("Call [REDACTED:PHONE] now."), 10);
    let outcome = rig.engine.ingest("kim", personal_event("e2")).await.unwrap();
    assert!(matches!(outcome, Outcome::Failed { .. }), "{outcome:?}");
    assert_eq!(rig.slack.calls().len(), 1);
    assert_eq!(rig.engine.snapshot("kim").await.unwrap().inbox.len(), 1);
}

#[tokio::test]
async fn replay_filters_the_event_too() {
    let rig = rig().await;
    rig.llm.reply(&ignore(), 10);
    let case = ReplayCase {
        event: personal_event("old"),
        actual_reply: "REAL".into(),
    };
    rig.engine.replay("kim", case).await.unwrap();
    let requests = all_requests(&rig);
    assert!(!requests.contains(EMAIL) && !requests.contains(PHONE));
}

#[tokio::test]
async fn a_correction_with_personal_data_is_filtered_in_later_prompts() {
    let rig = rig().await;
    rig.llm.reply(&post("Draft."), 10);
    let Outcome::PendingApproval { inbox_id } = rig.engine.ingest("kim", event("e1", "lee", "hi")).await.unwrap()
    else {
        panic!("expected a pending item")
    };
    // The owner types an address into the edit and the reason.
    let edit = bunsin_api::Edit {
        args: json!({"channel": "C1", "text": format!("Mail {EMAIL}.")})
            .as_object()
            .unwrap()
            .clone(),
        reason: format!("the contact is {PHONE}"),
    };
    rig.engine.approve("kim", inbox_id, Some(edit)).await.unwrap();
    // The owner's own text is sent as the owner wrote it.
    assert!(rig.slack.calls()[0].1["text"].as_str().unwrap().contains(EMAIL));

    rig.llm.reply(&ignore(), 10);
    rig.engine.ingest("kim", event("e2", "lee", "again")).await.unwrap();
    let later = serde_json::to_string(&rig.llm.requests()[1]).unwrap();
    assert!(
        later.contains("[REDACTED:EMAIL]") && later.contains("[REDACTED:PHONE]"),
        "{later}"
    );
    assert!(!later.contains(EMAIL) && !later.contains(PHONE));
}

#[tokio::test]
async fn personal_data_in_the_thread_field_is_filtered_too() {
    let rig = rig().await;
    rig.llm.reply(&ignore(), 10);
    let mut with_thread = event("e1", "lee", "see the thread");
    with_thread.thread = Some(format!("thread of {EMAIL}"));
    rig.engine.ingest("kim", with_thread).await.unwrap();
    assert!(!all_requests(&rig).contains(EMAIL));
    assert!(!rig.stores(EMAIL));
}
