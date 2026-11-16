//! When to suggest that a tool run without asking. The owner raises the step; the engine never does.

use crate::policy::{Tier, ToolClass, ToolRule};
use crate::records::DecisionCounts;
use serde::{Deserialize, Serialize};

/// The tool IDs of the built-in coding tools start with this. A coding job always asks the owner.
pub const CODING_TOOL_PREFIX: &str = "bunsin.coding/";

/// The owner's thresholds for a suggestion. Without them, no tool is suggested.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutonomyThresholds {
    /// The least number of calls the owner approved, edited or dismissed.
    pub min_reviewed: u32,
    /// The least share of those calls that the owner approved without a change, in percent.
    pub min_unchanged_percent: u32,
    /// Only the decisions of this many days back count.
    pub window_days: u32,
}

impl AutonomyThresholds {
    /// Refuses thresholds that no tool can meet or that count nothing.
    pub fn check(&self) -> Result<(), String> {
        if self.min_unchanged_percent > 100 {
            return Err("`autonomy.min_unchanged_percent` must be 100 or less".into());
        }
        if self.window_days == 0 {
            return Err("`autonomy.window_days` must be 1 or more".into());
        }
        Ok(())
    }
}

/// True if the owner could let `tool_id` run without asking. Only a write tool at the step
/// `approve` that is not held and not a coding tool qualifies, and its counts must meet the
/// owner's thresholds. A delete is never suggested: it always asks.
pub fn suggest(
    thresholds: Option<&AutonomyThresholds>,
    tool_id: &str,
    rule: &ToolRule,
    counts: &DecisionCounts,
    held: bool,
) -> bool {
    let Some(thresholds) = thresholds else {
        return false;
    };
    rule.class == ToolClass::Write
        && rule.tier == Tier::Approve
        && !tool_id.starts_with(CODING_TOOL_PREFIX)
        && !held
        && counts.reviewed() >= thresholds.min_reviewed
        && counts
            .unchanged_percent()
            .is_some_and(|percent| percent >= thresholds.min_unchanged_percent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::OutgoingText;

    const THRESHOLDS: AutonomyThresholds = AutonomyThresholds {
        min_reviewed: 10,
        min_unchanged_percent: 80,
        window_days: 30,
    };

    fn rule(class: ToolClass, tier: Tier) -> ToolRule {
        ToolRule {
            class,
            tier,
            outgoing_text: OutgoingText::None,
        }
    }

    /// `approved` of `reviewed` calls approved without a change; the rest edited.
    fn counts(approved: u32, reviewed: u32) -> DecisionCounts {
        DecisionCounts {
            approved,
            edited: reviewed - approved,
            ..Default::default()
        }
    }

    #[test]
    fn decision_table() {
        use Tier::*;
        use ToolClass::*;
        let t = Some(&THRESHOLDS);
        // (thresholds, tool, class, tier, approved, reviewed, held, expected)
        let cases = [
            (t, "slack/post", Write, Approve, 80, 100, false, true),
            (t, "slack/post", Write, Approve, 79, 100, false, false),
            (t, "slack/post", Write, Approve, 8, 10, false, true),
            // Too few reviews, even if every one was approved unchanged.
            (t, "slack/post", Write, Approve, 9, 9, false, false),
            // No thresholds: no suggestion.
            (None, "slack/post", Write, Approve, 100, 100, false, false),
            // A delete always asks.
            (t, "slack/delete", Delete, Approve, 100, 100, false, false),
            (t, "slack/search", Read, Autonomous, 100, 100, false, false),
            // A held tool is not suggested until the owner clears the hold.
            (t, "slack/post", Write, Approve, 100, 100, true, false),
            // Only the next step up from approve is suggested.
            (t, "slack/post", Write, Autonomous, 100, 100, false, false),
            (t, "slack/post", Write, Draft, 100, 100, false, false),
            (t, "slack/post", Write, Observe, 100, 100, false, false),
            (t, "bunsin.coding/start", Write, Approve, 100, 100, false, false),
        ];
        for (thresholds, tool, class, tier, approved, reviewed, held, expected) in cases {
            assert_eq!(
                suggest(thresholds, tool, &rule(class, tier), &counts(approved, reviewed), held),
                expected,
                "{tool} {class:?} {tier:?} {approved}/{reviewed} held={held} thresholds={}",
                thresholds.is_some()
            );
        }
    }

    #[test]
    fn a_tool_without_reviews_is_not_suggested_even_at_zero_thresholds() {
        let zero = AutonomyThresholds {
            min_reviewed: 0,
            min_unchanged_percent: 0,
            window_days: 1,
        };
        let write = rule(ToolClass::Write, Tier::Approve);
        assert!(!suggest(
            Some(&zero),
            "slack/post",
            &write,
            &DecisionCounts::default(),
            false
        ));
        assert!(suggest(Some(&zero), "slack/post", &write, &counts(0, 1), false));
    }

    #[test]
    fn thresholds_that_count_nothing_are_refused() {
        assert_eq!(THRESHOLDS.check(), Ok(()));
        let over = AutonomyThresholds {
            min_unchanged_percent: 101,
            ..THRESHOLDS
        };
        assert!(over.check().unwrap_err().contains("min_unchanged_percent"));
        let empty = AutonomyThresholds {
            window_days: 0,
            ..THRESHOLDS
        };
        assert!(empty.check().unwrap_err().contains("window_days"));
        let parse = |text: &str| serde_json::from_str::<AutonomyThresholds>(text);
        assert!(parse(r#"{"min_reviewed": 1, "min_unchanged_percent": 90, "window_days": 30}"#).is_ok());
        // A misspelled field is an error, not a missing threshold.
        assert!(parse(r#"{"min_reviewed": 1, "min_unchanged": 90, "window_days": 30}"#).is_err());
    }
}
