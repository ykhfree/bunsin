use serde::{Deserialize, Serialize};

/// The state of one inbox item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalState {
    /// A draft the agent must not run. The user can only dismiss it.
    Draft,
    Pending,
    Approved,
    Rejected,
    Executing,
    Executed,
    Failed,
    /// The server stopped while the call ran, or the call got no answer in its time limit.
    /// The call may or may not have reached its tool.
    /// Only the owner resolves it: done, drop, or retry as a new item.
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalEvent {
    Approve,
    Reject,
    StartExecution,
    ExecutionSucceeded,
    ExecutionFailed,
    /// A start found the call cut by a stop, or the call got no answer.
    Interrupt,
    /// The owner says that the cut call reached its tool.
    MarkDone,
    /// The owner gives up the cut call.
    Drop,
    /// An approved call that never started may not run now. A pending copy replaces it.
    Abandon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{event:?} is not allowed in state {state:?}")]
pub struct InvalidTransition {
    pub state: ApprovalState,
    pub event: ApprovalEvent,
}

impl ApprovalState {
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalState::Draft => "draft",
            ApprovalState::Pending => "pending",
            ApprovalState::Approved => "approved",
            ApprovalState::Rejected => "rejected",
            ApprovalState::Executing => "executing",
            ApprovalState::Executed => "executed",
            ApprovalState::Failed => "failed",
            ApprovalState::Unknown => "unknown",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        use ApprovalState::*;
        [Draft, Pending, Approved, Rejected, Executing, Executed, Failed, Unknown]
            .into_iter()
            .find(|s| s.as_str() == text)
    }

    /// Returns the next state. Each item runs at most once: no state leads back to `Approved`.
    pub fn on(self, event: ApprovalEvent) -> Result<ApprovalState, InvalidTransition> {
        use ApprovalEvent as E;
        use ApprovalState as S;
        match (self, event) {
            (S::Pending, E::Approve) => Ok(S::Approved),
            (S::Pending | S::Draft, E::Reject) => Ok(S::Rejected),
            (S::Approved, E::StartExecution) => Ok(S::Executing),
            (S::Executing, E::ExecutionSucceeded) => Ok(S::Executed),
            (S::Executing, E::ExecutionFailed) => Ok(S::Failed),
            (S::Executing, E::Interrupt) => Ok(S::Unknown),
            (S::Unknown, E::MarkDone) => Ok(S::Executed),
            (S::Unknown, E::Drop) => Ok(S::Failed),
            (S::Approved, E::Abandon) => Ok(S::Failed),
            (state, event) => Err(InvalidTransition { state, event }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalEvent as E;
    use super::ApprovalState as S;

    const STATES: [S; 8] = [
        S::Draft,
        S::Pending,
        S::Approved,
        S::Rejected,
        S::Executing,
        S::Executed,
        S::Failed,
        S::Unknown,
    ];
    const EVENTS: [E; 9] = [
        E::Approve,
        E::Reject,
        E::StartExecution,
        E::ExecutionSucceeded,
        E::ExecutionFailed,
        E::Interrupt,
        E::MarkDone,
        E::Drop,
        E::Abandon,
    ];

    #[test]
    fn only_the_listed_transitions_are_allowed() {
        let allowed = [
            (S::Pending, E::Approve, S::Approved),
            (S::Pending, E::Reject, S::Rejected),
            (S::Draft, E::Reject, S::Rejected),
            (S::Approved, E::StartExecution, S::Executing),
            (S::Executing, E::ExecutionSucceeded, S::Executed),
            (S::Executing, E::ExecutionFailed, S::Failed),
            (S::Executing, E::Interrupt, S::Unknown),
            (S::Unknown, E::MarkDone, S::Executed),
            (S::Unknown, E::Drop, S::Failed),
            (S::Approved, E::Abandon, S::Failed),
        ];
        for state in STATES {
            for event in EVENTS {
                let expected = allowed
                    .iter()
                    .find(|(s, e, _)| *s == state && *e == event)
                    .map(|(_, _, next)| *next);
                assert_eq!(state.on(event).ok(), expected, "{state:?} + {event:?}");
            }
        }
    }

    #[test]
    fn a_draft_is_never_approved_and_approval_is_not_repeated() {
        assert!(S::Draft.on(E::Approve).is_err());
        for state in [S::Approved, S::Executing, S::Executed, S::Failed, S::Rejected] {
            assert!(state.on(E::Approve).is_err(), "{state:?}");
        }
    }

    #[test]
    fn a_cut_call_never_runs_again() {
        // From `unknown` no event leads to `approved` or `executing`. A retry is a new item.
        for event in EVENTS {
            let next = S::Unknown.on(event);
            assert!(!matches!(next, Ok(S::Approved | S::Executing)), "{event:?}: {next:?}");
        }
        assert!(S::Unknown.on(E::StartExecution).is_err());
        assert!(S::Unknown.on(E::Approve).is_err());
        // Only a running call can be cut, and only a waiting approval can be abandoned.
        assert!(S::Approved.on(E::Interrupt).is_err());
        assert!(S::Executing.on(E::Abandon).is_err());
    }

    #[test]
    fn names_round_trip() {
        for state in STATES {
            assert_eq!(S::parse(state.as_str()), Some(state));
        }
        assert_eq!(S::parse("done"), None);
    }
}
