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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalEvent {
    Approve,
    Reject,
    StartExecution,
    ExecutionSucceeded,
    ExecutionFailed,
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
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        use ApprovalState::*;
        [
            Draft, Pending, Approved, Rejected, Executing, Executed, Failed,
        ]
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
            (state, event) => Err(InvalidTransition { state, event }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ApprovalEvent as E;
    use super::ApprovalState as S;

    const STATES: [S; 7] = [
        S::Draft,
        S::Pending,
        S::Approved,
        S::Rejected,
        S::Executing,
        S::Executed,
        S::Failed,
    ];
    const EVENTS: [E; 5] = [
        E::Approve,
        E::Reject,
        E::StartExecution,
        E::ExecutionSucceeded,
        E::ExecutionFailed,
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
        for state in [
            S::Approved,
            S::Executing,
            S::Executed,
            S::Failed,
            S::Rejected,
        ] {
            assert!(state.on(E::Approve).is_err(), "{state:?}");
        }
    }

    #[test]
    fn names_round_trip() {
        for state in STATES {
            assert_eq!(S::parse(state.as_str()), Some(state));
        }
        assert_eq!(S::parse("done"), None);
    }
}
