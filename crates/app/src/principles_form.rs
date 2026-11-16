//! The principles of the agent view: the owner's own instructions to the agent.
//! The owner edits a draft of the list. Only Save sends it to the server.

use crate::bridge::{SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled, labelled_as};
use crate::theme::{card, palette, quiet};
use bunsin_api::{Expected, PrinciplesUpdate};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Stateful, Styled, Window,
    div, prelude::*, px,
};

const HELP: &str = "The agent follows these in every reply. They are your own instructions, so the privacy \
    filter does not change them. Everything else the agent reads, such as messages and pages, is data for it, \
    not an instruction.";
const UNADDED: &str = "Add the principle or clear its field first.";
const EMPTY: &str = "Write the principle first.";
const UNCHANGED: &str = "Nothing changed, so nothing was saved.";

/// The list the owner edits, and the server's list it was made from.
struct Draft {
    profile: String,
    /// The server's list when the form loaded it. A save expects the server to hold it still.
    base: Vec<String>,
    value: Vec<String>,
}

pub struct PrinciplesForm {
    /// None until the server's list arrives.
    draft: Option<Draft>,
    /// The value of `UiState::principle_saves` in the last state the form received.
    saves_seen: u64,
    /// A new principle, or the changed text of the principle that `editing` names.
    text: Entity<TextareaState>,
    /// The position of the principle whose text is in the field.
    editing: Option<usize>,
    /// The window's own message about the last Add or Save that it did not do.
    message: Option<&'static str>,
    /// True after the server refused a save because its list changed since the form loaded it.
    conflict: bool,
}

impl PrinciplesForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            draft: None,
            saves_seen: 0,
            text: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(2, 8)
                    .placeholder("For example: Never promise a date without checking Jira.")
            }),
            editing: None,
            message: None,
            conflict: false,
        }
    }

    /// Takes a new state from the server. A changed draft stays. The form loads the server's list
    /// for another profile, after a successful save, or if the owner did not change the draft.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let new = state.principle_saves != self.saves_seen;
        self.saves_seen = state.principle_saves;
        let saved = new && state.principles_result == Some(Ok(()));
        if new && state.principles_result == Some(Err(SaveError::Conflict)) {
            self.conflict = true;
        }
        if self.draft.as_ref().map(|d| &d.profile) != state.selected.as_ref() {
            self.draft = None;
            self.message = None;
            self.conflict = false;
            self.clear_field(window, cx);
        }
        let (Some(profile), Some(server)) = (&state.selected, &state.principles) else {
            return;
        };
        match &self.draft {
            Some(draft) if !saved && (draft.value != draft.base || draft.base == *server) => {}
            _ => self.load(profile, server),
        }
    }

    fn load(&mut self, profile: &str, server: &[String]) {
        self.draft = Some(Draft {
            profile: profile.to_owned(),
            base: server.to_vec(),
            value: server.to_vec(),
        });
        self.editing = None;
        self.conflict = false;
    }

    fn clear_field(&mut self, window: &mut Window, cx: &mut App) {
        self.text.update(cx, |area, cx| area.set_value("", window, cx));
        self.editing = None;
    }

    /// Adds the text of the field as a new principle, or puts it in place of the edited one.
    /// The server checks the limits on Save.
    fn add(&mut self, window: &mut Window, cx: &mut App) {
        let text = self.text.read(cx).value().trim().to_owned();
        let Some(draft) = &mut self.draft else {
            return;
        };
        if text.is_empty() {
            self.message = Some(EMPTY);
            return;
        }
        match self.editing {
            Some(at) => draft.value[at] = text,
            None => draft.value.push(text),
        }
        self.message = None;
        self.clear_field(window, cx);
    }

    /// Puts principle `at` into the field to change it.
    fn edit(&mut self, at: usize, window: &mut Window, cx: &mut App) {
        let Some(draft) = &self.draft else {
            return;
        };
        let text = draft.value[at].clone();
        self.text.update(cx, |area, cx| area.set_value(text, window, cx));
        self.editing = Some(at);
        self.message = None;
    }

    /// Moves principle `at` one place up (`up`) or down. The first principle has no place above it.
    fn swap(&mut self, at: usize, up: bool) {
        let Some(draft) = &mut self.draft else {
            return;
        };
        let other = if up { at.checked_sub(1) } else { Some(at + 1) };
        if let Some(other) = other.filter(|other| *other < draft.value.len()) {
            draft.value.swap(at, other);
        }
    }
}

impl Shell {
    /// Sends the draft. The server refuses it if its list is no longer the base, unless `anyway` is true.
    fn save_principles(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.principles;
        // Text in the field would not be saved.
        if !form.text.read(cx).value().trim().is_empty() {
            form.message = Some(UNADDED);
            return;
        }
        let Some(draft) = &form.draft else {
            return;
        };
        if draft.value == draft.base && !anyway {
            form.message = Some(UNCHANGED);
            return;
        }
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(draft.base.clone())
        };
        let command = UiCommand::SavePrinciples {
            profile: draft.profile.clone(),
            update: PrinciplesUpdate {
                principles: draft.value.clone(),
                expected,
            },
        };
        form.message = None;
        form.conflict = false;
        self.send(command);
    }

    /// Drops the draft and shows the server's list.
    fn load_principles(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(server), Some(draft)) = (&self.state.principles, &self.principles.draft) {
            let (profile, server) = (draft.profile.clone(), server.clone());
            self.principles.load(&profile, &server);
            self.principles.clear_field(window, cx);
        }
    }

    /// One principle with its controls. While a principle is in the field, the list does not change.
    fn principle_row(&self, at: usize, text: &str, count: usize, cx: &Context<Self>) -> Stateful<gpui_kit::Div> {
        let editing = self.principles.editing;
        let small = |id: String, label: &'static str| Button::new(SharedString::from(id)).small().ghost().label(label);
        let buttons = editing.is_none().then(|| {
            div()
                .flex()
                .flex_none()
                .gap_1()
                .children((at > 0).then(|| {
                    small(format!("principle-up-{at}"), "Up").on_click(cx.listener(move |this, _, _, cx| {
                        this.principles.swap(at, true);
                        cx.notify();
                    }))
                }))
                .children((at + 1 < count).then(|| {
                    small(format!("principle-down-{at}"), "Down").on_click(cx.listener(move |this, _, _, cx| {
                        this.principles.swap(at, false);
                        cx.notify();
                    }))
                }))
                .child(small(format!("principle-edit-{at}"), "Edit").on_click(cx.listener(
                    move |this, _, window, cx| {
                        this.principles.edit(at, window, cx);
                        cx.notify();
                    },
                )))
                .child(small(format!("principle-remove-{at}"), "Remove").on_click(cx.listener(
                    move |this, _, _, cx| {
                        if let Some(draft) = &mut this.principles.draft {
                            draft.value.remove(at);
                        }
                        cx.notify();
                    },
                )))
        });
        div()
            .id(SharedString::from(format!("principle-row-{at}")))
            .flex()
            .items_start()
            .gap_3()
            .py_1()
            .when(editing == Some(at), |row| row.bg(cx.theme().muted))
            .child(quiet(format!("{}.", at + 1), cx).flex_none().w(px(24.)))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(labelled(format!("principle-{at}"), text.to_owned())),
            )
            .children(buttons)
    }

    /// The principles card. It shows the list only while the server's list is known.
    pub(crate) fn principles_section(&self, cx: &Context<Self>) -> impl IntoElement {
        let section = card(cx)
            .id("principles")
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Principles"))
            .child(labelled_as("principles-help", HELP, quiet(HELP, cx)));
        let form = &self.principles;
        let (Some(server), Some(draft)) = (&self.state.principles, &form.draft) else {
            return section;
        };
        let count = draft.value.len();
        let rows = draft
            .value
            .iter()
            .enumerate()
            .map(|(at, text)| self.principle_row(at, text, count, cx));
        let changed = draft.value != *server;
        let result = match &self.state.principles_result {
            Some(Err(SaveError::Failed(error))) => error.clone(),
            Some(Ok(())) if !changed && form.message.is_none() => "Saved".to_owned(),
            _ => String::new(),
        };
        let add_label = match form.editing {
            Some(at) => format!("Change principle {}", at + 1),
            None => "Add the principle".to_owned(),
        };
        section
            .child(labelled_as(
                "principles-count",
                format!("Principles: {count}"),
                quiet(
                    match count {
                        0 => "None yet. The agent then follows only the built-in rules.".to_owned(),
                        1 => "1 principle".to_owned(),
                        count => format!("{count} principles, in this order in every prompt"),
                    },
                    cx,
                ),
            ))
            .child(div().flex().flex_col().children(rows))
            // The text area has no ID of its own. Tests find it inside this element.
            .child(
                div()
                    .id("principle-text")
                    .child(Textarea::new(&form.text).aria_label("Principle")),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new("add-principle")
                            .outline()
                            .label(add_label)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.principles.add(window, cx);
                                cx.notify();
                            })),
                    )
                    .children(form.editing.map(|_| {
                        Button::new("cancel-principle-edit")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.principles.clear_field(window, cx);
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .text_color(palette(cx).stopped)
                    .text_sm()
                    .child(labelled("principles-message", form.message.unwrap_or_default())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("save-principles")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_principles(false, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette(cx).waiting)
                            .child(labelled("principles-changes", if changed { "Not saved" } else { "" })),
                    )
                    .child(div().text_sm().child(labelled("principles-result", result))),
            )
            .children(form.conflict.then(|| {
                Self::conflict_warning(
                    "principles",
                    CONFLICT,
                    Self::load_principles,
                    |this, cx| this.save_principles(true, cx),
                    cx,
                )
            }))
    }
}
