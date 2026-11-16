//! Tests for the webhook checks and parsers. The bodies follow the Slack and Atlassian docs.
//! No test receives a real delivery.

use bunsin_connectors::webhook::{
    SLACK_MAX_SKEW_SECS, SlackDelivery, WebhookError, parse_jira, parse_slack, verify_atlassian, verify_slack,
};
use bunsin_core::event::InboundEvent;
use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";
const NOW: i64 = 1_700_000_000;

/// The hex HMAC-SHA256 of `parts`, computed here apart from the code under test.
fn hmac_hex(secret: &str, parts: &[&[u8]]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    for part in parts {
        mac.update(part);
    }
    mac.finalize().into_bytes().iter().map(|b| format!("{b:02x}")).collect()
}

fn slack_signature(secret: &str, timestamp: &str, body: &[u8]) -> String {
    format!(
        "v0={}",
        hmac_hex(secret, &[format!("v0:{timestamp}:").as_bytes(), body])
    )
}

#[test]
fn slack_accepts_a_correct_signature_within_the_window() {
    let body = br#"{"type":"event_callback"}"#;
    for sent_at in [NOW, NOW - SLACK_MAX_SKEW_SECS, NOW + SLACK_MAX_SKEW_SECS] {
        let timestamp = sent_at.to_string();
        let signature = slack_signature(SECRET, &timestamp, body);
        assert_eq!(
            verify_slack(SECRET, &timestamp, &signature, body, NOW),
            Ok(()),
            "{sent_at}"
        );
    }
}

#[test]
fn slack_rejects_wrong_secret_changed_body_and_old_or_future_timestamp() {
    let body = br#"{"type":"event_callback","event":{"text":"hi"}}"#;
    let timestamp = NOW.to_string();
    let signature = slack_signature(SECRET, &timestamp, body);

    let wrong_secret = slack_signature("another-secret", &timestamp, body);
    assert_eq!(
        verify_slack(SECRET, &timestamp, &wrong_secret, body, NOW),
        Err(WebhookError::BadSignature)
    );
    let changed = br#"{"type":"event_callback","event":{"text":"ho"}}"#;
    assert_eq!(
        verify_slack(SECRET, &timestamp, &signature, changed, NOW),
        Err(WebhookError::BadSignature)
    );
    // The timestamp is part of the signed text, so a replay with a new timestamp fails.
    let newer = (NOW + 1).to_string();
    assert_eq!(
        verify_slack(SECRET, &newer, &signature, body, NOW),
        Err(WebhookError::BadSignature)
    );
    // A correct signature over an old or a future timestamp is still refused.
    for sent_at in [NOW - SLACK_MAX_SKEW_SECS - 1, NOW + SLACK_MAX_SKEW_SECS + 1] {
        let timestamp = sent_at.to_string();
        let signature = slack_signature(SECRET, &timestamp, body);
        assert_eq!(
            verify_slack(SECRET, &timestamp, &signature, body, NOW),
            Err(WebhookError::Stale),
            "{sent_at}"
        );
    }
}

#[test]
fn slack_rejects_malformed_headers() {
    let body = b"{}";
    let timestamp = NOW.to_string();
    let hex = slack_signature(SECRET, &timestamp, body)[3..].to_owned();
    for (timestamp, signature) in [
        (timestamp.as_str(), hex.as_str()), // no `v0=`
        (timestamp.as_str(), "v1=00"),      // another version
        (timestamp.as_str(), "v0=zz"),      // not hex
        ("soon", "v0=00"),                  // not a number
        ("", "v0=00"),                      // empty
    ] {
        assert_eq!(
            verify_slack(SECRET, timestamp, signature, body, NOW),
            Err(WebhookError::BadHeader),
            "{timestamp} {signature}"
        );
    }
    // A shortened signature is a mismatch, not a match of the first bytes.
    let short = format!("v0={}", &hex[..32]);
    assert_eq!(
        verify_slack(SECRET, &timestamp, &short, body, NOW),
        Err(WebhookError::BadSignature)
    );
}

#[test]
fn atlassian_accepts_the_documented_test_vector() {
    // From the Jira Cloud webhooks page: secret, payload and expected signature.
    let signature = "sha256=a4771c39fbe90f317c7824e83ddef3caae9cb3d976c214ace1f2937e133263c9";
    assert_eq!(
        verify_atlassian("It's a Secret to Everybody", signature, b"Hello World!"),
        Ok(())
    );
}

#[test]
fn atlassian_rejects_wrong_secret_changed_body_and_other_methods() {
    let body = br#"{"webhookEvent":"comment_created"}"#;
    let signature = format!("sha256={}", hmac_hex(SECRET, &[body]));
    assert_eq!(verify_atlassian(SECRET, &signature, body), Ok(()));
    assert_eq!(
        verify_atlassian("another-secret", &signature, body),
        Err(WebhookError::BadSignature)
    );
    assert_eq!(
        verify_atlassian(SECRET, &signature, br#"{"webhookEvent":"comment_deleted"}"#),
        Err(WebhookError::BadSignature)
    );
    let hex = &signature["sha256=".len()..];
    for header in [hex.to_owned(), format!("sha1={hex}"), "sha256=not-hex".into()] {
        assert_eq!(
            verify_atlassian(SECRET, &header, body),
            Err(WebhookError::BadHeader),
            "{header}"
        );
    }
}

fn slack_body(event: Value) -> Vec<u8> {
    json!({
        "type": "event_callback",
        "team_id": "T1",
        "api_app_id": "A1",
        "event": event,
        "event_id": "Ev1",
        "event_time": NOW,
    })
    .to_string()
    .into_bytes()
}

fn slack_event(body: &[u8]) -> InboundEvent {
    match parse_slack("slack", body).unwrap() {
        SlackDelivery::Event(event) => event,
        other => panic!("expected an event, got {other:?}"),
    }
}

#[test]
fn slack_url_verification_returns_the_challenge() {
    let body = br#"{"token":"x","challenge":"3eZbrw1aBm2rZgRNFdxV","type":"url_verification"}"#;
    assert_eq!(
        parse_slack("slack", body),
        Ok(SlackDelivery::Challenge("3eZbrw1aBm2rZgRNFdxV".into()))
    );
    assert!(matches!(
        parse_slack("slack", br#"{"type":"url_verification"}"#),
        Err(WebhookError::BadBody(_))
    ));
}

#[test]
fn slack_message_becomes_an_event_with_channel_and_ts() {
    let top = slack_event(&slack_body(json!({
        "type": "message", "channel": "C1", "user": "U1", "text": "배포 확인 부탁", "ts": "1355517523.000005",
        "channel_type": "channel"
    })));
    assert_eq!(
        top,
        InboundEvent {
            source: "slack".into(),
            external_id: "C1:1355517523.000005".into(),
            thread: Some("C1:1355517523.000005".into()),
            author: "U1".into(),
            text: "배포 확인 부탁".into(),
        }
    );

    let reply = slack_event(&slack_body(json!({
        "type": "message", "channel": "C1", "user": "U2", "text": "done", "ts": "1355517600.000001",
        "thread_ts": "1355517523.000005"
    })));
    assert_eq!(reply.external_id, "C1:1355517600.000001");
    // The thread points at the parent, so a reply goes to the same thread.
    assert_eq!(reply.thread.as_deref(), Some("C1:1355517523.000005"));
}

#[test]
fn slack_message_and_app_mention_of_one_message_share_the_dedupe_key() {
    let fields = json!({"channel": "C1", "user": "U1", "text": "<@UBOT> help", "ts": "1.000001"});
    let mut message = fields.clone();
    message["type"] = json!("message");
    let mut mention = fields;
    mention["type"] = json!("app_mention");
    assert_eq!(
        slack_event(&slack_body(message)).dedupe_key(),
        slack_event(&slack_body(mention)).dedupe_key()
    );
}

#[test]
fn slack_ignores_changes_notices_and_other_event_types() {
    let ignored = [
        json!({"type": "message", "subtype": "message_changed", "channel": "C1", "ts": "1.1",
               "message": {"user": "U1", "text": "edited", "ts": "1.0"}}),
        json!({"type": "message", "subtype": "message_deleted", "channel": "C1", "ts": "1.1", "deleted_ts": "1.0"}),
        json!({"type": "message", "subtype": "channel_join", "channel": "C1", "user": "U1",
               "text": "<@U1> has joined the channel", "ts": "1.2"}),
        json!({"type": "reaction_added", "user": "U1", "reaction": "eyes", "item": {"channel": "C1", "ts": "1.0"}}),
        json!({"type": "message", "channel": "C1", "user": "U1", "text": "", "ts": "1.3"}),
    ];
    for event in ignored {
        assert_eq!(
            parse_slack("slack", &slack_body(event.clone())),
            Ok(SlackDelivery::Ignored),
            "{event}"
        );
    }
    assert_eq!(
        parse_slack("slack", br#"{"type":"app_rate_limited"}"#),
        Ok(SlackDelivery::Ignored)
    );
}

#[test]
fn slack_bot_message_keeps_the_bot_as_author_and_bad_bodies_fail() {
    // Skipping bots is the engine's job, so the parser keeps the message.
    let bot = slack_event(&slack_body(json!({
        "type": "message", "subtype": "bot_message", "channel": "C1", "bot_id": "B1", "text": "build ok", "ts": "2.0"
    })));
    assert_eq!(bot.author, "B1");

    assert!(matches!(
        parse_slack("slack", b"not json"),
        Err(WebhookError::BadBody(_))
    ));
    assert!(matches!(
        parse_slack("slack", br#"{"type":"event_callback"}"#),
        Err(WebhookError::BadBody(_))
    ));
}

fn comment(id: &str, updated: &str, body: Value) -> Value {
    json!({
        "id": id,
        "author": {"accountId": "acc-author", "displayName": "Kim"},
        "updateAuthor": {"accountId": "acc-editor", "displayName": "Lee"},
        "body": body,
        "created": "2026-11-17T09:00:00.000+0900",
        "updated": updated,
    })
}

fn jira_body(event: &str, extra: Value) -> Vec<u8> {
    let mut body = json!({
        "timestamp": 1606480436302_i64,
        "webhookEvent": event,
        "user": {"accountId": "acc-user", "displayName": "Park"},
        "issue": {"id": "99291", "key": "PROJ-7", "fields": {"summary": "Deploy fails", "description": null}},
    });
    for (key, value) in extra.as_object().unwrap() {
        body[key] = value.clone();
    }
    body.to_string().into_bytes()
}

#[test]
fn jira_comment_created_becomes_an_event_on_the_issue_thread() {
    let created = comment("10001", "2026-11-17T09:00:00.000+0900", json!("Please check *staging*"));
    let event = parse_jira("jira", &jira_body("comment_created", json!({"comment": created})))
        .unwrap()
        .unwrap();
    assert_eq!(
        event,
        InboundEvent {
            source: "jira".into(),
            external_id: "comment:10001:2026-11-17T09:00:00.000+0900".into(),
            thread: Some("PROJ-7".into()),
            author: "acc-editor".into(),
            text: "Please check *staging*".into(),
        }
    );
}

#[test]
fn jira_comment_as_issue_update_dedupes_and_an_edit_is_new() {
    let first = comment("10001", "2026-11-17T09:00:00.000+0900", json!("hi"));
    let created = parse_jira("jira", &jira_body("comment_created", json!({"comment": first.clone()})))
        .unwrap()
        .unwrap();
    let as_update = parse_jira(
        "jira",
        &jira_body(
            "jira:issue_updated",
            json!({"comment": first, "issue_event_type_name": "issue_commented"}),
        ),
    )
    .unwrap()
    .unwrap();
    assert_eq!(created.dedupe_key(), as_update.dedupe_key());

    let edited = comment("10001", "2026-11-17T09:05:00.000+0900", json!("hi, edited"));
    let edit = parse_jira("jira", &jira_body("comment_updated", json!({"comment": edited})))
        .unwrap()
        .unwrap();
    assert_ne!(created.dedupe_key(), edit.dedupe_key());
}

#[test]
fn jira_adf_comment_body_becomes_plain_text() {
    let adf = json!({"type": "doc", "version": 1, "content": [
        {"type": "paragraph", "content": [{"type": "text", "text": "first"}]},
        {"type": "paragraph", "content": [{"type": "text", "text": "second"}]}
    ]});
    let event = parse_jira(
        "jira",
        &jira_body("comment_created", json!({"comment": comment("5", "t", adf)})),
    )
    .unwrap()
    .unwrap();
    assert_eq!(event.text, "first\nsecond");
}

#[test]
fn jira_issue_event_uses_summary_description_and_the_acting_user() {
    let mut body: Value = serde_json::from_slice(&jira_body("jira:issue_created", json!({}))).unwrap();
    body["issue"]["fields"]["description"] = json!("Steps:\n1. deploy");
    let event = parse_jira("jira", body.to_string().as_bytes()).unwrap().unwrap();
    assert_eq!(event.external_id, "issue:99291:1606480436302");
    assert_eq!(event.author, "acc-user");
    assert_eq!(event.thread.as_deref(), Some("PROJ-7"));
    assert_eq!(event.text, "Deploy fails\n\nSteps:\n1. deploy");
}

#[test]
fn jira_other_events_are_none_and_broken_bodies_fail() {
    assert_eq!(
        parse_jira("jira", &jira_body("jira:issue_deleted", json!({}))),
        Ok(None)
    );
    assert_eq!(parse_jira("jira", &jira_body("worklog_created", json!({}))), Ok(None));
    // A comment event without its comment is broken, not ignorable.
    assert!(matches!(
        parse_jira("jira", &jira_body("comment_created", json!({}))),
        Err(WebhookError::BadBody(_))
    ));
    let mut no_key: Value = serde_json::from_slice(&jira_body("jira:issue_created", json!({}))).unwrap();
    no_key["issue"].as_object_mut().unwrap().remove("key");
    assert!(matches!(
        parse_jira("jira", no_key.to_string().as_bytes()),
        Err(WebhookError::BadBody(_))
    ));
    assert!(matches!(parse_jira("jira", b"{"), Err(WebhookError::BadBody(_))));
}
