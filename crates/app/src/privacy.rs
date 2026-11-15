//! The privacy section of the main window. The user edits a draft of the selected
//! profile's settings. Only Save sends the draft to the server.

use crate::bridge::{SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled};
use bunsin_api::{Expected, PrivacyUpdate};
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
const UNADDED: &str = "Add the pattern or clear the fields first";

/// The settings the user edits, and the server's settings they were made from.
struct Draft {
    profile: String,
    /// The server's settings when the form loaded them. A save expects the server to hold them still.
    base: PrivacyConfig,
    value: PrivacyConfig,
}

pub struct PrivacyForm {
    /// None until the server's settings arrive.
    draft: Option<Draft>,
    /// The value of `UiState::privacy_saves` in the last state the form received.
    saves_seen: u64,
    label: Entity<InputState>,
    pattern: Entity<InputState>,
    /// The window's own message about the last Add or Save.
    message: Option<&'static str>,
    /// True after the server refused a save because its settings changed since the form loaded them.
    conflict: bool,
}

impl PrivacyForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            draft: None,
            saves_seen: 0,
            label: cx.new(|cx| InputState::new(window, cx).placeholder("Label, for example project")),
            pattern: cx.new(|cx| InputState::new(window, cx).placeholder(r"Pattern, for example PRJ-\d{4}")),
            message: None,
            conflict: false,
        }
    }

    /// Takes a new state from the server. The draft stays if the user changed it. The form
    /// loads the server's settings if another profile is selected, if a save just succeeded,
    /// or if the user did not change the draft.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let new = state.privacy_saves != self.saves_seen;
        self.saves_seen = state.privacy_saves;
        let saved = new && state.privacy_result == Some(Ok(()));
        if new && state.privacy_result == Some(Err(SaveError::Conflict)) {
            self.conflict = true;
        }
        if self.draft.as_ref().map(|d| &d.profile) != state.selected.as_ref() {
            self.draft = None;
            self.message = None;
            self.conflict = false;
            self.clear_fields(window, cx);
        }
        let (Some(profile), Some(server)) = (&state.selected, &state.privacy) else {
            return;
        };
        match &self.draft {
            Some(draft) if !saved && (draft.value != draft.base || draft.base == *server) => {}
            _ => self.load(profile, server),
        }
    }

    /// Replaces the draft and its base with the server's settings.
    fn load(&mut self, profile: &str, server: &PrivacyConfig) {
        self.draft = Some(Draft {
            profile: profile.to_owned(),
            base: server.clone(),
            value: server.clone(),
        });
        self.conflict = false;
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
        let Some(draft) = &mut self.draft else {
            return;
        };
        if label.is_empty() || pattern.is_empty() {
            self.message = Some("Enter a label and a pattern.");
            return;
        }
        draft.value.custom.push(CustomPattern { label, pattern });
        self.message = None;
        self.clear_fields(window, cx);
    }

    /// Sets the message if the Add fields hold text. A save would not store that text.
    fn save_check(&mut self, cx: &App) {
        let label = self.label.read(cx).value();
        let pattern = self.pattern.read(cx).value();
        self.message = (!label.trim().is_empty() || !pattern.is_empty()).then_some(UNADDED);
    }
}

impl Shell {
    /// Sends the draft. The server refuses it if its settings are no longer the base,
    /// unless `anyway` is true.
    fn save_privacy(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.privacy;
        form.save_check(cx);
        let (None, Some(draft)) = (form.message, &form.draft) else {
            return;
        };
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(draft.base.clone())
        };
        let command = UiCommand::SavePrivacy {
            profile: draft.profile.clone(),
            update: PrivacyUpdate {
                privacy: draft.value.clone(),
                expected,
            },
        };
        form.conflict = false;
        self.send(command);
    }

    /// Drops the draft and shows the server's settings.
    fn load_privacy(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if let (Some(server), Some(draft)) = (&self.state.privacy, &self.privacy.draft) {
            let profile = draft.profile.clone();
            self.privacy.load(&profile, server);
        }
    }

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
        let (Some(server), Some(Draft { value: draft, .. })) = (&self.state.privacy, &form.draft) else {
            return section;
        };

        let toggle = |id: &'static str, text: &'static str, on: bool, set: fn(&mut PrivacyConfig, bool)| {
            Checkbox::new(id)
                .label(text)
                .checked(on)
                .on_change(cx.listener(move |this, on: &bool, _, cx| {
                    if let Some(draft) = &mut this.privacy.draft {
                        set(&mut draft.value, *on);
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
                            if let Some(draft) = &mut this.privacy.draft {
                                draft.value.custom.remove(i);
                                cx.notify();
                            }
                        })),
                )
        });

        // "Saved" describes the form only while it equals the server's settings
        // and no message says that something was not done.
        let changed = draft != server;
        let result = match &self.state.privacy_result {
            Some(Err(SaveError::Failed(error))) => error.clone(),
            Some(Ok(())) if !changed && form.message.is_none() => "Saved".to_owned(),
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
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_privacy(false, cx);
                        cx.notify();
                    })),
            )
            .child(labelled("privacy-changes", if changed { "Not saved" } else { "" }))
            .child(labelled("privacy-result", result))
            .children(form.conflict.then(|| {
                Self::conflict_warning(
                    "privacy",
                    CONFLICT,
                    Self::load_privacy,
                    |this, cx| this.save_privacy(true, cx),
                    cx,
                )
            }))
    }
}
