//! Checks of the notes: they report stale, orphan and conflicting pages, and never change a page.

mod common;

use bunsin_api::{ApiError, EngineApi, ProfileConfig, WikiPageUpdate};
use bunsin_core::lint::FindingKind;
use bunsin_core::llm::LlmError;
use bunsin_core::wiki::{NotesConfig, WikiText};
use common::*;
use serde_json::json;

const DAY: i64 = 86_400;
const EMAIL: &str = "lee.park@corp.example.com";

fn notes(stale_days: Option<u32>, lint_every_hours: Option<u32>, lint_conflicts: bool) -> ProfileConfig {
    let mut config = config();
    config.notes = NotesConfig {
        stale_days,
        lint_every_hours,
        lint_conflicts,
    };
    config
}

async fn write(rig: &Rig, slug: &str, body: &str) {
    let update = WikiPageUpdate {
        text: WikiText {
            title: format!("Page {slug}"),
            summary: "one line".into(),
            body: body.into(),
        },
        expected_revision: None,
    };
    rig.engine.set_wiki_page("kim", slug, update).await.unwrap();
}

fn conflicts(list: &[(&str, &str, &str)]) -> String {
    let list: Vec<_> = list
        .iter()
        .map(|(a, b, detail)| json!({"a": a, "b": b, "detail": detail}))
        .collect();
    json!({ "conflicts": list }).to_string()
}

fn kinds(found: &[bunsin_core::lint::LintFinding]) -> Vec<(FindingKind, String)> {
    found.iter().map(|f| (f.kind, f.slugs.join("+"))).collect()
}

#[tokio::test]
async fn with_the_default_settings_nothing_is_checked_and_the_llm_is_not_called() {
    let rig = rig().await;
    write(&rig, "deploy", "Fridays.").await;
    rig.advance(400 * DAY);
    let refused = rig.engine.lint_notes("kim").await.unwrap_err();
    assert!(
        matches!(refused, ApiError::NotAllowed(ref text) if text.contains("off")),
        "{refused:?}"
    );
    assert!(rig.engine.background_tick().await.is_empty());
    assert!(rig.llm.requests().is_empty());
    assert!(rig.engine.lint_findings("kim").await.unwrap().is_empty());
}

#[tokio::test]
async fn a_page_is_stale_after_more_than_the_days_and_no_page_changes() {
    let rig = rig_with(notes(Some(10), None, false)).await;
    write(&rig, "deploy", "Fridays. See team.").await;
    write(&rig, "team", "Kim and Lee.").await;
    let before = rig.engine.wiki_index("kim").await.unwrap();

    rig.advance(10 * DAY);
    assert!(rig.engine.lint_notes("kim").await.unwrap().is_empty());
    rig.advance(1);
    let found = rig.engine.lint_notes("kim").await.unwrap();
    // `team` is named by `deploy`, so only `deploy` is an orphan too.
    assert_eq!(
        kinds(&found),
        [
            (FindingKind::Stale, "deploy".to_owned()),
            (FindingKind::Orphan, "deploy".to_owned()),
            (FindingKind::Stale, "team".to_owned()),
        ]
    );
    // A read by the agent keeps a page from being an orphan.
    rig.kim_store().mark_wiki_read("deploy", 1_000_000 + 10 * DAY).unwrap();
    let found = rig.engine.lint_notes("kim").await.unwrap();
    assert!(!kinds(&found).contains(&(FindingKind::Orphan, "deploy".to_owned())));
    // The findings of the last check replace the earlier ones.
    assert_eq!(rig.engine.lint_findings("kim").await.unwrap(), found);

    assert_eq!(rig.engine.wiki_index("kim").await.unwrap(), before);
    assert!(rig.llm.requests().is_empty());
}

#[tokio::test]
async fn only_the_pairs_that_were_sent_become_conflicts() {
    let rig = rig_with(notes(None, None, true)).await;
    for n in 0..5 {
        write(&rig, &format!("other{n}"), &format!("Unrelated topic{n}.")).await;
    }
    write(&rig, "deploy-a", &format!("Release window is friday. Ask {EMAIL}.")).await;
    write(&rig, "deploy-b", "Release window is monday.").await;
    rig.llm.reply(
        &conflicts(&[
            ("deploy-b", "deploy-a", &format!("Friday or Monday; ask {EMAIL}")),
            ("deploy-a", "other0", "made up"),
        ]),
        10,
    );
    let found = rig.engine.lint_notes("kim").await.unwrap();
    assert_eq!(kinds(&found), [(FindingKind::Conflict, "deploy-a+deploy-b".to_owned())]);
    assert_eq!(found[0].detail, "Friday or Monday; ask [REDACTED:EMAIL]");

    let requests = rig.llm.requests();
    assert_eq!(requests.len(), 1);
    let sent = &requests[0].messages[0].content;
    assert!(sent.contains("- deploy-a and deploy-b"), "{sent}");
    // Only the pages of the pairs go to the LLM, quoted and filtered.
    assert!(sent.contains("<data source=\"page\">"));
    assert!(!sent.contains("topic0") && !sent.contains(EMAIL), "{sent}");

    // A dismissed conflict is not sent again. With no other pair, the LLM is not called.
    rig.engine.dismiss_finding("kim", found[0].id).await.unwrap();
    assert!(rig.engine.lint_notes("kim").await.unwrap().is_empty());
    assert_eq!(rig.llm.requests().len(), 1);
}

#[tokio::test]
async fn a_failed_conflict_check_keeps_the_last_findings() {
    let rig = rig_with(notes(Some(1), None, true)).await;
    write(&rig, "deploy-a", "Release window friday.").await;
    write(&rig, "deploy-b", "Release window monday.").await;
    rig.advance(2 * DAY);
    rig.llm.reply(&conflicts(&[]), 10);
    let first = rig.engine.lint_notes("kim").await.unwrap();
    assert_eq!(first.len(), 4);
    rig.llm.push(Err(LlmError::Http {
        status: 500,
        body: format!("down, ask {EMAIL}"),
    }));
    let error = rig.engine.lint_notes("kim").await.unwrap_err().to_string();
    assert!(
        error.contains("conflict check failed") && !error.contains(EMAIL),
        "{error}"
    );
    assert_eq!(rig.engine.lint_findings("kim").await.unwrap(), first);
}

#[tokio::test]
async fn a_dismissed_finding_is_not_reported_again() {
    let rig = rig_with(notes(Some(1), None, false)).await;
    write(&rig, "deploy", "Fridays.").await;
    rig.advance(2 * DAY);
    let found = rig.engine.lint_notes("kim").await.unwrap();
    let stale = found.iter().find(|f| f.kind == FindingKind::Stale).unwrap().id;
    rig.engine.dismiss_finding("kim", stale).await.unwrap();
    assert!(matches!(
        rig.engine.dismiss_finding("kim", stale).await,
        Err(ApiError::NotAllowed(_))
    ));
    assert!(matches!(
        rig.engine.dismiss_finding("kim", 999).await,
        Err(ApiError::NotFound(_))
    ));
    let again = rig.engine.lint_notes("kim").await.unwrap();
    assert_eq!(kinds(&again), [(FindingKind::Orphan, "deploy".to_owned())]);
}

#[tokio::test]
async fn the_server_checks_the_notes_when_a_check_is_due() {
    let rig = rig_with(notes(Some(1), Some(2), false)).await;
    write(&rig, "deploy", "Fridays.").await;
    let checks = |rig: &Rig| {
        rig.kim_store()
            .list_audit(100)
            .unwrap()
            .iter()
            .filter(|entry| entry.kind == "notes_checked")
            .count()
    };
    assert!(rig.engine.background_tick().await.is_empty());
    assert_eq!(checks(&rig), 1);
    rig.advance(2 * 3_600 - 1);
    rig.engine.background_tick().await;
    assert_eq!(checks(&rig), 1);
    rig.advance(1);
    rig.engine.background_tick().await;
    assert_eq!(checks(&rig), 2);
    assert!(rig.engine.lint_findings("kim").await.unwrap().is_empty());
    // A day later the page is stale, and the next due check stores that.
    rig.advance(DAY);
    rig.engine.background_tick().await;
    assert_eq!(checks(&rig), 3);
    assert!(!rig.engine.lint_findings("kim").await.unwrap().is_empty());
}

#[tokio::test]
async fn settings_that_check_nothing_are_refused() {
    let rig = rig().await;
    for bad in [notes(Some(0), None, false), notes(None, Some(3), false)] {
        assert!(matches!(
            rig.engine.set_profile_config("kim", bad).await,
            Err(ApiError::Invalid(_))
        ));
    }
    rig.engine
        .set_profile_config("kim", notes(Some(30), Some(24), true))
        .await
        .unwrap();
}
