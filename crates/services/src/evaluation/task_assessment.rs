//! Immutable task assessments derived from canonical terminal output evidence.
//! This is a projection of completed execution, never a second Run lifecycle.

use astra_core::canonical_json_string;
use serde::{Deserialize, Serialize};
use sqlx::{Executor, MySql, Row, Transaction};
use thiserror::Error;

use super::{
    assessment::{
        EvidenceAvailability, EvidenceKind, EvidenceRef, Measurement, MeasurementStatus,
        TrialStatus,
    },
    durable::{
        DatabaseEvaluationPlanStore, EvaluationExperimentRecord, EvaluationPersistenceError,
    },
    execution::{
        DatabaseEvaluationObservationStore, EvaluationExecutionError, EvaluationObservationRecord,
        content_fingerprint,
    },
    task_verifier::{
        MAX_OUTPUT_BYTES, TaskVerifierSpec, TaskVerifierVerdict, verify_complete_output,
    },
};
pub use crate::runs::{CommittedTerminalProof, RunOutputProof};
use crate::runs::{
    TerminalOutputIdentity, TerminalOutputReadError, load_verified_terminal_output_in_transaction,
};

pub const TASK_ASSESSMENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssessmentUnavailableReason {
    TerminalNotCompleted,
    NoTerminalOutput,
    OutputTooLarge,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "reason",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TaskAssessmentOutcome {
    Pass,
    Fail,
    Unavailable(TaskAssessmentUnavailableReason),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskAssessmentRecord {
    pub schema_version: u32,
    pub assessment_id: String,
    pub assessment_fingerprint: String,
    pub owner_user_id: String,
    pub experiment_id: String,
    pub trial_id: String,
    pub session_id: String,
    pub execution_run_id: String,
    pub admission_run_generation: u64,
    pub execution_run_generation: u64,
    pub spec_fingerprint: String,
    pub observation_id: String,
    pub verifier_fingerprint: String,
    pub terminal: Option<CommittedTerminalProof>,
    pub output: Option<RunOutputProof>,
    pub outcome: TaskAssessmentOutcome,
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    content = "assessment",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum TaskAssessmentResult {
    Pending,
    Recorded(Box<TaskAssessmentRecord>),
}

#[derive(Debug, Error)]
pub enum TaskAssessmentError {
    #[error("invalid task assessment input: {0}")]
    InvalidInput(String),
    #[error("task assessment not found: {0}")]
    NotFound(String),
    #[error("task assessment integrity conflict: {0}")]
    Integrity(String),
    #[error("task assessment database operation failed: {operation}: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error(transparent)]
    Persistence(#[from] EvaluationPersistenceError),
    #[error(transparent)]
    Execution(#[from] EvaluationExecutionError),
}

impl TaskAssessmentError {
    /// Storage failures leave assessment pending; they never become a verdict.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Database { .. }
                | Self::Persistence(EvaluationPersistenceError::Database { .. })
                | Self::Execution(EvaluationExecutionError::Database { .. })
                | Self::Execution(EvaluationExecutionError::Persistence(
                    EvaluationPersistenceError::Database { .. }
                ))
        )
    }
}

impl From<TerminalOutputReadError> for TaskAssessmentError {
    fn from(error: TerminalOutputReadError) -> Self {
        match error {
            TerminalOutputReadError::Pending => {
                Self::Integrity("terminal evidence is pending".into())
            }
            TerminalOutputReadError::Proof(crate::runs::RunProofReadError::Database(source)) => {
                Self::Database {
                    operation: "read_task_output",
                    source,
                }
            }
            TerminalOutputReadError::Proof(crate::runs::RunProofReadError::Integrity(detail)) => {
                Self::Integrity(detail)
            }
        }
    }
}

fn verifier_fingerprint(spec: &TaskVerifierSpec) -> Result<String, TaskAssessmentError> {
    spec.validate().map_err(TaskAssessmentError::Integrity)?;
    let value =
        serde_json::to_value(spec).map_err(|e| TaskAssessmentError::Integrity(e.to_string()))?;
    Ok(content_fingerprint(&format!(
        "task-verifier.v1:{}",
        canonical_json_string(&value)
    )))
}

impl TaskAssessmentRecord {
    fn fingerprint(&self) -> Result<String, TaskAssessmentError> {
        let mut value = serde_json::to_value(self)
            .map_err(|e| TaskAssessmentError::Integrity(e.to_string()))?;
        let object = value
            .as_object_mut()
            .expect("assessment serializes as an object");
        for field in ["assessment_fingerprint", "assessment_id", "created_at"] {
            object.remove(field);
        }
        Ok(content_fingerprint(&format!(
            "task-assessment.v1:{}",
            canonical_json_string(&value)
        )))
    }

    fn validate_hash(&self) -> Result<(), TaskAssessmentError> {
        if self.schema_version != TASK_ASSESSMENT_SCHEMA_VERSION
            || self.assessment_fingerprint != self.fingerprint()?
            || self.assessment_id != self.assessment_fingerprint.replace("sha256:", "eva_")
        {
            return Err(TaskAssessmentError::Integrity(
                "assessment content identity mismatch".into(),
            ));
        }
        Ok(())
    }

    /// Pure historical report validation; no current model or receipt admission.
    pub fn validate_binding(
        &self,
        experiment: &EvaluationExperimentRecord,
        observation: &EvaluationObservationRecord,
    ) -> Result<(), TaskAssessmentError> {
        self.validate_hash()?;
        let spec = &experiment.spec;
        let planned = spec.plan_trials().map_err(TaskAssessmentError::Integrity)?;
        let trial = planned
            .iter()
            .find(|trial| trial.trial_id == self.trial_id)
            .ok_or_else(|| TaskAssessmentError::Integrity("assessment trial is absent".into()))?;
        let case = spec
            .cases
            .iter()
            .find(|case| case.case_id == trial.case_id)
            .ok_or_else(|| TaskAssessmentError::Integrity("assessment case is absent".into()))?;
        if self.owner_user_id != experiment.owner_user_id
            || self.owner_user_id != observation.owner_user_id
            || self.experiment_id != spec.experiment_id
            || self.experiment_id != experiment.experiment_id
            || self.experiment_id != observation.experiment_id
            || self.trial_id != observation.trial_id
            || self.trial_id != observation.observation.trial_id
            || self.spec_fingerprint != experiment.spec_fingerprint
            || self.spec_fingerprint != observation.observation.experiment_fingerprint
            || self.spec_fingerprint
                != spec
                    .spec_fingerprint()
                    .map_err(TaskAssessmentError::Integrity)?
            || self.observation_id != observation.observation_id
            || self.session_id != observation.session_id
            || self.execution_run_id != observation.execution_run_id
            || self.admission_run_generation != observation.admission_run_generation
            || self.execution_run_generation != observation.execution_run_generation
            || trial.case_id != observation.observation.case_id
            || trial.arm != observation.observation.arm
            || trial.repetition != observation.observation.repetition
            || self.verifier_fingerprint != verifier_fingerprint(&case.task_verifier)?
        {
            return Err(TaskAssessmentError::Integrity(
                "assessment is outside its frozen trial/observation binding".into(),
            ));
        }
        match (
            &observation.observation.status,
            &self.outcome,
            &self.terminal,
            &self.output,
        ) {
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Pass | TaskAssessmentOutcome::Fail,
                Some(_),
                Some(_),
            ) => {}
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Unavailable(
                    TaskAssessmentUnavailableReason::NoTerminalOutput,
                ),
                Some(_),
                None,
            ) => {}
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Unavailable(TaskAssessmentUnavailableReason::OutputTooLarge),
                Some(_),
                Some(_),
            ) => {}
            (
                status,
                TaskAssessmentOutcome::Unavailable(
                    TaskAssessmentUnavailableReason::TerminalNotCompleted,
                ),
                None,
                None,
            ) if *status != TrialStatus::Completed && *status != TrialStatus::Unknown => {}
            _ => {
                return Err(TaskAssessmentError::Integrity(
                    "assessment verdict lacks eligible terminal/output evidence".into(),
                ));
            }
        }
        if let (Some(terminal), Some(output)) = (&self.terminal, &self.output)
            && (output.receipt_event_idx < 0
                || output.receipt_event_idx >= terminal.terminal_event_idx
                || output.content_bytes == 0
                || output.source_event_id.is_empty()
                || output.content_hash.is_empty())
        {
            return Err(TaskAssessmentError::Integrity(
                "assessment output is outside terminal cut".into(),
            ));
        }
        Ok(())
    }

    pub fn measurement(&self) -> Measurement {
        let (status, value) = match self.outcome {
            TaskAssessmentOutcome::Pass => (MeasurementStatus::Observed, Some(1.0)),
            TaskAssessmentOutcome::Fail => (MeasurementStatus::Observed, Some(0.0)),
            TaskAssessmentOutcome::Unavailable(_) => (MeasurementStatus::Unavailable, None),
        };
        Measurement {
            name: "task_success".into(),
            unit: "boolean".into(),
            status,
            value,
            basis: Some(self.assessment_id.clone()),
        }
    }

    pub fn evidence(&self) -> EvidenceRef {
        EvidenceRef {
            evidence_id: self.assessment_id.clone(),
            kind: EvidenceKind::Assessment,
            availability: EvidenceAvailability::Available,
            content_hash: Some(self.assessment_fingerprint.clone()),
            locator: Some(format!("evaluation://assessment/{}", self.assessment_id)),
        }
    }
}

impl DatabaseEvaluationObservationStore {
    pub async fn assess_trial(
        &self,
        owner: &str,
        experiment_id: &str,
        trial_id: &str,
    ) -> Result<TaskAssessmentResult, TaskAssessmentError> {
        for (name, value) in [
            ("owner", owner),
            ("experiment", experiment_id),
            ("trial", trial_id),
        ] {
            if value.trim().is_empty() || value.len() > 128 {
                return Err(TaskAssessmentError::InvalidInput(format!(
                    "invalid {name} identity"
                )));
            }
        }
        if let Some(record) = load_assessment(self.pool.get(), owner, trial_id, false).await? {
            if record.experiment_id != experiment_id {
                return Err(TaskAssessmentError::NotFound(
                    "trial is outside experiment".into(),
                ));
            }
            return Ok(TaskAssessmentResult::Recorded(Box::new(record)));
        }
        // Discover only; do not establish the assessment transaction's snapshot
        // or lock Trial before the canonical Session -> Run -> Trial order.
        let target = sqlx::query(
            "SELECT session_id, run_id FROM evaluation_trial_bindings
            WHERE owner_user_id = ? AND experiment_id = ? AND trial_id = ?",
        )
        .bind(owner)
        .bind(experiment_id)
        .bind(trial_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| TaskAssessmentError::Database {
            operation: "discover_task_assessment_target",
            source,
        })?
        .ok_or_else(|| TaskAssessmentError::NotFound("trial".into()))?;
        let session_id: Option<String> = target
            .try_get("session_id")
            .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
        let run_id: Option<String> = target
            .try_get("run_id")
            .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
        let (Some(session_id), Some(run_id)) = (session_id, run_id) else {
            return Ok(TaskAssessmentResult::Pending);
        };
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| TaskAssessmentError::Database {
                    operation: "begin_task_assessment",
                    source,
                })?;
        let (experiment, binding, _) = DatabaseEvaluationPlanStore::lock_historical_trial_run(
            &mut tx,
            owner,
            trial_id,
            &session_id,
            &run_id,
        )
        .await?;
        if let Some(record) = load_assessment(&mut *tx, owner, trial_id, true).await? {
            if record.experiment_id != experiment_id {
                return Err(TaskAssessmentError::NotFound(
                    "trial is outside experiment".into(),
                ));
            }
            return Ok(TaskAssessmentResult::Recorded(Box::new(record)));
        }
        let observation =
            super::execution::load_observation_by_trial_in_transaction(&mut tx, owner, trial_id)
                .await?;
        let Some(observation) = observation.as_ref() else {
            return Ok(TaskAssessmentResult::Pending);
        };
        if experiment.experiment_id != experiment_id
            || binding.binding_status != "bound"
            || binding.session_id.as_deref() != Some(observation.session_id.as_str())
            || binding.run_id.as_deref() != Some(observation.execution_run_id.as_str())
            || binding.run_generation != Some(observation.admission_run_generation)
        {
            return Err(TaskAssessmentError::Integrity(
                "assessment observation does not match locked binding".into(),
            ));
        }
        if observation.observation.status == TrialStatus::Unknown {
            return Ok(TaskAssessmentResult::Pending);
        }
        let case = experiment
            .spec
            .cases
            .iter()
            .find(|case| case.case_id == binding.trial.case_id)
            .ok_or_else(|| TaskAssessmentError::Integrity("bound case is absent".into()))?;
        let mut record = TaskAssessmentRecord {
            schema_version: TASK_ASSESSMENT_SCHEMA_VERSION,
            assessment_id: String::new(),
            assessment_fingerprint: String::new(),
            owner_user_id: owner.into(),
            experiment_id: experiment_id.into(),
            trial_id: trial_id.into(),
            session_id: observation.session_id.clone(),
            execution_run_id: observation.execution_run_id.clone(),
            admission_run_generation: observation.admission_run_generation,
            execution_run_generation: observation.execution_run_generation,
            spec_fingerprint: experiment.spec_fingerprint.clone(),
            observation_id: observation.observation_id.clone(),
            verifier_fingerprint: verifier_fingerprint(&case.task_verifier)?,
            terminal: None,
            output: None,
            outcome: TaskAssessmentOutcome::Unavailable(
                TaskAssessmentUnavailableReason::TerminalNotCompleted,
            ),
            created_at: String::new(),
        };
        if observation.observation.status == TrialStatus::Completed {
            let evidence = load_verified_terminal_output_in_transaction(
                &mut tx,
                &TerminalOutputIdentity {
                    owner_user_id: owner,
                    session_id: &record.session_id,
                    run_id: &record.execution_run_id,
                    generation: record.execution_run_generation,
                },
                MAX_OUTPUT_BYTES,
            )
            .await;
            let evidence = match evidence {
                Ok(evidence) => evidence,
                Err(TerminalOutputReadError::Pending) => return Ok(TaskAssessmentResult::Pending),
                Err(error) => return Err(error.into()),
            };
            record.outcome = match (&evidence.output, evidence.content.as_deref()) {
                (None, _) => TaskAssessmentOutcome::Unavailable(
                    TaskAssessmentUnavailableReason::NoTerminalOutput,
                ),
                (Some(proof), _) if proof.content_bytes > MAX_OUTPUT_BYTES as u64 => {
                    TaskAssessmentOutcome::Unavailable(
                        TaskAssessmentUnavailableReason::OutputTooLarge,
                    )
                }
                (Some(_), None) => return Ok(TaskAssessmentResult::Pending),
                (Some(_), Some(content)) => {
                    match verify_complete_output(&case.task_verifier, Some(content))
                        .map_err(TaskAssessmentError::Integrity)?
                    {
                        TaskVerifierVerdict::Pass => TaskAssessmentOutcome::Pass,
                        TaskVerifierVerdict::Fail => TaskAssessmentOutcome::Fail,
                        TaskVerifierVerdict::Unavailable => {
                            return Err(TaskAssessmentError::Integrity(
                                "bounded verifier unexpectedly unavailable".into(),
                            ));
                        }
                    }
                }
            };
            record.terminal = Some(evidence.terminal);
            record.output = evidence.output;
        }
        record.assessment_fingerprint = record.fingerprint()?;
        record.assessment_id = record.assessment_fingerprint.replace("sha256:", "eva_");
        record.validate_binding(&experiment, observation)?;
        record.created_at = sqlx::query_scalar("SELECT CAST(NOW(6) AS CHAR)")
            .fetch_one(&mut *tx)
            .await
            .map_err(|source| TaskAssessmentError::Database {
                operation: "task_assessment_timestamp",
                source,
            })?;
        let json = serde_json::to_string(&record)
            .map_err(|e| TaskAssessmentError::Integrity(e.to_string()))?;
        sqlx::query("INSERT INTO evaluation_task_assessments (owner_user_id, trial_id, experiment_id, assessment_id, assessment_json) VALUES (?, ?, ?, ?, ?)")
            .bind(owner).bind(trial_id).bind(experiment_id).bind(&record.assessment_id).bind(json).execute(&mut *tx).await
            .map_err(|source| TaskAssessmentError::Database { operation: "insert_task_assessment", source })?;
        tx.commit()
            .await
            .map_err(|source| TaskAssessmentError::Database {
                operation: "commit_task_assessment",
                source,
            })?;
        Ok(TaskAssessmentResult::Recorded(Box::new(record)))
    }
}

fn decode_assessment(
    row: sqlx::mysql::MySqlRow,
) -> Result<TaskAssessmentRecord, TaskAssessmentError> {
    let get = |name| {
        row.try_get::<String, _>(name)
            .map_err(|e| TaskAssessmentError::Integrity(e.to_string()))
    };
    let record: TaskAssessmentRecord = serde_json::from_str(&get("assessment_json")?)
        .map_err(|e| TaskAssessmentError::Integrity(e.to_string()))?;
    record.validate_hash()?;
    if record.owner_user_id != get("owner_user_id")?
        || record.experiment_id != get("experiment_id")?
        || record.trial_id != get("trial_id")?
        || record.assessment_id != get("assessment_id")?
    {
        return Err(TaskAssessmentError::Integrity(
            "assessment storage key mismatch".into(),
        ));
    }
    Ok(record)
}

async fn load_assessment<'e, E: Executor<'e, Database = MySql>>(
    executor: E,
    owner: &str,
    trial: &str,
    lock: bool,
) -> Result<Option<TaskAssessmentRecord>, TaskAssessmentError> {
    let lock = if lock { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT owner_user_id, experiment_id, trial_id, assessment_id, assessment_json FROM evaluation_task_assessments WHERE owner_user_id = ? AND trial_id = ?{lock}"
    );
    sqlx::query(&sql)
        .bind(owner)
        .bind(trial)
        .fetch_optional(executor)
        .await
        .map_err(|source| TaskAssessmentError::Database {
            operation: "load_task_assessment",
            source,
        })?
        .map(decode_assessment)
        .transpose()
}

pub(crate) async fn list_assessments_in_transaction(
    tx: &mut Transaction<'_, MySql>,
    owner: &str,
    experiment: &str,
) -> Result<Vec<TaskAssessmentRecord>, TaskAssessmentError> {
    let rows = sqlx::query("SELECT owner_user_id, experiment_id, trial_id, assessment_id, assessment_json FROM evaluation_task_assessments WHERE owner_user_id = ? AND experiment_id = ? ORDER BY trial_id")
        .bind(owner).bind(experiment).fetch_all(&mut **tx).await
        .map_err(|source| TaskAssessmentError::Database { operation: "list_task_assessments", source })?;
    rows.into_iter().map(decode_assessment).collect()
}

/// Synthetic projection fixture only. Production verdicts come exclusively from assess_trial.
#[cfg(test)]
pub(crate) fn test_assessment_record(
    experiment: &EvaluationExperimentRecord,
    observation: &EvaluationObservationRecord,
    outcome: TaskAssessmentOutcome,
) -> TaskAssessmentRecord {
    let case = experiment
        .spec
        .cases
        .iter()
        .find(|case| case.case_id == observation.observation.case_id)
        .unwrap();
    let completed = observation.observation.status == TrialStatus::Completed;
    let output = matches!(
        outcome,
        TaskAssessmentOutcome::Pass
            | TaskAssessmentOutcome::Fail
            | TaskAssessmentOutcome::Unavailable(TaskAssessmentUnavailableReason::OutputTooLarge)
    )
    .then(|| RunOutputProof {
        receipt_event_idx: 10,
        source_event_id: "test-output".into(),
        content_hash: content_fingerprint("{}"),
        content_bytes: if matches!(
            outcome,
            TaskAssessmentOutcome::Unavailable(TaskAssessmentUnavailableReason::OutputTooLarge)
        ) {
            MAX_OUTPUT_BYTES as u64 + 1
        } else {
            2
        },
    });
    let mut record = TaskAssessmentRecord {
        schema_version: TASK_ASSESSMENT_SCHEMA_VERSION,
        assessment_id: String::new(),
        assessment_fingerprint: String::new(),
        owner_user_id: observation.owner_user_id.clone(),
        experiment_id: experiment.experiment_id.clone(),
        trial_id: observation.trial_id.clone(),
        session_id: observation.session_id.clone(),
        execution_run_id: observation.execution_run_id.clone(),
        admission_run_generation: observation.admission_run_generation,
        execution_run_generation: observation.execution_run_generation,
        spec_fingerprint: experiment.spec_fingerprint.clone(),
        observation_id: observation.observation_id.clone(),
        verifier_fingerprint: verifier_fingerprint(&case.task_verifier).unwrap(),
        terminal: completed.then(|| CommittedTerminalProof {
            settlement_batch_id: "test-atomic-batch".into(),
            terminal_event_idx: 12,
        }),
        output,
        outcome,
        created_at: "2026-09-19 00:00:00".into(),
    };
    record.assessment_fingerprint = record.fingerprint().unwrap();
    record.assessment_id = record.assessment_fingerprint.replace("sha256:", "eva_");
    record.validate_binding(experiment, observation).unwrap();
    record
}

#[cfg(test)]
mod tests {
    use super::super::{
        bootstrap::build_prepared_experiment_spec, execution::terminal_run_observation,
    };
    use super::*;

    fn fixture(status: TrialStatus) -> (EvaluationExperimentRecord, EvaluationObservationRecord) {
        let request = serde_json::from_value(serde_json::json!({
            "submission_idempotency_key": "assessment-test",
            "target": {"kind":"prompt", "baseline":{"revision_id":"base","content":"base"}, "candidate":{"revision_id":"candidate","content":"candidate"}},
            "case":{"case_id":"case", "message":"return JSON", "holdout":false, "verifier_config":{"expected":{"ok":true}}},
            "model_offering_id":"model", "max_concurrency":1,"max_wall_time_secs":30
        })).unwrap();
        let spec = build_prepared_experiment_spec(
            "owner",
            "experiment",
            &request,
            &crate::evaluation::test_support::execution_config("model", "openai", "case"),
            None,
        )
        .unwrap();
        let trial = spec.plan_trials().unwrap().remove(0);
        let fingerprint = spec.spec_fingerprint().unwrap();
        let experiment = EvaluationExperimentRecord {
            owner_user_id: "owner".into(),
            experiment_id: spec.experiment_id.clone(),
            spec_fingerprint: fingerprint.clone(),
            spec,
            submission_idempotency_key: "assessment-test".into(),
            planned_trial_count: 2,
            created_at: String::new(),
            updated_at: String::new(),
        };
        let observation = EvaluationObservationRecord {
            schema_version: super::super::execution::EVALUATION_EXECUTION_SCHEMA_VERSION,
            owner_user_id: "owner".into(),
            observation_id: "observation".into(),
            experiment_id: "experiment".into(),
            trial_id: trial.trial_id.clone(),
            session_id: "session".into(),
            execution_run_id: "run".into(),
            admission_run_generation: 0,
            execution_run_generation: 0,
            observation: terminal_run_observation(
                fingerprint,
                trial.trial_id,
                trial.case_id,
                trial.arm,
                trial.repetition,
                status,
                None,
                None,
                None,
                vec![],
            ),
            materialization_receipt_ids: vec![],
            request_fingerprint: "observation-request".into(),
            idempotency_key: "observation".into(),
            created_at: String::new(),
            updated_at: String::new(),
        };
        (experiment, observation)
    }

    #[test]
    fn task_assessment_binds_verdict_and_historical_identity() {
        let (experiment, observation) = fixture(TrialStatus::Completed);
        let pass = test_assessment_record(&experiment, &observation, TaskAssessmentOutcome::Pass);
        assert_eq!(pass.measurement().value, Some(1.0));
        assert_eq!(
            pass.evidence().content_hash.as_deref(),
            Some(pass.assessment_fingerprint.as_str())
        );
        let mut changed = pass.clone();
        changed.outcome = TaskAssessmentOutcome::Fail;
        assert!(changed.validate_binding(&experiment, &observation).is_err());
        let mut changed_observation = observation.clone();
        changed_observation.execution_run_generation += 1;
        assert!(
            pass.validate_binding(&experiment, &changed_observation)
                .is_err()
        );
        changed_observation = observation.clone();
        changed_observation.owner_user_id = "other".into();
        assert!(
            pass.validate_binding(&experiment, &changed_observation)
                .is_err()
        );
        let mut timestamp_only = pass.clone();
        timestamp_only.created_at = "later".into();
        assert_eq!(
            timestamp_only.fingerprint().unwrap(),
            pass.assessment_fingerprint
        );
        let fail = test_assessment_record(&experiment, &observation, TaskAssessmentOutcome::Fail);
        assert_eq!(fail.measurement().value, Some(0.0));
        assert_ne!(fail.assessment_fingerprint, pass.assessment_fingerprint);
    }

    #[test]
    fn task_assessment_noncompleted_cannot_claim_task_success_even_with_rehashed_output() {
        let (experiment, completed) = fixture(TrialStatus::Completed);
        let mut forged =
            test_assessment_record(&experiment, &completed, TaskAssessmentOutcome::Pass);
        let mut failed = completed;
        failed.observation.status = TrialStatus::Failed;
        forged.assessment_fingerprint = forged.fingerprint().unwrap();
        assert!(forged.validate_binding(&experiment, &failed).is_err());
        let unavailable = test_assessment_record(
            &experiment,
            &failed,
            TaskAssessmentOutcome::Unavailable(
                TaskAssessmentUnavailableReason::TerminalNotCompleted,
            ),
        );
        assert_eq!(unavailable.measurement().value, None);
        assert_eq!(
            unavailable.measurement().status,
            MeasurementStatus::Unavailable
        );
        assert!(unavailable.terminal.is_none());
        assert!(unavailable.output.is_none());
        assert!(
            TaskAssessmentError::Database {
                operation: "read",
                source: sqlx::Error::PoolTimedOut
            }
            .is_retryable()
        );
        assert!(!TaskAssessmentError::Integrity("hash".into()).is_retryable());
    }
}
