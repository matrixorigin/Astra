//! Wire types for the generic Evaluation control-plane endpoints.
//!
//! These requests carry only client intent. The server owns the authenticated
//! owner boundary, freezes the plan, and reads observations from the durable
//! execution store; clients cannot submit terminal evidence through this API.

use super::experiment::ExperimentSpec;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationExperimentCreateRequest {
    pub spec: ExperimentSpec,
    pub submission_idempotency_key: String,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct EvaluationReportQuery {
    pub baseline_label: Option<String>,
    pub candidate_label: Option<String>,
}

/// User intent for starting one already-frozen trial. The experiment and
/// trial identities are supplied by the authenticated URL; trusted runtime
/// material is derived server-side from the frozen plan.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTrialStartRequest {
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_time_budget_secs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTrialStartResponse {
    pub experiment_id: String,
    pub trial_id: String,
    pub session_id: String,
    pub run_id: String,
    pub status: String,
}
