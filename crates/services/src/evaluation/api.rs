//! Wire types for the generic Evaluation control-plane endpoints.
//!
//! These requests carry only client intent. The server owns the authenticated
//! owner boundary, freezes the plan, and reads observations from the durable
//! execution store; clients cannot submit terminal evidence through this API.

use super::experiment::{EvaluationTargetKind, ExperimentSpec};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationExperimentCreateRequest {
    pub spec: ExperimentSpec,
    pub submission_idempotency_key: String,
}

/// User-facing intent for a server-owned prepare/freeze operation. It carries
/// content and references, never derived hashes, credentials, receipts, or
/// runtime identities.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationExperimentPrepareRequest {
    pub submission_idempotency_key: String,
    pub target: EvaluationPrepareTarget,
    pub case: EvaluationPrepareCase,
    pub model_offering_id: String,
    pub max_concurrency: u16,
    pub max_wall_time_secs: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationPrepareTarget {
    pub kind: EvaluationTargetKind,
    pub baseline: EvaluationPrepareRevision,
    pub candidate: EvaluationPrepareRevision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationPrepareRevision {
    pub revision_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationPrepareCase {
    pub case_id: String,
    pub message: String,
    #[serde(default)]
    pub holdout: bool,
    /// Required task criterion. Implementation identity is frozen by the server.
    pub verifier_config: super::task_verifier::JsonValueEqualsConfig,
}

#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationExperimentPrepareResponse {
    pub experiment: super::durable::EvaluationExperimentRecord,
    pub trials: Vec<super::durable::EvaluationTrialBindingRecord>,
    pub adapter_profile_version: String,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_content: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_time_budget_secs: Option<u64>,
    /// Must match the frozen Edge selection when the experiment uses one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge_executor_id: Option<String>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn prepare_requires_criterion_and_rejects_client_verifier_identity() {
        let valid = json!({
            "case_id": "case-1",
            "message": "Return the expected JSON",
            "verifier_config": {"expected": {"ok": true}}
        });
        assert!(serde_json::from_value::<EvaluationPrepareCase>(valid.clone()).is_ok());
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove("verifier_config");
        assert!(serde_json::from_value::<EvaluationPrepareCase>(missing).is_err());
        let mut null = valid.clone();
        null["verifier_config"] = serde_json::Value::Null;
        assert!(serde_json::from_value::<EvaluationPrepareCase>(null).is_err());
        for field in ["verifier_id", "verifier_version", "task_verifier"] {
            let mut forged = valid.clone();
            forged[field] = json!("client-selected");
            assert!(serde_json::from_value::<EvaluationPrepareCase>(forged).is_err());
        }
    }
}
