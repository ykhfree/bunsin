//! Pre-load of the notes: the owner names one tool call that reads, and the agent keeps the durable
//! facts of its result as page changes. The changes go to the inbox like any other.
//!
//! The call passes the checks of a poll: a `read` rule and a policy that allows it now. The result is
//! the source of the changes. A proposal other than `ignore` is never run; the audit log records it.

use crate::llm_log::Purpose;
use crate::{Engine, Proposed, store_error};
use bunsin_api::{ApiError, ApiResult, PreloadNotes};
use bunsin_core::event::{Channel, InboundEvent};
use bunsin_core::privacy::RedactionCounts;
use bunsin_core::proposal::Proposal;
use bunsin_core::records::ReadNote;
use serde_json::json;

/// The source of the event that a pre-load hands to the agent.
pub const PRELOAD_SOURCE: &str = "bunsin.preload";

impl Engine {
    pub(crate) async fn preload(&self, profile: &str, request: PreloadNotes) -> ApiResult<Vec<i64>> {
        let ctx = self.ctx(profile).await?;
        let PreloadNotes { tool, args } = request;
        let result = self.read_once(&ctx, &tool, &args, "preload").await?;
        let owner = &ctx.config.owner_name;
        let event = InboundEvent {
            source: PRELOAD_SOURCE.to_owned(),
            external_id: format!("{tool} at {}", self.now()),
            thread: None,
            author: owner.clone(),
            text: format!(
                "{owner} asked you to read the result of `{tool}` below and keep its durable facts in your notes. \
                 Propose the pages in `wiki_updates` and choose the kind `ignore`.\n\n{result}"
            ),
            channel: Channel::Owner,
        };
        let key = event.dedupe_key();
        let first = ReadNote { tool, args };
        let (proposed, replies) = self
            .propose(&ctx, &event, &key, true, vec![first], Purpose::Preload)
            .await?;
        for reply in &replies {
            let stored = ctx.redactor.redact(reply, &mut RedactionCounts::new());
            ctx.store
                .put_artifact("llm_reply", stored.as_bytes(), self.now(), Some(&key), None)
                .map_err(store_error)?;
        }
        let Proposed::Ready {
            proposal,
            reads,
            wiki_updates,
            pages_read,
        } = proposed
        else {
            let (Proposed::Retry(error) | Proposed::Stop(error)) = proposed else {
                unreachable!("Ready is handled above")
            };
            // The error can hold the body of an upstream reply.
            let error = ctx.redactor.redact(&error, &mut RedactionCounts::new());
            return Err(ApiError::NotAllowed(format!(
                "the agent did not finish the pre-load: {error}"
            )));
        };
        // A pre-load only fills the notes. Whatever else the agent proposes is not run.
        let kind = match &proposal {
            Proposal::Ignore { .. } => None,
            Proposal::Escalate { .. } => Some("escalate"),
            Proposal::ToolCall { .. } => Some("tool_call"),
        };
        if let Some(kind) = kind {
            self.audit(
                &ctx.store,
                "preload_proposal_skipped",
                Some(&key),
                json!({"kind": kind}),
            )?;
        }
        let filed = self.file_wiki_changes(&ctx, &key, &event, &reads, &pages_read, wiki_updates)?;
        let detail = json!({"tool": reads[0].tool, "page_changes": filed.len()});
        self.audit(&ctx.store, "notes_preloaded", Some(&key), detail)?;
        Ok(filed)
    }
}
