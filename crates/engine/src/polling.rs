//! Repeats the searches of a profile and hands their new items to the pipeline.

use crate::{Ctx, Engine, internal, store_error};
use bunsin_api::{ApiError, ApiResult, EngineApi, Outcome, PollPreview, PollStatus};
use bunsin_core::event::InboundEvent;
use bunsin_core::policy::{Decision, ToolClass};
use bunsin_core::poll::{PollConfig, extract};
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::proposal::split_tool_id;
use bunsin_store::ProfileStore;
use serde_json::json;

/// The most new events one run of a poll hands to the pipeline. The rest wait for the next run.
const MAX_NEW_EVENTS_PER_RUN: u32 = 10;

fn status_key(poll: &PollConfig) -> String {
    format!("poll_status:{}", poll.identity())
}

fn baseline_key(poll: &PollConfig) -> String {
    format!("poll_baseline:{}", poll.identity())
}

/// Removes a profile from the set of profiles that are being polled.
struct PollingGuard<'a> {
    engine: &'a Engine,
    profile: String,
}

impl Drop for PollingGuard<'_> {
    fn drop(&mut self) {
        self.engine.polling.lock().expect("engine lock").remove(&self.profile);
    }
}

impl Engine {
    pub(crate) fn poll_statuses(&self, store: &ProfileStore) -> ApiResult<Vec<PollStatus>> {
        let config = Self::config(store)?;
        config
            .polls
            .iter()
            .map(|poll| match store.setting(&status_key(poll)).map_err(store_error)? {
                Some(text) => serde_json::from_str(&text).map_err(internal),
                None => Ok(PollStatus {
                    tool: poll.tool_id(),
                    ..PollStatus::default()
                }),
            })
            .collect()
    }

    /// Calls the search tool of a poll and returns its usable items, without the skipped ones.
    /// The tool must have a `read` rule and must be unchanged since the owner approved it.
    async fn fetch(&self, ctx: &Ctx, poll: &PollConfig) -> Result<(u32, Vec<InboundEvent>), String> {
        let tool_id = poll.tool_id();
        let is_read = ctx
            .config
            .policy
            .rules
            .get(&tool_id)
            .is_some_and(|rule| rule.class == ToolClass::Read);
        if !is_read {
            return Err(format!("`{tool_id}` has no rule with the class `read`"));
        }
        match self.decide(ctx, &tool_id).await.map_err(|e| e.to_string())? {
            Decision::Execute => {}
            other => return Err(format!("the policy does not allow `{tool_id}`: {other:?}")),
        }
        let connector = split_tool_id(&tool_id)
            .and_then(|(connector, _)| ctx.runtime.connectors.get(connector))
            .ok_or_else(|| format!("there is no connector `{}`", poll.connector))?;
        let result = connector
            .call_tool(&poll.tool, poll.args.clone())
            .await
            .map_err(|e| e.to_string())?;
        let extracted = extract(poll, &result)?;
        let events = extracted
            .events
            .into_iter()
            // The agent's own posts and the owner's own messages are not events for the agent.
            .filter(|event| !poll.skip_authors.contains(&event.author) && !event.text.contains(&ctx.config.ai_marker))
            .collect();
        Ok((extracted.items, events))
    }

    /// Runs one poll and returns what it did.
    async fn run_poll(&self, profile: &str, ctx: &Ctx, poll: &PollConfig) -> ApiResult<PollStatus> {
        let mut status = PollStatus {
            tool: poll.tool_id(),
            last_run_at: Some(self.now()),
            ..PollStatus::default()
        };
        let (items, events) = match self.fetch(ctx, poll).await {
            Ok(fetched) => fetched,
            Err(error) => {
                status.error = Some(error);
                return Ok(status);
            }
        };
        status.items = items;
        status.usable = events.len() as u32;

        // The first run of a search finds old messages. They are recorded as seen and not handled.
        let baseline = baseline_key(poll);
        if ctx.store.setting(&baseline).map_err(store_error)?.is_none() {
            for event in events {
                let event = Self::redact_event(&ctx.redactor, event, &mut RedactionCounts::new());
                let payload = serde_json::to_value(&event).map_err(internal)?;
                ctx.store
                    .mark_event_seen(&event.dedupe_key(), &payload, self.now())
                    .map_err(store_error)?;
            }
            ctx.store.set_setting(&baseline, "set").map_err(store_error)?;
            return Ok(status);
        }

        for event in events {
            if status.new_events >= MAX_NEW_EVENTS_PER_RUN {
                break;
            }
            match self.ingest(profile, event).await {
                Ok(Outcome::Duplicate) => {}
                Ok(_) => status.new_events += 1,
                // The cause is probably the same for the next event. The event stays open for a retry.
                Err(error) => {
                    status.error = Some(error.to_string());
                    break;
                }
            }
        }
        Ok(status)
    }

    /// Runs the polls of a profile that are due. Returns how many new events were handled.
    /// Does nothing if the profile is being polled already, or if its kill switch is on.
    pub async fn poll_profile(&self, profile: &str) -> ApiResult<u32> {
        if !self.polling.lock().expect("engine lock").insert(profile.to_owned()) {
            return Ok(0);
        }
        let _guard = PollingGuard {
            engine: self,
            profile: profile.to_owned(),
        };

        let store = self.store(profile)?;
        let config = Self::config(&store)?;
        // With the kill switch on, a handled event would be closed with a denied proposal.
        // It is better not to take events until the owner turns the switch off.
        if config.polls.is_empty() || store.kill_switch().map_err(store_error)? {
            return Ok(0);
        }
        let now = self.now();
        let due: Vec<&PollConfig> = config
            .polls
            .iter()
            .filter(|poll| {
                let last = self
                    .last_polls
                    .lock()
                    .expect("engine lock")
                    .get(&(profile.to_owned(), poll.identity()))
                    .copied();
                last.is_none_or(|last| now - last >= poll.every_secs as i64)
            })
            .collect();
        if due.is_empty() {
            return Ok(0);
        }

        let ctx = self.ctx(profile).await?;
        let mut new_events = 0;
        for poll in due {
            self.last_polls
                .lock()
                .expect("engine lock")
                .insert((profile.to_owned(), poll.identity()), now);
            let status = self.run_poll(profile, &ctx, poll).await?;
            new_events += status.new_events;

            let key = status_key(poll);
            let before: Option<PollStatus> = match store.setting(&key).map_err(store_error)? {
                Some(text) => serde_json::from_str(&text).ok(),
                None => None,
            };
            store
                .set_setting(&key, &serde_json::to_string(&status).map_err(internal)?)
                .map_err(store_error)?;
            // The audit log gets an entry when something happened or the state of the poll changed,
            // not on every quiet run.
            let unusable = |s: &PollStatus| s.items > 0 && s.usable == 0;
            let changed = before
                .as_ref()
                .is_none_or(|before| before.error != status.error || unusable(before) != unusable(&status));
            if status.new_events > 0 || changed {
                self.audit(&store, "poll", None, json!(status))?;
            }
        }
        Ok(new_events)
    }

    /// Polls every profile once. One failing profile does not stop the others.
    pub async fn poll_all(&self) -> Vec<(String, ApiError)> {
        let mut failures = Vec::new();
        for profile in self.app.list_profiles().unwrap_or_default() {
            if let Err(error) = self.poll_profile(&profile.id).await {
                failures.push((profile.id, error));
            }
        }
        failures
    }

    pub(crate) async fn preview_poll(&self, profile: &str, index: usize) -> ApiResult<PollPreview> {
        let ctx = self.ctx(profile).await?;
        let poll = ctx
            .config
            .polls
            .get(index)
            .ok_or_else(|| ApiError::NotFound(format!("poll number {index}")))?;
        let (items, events) = self.fetch(&ctx, poll).await.map_err(ApiError::NotAllowed)?;
        let events = events
            .into_iter()
            .map(|event| Self::redact_event(&ctx.redactor, event, &mut RedactionCounts::new()))
            .collect();
        Ok(PollPreview { items, events })
    }
}
