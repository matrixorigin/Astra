//! Pure preparation for starting one frozen evaluation trial.
//!
//! This module owns the boundary between user intent and the canonical Run
//! lifecycle. It derives stable identities and validates the small execution
//! profile supported by the first adapter; it does not create sessions, call
//! providers, or mutate evaluation state.

use super::api::EvaluationTrialStartRequest;
use super::durable::{EvaluationExperimentRecord, EvaluationTrialBindingRecord};
use super::execution::{EvaluationRunAdmission, EvaluationSkillRevision};
use super::experiment::EvaluationTargetKind;
use super::{content_fingerprint, prompt_context_fingerprint};
use astra_core::canonical_json_string;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use thiserror::Error;

const MAX_MESSAGE_BYTES: usize = 256 * 1024;
const MAX_SKILL_NAME_BYTES: usize = 128;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum EvaluationBootstrapError {
    #[error("invalid evaluation start request: {0}")]
    InvalidInput(String),
    #[error("evaluation start conflicts with the frozen plan: {0}")]
    Conflict(String),
    #[error("evaluation target is not executable by the current adapter: {0}")]
    Unsupported(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationTrialStartPlan {
    pub experiment_id: String,
    pub trial_id: String,
    pub session_id: String,
    pub run_id: String,
    pub request_fingerprint: String,
    pub message: String,
    pub revision_content: Option<String>,
    pub model_offering_id: String,
    pub execution_time_budget_secs: u64,
    pub admission: EvaluationRunAdmission,
}

/// Validate the frozen trial and derive every server-owned identity needed by
/// the canonical Run entrypoint. The first execution adapter intentionally
/// accepts only a fixed text case with no extra request context.
pub fn prepare_trial_start(
    owner_user_id: &str,
    experiment: &EvaluationExperimentRecord,
    trial: &EvaluationTrialBindingRecord,
    request: &EvaluationTrialStartRequest,
) -> Result<EvaluationTrialStartPlan, EvaluationBootstrapError> {
    if owner_user_id.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "owner_user_id must not be empty".to_string(),
        ));
    }
    if experiment.owner_user_id != owner_user_id || trial.owner_user_id != owner_user_id {
        return Err(EvaluationBootstrapError::Conflict(
            "evaluation plan is owned by another user".to_string(),
        ));
    }
    if experiment.experiment_id != trial.experiment_id {
        return Err(EvaluationBootstrapError::Conflict(
            "trial does not belong to the requested experiment".to_string(),
        ));
    }
    if trial.binding_status != "planned" && trial.binding_status != "bound" {
        return Err(EvaluationBootstrapError::Conflict(format!(
            "trial has unsupported binding status {}",
            trial.binding_status
        )));
    }
    experiment
        .spec
        .validate_trial_identity(&trial.trial)
        .map_err(EvaluationBootstrapError::Conflict)?;
    if request.message.trim().is_empty() {
        return Err(EvaluationBootstrapError::InvalidInput(
            "message must not be empty".to_string(),
        ));
    }
    if request.message.len() > MAX_MESSAGE_BYTES {
        return Err(EvaluationBootstrapError::InvalidInput(format!(
            "message exceeds the {MAX_MESSAGE_BYTES} byte limit"
        )));
    }

    // The initial adapter freezes the full Context assembly to exactly this
    // text payload. A caller cannot smuggle an unrecorded context through a
    // start request and still receive an Available observation.
    let input_content_hash = prompt_context_fingerprint(&request.message, &[], &[], None);
    if input_content_hash != trial.trial.input_content_hash
        || input_content_hash != experiment.spec.conditions.context_snapshot_hash
    {
        return Err(EvaluationBootstrapError::Conflict(
            "message does not match the frozen trial input identity".to_string(),
        ));
    }

    let revision = match trial.trial.arm {
        super::ComparisonArm::Baseline => &experiment.spec.target.baseline,
        super::ComparisonArm::Candidate => &experiment.spec.target.candidate,
    };
    let (revision_content, skill_revision) = match &experiment.spec.target.kind {
        EvaluationTargetKind::Prompt => {
            if request.skill_name.is_some() {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Prompt evaluation does not accept skill_name".to_string(),
                ));
            }
            let Some(content) = request.revision_content.clone() else {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Prompt evaluation requires revision_content".to_string(),
                ));
            };
            if content_fingerprint(&content) != revision.content_hash {
                return Err(EvaluationBootstrapError::Conflict(
                    "revision_content does not match the frozen revision".to_string(),
                ));
            }
            (Some(content), None)
        }
        EvaluationTargetKind::Skill => {
            if request.revision_content.is_some() {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Skill evaluation does not accept revision_content".to_string(),
                ));
            }
            let Some(skill_name) = request.skill_name.clone() else {
                return Err(EvaluationBootstrapError::InvalidInput(
                    "Skill evaluation requires skill_name".to_string(),
                ));
            };
            if skill_name.trim().is_empty() || skill_name.len() > MAX_SKILL_NAME_BYTES {
                return Err(EvaluationBootstrapError::InvalidInput(format!(
                    "skill_name must be non-empty and at most {MAX_SKILL_NAME_BYTES} bytes"
                )));
            }
            let skill_revision = EvaluationSkillRevision {
                skill_name,
                revision_id: revision.revision_id.clone(),
                content_hash: revision.content_hash.clone(),
            };
            skill_revision
                .validate_shape()
                .map_err(EvaluationBootstrapError::InvalidInput)?;
            (None, Some(skill_revision))
        }
        other => {
            return Err(EvaluationBootstrapError::Unsupported(format!(
                "target kind {other:?}"
            )));
        }
    };

    let execution_time_budget_secs = request
        .execution_time_budget_secs
        .unwrap_or(experiment.spec.budget.max_wall_time_secs);
    if execution_time_budget_secs == 0
        || execution_time_budget_secs > experiment.spec.budget.max_wall_time_secs
    {
        return Err(EvaluationBootstrapError::Conflict(
            "execution_time_budget_secs must be between 1 and the frozen wall-time budget"
                .to_string(),
        ));
    }

    let identity_payload = json!({
        "schema_version": 1,
        "owner_user_id": owner_user_id,
        "experiment_id": experiment.experiment_id,
        "trial_id": trial.trial_id,
    });
    let identity_canonical = canonical_json_string(&identity_payload);
    let identity_digest = format!("{:x}", Sha256::digest(identity_canonical.as_bytes()));
    let session_id = format!("evs_{}", &identity_digest[..60]);
    let run_id = format!("evr_{}", &identity_digest[4..64]);

    match (
        trial.binding_status.as_str(),
        trial.session_id.as_deref(),
        trial.run_id.as_deref(),
    ) {
        ("planned", None, None) => {}
        ("bound", Some(bound_session), Some(bound_run))
            if bound_session == session_id && bound_run == run_id => {}
        ("planned", Some(_), _) | ("planned", _, Some(_)) => {
            return Err(EvaluationBootstrapError::Conflict(
                "planned trial has a partial canonical Run binding".to_string(),
            ));
        }
        ("bound", _, _) => {
            return Err(EvaluationBootstrapError::Conflict(
                "trial is bound to a different canonical session or Run".to_string(),
            ));
        }
        (status, _, _) => {
            return Err(EvaluationBootstrapError::Conflict(format!(
                "trial has unsupported binding status {status}"
            )));
        }
    }

    // Normalize the default budget before hashing so omitted and explicit
    // frozen-budget requests are the same logical start intent.
    let request_payload = json!({
        "schema_version": 1,
        "owner_user_id": owner_user_id,
        "experiment_id": experiment.experiment_id,
        "trial_id": trial.trial_id,
        "message": request.message,
        "revision_content": revision_content,
        "skill_name": skill_revision.as_ref().map(|revision| &revision.skill_name),
        "execution_time_budget_secs": execution_time_budget_secs,
    });
    let request_fingerprint = format!(
        "{:x}",
        Sha256::digest(canonical_json_string(&request_payload).as_bytes())
    );
    let admission = EvaluationRunAdmission {
        experiment_id: experiment.experiment_id.clone(),
        trial_id: trial.trial_id.clone(),
        input_content_hash: trial.trial.input_content_hash.clone(),
        revision_content_hash: revision.content_hash.clone(),
        skill_revision,
        receipt_ids: Vec::new(),
        snapshot_envelope: None,
    };
    Ok(EvaluationTrialStartPlan {
        experiment_id: experiment.experiment_id.clone(),
        trial_id: trial.trial_id.clone(),
        session_id,
        run_id,
        request_fingerprint,
        message: request.message.clone(),
        revision_content,
        model_offering_id: experiment.spec.conditions.model_binding.clone(),
        execution_time_budget_secs,
        admission,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::assessment::ComparisonArm;
    use crate::evaluation::experiment::{
        DataIsolation, EvaluationBudget, EvaluationCase, EvaluationTarget, ExperimentSpec,
        FrozenConditions, MemoryIsolation, RevisionRef, TrialOrder,
    };

    fn fixtures() -> (
        EvaluationExperimentRecord,
        EvaluationTrialBindingRecord,
        String,
    ) {
        let message = "fixed input".to_string();
        let input_hash = prompt_context_fingerprint(&message, &[], &[], None);
        let baseline = "baseline prompt".to_string();
        let candidate = "candidate prompt".to_string();
        let spec = ExperimentSpec {
            schema_version: super::super::experiment::EXPERIMENT_SCHEMA_VERSION,
            experiment_id: "exp-bootstrap".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Prompt,
                baseline: RevisionRef {
                    revision_id: "base".to_string(),
                    content_hash: content_fingerprint(&baseline),
                },
                candidate: RevisionRef {
                    revision_id: "cand".to_string(),
                    content_hash: content_fingerprint(&candidate),
                },
            },
            cases: vec![EvaluationCase {
                case_id: "case-1".to_string(),
                input_snapshot_ref: "input://case-1".to_string(),
                input_content_hash: input_hash.clone(),
                verifier_id: "verifier".to_string(),
                verifier_version: "1".to_string(),
                holdout: false,
            }],
            repetitions: 1,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model-1".to_string(),
                provider_binding: "provider-1".to_string(),
                context_snapshot_hash: input_hash,
                tool_policy_hash: "sha256:policy".to_string(),
                cache_policy: "provider_default_recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
            },
            budget: EvaluationBudget {
                max_trials: 2,
                max_concurrency: 1,
                max_wall_time_secs: 30,
            },
        };
        let spec_fingerprint = spec.spec_fingerprint().expect("fingerprint");
        let planned = spec.plan_trials().expect("trials");
        let trial = planned
            .into_iter()
            .find(|trial| trial.arm == ComparisonArm::Baseline)
            .expect("baseline");
        let binding = EvaluationTrialBindingRecord {
            owner_user_id: "owner-1".to_string(),
            trial_id: trial.trial_id.clone(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint,
            trial,
            binding_status: "planned".to_string(),
            session_id: None,
            run_id: None,
            run_generation: None,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        let record = EvaluationExperimentRecord {
            owner_user_id: "owner-1".to_string(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint: spec.spec_fingerprint().expect("fingerprint"),
            spec,
            submission_idempotency_key: "submit-1".to_string(),
            planned_trial_count: 2,
            created_at: "now".to_string(),
            updated_at: "now".to_string(),
        };
        (record, binding, message)
    }

    #[test]
    fn derives_stable_session_run_and_normalized_request_identity() {
        let (experiment, trial, message) = fixtures();
        let request = EvaluationTrialStartRequest {
            message: message.clone(),
            revision_content: Some("baseline prompt".to_string()),
            skill_name: None,
            execution_time_budget_secs: None,
        };
        let explicit = EvaluationTrialStartRequest {
            execution_time_budget_secs: Some(30),
            ..request.clone()
        };
        let first = prepare_trial_start("owner-1", &experiment, &trial, &request).expect("plan");
        let second = prepare_trial_start("owner-1", &experiment, &trial, &explicit).expect("plan");
        assert_eq!(first.session_id, second.session_id);
        assert_eq!(first.run_id, second.run_id);
        assert_eq!(first.request_fingerprint, second.request_fingerprint);
        assert!(first.session_id.starts_with("evs_"));
        assert!(first.run_id.starts_with("evr_"));
    }

    #[test]
    fn rejects_context_or_revision_drift_before_execution() {
        let (experiment, trial, _) = fixtures();
        let mut request = EvaluationTrialStartRequest {
            message: "changed".to_string(),
            revision_content: Some("baseline prompt".to_string()),
            skill_name: None,
            execution_time_budget_secs: None,
        };
        assert!(matches!(
            prepare_trial_start("owner-1", &experiment, &trial, &request),
            Err(EvaluationBootstrapError::Conflict(_))
        ));
        request.message = "fixed input".to_string();
        request.revision_content = Some("changed prompt".to_string());
        assert!(matches!(
            prepare_trial_start("owner-1", &experiment, &trial, &request),
            Err(EvaluationBootstrapError::Conflict(_))
        ));
    }

    #[test]
    fn rejects_partial_or_foreign_existing_binding() {
        let (experiment, mut trial, message) = fixtures();
        let request = EvaluationTrialStartRequest {
            message,
            revision_content: Some("baseline prompt".to_string()),
            skill_name: None,
            execution_time_budget_secs: None,
        };
        trial.session_id = Some("evs_partial".to_string());
        let error = prepare_trial_start("owner-1", &experiment, &trial, &request)
            .expect_err("partial binding must not be reused");
        assert!(matches!(error, EvaluationBootstrapError::Conflict(_)));

        trial.binding_status = "bound".to_string();
        trial.run_id = Some("evr_foreign".to_string());
        let error = prepare_trial_start("owner-1", &experiment, &trial, &request)
            .expect_err("foreign binding must not be reused");
        assert!(matches!(error, EvaluationBootstrapError::Conflict(_)));
    }

    #[test]
    fn rejects_a_plan_loaded_for_another_owner() {
        let (mut experiment, trial, message) = fixtures();
        let request = EvaluationTrialStartRequest {
            message,
            revision_content: Some("baseline prompt".to_string()),
            skill_name: None,
            execution_time_budget_secs: None,
        };
        experiment.owner_user_id = "owner-2".to_string();
        let error = prepare_trial_start("owner-1", &experiment, &trial, &request)
            .expect_err("owner-scoped bootstrap must fail closed");
        assert!(matches!(error, EvaluationBootstrapError::Conflict(_)));
    }
}
