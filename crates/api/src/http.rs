//! The HTTP form of [`EngineApi`](crate::EngineApi), shared by the server and the client.
//!
//! Every request carries `Authorization: Bearer <access token>`. A token gives access
//! to one profile. Bodies are JSON.
//!
//! | Method | Path | Request body | Reply body |
//! |---|---|---|---|
//! | GET | `/v1/profiles` | | `[ProfileInfo]` (the caller's profile) |
//! | GET | `/v1/profiles/{p}/config` | | `ProfileConfig` |
//! | PUT | `/v1/profiles/{p}/config` | `ProfileConfig` | |
//! | GET | `/v1/profiles/{p}/snapshot` | | `ProfileSnapshot` |
//! | POST | `/v1/profiles/{p}/events` | `InboundEvent` | `Outcome` |
//! | POST | `/v1/profiles/{p}/inbox/{id}/approve` | `ApproveBody` | `Outcome` |
//! | POST | `/v1/profiles/{p}/inbox/{id}/reject` | `RejectBody` | |
//! | PUT | `/v1/profiles/{p}/kill-switch` | `KillSwitchBody` | `KillSwitchBody` |
//! | POST | `/v1/profiles/{p}/replay` | `ReplayCase` | `ReplayResult` |
//! | GET | `/v1/profiles/{p}/connectors/{c}/tools` | | `ToolsReport` |
//! | POST | `/v1/profiles/{p}/connectors/{c}/tools/approve` | `ApproveToolsBody` | |
//! | GET | `/v1/profiles/{p}/privacy` | | `PrivacyConfig` |
//! | PUT | `/v1/profiles/{p}/privacy` | `PrivacyUpdate` | |
//! | POST | `/v1/profiles/{p}/polls/{index}/test` | | `PollPreview` |
//! | GET | `/v1/profiles/{p}/connections` | | `Connections` |
//! | PUT | `/v1/profiles/{p}/connections/llm` | `LlmUpdate` | |
//! | PUT | `/v1/profiles/{p}/connections/connectors/{c}` | `ConnectorUpdate` | |
//! | DELETE | `/v1/profiles/{p}/connections/connectors/{c}` | | |
//! | GET | `/v1/profiles/{p}/wiki` | | `[WikiSummary]` |
//! | GET | `/v1/profiles/{p}/wiki/{slug}` | | `WikiPage` |
//! | PUT | `/v1/profiles/{p}/wiki/{slug}` | `WikiPageUpdate` | `WikiPage` |
//! | DELETE | `/v1/profiles/{p}/wiki/{slug}` | | |
//!
//! A failure replies with a non-2xx status and an `ErrorBody`.

use crate::{ApiError, Edit};
use serde::{Deserialize, Serialize};

pub const PROFILES: &str = "/v1/profiles";

pub fn profile_path(profile: &str, rest: &str) -> String {
    format!("{PROFILES}/{profile}/{rest}")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApproveBody {
    pub edit: Option<Edit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveToolsBody {
    /// Tool name to fingerprint, as the caller saw them.
    pub shown: bunsin_core::tool::PinSet,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectBody {
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillSwitchBody {
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub error: ApiError,
}

/// The HTTP status of each error kind.
pub fn status_of(error: &ApiError) -> u16 {
    match error {
        ApiError::Invalid(_) => 400,
        ApiError::Unauthorized(_) => 401,
        ApiError::NotFound(_) => 404,
        ApiError::NotAllowed(_) => 409,
        ApiError::Conflict(_) => 412,
        ApiError::Internal(_) => 500,
        ApiError::Unreachable(_) => 502,
    }
}
