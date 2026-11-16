//! The polls of a profile as the owner reads and replaces them.

use crate::Engine;
use bunsin_api::{ApiError, ApiResult, Expected, PollsUpdate, ProfileConfig};
use bunsin_core::poll::PollConfig;
use serde_json::json;

/// Refuses a poll whose tool is not in the approved list of its connector. A poll runs
/// without the owner, so it may call only a tool that the owner approved as it is listed.
fn check_approved(config: &ProfileConfig) -> ApiResult<()> {
    for poll in &config.polls {
        let approved = config
            .approved_tools
            .get(&poll.connector)
            .is_some_and(|tools| tools.iter().any(|tool| tool.name == poll.tool));
        if !approved {
            return Err(ApiError::Invalid(format!(
                "poll `{}` uses a tool that is not in the approved list",
                poll.tool_id()
            )));
        }
    }
    Ok(())
}

impl Engine {
    pub(crate) fn read_polls(&self, profile: &str) -> ApiResult<Vec<PollConfig>> {
        Ok(Self::config(&*self.store(profile)?)?.polls)
    }

    /// Replaces the polls. Each needs a `read` rule and an approved tool. The audit entry
    /// holds counts only, because the arguments of a search can name people.
    pub(crate) fn replace_polls(&self, profile: &str, update: PollsUpdate) -> ApiResult<()> {
        let store = self.store(profile)?;
        let (loaded, mut config) = Self::config_with_text(&store)?;
        if let Expected::Value(expected) = &update.expected
            && *expected != config.polls
        {
            return Err(ApiError::Conflict("the polls are not the ones this form loaded".into()));
        }
        let before = std::mem::replace(&mut config.polls, update.polls);
        Self::check_config(&config)?;
        check_approved(&config)?;
        Self::save_config_from(&store, &loaded, &config)?;
        let detail = json!({"before": before.len(), "after": config.polls.len()});
        self.audit(&store, "polls_changed", None, detail)
    }
}
