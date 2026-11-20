//! The owner's principles: instructions that go into every prompt of the agent.
//! They are the owner's own words, so they do not pass the privacy filter.

use crate::Engine;
use bunsin_api::{ApiError, ApiResult, Expected, PrinciplesUpdate};
use serde_json::json;

/// The most principles one profile can have. Every principle is in every prompt.
pub const MAX_PRINCIPLES: usize = 50;
/// The most characters of one principle.
pub const MAX_PRINCIPLE_CHARS: usize = 1_000;

/// Refuses a list that is too long or has an empty or too long principle.
/// An error names the principle by its position, counted from 1.
fn check(principles: &[String]) -> ApiResult<()> {
    if principles.len() > MAX_PRINCIPLES {
        return Err(ApiError::Invalid(format!(
            "at most {MAX_PRINCIPLES} principles are allowed"
        )));
    }
    for (index, principle) in principles.iter().enumerate() {
        let chars = principle.trim().chars().count();
        if chars == 0 {
            return Err(ApiError::Invalid(format!("principle {} is empty", index + 1)));
        }
        if chars > MAX_PRINCIPLE_CHARS {
            return Err(ApiError::Invalid(format!(
                "principle {} has {chars} characters; the most is {MAX_PRINCIPLE_CHARS}",
                index + 1
            )));
        }
    }
    Ok(())
}

impl Engine {
    pub(crate) fn read_principles(&self, profile: &str) -> ApiResult<Vec<String>> {
        Ok(Self::config(&*self.store(profile)?)?.principles)
    }

    /// Replaces the principles. Each is stored without the space around it.
    /// The audit entry holds counts only.
    pub(crate) fn replace_principles(&self, profile: &str, update: PrinciplesUpdate) -> ApiResult<()> {
        let principles: Vec<String> = update.principles.iter().map(|p| p.trim().to_owned()).collect();
        check(&principles)?;
        let store = self.store(profile)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        if let Expected::Value(expected) = &update.expected
            && *expected != config.principles
        {
            return Err(ApiError::Conflict(
                "the principles are not the ones this form loaded".into(),
            ));
        }
        let before = std::mem::replace(&mut config.principles, principles);
        Self::check_config(&config)?;
        Self::save_config_from(&store, &loaded, &config)?;
        let after = &config.principles;
        let added = after.iter().filter(|p| !before.contains(p)).count();
        let removed = before.iter().filter(|p| !after.contains(p)).count();
        let detail = json!({"before": before.len(), "after": after.len(), "added": added, "removed": removed});
        self.audit(&store, "principles_changed", None, detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limits_name_the_principle_by_its_position() {
        let ok = vec!["Answer briefly.".to_owned(); MAX_PRINCIPLES];
        assert_eq!(check(&ok), Ok(()));
        assert_eq!(check(&[]), Ok(()));
        let mut too_many = ok.clone();
        too_many.push("One more.".into());
        assert!(matches!(check(&too_many), Err(ApiError::Invalid(e)) if e.contains("at most 50")));
        let empty = vec!["First.".to_owned(), " \n ".to_owned()];
        assert_eq!(check(&empty), Err(ApiError::Invalid("principle 2 is empty".into())));
        // The limit counts characters, not bytes: Korean text has three bytes per character.
        let longest = "가".repeat(MAX_PRINCIPLE_CHARS);
        assert_eq!(check(std::slice::from_ref(&longest)), Ok(()));
        let long = vec![format!("{longest}가")];
        assert!(matches!(check(&long), Err(ApiError::Invalid(e)) if e.starts_with("principle 1 has 1001")));
    }
}
