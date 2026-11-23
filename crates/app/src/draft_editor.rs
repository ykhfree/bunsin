//! The editor of a waiting tool call in the inbox. The owner changes the text that the call
//! sends to other people and approves the call with it. The server adds the AI marker after
//! the edit, so the editor shows the marker as fixed text, not in a text area.

use crate::bridge::{UiCommand, UiState};
use crate::shell::{Shell, labelled, labelled_as};
use crate::theme::{palette, quiet};
use bunsin_api::{Edit, Permissions};
use bunsin_core::approval::ApprovalState;
use bunsin_core::marker::{TextPlace, set_text_at, strip_marker, text_places};
use bunsin_core::records::{InboxBody, InboxItem};
use gpui_kit::component::ActiveTheme;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::{
    App, AppContext, Context, Div, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::*,
};
use serde_json::{Map, Value};

const HELP: &str = "Change the text that the call sends. The other arguments stay as shown above.";
const EMPTY_TEXT: &str = "A text is empty. Write it, or dismiss the item.";

/// The places of outgoing text of a waiting tool call, or None if the owner cannot edit it:
/// the call is not waiting, its rule sends no text, or its arguments miss a place of the rule.
pub fn editable_places(item: &InboxItem, permissions: &Permissions) -> Option<Vec<TextPlace>> {
    let InboxBody::ToolCall { tool, args } = &item.body else {
        return None;
    };
    if item.state != ApprovalState::Pending {
        return None;
    }
    let rule = permissions.rules.get(tool)?;
    text_places(args, &rule.outgoing_text)
        .ok()
        .filter(|places| !places.is_empty())
}

/// One open edit.
struct Open {
    id: i64,
    /// The arguments of the item when the owner pressed Edit. The edit changes only the places of text.
    args: Map<String, Value>,
    /// Each place with its text area. The text areas start with the text without the marker.
    places: Vec<(TextPlace, Entity<TextareaState>)>,
    reason: Entity<InputState>,
    message: Option<&'static str>,
}

#[derive(Default)]
pub struct DraftEditor {
    /// The profile of the open edit.
    profile: Option<String>,
    open: Option<Open>,
}

impl DraftEditor {
    /// The item that is being edited.
    pub fn editing(&self) -> Option<i64> {
        self.open.as_ref().map(|open| open.id)
    }

    /// Ends the edit if another profile is selected, or if the item is no longer the waiting
    /// call that the edit started from. The owner then sees the item as the server has it.
    pub fn receive(&mut self, state: &UiState) {
        if self.profile != state.selected {
            self.open = None;
            self.profile = state.selected.clone();
        }
        // Without an answer from the server, the state says nothing about the item.
        let (Some(open), Some(snapshot)) = (&self.open, &state.snapshot) else {
            return;
        };
        let unchanged = snapshot.inbox.iter().any(|item| {
            item.id == open.id
                && item.state == ApprovalState::Pending
                && matches!(&item.body, InboxBody::ToolCall { args, .. } if *args == open.args)
        });
        if !unchanged {
            self.open = None;
        }
    }

    /// The arguments with the texts of the text areas, or None if every text is as it was.
    fn edited_args(open: &Open, marker: &str, cx: &App) -> Option<Map<String, Value>> {
        let mut args = open.args.clone();
        let mut changed = false;
        for (place, area) in &open.places {
            let text = area.read(cx).value().to_string();
            changed |= text != strip_marker(&place.text, marker);
            set_text_at(&mut args, &place.steps, &text);
        }
        changed.then_some(args)
    }
}

impl Shell {
    /// Opens the editor for item `id`. An edit of another item is dropped.
    fn open_editor(&mut self, item: &InboxItem, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(permissions), InboxBody::ToolCall { args, .. }) = (&self.state.permissions, &item.body) else {
            return;
        };
        let Some(places) = editable_places(item, permissions) else {
            return;
        };
        let marker = permissions.ai_marker.clone();
        let places = places
            .into_iter()
            .map(|place| {
                let text = strip_marker(&place.text, &marker).to_owned();
                let area = cx.new(|cx| {
                    let mut area = TextareaState::new(window, cx).auto_grow(3, 16);
                    area.set_value(text, window, cx);
                    area
                });
                (place, area)
            })
            .collect();
        self.draft_editor.profile = self.state.selected.clone();
        self.draft_editor.open = Some(Open {
            id: item.id,
            args: args.clone(),
            places,
            reason: cx.new(|cx| InputState::new(window, cx).placeholder("For example: shorter, no date")),
            message: None,
        });
    }

    /// Approves the item with the texts of the editor. Without a change it approves the item as it is.
    fn approve_edited(&mut self, cx: &mut Context<Self>) {
        let (Some(profile), Some(open), Some(permissions)) = (
            self.draft_editor.profile.clone(),
            self.draft_editor.open.as_mut(),
            &self.state.permissions,
        ) else {
            return;
        };
        if open
            .places
            .iter()
            .any(|(_, area)| area.read(cx).value().trim().is_empty())
        {
            open.message = Some(EMPTY_TEXT);
            return;
        }
        open.message = None;
        let command = match DraftEditor::edited_args(open, &permissions.ai_marker, cx) {
            None => UiCommand::Approve(open.id),
            Some(args) => UiCommand::ApproveEdited {
                profile,
                id: open.id,
                edit: Edit {
                    args,
                    reason: open.reason.read(cx).value().trim().to_owned(),
                },
            },
        };
        self.send(command);
    }

    /// The Edit button of a waiting call, or nothing if the call has no text that the owner can edit.
    pub(crate) fn edit_button(&self, item: &InboxItem, cx: &Context<Self>) -> Option<impl IntoElement> {
        let permissions = self.state.permissions.as_ref()?;
        editable_places(item, permissions)?;
        let item = item.clone();
        Some(
            Button::new(SharedString::from(format!("edit-{}", item.id)))
                .outline()
                .label("Edit the text")
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.open_editor(&item, window, cx);
                    cx.notify();
                })),
        )
    }

    /// The editor of item `id` with its buttons, if it is open for that item.
    pub(crate) fn draft_editor_section(&self, id: i64, cx: &Context<Self>) -> Option<Div> {
        let open = self.draft_editor.open.as_ref().filter(|open| open.id == id)?;
        let marker = self
            .state
            .permissions
            .as_ref()
            .map(|p| p.ai_marker.clone())
            .unwrap_or_default();
        let several = open.places.len() > 1;
        let places = open.places.iter().enumerate().map(|(n, (place, area))| {
            // One place is the text. Several places are named, so the owner knows which is which.
            let name = if several {
                format!("Text at {}", place.label())
            } else {
                format!("Text ({})", place.label())
            };
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(labelled_as(
                    format!("edit-place-{id}-{n}"),
                    name.clone(),
                    quiet(name, cx),
                ))
                // The text area has no ID of its own. Tests find it inside this element.
                .child(
                    div()
                        .id(SharedString::from(format!("edit-text-{id}-{n}")))
                        .child(Textarea::new(area).aria_label("Text to send")),
                )
        });
        let marker_text = format!("The server adds this line after the text: {marker}");
        let mono = cx.theme().mono_font_family.clone();
        Some(
            div()
                .flex()
                .flex_col()
                .gap_2()
                .pt_1()
                .child(div().font_weight(FontWeight::MEDIUM).child("Edit before approval"))
                .child(quiet(HELP, cx))
                .children(places)
                .child(labelled_as(
                    format!("edit-marker-{id}"),
                    marker_text,
                    div()
                        .flex()
                        .flex_wrap()
                        .gap_x_2()
                        .child(quiet("The server adds this line after the text:", cx))
                        .child(quiet(marker, cx).font_family(mono)),
                ))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(quiet("Why you changed it (optional). The agent learns from it.", cx))
                        .child(
                            Input::new(&open.reason)
                                .id(SharedString::from(format!("edit-reason-{id}")))
                                .aria_label("Why you changed it"),
                        ),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(palette(cx).stopped)
                        .child(labelled(format!("edit-message-{id}"), open.message.unwrap_or_default())),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new(SharedString::from(format!("approve-edited-{id}")))
                                .primary()
                                .label("Approve and run with this text")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.approve_edited(cx);
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new(SharedString::from(format!("cancel-edit-{id}")))
                                .ghost()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.draft_editor.open = None;
                                    cx.notify();
                                })),
                        ),
                ),
        )
    }
}
