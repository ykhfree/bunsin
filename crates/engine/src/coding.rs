//! Coding jobs. An approved call of `bunsin.coding/start` starts a job on the server's coder and
//! returns at once. The background task collects the jobs that ended: the engine stores the result as
//! artifacts and puts a draft into the inbox that tells the owner to review and merge the branch.
//!
//! The engine never merges and never waits for a job. A stop while a job runs leaves the item
//! `executing`, so the next start marks it `unknown`.

use crate::{Ctx, Engine, store_error};
use bunsin_api::{ApiError, ApiResult, ProfileConfig};
use bunsin_core::approval::ApprovalState;
use bunsin_core::coding::{
    CODING_START_TOOL, CodingJobSpec, CodingOutcome, CodingReport, StartArgs, TestOutcome, job_id, report_text,
};
use bunsin_core::context::ToolHint;
use bunsin_core::policy::{OutgoingText, ToolClass};
use bunsin_core::privacy::{RedactionCounts, Redactor};
use bunsin_core::records::{InboxBody, InboxItem};
use serde_json::{Map, Value, json};

impl Engine {
    /// The hint of the coding tool: only with coding settings and a rule for the tool.
    pub(crate) fn coding_hint(config: &ProfileConfig) -> Option<ToolHint> {
        let coding = config.coding.as_ref()?;
        config.policy.rules.contains_key(CODING_START_TOOL).then(|| ToolHint {
            tool_id: CODING_START_TOOL.to_owned(),
            description: coding.tool_description(),
            input_schema: coding.tool_schema(),
        })
    }

    /// Refuses a rule for a coding tool that is not a write without outgoing text.
    pub(crate) fn check_coding_rules(config: &ProfileConfig) -> ApiResult<()> {
        if let Some(coding) = &config.coding {
            coding.check().map_err(ApiError::Invalid)?;
        }
        for (tool, rule) in &config.policy.rules {
            if !bunsin_core::coding::is_coding_tool(tool) {
                continue;
            }
            if tool != CODING_START_TOOL {
                return Err(ApiError::Invalid(format!("there is no built-in tool `{tool}`")));
            }
            if rule.class != ToolClass::Write || rule.outgoing_text != OutgoingText::None {
                return Err(ApiError::Invalid(format!(
                    "the rule of `{tool}` must have the class `write` and the outgoing text \"none\""
                )));
            }
        }
        Ok(())
    }

    /// Starts the job of an approved call that is `executing`. The server's coder runs it in the background.
    pub(crate) fn start_coding(&self, ctx: &Ctx, item: &InboxItem, args: &Map<String, Value>) -> Result<(), String> {
        let config = ctx.config.coding.as_ref().ok_or("the settings have no `coding`")?;
        let spec = CodingJobSpec::new(&ctx.profile, item.id, config, StartArgs::parse(args)?)?;
        let (job, repo) = (spec.job_id.clone(), spec.repo_name.clone());
        self.backends.coder()?.start(spec)?;
        let detail = json!({"inbox_id": item.id, "job": job, "repo": repo});
        self.audit(&ctx.store, "coding_started", Some(&item.event_key), detail)
            .map_err(|e| e.to_string())
    }

    /// Records the jobs that ended since the last call. One failing report does not stop the others.
    pub(crate) fn coding_tick(&self) -> Vec<(String, ApiError)> {
        let Ok(coder) = self.backends.coder() else {
            return Vec::new();
        };
        coder
            .finished()
            .into_iter()
            .filter_map(|report| {
                let profile = report.profile.clone();
                self.finish_coding_job(report).err().map(|error| (profile, error))
            })
            .collect()
    }

    /// Stores the result of an ended job, moves its item to `executed` or `failed`, and adds a draft for
    /// the owner. A report for an item that is no longer `executing` is ignored.
    fn finish_coding_job(&self, report: CodingReport) -> ApiResult<()> {
        let store = self.store(&report.profile)?;
        let config = Self::config(&store)?;
        let redactor = Self::redactor(&config)?;
        let item = store.inbox_item(report.inbox_id).map_err(store_error)?;
        let key = Some(item.event_key.as_str());
        let repo = match &item.body {
            InboxBody::ToolCall { tool, args } if tool == CODING_START_TOOL => {
                args.get("repo").and_then(Value::as_str).unwrap_or_default().to_owned()
            }
            _ => String::new(),
        };
        if item.state != ApprovalState::Executing || repo.is_empty() {
            let detail = json!({"inbox_id": item.id, "state": item.state.as_str()});
            return self.audit(&store, "coding_report_ignored", key, detail);
        }
        // The output of a coding agent and of the tests can hold anything. It is stored filtered.
        let outcome = report
            .outcome
            .map(|outcome| filtered(&redactor, outcome))
            .map_err(|error| filter(&redactor, &error));
        if let Ok(outcome) = &outcome {
            let result = CodingOutcome {
                transcript: String::new(),
                ..outcome.clone()
            };
            let result = serde_json::to_vec_pretty(&result).map_err(crate::internal)?;
            for (kind, content) in [
                ("coding_result", result.as_slice()),
                ("coding_transcript", outcome.transcript.as_bytes()),
            ] {
                store
                    .put_artifact(kind, content, self.now(), key, Some(item.id))
                    .map_err(store_error)?;
            }
        }
        let to = match &outcome {
            Ok(outcome) if outcome.ended_well() => ApprovalState::Executed,
            _ => ApprovalState::Failed,
        };
        if !store
            .move_inbox_state(item.id, ApprovalState::Executing, to, self.now())
            .map_err(store_error)?
        {
            return Ok(());
        }
        let summary = report_text(&job_id(item.id, &report.profile), &repo, &outcome);
        let body = InboxBody::Escalation { summary };
        let rationale = "a coding job ended; review its branch and merge it yourself";
        let draft = store
            .add_inbox_item(&item.event_key, &body, rationale, &[], ApprovalState::Draft, self.now())
            .map_err(store_error)?;
        let detail = json!({"inbox_id": item.id, "state": to.as_str(), "draft": draft.id});
        self.audit(&store, "coding_finished", key, detail)?;
        if to == ApprovalState::Executed {
            self.follow_up_caller(&store, &config, &item)?;
        }
        Ok(())
    }
}

fn filter(redactor: &Redactor, text: &str) -> String {
    redactor.redact(text, &mut RedactionCounts::new())
}

fn filtered(redactor: &Redactor, outcome: CodingOutcome) -> CodingOutcome {
    CodingOutcome {
        diff_stat: filter(redactor, &outcome.diff_stat),
        test: outcome.test.map(|test| TestOutcome {
            output: filter(redactor, &test.output),
            ..test
        }),
        summary: outcome.summary.map(|summary| filter(redactor, &summary)),
        transcript: filter(redactor, &outcome.transcript),
        ..outcome
    }
}
