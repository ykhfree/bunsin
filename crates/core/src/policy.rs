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
    Arg(String),
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

    /// The argument that gets the AI marker, if the tool sends text.
    pub fn text_arg(&self) -> Option<&str> {
        match &self.outgoing_text {
            OutgoingText::None => None,
            OutgoingText::Arg(name) => Some(name),
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
    /// says about the tool is an input.
    pub fn decide(&self, tool_id: &str, pin_holds: bool, kill_switch: bool) -> Decision {
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
        // (class, tier, pin_holds, kill_switch, expected)
        let cases = [
            (Read, Tier::Autonomous, true, false, Execute),
            (Read, Tier::Observe, true, false, Execute),
            (Read, Tier::Autonomous, true, true, Execute),
            (
                Read,
                Tier::Autonomous,
                false,
                false,
                Deny(DenyReason::PinChanged),
            ),
            (Write, Tier::Observe, true, false, RecordOnly),
            (Write, Tier::Draft, true, false, DraftOnly),
            (Write, Tier::Approve, true, false, NeedsApproval),
            (Write, Tier::Autonomous, true, false, Execute),
            (
                Write,
                Tier::Autonomous,
                true,
                true,
                Deny(DenyReason::KillSwitch),
            ),
            (
                Write,
                Tier::Observe,
                true,
                true,
                Deny(DenyReason::KillSwitch),
            ),
            (
                Write,
                Tier::Autonomous,
                false,
                false,
                Deny(DenyReason::PinChanged),
            ),
            (Delete, Tier::Approve, true, false, NeedsApproval),
            (Delete, Tier::Autonomous, true, false, NeedsApproval),
            (
                Delete,
                Tier::Autonomous,
                true,
                true,
                Deny(DenyReason::KillSwitch),
            ),
        ];
        for (class, tier, pin_holds, kill_switch, expected) in cases {
            let p = policy(&[("slack/tool", class, tier)]);
            assert_eq!(
                p.decide("slack/tool", pin_holds, kill_switch),
                expected,
                "{class:?} {tier:?} pin={pin_holds} kill={kill_switch}"
            );
        }
    }

    #[test]
    fn a_tool_outside_the_table_is_denied() {
        let p = policy(&[("slack/post", ToolClass::Write, Tier::Autonomous)]);
        assert_eq!(
            p.decide("slack/delete_channel", true, false),
            Decision::Deny(DenyReason::UnknownTool)
        );
        // The same tool name under another connector is another tool.
        assert_eq!(
            p.decide("jira/post", true, false),
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
        assert_eq!(p.decide("slack/post", true, false), Decision::NeedsApproval);
    }

    #[test]
    fn a_rule_must_say_where_the_outgoing_text_is() {
        let parse = |text: &str| serde_json::from_str::<ToolRule>(text);
        let rule =
            parse(r#"{"class": "write", "tier": "approve", "outgoing_text": {"arg": "text"}}"#)
                .unwrap();
        assert_eq!(rule.text_arg(), Some("text"));
        let silent =
            parse(r#"{"class": "write", "tier": "approve", "outgoing_text": "none"}"#).unwrap();
        assert_eq!(silent.text_arg(), None);
        // Leaving the field out is an error, not "no marker".
        assert!(parse(r#"{"class": "write", "tier": "approve"}"#).is_err());
        // A misspelled field is an error too.
        assert!(parse(r#"{"class": "write", "tier": "approve", "outgoing_text": "none", "text_args": "text"}"#).is_err());
        assert!(
            parse(r#"{"class": "write", "tier": "approve", "outgoing_txt": {"arg": "text"}}"#)
                .is_err()
        );
    }
}
