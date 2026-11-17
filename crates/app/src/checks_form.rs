//! The checks cards of the settings view: how the agent keeps its notes, and the owner's
//! evaluation settings. Each card saves its own part of the settings with the conflict check.

use crate::bridge::{Part, UiCommand, UiState};
use crate::part_form::PartTrack;
use crate::shell::{CONFLICT, Shell, labelled_as};
use crate::theme::{card, field, quiet};
use bunsin_api::{EvalSettingsUpdate, NotesSettingsUpdate};
use bunsin_core::eval::{EvalConfig, EvalPass};
use bunsin_core::wiki::NotesConfig;
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::{App, AppContext, Context, Div, Entity, FontWeight, ParentElement, Styled, Window, div, px};

/// What auto-apply does, and what it never does. The engine's rules in `notes.rs` decide this.
pub const AUTO_APPLY_HELP: &str = "With auto-apply, a page change of the agent is stored without asking only \
    for a message from your own channel while the agent runs. It never applies a change from a pre-load, an \
    inbound question or a webhook event, and it never overwrites a page that you wrote. Those changes wait \
    for you in the inbox.";
const NOTES_HELP: &str = "A check reports pages that nobody changed for a long time, pages that no other page \
    names, and, if you ask for it, pairs of pages that say different things. A check never changes a page.";
const EVAL_HELP: &str = "A run passes when each rate meets its mark. The LLM judge compares each draft with \
    the real reply; its verdict is shown only and changes no rate and no pass.";
const DAYS: &str = "Enter a whole number of 1 or more, or leave the field empty.";
const MARKS: &str = "Enter three whole numbers from 0 to 100, or leave all three empty.";

/// An optional whole number of a field: empty is none.
fn optional_number(text: &str) -> Result<Option<u32>, &'static str> {
    match text.trim() {
        "" => Ok(None),
        text => text.parse().map(Some).map_err(|_| DAYS),
    }
}

/// The pass mark of three field texts: three empty fields are none, three whole numbers are a mark.
fn pass_mark(texts: [&str; 3]) -> Result<Option<EvalPass>, &'static str> {
    if texts.iter().all(|text| text.trim().is_empty()) {
        return Ok(None);
    }
    let [kind, overlap, boundary] = texts.map(|text| text.trim().parse::<u32>().map_err(|_| MARKS));
    Ok(Some(EvalPass {
        kind_percent: kind?,
        overlap_percent: overlap?,
        boundary_percent: boundary?,
    }))
}

/// The text of a field for an optional number.
fn number_text(value: Option<u32>) -> String {
    value.map(|n| n.to_string()).unwrap_or_default()
}

fn read(input: &Entity<InputState>, cx: &App) -> String {
    input.read(cx).value().to_string()
}

fn set(input: &Entity<InputState>, value: String, window: &mut Window, cx: &mut App) {
    input.update(cx, |input, cx| input.set_value(value, window, cx));
}

pub struct ChecksForm {
    notes: PartTrack<NotesConfig>,
    auto_apply: bool,
    lint_conflicts: bool,
    stale_days: Entity<InputState>,
    lint_hours: Entity<InputState>,
    eval: PartTrack<EvalConfig>,
    judge: bool,
    marks: [Entity<InputState>; 3],
}

impl ChecksForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            notes: PartTrack::new(Part::Notes),
            auto_apply: false,
            lint_conflicts: false,
            stale_days: input("Empty: no page is stale", window, cx),
            lint_hours: input("Empty: only when you start a check", window, cx),
            eval: PartTrack::new(Part::Eval),
            judge: false,
            marks: [
                input("For example 80", window, cx),
                input("For example 40", window, cx),
                input("For example 100", window, cx),
            ],
        }
    }

    fn notes_value(&self, cx: &App) -> Result<NotesConfig, &'static str> {
        Ok(NotesConfig {
            auto_apply: self.auto_apply,
            stale_days: optional_number(&read(&self.stale_days, cx))?,
            lint_every_hours: optional_number(&read(&self.lint_hours, cx))?,
            lint_conflicts: self.lint_conflicts,
        })
    }

    fn fill_notes(&mut self, notes: &NotesConfig, window: &mut Window, cx: &mut App) {
        self.auto_apply = notes.auto_apply;
        self.lint_conflicts = notes.lint_conflicts;
        set(&self.stale_days, number_text(notes.stale_days), window, cx);
        set(&self.lint_hours, number_text(notes.lint_every_hours), window, cx);
    }

    fn eval_value(&self, cx: &App) -> Result<EvalConfig, &'static str> {
        let [kind, overlap, boundary] = self.marks.each_ref().map(|input| read(input, cx));
        Ok(EvalConfig {
            pass: pass_mark([&kind, &overlap, &boundary])?,
            judge: self.judge,
        })
    }

    fn fill_eval(&mut self, eval: &EvalConfig, window: &mut Window, cx: &mut App) {
        self.judge = eval.judge;
        let values = match eval.pass {
            Some(pass) => [
                Some(pass.kind_percent),
                Some(pass.overlap_percent),
                Some(pass.boundary_percent),
            ],
            None => [None; 3],
        };
        for (input, value) in self.marks.iter().zip(values) {
            set(input, number_text(value), window, cx);
        }
    }

    fn notes_changed(&self, cx: &App) -> bool {
        self.notes
            .base
            .as_ref()
            .is_some_and(|base| self.notes_value(cx).as_ref() != Ok(base))
    }

    fn eval_changed(&self, cx: &App) -> bool {
        self.eval
            .base
            .as_ref()
            .is_some_and(|base| self.eval_value(cx).as_ref() != Ok(base))
    }

    /// Takes a new state from the server. A card keeps what the owner changed, and loads the
    /// server's part otherwise.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let changed = self.notes_changed(cx);
        if let Some(server) = self.notes.receive(state, state.notes_settings.as_ref(), changed) {
            self.fill_notes(&server, window, cx);
        }
        let changed = self.eval_changed(cx);
        if let Some(server) = self.eval.receive(state, state.eval_settings.as_ref(), changed) {
            self.fill_eval(&server, window, cx);
        }
    }
}

impl Shell {
    fn save_notes_settings(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.checks;
        let notes = match form.notes_value(cx) {
            Ok(notes) => notes,
            Err(message) => {
                form.notes.message = Some(message);
                return;
            }
        };
        form.notes.message = None;
        let Some((profile, expected)) = form.notes.save_target(anyway) else {
            return;
        };
        self.send(UiCommand::SaveNotesSettings {
            profile,
            update: NotesSettingsUpdate { notes, expected },
        });
    }

    fn load_notes_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(server) = self.state.notes_settings.clone() {
            self.checks.notes.load(&server);
            self.checks.fill_notes(&server, window, cx);
        }
    }

    fn save_eval_settings(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.checks;
        let eval = match form.eval_value(cx) {
            Ok(eval) => eval,
            Err(message) => {
                form.eval.message = Some(message);
                return;
            }
        };
        form.eval.message = None;
        let Some((profile, expected)) = form.eval.save_target(anyway) else {
            return;
        };
        self.send(UiCommand::SaveEvalSettings {
            profile,
            update: EvalSettingsUpdate { eval, expected },
        });
    }

    fn load_eval_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(server) = self.state.eval_settings.clone() {
            self.checks.eval.load(&server);
            self.checks.fill_eval(&server, window, cx);
        }
    }

    fn notes_settings_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(470.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Notes"))
            .child(labelled_as(
                "auto-apply-help",
                AUTO_APPLY_HELP,
                quiet(AUTO_APPLY_HELP, cx),
            ));
        let form = &self.checks;
        if form.notes.base.is_none() {
            return section;
        }
        let toggle = |id: &'static str, label: &'static str, on: bool, set: fn(&mut ChecksForm, bool)| {
            Checkbox::new(id)
                .label(label)
                .checked(on)
                .on_change(cx.listener(move |this, on: &bool, _, cx| {
                    set(&mut this.checks, *on);
                    cx.notify();
                }))
        };
        let changed = form.notes_changed(cx);
        section
            .child(toggle(
                "notes-auto-apply",
                "Auto-apply: store the agent's page changes without asking",
                form.auto_apply,
                |f, on| f.auto_apply = on,
            ))
            .child(labelled_as("notes-help", NOTES_HELP, quiet(NOTES_HELP, cx)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        field(
                            "Days until a page is stale",
                            Input::new(&form.stale_days)
                                .id("notes-stale-days")
                                .aria_label("Stale days"),
                            cx,
                        )
                        .flex_1(),
                    )
                    .child(
                        field(
                            "Check every this many hours",
                            Input::new(&form.lint_hours)
                                .id("notes-lint-hours")
                                .aria_label("Check hours"),
                            cx,
                        )
                        .flex_1(),
                    ),
            )
            .child(toggle(
                "notes-lint-conflicts",
                "Ask the LLM about pages that disagree (one LLM call per check)",
                form.lint_conflicts,
                |f, on| f.lint_conflicts = on,
            ))
            .child(self.save_bar("notes-settings", &form.notes, changed, Self::save_notes_settings, cx))
            .children(form.notes.conflict.then(|| {
                Self::conflict_warning(
                    "notes-settings",
                    CONFLICT,
                    Self::load_notes_settings,
                    |this, cx| this.save_notes_settings(true, cx),
                    cx,
                )
            }))
    }

    fn eval_settings_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(470.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Evaluation"))
            .child(labelled_as("eval-settings-help", EVAL_HELP, quiet(EVAL_HELP, cx)));
        let form = &self.checks;
        if form.eval.base.is_none() {
            return section;
        }
        let [kind, overlap, boundary] = &form.marks;
        let mark = |name: &'static str, id: &'static str, state: &Entity<InputState>| {
            field(name, Input::new(state).id(id).aria_label(name), cx).flex_1()
        };
        let changed = form.eval_changed(cx);
        section
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(mark("Kind, %", "pass-kind", kind))
                    .child(mark("Overlap, %", "pass-overlap", overlap))
                    .child(mark("Boundary, %", "pass-boundary", boundary)),
            )
            .child(
                Checkbox::new("eval-judge")
                    .label("Ask the LLM judge (one LLM call per case)")
                    .checked(form.judge)
                    .on_change(cx.listener(|this, on: &bool, _, cx| {
                        this.checks.judge = *on;
                        cx.notify();
                    })),
            )
            .child(self.save_bar("eval-settings", &form.eval, changed, Self::save_eval_settings, cx))
            .children(form.eval.conflict.then(|| {
                Self::conflict_warning(
                    "eval-settings",
                    CONFLICT,
                    Self::load_eval_settings,
                    |this, cx| this.save_eval_settings(true, cx),
                    cx,
                )
            }))
    }

    /// The checks cards of the settings view.
    pub(crate) fn checks_cards(&self, cx: &Context<Self>) -> [Div; 2] {
        [self.notes_settings_card(cx), self.eval_settings_card(cx)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_field_is_no_number_and_text_is_refused() {
        assert_eq!(optional_number(""), Ok(None));
        assert_eq!(optional_number(" 30 "), Ok(Some(30)));
        assert_eq!(optional_number("thirty"), Err(DAYS));
        assert_eq!(optional_number("-1"), Err(DAYS));
        assert_eq!(number_text(Some(7)), "7");
        assert_eq!(number_text(None), "");
    }

    #[test]
    fn three_empty_marks_are_no_pass_mark_and_a_partial_mark_is_refused() {
        assert_eq!(pass_mark(["", " ", ""]), Ok(None));
        assert_eq!(
            pass_mark(["80", "40", "100"]),
            Ok(Some(EvalPass {
                kind_percent: 80,
                overlap_percent: 40,
                boundary_percent: 100
            }))
        );
        assert_eq!(pass_mark(["80", "", "100"]), Err(MARKS));
        assert_eq!(pass_mark(["80", "x", "100"]), Err(MARKS));
    }
}
