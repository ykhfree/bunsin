//! The main window: profiles, kill switch, approval inbox and audit log.

use crate::bridge::{UiCommand, UiState};
use bunsin_core::approval::ApprovalState;
use bunsin_core::records::{InboxBody, InboxItem};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::button::Button;
use gpui_kit::{
    Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, prelude::*, px,
};
use tokio::sync::mpsc;

const AUDIT_ROWS: usize = 50;

pub struct Shell {
    pub state: UiState,
    commands: mpsc::UnboundedSender<UiCommand>,
}

impl Shell {
    pub fn new(
        mut states: mpsc::UnboundedReceiver<UiState>,
        commands: mpsc::UnboundedSender<UiCommand>,
        cx: &mut Context<Self>,
    ) -> Self {
        // tokio channels work on the GPUI executor. tokio timers do not, so none is used here.
        cx.spawn(async move |this, cx| {
            while let Some(state) = states.recv().await {
                let updated = this.update(cx, |shell, cx| {
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
        }
    }

    fn send(&self, command: UiCommand) {
        // A closed channel means the engine task ended. The window then shows stale state.
        let _ = self.commands.send(command);
    }
}

/// One line that describes an inbox item.
pub fn inbox_title(item: &InboxItem) -> String {
    let what = match &item.body {
        InboxBody::ToolCall { tool, .. } => tool.as_str(),
        InboxBody::Escalation { .. } => "needs your decision",
    };
    format!("{} · {what}", item.state.as_str())
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
    }
}

fn labelled(id: impl Into<SharedString>, text: impl Into<SharedString>) -> impl IntoElement {
    let text = text.into();
    div()
        .id(id.into())
        .test_support()
        .aria_label(text.clone())
        .child(text)
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = self.state.snapshot.as_ref();
        let stopped = snapshot.is_some_and(|s| s.kill_switch);
        let status = match snapshot {
            None => "No profile",
            Some(_) if stopped => "Stopped: the agent makes no changes",
            Some(_) => "Running",
        };

        let profiles =
            self.state.profiles.iter().map(|profile| {
                let id = profile.id.clone();
                let mark = if self.state.selected.as_deref() == Some(id.as_str()) {
                    "● "
                } else {
                    ""
                };
                let text =
                    SharedString::from(format!("{mark}{} ({})", profile.display_name, profile.id));
                div()
                    .id(SharedString::from(format!("profile-{id}")))
                    .test_support()
                    .aria_label(text.clone())
                    .cursor_pointer()
                    .child(text)
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.send(UiCommand::SelectProfile(id.clone()))
                    }))
            });

        let inbox = snapshot
            .map(|s| s.inbox.as_slice())
            .unwrap_or_default()
            .iter()
            .map(|item| {
                let id = item.id;
                let mut row = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .border_1()
                    .child(labelled(format!("inbox-{id}"), inbox_title(item)))
                    .child(labelled(format!("detail-{id}"), inbox_detail(item)))
                    .child(div().child(SharedString::from(format!("Why: {}", item.rationale))));
                let mut buttons = div().flex().gap_2();
                if item.state == ApprovalState::Pending {
                    buttons = buttons.child(
                        Button::new(SharedString::from(format!("approve-{id}")))
                            .label("Approve and run")
                            .on_click(
                                cx.listener(move |this, _, _, _| this.send(UiCommand::Approve(id))),
                            ),
                    );
                }
                if matches!(item.state, ApprovalState::Pending | ApprovalState::Draft) {
                    buttons = buttons.child(
                        Button::new(SharedString::from(format!("reject-{id}")))
                            .label("Dismiss")
                            .on_click(
                                cx.listener(move |this, _, _, _| this.send(UiCommand::Reject(id))),
                            ),
                    );
                    row = row.child(buttons);
                }
                row
            });

        let audit = snapshot.map(|s| s.audit.as_slice()).unwrap_or_default();
        let audit_rows = audit.iter().take(AUDIT_ROWS).map(|entry| {
            div().child(SharedString::from(format!(
                "{}  {}  {}",
                entry.at, entry.kind, entry.detail
            )))
        });

        div()
            .size_full()
            .flex()
            .gap_4()
            .p_4()
            .child(
                div()
                    .w(px(220.))
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child("Profiles")
                    .children(profiles),
            )
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .gap_3()
                            .child(labelled("status", status))
                            .child(
                                Button::new("kill-switch")
                                    .label(if stopped {
                                        "Resume changes"
                                    } else {
                                        "Stop all changes"
                                    })
                                    .on_click(cx.listener(move |this, _, _, _| {
                                        this.send(UiCommand::SetKillSwitch(!stopped))
                                    })),
                            ),
                    )
                    .child(labelled(
                        "error",
                        self.state.error.clone().unwrap_or_default(),
                    ))
                    .child(labelled(
                        "inbox-count",
                        format!("Approval inbox: {}", inbox.len()),
                    ))
                    .child(
                        div()
                            .id("inbox")
                            .flex()
                            .flex_col()
                            .gap_2()
                            .overflow_y_scroll()
                            .children(inbox),
                    )
                    .child(labelled(
                        "audit-count",
                        format!("Audit log: {} entries", audit.len()),
                    ))
                    .child(
                        div()
                            .id("audit")
                            .flex()
                            .flex_col()
                            .overflow_y_scroll()
                            .children(audit_rows),
                    ),
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
            state: ApprovalState::Pending,
            created_at: 0,
            updated_at: 0,
        }
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
    fn titles_name_the_state_and_the_tool() {
        let call = InboxBody::ToolCall {
            tool: "jira/add_comment".into(),
            args: Default::default(),
        };
        assert_eq!(inbox_title(&item(call)), "pending · jira/add_comment");
        let escalation = InboxBody::Escalation {
            summary: "Decide the budget.".into(),
        };
        assert_eq!(
            inbox_title(&item(escalation.clone())),
            "pending · needs your decision"
        );
        assert_eq!(inbox_detail(&item(escalation)), "Decide the budget.");
    }
}
