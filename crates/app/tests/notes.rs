//! Headless tests of the notes view: real view, real engine. The window has laptop size.

mod common;

use bunsin_api::{ApiError, EngineApi, WikiPageUpdate};
use bunsin_app::bridge::{NoteResult, OpenNote, SaveError, UiState};
use bunsin_app::shell::Shell;
use bunsin_core::wiki::{WikiAuthor, WikiText};
use common::Rig;
use common::window::{REFRESH, label, on_screen, open, runtime, value, wait_until};
use gpui_kit::test::TestWindowExt;
use gpui_kit::{AnyWindowHandle, App, AppContext, Entity, TestAppContext, Window};

const EMPTY: &str = "Select a page, or make a new one.";
const PAGE_CONFLICT: &str = "The page changed on the server since this form loaded it. Nothing was saved.";

fn text(title: &str, summary: &str, body: &str) -> WikiPageUpdate {
    WikiPageUpdate {
        text: WikiText {
            title: title.into(),
            summary: summary.into(),
            body: body.into(),
        },
        expected_revision: None,
    }
}

/// Gives `kim` the pages `deploy-schedule` and `team`, written by the owner.
async fn seed(rig: &Rig) {
    let deploy = text(
        "Deploy schedule",
        "When the team deploys",
        "The team deploys on Fridays.",
    );
    rig.engine
        .set_wiki_page("kim", "deploy-schedule", deploy)
        .await
        .unwrap();
    let team = text("Team", "Who does what", "Kim leads.");
    rig.engine.set_wiki_page("kim", "team", team).await.unwrap();
}

/// Opens the window on the notes view, with the two seeded pages in the list.
fn open_notes(cx: &mut TestAppContext, rt: &tokio::runtime::Runtime, rig: &Rig) -> (AnyWindowHandle, Entity<Shell>) {
    let (window, shell) = open(cx, rt, rig);
    on_screen(cx, window, |window, cx| window.click("tab-notes", cx));
    (window, shell)
}

/// Clicks a page in the list and waits until the server's page is on screen.
fn open_page(cx: &mut TestAppContext, window: AnyWindowHandle, shell: &Entity<Shell>, slug: &'static str) {
    on_screen(cx, window, |window, cx| window.click(format!("note-row-{slug}"), cx));
    wait_until(cx, shell, "the opened page", |s| {
        s.state
            .open_note
            .as_ref()
            .is_some_and(|open| open.slug == slug && open.page.is_some())
    });
}

/// The text in the text area.
fn body(window: &mut Window) -> String {
    let area = window.within("note-body").find("input-state");
    area.value().unwrap_or_default().to_owned()
}

/// Replaces the text of a single-line field.
fn retype(window: &mut Window, cx: &mut App, id: &'static str, text: &str) {
    window.click(id, cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

/// Replaces the text in the text area.
fn retype_body(window: &mut Window, cx: &mut App, text: &str) {
    window.within("note-body").click("input-state", cx);
    window.press("cmd-a", cx);
    window.input(text, cx);
}

/// Waits for the result of the next save or delete, counted from `before`.
fn wait_for_result(cx: &mut TestAppContext, shell: &Entity<Shell>, before: u64) -> NoteResult {
    wait_until(cx, shell, "the result of the notes command", |s| {
        s.state.note_commands > before
    });
    shell.read_with(cx, |s, _| s.state.note_result.clone().unwrap())
}

fn commands(cx: &mut TestAppContext, shell: &Entity<Shell>) -> u64 {
    shell.read_with(cx, |s, _| s.state.note_commands)
}

/// Lets several refreshes pass, for a check that nothing happened.
fn let_refreshes_pass(cx: &mut TestAppContext) {
    for _ in 0..4 {
        std::thread::sleep(REFRESH);
        cx.run_until_parked();
    }
}

async fn page_body(rig: &Rig, slug: &str) -> String {
    rig.engine.wiki_page("kim", slug).await.unwrap().text.body
}

#[gpui_kit::test]
fn the_list_shows_the_pages_and_a_click_opens_one(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "notes-count"), "Pages: 2");
        assert_eq!(
            label(window, "note-row-deploy-schedule"),
            "Deploy schedule (deploy-schedule) · When the team deploys"
        );
        assert_eq!(label(window, "note-row-team"), "Team (team) · Who does what");
        assert_eq!(label(window, "note-heading"), EMPTY);
        assert!(window.try_find("note-save").is_none());
    });
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 1 · written by you"
        );
        assert_eq!(value(window, "note-title"), "Team");
        assert_eq!(value(window, "note-summary"), "Who does what");
        assert_eq!(body(window), "Kim leads.");
        assert_eq!(label(window, "note-row-team"), "● Team (team) · Who does what");
        assert_eq!(label(window, "note-changes"), "");
        // The name of an existing page cannot be edited.
        assert!(window.try_find("note-slug").is_none());
    });
}

#[gpui_kit::test]
fn an_edit_is_stored_on_the_server_only_by_save(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "note-title", "The team");
        retype_body(window, cx, "Kim leads.\nLee reviews.");
    });
    let_refreshes_pass(cx);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-changes"), "Not saved");
        assert_eq!(body(window), "Kim leads.\nLee reviews.");
    });
    assert_eq!(rt.block_on(page_body(&rig, "team")), "Kim leads.");

    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(matches!(result, NoteResult::Saved { result: Ok(()), .. }), "{result:?}");
    let page = rt.block_on(rig.engine.wiki_page("kim", "team")).unwrap();
    assert_eq!(page.text.title, "The team");
    assert_eq!(page.text.body, "Kim leads.\nLee reviews.");
    assert_eq!((page.revision, page.author), (2, WikiAuthor::Owner));
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 2 · written by you"
        );
        assert_eq!(label(window, "note-result"), "Saved");
        assert_eq!(label(window, "note-changes"), "");
        assert_eq!(label(window, "note-row-team"), "● The team (team) · Who does what");
    });
}

#[gpui_kit::test]
fn save_is_in_reach_below_a_page_of_the_largest_size(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let long: String = (0..363).map(|i| format!("Line {i:03} of the page.\n")).collect();
    assert!((7_900..=8_000).contains(&long.chars().count()));
    rt.block_on(
        rig.engine
            .set_wiki_page("kim", "long", text("Long", "A long page", &long)),
    )
    .unwrap();
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "long");
    let before = commands(cx, &shell);
    // No scroll: the text scrolls inside its own area.
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "note-summary", "Still a long page");
        window.click("note-save", cx);
    });
    let result = wait_for_result(cx, &shell, before);
    assert!(matches!(result, NoteResult::Saved { result: Ok(()), .. }), "{result:?}");
    let page = rt.block_on(rig.engine.wiki_page("kim", "long")).unwrap();
    assert_eq!(page.text.summary, "Still a long page");
    assert_eq!(page.text.body, long);
}

#[gpui_kit::test]
fn typed_text_survives_a_newer_page_and_save_then_warns(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| retype_body(window, cx, "My edit."));
    // Another client changes the page.
    rt.block_on(
        rig.engine
            .set_wiki_page("kim", "team", text("Team", "Who does what", "Server edit.")),
    )
    .unwrap();
    wait_until(cx, &shell, "the newer page", |s| {
        s.state
            .open_note
            .as_ref()
            .and_then(|o| o.page.as_ref())
            .is_some_and(|p| p.revision == 2)
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(body(window), "My edit.");
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 1 · written by you"
        );
    });

    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(
        matches!(
            result,
            NoteResult::Saved {
                result: Err(SaveError::Conflict),
                ..
            }
        ),
        "{result:?}"
    );
    assert_eq!(rt.block_on(page_body(&rig, "team")), "Server edit.");
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-conflict"), PAGE_CONFLICT);
        assert_eq!(body(window), "My edit.");
    });

    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save-anyway", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(matches!(result, NoteResult::Saved { result: Ok(()), .. }), "{result:?}");
    let page = rt.block_on(rig.engine.wiki_page("kim", "team")).unwrap();
    assert_eq!((page.text.body.as_str(), page.revision), ("My edit.", 3));
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("note-conflict").is_none());
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 3 · written by you"
        );
    });
}

#[gpui_kit::test]
fn load_after_a_conflict_shows_the_page_of_the_server(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| retype_body(window, cx, "My edit."));
    rt.block_on(
        rig.engine
            .set_wiki_page("kim", "team", text("Team", "Who does what", "Server edit.")),
    )
    .unwrap();
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    wait_for_result(cx, &shell, before);
    on_screen(cx, window, |window, cx| window.click("note-load", cx));
    on_screen(cx, window, |window, _| {
        assert_eq!(body(window), "Server edit.");
        assert!(window.try_find("note-conflict").is_none());
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 2 · written by you"
        );
        assert_eq!(label(window, "note-changes"), "");
    });
    assert_eq!(rt.block_on(page_body(&rig, "team")), "Server edit.");
}

#[gpui_kit::test]
fn an_unchanged_form_follows_the_page_of_the_server(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    rt.block_on(
        rig.engine
            .set_wiki_page("kim", "team", text("Team", "Who does what", "Server edit.")),
    )
    .unwrap();
    wait_until(cx, &shell, "the newer page", |s| {
        s.state
            .open_note
            .as_ref()
            .and_then(|o| o.page.as_ref())
            .is_some_and(|p| p.revision == 2)
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(body(window), "Server edit.");
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 2 · written by you"
        );
    });
}

#[gpui_kit::test]
fn a_new_page_is_stored_and_never_over_an_existing_page(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| window.click("note-new", cx));
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "note-heading"), "New page");
        // A page that is not stored cannot be deleted.
        assert!(window.try_find("note-delete").is_none());
        retype(window, cx, "note-slug", "team");
        retype(window, cx, "note-title", "Other team");
        retype(window, cx, "note-summary", "Another team");
        retype_body(window, cx, "Park leads.");
    });
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(
        matches!(
            result,
            NoteResult::Saved {
                result: Err(SaveError::Conflict),
                ..
            }
        ),
        "{result:?}"
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-message"),
            "A page with the name `team` exists already. Open it in the list, or use another name."
        );
        // The window offers no way to overwrite the page that the user has not seen.
        assert!(window.try_find("note-save-anyway").is_none());
        assert_eq!(body(window), "Park leads.");
    });
    assert_eq!(rt.block_on(page_body(&rig, "team")), "Kim leads.");

    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "note-slug", "team-2");
        window.click("note-save", cx);
    });
    let result = wait_for_result(cx, &shell, before);
    assert!(matches!(result, NoteResult::Saved { result: Ok(()), .. }), "{result:?}");
    assert_eq!(rt.block_on(page_body(&rig, "team-2")), "Park leads.");
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "notes-count"), "Pages: 3");
        assert_eq!(
            label(window, "note-heading"),
            "Page `team-2` · revision 1 · written by you"
        );
        assert_eq!(label(window, "note-row-team-2"), "● Other team (team-2) · Another team");
        assert_eq!(label(window, "note-message"), "");
    });
}

#[gpui_kit::test]
fn a_refused_text_shows_the_reason_of_the_server_and_stays(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| window.click("note-new", cx));
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "note-slug", "Bad Name");
        retype(window, cx, "note-title", "Title");
        retype(window, cx, "note-summary", "Summary");
        retype_body(window, cx, "Text.");
        window.click("note-save", cx);
    });
    let result = wait_for_result(cx, &shell, before);
    assert!(
        matches!(
            result,
            NoteResult::Saved {
                result: Err(SaveError::Failed(_)),
                ..
            }
        ),
        "{result:?}"
    );
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-result"),
            "invalid request: the page name `Bad Name` is invalid: use a-z, 0-9 and `-`"
        );
        assert_eq!(value(window, "note-slug"), "Bad Name");
        assert_eq!(body(window), "Text.");
        assert_eq!(label(window, "notes-count"), "Pages: 2");
    });

    // An existing page: a text without a title is refused, and the page stays.
    open_page_after_discard(cx, window, &shell, "team");
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| {
        retype(window, cx, "note-title", " ");
        window.click("note-save", cx);
    });
    let result = wait_for_result(cx, &shell, before);
    assert!(
        matches!(
            result,
            NoteResult::Saved {
                result: Err(SaveError::Failed(_)),
                ..
            }
        ),
        "{result:?}"
    );
    on_screen(cx, window, |window, _| {
        assert!(label(window, "note-result").contains("title"))
    });
    assert_eq!(rt.block_on(rig.engine.wiki_page("kim", "team")).unwrap().revision, 1);
}

/// Clicks a page while the fields hold unsaved text, then discards that text.
fn open_page_after_discard(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    shell: &Entity<Shell>,
    slug: &'static str,
) {
    on_screen(cx, window, |window, cx| window.click(format!("note-row-{slug}"), cx));
    on_screen(cx, window, |window, cx| window.click("note-discard", cx));
    wait_until(cx, shell, "the opened page", |s| {
        s.state
            .open_note
            .as_ref()
            .is_some_and(|open| open.slug == slug && open.page.is_some())
    });
}

#[gpui_kit::test]
fn unsaved_text_is_not_dropped_without_a_question(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| {
        retype_body(window, cx, "Half typed.");
        window.click("note-row-deploy-schedule", cx);
    });
    let_refreshes_pass(cx);
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "note-blocked"),
            "This page has changes that are not saved."
        );
        assert_eq!(body(window), "Half typed.");
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 1 · written by you"
        );
        // "New page" is refused in the same way.
        window.click("note-new", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(body(window), "Half typed.");
        // The other views do not drop the text.
        window.click("tab-inbox", cx);
    });
    on_screen(cx, window, |window, cx| window.click("tab-settings", cx));
    on_screen(cx, window, |window, cx| window.click("tab-notes", cx));
    let_refreshes_pass(cx);
    on_screen(cx, window, |window, cx| {
        assert_eq!(body(window), "Half typed.");
        // The last refused target was "New page". Discard goes there.
        window.click("note-discard", cx);
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-heading"), "New page");
        assert_eq!(body(window), "");
        assert!(window.try_find("note-blocked").is_none());
    });
    assert_eq!(rt.block_on(page_body(&rig, "team")), "Kim leads.");
}

#[gpui_kit::test]
fn discard_opens_the_page_that_was_clicked(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| retype_body(window, cx, "Half typed."));
    open_page_after_discard(cx, window, &shell, "deploy-schedule");
    on_screen(cx, window, |window, _| {
        assert_eq!(body(window), "The team deploys on Fridays.");
        assert_eq!(label(window, "note-changes"), "");
    });
    // A click on the open page with unsaved text asks too, and Discard shows the stored text again.
    on_screen(cx, window, |window, cx| {
        retype_body(window, cx, "Half typed again.");
        window.click("note-row-deploy-schedule", cx);
    });
    on_screen(cx, window, |window, cx| window.click("note-discard", cx));
    on_screen(cx, window, |window, _| {
        assert_eq!(body(window), "The team deploys on Fridays.")
    });
}

#[gpui_kit::test]
fn a_page_is_deleted_only_by_two_clicks_on_the_same_page(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "deploy-schedule");
    on_screen(cx, window, |window, cx| window.click("note-delete", cx));
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-confirm"),
            "Delete this page? Its history stays on the server."
        );
        assert_eq!(label(window, "note-delete"), "Yes, delete the page");
    });
    // Another page opens. The question about the first page is gone.
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "note-confirm"), "");
        assert_eq!(label(window, "note-delete"), "Delete");
        window.click("note-delete", cx);
    });
    let_refreshes_pass(cx);
    assert_eq!(rt.block_on(rig.engine.wiki_index("kim")).unwrap().len(), 2);

    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-delete", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(
        matches!(result, NoteResult::Deleted { result: Ok(()), .. }),
        "{result:?}"
    );
    let gone = rt.block_on(rig.engine.wiki_page("kim", "team"));
    assert!(matches!(gone, Err(ApiError::NotFound(_))), "{gone:?}");
    rt.block_on(page_body(&rig, "deploy-schedule"));
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "notes-count"), "Pages: 1");
        assert_eq!(label(window, "note-heading"), EMPTY);
        assert!(window.try_find("note-row-team").is_none());
    });
    // The history keeps the text, as the question says.
    let history = rt.block_on(rig.engine.wiki_history("kim", "team")).unwrap();
    assert_eq!(history[0].as_ref().unwrap().text.body, "Kim leads.");
    assert!(history[1].is_none());
}

#[gpui_kit::test]
fn a_page_of_one_profile_is_not_shown_for_another(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| {
        retype_body(window, cx, "Half typed.");
        window.click("profile-lee", cx);
    });
    wait_until(cx, &shell, "the other profile", |s| {
        s.state.selected.as_deref() == Some("lee") && s.state.connected
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "notes-count"), "Pages: 0");
        assert_eq!(label(window, "note-heading"), EMPTY);
        assert!(window.try_find("note-row-team").is_none());
        window.click("note-new", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(value(window, "note-title"), "");
        assert_eq!(body(window), "");
        window.click("profile-kim", cx);
    });
    wait_until(cx, &shell, "the first profile", |s| {
        s.state.selected.as_deref() == Some("kim") && s.state.connected
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-heading"), EMPTY);
        assert_eq!(label(window, "notes-count"), "Pages: 2");
    });
    shell.read_with(cx, |s, _| assert_eq!(s.state.open_note, None));
}

#[gpui_kit::test]
fn a_page_that_was_deleted_elsewhere_can_be_stored_again(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| retype_body(window, cx, "My edit."));
    rt.block_on(rig.engine.delete_wiki_page("kim", "team")).unwrap();
    wait_until(cx, &shell, "the missing page", |s| {
        s.state.connected && s.state.open_note.as_ref().is_some_and(|o| o.page.is_none())
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-gone"),
            "The server has no such page any more. Save anyway stores it again."
        );
        assert_eq!(body(window), "My edit.");
    });
    // A missing page is not a lost connection.
    shell.read_with(cx, |s, _| assert_eq!(s.state.error, None));
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(
        matches!(
            result,
            NoteResult::Saved {
                result: Err(SaveError::Conflict),
                ..
            }
        ),
        "{result:?}"
    );
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save-anyway", cx));
    let result = wait_for_result(cx, &shell, before);
    assert!(matches!(result, NoteResult::Saved { result: Ok(()), .. }), "{result:?}");
    assert_eq!(rt.block_on(page_body(&rig, "team")), "My edit.");
    on_screen(cx, window, |window, _| assert!(window.try_find("note-gone").is_none()));
}

#[gpui_kit::test]
fn a_page_of_the_agent_shows_its_sources_and_a_waiting_change(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(async {
        common::ingest_note(&rig, "n1", false, "The team deploys on Fridays.").await;
        rig.engine.approve("kim", 2, None).await.unwrap();
    });
    let (window, shell) = open_notes(cx, &rt, &rig);
    open_page(cx, window, &shell, "deploy-schedule");
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-heading"),
            "Page `deploy-schedule` · revision 1 · written by the agent, approved by you"
        );
        let sources = label(window, "note-sources");
        assert!(sources.starts_with("Sources:\n- "), "{sources}");
        assert!(
            sources.contains(r#"- slack/search: {"query":"deploy date"}"#),
            "{sources}"
        );
        assert!(window.try_find("note-pending").is_none());
    });
    // The agent proposes a change of the open page.
    rt.block_on(common::ingest_note(&rig, "n2", true, "The team deploys on Thursdays."));
    wait_until(cx, &shell, "the waiting change", |s| {
        s.state.snapshot.as_ref().is_some_and(|snap| snap.inbox.len() == 5)
    });
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-pending"),
            "A change by the agent for this page waits in the inbox. \
             If you save here, that change can no longer be approved."
        );
        // The page itself did not change.
        assert_eq!(body(window), "The team deploys on Fridays.");
        assert_eq!(
            label(window, "note-agent-page"),
            "If you save a change, the page counts as written by you. Its sources stay."
        );
    });

    // Save without a change stores nothing: the page stays the agent's, with its sources.
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    let_refreshes_pass(cx);
    assert_eq!(commands(cx, &shell), before);
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-message"), "Nothing changed, so nothing was saved.");
    });
    let page = rt.block_on(rig.engine.wiki_page("kim", "deploy-schedule")).unwrap();
    assert_eq!(
        (page.revision, page.author, page.sources.len()),
        (1, WikiAuthor::Agent, 2)
    );

    // A saved change makes the page the owner's. The waiting change of the agent can then not be approved.
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| {
        retype_body(window, cx, "The team deploys on Mondays.");
        window.click("note-save", cx);
    });
    let result = wait_for_result(cx, &shell, before);
    assert!(matches!(result, NoteResult::Saved { result: Ok(()), .. }), "{result:?}");
    let page = rt.block_on(rig.engine.wiki_page("kim", "deploy-schedule")).unwrap();
    assert_eq!(
        (page.revision, page.author, page.sources.len()),
        (2, WikiAuthor::Owner, 2)
    );
    on_screen(cx, window, |window, _| {
        assert!(window.try_find("note-agent-page").is_none());
        // The window still shows where the facts of the page came from.
        assert!(label(window, "note-sources").contains("- slack/search:"));
    });
    let waiting = rt.block_on(rig.engine.snapshot("kim")).unwrap().inbox;
    let waiting = waiting
        .iter()
        .find(|i| i.state.as_str() == "pending" && matches!(i.body, bunsin_core::records::InboxBody::WikiChange { .. }))
        .unwrap()
        .id;
    let refused = rt.block_on(rig.engine.approve("kim", waiting, None));
    assert!(matches!(refused, Err(ApiError::NotAllowed(_))), "{refused:?}");
    assert_eq!(
        rt.block_on(page_body(&rig, "deploy-schedule")),
        "The team deploys on Mondays."
    );
}

#[gpui_kit::test]
fn a_late_state_does_not_replace_a_new_page(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let page = rt.block_on(rig.engine.wiki_page("kim", "team")).unwrap();
    // The test is the bridge: it sends the states, and it answers no command.
    let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    cx.update(gpui_kit::init);
    let (window, shell) = cx.update(|cx| {
        gpui_kit::open_window(Default::default(), cx, |window, cx| {
            cx.new(|cx| Shell::new(receiver, commands, window, cx))
        })
        .unwrap()
    });
    let with_team_open = UiState {
        selected: Some("kim".into()),
        notes: Some(vec![]),
        open_note: Some(OpenNote {
            profile: "kim".into(),
            slug: "team".into(),
            page: Some(page),
            history: None,
        }),
        connected: true,
        ..UiState::default()
    };
    states.send(with_team_open.clone()).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| {
        window.click("tab-notes", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(body(window), "Kim leads.");
        window.click("note-new", cx);
    });
    on_screen(cx, window, |window, cx| retype_body(window, cx, "A new text."));
    // A refresh that started before the click arrives now. It still names the page `team`.
    states.send(with_team_open).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-heading"), "New page");
        assert_eq!(body(window), "A new text.");
    });
    drop(shell);
}

#[gpui_kit::test]
fn a_save_is_not_forgotten_if_the_reload_after_it_fails(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let page = rt.block_on(rig.engine.wiki_page("kim", "team")).unwrap();
    // The test is the bridge: it sends the states, and it answers no command.
    let (commands, _received) = tokio::sync::mpsc::unbounded_channel();
    let (states, receiver) = tokio::sync::mpsc::unbounded_channel();
    cx.update(gpui_kit::init);
    let (window, _shell) = cx.update(|cx| {
        gpui_kit::open_window(Default::default(), cx, |window, cx| {
            cx.new(|cx| Shell::new(receiver, commands, window, cx))
        })
        .unwrap()
    });
    let open = |page| UiState {
        selected: Some("kim".into()),
        notes: Some(vec![]),
        open_note: Some(OpenNote {
            profile: "kim".into(),
            slug: "team".into(),
            page: Some(page),
            history: None,
        }),
        connected: true,
        ..UiState::default()
    };
    states.send(open(page.clone())).unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, cx| window.click("tab-notes", cx));
    on_screen(cx, window, |window, cx| {
        retype_body(window, cx, "My edit.");
        window.click("note-save", cx);
    });
    // The server stored the text, but the reload after the save got no answer.
    let saved = Some(NoteResult::Saved {
        slug: "team".into(),
        result: Ok(()),
    });
    let mut stored = page.clone();
    stored.text.body = "My edit.".into();
    stored.revision = 2;
    states
        .send(UiState {
            note_result: saved.clone(),
            note_commands: 1,
            connected: false,
            ..open(page)
        })
        .unwrap();
    cx.run_until_parked();
    // The next refresh has an answer. It still carries the result of the save.
    states
        .send(UiState {
            note_result: saved,
            note_commands: 1,
            ..open(stored)
        })
        .unwrap();
    cx.run_until_parked();
    on_screen(cx, window, |window, _| {
        assert_eq!(
            label(window, "note-heading"),
            "Page `team` · revision 2 · written by you"
        );
        assert_eq!(label(window, "note-changes"), "");
        assert_eq!(label(window, "note-result"), "Saved");
    });
}

fn history_len(shell: &Shell) -> Option<usize> {
    shell.state.open_note.as_ref()?.history.as_ref().map(Vec::len)
}

#[gpui_kit::test]
fn an_old_revision_is_read_and_saved_again_through_the_normal_save(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let later = text("Team", "Who does what", "Lee leads.");
    rt.block_on(rig.engine.set_wiki_page("kim", "team", later)).unwrap();
    // The history is below the text area. This window is tall enough to click it without a scroll.
    let (window, shell) = common::window::open_sized(cx, &rt, &rig, (1280., 1400.));
    on_screen(cx, window, |window, cx| window.click("tab-notes", cx));
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "note-history"), "History");
        assert!(window.try_find("note-rev-0").is_none());
        window.click("note-history", cx);
    });
    wait_until(cx, &shell, "the history", |s| history_len(s) == Some(2));
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "note-history"), "Hide the history");
        let first = label(window, "note-rev-0");
        assert!(first.starts_with("Revision 1 · by you · "), "{first}");
        assert!(label(window, "note-rev-1").starts_with("Revision 2 · "));
        // Nothing is chosen yet.
        assert!(window.try_find("note-rev-text").is_none());
        window.click("note-rev-0", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "note-rev-text"), "Team\nWho does what\n\nKim leads.");
        // Typed text is not replaced by an old revision.
        retype_body(window, cx, "Typed by me.");
        window.click("note-rev-load", cx);
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(
            label(window, "note-message"),
            "This page has changes that are not saved. Save or discard them first."
        );
        assert_eq!(body(window), "Typed by me.");
        retype_body(window, cx, "Lee leads.");
        window.click("note-rev-load", cx);
    });
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| {
        assert_eq!(body(window), "Kim leads.");
        assert_eq!(label(window, "note-changes"), "Not saved");
        window.click("note-save", cx);
    });
    assert_eq!(
        wait_for_result(cx, &shell, before),
        NoteResult::Saved {
            slug: "team".into(),
            result: Ok(()),
        }
    );
    let page = rt.block_on(rig.engine.wiki_page("kim", "team")).unwrap();
    assert_eq!((page.revision, page.text.body.as_str()), (3, "Kim leads."));
    // The history stays shown and gets the new revision.
    wait_until(cx, &shell, "the new revision in the history", |s| {
        history_len(s) == Some(3)
    });
    on_screen(cx, window, |window, cx| window.click("note-history", cx));
    wait_until(cx, &shell, "the hidden history", |s| history_len(s).is_none());
}

#[gpui_kit::test]
fn an_old_text_is_saved_with_the_revision_check(cx: &mut TestAppContext) {
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    rt.block_on(seed(&rig));
    let later = text("Team", "Who does what", "Lee leads.");
    rt.block_on(rig.engine.set_wiki_page("kim", "team", later)).unwrap();
    let (window, shell) = common::window::open_sized(cx, &rt, &rig, (1280., 1400.));
    on_screen(cx, window, |window, cx| window.click("tab-notes", cx));
    open_page(cx, window, &shell, "team");
    on_screen(cx, window, |window, cx| window.click("note-history", cx));
    wait_until(cx, &shell, "the history", |s| history_len(s) == Some(2));
    on_screen(cx, window, |window, cx| window.click("note-rev-0", cx));
    on_screen(cx, window, |window, cx| window.click("note-rev-load", cx));
    // Another client changes the page before the save.
    let other = text("Team", "Who does what", "Park leads.");
    rt.block_on(rig.engine.set_wiki_page("kim", "team", other)).unwrap();
    let before = commands(cx, &shell);
    on_screen(cx, window, |window, cx| window.click("note-save", cx));
    assert_eq!(
        wait_for_result(cx, &shell, before),
        NoteResult::Saved {
            slug: "team".into(),
            result: Err(SaveError::Conflict),
        }
    );
    assert_eq!(rt.block_on(page_body(&rig, "team")), "Park leads.");
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "note-conflict"), PAGE_CONFLICT);
        assert_eq!(body(window), "Kim leads.");
    });
}

#[gpui_kit::test]
fn the_owner_checks_the_notes_and_dismisses_a_finding(cx: &mut TestAppContext) {
    let rt = runtime();
    let now = 30 * 86_400;
    let rig = rt.block_on(common::rig_at(now));
    // Check with the default settings: the server says the checks are off. It is not a window error.
    let (window, shell) = open_notes(cx, &rt, &rig);
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "findings-count"), "Findings: 0");
        window.click("lint-notes", cx);
    });
    wait_until(cx, &shell, "the refused check", |s| s.state.lint_result.is_some());
    on_screen(cx, window, |window, _| {
        assert!(label(window, "lint-result").contains("checks of the notes are off"));
    });
    assert_eq!(shell.read_with(cx, |s, _| s.state.error.clone()), None);

    // A page that nobody changed for 29 days, with checks after 7 days.
    let mut config = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    config.notes.stale_days = Some(7);
    rt.block_on(rig.engine.set_profile_config("kim", config)).unwrap();
    let store = rig.engine.app_store().open_profile("kim").unwrap();
    let page = WikiText {
        title: "Deploy".into(),
        summary: "When".into(),
        body: "Fridays.".into(),
    };
    store
        .write_wiki_page("deploy", &page, &[], WikiAuthor::Owner, 0, None, 86_400)
        .unwrap();
    on_screen(cx, window, |window, cx| window.click("lint-notes", cx));
    wait_until(cx, &shell, "two findings", |s| {
        s.state.lint_findings.as_ref().is_some_and(|found| found.len() == 2)
    });
    let ids: Vec<i64> = shell.read_with(cx, |s, _| {
        s.state.lint_findings.as_ref().unwrap().iter().map(|f| f.id).collect()
    });
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "lint-result"), "2 findings.");
        assert_eq!(label(window, "findings-count"), "Findings: 2");
        assert_eq!(
            window.find(format!("finding-{}", ids[0])).label().unwrap_or_default(),
            "Old: deploy · nobody changed it for 29 day(s)"
        );
        assert_eq!(
            window.find(format!("finding-{}", ids[1])).label().unwrap_or_default(),
            "Not named: deploy · no other page names it, and the agent never read it"
        );
        // The findings are below the pre-load card. The column scrolls.
        window.scroll(
            "preload-tools",
            gpui_kit::ScrollDelta::Pixels(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(-600.))),
            cx,
        );
    });
    on_screen(cx, window, |window, cx| {
        window.click(format!("dismiss-finding-{}", ids[0]), cx);
    });
    wait_until(cx, &shell, "one finding", |s| {
        s.state.lint_findings.as_ref().is_some_and(|found| found.len() == 1)
    });
    // The check changed no page.
    assert_eq!(rt.block_on(rig.engine.wiki_page("kim", "deploy")).unwrap().revision, 1);
}

#[gpui_kit::test]
fn the_owner_preloads_the_notes_from_one_read(cx: &mut TestAppContext) {
    use bunsin_core::policy::ToolRule;
    use bunsin_testkit::tool;
    let rt = runtime();
    let rig = rt.block_on(common::rig());
    let tools = vec![tool("post", "Post a message"), tool("search", "Search messages")];
    rig.slack.set_tools(tools.clone());
    rig.slack.set_result(
        "search",
        serde_json::json!({"messages": [{"text": "We release on Thursdays."}]}),
    );
    let mut config = rt.block_on(rig.engine.profile_config("kim")).unwrap();
    config.approved_tools.insert("slack".into(), tools);
    config.policy.rules.insert("slack/search".into(), ToolRule::read());
    rt.block_on(rig.engine.set_profile_config("kim", config)).unwrap();
    let keep = serde_json::json!({"kind": "ignore", "rationale": "noted", "summary": null, "tool": null,
        "args_json": null, "wiki_updates": [{"slug": "releases", "title": "Releases", "summary": "When",
        "body": "Thursdays."}]});
    rig.llm.reply(&keep.to_string(), 10);

    let (window, shell) = open_notes(cx, &rt, &rig);
    // Arguments that are not a JSON object are not sent.
    on_screen(cx, window, |window, cx| {
        assert_eq!(label(window, "preload-tools"), "Tools that read: slack/search");
        window.click("preload-tool", cx);
        window.input("slack/search", cx);
        window.click("preload-args", cx);
        window.input("query=release", cx);
        window.click("preload-notes", cx);
    });
    on_screen(cx, window, |window, _| {
        assert!(label(window, "preload-result").starts_with("The arguments are not a JSON object"));
    });
    assert!(rig.slack.calls().is_empty());

    on_screen(cx, window, |window, cx| {
        retype(window, cx, "preload-args", "{\"query\": \"release\"}");
        window.click("preload-notes", cx);
    });
    wait_until(cx, &shell, "the pre-load", |s| s.state.preload_result.is_some());
    on_screen(cx, window, |window, _| {
        assert_eq!(label(window, "preload-result"), "1 page change waits in the inbox.");
    });
    assert_eq!(rig.slack.calls().len(), 1);
    let pending = rt.block_on(rig.engine.snapshot("kim")).unwrap().inbox;
    assert!(pending.iter().any(|item| matches!(
        &item.body,
        bunsin_core::records::InboxBody::WikiChange { slug, .. } if slug == "releases"
    ) && item.state == bunsin_core::approval::ApprovalState::Pending));
}
