//! The privacy section of the main window. The user edits a draft of the selected
//! profile's settings. Only Save sends the draft to the server.

use crate::bridge::{SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled, labelled_as};
use crate::theme::{card, field, palette, quiet};
use bunsin_api::{Expected, PrivacyUpdate};
use bunsin_core::privacy::{CustomPattern, PrivacyConfig};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::*, px,
};

const HELP: &str = "Matching text is removed before the agent sees it. \
    The filter finds patterns, such as email addresses and phone numbers. \
    It does not find names or postal addresses by itself: add them as terms. \
    Custom patterns use the syntax of the Rust regex crate.";
const TERMS_HELP: &str = "A term is removed wherever it is in the text, in upper or lower case. \
    \"Kim\" also removes the start of \"Kimball\". The terms are stored in the profile settings on the server.";
const UNADDED: &str = "Add the pattern or clear the fields first";
const UNADDED_TERM: &str = "Add the term or clear its field first";

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
    term: Entity<InputState>,
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
            term: cx.new(|cx| InputState::new(window, cx).placeholder("A name or an address")),
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
        for input in [&self.label, &self.pattern, &self.term] {
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

    /// Adds the typed term to the draft. The server checks its length on Save.
    fn add_term(&mut self, window: &mut Window, cx: &mut App) {
        let term = self.term.read(cx).value().trim().to_owned();
        let Some(draft) = &mut self.draft else {
            return;
        };
        if term.is_empty() {
            self.message = Some("Enter a term.");
            return;
        }
        draft.value.terms.push(term);
        self.message = None;
        self.term.update(cx, |input, cx| input.set_value("", window, cx));
    }

    /// Sets the message if the Add fields hold text. A save would not store that text.
    fn save_check(&mut self, cx: &App) {
        let label = self.label.read(cx).value();
        let pattern = self.pattern.read(cx).value();
        let term = self.term.read(cx).value();
        self.message = if !label.trim().is_empty() || !pattern.is_empty() {
            Some(UNADDED)
        } else if !term.trim().is_empty() {
            Some(UNADDED_TERM)
        } else {
            None
        };
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
        let cx = &*cx;
        let section = card(cx)
            .id("privacy")
            .w(px(420.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Privacy filter"))
            .child(labelled_as("privacy-help", HELP, quiet(HELP, cx)));
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
                .items_center()
                .gap_3()
                .child(labelled(format!("pattern-label-{i}"), custom.label.clone()))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .font_family(cx.theme().mono_font_family.clone())
                        .text_size(cx.theme().mono_font_size)
                        .child(labelled(format!("pattern-regex-{i}"), custom.pattern.clone())),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-pattern-{i}")))
                        .small()
                        .ghost()
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(draft) = &mut this.privacy.draft {
                                draft.value.custom.remove(i);
                                cx.notify();
                            }
                        })),
                )
        });

        let term_rows = draft.terms.iter().enumerate().map(|(i, term)| {
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(labelled(format!("term-{i}"), term.clone())),
                )
                .child(
                    Button::new(SharedString::from(format!("remove-term-{i}")))
                        .small()
                        .ghost()
                        .label("Remove")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if let Some(draft) = &mut this.privacy.draft {
                                draft.value.terms.remove(i);
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
            .child(labelled_as(
                "pattern-count",
                format!("Custom patterns: {}", draft.custom.len()),
                div()
                    .pt_2()
                    .font_weight(FontWeight::MEDIUM)
                    .child(match draft.custom.len() {
                        0 => "Your own patterns: none".to_owned(),
                        count => format!("Your own patterns: {count}"),
                    }),
            ))
            .children(rows)
            .child(field(
                "Name of the pattern",
                Input::new(&form.label).id("new-label").aria_label("Label"),
                cx,
            ))
            .child(field(
                "Pattern",
                Input::new(&form.pattern).id("new-pattern").aria_label("Pattern"),
                cx,
            ))
            .child(
                Button::new("add-pattern")
                    .self_start()
                    .outline()
                    .label("Add the pattern")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.privacy.add(window, cx);
                        cx.notify();
                    })),
            )
            .child(labelled_as(
                "term-count",
                format!("Terms: {}", draft.terms.len()),
                div()
                    .pt_2()
                    .font_weight(FontWeight::MEDIUM)
                    .child(match draft.terms.len() {
                        0 => "Names and addresses: none".to_owned(),
                        count => format!("Names and addresses: {count}"),
                    }),
            ))
            .child(labelled_as("terms-help", TERMS_HELP, quiet(TERMS_HELP, cx)))
            .children(term_rows)
            .child(field(
                "Term",
                Input::new(&form.term).id("new-term").aria_label("Term"),
                cx,
            ))
            .child(
                Button::new("add-term")
                    .self_start()
                    .outline()
                    .label("Add the term")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.privacy.add_term(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_color(palette(cx).stopped)
                    .text_sm()
                    .child(labelled("privacy-message", form.message.unwrap_or_default())),
            )
            .child(
                Button::new("save-privacy")
                    .self_start()
                    .primary()
                    .label("Save")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.save_privacy(false, cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(palette(cx).waiting)
                    .child(labelled("privacy-changes", if changed { "Not saved" } else { "" })),
            )
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
