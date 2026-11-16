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
//! | GET | `/v1/profiles/{p}/permissions` | | `Permissions` |
//! | PUT | `/v1/profiles/{p}/permissions` | `PermissionsUpdate` | |
//! | GET | `/v1/profiles/{p}/principles` | | `[String]` |
//! | PUT | `/v1/profiles/{p}/principles` | `PrinciplesUpdate` | |
//! | GET | `/v1/profiles/{p}/polls` | | `[PollConfig]` |
//! | PUT | `/v1/profiles/{p}/polls` | `PollsUpdate` | |
//! | POST | `/v1/profiles/{p}/polls/{index}/test` | | `PollPreview` |
//! | POST | `/v1/profiles/{p}/llm/test` | | `LlmTest` |
//! | POST | `/v1/profiles/{p}/llm-usage` | | `[LlmUsage]` |
//! | POST | `/v1/profiles/{p}/resolve-unknown` | `ResolveUnknownBody` | `Outcome or null` |
//! | POST | `/v1/profiles/{p}/corrections` | | `[CorrectionRecord]` |
//! | POST | `/v1/profiles/{p}/retire-correction` | `RetireCorrectionBody` | |
//! | POST | `/v1/profiles/{p}/autonomy` | | `Autonomy` |
//! | POST | `/v1/profiles/{p}/report-autonomous` | `ReportAutonomousBody` | |
//! | POST | `/v1/profiles/{p}/clear-autonomy-hold` | `ClearAutonomyHoldBody` | |
//! | POST | `/v1/profiles/{p}/add-replay-cases` | `AddReplayCasesBody` | `[i64]` |
//! | POST | `/v1/profiles/{p}/replay-cases` | | `[ReplayCaseRecord]` |
//! | POST | `/v1/profiles/{p}/delete-replay-case` | `DeleteReplayCaseBody` | |
//! | POST | `/v1/profiles/{p}/start-eval` | | `i64` |
//! | POST | `/v1/profiles/{p}/eval-report` | | `EvalReport` |
//! | POST | `/v1/profiles/{p}/extract-principles` | `ExtractPrinciplesBody` | `[PrincipleCandidate]` |
//! | POST | `/v1/profiles/{p}/principle-candidates` | | `[PrincipleCandidate]` |
//! | POST | `/v1/profiles/{p}/adopt-principle` | `AdoptPrinciple` | |
//! | POST | `/v1/profiles/{p}/dismiss-principle` | `DismissPrincipleBody` | |
//! | GET | `/v1/profiles/{p}/connections` | | `Connections` |
//! | PUT | `/v1/profiles/{p}/connections/llm` | `LlmUpdate` | |
//! | PUT | `/v1/profiles/{p}/connections/connectors/{c}` | `ConnectorUpdate` | |
//! | DELETE | `/v1/profiles/{p}/connections/connectors/{c}` | | |
//! | PUT | `/v1/profiles/{p}/connections/connectors/{c}/command` | `CommandConnectorUpdate` | |
//! | GET | `/v1/profiles/{p}/wiki` | | `[WikiSummary]` |
//! | GET | `/v1/profiles/{p}/wiki/{slug}` | | `WikiPage` |
//! | PUT | `/v1/profiles/{p}/wiki/{slug}` | `WikiPageUpdate` | `WikiPage` |
//! | DELETE | `/v1/profiles/{p}/wiki/{slug}` | | |
//! | GET | `/v1/profiles/{p}/wiki/{slug}/history` | | `[WikiPage or null]` |
//!
//! The newer calls use `POST /v1/profiles/{p}/<method name in kebab case>`, with the arguments
//! as a JSON body.
//!
//! A failure replies with a non-2xx status and an `ErrorBody`.

use crate::{ApiError, Edit, ReplayCase, Resolution};
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
pub struct ResolveUnknownBody {
    pub inbox_id: i64,
    pub resolution: Resolution,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireCorrectionBody {
    pub id: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportAutonomousBody {
    pub inbox_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClearAutonomyHoldBody {
    /// `<connector>/<tool>`.
    pub tool: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddReplayCasesBody {
    pub cases: Vec<ReplayCase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteReplayCaseBody {
    pub id: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractPrinciplesBody {
    pub source: bunsin_core::candidates::PrincipleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DismissPrincipleBody {
    pub id: i64,
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
