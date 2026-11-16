//! The main window: profiles, kill switch, approval inbox, audit log, notes, permissions, principles,
//! polls, privacy settings and connections.

use crate::bridge::{UiCommand, UiState};
use crate::connections_form::ConnectionsForm;
use crate::draft_editor::DraftEditor;
use crate::notes_form::NotesForm;
use crate::permissions_form::PermissionsForm;
use crate::polls_form::PollsForm;
use crate::principles_form::PrinciplesForm;
use crate::privacy::PrivacyForm;
use crate::theme::{card, code_block, icon, palette, quiet, state_mark, utc_minute};
use bunsin_api::{Autonomy, Resolution};
use bunsin_core::approval::ApprovalState;
use bunsin_core::records::{InboxBody, InboxItem};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::{
    AnyElement, App, Context, Div, FontWeight, Hsla, IntoElement, ParentElement, Render, SharedString, Styled, Window,
    div, prelude::*, px,
};
use tokio::sync::mpsc;

const AUDIT_ROWS: usize = 50;

pub struct Shell {
    pub state: UiState,
    commands: mpsc::UnboundedSender<UiCommand>,
    pub(crate) privacy: PrivacyForm,
    pub(crate) connections: ConnectionsForm,
    pub(crate) notes: NotesForm,
    pub(crate) permissions: PermissionsForm,
    pub(crate) principles: PrinciplesForm,
    pub(crate) polls: PollsForm,
    /// The edit of a waiting call in the inbox, if one is open.
    pub draft_editor: DraftEditor,
    /// Reads an existing Keychain item for the connections form. Inert unless the app sets one.
    pub(crate) keychain: std::sync::Arc<dyn crate::connection::KeychainLookup>,
    /// The views never share the window, so no form can squeeze the inbox.
    pub view: View,
    /// The `unknown` item whose Run again button was clicked once. A second click on it runs the call.
    retry_armed: Option<i64>,
}

/// What the main part of the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    /// The approval inbox and the audit log.
    Inbox,
    /// The pages of the agent's notes.
    Notes,
    /// What the agent may do with each approved tool.
    Permissions,
    /// How the agent works: the owner's principles and the searches that find events.
    Agent,
    /// The connections and the privacy filter.
    Settings,
}

impl Shell {
    pub fn new(
        mut states: mpsc::UnboundedReceiver<UiState>,
        commands: mpsc::UnboundedSender<UiCommand>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // tokio channels work on the GPUI executor. tokio timers do not, so none is used here.
        cx.spawn_in(window, async move |this, cx| {
            while let Some(state) = states.recv().await {
                let updated = this.update_in(cx, |shell, window, cx| {
                    shell.privacy.receive(&state, window, cx);
                    shell.connections.receive(&state, window, cx);
                    shell.notes.receive(&state, window, cx);
                    shell.permissions.receive(&state);
                    shell.principles.receive(&state, window, cx);
                    shell.polls.receive(&state, window, cx);
                    shell.draft_editor.receive(&state);
                    shell.state = state;
                    cx.notify();
                });
                if updated.is_err() {
                    break;
                }
            }
        })
        .detach();
        let _ = commands.send(UiCommand::Refresh);
        Self {
            state: UiState::default(),
            commands,
            privacy: PrivacyForm::new(window, cx),
            connections: ConnectionsForm::new(window, cx),
            notes: NotesForm::new(window, cx),
            permissions: PermissionsForm::new(),
            principles: PrinciplesForm::new(window, cx),
            polls: PollsForm::new(window, cx),
            draft_editor: DraftEditor::default(),
            keychain: std::sync::Arc::new(crate::connection::NoKeychain),
            view: View::Inbox,
            retry_armed: None,
        }
    }

    /// Lets the connections form read items of the Keychain. Only the real app calls this.
    pub fn set_keychain(&mut self, keychain: std::sync::Arc<dyn crate::connection::KeychainLookup>) {
        self.keychain = keychain;
    }

    pub(crate) fn send(&self, command: UiCommand) {
        // A closed channel means the engine task ended. The window then shows stale state.
        let _ = self.commands.send(command);
    }
}

/// One line that describes an inbox item.
pub fn inbox_title(item: &InboxItem) -> String {
    let what = match &item.body {
        InboxBody::ToolCall { tool, .. } => tool.clone(),
        InboxBody::Escalation { .. } => "needs your decision".to_owned(),
        // A note is not an action. The title says so, to tell it from a tool call at a glance.
        InboxBody::WikiChange { slug, previous, .. } => {
            format!(
                "note `{slug}` ({})",
                if previous.is_some() { "change" } else { "new page" }
            )
        }
    };
    format!("{} · {what}", item.state.as_str())
}

/// True if the owner can report a problem with the item: a tool call that ran without asking and
/// whose tool is not held. A report of a held tool would change nothing.
pub fn reportable(item: &InboxItem, autonomy: Option<&Autonomy>) -> bool {
    let InboxBody::ToolCall { tool, .. } = &item.body else {
        return false;
    };
    let held = autonomy.is_some_and(|autonomy| autonomy.tools.iter().any(|entry| entry.tool == *tool && entry.held));
    item.autonomous && matches!(item.state, ApprovalState::Executed | ApprovalState::Failed) && !held
}

/// What the user must read before deciding. A tool call shows every argument,
/// so the user approves exactly what will run, not only its message text.
pub fn inbox_detail(item: &InboxItem) -> String {
    match &item.body {
        InboxBody::ToolCall { args, .. } => {
            let all = serde_json::to_string_pretty(&serde_json::Value::Object(args.clone()))
                .expect("a JSON value always serializes");
            format!("Arguments:\n{all}")
        }
        InboxBody::Escalation { summary } => summary.clone(),
        // The owner sees the whole new page, where its facts came from, and what it replaces.
        InboxBody::WikiChange {
            text,
            sources,
            previous,
            ..
        } => {
            let sources: Vec<String> = sources
                .iter()
                .map(|s| format!("- {}: {}", s.origin, s.detail))
                .collect();
            let mut detail = format!(
                "{}\n{}\n\n{}\n\nSources:\n{}",
                text.title,
                text.summary,
                text.body,
                sources.join("\n")
            );
            if let Some(previous) = previous {
                detail.push_str(&format!(
                    "\n\nReplaces:\n{}\n{}\n\n{}",
                    previous.title, previous.summary, previous.body
                ));
            }
            detail
        }
    }
}

/// The reads the agent made before it proposed the item, one line per read. None if it made none.
pub fn inbox_basis(item: &InboxItem) -> Option<String> {
    let lines: Vec<_> = item
        .basis
        .iter()
        .map(|read| {
            let args = serde_json::to_string(&read.args).expect("a JSON map always serializes");
            format!("Read: {} {args}", read.tool)
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// What a form shows when the server refused its save because the settings changed meanwhile.
pub(crate) const CONFLICT: &str = "The settings changed on the server since this form loaded them. Nothing was saved.";

impl Shell {
    /// The conflict warning of form `id`, with its two choices.
    pub(crate) fn conflict_warning(
        id: &str,
        text: &'static str,
        load: fn(&mut Self, &mut Window, &mut Context<Self>),
        save_anyway: fn(&mut Self, &mut Context<Self>),
        cx: &Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(labelled(format!("{id}-conflict"), text))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("{id}-load")))
                            .label("Load the server's version")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                load(this, window, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new(SharedString::from(format!("{id}-save-anyway")))
                            .label("Save anyway")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                save_anyway(this, cx);
                                cx.notify();
                            })),
                    ),
            )
    }
}

/// Text that tests can read through its accessibility label. Never pass a secret.
pub(crate) fn labelled(id: impl Into<SharedString>, text: impl Into<SharedString>) -> impl IntoElement {
    let text = text.into();
    // An empty text still has the height of a line, so it is not drawn.
    let drawn = (!text.is_empty()).then(|| text.clone());
    div().id(id.into()).test_support().aria_label(text).children(drawn)
}

/// An element that tests read through `label`, drawn as `content`.
/// Build both from the same value, so that the picture and the label cannot differ.
pub(crate) fn labelled_as(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    content: impl IntoElement,
) -> impl IntoElement {
    div()
        .id(id.into())
        .test_support()
        .aria_label(label.into())
        .child(content)
}

/// The word and the color that show a state of an inbox item.
fn state_view(state: ApprovalState, cx: &App) -> (&'static str, Hsla) {
    let palette = palette(cx);
    let quiet = cx.theme().muted_foreground;
    match state {
        ApprovalState::Draft => ("Draft, cannot run", quiet),
        ApprovalState::Pending => ("Waiting for you", palette.waiting),
        ApprovalState::Approved => ("Approved", palette.done),
        ApprovalState::Executing => ("Running", palette.waiting),
        ApprovalState::Executed => ("Done", palette.done),
        ApprovalState::Rejected => ("Dismissed", quiet),
        ApprovalState::Failed => ("Failed", palette.stopped),
        ApprovalState::Unknown => ("Outcome unknown", palette.stopped),
    }
}

/// One argument value as the owner reads it: a text as it is, anything else as JSON.
fn argument_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

impl Shell {
    /// What an item is, as its first line.
    fn item_head(item: &InboxItem, cx: &App) -> impl IntoElement {
        let (word, color) = match (&item.body, item.state) {
            // An escalation is a draft that only the owner can answer, outside this window.
            (InboxBody::Escalation { .. }, ApprovalState::Draft) => ("Needs your own answer", palette(cx).waiting),
            (_, state) => state_view(state, cx),
        };
        let mono = cx.theme().mono_font_family.clone();
        let what = match &item.body {
            InboxBody::ToolCall { tool, .. } => div().font_family(mono).child(SharedString::from(tool.clone())),
            InboxBody::Escalation { .. } => div().child("Needs your decision"),
            InboxBody::WikiChange { slug, previous, .. } => div()
                .flex()
                .gap_2()
                .child(SharedString::from(format!("Note {slug}")))
                .child(
                    quiet(
                        if previous.is_some() {
                            "changes the page"
                        } else {
                            "new page"
                        },
                        cx,
                    )
                    .font_weight(FontWeight::NORMAL),
                ),
        };
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_3()
            .child(what.font_weight(FontWeight::SEMIBOLD))
            .child(state_mark(word, color))
    }

    /// What the owner must read before deciding. Nothing in it is cut or hidden.
    fn item_detail(item: &InboxItem, cx: &App) -> AnyElement {
        match &item.body {
            // Every argument has its own row, so the owner approves exactly what will run.
            InboxBody::ToolCall { args, .. } => code_block(cx)
                .flex()
                .flex_col()
                .gap_2()
                .children(args.iter().map(|(name, value)| {
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_x_3()
                        .child(
                            div()
                                .min_w(px(96.))
                                .flex_none()
                                .text_color(cx.theme().muted_foreground)
                                .child(SharedString::from(name.clone())),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w(px(280.))
                                .child(SharedString::from(argument_text(value))),
                        )
                }))
                .into_any_element(),
            InboxBody::Escalation { summary } => div().child(SharedString::from(summary.clone())).into_any_element(),
            InboxBody::WikiChange {
                text,
                sources,
                previous,
                ..
            } => {
                let sources = sources
                    .iter()
                    .map(|source| div().child(SharedString::from(format!("{}: {}", source.origin, source.detail))));
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(text.title.clone())),
                    )
                    .child(quiet(text.summary.clone(), cx))
                    .child(div().child(SharedString::from(text.body.clone())))
                    .child(quiet("Where the agent read this", cx))
                    .child(code_block(cx).flex().flex_col().gap_1().children(sources))
                    .children(previous.as_ref().map(|previous| {
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(quiet("The page now, which this replaces", cx))
                            .child(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(SharedString::from(format!(
                                        "{}\n{}\n\n{}",
                                        previous.title, previous.summary, previous.body
                                    ))),
                            )
                    }))
                    .into_any_element()
            }
        }
    }

    /// One inbox item as a card.
    fn item_card(&self, item: &InboxItem, cx: &Context<Self>) -> impl IntoElement {
        let id = item.id;
        let detail = labelled_as(format!("detail-{id}"), inbox_detail(item), Self::item_detail(item, cx));
        let mut row = card(cx)
            .flex_none()
            .child(labelled_as(
                format!("inbox-{id}"),
                inbox_title(item),
                Self::item_head(item, cx),
            ))
            .child(match item.body {
                // A long page scrolls in its own box, so its buttons stay close.
                InboxBody::WikiChange { .. } => div()
                    .id(SharedString::from(format!("detail-box-{id}")))
                    .test_support()
                    .max_h(px(220.))
                    .overflow_y_scroll()
                    // The frame shows that this box is a window onto a longer text.
                    .p_3()
                    .rounded(cx.theme().radius)
                    .border_1()
                    .border_color(cx.theme().border)
                    .child(detail)
                    .into_any_element(),
                // An action shows every argument at full height. None is hidden in a box.
                _ => detail.into_any_element(),
            })
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(quiet("Why", cx).flex_none())
                    .child(div().text_sm().child(SharedString::from(item.rationale.clone()))),
            )
            .children(inbox_basis(item).map(|basis| {
                let lines = item.basis.iter().map(|read| {
                    let args = serde_json::to_string(&read.args).expect("a JSON map always serializes");
                    div().child(SharedString::from(format!("{} {args}", read.tool)))
                });
                labelled_as(
                    format!("basis-{id}"),
                    basis,
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(quiet("Read before this proposal", cx))
                        .child(
                            div()
                                .font_family(cx.theme().mono_font_family.clone())
                                .text_size(cx.theme().mono_font_size)
                                .text_color(cx.theme().muted_foreground)
                                .children(lines),
                        ),
                )
            }));
        // While the owner edits the text, the editor has its own buttons.
        if let Some(editor) = self.draft_editor_section(id, cx) {
            return row.child(editor);
        }
        let mut buttons = div().flex().gap_2().pt_1();
        if item.state == ApprovalState::Pending {
            buttons = buttons
                .child(
                    Button::new(SharedString::from(format!("approve-{id}")))
                        .primary()
                        // A note is stored. Nothing runs on another system.
                        .label(match item.body {
                            InboxBody::WikiChange { .. } => "Approve and keep",
                            _ => "Approve and run",
                        })
                        .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::Approve(id)))),
                )
                .children(self.edit_button(item, cx));
        }
        if item.state == ApprovalState::Unknown {
            return row
                .child(labelled(
                    format!("unknown-note-{id}"),
                    "The server stopped while this call ran. It may have reached its tool. \
                     Check the target system, then choose.",
                ))
                .child(
                    buttons
                        .child(self.resolve_button(id, Resolution::Done, "It ran: mark done", cx))
                        .child(self.resolve_button(id, Resolution::Drop, "It did not run: drop", cx))
                        .child(self.resolve_button(id, Resolution::Retry, "Run again", cx)),
                )
                .child(labelled(
                    format!("retry-warning-{id}"),
                    "Run again sends the call as a new item. If the first call reached its tool, it is sent twice.",
                ));
        }
        if matches!(item.state, ApprovalState::Pending | ApprovalState::Draft) {
            buttons = buttons.child(
                Button::new(SharedString::from(format!("reject-{id}")))
                    .ghost()
                    .label("Dismiss")
                    .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::Reject(id)))),
            );
            row = row.child(buttons);
        }
        if reportable(item, self.state.autonomy.as_ref()) {
            row = row.child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .gap_2()
                    .pt_1()
                    .child(
                        Button::new(SharedString::from(format!("report-{id}")))
                            .outline()
                            .label("Report a problem")
                            .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::ReportAutonomous(id)))),
                    )
                    .child(quiet(
                        "This call ran without asking. A report holds its tool: each later call asks you.",
                        cx,
                    )),
            );
        }
        row
    }

    /// A button that resolves an item whose call a stop cut.
    fn resolve_button(&self, id: i64, resolution: Resolution, text: &'static str, cx: &Context<Self>) -> Button {
        let name = match resolution {
            Resolution::Done => "done",
            Resolution::Drop => "drop",
            Resolution::Retry => "retry",
        };
        let button = Button::new(SharedString::from(format!("{name}-{id}")));
        // Only a retry can send something, maybe a second time. It looks like a risk and needs a second click.
        if resolution == Resolution::Retry {
            let armed = self.retry_armed == Some(id);
            return button
                .danger()
                .label(if armed { "Yes, run it again" } else { text })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.retry(id);
                    cx.notify();
                }));
        }
        button
            .outline()
            .label(text)
            .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::ResolveUnknown(id, resolution))))
    }

    /// The first click on Run again of an item asks. The second click on the same item runs the call.
    fn retry(&mut self, id: i64) {
        if self.retry_armed != Some(id) {
            self.retry_armed = Some(id);
            return;
        }
        self.retry_armed = None;
        self.send(UiCommand::ResolveUnknown(id, Resolution::Retry));
    }

    /// The left column: the views, the profiles and the state of the connection.
    fn sidebar(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (accent_bg, quiet_ink, primary, on_primary) = (
            theme.sidebar_accent,
            theme.muted_foreground,
            theme.primary,
            theme.primary_foreground,
        );
        let radius = theme.radius;
        let waiting = self.state.snapshot.as_ref().map_or(0, |snapshot| {
            snapshot
                .inbox
                .iter()
                .filter(|item| item.state == ApprovalState::Pending)
                .count()
        });
        let views = [
            ("tab-inbox", "Inbox", "inbox", View::Inbox, waiting),
            ("tab-notes", "Notes", "book-open", View::Notes, 0),
            ("tab-permissions", "Permissions", "circle-check", View::Permissions, 0),
            ("tab-agent", "Agent", "bot", View::Agent, 0),
            ("tab-settings", "Settings", "settings", View::Settings, 0),
        ]
        .map(|(id, text, picture, view, count)| {
            let open = self.view == view;
            div()
                .id(id)
                .test_support()
                .aria_label(text)
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded(radius)
                .cursor_pointer()
                .when(open, |row| row.bg(accent_bg).font_weight(FontWeight::MEDIUM))
                .when(!open, |row| row.text_color(quiet_ink).hover(|row| row.bg(accent_bg)))
                .child(icon(picture).size_4())
                .child(div().flex_1().child(text))
                // The number of items that wait for the owner.
                .children((count > 0).then(|| {
                    div()
                        .px_1p5()
                        .rounded_full()
                        .bg(primary)
                        .text_color(on_primary)
                        .text_xs()
                        .child(SharedString::from(count.to_string()))
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.view = view;
                    cx.notify();
                }))
        });

        let profiles = self.state.profiles.iter().map(|profile| {
            let id = profile.id.clone();
            let selected = self.state.selected.as_deref() == Some(id.as_str());
            let label = format!(
                "{}{} ({})",
                if selected { "● " } else { "" },
                profile.display_name,
                profile.id
            );
            let initial: String = profile.display_name.chars().take(1).collect();
            div()
                .id(SharedString::from(format!("profile-{id}")))
                .test_support()
                .aria_label(label)
                .flex()
                .items_center()
                .gap_2()
                .px_2()
                .py_1p5()
                .rounded(radius)
                .cursor_pointer()
                .when(selected, |row| row.bg(accent_bg))
                .when(!selected, |row| row.hover(|row| row.bg(accent_bg)))
                .child(
                    div()
                        .size(px(22.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_xs()
                        .when(selected, |mark| mark.bg(primary).text_color(on_primary))
                        .when(!selected, |mark| mark.bg(accent_bg).text_color(quiet_ink))
                        .child(SharedString::from(initial)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(SharedString::from(profile.display_name.clone())),
                )
                .child(quiet(profile.id.clone(), cx))
                .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::SelectProfile(id.clone()))))
        });

        let palette = palette(cx);
        let connection = if self.state.connected {
            state_mark("Connected to the server", palette.done)
        } else {
            state_mark("Not connected", palette.stopped)
        };
        div()
            .w(px(232.))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .gap_5()
            .p_3()
            .bg(theme.sidebar)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Bunsin"),
            )
            .child(div().flex().flex_col().gap_0p5().children(views))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(quiet("Profiles", cx).px_2().pb_1())
                    .children(profiles),
            )
            .child(div().flex_1())
            // The connect screen puts its Disconnect button into the room below.
            .child(div().px_2().pb_10().child(labelled_as(
                "connection",
                if self.state.connected {
                    "connected"
                } else {
                    "not connected"
                },
                connection,
            )))
    }

    /// The line above every view: its name, the state of the agent and the kill switch.
    fn header(&self, cx: &Context<Self>) -> impl IntoElement {
        let snapshot = self.state.snapshot.as_ref();
        let stopped = snapshot.is_some_and(|s| s.kill_switch);
        let palette = palette(cx);
        let (status, color) = match snapshot {
            None if !self.state.connected => ("Not connected to the server", palette.stopped),
            None => ("No profile", cx.theme().muted_foreground),
            Some(_) if stopped => ("Stopped: the agent makes no changes", palette.stopped),
            Some(_) => ("Running", palette.done),
        };
        let title = match self.view {
            View::Inbox => "Inbox",
            View::Notes => "Notes",
            View::Permissions => "Permissions",
            View::Agent => "Agent",
            View::Settings => "Settings",
        };
        let switch = Button::new("kill-switch")
            .label(if stopped { "Resume changes" } else { "Stop all changes" })
            .icon(icon(if stopped { "play" } else { "pause" }))
            .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::SetKillSwitch(!stopped))));
        div()
            .flex()
            .items_center()
            .gap_4()
            .child(div().flex_1().text_xl().font_weight(FontWeight::SEMIBOLD).child(title))
            .child(labelled_as("status", status, state_mark(status, color)))
            .child(if stopped { switch.primary() } else { switch.outline() })
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The forms are built first. They need `cx` for writing.
        let notes = (self.view == View::Notes).then(|| self.notes_section(cx).into_any_element());
        let forms = (self.view == View::Settings).then(|| {
            (
                self.connections_section(cx).into_any_element(),
                self.privacy_section(cx).into_any_element(),
            )
        });
        let cx = &*cx;
        let theme = cx.theme();
        let snapshot = self.state.snapshot.as_ref();
        let items = snapshot.map(|s| s.inbox.as_slice()).unwrap_or_default();
        let waiting = items.iter().filter(|item| item.state == ApprovalState::Pending).count();
        let audit = snapshot.map(|s| s.audit.as_slice()).unwrap_or_default();
        let error = self.state.error.clone().unwrap_or_default();

        let page = if self.view == View::Permissions {
            self.permissions_section(cx)
        } else if self.view == View::Agent {
            div()
                .id("agent")
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .flex()
                .flex_wrap()
                .items_start()
                .gap_5()
                .overflow_y_scroll()
                .child(self.principles_section(cx))
                .child(self.polls_section(cx))
                .into_any_element()
        } else if let Some(notes) = notes {
            notes
        } else if let Some((connections, privacy)) = forms {
            div()
                .id("settings")
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .flex()
                .flex_wrap()
                .items_start()
                .gap_5()
                .overflow_y_scroll()
                .child(connections)
                .child(privacy)
                .into_any_element()
        } else {
            let activity = audit.iter().take(AUDIT_ROWS).map(|entry| {
                div()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .py_2()
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .child(div().text_sm().child(SharedString::from(entry.kind.replace('_', " "))))
                            .child(quiet(utc_minute(entry.at), cx).flex_none()),
                    )
                    .child(
                        div()
                            .font_family(theme.mono_font_family.clone())
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(SharedString::from(entry.detail.to_string())),
                    )
            });
            div()
                .flex_1()
                .min_h(px(0.))
                // Long lines must not push the columns out of the window.
                .min_w(px(0.))
                .flex()
                .gap_5()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(labelled_as(
                            "inbox-count",
                            format!("Approval inbox: {}", items.len()),
                            quiet(
                                match (snapshot.is_some(), items.len(), waiting) {
                                    // Without an answer of the server, the list says nothing about the inbox.
                                    (false, ..) => String::new(),
                                    (true, 0, _) => {
                                        "Nothing is here yet. What the agent proposes appears in this list.".to_owned()
                                    }
                                    (true, all, 0) => format!("{all} in the list, none waits for you"),
                                    (true, all, 1) => format!("{all} in the list, 1 waits for you"),
                                    (true, all, waiting) => format!("{all} in the list, {waiting} wait for you"),
                                },
                                cx,
                            ),
                        ))
                        .child(
                            div()
                                .id("inbox")
                                // The list has the whole height. The activity log is beside it, not below.
                                .flex_1()
                                .min_h(px(0.))
                                .flex()
                                .flex_col()
                                .gap_3()
                                .overflow_y_scroll()
                                .children(items.iter().map(|item| self.item_card(item, cx))),
                        ),
                )
                .child(
                    div()
                        .w(px(330.))
                        .flex_none()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(labelled_as(
                            "audit-count",
                            format!("Audit log: {} entries", audit.len()),
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(div().font_weight(FontWeight::SEMIBOLD).child("Activity"))
                                .child(quiet(
                                    match audit.len() {
                                        1 => "1 entry, time in UTC".to_owned(),
                                        count => format!("{count} entries, times in UTC"),
                                    },
                                    cx,
                                )),
                        ))
                        .child(
                            div()
                                .id("audit")
                                .flex_1()
                                .min_h(px(0.))
                                .flex()
                                .flex_col()
                                .overflow_y_scroll()
                                .children(activity),
                        ),
                )
                .into_any_element()
        };

        div()
            .size_full()
            .flex()
            .bg(theme.background)
            .text_color(theme.foreground)
            .text_size(theme.font_size)
            .child(self.sidebar(cx))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .p_6()
                    .child(
                        // The failure of a command sits under the header. It takes no room while there is none.
                        div().flex().flex_col().child(self.header(cx)).child(labelled_as(
                            "error",
                            error.clone(),
                            // An empty text still has the height of a line, so it is not drawn.
                            div().children((!error.is_empty()).then(|| {
                                div()
                                    .pt_2()
                                    .text_color(palette(cx).stopped)
                                    .child(SharedString::from(error))
                            })),
                        )),
                    )
                    .child(page),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(body: InboxBody) -> InboxItem {
        InboxItem {
            id: 1,
            event_key: "k".into(),
            body,
            rationale: "r".into(),
            basis: Vec::new(),
            state: ApprovalState::Pending,
            created_at: 0,
            updated_at: 0,
            autonomous: false,
            retry_of: None,
        }
    }

    #[test]
    fn only_a_finished_autonomous_call_of_a_tool_that_is_not_held_can_be_reported() {
        let post = || InboxBody::ToolCall {
            tool: "slack/post".into(),
            args: json!({"text": "hi"}).as_object().unwrap().clone(),
        };
        let ran = InboxItem {
            autonomous: true,
            state: ApprovalState::Executed,
            ..item(post())
        };
        assert!(reportable(&ran, None));
        let failed = InboxItem {
            state: ApprovalState::Failed,
            ..ran.clone()
        };
        assert!(reportable(&failed, None));
        for state in [
            ApprovalState::Approved,
            ApprovalState::Executing,
            ApprovalState::Unknown,
        ] {
            assert!(!reportable(&InboxItem { state, ..ran.clone() }, None), "{state:?}");
        }
        let approved_by_owner = InboxItem {
            autonomous: false,
            ..ran.clone()
        };
        assert!(!reportable(&approved_by_owner, None));
        let entry = |tool: &str, held| bunsin_api::ToolAutonomy {
            tool: tool.into(),
            counts: Default::default(),
            held,
            suggested: false,
        };
        let autonomy = |tools| Autonomy {
            thresholds: None,
            tools,
        };
        assert!(!reportable(&ran, Some(&autonomy(vec![entry("slack/post", true)]))));
        assert!(reportable(
            &ran,
            Some(&autonomy(vec![entry("slack/post", false), entry("slack/react", true)]))
        ));
    }

    #[test]
    fn a_tool_call_shows_every_argument_not_only_the_text() {
        let args = json!({"channel": "C-ALL-COMPANY", "text": "hello", "broadcast": true});
        let body = InboxBody::ToolCall {
            tool: "slack/post".into(),
            args: args.as_object().unwrap().clone(),
        };
        let detail = inbox_detail(&item(body));
        for expected in [
            "\"channel\": \"C-ALL-COMPANY\"",
            "\"text\": \"hello\"",
            "\"broadcast\": true",
        ] {
            assert!(detail.contains(expected), "{detail}");
        }
    }

    #[test]
    fn the_basis_has_one_line_per_read_and_none_without_reads() {
        let mut with_reads = item(InboxBody::Escalation { summary: "s".into() });
        assert_eq!(inbox_basis(&with_reads), None);
        let read = |tool: &str, args: serde_json::Value| bunsin_core::records::ReadNote {
            tool: tool.into(),
            args: args.as_object().unwrap().clone(),
        };
        with_reads.basis = vec![
            read("slack/search", json!({"query": "deploy date"})),
            read("jira/get_issue", json!({"key": "PRJ-1"})),
        ];
        assert_eq!(
            inbox_basis(&with_reads).as_deref(),
            Some("Read: slack/search {\"query\":\"deploy date\"}\nRead: jira/get_issue {\"key\":\"PRJ-1\"}")
        );
    }

    #[test]
    fn titles_name_the_state_and_the_tool() {
        let call = InboxBody::ToolCall {
            tool: "jira/add_comment".into(),
            args: Default::default(),
        };
        assert_eq!(inbox_title(&item(call)), "pending · jira/add_comment");
        let escalation = InboxBody::Escalation {
            summary: "Decide the budget.".into(),
        };
        assert_eq!(inbox_title(&item(escalation.clone())), "pending · needs your decision");
        assert_eq!(inbox_detail(&item(escalation)), "Decide the budget.");
    }

    #[test]
    fn a_note_shows_its_whole_text_its_sources_and_what_it_replaces() {
        use bunsin_core::wiki::{WikiSource, WikiText};
        let text = |body: &str| WikiText {
            title: "Deploy schedule".into(),
            summary: "When we deploy".into(),
            body: body.into(),
        };
        let new_page = InboxBody::WikiChange {
            slug: "deploy".into(),
            text: text("Fridays at 3 pm."),
            sources: vec![WikiSource {
                origin: "slack/search".into(),
                detail: "{\"query\":\"deploy\"}".into(),
            }],
            base_revision: 0,
            previous: None,
        };
        assert_eq!(
            inbox_title(&item(new_page.clone())),
            "pending · note `deploy` (new page)"
        );
        let detail = inbox_detail(&item(new_page));
        assert!(
            detail.contains("Fridays at 3 pm.") && detail.contains("- slack/search: {\"query\":\"deploy\"}"),
            "{detail}"
        );
        assert!(!detail.contains("Replaces:"));

        let change = InboxBody::WikiChange {
            slug: "deploy".into(),
            text: text("Thursdays."),
            sources: vec![],
            base_revision: 3,
            previous: Some(text("Fridays at 3 pm.")),
        };
        assert_eq!(inbox_title(&item(change.clone())), "pending · note `deploy` (change)");
        let detail = inbox_detail(&item(change));
        assert!(
            detail.contains("Thursdays.") && detail.contains("Replaces:") && detail.contains("Fridays at 3 pm."),
            "{detail}"
        );
    }
}
