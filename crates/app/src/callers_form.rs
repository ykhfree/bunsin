//! The callers section of the agent view: the caller tokens that let other people's agents
//! ask this agent through the inbound endpoint.

use crate::bridge::UiCommand;
use crate::shell::{Shell, labelled, labelled_as};
use crate::theme::{card, code_block, field, notice, palette, quiet, utc_minute};
use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{
    App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::*, px,
};

const HELP: &str = "A caller token lets one outside caller ask your agent through its inbound endpoint. \
    It opens nothing else. Every answer waits for your approval in the inbox.";
const SHOWN_ONCE: &str = "Copy this token now. The server keeps only its hash and cannot show it again.";

pub struct CallersForm {
    label: Entity<InputState>,
    /// The window's own message about the last Create.
    message: Option<&'static str>,
    /// The token whose Revoke button was clicked once. A second click revokes it.
    revoke_armed: Option<i64>,
}

impl CallersForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        Self {
            label: cx.new(|cx| InputState::new(window, cx).placeholder("Who gets it, for example Team Lee's bot")),
            message: None,
            revoke_armed: None,
        }
    }
}

impl Shell {
    fn add_caller(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let label = self.callers.label.read(cx).value().trim().to_owned();
        let Some(profile) = self.state.selected.clone() else {
            return;
        };
        if label.is_empty() {
            self.callers.message = Some("Enter who gets the token.");
            return;
        }
        self.callers.message = None;
        self.callers
            .label
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.send(UiCommand::AddCaller { profile, label });
    }

    /// The callers section. It lists the tokens only while the server's list is known.
    pub(crate) fn callers_section(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let cx = &*cx;
        let section = card(cx)
            .id("callers")
            .w(px(420.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Callers"))
            .child(labelled_as("callers-help", HELP, quiet(HELP, cx)));
        let (Some(profile), Some(callers)) = (&self.state.selected, &self.state.callers) else {
            return section;
        };
        let form = &self.callers;
        let rows = callers.iter().map(|caller| {
            let id = caller.id;
            let state = match caller.revoked_at {
                Some(at) => format!("revoked {}", utc_minute(at)),
                None => format!("since {}", utc_minute(caller.created_at)),
            };
            let line = format!("#{id} {} · {state}", caller.label);
            let revoke = caller.revoked_at.is_none().then(|| {
                let armed = form.revoke_armed == Some(id);
                let profile = profile.clone();
                let button = Button::new(SharedString::from(format!("revoke-caller-{id}")))
                    .small()
                    .label(if armed { "Click again to revoke" } else { "Revoke" })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.callers.revoke_armed == Some(id) {
                            this.callers.revoke_armed = None;
                            this.send(UiCommand::RevokeCaller {
                                profile: profile.clone(),
                                id,
                            });
                        } else {
                            this.callers.revoke_armed = Some(id);
                        }
                        cx.notify();
                    }));
                if armed { button.danger() } else { button.ghost() }
            });
            div()
                .flex()
                .items_center()
                .gap_3()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .child(labelled(format!("caller-{id}"), line)),
                )
                .children(revoke)
        });
        // The token is drawn, not labelled: accessibility labels must never hold a secret.
        let created = self.state.new_caller.as_ref().map(|created| {
            notice(palette(cx).waiting, cx)
                .flex_col()
                .items_start()
                .child(labelled("caller-shown-once", SHOWN_ONCE))
                .child(code_block(cx).child(SharedString::from(created.token.clone())))
        });
        section
            .child(labelled_as(
                "caller-count",
                format!("Caller tokens: {}", callers.len()),
                div().font_weight(FontWeight::MEDIUM).child(match callers.len() {
                    0 => "Caller tokens: none".to_owned(),
                    count => format!("Caller tokens: {count}"),
                }),
            ))
            .children(rows)
            .children(created)
            .child(field(
                "Who gets the token",
                Input::new(&form.label).id("new-caller").aria_label("Caller label"),
                cx,
            ))
            .child(
                Button::new("add-caller")
                    .self_start()
                    .outline()
                    .label("Create a caller token")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.add_caller(window, cx);
                        cx.notify();
                    })),
            )
            .child(
                div()
                    .text_color(palette(cx).stopped)
                    .text_sm()
                    .child(labelled("callers-message", form.message.unwrap_or_default())),
            )
    }
}
