//! The permission rules on the command line: one rule at a time, with the conflict check.

use bunsin_api::{ApiError, EngineApi, Expected, Permissions, PermissionsUpdate};
use bunsin_core::policy::{OutgoingText, Tier, ToolClass, ToolRule};
use std::collections::BTreeMap;

/// Reads a word such as `write` or `approve` as the value it names.
fn word<T: serde::de::DeserializeOwned>(text: &str, what: &str) -> Result<T, String> {
    serde_json::from_value(serde_json::Value::String(text.to_owned())).map_err(|_| format!("`{text}` is not a {what}"))
}

fn spell<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Refuses a tool ID that is not `<connector>/<tool>`.
pub fn check_tool_id(tool: &str) -> Result<(), String> {
    match tool.split_once('/') {
        Some((connector, name)) if !connector.is_empty() && !name.is_empty() => Ok(()),
        _ => Err(format!("`{tool}` is not a tool ID: write <connector>/<tool>")),
    }
}

/// Builds a rule from the options of `rule-set`.
///
/// A write or delete rule needs an explicit text choice: places with `--text`, or `--no-text`.
/// Without a step it starts at `approve`.
pub fn parse_rule(class: &str, tier: Option<&str>, texts: &[String], no_text: bool) -> Result<ToolRule, String> {
    let class: ToolClass = word(class, "class (read, write or delete)")?;
    if class == ToolClass::Read {
        if tier.is_some() || !texts.is_empty() || no_text {
            return Err("a read rule has no step and sends no text: give only --class read".into());
        }
        return Ok(ToolRule::read());
    }
    let tier: Tier = match tier {
        Some(tier) => word(tier, "step (observe, draft, approve or autonomous)")?,
        None => Tier::Approve,
    };
    if class == ToolClass::Delete && tier == Tier::Autonomous {
        return Err("a delete always asks: use --tier approve".into());
    }
    let outgoing_text = match (texts, no_text) {
        ([], false) => {
            return Err(
                "say where the text for other people is: --text <place> (repeat it for more places), \
                 or --no-text if the tool sends none"
                    .into(),
            );
        }
        ([], true) => OutgoingText::None,
        (_, true) => return Err("give --text or --no-text, not both".into()),
        // One argument at the top is stored in the form that the window also writes.
        ([one], false) if !one.contains('.') => OutgoingText::Arg(one.clone()),
        (places, false) => OutgoingText::Paths(places.to_vec()),
    };
    outgoing_text.check()?;
    Ok(ToolRule {
        class,
        tier,
        outgoing_text,
    })
}

/// The table with one rule set (`Some`) or removed (`None`). The update expects the table
/// as it was read, so the server refuses it if another change came in between.
pub fn change(
    read: &BTreeMap<String, ToolRule>,
    tool: &str,
    rule: Option<ToolRule>,
) -> Result<PermissionsUpdate, String> {
    check_tool_id(tool)?;
    let mut rules = read.clone();
    match rule {
        Some(rule) => {
            rules.insert(tool.to_owned(), rule);
        }
        None => {
            rules.remove(tool).ok_or_else(|| format!("`{tool}` has no rule"))?;
        }
    }
    Ok(PermissionsUpdate {
        rules,
        expected: Expected::Value(read.clone()),
    })
}

/// One line per rule, then one line per approved tool without a rule.
/// The columns are the tool ID, the class, the step and the places of text.
pub fn table_lines(permissions: &Permissions) -> Vec<String> {
    let approved: Vec<String> = permissions
        .tools
        .iter()
        .flat_map(|(connector, tools)| tools.iter().map(move |tool| format!("{connector}/{}", tool.name)))
        .collect();
    let mut lines: Vec<String> = permissions
        .rules
        .iter()
        .map(|(tool, rule)| {
            let mut line = match rule.class {
                ToolClass::Read => format!("{tool}\tread"),
                _ => {
                    let text = match rule.outgoing_text.locations().as_slice() {
                        [] => "no text".to_owned(),
                        places => format!("text in {}", places.join(", ")),
                    };
                    format!("{tool}\t{}\t{}\t{text}", spell(&rule.class), spell(&rule.tier))
                }
            };
            if !approved.contains(tool) {
                line.push_str("\t(the tool is not approved)");
            }
            line
        })
        .collect();
    lines.extend(
        approved
            .iter()
            .filter(|tool| !permissions.rules.contains_key(*tool))
            .map(|tool| format!("{tool}\toff")),
    );
    lines
}

/// Reads the table, changes one rule and saves the table. The save expects the table as it
/// was read, so a change by someone else in between is refused, not overwritten.
pub async fn save_rule(
    engine: &dyn EngineApi,
    profile: &str,
    tool: &str,
    rule: Option<ToolRule>,
) -> Result<(), String> {
    let read = engine.permissions(profile).await.map_err(|e| e.to_string())?;
    let update = change(&read.rules, tool, rule)?;
    engine.set_permissions(profile, update).await.map_err(|e| match e {
        ApiError::Conflict(_) => format!("{e}. Nothing was saved. Run the command again."),
        other => other.to_string(),
    })
}
