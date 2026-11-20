//! Principle candidates: rules the LLM finds in the owner's past texts. A candidate is only a
//! proposal. It becomes a principle only when the owner adopts it.

use crate::privacy::PLACEHOLDER_PREFIX;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// The most characters of one candidate.
pub const MAX_CANDIDATE_CHARS: usize = 300;
/// The most candidates one extraction keeps.
pub const MAX_CANDIDATES: usize = 10;

/// Where an extraction reads the owner's texts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PrincipleSource {
    /// The owner's edits of the agent's drafts and the reasons, without the retired ones.
    Corrections,
    /// The real replies of the stored replay cases.
    ReplayCases,
    /// The result of one call of a tool that reads, for example a search of the owner's sent messages.
    /// The tool needs a `read` rule and the policy must allow it now.
    Read { tool: String, args: Map<String, Value> },
}

impl PrincipleSource {
    /// A short name for the store and the audit log. It holds no argument, which can name people.
    pub fn label(&self) -> String {
        match self {
            PrincipleSource::Corrections => "corrections".to_owned(),
            PrincipleSource::ReplayCases => "replay_cases".to_owned(),
            PrincipleSource::Read { tool, .. } => format!("read:{tool}"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateState {
    New,
    Adopted,
    Dismissed,
}

impl CandidateState {
    pub fn as_str(self) -> &'static str {
        match self {
            CandidateState::New => "new",
            CandidateState::Adopted => "adopted",
            CandidateState::Dismissed => "dismissed",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [CandidateState::New, CandidateState::Adopted, CandidateState::Dismissed]
            .into_iter()
            .find(|state| state.as_str() == text)
    }
}

/// A stored candidate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrincipleCandidate {
    pub id: i64,
    pub text: String,
    /// A quote from the owner's texts that shows the rule. It passed the privacy filter.
    pub evidence: String,
    /// `PrincipleSource::label` of the extraction.
    pub source: String,
    pub state: CandidateState,
    pub created_at: i64,
    /// When the owner adopted or dismissed it.
    pub decided_at: Option<i64>,
}

/// One candidate as the LLM proposes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Proposed {
    pub text: String,
    pub evidence: String,
}

/// The reply of the LLM.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposedList {
    pub candidates: Vec<Proposed>,
}

/// The JSON Schema of the reply.
pub fn candidates_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["candidates"],
        "properties": {
            "candidates": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["text", "evidence"],
                    "properties": {
                        "text": {"type": "string"},
                        "evidence": {"type": "string"}
                    }
                }
            }
        }
    })
}

/// The text of a quote as it was before `context::quote_untrusted` escaped it.
fn unescaped(text: &str) -> String {
    text.replace("&lt;", "<").replace("&amp;", "&")
}

/// The proposed candidates that pass, in their order, at most `MAX_CANDIDATES`. Both texts of each
/// must have passed the privacy filter. A candidate is dropped if its text is empty, longer than
/// `MAX_CANDIDATE_CHARS`, or holds a `[REDACTED:...]` placeholder; if its evidence is not an exact part
/// of one of the `inputs`; or if its text repeats a candidate before it or one of `known`.
pub fn accept(proposed: Vec<Proposed>, inputs: &[String], known: &[String]) -> Vec<Proposed> {
    let mut kept: Vec<Proposed> = Vec::new();
    for candidate in proposed {
        let text = candidate.text.trim().to_owned();
        let evidence = unescaped(candidate.evidence.trim());
        let chars = text.chars().count();
        let fits = (1..=MAX_CANDIDATE_CHARS).contains(&chars) && !text.contains(PLACEHOLDER_PREFIX);
        let quoted = !evidence.is_empty() && inputs.iter().any(|input| input.contains(&evidence));
        let new = !known.contains(&text) && !kept.iter().any(|other| other.text == text);
        if fits && quoted && new {
            kept.push(Proposed { text, evidence });
        }
        if kept.len() == MAX_CANDIDATES {
            break;
        }
    }
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proposed(text: &str, evidence: &str) -> Proposed {
        Proposed {
            text: text.into(),
            evidence: evidence.into(),
        }
    }

    #[test]
    fn a_candidate_needs_a_short_clean_text_and_a_real_quote() {
        let inputs = vec!["We never deploy on Fridays. <b>Ask</b> me first & wait.".to_owned()];
        let longest = "가".repeat(MAX_CANDIDATE_CHARS);
        let kept = accept(
            vec![
                proposed(" Do not deploy on Fridays. ", "never deploy on Fridays"),
                proposed(&longest, "Ask"),
                proposed(&format!("{longest}가"), "Ask"),
                proposed("Mail [REDACTED:EMAIL] first.", "Ask"),
                proposed("Ask first.", "always ask first"),
                proposed("Wait for an answer.", ""),
                // The LLM saw the escaped quote.
                proposed("Ask before you act.", "&lt;b>Ask&lt;/b> me first &amp; wait"),
                proposed("Do not deploy on Fridays.", "Fridays"),
                proposed("Do not deploy on Fridays! ", "Fridays"),
                proposed("Keep the old rule.", "Ask"),
                proposed("   ", "Ask"),
            ],
            &inputs,
            &["Keep the old rule.".to_owned()],
        );
        let texts: Vec<&str> = kept.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Do not deploy on Fridays.",
                longest.as_str(),
                "Ask before you act.",
                "Do not deploy on Fridays!"
            ]
        );
        assert_eq!(kept[2].evidence, "<b>Ask</b> me first & wait");
    }

    #[test]
    fn at_most_ten_candidates_are_kept() {
        let inputs = vec!["Ask me first.".to_owned()];
        let many = (0..15).map(|n| proposed(&format!("Rule {n}."), "Ask")).collect();
        let kept = accept(many, &inputs, &[]);
        assert_eq!(kept.len(), MAX_CANDIDATES);
        assert_eq!(kept[9].text, "Rule 9.");
    }

    #[test]
    fn the_label_of_a_read_names_the_tool_but_not_its_arguments() {
        let read = PrincipleSource::Read {
            tool: "slack/search".into(),
            args: json!({"query": "from:kim"}).as_object().unwrap().clone(),
        };
        assert_eq!(read.label(), "read:slack/search");
        assert_eq!(PrincipleSource::Corrections.label(), "corrections");
        let parsed: PrincipleSource = serde_json::from_str(r#"{"kind": "replay_cases"}"#).unwrap();
        assert_eq!(parsed, PrincipleSource::ReplayCases);
        assert_eq!(CandidateState::parse("dismissed"), Some(CandidateState::Dismissed));
    }
}
