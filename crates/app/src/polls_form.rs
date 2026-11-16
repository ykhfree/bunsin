//! The polls of the agent view: the searches the server repeats to find new events.
//! The owner edits a draft of the list. Only Save sends it to the server.
//! Test runs a saved poll once and shows the events it would hand to the agent.

use crate::bridge::{PollResult, SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled, labelled_as};
use crate::theme::{card, code_block, field, palette, quiet, utc_minute};
use bunsin_api::{Expected, PollPreview, PollStatus, PollsUpdate};
use bunsin_core::poll::{PollConfig, PollMapping};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    App, AppContext, Context, Div, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window, div,
    prelude::*, px,
};
use serde_json::{Map, Value};

const HELP: &str = "The server calls each search tool on a timer and hands new items to the agent. \
    The tool needs a Read rule and must be in the approved list. The mapping says where the ID, the text and the \
    author of an item are in the tool's result, as dot paths such as user.id.";
const TEST_HELP: &str = "Test runs the saved poll once. It stores nothing and handles no event.";
const SAVE_FIRST: &str = "Save the list first. Test runs the saved polls.";
const UNADDED: &str = "Add the poll or clear its fields first.";
const NAMES: &str = "Enter the connector and the tool.";
const SECONDS: &str = "Enter the interval as a whole number of seconds.";
const UNCHANGED: &str = "Nothing changed, so nothing was saved.";
const DEFAULT_SECS: &str = "60";

/// The list the owner edits, and the server's list it was made from.
struct Draft {
    profile: String,
    /// The server's list when the form loaded it. A save expects the server to hold it still.
    base: Vec<PollConfig>,
    value: Vec<PollConfig>,
}

/// The fields of one poll.
struct Fields {
    connector: Entity<InputState>,
    tool: Entity<InputState>,
    every: Entity<InputState>,
    args: Entity<TextareaState>,
    items: Entity<InputState>,
    id: Entity<InputState>,
    text: Entity<InputState>,
    author: Entity<InputState>,
    thread: Entity<InputState>,
    /// One account name or ID in each line.
    skip: Entity<TextareaState>,
}

pub struct PollsForm {
    /// None until the server's list arrives.
    draft: Option<Draft>,
    /// The value of `UiState::poll_commands` in the last state the form received.
    commands_seen: u64,
    fields: Fields,
    /// The position of the poll whose values are in the fields.
    editing: Option<usize>,
    /// The window's own message about the last Add, Test or Save that it did not do.
    message: Option<String>,
    /// True after the server refused a save because its list changed since the form loaded it.
    conflict: bool,
}

fn value(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().trim().to_owned()
}

fn area(area: &Entity<TextareaState>, cx: &App) -> String {
    area.read(cx).value().to_string()
}

/// The arguments of a poll from the text of the field. An empty field is no argument.
fn parse_args(text: &str) -> Result<Map<String, Value>, String> {
    if text.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str(text) {
        Ok(Value::Object(args)) => Ok(args),
        Ok(_) => Err("The arguments must be a JSON object, for example {\"query\": \"to:me\"}.".to_owned()),
        Err(error) => Err(format!("The arguments are not valid JSON: {error}.")),
    }
}

/// One line of the list for a poll.
fn poll_line(poll: &PollConfig) -> String {
    format!("{} every {} s", poll.tool_id(), poll.every_secs)
}

fn mapping_line(mapping: &PollMapping) -> String {
    let items = if mapping.items.is_empty() {
        "(the result)"
    } else {
        &mapping.items
    };
    let mut line = format!(
        "items: {items}, id: {}, text: {}, author: {}",
        mapping.id, mapping.text, mapping.author
    );
    if let Some(thread) = &mapping.thread {
        line.push_str(&format!(", thread: {thread}"));
    }
    line
}

/// What the last run of a poll did.
pub fn status_text(status: &PollStatus) -> String {
    let Some(at) = status.last_run_at else {
        return "Not run yet".to_owned();
    };
    let when = format!("Last run {} UTC", utc_minute(at));
    match &status.error {
        Some(error) => format!("{when}: failed: {error}"),
        None if status.items > 0 && status.usable == 0 => format!(
            "{when}: {} items, none usable: the mapping does not fit the result",
            status.items
        ),
        None => format!(
            "{when}: {} items, {} usable, {} new events",
            status.items, status.usable, status.new_events
        ),
    }
}

/// What a test found: the counts, then one line per event.
pub fn preview_text(preview: &PollPreview) -> String {
    let mut lines = vec![format!(
        "Test: {} items in the result, {} events for the agent",
        preview.items,
        preview.events.len()
    )];
    lines.extend(preview.events.iter().map(|event| {
        let thread = event
            .thread
            .as_deref()
            .map(|t| format!(", thread {t}"))
            .unwrap_or_default();
        format!("- {} (ID {}{thread}): {}", event.author, event.external_id, event.text)
    }));
    lines.join("\n")
}

impl PollsForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        let every = input("Seconds, 30 at least", window, cx);
        every.update(cx, |every, cx| every.set_value(DEFAULT_SECS, window, cx));
        Self {
            draft: None,
            commands_seen: 0,
            fields: Fields {
                connector: input("slack", window, cx),
                tool: input("search_messages", window, cx),
                every,
                args: cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(2, 8)
                        .placeholder("{\"query\": \"to:me\"}")
                }),
                items: input("messages (empty if the result is the list)", window, cx),
                id: input("ts", window, cx),
                text: input("text", window, cx),
                author: input("user", window, cx),
                thread: input("thread_ts (optional)", window, cx),
                skip: cx.new(|cx| {
                    TextareaState::new(window, cx)
                        .auto_grow(2, 6)
                        .placeholder("Your own account names or IDs, one in each line")
                }),
            },
            editing: None,
            message: None,
            conflict: false,
        }
    }

    /// Takes a new state from the server. A changed draft stays. The form loads the server's list
    /// for another profile, after a successful save, or if the owner did not change the draft.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let new = state.poll_commands != self.commands_seen;
        self.commands_seen = state.poll_commands;
        let saved = new && state.poll_result == Some(PollResult::Saved(Ok(())));
        if new && state.poll_result == Some(PollResult::Saved(Err(SaveError::Conflict))) {
            self.conflict = true;
        }
        if self.draft.as_ref().map(|d| &d.profile) != state.selected.as_ref() {
            self.draft = None;
            self.message = None;
            self.conflict = false;
            self.fill(None, window, cx);
        }
        let (Some(profile), Some(server)) = (&state.selected, &state.polls) else {
            return;
        };
        match &self.draft {
            Some(draft) if !saved && (draft.value != draft.base || draft.base == *server) => {}
            _ => self.load(profile, server),
        }
    }

    fn load(&mut self, profile: &str, server: &[PollConfig]) {
        self.draft = Some(Draft {
            profile: profile.to_owned(),
            base: server.to_vec(),
            value: server.to_vec(),
        });
        self.editing = None;
        self.conflict = false;
    }

    /// Shows `poll` in the fields, or empties them for a new poll.
    fn fill(&mut self, poll: Option<&PollConfig>, window: &mut Window, cx: &mut App) {
        let f = &self.fields;
        let text = |get: fn(&PollConfig) -> String| poll.map(get).unwrap_or_default();
        let inputs = [
            (&f.connector, text(|p| p.connector.clone())),
            (&f.tool, text(|p| p.tool.clone())),
            (
                &f.every,
                poll.map_or(DEFAULT_SECS.to_owned(), |p| p.every_secs.to_string()),
            ),
            (&f.items, text(|p| p.mapping.items.clone())),
            (&f.id, text(|p| p.mapping.id.clone())),
            (&f.text, text(|p| p.mapping.text.clone())),
            (&f.author, text(|p| p.mapping.author.clone())),
            (&f.thread, text(|p| p.mapping.thread.clone().unwrap_or_default())),
        ];
        for (input, value) in inputs {
            input.update(cx, |input, cx| input.set_value(value, window, cx));
        }
        let args = poll
            .filter(|p| !p.args.is_empty())
            .map(|p| serde_json::to_string_pretty(&p.args).expect("a JSON map always serializes"))
            .unwrap_or_default();
        f.args.update(cx, |area, cx| area.set_value(args, window, cx));
        let skip = text(|p| p.skip_authors.join("\n"));
        f.skip.update(cx, |area, cx| area.set_value(skip, window, cx));
        self.editing = None;
    }

    /// True if a field holds text that Add has not taken.
    fn unadded(&self, cx: &App) -> bool {
        let f = &self.fields;
        [&f.connector, &f.tool, &f.items, &f.id, &f.text, &f.author, &f.thread]
            .into_iter()
            .any(|input| !value(input, cx).is_empty())
            || !area(&f.args, cx).trim().is_empty()
            || !area(&f.skip, cx).trim().is_empty()
    }

    /// The poll in the fields. The server checks the interval and the mapping on Save.
    fn poll(&self, cx: &App) -> Result<PollConfig, String> {
        let f = &self.fields;
        let (connector, tool) = (value(&f.connector, cx), value(&f.tool, cx));
        if connector.is_empty() || tool.is_empty() {
            return Err(NAMES.to_owned());
        }
        let every_secs = value(&f.every, cx).parse().map_err(|_| SECONDS.to_owned())?;
        let thread = Some(value(&f.thread, cx)).filter(|thread| !thread.is_empty());
        Ok(PollConfig {
            connector,
            tool,
            args: parse_args(&area(&f.args, cx))?,
            every_secs,
            mapping: PollMapping {
                items: value(&f.items, cx),
                id: value(&f.id, cx),
                text: value(&f.text, cx),
                author: value(&f.author, cx),
                thread,
            },
            skip_authors: area(&f.skip, cx)
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect(),
        })
    }

    /// Adds the poll of the fields, or puts it in place of the edited one.
    fn add(&mut self, window: &mut Window, cx: &mut App) {
        let poll = match self.poll(cx) {
            Ok(poll) => poll,
            Err(message) => {
                self.message = Some(message);
                return;
            }
        };
        let Some(draft) = &mut self.draft else {
            return;
        };
        match self.editing {
            Some(at) => draft.value[at] = poll,
            None => draft.value.push(poll),
        }
        self.message = None;
        self.fill(None, window, cx);
    }
}

impl Shell {
    /// Sends the draft. The server refuses it if its list is no longer the base, unless `anyway` is true.
    fn save_polls(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.polls;
        if form.unadded(cx) {
            form.message = Some(UNADDED.to_owned());
            return;
        }
        let Some(draft) = &form.draft else {
            return;
        };
        if draft.value == draft.base && !anyway {
            form.message = Some(UNCHANGED.to_owned());
            return;
        }
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(draft.base.clone())
        };
        let command = UiCommand::SavePolls {
            profile: draft.profile.clone(),
            update: PollsUpdate {
                polls: draft.value.clone(),
                expected,
            },
        };
        form.message = None;
        form.conflict = false;
        self.send(command);
    }

    /// Drops the draft and shows the server's list.
    fn load_polls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let (Some(server), Some(draft)) = (&self.state.polls, &self.polls.draft) {
            let (profile, server) = (draft.profile.clone(), server.clone());
            self.polls.load(&profile, &server);
            self.polls.fill(None, window, cx);
        }
    }

    /// Runs saved poll `at` once. A changed draft is not tested: the server has other polls.
    fn test_poll(&mut self, at: usize) {
        let Some(draft) = &self.polls.draft else {
            return;
        };
        if draft.value != draft.base {
            self.polls.message = Some(SAVE_FIRST.to_owned());
            return;
        }
        let profile = draft.profile.clone();
        self.polls.message = None;
        self.send(UiCommand::TestPoll { profile, index: at });
    }

    /// One poll with its last run, its controls and the result of its test.
    fn poll_row(&self, at: usize, poll: &PollConfig, cx: &Context<Self>) -> Div {
        let form = &self.polls;
        let saved = form
            .draft
            .as_ref()
            .is_some_and(|d| d.base == d.value && self.state.polls.as_ref() == Some(&d.value));
        // The status of the server's poll at this place, if the draft shows that poll.
        let status = saved
            .then(|| self.state.snapshot.as_ref()?.polls.get(at).map(status_text))
            .flatten();
        let test = match &self.state.poll_result {
            Some(PollResult::Tested { index, result }) if *index == at && saved => Some(match result {
                Ok(preview) => preview_text(preview),
                Err(error) => format!("Test failed: {error}"),
            }),
            _ => None,
        };
        let small = |id: String, label: &'static str| Button::new(SharedString::from(id)).small().ghost().label(label);
        let buttons = form.editing.is_none().then(|| {
            div()
                .flex()
                .gap_1()
                .child(
                    small(format!("poll-test-{at}"), "Test").on_click(cx.listener(move |this, _, _, cx| {
                        this.test_poll(at);
                        cx.notify();
                    })),
                )
                .child(
                    small(format!("poll-edit-{at}"), "Edit").on_click(cx.listener(move |this, _, window, cx| {
                        let poll = this.polls.draft.as_ref().map(|d| d.value[at].clone());
                        this.polls.fill(poll.as_ref(), window, cx);
                        this.polls.editing = Some(at);
                        this.polls.message = None;
                        cx.notify();
                    })),
                )
                .child(
                    small(format!("poll-remove-{at}"), "Remove").on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(draft) = &mut this.polls.draft {
                            draft.value.remove(at);
                        }
                        cx.notify();
                    })),
                )
        });
        let args = Value::Object(poll.args.clone()).to_string();
        let skip = if poll.skip_authors.is_empty() {
            "skips no author".to_owned()
        } else {
            format!("skips: {}", poll.skip_authors.join(", "))
        };
        let mono = cx.theme().mono_font_family.clone();
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .when(form.editing == Some(at), |row| row.bg(cx.theme().muted))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(labelled_as(
                        format!("poll-{at}"),
                        poll_line(poll),
                        div()
                            .font_weight(FontWeight::MEDIUM)
                            .font_family(mono.clone())
                            .child(SharedString::from(poll_line(poll))),
                    ))
                    .children(buttons),
            )
            .child(labelled_as(
                format!("poll-args-{at}"),
                args.clone(),
                quiet(args, cx).font_family(mono.clone()).text_xs(),
            ))
            .child(labelled_as(
                format!("poll-mapping-{at}"),
                mapping_line(&poll.mapping),
                quiet(mapping_line(&poll.mapping), cx),
            ))
            .child(labelled_as(format!("poll-skip-{at}"), skip.clone(), quiet(skip, cx)))
            .children(status.map(|status| {
                let color = if status.contains("failed") || status.contains("none usable") {
                    palette(cx).stopped
                } else {
                    cx.theme().muted_foreground
                };
                div()
                    .text_sm()
                    .text_color(color)
                    .child(labelled(format!("poll-status-{at}"), status))
            }))
            .children(test.map(|test| {
                code_block(cx)
                    .text_sm()
                    .child(labelled(format!("poll-test-result-{at}"), test))
            }))
    }

    /// The polls card. It shows the list only while the server's list is known.
    pub(crate) fn polls_section(&self, cx: &Context<Self>) -> impl IntoElement {
        let section = card(cx)
            .id("polls")
            .w(px(520.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Polls"))
            .child(labelled_as("polls-help", HELP, quiet(HELP, cx)));
        let form = &self.polls;
        let (Some(server), Some(draft)) = (&self.state.polls, &form.draft) else {
            return section;
        };
        let count = draft.value.len();
        let rows = draft
            .value
            .iter()
            .enumerate()
            .map(|(at, poll)| self.poll_row(at, poll, cx));
        let changed = draft.value != *server;
        let result = match &self.state.poll_result {
            Some(PollResult::Saved(Err(SaveError::Failed(error)))) => error.clone(),
            Some(PollResult::Saved(Ok(()))) if !changed && form.message.is_none() => "Saved".to_owned(),
            _ => String::new(),
        };
        let add_label = match form.editing {
            Some(at) => format!("Change poll {}", at + 1),
            None => "Add the poll".to_owned(),
        };
        let f = &form.fields;
        let input = |name: &'static str, id: &'static str, label: &'static str, state: &Entity<InputState>| {
            field(name, Input::new(state).id(id).aria_label(label), cx).flex_1()
        };
        section
            .child(labelled_as(
                "polls-count",
                format!("Polls: {count}"),
                quiet(
                    match count {
                        0 => "None yet. Without a poll, events come only from `bunsind ingest`.".to_owned(),
                        1 => "1 poll".to_owned(),
                        count => format!("{count} polls"),
                    },
                    cx,
                ),
            ))
            .child(labelled_as("polls-test-help", TEST_HELP, quiet(TEST_HELP, cx)))
            .child(div().flex().flex_col().gap_2().children(rows))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(input("Connector", "poll-connector", "Connector", &f.connector))
                    .child(input("Tool", "poll-tool", "Tool", &f.tool))
                    .child(input("Every (seconds)", "poll-every", "Interval in seconds", &f.every)),
            )
            // The text areas have no ID of their own. Tests find them inside these elements.
            .child(field(
                "Arguments of the tool, as a JSON object",
                div()
                    .id("poll-args")
                    .child(Textarea::new(&f.args).aria_label("Arguments")),
                cx,
            ))
            .child(quiet("Where the fields of an item are in the result", cx))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(input("List of items", "poll-items", "Path of the items", &f.items))
                    .child(input("ID", "poll-id", "Path of the ID", &f.id)),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(input("Text", "poll-text", "Path of the text", &f.text))
                    .child(input("Author", "poll-author", "Path of the author", &f.author))
                    .child(input("Thread", "poll-thread", "Path of the thread", &f.thread)),
            )
            .child(field(
                "Authors to skip: your own accounts, one in each line",
                div()
                    .id("poll-skip")
                    .child(Textarea::new(&f.skip).aria_label("Authors to skip")),
                cx,
            ))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(Button::new("add-poll").outline().label(add_label).on_click(cx.listener(
                        |this, _, window, cx| {
                            this.polls.add(window, cx);
                            cx.notify();
                        },
                    )))
                    .children(form.editing.map(|_| {
                        Button::new("cancel-poll-edit")
                            .ghost()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.polls.fill(None, window, cx);
                                cx.notify();
                            }))
                    })),
            )
            .child(
                div()
                    .text_color(palette(cx).stopped)
                    .text_sm()
                    .child(labelled("polls-message", form.message.clone().unwrap_or_default())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("save-polls")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_polls(false, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(palette(cx).waiting)
                            .child(labelled("polls-changes", if changed { "Not saved" } else { "" })),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(palette(cx).stopped)
                    .child(labelled("polls-result", result)),
            )
            .children(form.conflict.then(|| {
                Self::conflict_warning(
                    "polls",
                    CONFLICT,
                    Self::load_polls,
                    |this, cx| this.save_polls(true, cx),
                    cx,
                )
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunsin_core::event::InboundEvent;

    #[test]
    fn the_arguments_are_a_json_object_or_nothing() {
        assert_eq!(parse_args("  \n"), Ok(Map::new()));
        let args = parse_args("{\"query\": \"to:me\", \"count\": 20}").unwrap();
        assert_eq!(args["query"], "to:me");
        assert_eq!(args["count"], 20);
        assert!(parse_args("[1, 2]").unwrap_err().contains("must be a JSON object"));
        assert!(
            parse_args("{query: x}")
                .unwrap_err()
                .starts_with("The arguments are not valid JSON")
        );
    }

    #[test]
    fn the_status_says_when_the_mapping_does_not_fit() {
        assert_eq!(status_text(&PollStatus::default()), "Not run yet");
        let run = |items, usable, error: Option<&str>| PollStatus {
            tool: "slack/search".into(),
            last_run_at: Some(1_794_821_400),
            items,
            usable,
            new_events: 1,
            error: error.map(str::to_owned),
        };
        assert_eq!(
            status_text(&run(5, 4, None)),
            "Last run 2026-11-16 09:30 UTC: 5 items, 4 usable, 1 new events"
        );
        assert!(status_text(&run(5, 0, None)).ends_with("none usable: the mapping does not fit the result"));
        // No items is not a mapping fault.
        assert!(status_text(&run(0, 0, None)).ends_with("0 items, 0 usable, 1 new events"));
        assert!(status_text(&run(0, 0, Some("timeout"))).ends_with(": failed: timeout"));
    }

    #[test]
    fn a_preview_lists_each_event_with_its_author_and_id() {
        let event = |id: &str, thread: Option<&str>| InboundEvent {
            source: "slack".into(),
            external_id: id.into(),
            thread: thread.map(str::to_owned),
            author: "lee".into(),
            text: "When do we deploy?".into(),
            channel: Default::default(),
        };
        let preview = PollPreview {
            items: 3,
            events: vec![event("1.1", None), event("1.2", Some("1.0"))],
        };
        assert_eq!(
            preview_text(&preview),
            "Test: 3 items in the result, 2 events for the agent\n\
             - lee (ID 1.1): When do we deploy?\n\
             - lee (ID 1.2, thread 1.0): When do we deploy?"
        );
    }
}
