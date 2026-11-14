//! The privacy section of the main window. The user edits a draft of the selected
//! profile's settings. Only Save sends the draft to the server.

use crate::bridge::{UiCommand, UiState};
use crate::shell::{Shell, labelled};
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use gpui_kit::component::button::Button;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{
    App, AppContext, Context, Entity, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px,
};

const HELP: &str = "Matching text is removed before the agent sees it. \
    The filter finds patterns, such as email addresses and phone numbers. \
    It does not find names or postal addresses. \
    Custom patterns use the syntax of the Rust regex crate.";

pub struct PrivacyForm {
    /// The profile and the settings the user edits. None until the server's settings arrive.
    draft: Option<(String, PrivacyConfig)>,
    /// The value of `UiState::privacy_saves` in the last state the form received.
    saves_seen: u64,
    label: Entity<InputState>,
    pattern: Entity<InputState>,
    /// The window's own message about the last Add.
    message: Option<&'static str>,
}

impl PrivacyForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            draft: None,
            saves_seen: 0,
            label: cx.new(|cx| InputState::new(window, cx).placeholder("Label, for example project")),
            pattern: cx.new(|cx| InputState::new(window, cx).placeholder(r"Pattern, for example PRJ-\d{4}")),
            message: None,
        }
    }

    /// Takes a new state from the server. The draft stays, unless another profile is
    /// selected or a save just succeeded. Then the form loads the server's settings.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let saved = state.privacy_saves != self.saves_seen && state.privacy_result == Some(Ok(()));
        self.saves_seen = state.privacy_saves;
        if self.draft.as_ref().map(|(profile, _)| profile) != state.selected.as_ref() {
            self.draft = None;
            self.message = None;
            self.clear_fields(window, cx);
        }
        if (self.draft.is_none() || saved)
            && let (Some(profile), Some(privacy)) = (&state.selected, &state.privacy)
        {
            self.draft = Some((profile.clone(), privacy.clone()));
        }
    }

    fn clear_fields(&self, window: &mut Window, cx: &mut App) {
        for input in [&self.label, &self.pattern] {
            input.update(cx, |input, cx| input.set_value("", window, cx));
        }
    }

    /// Adds the typed pattern to the draft. The server checks the pattern on Save.
    fn add(&mut self, window: &mut Window, cx: &mut App) {
        let label = self.label.read(cx).value().trim().to_owned();
        let pattern = self.pattern.read(cx).value().to_string();
        let Some((_, draft)) = &mut self.draft else {
            return;
        };
        if label.is_empty() || pattern.is_empty() {
            self.message = Some("Enter a label and a pattern.");
            return;
        }
        draft.custom.push(CustomPattern { label, pattern });
        self.message = None;
        self.clear_fields(window, cx);
    }
}

impl Shell {
    /// The privacy section. It shows the form only while the server's settings are known.
    pub(crate) fn privacy_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let section = div()
            .id("privacy")
            .w(px(360.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_2()
            .overflow_y_scroll()
            .child("Privacy")
            .child(labelled("privacy-help", HELP));
        let form = &self.privacy;
        let (Some(server), Some((_, draft))) = (&self.state.privacy, &form.draft) else {
            return section;
        };

        let toggle = |id: &'static str, text: &'static str, on: bool, set: fn(&mut PrivacyConfig, bool)| {
            Checkbox::new(id)
                .label(text)
                .checked(on)
                .on_change(cx.listener(move |this, on: &bool, _, cx| {
                    if let Some((_, draft)) = &mut this.privacy.draft {
                        set(draft, *on);
                        cx.notify();
                    }
                }))
        };

        let rows = draft.custom.iter().enumerate().map(|(i, custom)| {
            div()
                .flex()
                .gap_2()
                .child(labelled(format!("pattern-label-{i}"), custom.label.clone()))
                .child(labelled(format!("pattern-regex-{i}"), custom.pattern.clone()))
                .child(
                    Button::new(SharedString::from(format!("remove-pattern-{i}")))
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some((_, draft)) = &mut this.privacy.draft {
                                draft.custom.remove(i);
                                cx.notify();
                            }
                        })),
                )
        });

        // "Saved" describes the form only while it equals the server's settings.
        let changed = draft != server;
        let result = match &self.state.privacy_result {
            Some(Err(error)) => error.clone(),
            Some(Ok(())) if !changed => "Saved".to_owned(),
            _ => String::new(),
        };

        section
            .child(toggle("privacy-email", "Email addresses", draft.email, |c, on| {
                c.email = on
            }))
            .child(toggle("privacy-phone", "Phone numbers", draft.phone, |c, on| {
                c.phone = on
            }))
            .child(toggle(
                "privacy-resident-number",
                "Korean resident registration numbers",
                draft.resident_number,
                |c, on| c.resident_number = on,
            ))
            .child(toggle(
                "privacy-card-number",
                "Payment card numbers",
                draft.card_number,
                |c, on| c.card_number = on,
            ))
            .child(labelled(
                "pattern-count",
                format!("Custom patterns: {}", draft.custom.len()),
            ))
            .children(rows)
            .child(Input::new(&form.label).id("new-label").aria_label("Label"))
            .child(Input::new(&form.pattern).id("new-pattern").aria_label("Pattern"))
            .child(
                Button::new("add-pattern")
                    .label("Add")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.privacy.add(window, cx);
                        cx.notify();
                    })),
            )
            .child(labelled("privacy-message", form.message.unwrap_or_default()))
            .child(
                Button::new("save-privacy")
                    .label("Save")
                    .on_click(cx.listener(|this, _, _, _| {
                        if let Some((profile, draft)) = &this.privacy.draft {
                            this.send(UiCommand::SavePrivacy {
                                profile: profile.clone(),
                                privacy: draft.clone(),
                            });
                        }
                    })),
            )
            .child(labelled("privacy-changes", if changed { "Not saved" } else { "" }))
            .child(labelled("privacy-result", result))
    }
}
