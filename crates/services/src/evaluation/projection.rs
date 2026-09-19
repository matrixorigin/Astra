//! Owner-scoped read models for controlled evaluation progress.
//!
//! Projection is deliberately read-only. The Run/Session lifecycle remains
//! the only execution authority and the observation store remains the only
//! terminal evaluation fact. A projection can therefore say that a trial is
//! waiting for evidence or that a status read is unavailable without
//! inventing a second lifecycle or silently marking a trial successful.

use super::assessment::TrialObservation;
use super::durable::{
    DatabaseEvaluationPlanStore, EvaluationExperimentRecord, EvaluationPersistenceError,
    EvaluationTrialBindingRecord, count_trials_tx, load_experiment_read_tx, load_trial_bindings_tx,
    validate_bounded, validate_owner,
};
use super::execution::{
    DatabaseEvaluationObservationStore, EvaluationExecutionError, EvaluationObservationRecord,
};
use super::task_assessment::{TaskAssessmentError, TaskAssessmentRecord};
use astra_core::{
    STATUS_CANCELLED, STATUS_COMPLETED, STATUS_FAILED, STATUS_PAUSED, STATUS_RUNNING,
    STATUS_WAITING, SharedPool,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationTrialLifecycle {
    Planned,
    Running,
    Waiting,
    Paused,
    TerminalAwaitingObservation,
    Observed,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationTrialProjection {
    pub binding: EvaluationTrialBindingRecord,
    pub run_status: Option<String>,
    pub lifecycle: EvaluationTrialLifecycle,
    pub observation: Option<EvaluationObservationRecord>,
    pub task_assessment: Option<TaskAssessmentRecord>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationExperimentProjection {
    pub experiment: EvaluationExperimentRecord,
    pub trials: Vec<EvaluationTrialProjection>,
    pub observed_trial_count: usize,
    pub missing_observation_trial_ids: Vec<String>,
    pub unavailable_trial_ids: Vec<String>,
}

#[derive(Debug, Error)]
pub enum EvaluationProjectionError {
    #[error(transparent)]
    Persistence(#[from] EvaluationPersistenceError),
    #[error(transparent)]
    Execution(#[from] EvaluationExecutionError),
    #[error(transparent)]
    Assessment(#[from] TaskAssessmentError),
    #[error("evaluation projection conflict: {0}")]
    Conflict(String),
}

impl EvaluationExperimentProjection {
    /// Build a projection from already owner-scoped records. Callers that
    /// read from the database should use [`DatabaseEvaluationProjectionStore`]
    /// so all inputs come from the same authenticated owner boundary.
    pub fn from_records(
        experiment: EvaluationExperimentRecord,
        bindings: Vec<EvaluationTrialBindingRecord>,
        run_statuses: BTreeMap<String, String>,
        observations: Vec<EvaluationObservationRecord>,
        assessments: Vec<TaskAssessmentRecord>,
    ) -> Result<Self, EvaluationProjectionError> {
        let owner = experiment.owner_user_id.as_str();
        let experiment_id = experiment.experiment_id.as_str();
        let fingerprint = experiment.spec_fingerprint.as_str();
        let planned = experiment
            .spec
            .plan_trials()
            .map_err(EvaluationProjectionError::Conflict)?;
        if planned.len() != experiment.planned_trial_count || bindings.len() != planned.len() {
            return Err(EvaluationProjectionError::Conflict(
                "persisted trial count does not match the frozen plan".to_string(),
            ));
        }
        let expected_by_id = planned
            .into_iter()
            .map(|trial| (trial.trial_id.clone(), trial))
            .collect::<BTreeMap<_, _>>();
        let mut observation_by_trial = BTreeMap::<String, EvaluationObservationRecord>::new();
        for observation in observations {
            if observation.owner_user_id != owner
                || observation.experiment_id != experiment_id
                || observation.observation.experiment_fingerprint != fingerprint
            {
                return Err(EvaluationProjectionError::Conflict(format!(
                    "observation {} is outside the experiment owner/fingerprint boundary",
                    observation.observation_id
                )));
            }
            if observation.trial_id != observation.observation.trial_id {
                return Err(EvaluationProjectionError::Conflict(format!(
                    "observation {} outer trial identity does not match its content",
                    observation.observation_id
                )));
            }
            if let Some(previous) = observation_by_trial.insert(
                observation.observation.trial_id.clone(),
                observation.clone(),
            ) && previous != observation
            {
                return Err(EvaluationProjectionError::Conflict(format!(
                    "trial {} has conflicting observations",
                    observation.observation.trial_id
                )));
            }
        }

        let mut seen_trials = BTreeSet::new();
        let mut trials = Vec::with_capacity(bindings.len());
        for binding in bindings {
            let Some(expected_trial) = expected_by_id.get(&binding.trial_id) else {
                return Err(EvaluationProjectionError::Conflict(format!(
                    "trial {} is not in the frozen plan",
                    binding.trial_id
                )));
            };
            if binding.owner_user_id != owner
                || binding.experiment_id != experiment_id
                || binding.spec_fingerprint != fingerprint
                || binding.trial != *expected_trial
                || !seen_trials.insert(binding.trial_id.clone())
            {
                return Err(EvaluationProjectionError::Conflict(format!(
                    "trial {} is outside the experiment or duplicated",
                    binding.trial_id
                )));
            }
            let observation = observation_by_trial.remove(&binding.trial_id);
            let run_status = run_statuses.get(&binding.trial_id).cloned();
            if let Some(observation) = &observation {
                if binding.binding_status != "bound"
                    || binding.session_id.as_deref() != Some(observation.session_id.as_str())
                    || binding.run_id.as_deref() != Some(observation.execution_run_id.as_str())
                    || binding.run_generation != Some(observation.admission_run_generation)
                {
                    return Err(EvaluationProjectionError::Conflict(format!(
                        "observation {} is not tied to the persisted trial run identity",
                        observation.observation_id
                    )));
                }
                if observation.trial_id != binding.trial_id
                    || observation.observation.case_id != binding.trial.case_id
                    || observation.observation.arm != binding.trial.arm
                    || observation.observation.repetition != binding.trial.repetition
                {
                    return Err(EvaluationProjectionError::Conflict(format!(
                        "observation {} does not match trial {}",
                        observation.observation_id, binding.trial_id
                    )));
                }
            }
            let lifecycle = if observation.is_some() {
                EvaluationTrialLifecycle::Observed
            } else if binding.binding_status == "planned" {
                EvaluationTrialLifecycle::Planned
            } else if binding.binding_status != "bound" {
                EvaluationTrialLifecycle::Unavailable
            } else {
                match run_status.as_deref() {
                    Some(STATUS_COMPLETED | STATUS_FAILED | STATUS_CANCELLED | "delegated") => {
                        EvaluationTrialLifecycle::TerminalAwaitingObservation
                    }
                    Some(STATUS_RUNNING) => EvaluationTrialLifecycle::Running,
                    Some(STATUS_WAITING) => EvaluationTrialLifecycle::Waiting,
                    Some(STATUS_PAUSED) => EvaluationTrialLifecycle::Paused,
                    Some(_) => EvaluationTrialLifecycle::Unavailable,
                    None => EvaluationTrialLifecycle::Unavailable,
                }
            };
            trials.push(EvaluationTrialProjection {
                binding,
                run_status,
                lifecycle,
                observation,
                task_assessment: None,
            });
        }
        if let Some((trial_id, _)) = observation_by_trial.into_iter().next() {
            return Err(EvaluationProjectionError::Conflict(format!(
                "observation references a trial outside the persisted plan: {trial_id}"
            )));
        }
        if let Some(trial_id) = run_statuses
            .keys()
            .find(|trial_id| !seen_trials.contains(*trial_id))
        {
            return Err(EvaluationProjectionError::Conflict(format!(
                "run status references a trial outside the persisted plan: {trial_id}"
            )));
        }
        let observed_trial_count = trials
            .iter()
            .filter(|trial| trial.observation.is_some())
            .count();
        let missing_observation_trial_ids = trials
            .iter()
            .filter(|trial| trial.observation.is_none())
            .map(|trial| trial.binding.trial_id.clone())
            .collect::<Vec<_>>();
        let unavailable_trial_ids = trials
            .iter()
            .filter(|trial| matches!(trial.lifecycle, EvaluationTrialLifecycle::Unavailable))
            .map(|trial| trial.binding.trial_id.clone())
            .collect::<Vec<_>>();
        for assessment in assessments {
            let trial = trials
                .iter_mut()
                .find(|trial| trial.binding.trial_id == assessment.trial_id)
                .ok_or_else(|| {
                    EvaluationProjectionError::Conflict(
                        "assessment references an unplanned trial".into(),
                    )
                })?;
            let observation = trial.observation.as_ref().ok_or_else(|| {
                EvaluationProjectionError::Conflict("assessment has no terminal observation".into())
            })?;
            assessment.validate_binding(&experiment, observation)?;
            if let Some(previous) = &trial.task_assessment {
                if previous != &assessment {
                    return Err(EvaluationProjectionError::Conflict(
                        "trial has conflicting task assessments".into(),
                    ));
                }
            } else {
                trial.task_assessment = Some(assessment);
            }
        }
        Ok(Self {
            experiment,
            trials,
            observed_trial_count,
            missing_observation_trial_ids,
            unavailable_trial_ids,
        })
    }

    pub fn observations(&self) -> Vec<TrialObservation> {
        self.trials
            .iter()
            .filter_map(|trial| trial.observation.as_ref())
            .map(|record| record.observation.clone())
            .collect()
    }
}

#[derive(Clone)]
pub struct DatabaseEvaluationProjectionStore {
    plan_store: DatabaseEvaluationPlanStore,
}

impl DatabaseEvaluationProjectionStore {
    pub fn new(pool: SharedPool) -> Self {
        Self {
            plan_store: DatabaseEvaluationPlanStore::new(pool),
        }
    }

    pub async fn load_experiment(
        &self,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<EvaluationExperimentProjection, EvaluationProjectionError> {
        validate_owner(owner_user_id)?;
        validate_bounded("experiment_id", experiment_id, 128)?;
        let pool = self.plan_store.shared_pool();
        let mut tx = pool.get().begin().await.map_err(|source| {
            EvaluationProjectionError::Persistence(EvaluationPersistenceError::Database {
                operation: "begin_load_evaluation_projection",
                source,
            })
        })?;
        let experiment = load_experiment_read_tx(&mut tx, owner_user_id, experiment_id)
            .await?
            .ok_or_else(|| EvaluationPersistenceError::NotFound(experiment_id.to_string()))?;
        let trial_count = count_trials_tx(&mut tx, owner_user_id, experiment_id).await?;
        if trial_count != experiment.planned_trial_count {
            return Err(EvaluationProjectionError::Persistence(
                EvaluationPersistenceError::Conflict(format!(
                    "experiment {experiment_id} trial count is inconsistent"
                )),
            ));
        }
        let bindings = load_trial_bindings_tx(&mut tx, owner_user_id, experiment_id).await?;
        let run_statuses = DatabaseEvaluationPlanStore::list_trial_run_statuses_tx(
            &mut tx,
            owner_user_id,
            experiment_id,
        )
        .await?;
        let observations = DatabaseEvaluationObservationStore::list_observations_in_transaction(
            &mut tx,
            owner_user_id,
            experiment_id,
        )
        .await?;
        let assessments = super::task_assessment::list_assessments_in_transaction(
            &mut tx,
            owner_user_id,
            experiment_id,
        )
        .await?;
        tx.commit().await.map_err(|source| {
            EvaluationProjectionError::Persistence(EvaluationPersistenceError::Database {
                operation: "commit_load_evaluation_projection",
                source,
            })
        })?;
        EvaluationExperimentProjection::from_records(
            experiment,
            bindings,
            run_statuses,
            observations,
            assessments,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::evaluation::experiment::{
        DataIsolation, EvaluationBudget, EvaluationCase, EvaluationTarget, EvaluationTargetKind,
        ExperimentSpec, FrozenConditions, MemoryIsolation, RevisionRef, TrialOrder, TrialUnit,
    };
    use crate::evaluation::{EvidenceRef, TrialStatus};

    fn fixture() -> (
        EvaluationExperimentRecord,
        Vec<EvaluationTrialBindingRecord>,
        Vec<EvaluationObservationRecord>,
    ) {
        let spec = ExperimentSpec {
            schema_version: 1,
            experiment_id: "exp-projection".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Prompt,
                baseline: RevisionRef {
                    revision_id: "base".to_string(),
                    content_hash:
                        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                            .to_string(),
                    content: None,
                },
                candidate: RevisionRef {
                    revision_id: "cand".to_string(),
                    content_hash:
                        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                            .to_string(),
                    content: None,
                },
                skill_name: None,
            },
            cases: vec![EvaluationCase {
                case_id: "case".to_string(),
                input_snapshot_ref: "input://case".to_string(),
                input_content_hash:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_string(),
                holdout: false,
                task_verifier: crate::evaluation::task_verifier::TaskVerifierSpec::freeze(
                    crate::evaluation::task_verifier::JsonValueEqualsConfig {
                        expected: serde_json::json!({"ok": true}),
                    },
                )
                .unwrap(),
                input_content: None,
            }],
            repetitions: 1,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
                execution_config: crate::evaluation::test_support::execution_config(
                    "model", "provider", "case",
                ),
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model".to_string(),
                provider_binding: "provider".to_string(),
                context_snapshot_hash:
                    "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_string(),
                tool_policy_hash:
                    "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                        .to_string(),
                cache_policy: "provider_default_recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
            },
            budget: EvaluationBudget {
                max_trials: 2,
                max_concurrency: 1,
                max_wall_time_secs: 60,
            },
            adapter_profile_version: None,
            measurement_profile:
                crate::evaluation::measurement_profile::MeasurementProfile::InstructionOnlyV1,
        };
        let fingerprint = spec.spec_fingerprint().expect("spec fingerprint");
        let experiment = EvaluationExperimentRecord {
            owner_user_id: "owner".to_string(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint: fingerprint.clone(),
            spec,
            submission_idempotency_key: "submission".to_string(),
            planned_trial_count: 2,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        let trials = experiment.spec.plan_trials().expect("plan trials");
        let bindings = trials
            .into_iter()
            .map(|trial: TrialUnit| EvaluationTrialBindingRecord {
                owner_user_id: "owner".to_string(),
                trial_id: trial.trial_id.clone(),
                experiment_id: experiment.experiment_id.clone(),
                spec_fingerprint: fingerprint.clone(),
                trial,
                binding_status: "planned".to_string(),
                session_id: None,
                run_id: None,
                run_generation: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:00Z".to_string(),
            })
            .collect();
        (experiment, bindings, Vec::new())
    }

    fn observation_for(
        experiment: &EvaluationExperimentRecord,
        binding: &EvaluationTrialBindingRecord,
        session_id: &str,
        run_id: &str,
        generation: u64,
    ) -> EvaluationObservationRecord {
        EvaluationObservationRecord {
            schema_version: super::super::EVALUATION_EXECUTION_SCHEMA_VERSION,
            owner_user_id: experiment.owner_user_id.clone(),
            observation_id: format!("observation-{}", binding.trial_id),
            experiment_id: experiment.experiment_id.clone(),
            trial_id: binding.trial_id.clone(),
            session_id: session_id.to_string(),
            execution_run_id: run_id.to_string(),
            admission_run_generation: generation,
            execution_run_generation: generation,
            observation: TrialObservation {
                experiment_fingerprint: experiment.spec_fingerprint.clone(),
                trial_id: binding.trial_id.clone(),
                case_id: binding.trial.case_id.clone(),
                arm: binding.trial.arm.clone(),
                repetition: binding.trial.repetition,
                status: TrialStatus::Completed,
                measurements: Vec::new(),
                evidence: Vec::new(),
            },
            materialization_receipt_ids: Vec::new(),
            request_fingerprint:
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                    .to_string(),
            idempotency_key: format!("observation-key-{}", binding.trial_id),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn projection_distinguishes_planned_and_unavailable_observation() {
        let (experiment, mut bindings, observations) = fixture();
        bindings[0].binding_status = "bound".to_string();
        bindings[0].session_id = Some("session".to_string());
        bindings[0].run_id = Some("run".to_string());
        bindings[0].run_generation = Some(1);
        let projection = EvaluationExperimentProjection::from_records(
            experiment,
            bindings,
            BTreeMap::new(),
            observations,
            Vec::new(),
        )
        .expect("projection");
        assert_eq!(
            projection.trials[1].lifecycle,
            EvaluationTrialLifecycle::Planned
        );
        assert_eq!(
            projection.trials[0].lifecycle,
            EvaluationTrialLifecycle::Unavailable
        );
        assert_eq!(projection.observed_trial_count, 0);
        assert_eq!(projection.missing_observation_trial_ids.len(), 2);
    }

    #[test]
    fn projection_rejects_foreign_observation_and_conflicting_duplicates() {
        let (experiment, bindings, _) = fixture();
        let observation = EvaluationObservationRecord {
            schema_version: super::super::EVALUATION_EXECUTION_SCHEMA_VERSION,
            owner_user_id: "other-owner".to_string(),
            observation_id: "observation".to_string(),
            experiment_id: experiment.experiment_id.clone(),
            trial_id: bindings[0].trial_id.clone(),
            session_id: "session".to_string(),
            execution_run_id: "run".to_string(),
            admission_run_generation: 1,
            execution_run_generation: 1,
            observation: TrialObservation {
                experiment_fingerprint: experiment.spec_fingerprint.clone(),
                trial_id: bindings[0].trial_id.clone(),
                case_id: bindings[0].trial.case_id.clone(),
                arm: bindings[0].trial.arm.clone(),
                repetition: 0,
                status: TrialStatus::Failed,
                measurements: Vec::new(),
                evidence: vec![EvidenceRef {
                    evidence_id: "trace".to_string(),
                    kind: super::super::EvidenceKind::Trace,
                    availability: super::super::EvidenceAvailability::Missing,
                    content_hash: None,
                    locator: None,
                }],
            },
            materialization_receipt_ids: Vec::new(),
            request_fingerprint:
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                    .to_string(),
            idempotency_key: "observation-key".to_string(),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
        };
        assert!(
            EvaluationExperimentProjection::from_records(
                experiment.clone(),
                bindings.clone(),
                BTreeMap::new(),
                vec![observation.clone()],
                Vec::new(),
            )
            .is_err()
        );
        let mut owned = observation;
        owned.owner_user_id = "owner".to_string();
        let conflict = EvaluationObservationRecord {
            observation: TrialObservation {
                status: TrialStatus::Completed,
                ..owned.observation.clone()
            },
            ..owned.clone()
        };
        assert!(
            EvaluationExperimentProjection::from_records(
                experiment,
                bindings,
                BTreeMap::new(),
                vec![owned, conflict],
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn projection_fences_observation_identity_and_unknown_run_status() {
        let (experiment, mut bindings, _) = fixture();
        bindings[0].binding_status = "bound".to_string();
        bindings[0].session_id = Some("session-0".to_string());
        bindings[0].run_id = Some("run-0".to_string());
        bindings[0].run_generation = Some(1);
        let foreign_observation =
            observation_for(&experiment, &bindings[0], "session-other", "run-0", 1);
        let error = EvaluationExperimentProjection::from_records(
            experiment.clone(),
            bindings.clone(),
            BTreeMap::new(),
            vec![foreign_observation],
            Vec::new(),
        )
        .expect_err("foreign execution identity must be rejected");
        assert!(error.to_string().contains("run identity"));
        let mut recovered = observation_for(&experiment, &bindings[0], "session-0", "run-0", 1);
        recovered.execution_run_generation = 3;
        recovered.observation.status = TrialStatus::Failed;
        let projection = EvaluationExperimentProjection::from_records(
            experiment.clone(),
            bindings.clone(),
            BTreeMap::new(),
            vec![recovered.clone()],
            Vec::new(),
        )
        .expect("persisted recovery observation binds to original admission generation");
        assert_eq!(projection.observed_trial_count, 1);
        recovered.admission_run_generation = 2;
        assert!(
            EvaluationExperimentProjection::from_records(
                experiment.clone(),
                bindings.clone(),
                BTreeMap::new(),
                vec![recovered],
                Vec::new(),
            )
            .is_err(),
            "terminal generation cannot replace the immutable binding generation"
        );

        let mut statuses = BTreeMap::new();
        statuses.insert(bindings[0].trial_id.clone(), "future_status".to_string());
        bindings[1].binding_status = "bound".to_string();
        bindings[1].session_id = Some("session-1".to_string());
        bindings[1].run_id = Some("run-1".to_string());
        bindings[1].run_generation = Some(1);
        statuses.insert(bindings[1].trial_id.clone(), STATUS_WAITING.to_string());
        let projection = EvaluationExperimentProjection::from_records(
            experiment,
            bindings,
            statuses,
            Vec::new(),
            Vec::new(),
        )
        .expect("projection");
        assert_eq!(
            projection.trials[0].lifecycle,
            EvaluationTrialLifecycle::Unavailable
        );
        assert_eq!(
            projection.trials[1].lifecycle,
            EvaluationTrialLifecycle::Waiting
        );
    }

    #[test]
    fn projection_exposes_paused_runs_without_treating_them_as_terminal() {
        let (experiment, mut bindings, _) = fixture();
        for (index, binding) in bindings.iter_mut().enumerate() {
            binding.binding_status = "bound".to_string();
            binding.session_id = Some(format!("session-{index}"));
            binding.run_id = Some(format!("run-{index}"));
            binding.run_generation = Some(1);
        }
        let statuses = bindings
            .iter()
            .map(|binding| (binding.trial_id.clone(), STATUS_PAUSED.to_string()))
            .collect();
        let projection = EvaluationExperimentProjection::from_records(
            experiment,
            bindings,
            statuses,
            Vec::new(),
            Vec::new(),
        )
        .expect("projection");
        assert!(
            projection
                .trials
                .iter()
                .all(|trial| trial.lifecycle == EvaluationTrialLifecycle::Paused)
        );
    }
}
