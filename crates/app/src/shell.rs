//! The main window: profiles, kill switch, approval inbox, audit log, privacy settings and connections.

use crate::bridge::{UiCommand, UiState};
use crate::connections_form::ConnectionsForm;
use crate::privacy::PrivacyForm;
use bunsin_core::approval::ApprovalState;
use bunsin_core::records::{InboxBody, InboxItem};
use gpui_kit::base::TestSupportExt;
use gpui_kit::component::button::Button;
use gpui_kit::{Context, Div, IntoElement, ParentElement, Render, SharedString, Styled, Window, div, prelude::*, px};
use tokio::sync::mpsc;

const AUDIT_ROWS: usize = 50;

pub struct Shell {
    pub state: UiState,
    commands: mpsc::UnboundedSender<UiCommand>,
    pub(crate) privacy: PrivacyForm,
    pub(crate) connections: ConnectionsForm,
    /// False: the inbox and the audit log are shown. True: the settings forms are shown.
    /// The two never share the window, so the forms cannot squeeze the inbox.
    pub settings_open: bool,
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
            settings_open: false,
        }
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
        load: fn(&mut Self, &mut Window, &mut Context<Self>),
        save_anyway: fn(&mut Self, &mut Context<Self>),
        cx: &Context<Self>,
    ) -> Div {
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(labelled(format!("{id}-conflict"), CONFLICT))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        Button::new(SharedString::from(format!("{id}-load")))
                            .label("Load the server's settings")
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
    div().id(id.into()).test_support().aria_label(text.clone()).child(text)
}

impl Render for Shell {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The forms are built first. The lists below borrow `cx` until they are drawn.
        let forms = self.settings_open.then(|| {
            (
                self.connections_section(cx).into_any_element(),
                self.privacy_section(cx).into_any_element(),
            )
        });
        let snapshot = self.state.snapshot.as_ref();
        let stopped = snapshot.is_some_and(|s| s.kill_switch);
        let status = match snapshot {
            None if !self.state.connected => "Not connected to the server",
            None => "No profile",
            Some(_) if stopped => "Stopped: the agent makes no changes",
            Some(_) => "Running",
        };

        let profiles = self.state.profiles.iter().map(|profile| {
            let id = profile.id.clone();
            let mark = if self.state.selected.as_deref() == Some(id.as_str()) {
                "● "
            } else {
                ""
            };
            let text = SharedString::from(format!("{mark}{} ({})", profile.display_name, profile.id));
            div()
                .id(SharedString::from(format!("profile-{id}")))
                .test_support()
                .aria_label(text.clone())
                .cursor_pointer()
                .child(text)
                .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::SelectProfile(id.clone()))))
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
                    .child(match item.body {
                        // A long page scrolls in its own box, so its buttons stay close.
                        InboxBody::WikiChange { .. } => div()
                            .id(SharedString::from(format!("detail-box-{id}")))
                            .test_support()
                            .max_h(px(220.))
                            .overflow_y_scroll()
                            .child(labelled(format!("detail-{id}"), inbox_detail(item)))
                            .into_any_element(),
                        // An action shows every argument at full height. None is hidden in a box.
                        _ => labelled(format!("detail-{id}"), inbox_detail(item)).into_any_element(),
                    })
                    .child(div().child(SharedString::from(format!("Why: {}", item.rationale))))
                    .children(inbox_basis(item).map(|basis| labelled(format!("basis-{id}"), basis)));
                let mut buttons = div().flex().gap_2();
                if item.state == ApprovalState::Pending {
                    buttons = buttons.child(
                        Button::new(SharedString::from(format!("approve-{id}")))
                            // A note is stored. Nothing runs on another system.
                            .label(match item.body {
                                InboxBody::WikiChange { .. } => "Approve and keep",
                                _ => "Approve and run",
                            })
                            .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::Approve(id)))),
                    );
                }
                if matches!(item.state, ApprovalState::Pending | ApprovalState::Draft) {
                    buttons = buttons.child(
                        Button::new(SharedString::from(format!("reject-{id}")))
                            .label("Dismiss")
                            .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::Reject(id)))),
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

        let page = if let Some((connections, privacy)) = forms {
            div()
                .id("settings")
                .flex_1()
                .min_w(px(0.))
                .flex()
                .flex_wrap()
                .gap_4()
                .overflow_y_scroll()
                .child(connections)
                .child(privacy)
                .into_any_element()
        } else {
            div()
                .flex_1()
                // Long lines must not push the settings columns out of the window.
                .min_w(px(0.))
                .flex()
                .flex_col()
                .gap_3()
                .child(
                    div().flex().gap_3().child(labelled("status", status)).child(
                        Button::new("kill-switch")
                            .label(if stopped { "Resume changes" } else { "Stop all changes" })
                            .on_click(cx.listener(move |this, _, _, _| this.send(UiCommand::SetKillSwitch(!stopped)))),
                    ),
                )
                .child(labelled("error", self.state.error.clone().unwrap_or_default()))
                .child(labelled("inbox-count", format!("Approval inbox: {}", inbox.len())))
                .child(
                    div()
                        .id("inbox")
                        // The inbox gets the height that the audit log does not need.
                        .flex_1()
                        .min_h(px(0.))
                        .flex()
                        .flex_col()
                        .gap_2()
                        .overflow_y_scroll()
                        .children(inbox),
                )
                .child(labelled("audit-count", format!("Audit log: {} entries", audit.len())))
                .child(
                    div()
                        .id("audit")
                        // A short log at the bottom. The approval items are what the owner acts on.
                        .max_h(px(160.))
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .children(audit_rows),
                )
                .into_any_element()
        };

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
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(
                                Button::new("tab-inbox")
                                    .label("Inbox")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.settings_open = false;
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new("tab-settings").label("Settings").on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.settings_open = true;
                                    cx.notify();
                                },
                            ))),
                    )
                    .child("Profiles")
                    .children(profiles),
            )
            .child(page)
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
