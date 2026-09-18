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
