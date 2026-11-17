//! What the cards that save one part of the settings share: the server's value they loaded, the
//! conflict check of a save, and the line with the Save button.

use crate::bridge::{Part, SaveError, UiState};
use crate::shell::{Shell, labelled};
use crate::theme::palette;
use bunsin_api::Expected;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::{Context, Div, ParentElement, SharedString, Styled, div};

/// What a card of one part of the settings loaded from the server, and what came of its saves.
pub(crate) struct PartTrack<T> {
    part: Part,
    /// The profile whose part the card holds.
    profile: Option<String>,
    /// The server's value when the card loaded it. A save expects the server to hold it still.
    pub base: Option<T>,
    /// The value of `UiState::part_saves` in the last state the card received.
    saves_seen: u64,
    /// True after the server refused a save because the part changed since the card loaded it.
    pub conflict: bool,
    /// The window's own message about the last save or add that it did not do.
    pub message: Option<&'static str>,
}

impl<T: Clone + PartialEq> PartTrack<T> {
    pub fn new(part: Part) -> Self {
        Self {
            part,
            profile: None,
            base: None,
            saves_seen: 0,
            conflict: false,
            message: None,
        }
    }

    /// The result of the newest command if it saved this part.
    fn own_result<'a>(&self, state: &'a UiState) -> Option<&'a Result<(), SaveError>> {
        match &state.part_result {
            Some((part, result)) if *part == self.part => Some(result),
            _ => None,
        }
    }

    /// Takes a new state. Returns the server's value if the card must show it: for another profile,
    /// before the first load, after a successful save of this part, or while the owner changed nothing.
    /// `changed` is true while the card differs from `base`.
    pub fn receive(&mut self, state: &UiState, server: Option<&T>, changed: bool) -> Option<T> {
        let new = state.part_saves != self.saves_seen;
        self.saves_seen = state.part_saves;
        let result = self.own_result(state).filter(|_| new);
        let saved = result == Some(&Ok(()));
        if result == Some(&Err(SaveError::Conflict)) {
            self.conflict = true;
        }
        if self.profile != state.selected {
            self.profile = state.selected.clone();
            self.base = None;
            self.message = None;
            self.conflict = false;
        }
        let server = server?;
        if saved || self.base.is_none() || (!changed && self.base.as_ref() != Some(server)) {
            self.load(server);
            return Some(server.clone());
        }
        None
    }

    /// Takes `server` as the new base. The card shows it.
    pub fn load(&mut self, server: &T) {
        self.base = Some(server.clone());
        self.conflict = false;
    }

    /// The profile and the expectation of a save. `None` until the card has loaded the part.
    /// With `anyway`, the save does not check what the server holds.
    pub fn save_target(&mut self, anyway: bool) -> Option<(String, Expected<T>)> {
        let (Some(profile), Some(base)) = (self.profile.clone(), self.base.clone()) else {
            return None;
        };
        self.conflict = false;
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(base)
        };
        Some((profile, expected))
    }

    /// The failure of the last save of this part, or "Saved" while nothing changed since.
    pub fn result_text(&self, state: &UiState, changed: bool) -> String {
        match self.own_result(state) {
            Some(Err(SaveError::Failed(error))) => error.clone(),
            Some(Ok(())) if !changed && self.message.is_none() => "Saved".to_owned(),
            _ => String::new(),
        }
    }
}

impl Shell {
    /// The message, the Save button, "Not saved" and the result of a card with the ID prefix `id`.
    pub(crate) fn save_bar<T: Clone + PartialEq>(
        &self,
        id: &'static str,
        track: &PartTrack<T>,
        changed: bool,
        save: fn(&mut Shell, bool, &mut Context<Shell>),
        cx: &Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .text_sm()
                    .text_color(palette(cx).stopped)
                    .child(labelled(format!("{id}-message"), track.message.unwrap_or_default())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new(SharedString::from(format!("save-{id}")))
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(move |this, _, _, cx| {
                                save(this, false, cx);
                                cx.notify();
                            })),
                    )
                    .child(div().text_sm().text_color(palette(cx).waiting).child(labelled(
                        format!("{id}-changes"),
                        if changed { "Not saved" } else { "" },
                    )))
                    .child(div().text_sm().child(labelled(
                        format!("{id}-result"),
                        track.result_text(&self.state, changed),
                    ))),
            )
    }
}
