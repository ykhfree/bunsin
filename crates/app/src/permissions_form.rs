//! The permissions view of the main window: one rule for each approved tool.
//! The clicks change a draft. Only Save sends the table to the server.

use crate::bridge::{SaveError, UiCommand, UiState};
use crate::shell::{Shell, View, labelled, labelled_as};
use crate::theme::{card, notice, palette, quiet};
use bunsin_api::{Expected, PermissionsUpdate, ToolAutonomy};
use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use bunsin_core::tool::ToolSpec;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    AnyElement, Context, FontWeight, IntoElement, ParentElement, SharedString, Styled, Window, div, prelude::*, px,
};
use std::collections::BTreeMap;

const HELP: &str = "The agent can use a tool only if it has a rule here. Read: the agent calls the tool without \
    asking, also while all changes are stopped. Give Read only to a tool that changes nothing and sends nothing out. \
    Write and Delete: the step says how far the agent goes. For a tool that sends text to other people, choose \
    every argument that holds such text: the server adds the AI marker to each.";
const NESTED: &str = "The buttons list the text arguments at the top of a tool's input. A click adds or removes one. \
    If the text is inside another value, for example in the items of a list, set the rule with \
    `bunsind rule-set` or in the settings file.";
const HELD: &str = "Held at approval: you dismissed or reported a call of this tool while it ran without \
    asking. Each call asks you, whatever the step says, until you clear the hold.";
const PAGE_CONFLICT: &str = "The rules changed on the server since this view loaded them. Nothing was saved.";
const UNCHANGED: &str = "Nothing changed, so nothing was saved.";

/// One rule of the draft. A write or delete rule is not complete until the owner says
/// where its text for other people is, or that it sends none.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DraftRule {
    class: ToolClass,
    tier: Tier,
    /// None: not chosen yet.
    text: Option<OutgoingText>,
}

impl DraftRule {
    fn of(rule: &ToolRule) -> Self {
        Self {
            class: rule.class,
            tier: rule.tier,
            text: Some(rule.outgoing_text.clone()),
        }
    }

    /// The rule to store, or None if the text is not chosen yet.
    fn rule(&self) -> Option<ToolRule> {
        Some(ToolRule {
            class: self.class,
            tier: self.tier,
            outgoing_text: self.text.clone()?,
        })
    }
}

/// The names of the string arguments at the top of a tool's input. Only these can get the AI marker.
fn text_args(tool: &ToolSpec) -> Vec<String> {
    let is_text = |schema: &serde_json::Value| match &schema["type"] {
        serde_json::Value::String(kind) => kind == "string",
        serde_json::Value::Array(kinds) => kinds.iter().any(|kind| kind == "string"),
        _ => false,
    };
    tool.input_schema["properties"]
        .as_object()
        .map(|properties| {
            properties
                .iter()
                .filter(|(_, schema)| is_text(schema))
                .map(|(name, _)| name.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn class_word(class: ToolClass) -> &'static str {
    match class {
        ToolClass::Read => "read",
        ToolClass::Write => "write",
        ToolClass::Delete => "delete",
    }
}

pub(crate) fn tier_word(tier: Tier) -> &'static str {
    match tier {
        Tier::Observe => "observe",
        Tier::Draft => "draft",
        Tier::Approve => "approve",
        Tier::Autonomous => "autonomous",
    }
}

/// The top-level arguments that a text choice names, or None if it names a place inside another value.
fn top_level(text: &OutgoingText) -> Option<Vec<String>> {
    match text {
        OutgoingText::None => Some(Vec::new()),
        OutgoingText::Arg(name) => Some(vec![name.clone()]),
        OutgoingText::Paths(paths) => paths.iter().all(|path| !path.contains('.')).then(|| paths.clone()),
    }
}

/// The text choice after a click on `arg`: the argument is added or removed. `offered` sets the order.
/// The choice is empty again when the last argument is removed, so "no text" is never a default.
fn toggle_arg(text: Option<&OutgoingText>, arg: &str, offered: &[String]) -> Option<OutgoingText> {
    let mut chosen = text.and_then(top_level).unwrap_or_default();
    if let Some(at) = chosen.iter().position(|name| name == arg) {
        chosen.remove(at);
    } else {
        chosen.push(arg.to_owned());
    }
    let mut chosen: Vec<String> = offered.iter().filter(|name| chosen.contains(name)).cloned().collect();
    match chosen.len() {
        0 => None,
        // One argument is stored in the form from before, so older clients can read the rule.
        1 => chosen.pop().map(OutgoingText::Arg),
        _ => Some(OutgoingText::Paths(chosen)),
    }
}

/// The places of a text choice as a list, for example "`text`, `blocks.*.text`".
fn places(text: &OutgoingText) -> String {
    let names: Vec<String> = text.locations().iter().map(|place| format!("`{place}`")).collect();
    names.join(", ")
}

/// A rule in a few words, for the list of changes.
fn rule_words(rule: Option<&DraftRule>) -> String {
    match rule {
        None => "off".to_owned(),
        Some(rule) if rule.class == ToolClass::Read => "read".to_owned(),
        Some(rule) => format!(
            "{}, {}, {}",
            class_word(rule.class),
            tier_word(rule.tier),
            match &rule.text {
                None => "text not chosen".to_owned(),
                Some(OutgoingText::None) => "no text".to_owned(),
                Some(text) => format!("text in {}", places(text)),
            }
        ),
    }
}

/// When a tool is suggested, as one line above the list.
fn thresholds_text(thresholds: Option<&AutonomyThresholds>) -> String {
    match thresholds {
        None => "Suggestions for Autonomous are off. Set `autonomy` in the settings file to get them.".to_owned(),
        Some(t) => format!(
            "Suggested for Autonomous: a write tool at Approve with {} or more reviewed calls and {}% or more \
             approved unchanged in the last {} days. A hold starts the counts again.",
            t.min_reviewed, t.min_unchanged_percent, t.window_days
        ),
    }
}

/// The counts of one tool as a line under it.
fn counts_text(entry: &ToolAutonomy, thresholds: Option<&AutonomyThresholds>) -> String {
    let c = &entry.counts;
    let days = match thresholds {
        None => "All days".to_owned(),
        Some(t) => format!("Last {} days", t.window_days),
    };
    let percent = c
        .unchanged_percent()
        .map_or_else(String::new, |percent| format!(" ({percent}%)"));
    format!(
        "{days}: {} reviewed, {} approved unchanged{percent}, {} edited, {} dismissed. \
         Ran without asking: {}, failed: {}.",
        c.reviewed(),
        c.approved,
        c.edited,
        c.rejected,
        c.autonomous,
        c.autonomous_failed
    )
}

/// What a rule lets the agent do, as a sentence under the tool.
fn rule_sentence(rule: Option<&DraftRule>) -> String {
    let Some(rule) = rule else {
        return "The agent cannot use this tool.".to_owned();
    };
    let does = match (rule.class, rule.tier) {
        (ToolClass::Read, _) => {
            return "Read: the agent calls it without asking, also while all changes are stopped.".to_owned();
        }
        (_, Tier::Observe) => "the proposal is stored. You do not see it, and it does not run.",
        (_, Tier::Draft) => "you see the draft. It does not run.",
        (_, Tier::Approve) => "it runs after you approve.",
        // The server asks for every delete, whatever the step says.
        (ToolClass::Delete, Tier::Autonomous) => "it runs after you approve. A delete always asks.",
        (_, Tier::Autonomous) => "it runs without asking.",
    };
    let text = match &rule.text {
        None => "Choose where its text for other people is.".to_owned(),
        Some(OutgoingText::None) => "It sends no text to other people.".to_owned(),
        Some(text) => format!("The AI marker goes into {}.", places(text)),
    };
    let class = if rule.class == ToolClass::Write {
        "Write"
    } else {
        "Delete"
    };
    format!("{class}: {does} {text}")
}

pub struct PermissionsForm {
    /// The profile whose rules the draft is for.
    profile: Option<String>,
    /// The server's rules when the draft was made from them.
    base: BTreeMap<String, ToolRule>,
    draft: BTreeMap<String, DraftRule>,
    /// The value of `UiState::permission_saves` in the last state the form received.
    saves_seen: u64,
    /// The window's own message about the last Save that it did not send.
    message: Option<String>,
    /// True after the server refused a save because the rules changed since the draft was made.
    conflict: bool,
}

impl PermissionsForm {
    pub fn new() -> Self {
        Self {
            profile: None,
            base: BTreeMap::new(),
            draft: BTreeMap::new(),
            saves_seen: 0,
            message: None,
            conflict: false,
        }
    }

    fn base_draft(&self) -> BTreeMap<String, DraftRule> {
        self.base
            .iter()
            .map(|(tool, rule)| (tool.clone(), DraftRule::of(rule)))
            .collect()
    }

    fn changed(&self) -> bool {
        self.draft != self.base_draft()
    }

    fn load(&mut self, rules: &BTreeMap<String, ToolRule>) {
        self.base = rules.clone();
        self.draft = self.base_draft();
        self.message = None;
        self.conflict = false;
    }

    /// Takes a new state from the server. A changed draft stays. The form loads the server's
    /// rules for another profile, after a successful save, or if the draft was not changed.
    pub fn receive(&mut self, state: &UiState) {
        let new = state.permission_saves != self.saves_seen;
        self.saves_seen = state.permission_saves;
        let saved = new && state.permissions_result == Some(Ok(()));
        if new && state.permissions_result == Some(Err(SaveError::Conflict)) {
            self.conflict = true;
        }
        if self.profile != state.selected {
            self.profile = None;
            self.load(&BTreeMap::new());
        }
        let (Some(profile), Some(server)) = (&state.selected, &state.permissions) else {
            return;
        };
        if self.profile.is_none() || saved || (!self.changed() && self.base != server.rules) {
            self.load(&server.rules);
        }
        self.profile = Some(profile.clone());
    }

    /// The lines "tool: before → after" of every rule that a save would change.
    fn changes(&self) -> Vec<String> {
        let base = self.base_draft();
        let tools: std::collections::BTreeSet<&String> = base.keys().chain(self.draft.keys()).collect();
        tools
            .into_iter()
            .filter(|tool| base.get(*tool) != self.draft.get(*tool))
            .map(|tool| {
                format!(
                    "{tool}: {} → {}",
                    rule_words(base.get(tool)),
                    rule_words(self.draft.get(tool))
                )
            })
            .collect()
    }

    /// Sets the class of a tool. A new write or delete rule starts at "run after approval",
    /// whatever the rule before it was, and it has no text choice until the owner makes one.
    fn set_class(&mut self, tool: &str, class: Option<ToolClass>) {
        self.message = None;
        let Some(class) = class else {
            self.draft.remove(tool);
            return;
        };
        let rule = match (class, self.draft.get(tool)) {
            (ToolClass::Read, _) => DraftRule::of(&ToolRule::read()),
            // The text choice stays between write and delete. The step does not.
            (_, Some(old)) if old.class != ToolClass::Read => DraftRule {
                class,
                tier: Tier::Approve,
                text: old.text.clone(),
            },
            _ => DraftRule {
                class,
                tier: Tier::Approve,
                text: None,
            },
        };
        self.draft.insert(tool.to_owned(), rule);
    }
}

impl Default for PermissionsForm {
    fn default() -> Self {
        Self::new()
    }
}

impl Shell {
    /// Sends the draft. The server refuses it if its rules are no longer the base,
    /// unless `anyway` is true.
    fn save_permissions(&mut self, anyway: bool, _: &mut Context<Self>) {
        let form = &mut self.permissions;
        let Some(profile) = form.profile.clone() else {
            return;
        };
        // A write that sends text without the AI marker must be a choice, not a default.
        let undecided: Vec<&str> = form
            .draft
            .iter()
            .filter(|(_, rule)| rule.text.is_none())
            .map(|(tool, _)| tool.as_str())
            .collect();
        if !undecided.is_empty() {
            form.message = Some(format!(
                "Choose where the text for other people is, for: {}.",
                undecided.join(", ")
            ));
            return;
        }
        if !form.changed() {
            form.message = Some(UNCHANGED.to_owned());
            return;
        }
        let rules = form
            .draft
            .iter()
            .filter_map(|(tool, rule)| Some((tool.clone(), rule.rule()?)))
            .collect();
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(form.base.clone())
        };
        form.message = None;
        form.conflict = false;
        self.send(UiCommand::SavePermissions {
            profile,
            update: PermissionsUpdate { rules, expected },
        });
    }

    /// Drops the draft and shows the server's rules.
    fn load_permissions(&mut self, _: &mut Window, _: &mut Context<Self>) {
        if let Some(server) = &self.state.permissions {
            let rules = server.rules.clone();
            self.permissions.load(&rules);
        }
    }

    /// One choice of a row: a small button that shows whether it is the chosen one.
    fn choice(
        id: String,
        label: impl Into<SharedString>,
        chosen: bool,
        cx: &Context<Self>,
        set: impl Fn(&mut Self) + 'static,
    ) -> AnyElement {
        let button = Button::new(SharedString::from(id))
            .small()
            .label(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                set(this);
                cx.notify();
            }));
        if chosen { button.primary() } else { button.outline() }.into_any_element()
    }

    /// The row of one approved tool.
    fn tool_row(&self, id: &str, tool: &ToolSpec, cx: &Context<Self>) -> AnyElement {
        let rule = self.permissions.draft.get(id);
        let class = rule.map(|rule| rule.class);
        let classes = [
            ("off", "Off", None),
            ("read", "Read", Some(ToolClass::Read)),
            ("write", "Write", Some(ToolClass::Write)),
            ("delete", "Delete", Some(ToolClass::Delete)),
        ]
        .map(|(key, label, value)| {
            let tool_id = id.to_owned();
            Self::choice(format!("perm-{id}-{key}"), label, class == value, cx, move |this| {
                this.permissions.set_class(&tool_id, value)
            })
        });
        let row = || div().flex().flex_wrap().items_center().gap_2();
        let mut lines: Vec<AnyElement> = Vec::new();
        if let Some(rule) = rule.filter(|rule| rule.class != ToolClass::Read) {
            // "Autonomous" is not offered for a delete. The setup offers only Observe and Approve,
            // the steps a setup may end with. A stored rule's step is always shown.
            let setup = self.view == View::Setup;
            let tiers = [Tier::Observe, Tier::Draft, Tier::Approve, Tier::Autonomous]
                .into_iter()
                .filter(|tier| {
                    rule.tier == *tier
                        || match tier {
                            Tier::Draft | Tier::Autonomous if setup => false,
                            Tier::Autonomous => rule.class == ToolClass::Write,
                            _ => true,
                        }
                })
                .map(|tier| {
                    let tool_id = id.to_owned();
                    let word = tier_word(tier);
                    let label = format!("{}{}", word[..1].to_uppercase(), &word[1..]);
                    Self::choice(
                        format!("perm-{id}-tier-{word}"),
                        label,
                        rule.tier == tier,
                        cx,
                        move |this| {
                            if let Some(rule) = this.permissions.draft.get_mut(&tool_id) {
                                rule.tier = tier;
                            }
                        },
                    )
                });
            lines.push(row().child(quiet("Step", cx)).children(tiers).into_any_element());

            // The arguments the buttons can offer, and the stored ones even if they are not among them.
            // The server's ones stay in the list after another click, so the owner can go back to them.
            let mut args = text_args(tool);
            let base = self.permissions.base.get(id).map(|rule| &rule.outgoing_text);
            for stored in [rule.text.as_ref(), base].into_iter().flatten().filter_map(top_level) {
                for name in stored {
                    if !args.contains(&name) {
                        args.push(name);
                    }
                }
            }
            let chosen_args = rule.text.as_ref().and_then(top_level).unwrap_or_default();
            let mut texts: Vec<AnyElement> = args
                .iter()
                .map(|arg| {
                    let (tool_id, arg_name, offered) = (id.to_owned(), arg.clone(), args.clone());
                    let chosen = chosen_args.contains(arg);
                    Self::choice(format!("perm-{id}-text-{arg}"), arg.clone(), chosen, cx, move |this| {
                        if let Some(rule) = this.permissions.draft.get_mut(&tool_id) {
                            rule.text = toggle_arg(rule.text.as_ref(), &arg_name, &offered);
                        }
                        this.permissions.message = None;
                    })
                })
                .collect();
            // A stored choice with a place inside another value has one button. A click restores it whole.
            if let Some(stored) = base.filter(|text| top_level(text).is_none()) {
                let (tool_id, value) = (id.to_owned(), stored.clone());
                let chosen = rule.text.as_ref() == Some(stored);
                texts.push(Self::choice(
                    format!("perm-{id}-text-stored"),
                    places(stored),
                    chosen,
                    cx,
                    move |this| {
                        if let Some(rule) = this.permissions.draft.get_mut(&tool_id) {
                            rule.text = Some(value.clone());
                        }
                        this.permissions.message = None;
                    },
                ));
            }
            let tool_id = id.to_owned();
            let none = Self::choice(
                format!("perm-{id}-text-none"),
                "Sends no text to other people",
                rule.text == Some(OutgoingText::None),
                cx,
                move |this| {
                    if let Some(rule) = this.permissions.draft.get_mut(&tool_id) {
                        rule.text = Some(OutgoingText::None);
                    }
                    this.permissions.message = None;
                },
            );
            lines.push(
                row()
                    .child(quiet("Text for other people is in", cx))
                    .children(texts)
                    .child(none)
                    .into_any_element(),
            );
        }
        let sentence = rule_sentence(rule);
        let autonomy = self.state.autonomy.as_ref();
        let thresholds = autonomy.and_then(|autonomy| autonomy.thresholds.as_ref());
        let entry = autonomy.and_then(|autonomy| autonomy.tools.iter().find(|entry| entry.tool == id));
        let colors = palette(cx);
        let counts = entry.map(|entry| {
            let text = counts_text(entry, thresholds);
            row()
                .child(labelled_as(format!("perm-{id}-counts"), text.clone(), quiet(text, cx)))
                .children(entry.suggested.then(|| {
                    labelled_as(
                        format!("perm-{id}-suggested"),
                        "Suggested: Autonomous",
                        div()
                            .px_2()
                            .rounded_full()
                            .bg(colors.done.opacity(0.15))
                            .text_xs()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(colors.done)
                            .child("Suggested: Autonomous"),
                    )
                }))
        });
        let held = entry.filter(|entry| entry.held).map(|_| {
            let (profile, tool_id) = (self.state.selected.clone(), id.to_owned());
            notice(colors.stopped, cx)
                .child(labelled_as(format!("perm-{id}-held"), HELD, div().child(HELD)))
                .child(
                    Button::new(SharedString::from(format!("perm-{id}-clear-hold")))
                        .small()
                        .outline()
                        .label("Clear the hold")
                        .on_click(cx.listener(move |this, _, _, _| {
                            if let Some(profile) = profile.clone() {
                                let tool = tool_id.clone();
                                this.send(UiCommand::ClearAutonomyHold { profile, tool });
                            }
                        })),
                )
        });
        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_3()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            // The name and the class share a line, so more tools fit on a screen.
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .font_family(cx.theme().mono_font_family.clone())
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(tool.name.clone())),
                    )
                    .child(row().children(classes)),
            )
            // The description is the MCP server's text. It is data, and it gets a small box.
            .child(
                div()
                    .max_h(px(40.))
                    .overflow_hidden()
                    .child(quiet(tool.description.clone(), cx)),
            )
            .children(lines)
            .child(labelled_as(
                format!("perm-{id}-state"),
                sentence.clone(),
                quiet(sentence, cx),
            ))
            .children(counts)
            .children(held)
            .into_any_element()
    }

    /// The permissions view: a bar with Save that stays on screen, and the list of tools below it.
    pub(crate) fn permissions_section(&self, cx: &Context<Self>) -> AnyElement {
        let form = &self.permissions;
        let colors = palette(cx);
        let Some(server) = &self.state.permissions else {
            return quiet("The rules are not loaded.", cx).into_any_element();
        };
        let thresholds_text = thresholds_text(
            self.state
                .autonomy
                .as_ref()
                .and_then(|autonomy| autonomy.thresholds.as_ref()),
        );
        let changes = form.changes();
        let changes_text = if changes.is_empty() {
            String::new()
        } else {
            // The count is in the first line: the box shows a few lines and scrolls for the others.
            let rules = if changes.len() == 1 { "rule" } else { "rules" };
            format!("Not saved: {} {rules}\n{}", changes.len(), changes.join("\n"))
        };
        let result = match &self.state.permissions_result {
            Some(Err(SaveError::Failed(error))) => Err(error.clone()),
            Some(Ok(())) if changes.is_empty() && form.message.is_none() => Ok("Saved"),
            _ => Ok(""),
        };
        let (result_text, result_color) = match &result {
            Ok(text) => ((*text).to_owned(), colors.done),
            Err(error) => (error.clone(), colors.stopped),
        };
        let bar = card(cx)
            .flex_none()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        Button::new("save-permissions")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_permissions(false, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(result_color)
                            .child(labelled("permissions-result", result_text)),
                    )
                    .child(div().text_sm().text_color(colors.stopped).child(labelled(
                        "permissions-message",
                        form.message.clone().unwrap_or_default(),
                    ))),
            )
            // The changes are listed beside Save, so a wrong click far down the list is seen before saving.
            .child(
                div()
                    .id("permissions-changes-box")
                    .max_h(px(96.))
                    .overflow_y_scroll()
                    .text_sm()
                    .text_color(colors.waiting)
                    .child(labelled("permissions-changes", changes_text)),
            )
            .children(form.conflict.then(|| {
                Self::conflict_warning(
                    "permissions",
                    PAGE_CONFLICT,
                    Self::load_permissions,
                    |this, cx| this.save_permissions(true, cx),
                    cx,
                )
            }));

        let mut groups: Vec<AnyElement> = Vec::new();
        for (connector, tools) in &server.tools {
            let rows = tools
                .iter()
                .map(|tool| self.tool_row(&format!("{connector}/{}", tool.name), tool, cx));
            groups.push(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(SharedString::from(connector.clone())),
                    )
                    .children(rows)
                    .into_any_element(),
            );
        }
        // A rule can outlive its tool: the tool is gone from the approved list, or the rule
        // came from the settings file. It is shown as it is, and it can be removed.
        let approved = |id: &str| {
            server
                .tools
                .iter()
                .any(|(connector, tools)| tools.iter().any(|tool| format!("{connector}/{}", tool.name) == id))
        };
        let orphans: Vec<AnyElement> = form
            .draft
            .iter()
            .filter(|(id, _)| !approved(id))
            .map(|(id, rule)| {
                let text = format!("{id}: {}", rule_words(Some(rule)));
                let tool_id = id.clone();
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(labelled(format!("perm-{id}-orphan"), text))
                    .child(Self::choice(
                        format!("perm-{id}-remove"),
                        "Remove the rule",
                        false,
                        cx,
                        move |this| this.permissions.set_class(&tool_id, None),
                    ))
                    .into_any_element()
            })
            .collect();
        if !orphans.is_empty() {
            // They come first: under a long list of tools, nobody would see them.
            groups.insert(
                0,
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Rules without an approved tool"),
                    )
                    .child(quiet(
                        "The agent cannot use these until their tools are approved in Settings.",
                        cx,
                    ))
                    .children(orphans)
                    .into_any_element(),
            );
        }
        if server.tools.values().all(Vec::is_empty) && groups.len() <= server.tools.len() {
            groups.push(
                labelled_as(
                    "permissions-empty",
                    "no approved tool",
                    quiet(
                        "No tool is approved yet. In Settings, press Check on a connector and approve its tools.",
                        cx,
                    ),
                )
                .into_any_element(),
            );
        }

        div()
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_col()
            .gap_3()
            .child(bar)
            .child(
                div()
                    .id("permissions-list")
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .flex_col()
                    .gap_4()
                    .overflow_y_scroll()
                    .child(labelled_as("permissions-help", HELP, quiet(HELP, cx)))
                    .child(quiet(NESTED, cx))
                    .child(labelled_as(
                        "permissions-thresholds",
                        thresholds_text.clone(),
                        quiet(thresholds_text, cx),
                    ))
                    .children(groups),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tool(schema: serde_json::Value) -> ToolSpec {
        ToolSpec {
            name: "post".into(),
            description: String::new(),
            input_schema: schema,
        }
    }

    #[test]
    fn only_string_arguments_at_the_top_can_hold_the_text() {
        let schema = json!({"type": "object", "properties": {
            "channel": {"type": "string"},
            "text": {"type": ["string", "null"]},
            "count": {"type": "integer"},
            "blocks": {"type": "array", "items": {"type": "string"}},
            "either": {"anyOf": [{"type": "string"}, {"type": "null"}]}
        }});
        assert_eq!(text_args(&tool(schema)), ["channel", "text"]);
        assert!(text_args(&tool(json!({"type": "object"}))).is_empty());
        assert!(text_args(&tool(json!("not a schema"))).is_empty());
    }

    #[test]
    fn a_new_write_starts_at_approve_without_a_text_choice() {
        let mut form = PermissionsForm::new();
        form.set_class("slack/post", Some(ToolClass::Write));
        let rule = form.draft["slack/post"].clone();
        assert_eq!((rule.tier, rule.text.clone()), (Tier::Approve, None));
        assert_eq!(rule.rule(), None);

        // The step does not follow the rule into another class. The text choice does.
        let rule = form.draft.get_mut("slack/post").unwrap();
        rule.tier = Tier::Autonomous;
        rule.text = Some(OutgoingText::Arg("text".into()));
        form.set_class("slack/post", Some(ToolClass::Delete));
        let rule = &form.draft["slack/post"];
        assert_eq!(rule.tier, Tier::Approve);
        assert_eq!(rule.text, Some(OutgoingText::Arg("text".into())));
        // After Read, a write is a new rule again.
        form.set_class("slack/post", Some(ToolClass::Read));
        assert_eq!(form.draft["slack/post"].rule(), Some(ToolRule::read()));
        form.set_class("slack/post", Some(ToolClass::Write));
        assert_eq!(form.draft["slack/post"].text, None);
        form.set_class("slack/post", None);
        assert!(form.draft.is_empty());
    }

    #[test]
    fn a_rule_is_described_in_words() {
        let delete = DraftRule {
            class: ToolClass::Delete,
            tier: Tier::Autonomous,
            text: Some(OutgoingText::None),
        };
        assert_eq!(rule_words(None), "off");
        assert_eq!(rule_words(Some(&delete)), "delete, autonomous, no text");
        // The sentence says what the server does with a delete, not what the step says.
        assert!(rule_sentence(Some(&delete)).contains("A delete always asks."));
        assert_eq!(rule_sentence(None), "The agent cannot use this tool.");
        let nested = DraftRule {
            class: ToolClass::Write,
            tier: Tier::Approve,
            text: Some(OutgoingText::Paths(vec!["text".into(), "blocks.*.text".into()])),
        };
        assert_eq!(
            rule_words(Some(&nested)),
            "write, approve, text in `text`, `blocks.*.text`"
        );
        assert!(rule_sentence(Some(&nested)).ends_with("The AI marker goes into `text`, `blocks.*.text`."));
    }

    #[test]
    fn the_counts_name_their_days_and_the_share_approved_unchanged() {
        let entry = ToolAutonomy {
            tool: "slack/post".into(),
            counts: bunsin_core::records::DecisionCounts {
                approved: 3,
                edited: 1,
                rejected: 0,
                autonomous: 2,
                autonomous_failed: 1,
            },
            held: false,
            suggested: true,
        };
        let thresholds = AutonomyThresholds {
            min_reviewed: 4,
            min_unchanged_percent: 75,
            window_days: 30,
        };
        assert_eq!(
            counts_text(&entry, Some(&thresholds)),
            "Last 30 days: 4 reviewed, 3 approved unchanged (75%), 1 edited, 0 dismissed. \
             Ran without asking: 2, failed: 1."
        );
        let none = ToolAutonomy {
            counts: Default::default(),
            ..entry
        };
        assert_eq!(
            counts_text(&none, None),
            "All days: 0 reviewed, 0 approved unchanged, 0 edited, 0 dismissed. Ran without asking: 0, failed: 0."
        );
        assert!(thresholds_text(None).contains("are off"));
        assert!(thresholds_text(Some(&thresholds)).contains("with 4 or more reviewed calls and 75% or more"));
    }

    #[test]
    fn a_click_adds_or_removes_one_argument() {
        let offered: Vec<String> = ["channel", "text", "title"].map(String::from).to_vec();
        let arg = |name: &str| OutgoingText::Arg(name.into());
        let paths = |names: &[&str]| OutgoingText::Paths(names.iter().map(|n| (*n).to_owned()).collect());
        // The first click makes the one-argument form from before.
        assert_eq!(toggle_arg(None, "text", &offered), Some(arg("text")));
        assert_eq!(
            toggle_arg(Some(&OutgoingText::None), "text", &offered),
            Some(arg("text"))
        );
        // A second argument makes a list in the order of the buttons, not of the clicks.
        let two = toggle_arg(Some(&arg("title")), "channel", &offered);
        assert_eq!(two, Some(paths(&["channel", "title"])));
        let three = toggle_arg(two.as_ref(), "text", &offered);
        assert_eq!(three, Some(paths(&["channel", "text", "title"])));
        // A click on a chosen argument removes it. Without any, nothing is chosen: not "no text".
        assert_eq!(
            toggle_arg(Some(&paths(&["text", "title"])), "title", &offered),
            Some(arg("text"))
        );
        assert_eq!(toggle_arg(Some(&arg("text")), "text", &offered), None);
        // A click while a place inside another value is chosen starts a new choice of arguments.
        assert_eq!(
            toggle_arg(Some(&paths(&["text", "blocks.*.text"])), "title", &offered),
            Some(arg("title"))
        );
    }

    #[test]
    fn only_choices_of_top_level_arguments_are_shown_as_buttons() {
        let paths = |names: &[&str]| OutgoingText::Paths(names.iter().map(|n| (*n).to_owned()).collect());
        assert_eq!(top_level(&OutgoingText::None), Some(vec![]));
        assert_eq!(
            top_level(&OutgoingText::Arg("a.b".into())),
            Some(vec!["a.b".to_owned()])
        );
        assert_eq!(
            top_level(&paths(&["text", "title"])),
            Some(vec!["text".into(), "title".into()])
        );
        assert_eq!(top_level(&paths(&["text", "blocks.*.text"])), None);
    }
}
