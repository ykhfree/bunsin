//! Every LLM call goes through `Engine::complete`, which records it in the call log.
//! A row holds the time, the job, the purpose, the tokens, the latency and how the call ended.
//! It holds no text of the request or the reply.

use crate::{Engine, store_error};
use bunsin_api::ApiResult;
use bunsin_core::llm::{CompletionRequest, CompletionResponse, LlmError, StopReason};
use bunsin_core::ports::LlmClient;
use bunsin_core::records::LlmUsage;
use bunsin_store::{LlmCall, ProfileStore};
use std::time::Instant;

/// `llm_usage` adds up the calls of this many days back from now.
const USAGE_DAYS: i64 = 90;

/// What an LLM call was for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// The handling of an inbound event.
    Event,
    /// A replay of a past case.
    Replay,
    /// The owner's connection test.
    LlmTest,
    /// The LLM judge of an evaluation run.
    Judge,
    /// The search for principle candidates in the owner's texts.
    Principles,
    /// The conflict check of the notes.
    NotesCheck,
}

impl Purpose {
    fn as_str(self) -> &'static str {
        match self {
            Purpose::Event => "event",
            Purpose::Replay => "replay",
            Purpose::LlmTest => "llm_test",
            Purpose::Judge => "judge",
            Purpose::Principles => "principles",
            Purpose::NotesCheck => "notes_check",
        }
    }
}

/// How a call ended, as a fixed kind. An error message is never stored: an HTTP error body
/// is upstream text that did not pass the privacy filter.
fn outcome(result: &Result<CompletionResponse, LlmError>) -> &'static str {
    match result {
        Ok(response) => match response.stop {
            StopReason::End => "end",
            StopReason::MaxTokens => "max_tokens",
            StopReason::Refusal => "refusal",
        },
        Err(LlmError::RateLimited) => "rate_limited",
        Err(LlmError::Http { .. }) => "http_error",
        Err(LlmError::Transport(_)) => "transport_error",
        Err(LlmError::Malformed(_)) => "malformed",
    }
}

impl Engine {
    /// Sends one request to the LLM and records the call. The outer error is a store failure;
    /// the inner result is the LLM's answer.
    pub(crate) async fn complete(
        &self,
        store: &ProfileStore,
        llm: &dyn LlmClient,
        job: Option<&str>,
        purpose: Purpose,
        request: CompletionRequest,
    ) -> ApiResult<Result<CompletionResponse, LlmError>> {
        let started = Instant::now();
        let result = llm.complete(request).await;
        let usage = result.as_ref().map(|response| response.usage).unwrap_or_default();
        store
            .record_llm_call(&LlmCall {
                at: self.now(),
                job: job.map(str::to_owned),
                purpose: purpose.as_str().to_owned(),
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                latency_ms: started.elapsed().as_millis() as u64,
                outcome: outcome(&result).to_owned(),
            })
            .map_err(store_error)?;
        Ok(result)
    }

    /// The LLM calls of the last `USAGE_DAYS` days, added up by UTC day and purpose.
    pub(crate) fn read_llm_usage(&self, profile: &str) -> ApiResult<Vec<LlmUsage>> {
        let since = self.now() - USAGE_DAYS * 24 * 60 * 60;
        self.store(profile)?.llm_usage(since).map_err(store_error)
    }
}
