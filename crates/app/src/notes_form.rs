//! The notes view of the main window: the list of pages and one page to read and edit.
//! The typed text lives in the input fields. Only Save sends it to the server.

use crate::bridge::{NoteResult, SaveError, UiCommand, UiState};
use crate::shell::{Shell, labelled, labelled_as};
use crate::theme::{card, code_block, field, icon, notice, palette, quiet, utc_minute};
use bunsin_api::{PreloadNotes, WikiPageUpdate};
use bunsin_core::approval::ApprovalState;
use bunsin_core::lint::{FindingKind, LintFinding};
use bunsin_core::policy::ToolClass;
use bunsin_core::records::InboxBody;
use bunsin_core::wiki::{WikiAuthor, WikiPage, WikiText};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::*, px,
};

const PAGE_CONFLICT: &str = "The page changed on the server since this form loaded it. Nothing was saved.";
const UNSAVED: &str = "This page has changes that are not saved.";
const GONE: &str = "The server has no such page any more. Save anyway stores it again.";
const PENDING: &str = "A change by the agent for this page waits in the inbox. \
    If you save here, that change can no longer be approved.";
const UNCHANGED: &str = "Nothing changed, so nothing was saved.";
const AGENT_PAGE: &str = "If you save a change, the page counts as written by you. Its sources stay.";
const EMPTY: &str = "Select a page, or make a new one.";
const CONFIRM_DELETE: &str = "Delete this page? Its history stays on the server.";
const OLD_LOADED: &str = "The old text is in the fields now. Save stores it as a new revision.";
const OLD_BLOCKED: &str = "This page has changes that are not saved. Save or discard them first.";
const PRELOAD_HELP: &str = "Bunsin calls one tool that reads, with the arguments you give. The agent proposes \
    pages from the result. They wait in the inbox like other page changes; nothing else is run.";
const CHECKS_HELP: &str = "A check reports old pages, pages that nothing names, and, if the settings turn it on, \
    pages that may say different things. It changes no page. The settings `notes` say what to check.";

/// A finding as one line: what, which pages, and why.
pub fn finding_text(finding: &LintFinding) -> String {
    let what = match finding.kind {
        FindingKind::Stale => "Old",
        FindingKind::Orphan => "Not named",
        FindingKind::Conflict => "May conflict",
    };
    format!("{what}: {} · {}", finding.slugs.join(" and "), finding.detail)
}

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
    /// The position in the history of the revision that the owner reads, if any.
    revision_shown: Option<usize>,
    /// The tool ID of a pre-load.
    preload_tool: Entity<InputState>,
    /// The arguments of a pre-load, as a JSON object.
    preload_args: Entity<InputState>,
    /// Why the window did not send a pre-load.
    preload_message: Option<String>,
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
            revision_shown: None,
            preload_tool: cx.new(|cx| InputState::new(window, cx).placeholder("connector/tool")),
            preload_args: cx.new(|cx| InputState::new(window, cx).placeholder("{\"query\": \"...\"}")),
            preload_message: None,
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
        self.revision_shown = None;
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

    /// Shows or hides the revisions of the open page.
    fn toggle_history(&mut self) {
        let Some(open) = &self.state.open_note else {
            return;
        };
        let command = UiCommand::NoteHistory {
            profile: open.profile.clone(),
            show: open.history.is_none(),
        };
        self.notes.revision_shown = None;
        self.send(command);
    }

    /// Puts the text of an old revision into the fields. The base stays the loaded page,
    /// so Save checks the revision as for any edit. Typed text is not replaced.
    fn load_revision(&mut self, text: WikiText, window: &mut Window, cx: &mut Context<Self>) {
        let form = &mut self.notes;
        if form.changed(cx) {
            form.message = Some(OLD_BLOCKED.to_owned());
            return;
        }
        for (input, value) in [(&form.title, &text.title), (&form.summary, &text.summary)] {
            input.update(cx, |input, cx| input.set_value(value.clone(), window, cx));
        }
        form.body.update(cx, |body, cx| body.set_value(text.body, window, cx));
        form.message = Some(OLD_LOADED.to_owned());
    }

    /// The revisions of the open page, newest first, and the text of the one the owner chose.
    /// Nothing here can be edited.
    fn note_history(&self, cx: &Context<Self>) -> Option<impl IntoElement> {
        let history = self.state.open_note.as_ref()?.history.as_ref()?;
        let shown = self.notes.revision_shown.filter(|at| *at < history.len());
        let theme = cx.theme();
        let (selected_bg, radius) = (theme.muted, theme.radius);
        let rows = history.iter().enumerate().rev().map(|(at, revision)| {
            let text = match revision {
                Some(page) => {
                    let author = match page.author {
                        WikiAuthor::Agent => "the agent",
                        WikiAuthor::Owner => "you",
                    };
                    format!(
                        "Revision {} · by {author} · {} UTC",
                        page.revision,
                        utc_minute(page.updated_at)
                    )
                }
                None => "Deleted".to_owned(),
            };
            let chosen = shown == Some(at);
            div()
                .id(SharedString::from(format!("note-rev-{at}")))
                .test_support()
                .aria_label(text.clone())
                .px_2()
                .py_1()
                .rounded(radius)
                .text_sm()
                .cursor_pointer()
                .when(chosen, |row| row.bg(selected_bg))
                .when(!chosen, |row| row.hover(|row| row.bg(selected_bg)))
                .child(SharedString::from(text))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.notes.revision_shown = Some(at);
                    cx.notify();
                }))
        });
        let old = shown.and_then(|at| history[at].clone()).map(|page| {
            let label = format!("{}\n{}\n\n{}", page.text.title, page.text.summary, page.text.body);
            let text = page.text.clone();
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(
                    div()
                        .id("note-rev-box")
                        .max_h(px(220.))
                        .overflow_y_scroll()
                        .child(labelled_as(
                            "note-rev-text",
                            label,
                            code_block(cx)
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child(page.text.title))
                                .child(quiet(page.text.summary, cx))
                                .child(div().child(page.text.body)),
                        )),
                )
                .child(
                    Button::new("note-rev-load")
                        .small()
                        .outline()
                        .self_start()
                        .label("Put this text into the fields")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.load_revision(text.clone(), window, cx);
                            cx.notify();
                        })),
                )
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .child(quiet("Revisions, newest first. Choose one to read it.", cx))
                .child(
                    div()
                        .id("note-revisions")
                        .max_h(px(160.))
                        .overflow_y_scroll()
                        .flex()
                        .flex_col()
                        .children(rows),
                )
                .children(old),
        )
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
    fn notes_list(&self, cx: &Context<Self>) -> impl IntoElement {
        let pages = self.state.notes.as_deref().unwrap_or_default();
        let open = match &self.notes.draft {
            Some(Draft::Page { base, .. }) => Some(base.slug.as_str()),
            _ => None,
        };
        let theme = cx.theme();
        let (selected_bg, radius, mono) = (theme.muted, theme.radius, theme.mono_font_family.clone());
        let rows = pages.iter().map(|page| {
            let slug = page.slug.clone();
            let selected = open == Some(slug.as_str());
            let mark = if selected { "● " } else { "" };
            let label = format!("{mark}{} ({slug}) · {}", page.title, page.summary);
            div()
                .id(SharedString::from(format!("note-row-{slug}")))
                .test_support()
                .aria_label(label)
                .flex()
                .flex_col()
                .gap_0p5()
                .px_3()
                .py_2()
                .rounded(radius)
                .cursor_pointer()
                .when(selected, |row| row.bg(selected_bg))
                .when(!selected, |row| row.hover(|row| row.bg(selected_bg)))
                .child(
                    div()
                        .flex()
                        .justify_between()
                        .gap_2()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(SharedString::from(page.title.clone())),
                        )
                        .child(quiet(slug.clone(), cx).flex_none().font_family(mono.clone()).text_xs()),
                )
                .child(quiet(page.summary.clone(), cx))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.note_go(Nav::Open(slug.clone()), false, window, cx);
                    cx.notify();
                }))
        });
        div()
            .w(px(300.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(labelled_as(
                        "notes-count",
                        format!("Pages: {}", pages.len()),
                        quiet(
                            match pages.len() {
                                0 => "No page yet".to_owned(),
                                1 => "1 page".to_owned(),
                                count => format!("{count} pages"),
                            },
                            cx,
                        ),
                    ))
                    .child(
                        Button::new("note-new")
                            .small()
                            .icon(icon("plus"))
                            .label("New page")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.note_go(Nav::New, false, window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("notes-list")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .overflow_y_scroll()
                    .children(rows),
            )
    }

    /// The notes view: the list, and the open page if there is one.
    pub(crate) fn notes_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let form = &self.notes;
        let changed = form.changed(cx);
        let cx = &*cx;
        let colors = palette(cx);
        let view = div()
            .id("notes")
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .gap_5()
            .child(self.notes_list(cx));
        let editor = card(cx).id("note-editor").flex_1().min_w(px(0.)).overflow_y_scroll();
        // The window's own message. It is not drawn while there is none.
        let message = div()
            .text_sm()
            .text_color(cx.theme().muted_foreground)
            .child(labelled("note-message", form.message.clone().unwrap_or_default()));
        let Some(draft) = &form.draft else {
            return view
                .child(
                    editor
                        .child(labelled_as("note-heading", EMPTY, quiet(EMPTY, cx)))
                        .child(message),
                )
                .child(self.notes_side(cx));
        };

        let page = match draft {
            Draft::Page { base, gone } => Some((base, *gone)),
            Draft::New => None,
        };
        let (heading, name, about) = match page {
            None => ("New page".to_owned(), "New page".to_owned(), String::new()),
            Some((base, _)) => {
                let author = match base.author {
                    WikiAuthor::Agent => "the agent, approved by you",
                    WikiAuthor::Owner => "you",
                };
                (
                    format!(
                        "Page `{}` · revision {} · written by {author}",
                        base.slug, base.revision
                    ),
                    base.slug.clone(),
                    format!("revision {}, written by {author}", base.revision),
                )
            }
        };
        let result = match &self.state.note_result {
            Some(NoteResult::Saved {
                result: Err(SaveError::Failed(error)),
                ..
            })
            | Some(NoteResult::Deleted { result: Err(error), .. }) => Err(error.clone()),
            Some(NoteResult::Saved { result: Ok(()), .. }) if !changed => Ok("Saved"),
            _ => Ok(""),
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
                .map(|source| format!("{}: {}", source.origin, source.detail))
                .collect();
            let label = format!("Sources:\n- {}", lines.join("\n- "));
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(quiet("Where the agent read this", cx))
                .child(
                    div()
                        .id("note-sources-box")
                        .max_h(px(96.))
                        .overflow_y_scroll()
                        .child(labelled_as(
                            "note-sources",
                            label,
                            code_block(cx)
                                .flex()
                                .flex_col()
                                .gap_1()
                                .children(lines.into_iter().map(|line| div().child(SharedString::from(line)))),
                        )),
                )
        });

        let mut buttons =
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    Button::new("note-save")
                        .primary()
                        .label("Save")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.save_note(false, cx);
                            cx.notify();
                        })),
                );
        if page.is_some() {
            let shown = self.state.open_note.as_ref().is_some_and(|open| open.history.is_some());
            buttons = buttons.child(
                Button::new("note-history")
                    .ghost()
                    .label(if shown { "Hide the history" } else { "History" })
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.toggle_history();
                        cx.notify();
                    })),
            );
            let delete = Button::new("note-delete")
                .label(if form.confirm_delete {
                    "Yes, delete the page"
                } else {
                    "Delete"
                })
                .on_click(cx.listener(|this, _, _, cx| {
                    this.delete_note();
                    cx.notify();
                }));
            buttons = buttons.child(if form.confirm_delete {
                delete.danger()
            } else {
                delete.ghost()
            });
        }
        let confirm = if form.confirm_delete { CONFIRM_DELETE } else { "" };
        let unsaved = if changed { "Not saved" } else { "" };
        let (result_text, result_color) = match &result {
            Ok(text) => ((*text).to_owned(), colors.done),
            Err(error) => (error.clone(), colors.stopped),
        };
        let buttons = buttons
            .child(labelled_as(
                "note-changes",
                unsaved,
                div().text_sm().text_color(colors.waiting).child(unsaved),
            ))
            .child(labelled_as(
                "note-result",
                result_text.clone(),
                div()
                    .text_sm()
                    .text_color(result_color)
                    .child(SharedString::from(result_text)),
            ))
            .child(labelled_as(
                "note-confirm",
                confirm,
                div().text_sm().text_color(colors.stopped).child(confirm),
            ));

        // What the owner must know before typing or saving. It is above the fields, so it is never below the fold.
        let notices = div()
            .flex()
            .flex_col()
            .gap_2()
            .children(form.blocked.clone().map(|nav| {
                notice(colors.waiting, cx)
                    .child(div().flex_1().min_w(px(0.)).child(labelled("note-blocked", UNSAVED)))
                    .child(
                        Button::new("note-discard")
                            .small()
                            .label("Discard the changes")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.note_go(nav.clone(), true, window, cx);
                                cx.notify();
                            })),
                    )
            }))
            .children(form.conflict.then(|| {
                notice(colors.stopped, cx).child(div().flex_1().min_w(px(0.)).child(Self::conflict_warning(
                    "note",
                    PAGE_CONFLICT,
                    Self::load_note,
                    |this, cx| this.save_note(true, cx),
                    cx,
                )))
            }))
            .children(page.is_some_and(|(_, gone)| gone).then(|| {
                notice(colors.stopped, cx).child(div().flex_1().min_w(px(0.)).child(labelled("note-gone", GONE)))
            }))
            .children(pending.then(|| {
                notice(colors.waiting, cx).child(div().flex_1().min_w(px(0.)).child(labelled("note-pending", PENDING)))
            }))
            .children(
                page.is_some_and(|(base, _)| base.author == WikiAuthor::Agent)
                    .then(|| labelled_as("note-agent-page", AGENT_PAGE, quiet(AGENT_PAGE, cx))),
            );

        let editor = editor
            .child(labelled_as(
                "note-heading",
                heading,
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_x_3()
                    .child(
                        div()
                            .text_lg()
                            .font_weight(FontWeight::SEMIBOLD)
                            .when(page.is_some(), |name| {
                                name.font_family(cx.theme().mono_font_family.clone())
                            })
                            .child(SharedString::from(name)),
                    )
                    .child(quiet(about, cx)),
            ))
            .child(notices)
            .children(page.is_none().then(|| {
                field(
                    "Name, in a-z, 0-9 and -",
                    Input::new(&form.slug).id("note-slug").aria_label("Page name"),
                    cx,
                )
            }))
            .child(field(
                "Title",
                Input::new(&form.title).id("note-title").aria_label("Title"),
                cx,
            ))
            .child(field(
                "Summary, one line for the list",
                Input::new(&form.summary).id("note-summary").aria_label("Summary"),
                cx,
            ))
            // The text area has no ID of its own. Tests find it inside this element.
            .child(field(
                "Text",
                div()
                    .id("note-body")
                    .child(Textarea::new(&form.body).aria_label("Text").h(px(240.))),
                cx,
            ))
            .child(buttons)
            .child(message)
            .children(self.note_history(cx))
            .children(sources);
        view.child(editor).child(self.notes_side(cx))
    }

    /// The column beside the page: the pre-load and the checks of the notes.
    fn notes_side(&self, cx: &Context<Self>) -> impl IntoElement {
        div()
            .id("notes-side")
            .w(px(300.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_4()
            .overflow_y_scroll()
            .child(self.preload_card(cx))
            .child(self.checks_card(cx))
    }

    /// Sends a pre-load of the typed tool and arguments. Arguments that are not a JSON object are not sent.
    fn preload_notes(&mut self, cx: &App) {
        let form = &mut self.notes;
        let Some(profile) = form.profile.clone() else {
            return;
        };
        let tool = form.preload_tool.read(cx).value().trim().to_owned();
        let args = form.preload_args.read(cx).value().trim().to_owned();
        let args = match args.as_str() {
            "" => Ok(serde_json::Map::new()),
            text => serde_json::from_str(text).map_err(|e| format!("The arguments are not a JSON object: {e}")),
        };
        match args {
            Ok(args) => {
                form.preload_message = None;
                let request = PreloadNotes { tool, args };
                self.send(UiCommand::PreloadNotes { profile, request });
            }
            Err(error) => form.preload_message = Some(error),
        }
    }

    /// One read that the agent turns into page changes, which go to the inbox.
    pub(crate) fn preload_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let form = &self.notes;
        let readers: Vec<&str> = self
            .state
            .permissions
            .iter()
            .flat_map(|permissions| &permissions.rules)
            .filter(|(_, rule)| rule.class == ToolClass::Read)
            .map(|(tool, _)| tool.as_str())
            .collect();
        let tools = match readers.is_empty() {
            true => "No tool has a read rule yet.".to_owned(),
            false => format!("Tools that read: {}", readers.join(", ")),
        };
        let result = match (&form.preload_message, &self.state.preload_result) {
            (Some(message), _) => message.clone(),
            (None, None) => String::new(),
            (None, Some(Ok(0))) => "The agent kept no page.".to_owned(),
            (None, Some(Ok(1))) => "1 page change waits in the inbox.".to_owned(),
            (None, Some(Ok(count))) => format!("{count} page changes wait in the inbox."),
            (None, Some(Err(error))) => error.clone(),
        };
        card(cx)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Read into the notes"))
            .child(quiet(PRELOAD_HELP, cx))
            .child(labelled_as("preload-tools", tools.clone(), quiet(tools, cx)))
            .child(field(
                "Tool",
                Input::new(&form.preload_tool).id("preload-tool").aria_label("Tool"),
                cx,
            ))
            .child(field(
                "Arguments, a JSON object",
                Input::new(&form.preload_args)
                    .id("preload-args")
                    .aria_label("Arguments"),
                cx,
            ))
            .child(
                div().flex().child(
                    Button::new("preload-notes")
                        .small()
                        .outline()
                        .label("Read")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.preload_notes(cx);
                            cx.notify();
                        })),
                ),
            )
            .child(div().text_sm().child(labelled("preload-result", result)))
    }

    /// The findings of the last check, with a button to check now and one to dismiss each finding.
    fn checks_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let result = match &self.state.lint_result {
            None => String::new(),
            Some(Ok(0)) => "No finding.".to_owned(),
            Some(Ok(1)) => "1 finding.".to_owned(),
            Some(Ok(found)) => format!("{found} findings."),
            Some(Err(error)) => error.clone(),
        };
        let findings = self.state.lint_findings.as_deref().unwrap_or_default();
        let count = match findings.len() {
            0 => "Nothing to look at.".to_owned(),
            1 => "1 finding to look at.".to_owned(),
            count => format!("{count} findings to look at."),
        };
        let rows = findings.iter().map(|finding| {
            let id = finding.id;
            let text = finding_text(finding);
            div()
                .flex()
                .flex_col()
                .gap_1()
                .py_2()
                .border_b_1()
                .border_color(cx.theme().border)
                .child(labelled_as(
                    format!("finding-{id}"),
                    text.clone(),
                    div().text_sm().child(SharedString::from(text)),
                ))
                .child(
                    div().flex().child(
                        Button::new(SharedString::from(format!("dismiss-finding-{id}")))
                            .small()
                            .outline()
                            .label("Dismiss")
                            .on_click(cx.listener(move |this, _, _, _| {
                                if let Some(profile) = this.notes.profile.clone() {
                                    this.send(UiCommand::DismissFinding { profile, id });
                                }
                            })),
                    ),
                )
        });
        card(cx)
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Checks"))
            .child(quiet(CHECKS_HELP, cx))
            .child(
                div()
                    .flex()
                    .child(
                        Button::new("lint-notes")
                            .small()
                            .outline()
                            .label("Check now")
                            .on_click(cx.listener(|this, _, _, _| {
                                if let Some(profile) = this.notes.profile.clone() {
                                    this.send(UiCommand::LintNotes { profile });
                                }
                            })),
                    ),
            )
            .child(div().text_sm().child(labelled("lint-result", result)))
            .child(labelled_as(
                "findings-count",
                format!("Findings: {}", findings.len()),
                quiet(count, cx),
            ))
            .child(div().flex().flex_col().children(rows))
    }
}
