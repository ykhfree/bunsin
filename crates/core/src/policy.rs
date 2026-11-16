use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How much the agent may do with a write tool. The user raises it step by step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Record the proposal only.
    Observe,
    /// Show the draft to the user. The agent never runs it.
    Draft,
    /// Run after the user approves.
    Approve,
    /// Run without asking.
    Autonomous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolClass {
    Read,
    Write,
    Delete,
}

/// Where a tool carries text that other people will read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingText {
    /// The tool sends no text to other people.
    None,
    /// The named string argument holds the text. The AI marker is added to it.
    /// The name is one argument at the top of the input, also if it contains a dot.
    Arg(String),
    /// Each path names a place that holds text. The AI marker is added to every one.
    /// A path is a list of argument names joined by `.`. The step `*` means every item of a list,
    /// so `blocks.*.text` is the `text` of each item of `blocks`.
    Paths(Vec<String>),
}

impl OutgoingText {
    /// The places that get the AI marker, as the rule spells them.
    pub fn locations(&self) -> Vec<&str> {
        match self {
            OutgoingText::None => Vec::new(),
            OutgoingText::Arg(name) => vec![name],
            OutgoingText::Paths(paths) => paths.iter().map(String::as_str).collect(),
        }
    }

    /// Refuses a choice that names no place or a path that cannot name a place.
    pub fn check(&self) -> Result<(), String> {
        let OutgoingText::Paths(paths) = self else {
            return Ok(());
        };
        if paths.is_empty() {
            return Err("`paths` names no place: use \"none\" for a tool that sends no text".into());
        }
        for path in paths {
            let steps: Vec<&str> = path.split('.').collect();
            if steps.iter().any(|step| step.is_empty()) || steps[0] == "*" {
                return Err(format!(
                    "the path `{path}` is invalid: join argument names with `.`, start with a name, \
                     and use `*` for every item of a list"
                ));
            }
        }
        Ok(())
    }
}

/// One row of the permission table. Every field is required, so a rule cannot
/// leave out by accident where the AI marker goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRule {
    pub class: ToolClass,
    pub tier: Tier,
    pub outgoing_text: OutgoingText,
}

impl ToolRule {
    pub fn read() -> Self {
        Self {
            class: ToolClass::Read,
            tier: Tier::Autonomous,
            outgoing_text: OutgoingText::None,
        }
    }

    /// A new write or delete rule starts at "run after approval".
    pub fn write(class: ToolClass, outgoing_text: OutgoingText) -> Self {
        Self {
            class,
            tier: Tier::Approve,
            outgoing_text,
        }
    }
}

/// The permission table of one profile. Keys are tool IDs: `<connector>/<tool>`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    pub rules: BTreeMap<String, ToolRule>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyReason {
    /// The tool is not in the permission table.
    UnknownTool,
    /// The connector's tool list differs from the list the user approved.
    PinChanged,
    /// The kill switch is on.
    KillSwitch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Execute,
    NeedsApproval,
    DraftOnly,
    RecordOnly,
    Deny(DenyReason),
}

impl Policy {
    /// Decides what happens to a proposed call of `tool_id`.
    ///
    /// The inputs are facts the harness measured. Nothing the LLM or a connector
    /// says about the tool is an input. `held` is true while the owner's hold keeps the tool
    /// at approval: then a call that its step would run asks the owner.
    pub fn decide(&self, tool_id: &str, pin_holds: bool, kill_switch: bool, held: bool) -> Decision {
        let Some(rule) = self.rules.get(tool_id) else {
            return Decision::Deny(DenyReason::UnknownTool);
        };
        if !pin_holds {
            return Decision::Deny(DenyReason::PinChanged);
        }
        if rule.class == ToolClass::Read {
            return Decision::Execute;
        }
        if kill_switch {
            return Decision::Deny(DenyReason::KillSwitch);
        }
        match (rule.tier, rule.class) {
            (Tier::Observe, _) => Decision::RecordOnly,
            (Tier::Draft, _) => Decision::DraftOnly,
            (Tier::Approve, _) => Decision::NeedsApproval,
            // A delete is never autonomous.
            (Tier::Autonomous, ToolClass::Delete) => Decision::NeedsApproval,
            (Tier::Autonomous, _) if held => Decision::NeedsApproval,
            (Tier::Autonomous, _) => Decision::Execute,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(entries: &[(&str, ToolClass, Tier)]) -> Policy {
        Policy {
            rules: entries
                .iter()
                .map(|(id, class, tier)| {
                    (
                        (*id).to_owned(),
                        ToolRule {
                            class: *class,
                            tier: *tier,
                            outgoing_text: OutgoingText::None,
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn decision_table() {
        use Decision::*;
        use ToolClass::*;
        // (class, tier, pin_holds, kill_switch, held, expected)
        let cases = [
            (Read, Tier::Autonomous, true, false, false, Execute),
            (Read, Tier::Observe, true, false, false, Execute),
            (Read, Tier::Autonomous, true, true, false, Execute),
            (
                Read,
                Tier::Autonomous,
                false,
                false,
                false,
                Deny(DenyReason::PinChanged),
            ),
            (Write, Tier::Observe, true, false, false, RecordOnly),
            (Write, Tier::Draft, true, false, false, DraftOnly),
            (Write, Tier::Approve, true, false, false, NeedsApproval),
            (Write, Tier::Autonomous, true, false, false, Execute),
            (Write, Tier::Autonomous, true, true, false, Deny(DenyReason::KillSwitch)),
            (Write, Tier::Observe, true, true, false, Deny(DenyReason::KillSwitch)),
            (
                Write,
                Tier::Autonomous,
                false,
                false,
                false,
                Deny(DenyReason::PinChanged),
            ),
            (Delete, Tier::Approve, true, false, false, NeedsApproval),
            (Delete, Tier::Autonomous, true, false, false, NeedsApproval),
            (
                Delete,
                Tier::Autonomous,
                true,
                true,
                false,
                Deny(DenyReason::KillSwitch),
            ),
            // A hold keeps an autonomous write at approval. It changes no lower step.
            (Write, Tier::Autonomous, true, false, true, NeedsApproval),
            (Write, Tier::Draft, true, false, true, DraftOnly),
            (Write, Tier::Autonomous, true, true, true, Deny(DenyReason::KillSwitch)),
            (
                Write,
                Tier::Autonomous,
                false,
                false,
                true,
                Deny(DenyReason::PinChanged),
            ),
        ];
        for (class, tier, pin_holds, kill_switch, held, expected) in cases {
            let p = policy(&[("slack/tool", class, tier)]);
            assert_eq!(
                p.decide("slack/tool", pin_holds, kill_switch, held),
                expected,
                "{class:?} {tier:?} pin={pin_holds} kill={kill_switch} held={held}"
            );
        }
    }

    #[test]
    fn a_tool_outside_the_table_is_denied() {
        let p = policy(&[("slack/post", ToolClass::Write, Tier::Autonomous)]);
        assert_eq!(
            p.decide("slack/delete_channel", true, false, false),
            Decision::Deny(DenyReason::UnknownTool)
        );
        // The same tool name under another connector is another tool.
        assert_eq!(
            p.decide("jira/post", true, false, false),
            Decision::Deny(DenyReason::UnknownTool)
        );
    }

    #[test]
    fn a_new_write_rule_needs_approval() {
        let mut p = Policy::default();
        p.rules.insert(
            "slack/post".into(),
            ToolRule::write(ToolClass::Write, OutgoingText::Arg("text".into())),
        );
        assert_eq!(p.decide("slack/post", true, false, false), Decision::NeedsApproval);
    }

    #[test]
    fn a_rule_must_say_where_the_outgoing_text_is() {
        let parse = |text: &str| serde_json::from_str::<ToolRule>(text);
        let rule = parse(r#"{"class": "write", "tier": "approve", "outgoing_text": {"arg": "text"}}"#).unwrap();
        assert_eq!(rule.outgoing_text, OutgoingText::Arg("text".into()));
        let silent = parse(r#"{"class": "write", "tier": "approve", "outgoing_text": "none"}"#).unwrap();
        assert_eq!(silent.outgoing_text, OutgoingText::None);
        // Leaving the field out is an error, not "no marker".
        assert!(parse(r#"{"class": "write", "tier": "approve"}"#).is_err());
        // A misspelled field is an error too.
        assert!(
            parse(r#"{"class": "write", "tier": "approve", "outgoing_text": "none", "text_args": "text"}"#).is_err()
        );
        assert!(parse(r#"{"class": "write", "tier": "approve", "outgoing_txt": {"arg": "text"}}"#).is_err());
    }

    #[test]
    fn a_rule_can_name_several_places_of_text() {
        let parse = |text: &str| serde_json::from_str::<OutgoingText>(text).unwrap();
        // The stored forms from before keep their meaning.
        assert_eq!(parse(r#""none""#).locations(), Vec::<&str>::new());
        assert_eq!(parse(r#"{"arg": "text"}"#), OutgoingText::Arg("text".into()));
        let both = parse(r#"{"paths": ["text", "blocks.*.text"]}"#);
        assert_eq!(both.locations(), ["text", "blocks.*.text"]);
        assert_eq!(both.check(), Ok(()));
        let rule = ToolRule::write(ToolClass::Write, both);
        assert_eq!(
            serde_json::to_value(&rule.outgoing_text).unwrap(),
            serde_json::json!({"paths": ["text", "blocks.*.text"]})
        );
    }

    #[test]
    fn paths_that_name_no_place_are_refused() {
        let paths = |list: &[&str]| OutgoingText::Paths(list.iter().map(|p| (*p).to_owned()).collect());
        assert!(paths(&[]).check().unwrap_err().contains("names no place"));
        for bad in ["", "blocks..text", "blocks.", ".text", "*", "*.text"] {
            assert!(paths(&["text", bad]).check().is_err(), "{bad:?}");
        }
        assert_eq!(paths(&["attachments.*.fields.*.value", "title"]).check(), Ok(()));
        // An `arg` is one name at the top, so it needs no check.
        assert_eq!(OutgoingText::Arg("a.b".into()).check(), Ok(()));
    }
}
