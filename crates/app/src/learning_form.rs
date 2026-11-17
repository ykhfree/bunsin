//! The learning view: what the agent learns from the owner, and how the owner checks it.
//!
//! - Corrections: the owner's edits of drafts. The owner retires one to stop giving it to the agent.
//! - Principle candidates: rules the LLM found in the owner's texts. Only the owner adopts one.
//! - Evaluation: past cases, a run in the background, and the report with the scores.
//! - The thresholds of the autonomy suggestions in the permissions view.

use crate::bridge::{SaveError, UiCommand, UiState};
use crate::shell::{CONFLICT, Shell, labelled, labelled_as};
use crate::theme::{card, field, palette, quiet, utc_minute};
use bunsin_api::{AdoptPrinciple, AutonomyThresholdsUpdate, EvalReport, Expected};
use bunsin_core::autonomy::AutonomyThresholds;
use bunsin_core::candidates::{CandidateState, PrincipleCandidate, PrincipleSource};
use bunsin_core::eval::{EvalResult, RunState};
use bunsin_core::records::CorrectionRecord;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input::{Input, InputState, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Sizable};
use gpui_kit::{
    AnyElement, App, AppContext, Context, Div, Entity, FontWeight, IntoElement, ParentElement, SharedString, Styled,
    Window, div, prelude::*, px,
};

const CORRECTIONS_HELP: &str = "Your edits of the agent's drafts. A new message gets the corrections of similar \
    messages. Retire one to stop giving it to the agent; it stays stored.";
const CANDIDATES_HELP: &str = "The LLM reads your texts as data and proposes rules you follow. \
    A candidate becomes a principle only when you adopt it.";
const EVAL_HELP: &str = "The agent answers past messages again without seeing your real reply. The server compares \
    the drafts without an LLM. Add cases with bunsind add-replay-cases.";
const THRESHOLDS_HELP: &str = "When the calls of a write tool meet these numbers, the permissions view suggests \
    letting it run without asking. Only you raise a step.";
const NUMBERS: &str = "Enter three whole numbers, or turn the suggestions off.";

pub struct LearningForm {
    /// The profile whose data the fields hold.
    profile: Option<String>,
    /// The candidate whose text is in the field to edit before adoption.
    editing: Option<i64>,
    text: Entity<TextareaState>,
    min_reviewed: Entity<InputState>,
    min_unchanged: Entity<InputState>,
    window_days: Entity<InputState>,
    /// The server's thresholds when the fields were loaded. A save expects the server to hold them still.
    base: Option<Option<AutonomyThresholds>>,
    /// The value of `UiState::threshold_saves` in the last state the form received.
    saves_seen: u64,
    /// The window's own message about the last save that it did not send.
    message: Option<&'static str>,
    /// True after the server refused a save because its thresholds changed since the form loaded them.
    conflict: bool,
}

/// The field texts of `thresholds`. No thresholds are empty fields.
fn texts(thresholds: Option<AutonomyThresholds>) -> [String; 3] {
    match thresholds {
        Some(t) => [
            t.min_reviewed.to_string(),
            t.min_unchanged_percent.to_string(),
            t.window_days.to_string(),
        ],
        None => Default::default(),
    }
}

/// The thresholds of the field texts: three empty fields are none, three whole numbers are thresholds.
fn parse(texts: &[String; 3]) -> Result<Option<AutonomyThresholds>, &'static str> {
    if texts.iter().all(String::is_empty) {
        return Ok(None);
    }
    let number = |text: &String| text.parse::<u32>().map_err(|_| NUMBERS);
    Ok(Some(AutonomyThresholds {
        min_reviewed: number(&texts[0])?,
        min_unchanged_percent: number(&texts[1])?,
        window_days: number(&texts[2])?,
    }))
}

/// One line of the rates of a run. A rate without cases is a dash.
pub fn rates_text(report: &EvalReport) -> String {
    let rate = |rate: Option<u32>| rate.map_or_else(|| "-".to_owned(), |rate| format!("{rate}%"));
    let s = &report.summary;
    format!(
        "Kind {} · Overlap {} · Boundary {} · Over-escalation {} · Read {} · Failed {} of {}",
        rate(s.kind_percent),
        rate(s.overlap_percent),
        rate(s.boundary_percent),
        rate(s.over_escalation_percent),
        rate(s.read_percent),
        s.errors,
        s.cases
    )
}

/// The state of the newest run.
pub fn run_text(report: &EvalReport) -> String {
    match &report.run {
        None => "No run yet.".to_owned(),
        Some(run) if run.state == RunState::Running => format!(
            "Run {}: {} of {} cases left. The server replays three cases every few seconds.",
            run.id,
            report.remaining,
            report.remaining as usize + report.results.len()
        ),
        Some(run) => format!(
            "Run {}: done at {} UTC",
            run.id,
            utc_minute(run.finished_at.unwrap_or(run.started_at))
        ),
    }
}

/// Whether the run meets the owner's pass mark.
pub fn pass_text(report: &EvalReport) -> String {
    let Some(pass) = &report.pass else {
        return "No pass mark. Set eval.pass in the settings file.".to_owned();
    };
    let verdict = match report.passed {
        Some(true) => "passed",
        Some(false) => "not passed",
        None => "not judged yet",
    };
    format!(
        "Pass mark: kind {}%, overlap {}%, boundary {}%: {verdict}",
        pass.kind_percent, pass.overlap_percent, pass.boundary_percent
    )
}

/// One line for the result of a case.
pub fn result_text(result: &EvalResult) -> String {
    let Some(score) = &result.score else {
        return format!(
            "#{} {}: failed: {}",
            result.case_id,
            result.expect.as_str(),
            result.error.as_deref().unwrap_or_default()
        );
    };
    let mut line = format!(
        "#{} {} → {} ({})",
        result.case_id,
        result.expect.as_str(),
        score.kind.as_str(),
        if score.kind_ok { "ok" } else { "wrong" }
    );
    if let Some(permille) = score.overlap_permille {
        line.push_str(&format!(", overlap {}%", permille / 10));
    }
    if let Some(judge) = &result.judge {
        line.push_str(&format!(", judge: {}", judge.verdict.as_str()));
    }
    line
}

/// One line for a correction.
pub fn correction_text(record: &CorrectionRecord) -> String {
    let c = &record.correction;
    let retired = if record.retired_at.is_some() { " (retired)" } else { "" };
    format!("#{} {}: {} → {}{retired}", record.id, c.tool, c.original, c.edited)
}

impl LearningForm {
    pub fn new(window: &mut Window, cx: &mut App) -> Self {
        let input = |placeholder: &'static str, window: &mut Window, cx: &mut App| {
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder))
        };
        Self {
            profile: None,
            editing: None,
            text: cx.new(|cx| TextareaState::new(window, cx).auto_grow(2, 6)),
            min_reviewed: input("20", window, cx),
            min_unchanged: input("90", window, cx),
            window_days: input("30", window, cx),
            base: None,
            saves_seen: 0,
            message: None,
            conflict: false,
        }
    }

    fn fields(&self, cx: &App) -> [String; 3] {
        [&self.min_reviewed, &self.min_unchanged, &self.window_days]
            .map(|input| input.read(cx).value().trim().to_owned())
    }

    fn fill(&mut self, thresholds: Option<AutonomyThresholds>, window: &mut Window, cx: &mut App) {
        let inputs = [&self.min_reviewed, &self.min_unchanged, &self.window_days];
        for (input, text) in inputs.into_iter().zip(texts(thresholds)) {
            input.update(cx, |input, cx| input.set_value(text, window, cx));
        }
        self.base = Some(thresholds);
        self.conflict = false;
    }

    /// Takes a new state from the server. Changed fields stay. The form loads the server's thresholds for
    /// another profile, after a successful save, or if the owner did not change the fields.
    pub fn receive(&mut self, state: &UiState, window: &mut Window, cx: &mut App) {
        let new = state.threshold_saves != self.saves_seen;
        self.saves_seen = state.threshold_saves;
        let saved = new && state.thresholds_result == Some(Ok(()));
        if new && state.thresholds_result == Some(Err(SaveError::Conflict)) {
            self.conflict = true;
        }
        if self.profile != state.selected {
            self.profile = state.selected.clone();
            self.base = None;
            self.message = None;
            self.conflict = false;
            self.close_edit(window, cx);
        }
        // An edited candidate that is no longer new cannot be adopted.
        let open = |id| {
            state
                .candidates
                .iter()
                .flatten()
                .any(|c| c.id == id && c.state == CandidateState::New)
        };
        if state.candidates.is_some() && self.editing.is_some_and(|id| !open(id)) {
            self.close_edit(window, cx);
        }
        let Some(server) = state.autonomy.as_ref().map(|autonomy| autonomy.thresholds) else {
            return;
        };
        let changed = self.base.is_some_and(|base| self.fields(cx) != texts(base));
        if saved || self.base.is_none() || (!changed && self.base != Some(server)) {
            self.fill(server, window, cx);
        }
    }

    fn close_edit(&mut self, window: &mut Window, cx: &mut App) {
        self.text.update(cx, |area, cx| area.set_value("", window, cx));
        self.editing = None;
    }
}

impl Shell {
    fn profile_name(&self) -> Option<String> {
        self.state.selected.clone()
    }

    /// Adopts candidate `id`, with the owner's edit if `text` is set. The adoption expects the principles
    /// that the window shows, so a change on the server meanwhile is refused.
    fn adopt(&mut self, id: i64, text: Option<String>) {
        let (Some(profile), Some(principles)) = (self.profile_name(), self.state.principles.clone()) else {
            return;
        };
        self.send(UiCommand::AdoptPrinciple {
            profile,
            adoption: AdoptPrinciple {
                id,
                text,
                expected: Expected::Value(principles),
            },
        });
    }

    /// Sends the thresholds of the fields. The server refuses them if its thresholds are no longer the base,
    /// unless `anyway` is true.
    fn save_thresholds(&mut self, anyway: bool, cx: &mut Context<Self>) {
        let form = &mut self.learning;
        let (Some(profile), Some(base)) = (form.profile.clone(), form.base) else {
            return;
        };
        let thresholds = match parse(&form.fields(cx)) {
            Ok(thresholds) => thresholds,
            Err(message) => {
                form.message = Some(message);
                return;
            }
        };
        let expected = if anyway {
            Expected::Unchecked
        } else {
            Expected::Value(base)
        };
        form.message = None;
        form.conflict = false;
        self.send(UiCommand::SaveAutonomyThresholds {
            profile,
            update: AutonomyThresholdsUpdate { thresholds, expected },
        });
    }

    fn load_thresholds(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(autonomy) = &self.state.autonomy {
            let server = autonomy.thresholds;
            self.learning.fill(server, window, cx);
        }
    }

    fn small_button(id: String, label: &'static str) -> Button {
        Button::new(SharedString::from(id)).small().label(label)
    }

    fn corrections_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Corrections"))
            .child(quiet(CORRECTIONS_HELP, cx));
        let Some(records) = &self.state.corrections else {
            return section;
        };
        let retired = records.iter().filter(|r| r.retired_at.is_some()).count();
        let count = match records.len() {
            0 => "None yet. An edit of a waiting call in the inbox is stored here.".to_owned(),
            all => format!("{all} stored, {retired} retired"),
        };
        let rows =
            records.iter().map(|record| {
                let id = record.id;
                let c = &record.correction;
                let content = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .gap_2()
                            .child(
                                div()
                                    .font_family(cx.theme().mono_font_family.clone())
                                    .child(SharedString::from(c.tool.clone())),
                            )
                            .child(quiet(
                                format!("from {} · {} UTC", c.source, utc_minute(record.created_at)),
                                cx,
                            )),
                    )
                    .child(quiet(format!("Draft: {}", c.original), cx))
                    .child(div().text_sm().child(SharedString::from(format!("Edit: {}", c.edited))))
                    .children((!c.reason.is_empty()).then(|| quiet(format!("Why: {}", c.reason), cx)));
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(labelled_as(
                        format!("correction-{id}"),
                        correction_text(record),
                        content,
                    ))
                    .child(match record.retired_at {
                        Some(_) => quiet("Retired: the agent no longer gets it.", cx).into_any_element(),
                        // The row keeps the button at its own width, on the left.
                        None => div()
                            .flex()
                            .child(Self::small_button(format!("retire-{id}"), "Retire").outline().on_click(
                                cx.listener(move |this, _, _, _| {
                                    if let Some(profile) = this.profile_name() {
                                        this.send(UiCommand::RetireCorrection { profile, id });
                                    }
                                }),
                            ))
                            .into_any_element(),
                    })
            });
        section
            .child(labelled_as(
                "corrections-count",
                format!("Corrections: {}", records.len()),
                quiet(count, cx),
            ))
            .child(div().flex().flex_col().children(rows))
    }

    fn candidate_row(&self, candidate: &PrincipleCandidate, cx: &Context<Self>) -> Div {
        let id = candidate.id;
        let row = div()
            .flex()
            .flex_col()
            .gap_1()
            .py_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(labelled(format!("candidate-{id}"), candidate.text.clone()))
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(labelled(
                format!("candidate-evidence-{id}"),
                format!("“{}”", candidate.evidence),
            )))
            .child(quiet(format!("from {}", candidate.source), cx));
        if self.learning.editing == Some(id) {
            return row
                // The text area has no ID of its own. Tests find it inside this element.
                .child(
                    div()
                        .id("candidate-text")
                        .child(Textarea::new(&self.learning.text).aria_label("Principle")),
                )
                .child(
                    div()
                        .flex()
                        .gap_2()
                        .child(
                            Button::new("adopt-edit")
                                .primary()
                                .small()
                                .label("Adopt the edit")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    let text = this.learning.text.read(cx).value().trim().to_owned();
                                    if !text.is_empty() {
                                        this.adopt(id, Some(text));
                                        this.learning.close_edit(window, cx);
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("cancel-candidate-edit")
                                .ghost()
                                .small()
                                .label("Cancel")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.learning.close_edit(window, cx);
                                    cx.notify();
                                })),
                        ),
                );
        }
        let text = candidate.text.clone();
        row.child(
            div()
                .flex()
                .gap_2()
                .child(
                    Self::small_button(format!("adopt-{id}"), "Adopt")
                        .outline()
                        .on_click(cx.listener(move |this, _, _, _| this.adopt(id, None))),
                )
                .child(
                    Self::small_button(format!("edit-candidate-{id}"), "Edit, then adopt")
                        .ghost()
                        .on_click(cx.listener(move |this, _, window, cx| {
                            let text = text.clone();
                            this.learning
                                .text
                                .update(cx, |area, cx| area.set_value(text, window, cx));
                            this.learning.editing = Some(id);
                            cx.notify();
                        })),
                )
                .child(
                    Self::small_button(format!("dismiss-{id}"), "Dismiss")
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, _| {
                            if let Some(profile) = this.profile_name() {
                                this.send(UiCommand::DismissPrinciple { profile, id });
                            }
                        })),
                ),
        )
    }

    pub(crate) fn candidates_card(&self, cx: &Context<Self>) -> Div {
        let extract = |id: &'static str, label: &'static str, source: PrincipleSource| {
            Button::new(id)
                .outline()
                .small()
                .label(label)
                .on_click(cx.listener(move |this, _, _, _| {
                    if let Some(profile) = this.profile_name() {
                        let source = source.clone();
                        this.send(UiCommand::ExtractPrinciples { profile, source });
                    }
                }))
        };
        let result = match &self.state.extract_result {
            None => String::new(),
            Some(Ok(0)) => "No new candidate.".to_owned(),
            Some(Ok(1)) => "1 new candidate.".to_owned(),
            Some(Ok(found)) => format!("{found} new candidates."),
            Some(Err(error)) => error.clone(),
        };
        let section = card(cx)
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Principle candidates"))
            .child(quiet(CANDIDATES_HELP, cx))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_2()
                    .child(extract(
                        "extract-corrections",
                        "Find in my corrections",
                        PrincipleSource::Corrections,
                    ))
                    .child(extract(
                        "extract-replay-cases",
                        "Find in my replay cases",
                        PrincipleSource::ReplayCases,
                    )),
            )
            .child(div().text_sm().child(labelled("extract-result", result)));
        let Some(candidates) = &self.state.candidates else {
            return section;
        };
        let open: Vec<&PrincipleCandidate> = candidates.iter().filter(|c| c.state == CandidateState::New).collect();
        let decided = |state| candidates.iter().filter(|c| c.state == state).count();
        let count = format!(
            "{} to decide, {} adopted, {} dismissed",
            open.len(),
            decided(CandidateState::Adopted),
            decided(CandidateState::Dismissed)
        );
        section
            .child(labelled_as(
                "candidates-count",
                format!("New candidates: {}", open.len()),
                quiet(count, cx),
            ))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .children(open.into_iter().map(|candidate| self.candidate_row(candidate, cx))),
            )
    }

    pub(crate) fn eval_card(&self, cx: &Context<Self>) -> Div {
        let section = card(cx)
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Evaluation"))
            .child(quiet(EVAL_HELP, cx));
        let (Some(cases), Some(report)) = (&self.state.replay_cases, &self.state.eval_report) else {
            return section;
        };
        let rows = cases.iter().map(|case| {
            let id = case.id;
            let label = format!("#{id} {}: {}", case.expect.as_str(), case.event.text);
            div()
                .flex()
                .items_start()
                .justify_between()
                .gap_2()
                .py_1()
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.))
                        .flex()
                        .flex_col()
                        .child(labelled(format!("case-{id}"), label))
                        .child(quiet(format!("Real reply: {}", case.actual_reply), cx)),
                )
                .child(
                    Self::small_button(format!("delete-case-{id}"), "Delete")
                        .ghost()
                        .on_click(cx.listener(move |this, _, _, _| {
                            if let Some(profile) = this.profile_name() {
                                this.send(UiCommand::DeleteReplayCase { profile, id });
                            }
                        })),
                )
        });
        let results = report.results.iter().map(|result| {
            div()
                .flex()
                .flex_col()
                .py_1()
                .child(labelled(format!("result-{}", result.case_id), result_text(result)))
                .children((!result.draft.is_empty()).then(|| quiet(format!("Draft: {}", result.draft), cx)))
        });
        let passed = match report.passed {
            Some(true) => palette(cx).done,
            Some(false) => palette(cx).stopped,
            None => cx.theme().muted_foreground,
        };
        section
            .child(labelled_as(
                "cases-count",
                format!("Replay cases: {}", cases.len()),
                quiet(
                    match cases.len() {
                        0 => "No case yet.".to_owned(),
                        1 => "1 case".to_owned(),
                        all => format!("{all} cases"),
                    },
                    cx,
                ),
            ))
            .child(div().flex().flex_col().children(rows))
            .child(
                div().flex().items_center().gap_3().child(
                    Button::new("start-eval")
                        .primary()
                        .small()
                        .label("Run the evaluation")
                        .on_click(cx.listener(|this, _, _, _| {
                            if let Some(profile) = this.profile_name() {
                                this.send(UiCommand::StartEval { profile });
                            }
                        })),
                ),
            )
            .child(div().text_sm().child(labelled("eval-state", run_text(report))))
            .children(report.run.is_some().then(|| {
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(div().text_sm().child(labelled("eval-rates", rates_text(report))))
                    .child(
                        div()
                            .text_sm()
                            .text_color(passed)
                            .child(labelled("eval-pass", pass_text(report))),
                    )
                    .child(div().flex().flex_col().children(results))
            }))
    }

    fn thresholds_card(&self, cx: &Context<Self>) -> Div {
        let form = &self.learning;
        let now = match self.state.autonomy.as_ref().map(|a| a.thresholds) {
            None => String::new(),
            Some(None) => "Now off: nothing is suggested.".to_owned(),
            Some(Some(t)) => format!(
                "Now: {} reviewed calls, {}% unchanged, last {} days",
                t.min_reviewed, t.min_unchanged_percent, t.window_days
            ),
        };
        let result = match &self.state.thresholds_result {
            Some(Err(SaveError::Failed(error))) => error.clone(),
            Some(Ok(())) => "Saved".to_owned(),
            _ => String::new(),
        };
        let input = |name: &'static str, id: &'static str, state: &Entity<InputState>| {
            field(name, Input::new(state).id(id).aria_label(name), cx).flex_1()
        };
        card(cx)
            .w(px(460.))
            .flex_none()
            .child(div().font_weight(FontWeight::SEMIBOLD).child("Autonomy suggestions"))
            .child(quiet(THRESHOLDS_HELP, cx))
            .child(div().text_sm().child(labelled("thresholds-now", now)))
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(input("Least reviewed calls", "min-reviewed", &form.min_reviewed))
                    .child(input("Least unchanged, %", "min-unchanged", &form.min_unchanged))
                    .child(input("Days counted", "window-days", &form.window_days)),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(palette(cx).stopped)
                    .child(labelled("thresholds-message", form.message.unwrap_or_default())),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("save-thresholds")
                            .primary()
                            .label("Save")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.save_thresholds(false, cx);
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("thresholds-off")
                            .outline()
                            .label("Turn suggestions off")
                            .on_click(cx.listener(|this, _, window, cx| {
                                // The base stays the one the form loaded, so the save still checks it.
                                let base = this.learning.base;
                                this.learning.fill(None, window, cx);
                                this.learning.base = base;
                                this.save_thresholds(false, cx);
                                cx.notify();
                            })),
                    )
                    .child(div().text_sm().child(labelled("thresholds-result", result))),
            )
            .children(form.conflict.then(|| {
                Self::conflict_warning(
                    "thresholds",
                    CONFLICT,
                    Self::load_thresholds,
                    |this, cx| this.save_thresholds(true, cx),
                    cx,
                )
            }))
    }

    /// The learning view: four cards that wrap and scroll together.
    pub(crate) fn learning_section(&self, cx: &Context<Self>) -> AnyElement {
        div()
            .id("learning")
            .flex_1()
            .min_h(px(0.))
            .min_w(px(0.))
            .flex()
            .flex_wrap()
            .items_start()
            .gap_5()
            .overflow_y_scroll()
            .child(self.corrections_card(cx))
            .child(self.candidates_card(cx))
            .child(self.eval_card(cx))
            .child(self.thresholds_card(cx))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_empty_fields_are_no_thresholds_and_a_partial_entry_is_refused() {
        let fields = |a: &str, b: &str, c: &str| [a.to_owned(), b.to_owned(), c.to_owned()];
        assert_eq!(parse(&fields("", "", "")), Ok(None));
        let thresholds = AutonomyThresholds {
            min_reviewed: 20,
            min_unchanged_percent: 90,
            window_days: 30,
        };
        assert_eq!(parse(&fields("20", "90", "30")), Ok(Some(thresholds)));
        assert_eq!(texts(Some(thresholds)), fields("20", "90", "30"));
        for bad in [
            fields("20", "", "30"),
            fields("20", "ninety", "30"),
            fields("-1", "90", "30"),
        ] {
            assert_eq!(parse(&bad), Err(NUMBERS), "{bad:?}");
        }
    }
}
