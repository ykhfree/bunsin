//! Replay evaluation: the agent drafts an answer to a past case, and the engine scores the draft
//! against the real reply without an LLM.
//!
//! A replay runs no tool and gets no notes and no corrections: they hold facts from after the case,
//! and a correction can hold the real reply. A run replays a few cases per background tick, so no
//! request waits for the whole set. A replay sends nothing, so a run goes on while the kill switch is on.

use crate::corrections::draft_text;
use crate::llm_log::Purpose;
use crate::{Ctx, Engine, LIST_LIMIT, Proposed, store_error};
use bunsin_api::{ApiError, ApiResult, EvalReport, ReplayCase, ReplayResult};
use bunsin_core::eval::{CaseScore, DraftKind, EvalResult, EvalSummary, Expect, ReplayCaseRecord, RunState};
use bunsin_core::event::InboundEvent;
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::proposal::Proposal;
use serde_json::json;

/// How many cases one background tick replays for one profile.
const CASES_PER_TICK: u32 = 3;

/// What the replay of one case gave.
struct Replayed {
    proposal: Option<Proposal>,
    score: Option<CaseScore>,
    /// The text of the draft, after the privacy filter.
    draft: String,
    /// Why no proposal came, after the privacy filter.
    error: Option<String>,
}

impl Engine {
    /// Replays one case and scores the draft. The event and the real reply pass the filter first.
    async fn replay_one(
        &self,
        ctx: &Ctx,
        event: InboundEvent,
        actual_reply: &str,
        expect: Expect,
    ) -> ApiResult<Replayed> {
        let filter = |text: &str| ctx.redactor.redact(text, &mut RedactionCounts::new());
        // Only the event goes to the LLM. The real reply is kept for the comparison.
        let event = Self::redact_event(&ctx.redactor, event, &mut RedactionCounts::new());
        let key = event.dedupe_key();
        let (proposed, replies) = self.propose(ctx, &event, &key, false, Purpose::Replay).await?;
        for reply in &replies {
            ctx.store
                .put_artifact("replay_reply", filter(reply).as_bytes(), self.now(), Some(&key), None)
                .map_err(store_error)?;
        }
        let proposal = match proposed {
            Proposed::Ready { proposal, .. } => proposal,
            // The error can hold the body of an upstream reply. It is stored, so it is filtered.
            Proposed::Retry(error) | Proposed::Stop(error) => {
                return Ok(Replayed {
                    proposal: None,
                    score: None,
                    draft: String::new(),
                    error: Some(filter(&error)),
                });
            }
        };
        let rules = &ctx.config.policy.rules;
        let (class, draft) = match &proposal {
            Proposal::ToolCall { tool, args, .. } => {
                let rule = rules.get(tool);
                let outgoing = rule.map(|rule| &rule.outgoing_text);
                (
                    rule.map(|rule| rule.class),
                    draft_text(args, outgoing, &ctx.config.ai_marker),
                )
            }
            Proposal::Escalate { summary, .. } => (None, summary.clone()),
            Proposal::Ignore { .. } => (None, String::new()),
        };
        let draft = filter(&draft);
        let kind = DraftKind::of(&proposal, class);
        Ok(Replayed {
            score: Some(CaseScore::new(expect, kind, &draft, &filter(actual_reply))),
            proposal: Some(proposal),
            draft,
            error: None,
        })
    }

    pub(crate) async fn replay_case(&self, profile: &str, case: ReplayCase) -> ApiResult<ReplayResult> {
        let ctx = self.ctx(profile).await?;
        let replayed = self
            .replay_one(&ctx, case.event, &case.actual_reply, case.expect)
            .await?;
        Ok(ReplayResult {
            proposal: replayed.proposal,
            error: replayed.error,
            actual_reply: case.actual_reply,
            score: replayed.score,
        })
    }

    /// Stores cases after the privacy filter. The audit entry holds the count only.
    pub(crate) fn add_cases(&self, profile: &str, cases: Vec<ReplayCase>) -> ApiResult<Vec<i64>> {
        let store = self.store(profile)?;
        let redactor = Self::redactor(&Self::config(&store)?)?;
        let mut ids = Vec::with_capacity(cases.len());
        for case in cases {
            let mut counts = RedactionCounts::new();
            let event = Self::redact_event(&redactor, case.event, &mut counts);
            let reply = redactor.redact(&case.actual_reply, &mut counts);
            ids.push(
                store
                    .add_replay_case(&event, &reply, case.expect, self.now())
                    .map_err(store_error)?,
            );
        }
        self.audit(&store, "replay_cases_added", None, json!({"count": ids.len()}))?;
        Ok(ids)
    }

    pub(crate) fn read_cases(&self, profile: &str) -> ApiResult<Vec<ReplayCaseRecord>> {
        self.store(profile)?.replay_cases(LIST_LIMIT).map_err(store_error)
    }

    pub(crate) fn delete_case(&self, profile: &str, id: i64) -> ApiResult<()> {
        let store = self.store(profile)?;
        store.delete_replay_case(id).map_err(store_error)?;
        self.audit(&store, "replay_case_deleted", None, json!({"id": id}))
    }

    pub(crate) fn start_run(&self, profile: &str) -> ApiResult<i64> {
        let store = self.store(profile)?;
        if store.replay_cases(1).map_err(store_error)?.is_empty() {
            return Err(ApiError::NotAllowed("there is no replay case to evaluate".into()));
        }
        let Some(run) = store.start_eval_run(self.now()).map_err(store_error)? else {
            return Err(ApiError::NotAllowed("an evaluation is running already".into()));
        };
        self.audit(&store, "eval_started", None, json!({"run": run}))?;
        Ok(run)
    }

    pub(crate) fn read_eval_report(&self, profile: &str) -> ApiResult<EvalReport> {
        let store = self.store(profile)?;
        let pass = Self::config(&store)?.eval.pass;
        let Some(run) = store.latest_eval_run().map_err(store_error)? else {
            return Ok(EvalReport {
                run: None,
                remaining: 0,
                results: Vec::new(),
                summary: EvalSummary::default(),
                pass,
                passed: None,
            });
        };
        let results = store.eval_results(run.id).map_err(store_error)?;
        let scores: Vec<_> = results.iter().map(|result| (result.expect, result.score)).collect();
        let summary = EvalSummary::of(&scores);
        let (remaining, passed) = match run.state {
            RunState::Running => (store.unscored_cases(run.id, u32::MAX).map_err(store_error)?.len(), None),
            RunState::Done => (0, pass.map(|pass| pass.passed_by(&summary))),
        };
        Ok(EvalReport {
            run: Some(run),
            remaining: remaining as u32,
            results,
            summary,
            pass,
            passed,
        })
    }

    /// Replays the next cases of the running run of a profile, or ends the run when none is left.
    async fn eval_tick(&self, profile: &str) -> ApiResult<()> {
        let store = self.store(profile)?;
        let Some(run) = store.latest_eval_run().map_err(store_error)? else {
            return Ok(());
        };
        if run.state != RunState::Running {
            return Ok(());
        }
        let cases = store.unscored_cases(run.id, CASES_PER_TICK).map_err(store_error)?;
        if cases.is_empty() {
            if store.finish_eval_run(run.id, self.now()).map_err(store_error)? {
                let summary = self.read_eval_report(profile)?.summary;
                self.audit(
                    &store,
                    "eval_finished",
                    None,
                    json!({"run": run.id, "summary": summary}),
                )?;
            }
            return Ok(());
        }
        let ctx = self.ctx(profile).await?;
        for case in cases {
            let replayed = self
                .replay_one(&ctx, case.event, &case.actual_reply, case.expect)
                .await?;
            let result = EvalResult {
                case_id: case.id,
                expect: case.expect,
                score: replayed.score,
                draft: replayed.draft,
                error: replayed.error,
            };
            store.add_eval_result(run.id, &result).map_err(store_error)?;
        }
        Ok(())
    }

    /// The work that runs without a request: one step of each profile's evaluation run.
    /// It ignores the kill switch, because it sends nothing. One failing profile does not stop the others.
    pub async fn background_tick(&self) -> Vec<(String, ApiError)> {
        let mut failures = Vec::new();
        for profile in self.app.list_profiles().unwrap_or_default() {
            if let Err(error) = self.eval_tick(&profile.id).await {
                failures.push((profile.id, error));
            }
        }
        failures
    }
}
