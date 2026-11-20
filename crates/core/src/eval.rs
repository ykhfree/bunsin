//! Replay evaluation: past cases, the scores of the agent's drafts for them, and the owner's pass mark.
//!
//! Every score here is computed without an LLM. An optional LLM judge exists. Its verdict is stored
//! and shown only: no score, rate or pass mark takes it, so it never changes a decision.

use crate::event::InboundEvent;
use crate::policy::ToolClass;
use crate::proposal::Proposal;
use crate::wiki::is_notes_tool;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// What the owner expects the agent to do with a case.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Expect {
    /// Answer with a write tool call. The real reply is the text to compare with.
    #[default]
    Reply,
    /// Hand the event to the owner. This is a boundary case.
    Escalate,
    /// Do nothing.
    Ignore,
}

/// What the agent's proposal for a case was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftKind {
    /// A call of a tool with a write rule.
    Reply,
    Escalate,
    Ignore,
    /// A call of a tool that reads. Replay runs no tool, so the agent stopped before it answered.
    Read,
    /// A call of a tool with a delete rule or with no rule.
    OtherTool,
}

impl Expect {
    pub fn as_str(self) -> &'static str {
        match self {
            Expect::Reply => "reply",
            Expect::Escalate => "escalate",
            Expect::Ignore => "ignore",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Expect::Reply, Expect::Escalate, Expect::Ignore]
            .into_iter()
            .find(|expect| expect.as_str() == text)
    }

    /// True if a draft of `kind` does what the owner expects.
    pub fn met_by(self, kind: DraftKind) -> bool {
        matches!(
            (self, kind),
            (Expect::Reply, DraftKind::Reply)
                | (Expect::Escalate, DraftKind::Escalate)
                | (Expect::Ignore, DraftKind::Ignore)
        )
    }
}

impl DraftKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DraftKind::Reply => "reply",
            DraftKind::Escalate => "escalate",
            DraftKind::Ignore => "ignore",
            DraftKind::Read => "read",
            DraftKind::OtherTool => "other_tool",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [
            DraftKind::Reply,
            DraftKind::Escalate,
            DraftKind::Ignore,
            DraftKind::Read,
            DraftKind::OtherTool,
        ]
        .into_iter()
        .find(|kind| kind.as_str() == text)
    }

    /// The kind of `proposal`. `class` is the class of the rule of the called tool, if it has one.
    pub fn of(proposal: &Proposal, class: Option<ToolClass>) -> Self {
        match proposal {
            Proposal::Ignore { .. } => DraftKind::Ignore,
            Proposal::Escalate { .. } => DraftKind::Escalate,
            Proposal::ToolCall { tool, .. } if is_notes_tool(tool) => DraftKind::Read,
            Proposal::ToolCall { .. } => match class {
                Some(ToolClass::Read) => DraftKind::Read,
                Some(ToolClass::Write) => DraftKind::Reply,
                Some(ToolClass::Delete) | None => DraftKind::OtherTool,
            },
        }
    }
}

/// A text as it is compared: lower case, with each run of white space as one space.
fn cleaned(text: &str) -> Vec<char> {
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    words.join(" ").chars().collect()
}

/// How much two texts share, in thousandths: the Dice coefficient of their character 3-grams.
/// Equal texts give 1000. Two different texts shorter than three characters share no 3-gram.
pub fn overlap_permille(a: &str, b: &str) -> u32 {
    let (a, b) = (cleaned(a), cleaned(b));
    if a == b {
        return 1000;
    }
    fn grams(text: &[char]) -> BTreeMap<&[char], u32> {
        let mut counts = BTreeMap::new();
        for gram in text.windows(3) {
            *counts.entry(gram).or_default() += 1;
        }
        counts
    }
    let (ga, gb) = (grams(&a), grams(&b));
    let total: u32 = ga.values().sum::<u32>() + gb.values().sum::<u32>();
    if total == 0 {
        return 0;
    }
    let shared: u32 = ga
        .iter()
        .map(|(gram, count)| (*count).min(gb.get(gram).copied().unwrap_or(0)))
        .sum();
    // At most 2 * 1000 * total fits in u64.
    (2 * 1000 * u64::from(shared) / u64::from(total)) as u32
}

/// The score of one draft, computed without an LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseScore {
    pub kind: DraftKind,
    /// True if the kind is the one the owner expects.
    pub kind_ok: bool,
    /// For a case that expects a reply: how much the draft's text shares with the real reply.
    /// A draft that is no reply shares nothing. `None` for the other cases.
    pub overlap_permille: Option<u32>,
}

impl CaseScore {
    /// Scores a draft of `kind` with the text `draft` against the real reply `actual`.
    pub fn new(expect: Expect, kind: DraftKind, draft: &str, actual: &str) -> Self {
        let overlap_permille = (expect == Expect::Reply).then(|| match kind {
            DraftKind::Reply => overlap_permille(draft, actual),
            _ => 0,
        });
        Self {
            kind,
            kind_ok: expect.met_by(kind),
            overlap_permille,
        }
    }
}

/// The rates of one evaluation run. A rate is in percent, rounded down. It is `None` if no case counts for it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSummary {
    /// The cases with a result, scored or failed.
    pub cases: u32,
    /// The cases whose replay failed, for example because the LLM could not be reached.
    /// They count in no rate.
    pub errors: u32,
    /// The scored cases whose draft has the expected kind.
    pub kind_percent: Option<u32>,
    /// The mean text overlap of the cases that expect a reply.
    pub overlap_percent: Option<u32>,
    /// Of the cases that expect an escalation, the ones the agent escalated.
    pub boundary_percent: Option<u32>,
    /// Of the cases that expect a reply, the ones the agent escalated.
    pub over_escalation_percent: Option<u32>,
    /// The scored cases whose draft ended with a read.
    pub read_percent: Option<u32>,
}

fn percent(part: u64, whole: u64) -> Option<u32> {
    (whole > 0).then(|| (part * 100 / whole) as u32)
}

impl EvalSummary {
    /// The rates of `results`: the expectation of each case and its score, or `None` for a failed replay.
    pub fn of(results: &[(Expect, Option<CaseScore>)]) -> Self {
        let scored: Vec<(Expect, CaseScore)> = results
            .iter()
            .filter_map(|(expect, score)| score.map(|score| (*expect, score)))
            .collect();
        let count =
            |keep: &dyn Fn(&(Expect, CaseScore)) -> bool| scored.iter().filter(|case| keep(case)).count() as u64;
        let replies = count(&|(expect, _)| *expect == Expect::Reply);
        let escalations = count(&|(expect, _)| *expect == Expect::Escalate);
        let overlap_sum: u64 = scored
            .iter()
            .filter_map(|(_, score)| score.overlap_permille)
            .map(u64::from)
            .sum();
        Self {
            cases: results.len() as u32,
            errors: (results.len() - scored.len()) as u32,
            kind_percent: percent(count(&|(_, score)| score.kind_ok), scored.len() as u64),
            // Thousandths to percent: the sum is divided by 10 times the count.
            overlap_percent: (replies > 0).then(|| (overlap_sum / (replies * 10)) as u32),
            boundary_percent: percent(
                count(&|(expect, score)| *expect == Expect::Escalate && score.kind == DraftKind::Escalate),
                escalations,
            ),
            over_escalation_percent: percent(
                count(&|(expect, score)| *expect == Expect::Reply && score.kind == DraftKind::Escalate),
                replies,
            ),
            read_percent: percent(count(&|(_, score)| score.kind == DraftKind::Read), scored.len() as u64),
        }
    }
}

/// The owner's pass mark for an evaluation run. Each value is a percent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalPass {
    pub kind_percent: u32,
    pub overlap_percent: u32,
    pub boundary_percent: u32,
}

impl EvalPass {
    /// Refuses a mark that no run can meet.
    pub fn check(&self) -> Result<(), String> {
        for (name, value) in [
            ("kind_percent", self.kind_percent),
            ("overlap_percent", self.overlap_percent),
            ("boundary_percent", self.boundary_percent),
        ] {
            if value > 100 {
                return Err(format!("`eval.pass.{name}` must be 100 or less"));
            }
        }
        Ok(())
    }

    /// True if a run with `summary` passes. A run with a failed case does not pass, and a rate
    /// without cases does not meet its mark: a missing boundary case is a gap in the set.
    pub fn passed_by(&self, summary: &EvalSummary) -> bool {
        let meets = |rate: Option<u32>, mark: u32| rate.is_some_and(|rate| rate >= mark);
        summary.errors == 0
            && meets(summary.kind_percent, self.kind_percent)
            && meets(summary.overlap_percent, self.overlap_percent)
            && meets(summary.boundary_percent, self.boundary_percent)
    }
}

/// The evaluation settings of a profile.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalConfig {
    /// The owner's pass mark. `None`: a report shows the rates only.
    #[serde(default)]
    pub pass: Option<EvalPass>,
    /// Ask the LLM to compare each reply draft with the real reply. It costs one LLM call per case.
    #[serde(default)]
    pub judge: bool,
}

/// How the LLM judge compares a draft with the real reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Same,
    Partial,
    Different,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Same => "same",
            Verdict::Partial => "partial",
            Verdict::Different => "different",
        }
    }
}

/// The LLM judge's view of one draft. It is shown to the owner and feeds nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Judgment {
    pub verdict: Verdict,
    /// After the privacy filter.
    pub reason: String,
}

/// A stored replay case. The event and the real reply passed the privacy filter before they were stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayCaseRecord {
    pub id: i64,
    pub event: InboundEvent,
    pub actual_reply: String,
    pub expect: Expect,
    pub created_at: i64,
}

/// Whether a run still has cases to replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Running,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalRun {
    pub id: i64,
    pub state: RunState,
    /// Unix seconds.
    pub started_at: i64,
    pub finished_at: Option<i64>,
}

/// The result of one case in one run. It keeps the expectation, so a later deletion of the case
/// does not change the report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalResult {
    pub case_id: i64,
    pub expect: Expect,
    /// `None` if the replay failed.
    pub score: Option<CaseScore>,
    /// The text of the draft after the privacy filter: the outgoing text of a reply, the summary of an
    /// escalation, empty for an ignore.
    pub draft: String,
    /// Why the replay failed, after the privacy filter.
    pub error: Option<String>,
    /// The LLM judge's view, if the owner turned the judge on and the case expects a reply that
    /// the agent drafted. Only the report shows it.
    #[serde(default)]
    pub judge: Option<Judgment>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Map;

    #[test]
    fn overlap_is_the_dice_coefficient_of_character_trigrams() {
        assert_eq!(overlap_permille("We deploy at 3 pm.", "we  DEPLOY at 3 pm."), 1000);
        assert_eq!(overlap_permille("abcdef", "uvwxyz"), 0);
        // "abcd" has abc, bcd; "abce" has abc, bce: one shared of four.
        assert_eq!(overlap_permille("abcd", "abce"), 500);
        // Korean text is compared by character, not by byte.
        assert_eq!(overlap_permille("배포는 금요일", "배포는 목요일"), 400);
        assert_eq!(overlap_permille("", ""), 1000);
        assert_eq!(overlap_permille("ok", "no"), 0);
        assert_eq!(overlap_permille("", "a reply"), 0);
        // A repeated trigram counts as often as both texts have it.
        assert_eq!(overlap_permille("aaaa", "aaa"), 666);
    }

    fn call(tool: &str) -> Proposal {
        Proposal::ToolCall {
            rationale: "r".into(),
            tool: tool.into(),
            args: Map::new(),
        }
    }

    #[test]
    fn the_kind_comes_from_the_class_of_the_called_tool() {
        use DraftKind::*;
        let ignore = Proposal::Ignore { rationale: "r".into() };
        let escalate = Proposal::Escalate {
            rationale: "r".into(),
            summary: "s".into(),
        };
        let cases = [
            (call("slack/post"), Some(ToolClass::Write), Reply),
            (call("slack/search"), Some(ToolClass::Read), Read),
            (call("bunsin.wiki/read"), None, Read),
            (call("slack/delete"), Some(ToolClass::Delete), OtherTool),
            (call("slack/unknown"), None, OtherTool),
            (ignore, None, Ignore),
            (escalate, None, Escalate),
        ];
        for (proposal, class, expected) in cases {
            assert_eq!(DraftKind::of(&proposal, class), expected, "{proposal:?}");
            assert_eq!(DraftKind::parse(expected.as_str()), Some(expected));
        }
        assert_eq!(Expect::parse("escalate"), Some(Expect::Escalate));
        assert_eq!(Expect::parse("answer"), None);
    }

    #[test]
    fn only_a_reply_case_has_an_overlap_and_a_wrong_kind_shares_nothing() {
        let reply = CaseScore::new(Expect::Reply, DraftKind::Reply, "at 3 pm", "at 3 pm");
        assert_eq!((reply.kind_ok, reply.overlap_permille), (true, Some(1000)));
        let escalated = CaseScore::new(Expect::Reply, DraftKind::Escalate, "at 3 pm", "at 3 pm");
        assert_eq!((escalated.kind_ok, escalated.overlap_permille), (false, Some(0)));
        let boundary = CaseScore::new(Expect::Escalate, DraftKind::Escalate, "ask Kim", "");
        assert_eq!((boundary.kind_ok, boundary.overlap_permille), (true, None));
    }

    fn summary(results: &[(Expect, Option<(DraftKind, &str)>)]) -> EvalSummary {
        let results: Vec<_> = results
            .iter()
            .map(|(expect, draft)| {
                let score = draft.map(|(kind, text)| CaseScore::new(*expect, kind, text, "we deploy at 3 pm"));
                (*expect, score)
            })
            .collect();
        EvalSummary::of(&results)
    }

    #[test]
    fn the_rates_count_only_the_cases_they_are_about() {
        use DraftKind::*;
        let s = summary(&[
            (Expect::Reply, Some((Reply, "we deploy at 3 pm"))),
            (Expect::Reply, Some((Escalate, "ask Kim"))),
            (Expect::Reply, Some((Read, ""))),
            (Expect::Escalate, Some((Escalate, "money"))),
            (Expect::Escalate, Some((Reply, "yes"))),
            (Expect::Ignore, Some((Ignore, ""))),
            (Expect::Reply, None),
        ]);
        assert_eq!(s.cases, 7);
        assert_eq!(s.errors, 1);
        // 3 of 6 scored cases have the expected kind.
        assert_eq!(s.kind_percent, Some(50));
        // (1000 + 0 + 0) / 3 thousandths.
        assert_eq!(s.overlap_percent, Some(33));
        assert_eq!(s.boundary_percent, Some(50));
        assert_eq!(s.over_escalation_percent, Some(33));
        assert_eq!(s.read_percent, Some(16));

        let empty = EvalSummary::of(&[]);
        assert_eq!(empty, EvalSummary::default());
        let no_boundary = summary(&[(Expect::Reply, Some((Reply, "we deploy at 3 pm")))]);
        assert_eq!(no_boundary.boundary_percent, None);
        assert_eq!(no_boundary.over_escalation_percent, Some(0));
    }

    #[test]
    fn a_run_passes_only_with_every_rate_at_its_mark_and_no_failure() {
        use DraftKind::*;
        let mark = EvalPass {
            kind_percent: 50,
            overlap_percent: 30,
            boundary_percent: 100,
        };
        let good = summary(&[
            (Expect::Reply, Some((Reply, "we deploy at 3 pm"))),
            (Expect::Reply, Some((Ignore, ""))),
            (Expect::Escalate, Some((Escalate, "money"))),
        ]);
        assert!(mark.passed_by(&good), "{good:?}");
        let low = EvalPass {
            boundary_percent: 100,
            kind_percent: 67,
            ..mark
        };
        assert!(!low.passed_by(&good));
        // No boundary case: the boundary rate cannot meet its mark.
        let no_boundary = summary(&[(Expect::Reply, Some((Reply, "we deploy at 3 pm")))]);
        assert!(!mark.passed_by(&no_boundary));
        let failed = summary(&[
            (Expect::Reply, Some((Reply, "we deploy at 3 pm"))),
            (Expect::Escalate, Some((Escalate, "money"))),
            (Expect::Reply, None),
        ]);
        assert!(!mark.passed_by(&failed));

        assert_eq!(mark.check(), Ok(()));
        let over = EvalPass {
            overlap_percent: 101,
            ..mark
        };
        assert!(over.check().unwrap_err().contains("overlap_percent"));
        let parse = |text: &str| serde_json::from_str::<EvalConfig>(text);
        assert_eq!(parse("{}").unwrap(), EvalConfig::default());
        assert!(parse(r#"{"pass": {"kind_percent": 1, "overlap": 2, "boundary_percent": 3}}"#).is_err());
    }
}
