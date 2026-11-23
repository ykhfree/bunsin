//! The setup of an agent, in the order of PLAN section 5: LLM, connectors, tools and rules,
//! knowledge, principles, evaluation, finish.
//!
//! Each step shows the controls that the other views have for it, and it sends only their commands.
//! The wizard stores nothing of its own on the server. The window keeps the step of each profile
//! while it is open. A new window starts at the first step whose result the server does not show.
//! The wizard never offers Autonomous. It ends only when every write and delete rule is at
//! Observe or Approve.

use crate::bridge::{ConnectionResult, UiCommand, UiState};
use crate::learning_form::rates_text;
use crate::permissions_form::tier_word;
use crate::shell::{Shell, View, labelled, labelled_as};
use crate::theme::{card, field, notice, palette, quiet};
use bunsin_api::{Expected, LlmConnection, Permissions, PermissionsUpdate, ReplayCase};
use bunsin_core::candidates::{CandidateState, PrincipleSource};
use bunsin_core::policy::{Tier, ToolClass};
use bunsin_core::records::InboxBody;
use bunsin_core::tool::pin;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window,
    div, prelude::*, px,
};
use std::collections::{BTreeMap, BTreeSet};

/// The steps of the setup, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Llm,
    Connect,
    Tools,
    Knowledge,
    Principles,
    Evaluation,
    Finish,
}

impl Step {
    pub const ALL: [Step; 7] = [
        Step::Llm,
        Step::Connect,
        Step::Tools,
        Step::Knowledge,
        Step::Principles,
        Step::Evaluation,
        Step::Finish,
    ];

    /// The place of the step, from 1.
    pub fn number(self) -> usize {
        self as usize + 1
    }

    pub fn title(self) -> &'static str {
        match self {
            Step::Llm => "LLM endpoint",
            Step::Connect => "Connect services",
            Step::Tools => "Tools and rules",
            Step::Knowledge => "Knowledge",
            Step::Principles => "Principles",
            Step::Evaluation => "Evaluation",
            Step::Finish => "Finish",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Step::Llm => {
                "Enter the address, the model and the token of the LLM endpoint. Save, then press Test. \
                 Next needs a test that works."
            }
            Step::Connect => {
                "Connect the services that the agent works with: an MCP server at an address, a REST API, or a \
                 command that the server runs if its operator allows it. Save a connector, then press Check."
            }
            Step::Tools => {
                "List the tools of each connector and approve the list. Then give each tool a rule. No tool \
                 gets a class by itself: you choose Read, Write or Delete. A new Write or Delete rule starts at \
                 Approve, and you choose where its text for other people is. The setup offers no Autonomous step."
            }
            Step::Knowledge => {
                "Optional. Read once from a tool that has a read rule, for example a search of your team's pages. \
                 The agent proposes pages of its notes from the result. Each page change waits in the inbox."
            }
            Step::Principles => {
                "The LLM reads your texts as data and proposes rules that you follow. It can read the result of a \
                 tool that has a read rule, for example a search of your sent messages. It can also read your \
                 corrections or your replay cases, but a new profile has none yet. Adopt, edit or dismiss each \
                 candidate."
            }
            Step::Evaluation => {
                "If you have past messages with the reply that you really gave, add them as replay cases. Then run \
                 the first evaluation. The agent answers each message again without seeing your reply."
            }
            Step::Finish => {
                "The agent starts at Observe or Approve: it runs no write without you. Change the rules later in \
                 the Permissions view."
            }
        }
    }

    fn next(self) -> Option<Step> {
        Self::ALL.get(self as usize + 1).copied()
    }

    fn back(self) -> Option<Step> {
        (self as usize).checked_sub(1).map(|at| Self::ALL[at])
    }
}

/// The first step whose result the server does not show. A new window resumes the setup there.
pub fn first_open_step(state: &UiState) -> Step {
    let Some(connections) = &state.connections else {
        return Step::Llm;
    };
    if connections.llm.is_none() {
        return Step::Llm;
    }
    if connections.connectors.is_empty() {
        return Step::Connect;
    }
    let approved = |name: &String| {
        state
            .permissions
            .as_ref()
            .and_then(|p| p.tools.get(name))
            .is_some_and(|tools| !tools.is_empty())
    };
    let ruled = state.permissions.as_ref().is_some_and(|p| !p.rules.is_empty());
    if !connections.connectors.keys().all(approved) || !ruled {
        return Step::Tools;
    }
    if state.notes.as_ref().is_none_or(Vec::is_empty) {
        return Step::Knowledge;
    }
    let candidates = state
        .candidates
        .as_ref()
        .is_some_and(|c| c.iter().any(|c| c.state == CandidateState::New));
    if state.principles.as_ref().is_none_or(Vec::is_empty) && !candidates {
        return Step::Principles;
    }
    if state.eval_report.as_ref().is_none_or(|report| report.run.is_none()) {
        return Step::Evaluation;
    }
    Step::Finish
}

/// True for a profile that nobody has set up: it lacks the LLM endpoint or every connector,
/// and it has no approved tool. A profile with approved tools was set up in another way.
pub fn needs_setup(state: &UiState) -> bool {
    match (&state.connections, &state.permissions) {
        (Some(connections), Some(permissions)) => {
            (connections.llm.is_none() || connections.connectors.is_empty())
                && permissions.tools.values().all(Vec::is_empty)
        }
        _ => false,
    }
}

/// The write and delete rules that are not at Observe or Approve, with their step.
pub fn unready_rules(permissions: &Permissions) -> Vec<(String, Tier)> {
    permissions
        .rules
        .iter()
        .filter(|(_, rule)| rule.class != ToolClass::Read && !matches!(rule.tier, Tier::Observe | Tier::Approve))
        .map(|(tool, rule)| (tool.clone(), rule.tier))
        .collect()
}

/// Reads the text of the cases field: a JSON array of cases, as `bunsind add-replay-cases` reads it.
pub fn parse_cases(text: &str) -> Result<Vec<ReplayCase>, String> {
    let cases: Vec<ReplayCase> =
        serde_json::from_str(text.trim()).map_err(|e| format!("The cases are not a JSON array of cases: {e}"))?;
    if cases.is_empty() {
        return Err("Enter at least one case.".to_owned());
    }
    Ok(cases)
}

const CASES_EXAMPLE: &str = r#"[{"event": {"source": "slack", "external_id": "c1", "author": "lee",
  "text": "when do we deploy?"}, "actual_reply": "At 3 pm.", "expect": "reply"}]"#;
const TEST_FIRST: &str = "Save the endpoint and press Test. Next needs a test that works. Skip goes on without it.";
const NOT_READY: &str = "Each write and delete rule must be at Observe or Approve before the setup ends.";

pub struct SetupForm {
    /// The profile whose fields the form holds.
    profile: Option<String>,
    /// The step of each profile while this window is open.
    steps: BTreeMap<String, Step>,
    /// The profiles for which the window decided whether to open the setup by itself. It decides once for each.
    offered: BTreeSet<String>,
    /// The LLM endpoint of each profile that passed its last test in this window.
    passed: BTreeMap<String, LlmConnection>,
    /// The value of `UiState::connection_commands` in the last state the form received.
    commands_seen: u64,
    /// The value of `UiState::case_adds` in the last state the form received.
    adds_seen: u64,
    /// The tool and the arguments of the read that principle candidates come from.
    read_tool: Entity<InputState>,
    read_args: Entity<InputState>,
    /// The replay cases to add.
    cases: Entity<TextareaState>,
    /// The window's own message about the last action that it did not do.
    message: Option<&'static str>,
    /// Why the last read or case entry was not sent.
    input_message: Option<String>,
}

impl SetupForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            profile: None,
            steps: BTreeMap::new(),
            offered: BTreeSet::new(),
            passed: BTreeMap::new(),
            commands_seen: 0,
            adds_seen: 0,
            read_tool: input("slack/search", window, cx),
            read_args: input(r#"{"query": "from:me"}"#, window, cx),
            cases: cx.new(|cx| {
                TextareaState::new(window, cx)
                    .auto_grow(3, 8)
                    .placeholder(CASES_EXAMPLE)
            }),
            message: None,
            input_message: None,
        }
    }

    /// Takes a new state from the server. Returns true if the window should open the setup by itself:
    /// the first time it sees a profile that needs a setup.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) -> bool {
        let tested = state.connection_commands != self.commands_seen;
        self.commands_seen = state.connection_commands;
        if tested
            && let (Some(ConnectionResult::LlmTested(Ok(test))), Some(profile), Some(connections)) =
                (&state.connection_result, &state.selected, &state.connections)
            && let Some(llm) = connections.llm.as_ref().filter(|_| test.works)
        {
            self.passed.insert(profile.clone(), llm.clone());
        }
        let added = state.case_adds != self.adds_seen;
        self.adds_seen = state.case_adds;
        if added && matches!(state.cases_result, Some(Ok(_))) {
            self.cases.update(cx, |area, cx| area.set_value("", window, cx));
        }
        if self.profile != state.selected {
            // Typed text can name people. It does not stay for another profile.
            for input in [&self.read_tool, &self.read_args] {
                input.update(cx, |input, cx| input.set_value("", window, cx));
            }
            self.cases.update(cx, |area, cx| area.set_value("", window, cx));
            self.message = None;
            self.input_message = None;
            self.profile = state.selected.clone();
        }
        let Some(profile) = &state.selected else {
            return false;
        };
        if state.connections.is_none() || state.permissions.is_none() || !self.offered.insert(profile.clone()) {
            return false;
        }
        needs_setup(state)
    }

    /// Keeps the step where the owner left the setup of the selected profile in this window.
    /// A profile without one starts at the first open step, once the server's state is loaded.
    pub fn open(&mut self, state: &UiState) {
        if let (Some(profile), Some(_)) = (&state.selected, &state.connections) {
            self.steps
                .entry(profile.clone())
                .or_insert_with(|| first_open_step(state));
        }
    }

    /// The step that the setup shows for the selected profile.
    pub fn step(&self) -> Step {
        self.profile
            .as_ref()
            .and_then(|profile| self.steps.get(profile))
            .copied()
            .unwrap_or(Step::Llm)
    }

    fn go(&mut self, step: Step) {
        if let Some(profile) = &self.profile {
            self.steps.insert(profile.clone(), step);
        }
        self.message = None;
        self.input_message = None;
    }

    /// True if the server's LLM endpoint is the one that passed the last test in this window.
    fn llm_passed(&self, state: &UiState) -> bool {
        let server = state.connections.as_ref().and_then(|c| c.llm.as_ref());
        let passed = self.profile.as_ref().and_then(|profile| self.passed.get(profile));
        server.is_some() && server == passed
    }
}

impl Shell {
    /// Shows the setup of the selected profile.
    pub(crate) fn open_setup(&mut self) {
        self.setup.open(&self.state);
        self.setup.message = None;
        self.view = View::Setup;
    }

    fn setup_next(&mut self, skip: bool) {
        let step = self.setup.step();
        if step == Step::Llm && !skip && !self.setup.llm_passed(&self.state) {
            self.setup.message = Some(TEST_FIRST);
            return;
        }
        if let Some(next) = step.next() {
            self.setup.go(next);
        }
    }

    /// Ends the setup if every write and delete rule is at Observe or Approve. Nothing is stored.
    fn setup_finish(&mut self) {
        let unready = self.state.permissions.as_ref().map(unready_rules);
        if unready.is_none_or(|unready| !unready.is_empty()) {
            self.setup.message = Some(NOT_READY);
            return;
        }
        if let Some(profile) = &self.setup.profile {
            self.setup.steps.remove(profile);
        }
        self.view = View::Inbox;
    }

    /// Sets each write and delete rule that is not at Observe or Approve to Approve.
    fn rules_to_approve(&mut self) {
        let (Some(profile), Some(server)) = (self.setup.profile.clone(), &self.state.permissions) else {
            return;
        };
        let mut rules = server.rules.clone();
        for (tool, _) in unready_rules(server) {
            if let Some(rule) = rules.get_mut(&tool) {
                rule.tier = Tier::Approve;
            }
        }
        let update = PermissionsUpdate {
            rules,
            expected: Expected::Value(server.rules.clone()),
        };
        self.setup.message = None;
        self.send(UiCommand::SavePermissions { profile, update });
    }

    /// Asks for principle candidates in the result of the typed read. Arguments that are not a JSON object are not sent.
    fn extract_from_read(&mut self, cx: &App) {
        let form = &mut self.setup;
        let Some(profile) = form.profile.clone() else {
            return;
        };
        let tool = form.read_tool.read(cx).value().trim().to_owned();
        let args = form.read_args.read(cx).value().trim().to_owned();
        let args = match args.as_str() {
            "" => Ok(serde_json::Map::new()),
            text => serde_json::from_str(text).map_err(|e| format!("The arguments are not a JSON object: {e}")),
        };
        match args {
            Ok(args) => {
                form.input_message = None;
                let source = PrincipleSource::Read { tool, args };
                self.send(UiCommand::ExtractPrinciples { profile, source });
            }
            Err(error) => form.input_message = Some(error),
        }
    }

    fn add_cases(&mut self, cx: &App) {
        let form = &mut self.setup;
        let Some(profile) = form.profile.clone() else {
            return;
        };
        match parse_cases(&form.cases.read(cx).value()) {
            Ok(cases) => {
                form.input_message = None;
                self.send(UiCommand::AddReplayCases { profile, cases });
            }
            Err(error) => form.input_message = Some(error),
        }
    }

    /// The line of step names above the page. The open step is marked.
    fn setup_steps(&self, step: Step, cx: &Context<Self>) -> impl IntoElement {
        let colors = palette(cx);
        div().flex().flex_wrap().gap_2().children(Step::ALL.map(|each| {
            let text = format!("{}. {}", each.number(), each.title());
            let open = each == step;
            div()
                .px_2()
                .py_0p5()
                .rounded_full()
                .text_sm()
                .when(open, |mark| {
                    mark.bg(colors.done.opacity(0.15)).font_weight(FontWeight::MEDIUM)
                })
                .when(!open, |mark| mark.text_color(cx.theme().muted_foreground))
                .child(SharedString::from(text))
        }))
    }

    /// The tool list of each connector, with List and Approve.
    fn setup_tool_lists(&self, cx: &Context<Self>) -> AnyElement {
        let Some(profile) = self.setup.profile.clone() else {
            return div().into_any_element();
        };
        let connectors: Vec<&String> = self
            .state
            .connections
            .iter()
            .flat_map(|c| c.connectors.keys())
            .collect();
        let rows = connectors.into_iter().map(|name| {
            let approved = self
                .state
                .permissions
                .as_ref()
                .and_then(|p| p.tools.get(name))
                .map_or(0, Vec::len);
            let check = UiCommand::CheckConnector {
                profile: profile.clone(),
                name: name.clone(),
            };
            let mut row = card(cx)
                .id(SharedString::from(format!("setup-{name}")))
                .gap_2()
                .child(
                    div()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(SharedString::from(name.clone())),
                )
                .child(labelled_as(
                    "approved-count",
                    format!("Approved tools: {approved}"),
                    quiet(format!("{approved} tool(s) approved"), cx),
                ))
                .child(
                    div().flex().child(
                        Button::new("list-tools")
                            .small()
                            .outline()
                            .label("List the tools")
                            .on_click(cx.listener(move |this, _, _, _| this.send(check.clone()))),
                    ),
                );
            match &self.state.connection_result {
                Some(ConnectionResult::Checked { name: checked, result }) if checked == name => match result {
                    Ok(report) => {
                        // The names and descriptions are the server's text. They are shown as data.
                        let tools = report.tools.iter().map(|tool| {
                            div()
                                .flex()
                                .flex_col()
                                .py_1()
                                .border_b_1()
                                .border_color(cx.theme().border)
                                .child(labelled(format!("tool-{}", tool.name), tool.name.clone()))
                                .child(
                                    div()
                                        .max_h(px(40.))
                                        .overflow_hidden()
                                        .child(quiet(tool.description.clone(), cx)),
                                )
                        });
                        row = row.child(div().flex().flex_col().children(tools));
                        row = if report.diff.is_clean() {
                            row.child(labelled("tools-state", "This list is approved."))
                        } else {
                            let approve = UiCommand::ApproveTools {
                                profile: profile.clone(),
                                name: name.clone(),
                                shown: pin(&report.tools),
                            };
                            row.child(
                                div().flex().child(
                                    Button::new("approve-tools")
                                        .small()
                                        .primary()
                                        .label("Approve these tools")
                                        .on_click(cx.listener(move |this, _, _, _| this.send(approve.clone()))),
                                ),
                            )
                        };
                    }
                    Err(error) => row = row.child(labelled("tools-state", error.clone())),
                },
                Some(ConnectionResult::ToolsApproved { name: approved, result }) if approved == name => {
                    let text = match result {
                        Ok(()) => "Tools approved. Give each one a rule.".to_owned(),
                        Err(error) => error.clone(),
                    };
                    row = row.child(labelled("tools-state", text));
                }
                _ => {}
            }
            row
        });
        div()
            .id("setup-tool-lists")
            .w(px(380.))
            .flex_none()
            .flex()
            .flex_col()
            .gap_3()
            .overflow_y_scroll()
            .children(rows)
            .into_any_element()
    }

    /// The read that principle candidates come from.
    fn setup_read_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let form = &self.setup;
        card(cx)
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Find in a read"))
            .child(quiet(
                "Bunsin calls one tool that reads, with the arguments you give. The LLM gets the result as data.",
                cx,
            ))
            .child(field(
                "Tool",
                Input::new(&form.read_tool).id("read-tool").aria_label("Tool"),
                cx,
            ))
            .child(field(
                "Arguments, a JSON object",
                Input::new(&form.read_args).id("read-args").aria_label("Arguments"),
                cx,
            ))
            .child(
                div().flex().child(
                    Button::new("extract-read")
                        .small()
                        .outline()
                        .label("Find in this read")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.extract_from_read(cx);
                            cx.notify();
                        })),
                ),
            )
            .child(
                div()
                    .text_sm()
                    .child(labelled("read-message", form.input_message.clone().unwrap_or_default())),
            )
    }

    /// The field that adds replay cases.
    fn setup_cases_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let form = &self.setup;
        let result = match (&form.input_message, &self.state.cases_result) {
            (Some(message), _) => message.clone(),
            (None, Some(Ok(1))) => "1 case added.".to_owned(),
            (None, Some(Ok(count))) => format!("{count} cases added."),
            (None, Some(Err(error))) => error.clone(),
            (None, None) => String::new(),
        };
        card(cx)
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Add replay cases"))
            .child(quiet(
                "A JSON array, as `bunsind add-replay-cases` reads it. The server filters personal data first.",
                cx,
            ))
            // The text area has no ID of its own. Tests find it inside this element.
            .child(
                div()
                    .id("setup-cases")
                    .child(Textarea::new(&form.cases).aria_label("Replay cases")),
            )
            .child(
                div().flex().child(
                    Button::new("add-cases")
                        .small()
                        .outline()
                        .label("Add the cases")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.add_cases(cx);
                            cx.notify();
                        })),
                ),
            )
            .child(div().text_sm().child(labelled("cases-result", result)))
    }

    /// What the setup did, and whether every write and delete rule is at Observe or Approve.
    fn setup_summary(&self, cx: &Context<Self>) -> impl IntoElement {
        let state = &self.state;
        let llm = match state.connections.as_ref().and_then(|c| c.llm.as_ref()) {
            None => "LLM endpoint: none",
            Some(_) if self.setup.llm_passed(state) => "LLM endpoint: saved, and its test works",
            Some(_) => "LLM endpoint: saved, not tested in this window",
        };
        let connectors = state.connections.as_ref().map_or(0, |c| c.connectors.len());
        let tools: usize = state
            .permissions
            .as_ref()
            .map_or(0, |p| p.tools.values().map(Vec::len).sum());
        let rules = state.permissions.as_ref().map(|p| &p.rules);
        let reads = rules.map_or(0, |r| r.values().filter(|r| r.class == ToolClass::Read).count());
        let writes = rules.map_or(0, |r| r.len()) - reads;
        let pages = state.notes.as_ref().map_or(0, Vec::len);
        let waiting = state.snapshot.as_ref().map_or(0, |s| {
            s.inbox
                .iter()
                .filter(|i| matches!(i.body, InboxBody::WikiChange { .. }))
                .filter(|i| i.state == bunsin_core::approval::ApprovalState::Pending)
                .count()
        });
        let principles = state.principles.as_ref().map_or(0, Vec::len);
        let eval = match &state.eval_report {
            Some(report) if report.run.is_some() => format!("Evaluation: {}", rates_text(report)),
            _ => "Evaluation: no run yet".to_owned(),
        };
        let lines = [
            ("summary-llm", llm.to_owned()),
            (
                "summary-connectors",
                format!("Connectors: {connectors}, approved tools: {tools}"),
            ),
            (
                "summary-rules",
                format!("Rules: {reads} read, {writes} write or delete"),
            ),
            (
                "summary-notes",
                format!("Notes: {pages} page(s), {waiting} page change(s) wait in the inbox"),
            ),
            ("summary-principles", format!("Principles: {principles}")),
            ("summary-eval", eval),
        ];
        let colors = palette(cx);
        let unready = state.permissions.as_ref().map(unready_rules).unwrap_or_default();
        let check = if unready.is_empty() {
            notice(colors.done, cx).child(labelled(
                "rules-check",
                "Every write and delete rule is at Observe or Approve.",
            ))
        } else {
            let names: Vec<String> = unready
                .iter()
                .map(|(tool, tier)| format!("{tool} ({})", tier_word(*tier)))
                .collect();
            notice(colors.stopped, cx)
                .child(labelled(
                    "rules-check",
                    format!("Not at Observe or Approve: {}", names.join(", ")),
                ))
                .child(
                    Button::new("rules-to-approve")
                        .small()
                        .outline()
                        .label("Set them to Approve")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.rules_to_approve();
                            cx.notify();
                        })),
                )
        };
        card(cx)
            .w(px(560.))
            .flex_none()
            .children(lines.map(|(id, text)| labelled_as(id, text.clone(), div().child(SharedString::from(text)))))
            .child(check)
    }

    /// The setup view: the step names, the help of the step, its controls, and the buttons that move.
    pub(crate) fn setup_section(&self, cx: &Context<Self>) -> AnyElement {
        let step = self.setup.step();
        let body = |cards: Vec<AnyElement>| {
            div()
                .id("setup-body")
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .flex()
                .flex_wrap()
                .items_start()
                .gap_5()
                .overflow_y_scroll()
                .children(cards)
                .into_any_element()
        };
        let page = match step {
            Step::Llm => {
                let state = if self.setup.llm_passed(&self.state) {
                    "The endpoint works. Next goes on."
                } else {
                    "No test of this endpoint has worked in this window yet."
                };
                body(vec![
                    self.llm_card(cx)
                        .map(|c| c.w(px(520.)).into_any_element())
                        .unwrap_or_else(|| div().into_any_element()),
                    labelled_as("setup-llm-state", state, quiet(state, cx)).into_any_element(),
                ])
            }
            Step::Connect => body(vec![
                self.connectors_card(cx)
                    .map(|c| c.w(px(520.)).into_any_element())
                    .unwrap_or_else(|| div().into_any_element()),
            ]),
            Step::Tools => div()
                .flex_1()
                .min_h(px(0.))
                .min_w(px(0.))
                .flex()
                .gap_5()
                .child(self.setup_tool_lists(cx))
                .child(self.permissions_section(cx))
                .into_any_element(),
            Step::Knowledge => body(vec![self.preload_card(cx).into_any_element()]),
            Step::Principles => body(vec![
                self.setup_read_card(cx).into_any_element(),
                self.candidates_card(cx).into_any_element(),
            ]),
            Step::Evaluation => body(vec![
                self.setup_cases_card(cx).into_any_element(),
                self.eval_card(cx).into_any_element(),
            ]),
            Step::Finish => body(vec![self.setup_summary(cx).into_any_element()]),
        };
        let mut nav = div().id("setup-nav").flex().items_center().gap_2();
        if let Some(back) = step.back() {
            nav = nav.child(Button::new("setup-back").outline().label("Back").on_click(cx.listener(
                move |this, _, _, cx| {
                    this.setup.go(back);
                    cx.notify();
                },
            )));
        }
        nav = if step == Step::Finish {
            nav.child(
                Button::new("setup-finish")
                    .primary()
                    .label("Finish")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.setup_finish();
                        cx.notify();
                    })),
            )
        } else {
            nav.child(
                Button::new("setup-skip")
                    .ghost()
                    .label("Skip")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.setup_next(true);
                        cx.notify();
                    })),
            )
            .child(
                Button::new("setup-next")
                    .primary()
                    .label("Next")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.setup_next(false);
                        cx.notify();
                    })),
            )
        };
        let nav = nav
            .child(
                div()
                    .flex_1()
                    .text_sm()
                    .text_color(palette(cx).stopped)
                    .child(labelled("setup-message", self.setup.message.unwrap_or_default())),
            )
            .child(
                Button::new("setup-leave")
                    .ghost()
                    .label("Leave the setup")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.view = View::Agent;
                        cx.notify();
                    })),
            );
        let title = format!("Step {} of {}: {}", step.number(), Step::ALL.len(), step.title());
        div()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_3()
            .child(self.setup_steps(step, cx))
            .child(labelled_as(
                "setup-step",
                title.clone(),
                div()
                    .text_lg()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(title)),
            ))
            .child(quiet(step.help(), cx))
            .child(page)
            .child(nav)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bunsin_api::{Connections, ConnectorConnection, TokenSource};
    use bunsin_core::policy::{OutgoingText, ToolRule};
    use bunsin_core::settings::{ConnectorKind, McpConfig, Wire};
    use bunsin_core::tool::ToolSpec;
    use bunsin_core::wiki::WikiSummary;

    fn llm() -> LlmConnection {
        LlmConnection {
            wire: Wire::AnthropicMessages,
            base_url: "https://llm.example".into(),
            model: "m".into(),
            timeout_secs: 30,
            token: TokenSource::Stored,
        }
    }

    fn connector() -> ConnectorConnection {
        ConnectorConnection {
            kind: ConnectorKind::Mcp(McpConfig::Http {
                url: "https://mcp.example/slack".into(),
            }),
            token: TokenSource::None,
            token_header: None,
            secret_env: Vec::new(),
            secret_env_missing: Vec::new(),
            oauth: None,
        }
    }

    fn post() -> ToolSpec {
        ToolSpec {
            name: "post".into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
        }
    }

    fn write(tier: Tier) -> ToolRule {
        ToolRule {
            tier,
            ..ToolRule::write(ToolClass::Write, OutgoingText::None)
        }
    }

    /// A state with what the server shows after each step up to `done`.
    fn state(llm_saved: bool, connected: bool, approved: bool, ruled: bool) -> UiState {
        let connectors = if connected {
            BTreeMap::from([("slack".to_owned(), connector())])
        } else {
            BTreeMap::new()
        };
        let tools = if approved {
            BTreeMap::from([("slack".to_owned(), vec![post()])])
        } else {
            BTreeMap::new()
        };
        let rules = if ruled {
            BTreeMap::from([("slack/post".to_owned(), write(Tier::Approve))])
        } else {
            BTreeMap::new()
        };
        UiState {
            connections: Some(Connections {
                llm: llm_saved.then(llm),
                connectors,
            }),
            permissions: Some(Permissions {
                tools,
                rules,
                ai_marker: String::new(),
            }),
            ..UiState::default()
        }
    }

    #[test]
    fn the_setup_resumes_at_the_first_step_the_server_does_not_show() {
        assert_eq!(first_open_step(&UiState::default()), Step::Llm);
        assert_eq!(first_open_step(&state(false, true, true, true)), Step::Llm);
        assert_eq!(first_open_step(&state(true, false, false, false)), Step::Connect);
        assert_eq!(first_open_step(&state(true, true, false, false)), Step::Tools);
        // Approved tools without any rule are not done yet.
        assert_eq!(first_open_step(&state(true, true, true, false)), Step::Tools);
        let mut ready = state(true, true, true, true);
        assert_eq!(first_open_step(&ready), Step::Knowledge);
        ready.notes = Some(vec![WikiSummary {
            slug: "deploy".into(),
            title: "Deploy".into(),
            summary: "When".into(),
            revision: 1,
            updated_at: 0,
        }]);
        assert_eq!(first_open_step(&ready), Step::Principles);
        ready.principles = Some(vec!["Answer briefly.".into()]);
        assert_eq!(first_open_step(&ready), Step::Evaluation);
    }

    #[test]
    fn only_a_profile_without_llm_or_connector_and_without_approved_tools_needs_the_setup() {
        assert!(!needs_setup(&UiState::default()), "nothing loaded yet");
        assert!(needs_setup(&state(false, false, false, false)));
        assert!(needs_setup(&state(true, false, false, false)));
        assert!(needs_setup(&state(false, true, false, false)));
        assert!(!needs_setup(&state(true, true, false, false)));
        // Approved tools mean the profile was set up in another way, for example with the settings file.
        assert!(!needs_setup(&state(false, false, true, true)));
    }

    #[test]
    fn a_write_or_delete_rule_is_ready_only_at_observe_or_approve() {
        let mut rules = BTreeMap::from([
            ("slack/search".to_owned(), ToolRule::read()),
            ("slack/post".to_owned(), write(Tier::Approve)),
            ("slack/react".to_owned(), write(Tier::Observe)),
        ]);
        let permissions = |rules: &BTreeMap<String, ToolRule>| Permissions {
            tools: BTreeMap::new(),
            rules: rules.clone(),
            ai_marker: String::new(),
        };
        assert!(unready_rules(&permissions(&rules)).is_empty());
        rules.insert("slack/edit".into(), write(Tier::Draft));
        rules.insert("slack/pin".into(), write(Tier::Autonomous));
        let delete = ToolRule {
            tier: Tier::Autonomous,
            ..ToolRule::write(ToolClass::Delete, OutgoingText::None)
        };
        rules.insert("slack/delete".into(), delete);
        assert_eq!(
            unready_rules(&permissions(&rules)),
            [
                ("slack/delete".to_owned(), Tier::Autonomous),
                ("slack/edit".to_owned(), Tier::Draft),
                ("slack/pin".to_owned(), Tier::Autonomous),
            ]
        );
    }

    #[test]
    fn the_steps_go_in_plan_order_and_stop_at_the_ends() {
        assert_eq!(Step::Llm.back(), None);
        assert_eq!(Step::Llm.next(), Some(Step::Connect));
        assert_eq!(Step::Evaluation.next(), Some(Step::Finish));
        assert_eq!(Step::Finish.next(), None);
        assert_eq!(Step::Finish.back(), Some(Step::Evaluation));
        let numbers: Vec<usize> = Step::ALL.iter().map(|s| s.number()).collect();
        assert_eq!(numbers, [1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn cases_are_a_json_array_of_at_least_one_case() {
        let one = r#"[{"event": {"source": "slack", "external_id": "c1", "author": "lee", "text": "when?"},
            "actual_reply": "At 3 pm."}]"#;
        let cases = parse_cases(one).unwrap();
        assert_eq!(cases.len(), 1);
        assert_eq!(cases[0].actual_reply, "At 3 pm.");
        assert_eq!(parse_cases("[]").unwrap_err(), "Enter at least one case.");
        assert!(
            parse_cases("{}")
                .unwrap_err()
                .starts_with("The cases are not a JSON array of cases")
        );
        assert!(parse_cases(r#"[{"actual_reply": "x"}]"#).is_err());
    }
}
