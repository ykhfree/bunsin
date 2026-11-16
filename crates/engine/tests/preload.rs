//! Pre-load of the notes: one read the owner names becomes page changes in the inbox.

mod common;

use bunsin_api::{ApiError, EngineApi, PreloadNotes};
use bunsin_core::approval::ApprovalState;
use bunsin_core::records::InboxBody;
use bunsin_engine::PRELOAD_SOURCE;
use common::*;
use serde_json::{Value, json};

const EMAIL: &str = "lee.park@corp.example.com";

fn request(tool: &str) -> PreloadNotes {
    PreloadNotes {
        tool: tool.into(),
        args: json!({"query": "in:#releases"}).as_object().unwrap().clone(),
    }
}

/// A reply of `kind` (as JSON) with one page change.
fn with_page(reply: &str, slug: &str, body: &str) -> String {
    let mut reply: Value = serde_json::from_str(reply).unwrap();
    reply["wiki_updates"] = json!([{"slug": slug, "title": "Releases", "summary": "When we release", "body": body}]);
    reply.to_string()
}

#[tokio::test]
async fn a_read_becomes_page_changes_with_their_source() {
    let rig = rig().await;
    rig.slack.set_result(
        "search",
        json!({"messages": [{"text": format!("We release on Thursdays. Ask {EMAIL}.")}]}),
    );
    rig.llm
        .reply(&with_page(&ignore(), "releases", "We release on Thursdays."), 10);
    let filed = rig.engine.preload_notes("kim", request("slack/search")).await.unwrap();
    assert_eq!(filed.len(), 1);

    // The tool ran once with the owner's arguments, and its result reached the LLM filtered.
    assert_eq!(rig.slack.calls(), [("search".to_owned(), request("slack/search").args)]);
    let sent = &rig.llm.requests()[0].messages[0].content;
    assert!(
        sent.contains("We release on Thursdays.") && !sent.contains(EMAIL),
        "{sent}"
    );

    let item = rig.kim_store().inbox_item(filed[0]).unwrap();
    assert_eq!(item.state, ApprovalState::Pending);
    let InboxBody::WikiChange { slug, sources, .. } = &item.body else {
        panic!("not a page change: {:?}", item.body);
    };
    assert_eq!(slug, "releases");
    assert!(
        sources[0]
            .detail
            .starts_with(&format!("{PRELOAD_SOURCE} message slack/search at "))
    );
    assert_eq!(sources[1].origin, "slack/search");
    assert_eq!(sources[1].detail, r#"{"query":"in:#releases"}"#);
    // Nothing is in the notes before the owner approves.
    assert!(rig.engine.wiki_index("kim").await.unwrap().is_empty());
    rig.engine.approve("kim", filed[0], None).await.unwrap();
    assert_eq!(rig.engine.wiki_index("kim").await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_tool_without_a_read_rule_is_refused_before_any_call() {
    let rig = rig().await;
    for tool in ["slack/post", "slack/unknown", "bunsin.wiki/read"] {
        let refused = rig.engine.preload_notes("kim", request(tool)).await.unwrap_err();
        assert!(
            matches!(refused, ApiError::NotAllowed(ref text) if text.contains("read")),
            "{refused:?}"
        );
    }
    assert!(rig.slack.calls().is_empty());
    assert!(rig.llm.requests().is_empty());
}

#[tokio::test]
async fn a_proposed_call_is_not_run_and_the_pages_still_go_to_the_inbox() {
    let rig = rig().await;
    rig.slack
        .set_result("search", json!({"messages": [{"text": "Releases on Thursdays."}]}));
    rig.llm.reply(&with_page(&post("Noted!"), "releases", "Thursdays."), 10);
    let filed = rig.engine.preload_notes("kim", request("slack/search")).await.unwrap();
    assert_eq!(filed.len(), 1);
    // Only the search ran. No post reached Slack and no call waits in the inbox.
    assert_eq!(rig.slack.calls().len(), 1);
    let inbox = rig.engine.snapshot("kim").await.unwrap().inbox;
    assert!(
        inbox
            .iter()
            .all(|item| !matches!(item.body, InboxBody::ToolCall { .. }))
    );
    let audit = rig.kim_store().list_audit(100).unwrap();
    let skipped = audit
        .iter()
        .find(|entry| entry.kind == "preload_proposal_skipped")
        .unwrap();
    assert_eq!(skipped.detail, json!({"kind": "tool_call"}));
}

#[tokio::test]
async fn a_failed_proposal_is_an_error_and_files_nothing() {
    let rig = rig().await;
    rig.slack.set_result("search", json!({"messages": []}));
    rig.llm.reply("not json", 10).reply("still not json", 10);
    let error = rig
        .engine
        .preload_notes("kim", request("slack/search"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("did not finish the pre-load"), "{error}");
    assert!(rig.engine.snapshot("kim").await.unwrap().inbox.is_empty());
}
