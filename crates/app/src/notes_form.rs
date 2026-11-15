//! The notes view of the main window: the list of pages and one page to read and edit.
//! The typed text lives in the input fields. Only Save sends it to the server.

use crate::bridge::{NoteResult, SaveError, UiCommand, UiState};
use crate::shell::{Shell, labelled};
use bunsin_api::WikiPageUpdate;
use bunsin_core::approval::ApprovalState;
use bunsin_core::records::InboxBody;
use bunsin_core::wiki::{WikiAuthor, WikiPage, WikiText};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px,
};

const PAGE_CONFLICT: &str = "The page changed on the server since this form loaded it. Nothing was saved.";
const UNSAVED: &str = "This page has changes that are not saved.";
const GONE: &str = "The server has no such page any more. Save anyway stores it again.";
const PENDING: &str = "A change by the agent for this page waits in the inbox. \
    If you save here, that change can no longer be approved.";
const UNCHANGED: &str = "Nothing changed, so nothing was saved.";
const AGENT_PAGE: &str = "If you save a change, the page counts as written by you. Its sources stay.";
const CONFIRM_DELETE: &str = "Delete this page? Its history stays on the server.";

/// What the fields hold.
enum Draft {
    /// A page of the server. `base` is the page as the form loaded it.
    Page {
        base: WikiPage,
        /// True if the server no longer has the page.
        gone: bool,
    },
    /// A page that the server does not have yet. Its name is in the name field.
    New,
}

/// Where the user wants to go.
#[derive(Clone)]
enum Nav {
    Open(String),
    New,
}

pub struct NotesForm {
    /// The profile whose notes the form shows.
    profile: Option<String>,
    /// None: no page is open.
    draft: Option<Draft>,
    /// The value of `UiState::note_commands` in the last state the form received.
    commands_seen: u64,
    slug: Entity<InputState>,
    title: Entity<InputState>,
    summary: Entity<InputState>,
    body: Entity<TextareaState>,
    /// The window's own message about the last action.
    message: Option<String>,
    /// True after the server refused a save because the page changed since the form loaded it.
    conflict: bool,
    /// True after the first click on Delete. The second click deletes.
    confirm_delete: bool,
    /// Where the user wanted to go while the fields held unsaved text.
    blocked: Option<Nav>,
}

impl NotesForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            profile: None,
            draft: None,
            commands_seen: 0,
            slug: cx.new(|cx| InputState::new(window, cx).placeholder("Page name: a-z, 0-9 and -")),
            title: cx.new(|cx| InputState::new(window, cx).placeholder("Title")),
            summary: cx.new(|cx| InputState::new(window, cx).placeholder("One line for the index")),
            body: cx.new(|cx| TextareaState::new(window, cx).placeholder("Text (Markdown)")),
            message: None,
            conflict: false,
            confirm_delete: false,
            blocked: None,
        }
    }

    /// The text in the fields.
    fn fields(&self, cx: &App) -> WikiText {
        WikiText {
            title: self.title.read(cx).value().to_string(),
            summary: self.summary.read(cx).value().to_string(),
            body: self.body.read(cx).value().to_string(),
        }
    }

    /// True if a save would change something.
    fn changed(&self, cx: &App) -> bool {
        let fields = self.fields(cx);
        match &self.draft {
            None => false,
            Some(Draft::Page { base, .. }) => fields != base.text,
            Some(Draft::New) => {
                !self.slug.read(cx).value().is_empty()
                    || !(fields.title.is_empty() && fields.summary.is_empty() && fields.body.is_empty())
            }
        }
    }

    fn set_fields(&mut self, slug: &str, text: &WikiText, window: &mut Window, cx: &mut App) {
        for (input, value) in [
            (&self.slug, slug),
            (&self.title, &text.title),
            (&self.summary, &text.summary),
        ] {
            input.update(cx, |input, cx| input.set_value(value.to_owned(), window, cx));
        }
        self.body
            .update(cx, |body, cx| body.set_value(text.body.clone(), window, cx));
        self.message = None;
        self.conflict = false;
        self.confirm_delete = false;
        self.blocked = None;
    }

    /// Shows `page` and drops what the fields held.
    fn load(&mut self, page: &WikiPage, window: &mut Window, cx: &mut App) {
        self.set_fields("", &page.text, window, cx);
        self.draft = Some(Draft::Page {
            base: page.clone(),
            gone: false,
        });
    }

    /// Shows no page.
    fn close(&mut self, window: &mut Window, cx: &mut App) {
        self.set_fields("", &WikiText::default(), window, cx);
        self.draft = None;
    }

    /// Takes a new state from the server. Typed text stays. The form loads the server's page
    /// if the user opened it, if a save just succeeded, or if the fields still equal the loaded
    /// page and the server has a newer one.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        if self.profile != state.selected {
            self.profile = state.selected.clone();
            self.close(window, cx);
        }
        // Without an answer from the server, the state says nothing about the pages.
        // The result of a command waits for the next state that has an answer.
        if !state.connected {
            return;
        }
        let new = state.note_commands != self.commands_seen;
        self.commands_seen = state.note_commands;
        let mut saved = false;
        match (&state.note_result, new) {
            (Some(NoteResult::Saved { slug, result }), true) if self.edits(slug, cx) => match result {
                Ok(()) => saved = true,
                Err(SaveError::Conflict) if matches!(self.draft, Some(Draft::New)) => {
                    // "Save anyway" would overwrite a page the user has not seen.
                    self.message = Some(format!(
                        "A page with the name `{slug}` exists already. Open it in the list, or use another name."
                    ));
                }
                Err(SaveError::Conflict) => self.conflict = true,
                // The view shows the server's text from the state.
                Err(SaveError::Failed(_)) => {}
            },
            (Some(NoteResult::Deleted { slug, result: Ok(()) }), true) if self.edits(slug, cx) => {
                self.close(window, cx);
            }
            _ => {}
        }
        let Some(open) = &state.open_note else {
            return;
        };
        let changed = self.changed(cx);
        match (&open.page, &mut self.draft) {
            (Some(page), Some(Draft::Page { base, gone })) if base.slug == page.slug => {
                *gone = false;
                if saved || (base.revision != page.revision && !changed) {
                    self.load(page, window, cx);
                }
            }
            // A new page stays until its save. A state from before "New page" can still name the page
            // that was open then.
            (Some(_), Some(Draft::New)) if !saved => {}
            (Some(page), _) => self.load(page, window, cx),
            (None, Some(Draft::Page { base, gone })) if base.slug == open.slug => *gone = true,
            // A new page waits for its first save.
            (None, Some(Draft::New)) => {}
            (None, _) => {
                self.close(window, cx);
                self.message = Some(format!("The server has no page `{}`.", open.slug));
            }
        }
    }

    /// True if the fields hold the page `slug`.
    fn edits(&self, slug: &str, cx: &App) -> bool {
        match &self.draft {
            None => false,
            Some(Draft::Page { base, .. }) => base.slug == slug,
            Some(Draft::New) => self.slug.read(cx).value().trim() == slug,
        }
    }
}

impl Shell {
    /// Goes to a page or to an empty new page. Unsaved text is kept unless `discard` is true.
    fn note_go(&mut self, nav: Nav, discard: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(profile) = self.notes.profile.clone() else {
            return;
        };
        if !discard && self.notes.changed(cx) {
            self.notes.blocked = Some(nav);
            return;
        }
        match nav {
            Nav::Open(slug) => {
                // The open page is on screen already. Show it as loaded.
                if let Some(Draft::Page { base, .. }) = &self.notes.draft
                    && base.slug == slug
                {
                    let base = base.clone();
                    self.notes.load(&base, window, cx);
                }
                // A new page is dropped here. The opened page then takes the empty fields.
                if matches!(self.notes.draft, Some(Draft::New)) {
                    self.notes.close(window, cx);
                }
                self.notes.blocked = None;
                self.send(UiCommand::OpenNote { profile, slug });
            }
            Nav::New => {
                self.notes.close(window, cx);
                self.notes.draft = Some(Draft::New);
                self.send(UiCommand::CloseNote);
            }
        }
    }

    /// Sends the fields. The server refuses them if the page is no longer the loaded one,
    /// unless `anyway` is true. A new page is never saved over an existing one.
    fn save_note(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.notes;
        let (Some(profile), Some(draft)) = (form.profile.clone(), &form.draft) else {
            return;
        };
        // A save without a change would still store a new revision in the owner's name.
        if let Draft::Page { gone: false, .. } = draft
            && !form.changed(cx)
        {
            form.message = Some(UNCHANGED.to_owned());
            return;
        }
        let (slug, expected_revision) = match draft {
            Draft::New => (form.slug.read(cx).value().trim().to_owned(), Some(0)),
            Draft::Page { base, .. } => (base.slug.clone(), (!anyway).then_some(base.revision)),
        };
        let update = WikiPageUpdate {
            text: form.fields(cx),
            expected_revision,
        };
        form.conflict = false;
        form.message = None;
        form.confirm_delete = false;
        self.send(UiCommand::SaveNote { profile, slug, update });
    }

    /// Drops the typed text and shows the server's page.
    fn load_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.state.open_note.as_ref().and_then(|open| open.page.clone()) {
            Some(page) => self.notes.load(&page, window, cx),
            None => self.notes.close(window, cx),
        }
    }

    /// The first click asks. The second click deletes the page.
    fn delete_note(&mut self) {
        let form = &mut self.notes;
        let (Some(profile), Some(Draft::Page { base, .. })) = (form.profile.clone(), &form.draft) else {
            return;
        };
        if !form.confirm_delete {
            form.confirm_delete = true;
            return;
        }
        let slug = base.slug.clone();
        form.confirm_delete = false;
        self.send(UiCommand::DeleteNote { profile, slug });
    }

    /// The list of pages. A click opens the page.
    fn notes_list(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let pages = self.state.notes.as_deref().unwrap_or_default();
        let open = match &self.notes.draft {
            Some(Draft::Page { base, .. }) => Some(base.slug.as_str()),
            _ => None,
        };
        let rows = pages.iter().map(|page| {
            let slug = page.slug.clone();
            let mark = if open == Some(slug.as_str()) { "● " } else { "" };
            let text = SharedString::from(format!("{mark}{} ({slug}) · {}", page.title, page.summary));
            div()
                .id(SharedString::from(format!("note-row-{slug}")))
                .test_support()
                .aria_label(text.clone())
                .cursor_pointer()
                .child(text)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.note_go(Nav::Open(slug.clone()), false, window, cx);
                    cx.notify();
                }))
        });
        div()
            .id("notes-list")
            .w(px(340.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .child(labelled("notes-count", format!("Pages: {}", pages.len())))
            .child(
                Button::new("note-new")
                    .label("New page")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.note_go(Nav::New, false, window, cx);
                        cx.notify();
                    })),
            )
            .children(rows)
    }

    /// The notes view: the list, and the open page if there is one.
    pub(crate) fn notes_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let form = &self.notes;
        let changed = form.changed(cx);
        let view = div()
            .id("notes")
            .flex_1()
            .min_w(px(0.))
            .flex()
            .gap_4()
            .child(self.notes_list(cx));
        let editor = div()
            .id("note-editor")
            .flex_1()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll();
        let message = labelled("note-message", form.message.clone().unwrap_or_default());
        let Some(draft) = &form.draft else {
            return view.child(
                editor
                    .child(labelled("note-heading", "Select a page, or make a new one."))
                    .child(message),
            );
        };

        let heading = match draft {
            Draft::New => "New page".to_owned(),
            Draft::Page { base, .. } => format!(
                "Page `{}` · revision {} · written by {}",
                base.slug,
                base.revision,
                match base.author {
                    WikiAuthor::Agent => "the agent, approved by you",
                    WikiAuthor::Owner => "you",
                }
            ),
        };
        let result = match &self.state.note_result {
            Some(NoteResult::Saved {
                result: Err(SaveError::Failed(error)),
                ..
            })
            | Some(NoteResult::Deleted { result: Err(error), .. }) => error.clone(),
            Some(NoteResult::Saved { result: Ok(()), .. }) if !changed => "Saved".to_owned(),
            _ => String::new(),
        };
        let page = match draft {
            Draft::Page { base, gone } => Some((base, *gone)),
            Draft::New => None,
        };
        // A pending change of the agent was written for the revision that a save here replaces.
        let pending = page.is_some_and(|(base, _)| {
            self.state.snapshot.as_ref().is_some_and(|snapshot| {
                snapshot.inbox.iter().any(|item| {
                    item.state == ApprovalState::Pending
                        && matches!(&item.body, InboxBody::WikiChange { slug, .. } if *slug == base.slug)
                })
            })
        });
        let sources = page.filter(|(base, _)| !base.sources.is_empty()).map(|(base, _)| {
            let lines: Vec<String> = base
                .sources
                .iter()
                .map(|source| format!("- {}: {}", source.origin, source.detail))
                .collect();
            div()
                .id("note-sources-box")
                .max_h(px(120.))
                .overflow_y_scroll()
                .child(labelled("note-sources", format!("Sources:\n{}", lines.join("\n"))))
        });

        let mut buttons = div()
            .flex()
            .gap_2()
            .child(
                Button::new("note-save")
                    .label("Save")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_note(false, cx);
                        cx.notify();
                    })),
            );
        if page.is_some() {
            buttons = buttons.child(
                Button::new("note-delete")
                    .label(if form.confirm_delete {
                        "Yes, delete the page"
                    } else {
                        "Delete"
                    })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.delete_note();
                        cx.notify();
                    })),
            );
        }

        let editor = editor
            .child(labelled("note-heading", heading))
            .children(
                page.is_none()
                    .then(|| Input::new(&form.slug).id("note-slug").aria_label("Page name")),
            )
            .child(Input::new(&form.title).id("note-title").aria_label("Title"))
            .child(Input::new(&form.summary).id("note-summary").aria_label("Summary"))
            // The text area has no ID of its own. Tests find it inside this element.
            .child(
                div()
                    .id("note-body")
                    .child(Textarea::new(&form.body).aria_label("Text").h(px(300.))),
            )
            .child(buttons)
            .child(labelled("note-changes", if changed { "Not saved" } else { "" }))
            .child(labelled(
                "note-confirm",
                if form.confirm_delete { CONFIRM_DELETE } else { "" },
            ))
            .child(message)
            .child(labelled("note-result", result))
            .children(page.is_some_and(|(_, gone)| gone).then(|| labelled("note-gone", GONE)))
            .children(pending.then(|| labelled("note-pending", PENDING)))
            .children(
                page.is_some_and(|(base, _)| base.author == WikiAuthor::Agent)
                    .then(|| labelled("note-agent-page", AGENT_PAGE)),
            )
            .children(form.conflict.then(|| {
                Self::conflict_warning(
                    "note",
                    PAGE_CONFLICT,
                    Self::load_note,
                    |this, cx| this.save_note(true, cx),
                    cx,
                )
            }))
            .children(form.blocked.clone().map(|nav| {
                div().flex().gap_2().child(labelled("note-blocked", UNSAVED)).child(
                    Button::new("note-discard")
                        .label("Discard the changes")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.note_go(nav.clone(), true, window, cx);
                            cx.notify();
                        })),
                )
            }))
            .children(sources);
        view.child(editor)
    }
}
