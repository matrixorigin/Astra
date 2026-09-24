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
use crate::tool_invocation_ledger::{
    DatabaseToolInvocationLedger, ToolInvocationLedgerStoreError, ToolInvocationRunEvidence,
};
use astra_turn_types::ToolInvocationCompletionRef;
use std::collections::BTreeSet;

pub const TASK_ASSESSMENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAssessmentUnavailableReason {
    TerminalNotCompleted,
    NoTerminalOutput,
    OutputTooLarge,
    CodingEvidenceUnavailable,
}

pub const TOOL_INVOCATION_COVERAGE_SCHEMA_VERSION: u32 = 1;

/// Immutable proof that the Run journal and retained invocation ledger agreed
/// on every invocation that crossed (or was completed by) an action boundary
/// when this assessment was created.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolInvocationCoverageProof {
    pub schema_version: u32,
    pub run_id: String,
    pub run_generation: u64,
    pub last_event_index: i64,
    pub admitted_action_ids: Vec<String>,
    pub completion_refs: Vec<ToolInvocationCompletionRef>,
    pub proof_fingerprint: String,
}

impl ToolInvocationCoverageProof {
    fn from_evidence(
        evidence: ToolInvocationRunEvidence,
        run_id: &str,
        run_generation: u64,
    ) -> Result<Self, TaskAssessmentError> {
        if evidence.history.run_generation != run_generation {
            return Err(TaskAssessmentError::Integrity(
                "tool invocation coverage run generation differs from the assessment run".into(),
            ));
        }
        let mut admitted_action_ids = evidence
            .history
            .grants
            .iter()
            .filter_map(|grant| grant.action_id.strip_prefix("tool_invocation:"))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        admitted_action_ids.sort();
        admitted_action_ids.dedup();
        let mut completion_refs = evidence.completions;
        completion_refs.sort_by(|left, right| left.identity.cmp(&right.identity));
        let completion_ids = completion_refs
            .iter()
            .map(|reference| reference.identity.storage_key())
            .collect::<BTreeSet<_>>();
        if completion_ids != admitted_action_ids.iter().cloned().collect() {
            return Err(TaskAssessmentError::Integrity(
                "tool invocation coverage set differs between grants and completions".into(),
            ));
        }
        let mut proof = Self {
            schema_version: TOOL_INVOCATION_COVERAGE_SCHEMA_VERSION,
            run_id: run_id.to_string(),
            run_generation,
            last_event_index: evidence.history.last_event_index,
            admitted_action_ids,
            completion_refs,
            proof_fingerprint: String::new(),
        };
        proof.proof_fingerprint = proof.fingerprint()?;
        Ok(proof)
    }

    fn fingerprint(&self) -> Result<String, TaskAssessmentError> {
        let mut value = serde_json::to_value(self)
            .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
        value
            .as_object_mut()
            .expect("coverage proof serializes as an object")
            .remove("proof_fingerprint");
        Ok(content_fingerprint(&format!(
            "tool-invocation-coverage.v1:{}",
            canonical_json_string(&value)
        )))
    }

    fn validate(&self, run_id: &str, run_generation: u64) -> Result<(), TaskAssessmentError> {
        if self.schema_version != TOOL_INVOCATION_COVERAGE_SCHEMA_VERSION
            || self.run_id != run_id
            || self.run_generation != run_generation
            || self.last_event_index < -1
            || self.proof_fingerprint != self.fingerprint()?
            || self
                .admitted_action_ids
                .windows(2)
                .any(|pair| pair[0] >= pair[1])
            || self
                .completion_refs
                .windows(2)
                .any(|pair| pair[0].identity >= pair[1].identity)
            || self
                .completion_refs
                .iter()
                .any(|reference| reference.identity.run_id != run_id)
            || self
                .completion_refs
                .iter()
                .map(|reference| reference.identity.storage_key())
                .collect::<BTreeSet<_>>()
                != self.admitted_action_ids.iter().cloned().collect()
        {
            return Err(TaskAssessmentError::Integrity(
                "tool invocation coverage proof identity is invalid".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodingAssessmentProof {
    pub artifact_id: String,
    pub content_hash: String,
    pub source_commit: String,
    pub source_tree: String,
    pub base_revision: String,
    pub result_revision: String,
    pub patch_hash: String,
    pub verifier_output_hash: String,
    pub verifier_exit_code: i32,
    pub namespace_active: bool,
    pub network_namespace: bool,
    pub scope_settled: bool,
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
    pub tool_invocation_coverage: ToolInvocationCoverageProof,
    pub terminal: Option<CommittedTerminalProof>,
    pub output: Option<RunOutputProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coding: Option<CodingAssessmentProof>,
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
    #[error(transparent)]
    Invocation(#[from] Box<ToolInvocationLedgerStoreError>),
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
        ) || matches!(self, Self::Invocation(error) if error.is_retryable())
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingArtifactWorkspace {
    connection_generation: u64,
    workspace_dir: String,
    source_commit: Option<String>,
    source_tree: Option<String>,
    base_revision: Option<String>,
    result_revision: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingArtifactIsolation {
    namespace_active: bool,
    network_namespace: bool,
    scope_settled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodingArtifactV1 {
    schema_version: u32,
    experiment_id: String,
    trial_id: String,
    session_id: String,
    run_id: String,
    run_generation: u64,
    spec_fingerprint: String,
    verifier: TaskVerifierSpec,
    workspace: CodingArtifactWorkspace,
    patch: String,
    verifier_exit_code: Option<i32>,
    verifier_output: String,
    isolation: CodingArtifactIsolation,
    verifier_passed: bool,
}

async fn load_coding_proof(
    tx: &mut Transaction<'_, MySql>,
    owner: &str,
    experiment: &EvaluationExperimentRecord,
    observation: &EvaluationObservationRecord,
    trial_id: &str,
    verifier: &TaskVerifierSpec,
) -> Result<Option<(CodingAssessmentProof, bool)>, TaskAssessmentError> {
    let artifact_id = format!("evaluation-coding-{trial_id}");
    let row = sqlx::query(
        "SELECT artifact_kind, source, content_json FROM session_artifacts
         WHERE user_id = ? AND session_id = ? AND artifact_id = ?",
    )
    .bind(owner)
    .bind(&observation.session_id)
    .bind(&artifact_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| TaskAssessmentError::Database {
        operation: "load_coding_evidence_artifact",
        source,
    })?;
    let Some(row) = row else { return Ok(None) };
    let artifact_kind: String = row
        .try_get("artifact_kind")
        .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
    let source: Option<String> = row
        .try_get("source")
        .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
    if artifact_kind != "evaluation_coding_evidence"
        || source.as_deref() != Some("evaluation_edge_finalize")
    {
        return Err(TaskAssessmentError::Integrity(
            "coding evidence artifact type mismatch".into(),
        ));
    }
    let content_json: String = row
        .try_get("content_json")
        .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
    let value: serde_json::Value = serde_json::from_str(&content_json)
        .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
    let content_hash = content_fingerprint(&canonical_json_string(&value));
    let artifact: CodingArtifactV1 = serde_json::from_value(value)
        .map_err(|error| TaskAssessmentError::Integrity(error.to_string()))?;
    if artifact.schema_version != 1
        || artifact.experiment_id != experiment.experiment_id
        || artifact.trial_id != trial_id
        || artifact.session_id != observation.session_id
        || artifact.run_id != observation.execution_run_id
        || artifact.run_generation != observation.execution_run_generation
        || artifact.spec_fingerprint != experiment.spec_fingerprint
        || artifact.verifier != *verifier
        || artifact.workspace.connection_generation == 0
        || artifact.workspace.workspace_dir.trim().is_empty()
        || !artifact.isolation.namespace_active
        || !artifact.isolation.network_namespace
        || !artifact.isolation.scope_settled
    {
        return Err(TaskAssessmentError::Integrity(
            "coding evidence artifact binding mismatch".into(),
        ));
    }
    let (
        Some(source_commit),
        Some(source_tree),
        Some(base_revision),
        Some(result_revision),
        Some(exit_code),
    ) = (
        artifact.workspace.source_commit,
        artifact.workspace.source_tree,
        artifact.workspace.base_revision,
        artifact.workspace.result_revision,
        artifact.verifier_exit_code,
    )
    else {
        return Err(TaskAssessmentError::Integrity(
            "coding evidence artifact is incomplete".into(),
        ));
    };
    Ok(Some((
        CodingAssessmentProof {
            artifact_id,
            content_hash,
            source_commit,
            source_tree,
            base_revision,
            result_revision,
            patch_hash: content_fingerprint(&artifact.patch),
            verifier_output_hash: content_fingerprint(&artifact.verifier_output),
            verifier_exit_code: exit_code,
            namespace_active: artifact.isolation.namespace_active,
            network_namespace: artifact.isolation.network_namespace,
            scope_settled: artifact.isolation.scope_settled,
        },
        artifact.verifier_passed,
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
        self.tool_invocation_coverage
            .validate(&self.execution_run_id, self.execution_run_generation)?;
        if self.terminal.as_ref().is_some_and(|terminal| {
            self.tool_invocation_coverage.last_event_index < terminal.terminal_event_idx
        }) {
            return Err(TaskAssessmentError::Integrity(
                "tool invocation coverage ends before the terminal Run event".into(),
            ));
        }
        match (
            &observation.observation.status,
            &self.outcome,
            &self.terminal,
            &self.output,
            &self.coding,
        ) {
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Pass | TaskAssessmentOutcome::Fail,
                Some(_),
                Some(_),
                None,
            ) => {}
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Pass | TaskAssessmentOutcome::Fail,
                Some(_),
                None,
                Some(proof),
            ) if proof.namespace_active && proof.network_namespace && proof.scope_settled => {}
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Unavailable(
                    TaskAssessmentUnavailableReason::NoTerminalOutput,
                ),
                Some(_),
                None,
                None,
            ) => {}
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Unavailable(TaskAssessmentUnavailableReason::OutputTooLarge),
                Some(_),
                Some(_),
                None,
            ) => {}
            (
                TrialStatus::Completed,
                TaskAssessmentOutcome::Unavailable(
                    TaskAssessmentUnavailableReason::CodingEvidenceUnavailable,
                ),
                Some(_),
                None,
                None,
            ) => {}
            (
                status,
                TaskAssessmentOutcome::Unavailable(
                    TaskAssessmentUnavailableReason::TerminalNotCompleted,
                ),
                None,
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
        if let Some(proof) = &self.coding
            && ([
                &proof.content_hash,
                &proof.patch_hash,
                &proof.verifier_output_hash,
            ]
            .iter()
            .any(|value| {
                value.len() != 71
                    || !value.starts_with("sha256:")
                    || !value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
            }) || proof.artifact_id != format!("evaluation-coding-{}", self.trial_id)
                || proof.source_commit.is_empty()
                || proof.source_tree.is_empty()
                || proof.base_revision.is_empty()
                || proof.result_revision.is_empty())
        {
            return Err(TaskAssessmentError::Integrity(
                "coding assessment proof is invalid".into(),
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

    pub fn coding_evidence(&self) -> Option<EvidenceRef> {
        self.coding.as_ref().map(|proof| EvidenceRef {
            evidence_id: proof.artifact_id.clone(),
            kind: EvidenceKind::Artifact,
            availability: EvidenceAvailability::Available,
            content_hash: Some(proof.content_hash.clone()),
            locator: Some(format!(
                "session-artifact://{}/{}",
                self.session_id, proof.artifact_id
            )),
        })
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
        let tool_invocation_coverage =
            match DatabaseToolInvocationLedger::inspect_run_evidence_in_transaction(
                &mut tx,
                owner,
                &observation.session_id,
                &observation.execution_run_id,
            )
            .await
            {
                Ok(evidence) => ToolInvocationCoverageProof::from_evidence(
                    evidence,
                    &observation.execution_run_id,
                    observation.execution_run_generation,
                )?,
                Err(ToolInvocationLedgerStoreError::EvidenceUnresolved(_)) => {
                    return Ok(TaskAssessmentResult::Pending);
                }
                Err(error) => {
                    return Err(TaskAssessmentError::Invocation(Box::new(error)));
                }
            };
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
            tool_invocation_coverage,
            terminal: None,
            output: None,
            coding: None,
            outcome: TaskAssessmentOutcome::Unavailable(
                TaskAssessmentUnavailableReason::TerminalNotCompleted,
            ),
            created_at: String::new(),
        };
        if observation.observation.status == TrialStatus::Completed {
            if matches!(
                case.task_verifier.config,
                super::task_verifier::TaskVerifierConfig::WorkspaceCommand { .. }
            ) {
                let terminal_evidence = load_verified_terminal_output_in_transaction(
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
                let terminal_evidence = match terminal_evidence {
                    Ok(evidence) => evidence,
                    Err(TerminalOutputReadError::Pending) => {
                        return Ok(TaskAssessmentResult::Pending);
                    }
                    Err(error) => return Err(error.into()),
                };
                match load_coding_proof(
                    &mut tx,
                    owner,
                    &experiment,
                    observation,
                    trial_id,
                    &case.task_verifier,
                )
                .await?
                {
                    Some((proof, passed)) => {
                        record.outcome = if passed {
                            TaskAssessmentOutcome::Pass
                        } else {
                            TaskAssessmentOutcome::Fail
                        };
                        record.coding = Some(proof);
                    }
                    None => {
                        record.outcome = TaskAssessmentOutcome::Unavailable(
                            TaskAssessmentUnavailableReason::CodingEvidenceUnavailable,
                        );
                    }
                }
                record.terminal = Some(terminal_evidence.terminal);
            } else {
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
                    Err(TerminalOutputReadError::Pending) => {
                        return Ok(TaskAssessmentResult::Pending);
                    }
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
    record
        .tool_invocation_coverage
        .validate(&record.execution_run_id, record.execution_run_generation)?;
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
    let mut tool_invocation_coverage = ToolInvocationCoverageProof {
        schema_version: TOOL_INVOCATION_COVERAGE_SCHEMA_VERSION,
        run_id: observation.execution_run_id.clone(),
        run_generation: observation.execution_run_generation,
        last_event_index: 12,
        admitted_action_ids: Vec::new(),
        completion_refs: Vec::new(),
        proof_fingerprint: String::new(),
    };
    tool_invocation_coverage.proof_fingerprint = tool_invocation_coverage.fingerprint().unwrap();
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
        tool_invocation_coverage,
        terminal: completed.then(|| CommittedTerminalProof {
            settlement_batch_id: "test-atomic-batch".into(),
            terminal_event_idx: 12,
        }),
        output,
        coding: None,
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
            "case":{"case_id":"case", "message":"return JSON", "holdout":false, "verifier_config":{"kind":"json_value_equals","expected":{"ok":true}}},
            "model_offering_id":"model", "max_concurrency":1,"max_wall_time_secs":30
        })).unwrap();
        let spec = build_prepared_experiment_spec(
            None,
            "experiment",
            &request,
            &crate::evaluation::test_support::execution_config("model", "openai", "case"),
            None,
            super::super::experiment::EvaluationJudgmentPolicy::Disabled,
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
        let mut stale_coverage = pass.clone();
        stale_coverage.tool_invocation_coverage.last_event_index = 11;
        stale_coverage.tool_invocation_coverage.proof_fingerprint = stale_coverage
            .tool_invocation_coverage
            .fingerprint()
            .unwrap();
        stale_coverage.assessment_fingerprint = stale_coverage.fingerprint().unwrap();
        stale_coverage.assessment_id = stale_coverage
            .assessment_fingerprint
            .replace("sha256:", "eva_");
        assert!(
            stale_coverage
                .validate_binding(&experiment, &observation)
                .is_err()
        );
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
