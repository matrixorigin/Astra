//! Owner-scoped persistence for generic evaluation registrations and trial
//! bindings. This module records execution identity; it does not run a model
//! or materialize a branch.

use super::experiment::{ExperimentSpec, TrialUnit};
use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Row, Transaction, query_scalar};
use std::collections::{BTreeMap, HashMap, HashSet};
use thiserror::Error;

const MAX_SUBMISSION_KEY_BYTES: usize = 128;
const MAX_OWNER_ID_BYTES: usize = 128;
const MAX_TRIAL_ID_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 64;
const MAX_RUN_ID_BYTES: usize = 64;
// This first persistence path intentionally keeps each transaction and read
// bounded. A later paged scheduler may raise these limits with an explicit
// batch protocol; accepting 100k rows in one transaction would make a retry
// or a concurrent tenant wait behind an unbounded JSON write.
const MAX_PERSISTED_TRIALS: usize = 4_096;
const MAX_PERSISTED_SPEC_BYTES: usize = 1_024 * 1_024;
const MAX_PERSISTED_TRIAL_BYTES: usize = 64 * 1_024;
const MAX_PERSISTED_PLAN_BYTES: usize = 16 * 1_024 * 1_024;

/// Retryable protocol dependencies, distinct from invalid frozen identities.
/// The stable prefix survives the canonical RunStore's String error boundary.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum EvaluationProtocolBlock {
    #[error("evaluation_protocol_blocked:preceding_trials_unbound")]
    PrecedingTrialsUnbound,
    #[error("evaluation_protocol_blocked:paired_predecessor_unobserved:{trial_id}")]
    PairedPredecessorUnobserved { trial_id: String },
    #[error("evaluation_protocol_blocked:concurrency_limit:{max_concurrency}")]
    ConcurrencyLimit { max_concurrency: u16 },
}

#[derive(Debug, Error)]
pub enum EvaluationPersistenceError {
    #[error(transparent)]
    ProtocolBlocked(#[from] EvaluationProtocolBlock),
    #[error("invalid evaluation input: {0}")]
    InvalidInput(String),
    #[error("evaluation database operation failed: {operation}: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("evaluation JSON operation failed: {operation}: {source}")]
    Json {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("evaluation identity conflict: {0}")]
    Conflict(String),
    #[error("evaluation record not found: {0}")]
    NotFound(String),
}

fn execution_persistence_error(
    error: super::execution::EvaluationExecutionError,
) -> EvaluationPersistenceError {
    use super::execution::EvaluationExecutionError;
    match error {
        EvaluationExecutionError::InvalidInput(detail) => {
            EvaluationPersistenceError::InvalidInput(detail)
        }
        EvaluationExecutionError::Conflict(detail) => EvaluationPersistenceError::Conflict(detail),
        EvaluationExecutionError::NotFound(detail) => EvaluationPersistenceError::NotFound(detail),
        EvaluationExecutionError::Database { operation, source } => {
            EvaluationPersistenceError::Database { operation, source }
        }
        EvaluationExecutionError::Json { operation, source } => {
            EvaluationPersistenceError::Json { operation, source }
        }
        EvaluationExecutionError::Persistence(error) => error,
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvaluationExperimentRecord {
    pub owner_user_id: String,
    pub experiment_id: String,
    pub spec_fingerprint: String,
    pub spec: ExperimentSpec,
    pub submission_idempotency_key: String,
    pub planned_trial_count: usize,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvaluationTrialBindingRecord {
    pub owner_user_id: String,
    pub trial_id: String,
    pub experiment_id: String,
    pub spec_fingerprint: String,
    pub trial: TrialUnit,
    /// `planned` means no canonical Run is bound yet; `bound` means the
    /// session/run fields are durable references to the existing backbone.
    pub binding_status: String,
    pub session_id: Option<String>,
    pub run_id: Option<String>,
    /// The canonical Run generation captured while the binding was created.
    /// Receipts must carry this same generation so a resumed/replaced runner
    /// cannot reuse an older materialization identity.
    pub run_generation: Option<u64>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Clone)]
pub struct DatabaseEvaluationPlanStore {
    pool: SharedPool,
}

impl DatabaseEvaluationPlanStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Release Evaluation retention without bypassing the Session deletion lifecycle.
    pub async fn delete_experiment(
        &self,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<(), EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        let db_error = |source| EvaluationPersistenceError::Database {
            operation: "delete_evaluation_experiment",
            source,
        };
        let mut tx = self.pool.get().begin().await.map_err(db_error)?;
        // Start takes this same mutex before any Session locks. New bindings
        // cannot appear while we discover and lock the existing evidence.
        load_experiment_tx(&mut tx, owner_user_id, experiment_id)
            .await?
            .ok_or_else(|| EvaluationPersistenceError::NotFound(experiment_id.to_string()))?;
        let runs: Vec<(String, String)> = sqlx::query_as(
            "SELECT session_id, run_id FROM evaluation_trial_bindings
             WHERE owner_user_id = ? AND experiment_id = ? AND binding_status = 'bound'
             ORDER BY session_id, run_id",
        )
        .bind(owner_user_id)
        .bind(experiment_id)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_error)?;
        let sessions: std::collections::BTreeSet<_> =
            runs.iter().map(|(session, _)| session).collect();
        // Keep the canonical fence -> Session -> Run -> Trial lock order used
        // by deletion and evidence writers. Hold every lock through the purge.
        for session in &sessions {
            sqlx::query("SELECT session_id FROM agent_session_lifecycle_fences WHERE user_id = ? AND session_id = ? FOR UPDATE")
                .bind(owner_user_id).bind(session)
                .fetch_optional(&mut *tx).await.map_err(db_error)?;
        }
        for session in &sessions {
            sqlx::query("SELECT session_id FROM agent_sessions WHERE user_id = ? AND session_id = ? FOR UPDATE")
                .bind(owner_user_id).bind(session)
                .fetch_optional(&mut *tx).await.map_err(db_error)?;
        }
        for (_, run) in &runs {
            let status: Option<String> = sqlx::query_scalar(
                "SELECT status FROM agent_runs WHERE user_id = ? AND run_id = ? FOR UPDATE",
            )
            .bind(owner_user_id)
            .bind(run)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_error)?;
            if status
                .as_deref()
                .is_some_and(|status| !crate::runs::durable_run_status_is_terminal(status))
            {
                return Err(EvaluationPersistenceError::Conflict(format!(
                    "cancel active trial run {run} before deleting the experiment"
                )));
            }
        }
        sqlx::query("SELECT trial_id FROM evaluation_trial_bindings WHERE owner_user_id = ? AND experiment_id = ? ORDER BY trial_id FOR UPDATE")
            .bind(owner_user_id).bind(experiment_id)
            .fetch_all(&mut *tx).await.map_err(db_error)?;
        for table in [
            "evaluation_task_assessments",
            "evaluation_trial_observations",
            "evaluation_materialization_receipts",
            "evaluation_trial_bindings",
            "evaluation_experiments",
        ] {
            sqlx::query(&format!(
                "DELETE FROM {table} WHERE owner_user_id = ? AND experiment_id = ?"
            ))
            .bind(owner_user_id)
            .bind(experiment_id)
            .execute(&mut *tx)
            .await
            .map_err(db_error)?;
        }
        tx.commit().await.map_err(db_error)
    }

    pub(crate) fn shared_pool(&self) -> SharedPool {
        self.pool.clone()
    }

    /// Evaluation sessions retain their Run and inference evidence while a
    /// trial binding remains active. Session lifecycle code uses this single
    /// owner-scoped query to suppress destructive close governance until the owner deletes the experiment.
    pub async fn session_has_bound_trial(
        &self,
        owner_user_id: &str,
        session_id: &str,
    ) -> Result<bool, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_bounded("session_id", session_id, MAX_SESSION_ID_BYTES)?;
        let count: i64 = query_scalar(
            "SELECT COUNT(*) FROM evaluation_trial_bindings
             WHERE owner_user_id = ? AND session_id = ? AND binding_status = 'bound'",
        )
        .bind(owner_user_id)
        .bind(session_id)
        .fetch_one(self.pool.get())
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "check evaluation session binding",
            source,
        })?;
        Ok(count > 0)
    }

    /// The Run creation transaction acquires this mutex BEFORE Session locks.
    /// Keep it until both the Run and its trial binding are committed.
    pub(crate) async fn lock_experiment_for_run_start(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        admission: &super::execution::EvaluationRunAdmission,
    ) -> Result<EvaluationExperimentRecord, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        admission
            .validate_shape()
            .map_err(EvaluationPersistenceError::InvalidInput)?;
        load_experiment_tx(tx, owner_user_id, &admission.experiment_id)
            .await?
            .ok_or_else(|| EvaluationPersistenceError::NotFound(admission.experiment_id.clone()))
    }

    /// Called only while holding the experiment mutex, then Session/Run locks.
    /// Current reads here must not use a snapshot established before waiting
    /// for the mutex. Exact bound identity retries skip new-admission gates.
    pub(crate) async fn admit_trial_run_start(
        tx: &mut Transaction<'_, MySql>,
        experiment: &EvaluationExperimentRecord,
        admission: &super::execution::EvaluationRunAdmission,
        session_id: &str,
        run_id: &str,
    ) -> Result<EvaluationTrialBindingRecord, EvaluationPersistenceError> {
        validate_bounded("session_id", session_id, MAX_SESSION_ID_BYTES)?;
        validate_bounded("run_id", run_id, MAX_RUN_ID_BYTES)?;
        if admission.experiment_id != experiment.experiment_id {
            return Err(EvaluationPersistenceError::Conflict(
                "admission experiment differs from locked experiment".into(),
            ));
        }
        let bindings = load_trial_bindings_with_lock_tx(
            tx,
            &experiment.owner_user_id,
            &experiment.experiment_id,
            true,
        )
        .await?;
        let canonical = canonical_trial_index(experiment)?;
        validate_trial_set(&bindings, experiment, &canonical)?;
        let target = bindings
            .iter()
            .find(|binding| binding.trial_id == admission.trial_id)
            .ok_or_else(|| EvaluationPersistenceError::NotFound(admission.trial_id.clone()))?;
        super::execution::validate_admission_for_trial(
            &experiment.spec,
            target,
            &experiment.owner_user_id,
            session_id,
            admission,
        )
        .map_err(execution_persistence_error)?;
        if target.binding_status == "bound" {
            if target.session_id.as_deref() != Some(session_id)
                || target.run_id.as_deref() != Some(run_id)
            {
                return Err(EvaluationPersistenceError::Conflict(
                    "trial is already bound to another canonical Run".into(),
                ));
            }
            return Ok(target.clone());
        }
        if bindings.iter().any(|binding| {
            binding.trial.sequence < target.trial.sequence && binding.binding_status != "bound"
        }) {
            return Err(EvaluationProtocolBlock::PrecedingTrialsUnbound.into());
        }
        let observations = super::execution::list_observations_with_lock_tx(
            tx,
            &experiment.owner_user_id,
            &experiment.experiment_id,
            true,
        )
        .await
        .map_err(execution_persistence_error)?;
        // Reuse the read model's exact owner/spec/binding checks rather than
        // introducing a second interpretation of persisted observation identity.
        let projection = super::projection::EvaluationExperimentProjection::from_records(
            experiment.clone(),
            bindings.clone(),
            BTreeMap::new(),
            observations,
            Vec::new(),
        )
        .map_err(|error| match error {
            super::projection::EvaluationProjectionError::Persistence(error) => error,
            super::projection::EvaluationProjectionError::Execution(error) => {
                execution_persistence_error(error)
            }
            super::projection::EvaluationProjectionError::Assessment(error) => {
                EvaluationPersistenceError::Conflict(error.to_string())
            }
            super::projection::EvaluationProjectionError::Conflict(detail) => {
                EvaluationPersistenceError::Conflict(detail)
            }
        })?;
        let observed = projection
            .trials
            .iter()
            .filter(|trial| trial.observation.is_some())
            .map(|trial| trial.binding.trial_id.as_str())
            .collect::<HashSet<_>>();
        if let Some(predecessor) = experiment
            .spec
            .paired_predecessor(&target.trial)
            .map_err(EvaluationPersistenceError::Conflict)?
            && !observed.contains(predecessor.trial_id.as_str())
        {
            return Err(EvaluationProtocolBlock::PairedPredecessorUnobserved {
                trial_id: predecessor.trial_id,
            }
            .into());
        }
        let outstanding = bindings
            .iter()
            .filter(|binding| {
                binding.binding_status == "bound" && !observed.contains(binding.trial_id.as_str())
            })
            .count();
        if outstanding >= usize::from(experiment.spec.budget.max_concurrency) {
            return Err(EvaluationProtocolBlock::ConcurrencyLimit {
                max_concurrency: experiment.spec.budget.max_concurrency,
            }
            .into());
        }
        Ok(target.clone())
    }

    /// Register an immutable experiment and all planned trial identities in
    /// one transaction. Repeating the same owner/idempotency/spec tuple
    /// returns the original registration without duplicating trials.
    pub async fn register_experiment(
        &self,
        owner_user_id: &str,
        spec: &ExperimentSpec,
        submission_idempotency_key: &str,
    ) -> Result<EvaluationExperimentRecord, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_submission_key(submission_idempotency_key)?;
        let spec_json =
            serde_json::to_string(spec).map_err(|source| EvaluationPersistenceError::Json {
                operation: "serialize_evaluation_spec",
                source,
            })?;
        if spec_json.len() > MAX_PERSISTED_SPEC_BYTES {
            return Err(EvaluationPersistenceError::InvalidInput(format!(
                "evaluation spec is {} bytes; persistence limit is {MAX_PERSISTED_SPEC_BYTES}",
                spec_json.len()
            )));
        }
        let spec_fingerprint = spec
            .spec_fingerprint()
            .map_err(EvaluationPersistenceError::InvalidInput)?;
        let trials = spec
            .plan_trials_with_limits(
                MAX_PERSISTED_TRIALS,
                MAX_PERSISTED_PLAN_BYTES.saturating_sub(spec_json.len()),
            )
            .map_err(EvaluationPersistenceError::InvalidInput)?;
        let trial_jsons = trials
            .iter()
            .map(|trial| {
                serde_json::to_string(trial).map_err(|source| EvaluationPersistenceError::Json {
                    operation: "serialize_evaluation_trial",
                    source,
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        validate_persistence_budget(&spec_json, &trial_jsons)?;

        let mut tx = self.pool.get().begin().await.map_err(|source| {
            EvaluationPersistenceError::Database {
                operation: "begin_evaluation_registration",
                source,
            }
        })?;
        let existing = load_experiment_tx(&mut tx, owner_user_id, &spec.experiment_id).await?;
        if let Some(existing) = existing {
            ensure_registration_identity(&existing, &spec_fingerprint, submission_idempotency_key)?;
            let existing_trials =
                load_trial_bindings_tx(&mut tx, owner_user_id, &spec.experiment_id).await?;
            let canonical = canonical_trial_index(&existing)?;
            validate_trial_set(&existing_trials, &existing, &canonical)?;
            if existing_trials.len() != trials.len() {
                return Err(EvaluationPersistenceError::Conflict(format!(
                    "experiment {} has {} persisted trials but plan requires {}",
                    spec.experiment_id,
                    existing_trials.len(),
                    trials.len()
                )));
            }
            tx.commit()
                .await
                .map_err(|source| EvaluationPersistenceError::Database {
                    operation: "commit_existing_evaluation_registration",
                    source,
                })?;
            return Ok(existing);
        }

        let submission_owner = sqlx::query(
            "SELECT experiment_id, spec_fingerprint
             FROM evaluation_experiments
             WHERE owner_user_id = ? AND submission_idempotency_key = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(submission_idempotency_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_evaluation_submission_key",
            source,
        })?;
        if let Some(row) = submission_owner {
            let existing_experiment =
                row_string(&row, "experiment_id", "load_evaluation_submission_key")?;
            let existing_fingerprint =
                row_string(&row, "spec_fingerprint", "load_evaluation_submission_key")?;
            return Err(EvaluationPersistenceError::Conflict(format!(
                "submission key already belongs to experiment {existing_experiment} ({existing_fingerprint})"
            )));
        }

        let insert_result = sqlx::query(
            "INSERT INTO evaluation_experiments
             (owner_user_id, experiment_id, spec_fingerprint, spec_json,
              submission_idempotency_key, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(owner_user_id)
        .bind(&spec.experiment_id)
        .bind(&spec_fingerprint)
        .bind(&spec_json)
        .bind(submission_idempotency_key)
        .execute(&mut *tx)
        .await;
        if let Err(source) = insert_result {
            // MatrixOne does not consistently support an upsert for every
            // unique-key shape. Recover a concurrent duplicate by rolling
            // back and reading the committed owner-scoped identity; never
            // turn a race into a duplicate trial plan or a false success.
            let duplicate = is_duplicate_key(&source);
            let _ = tx.rollback().await;
            if duplicate {
                match self
                    .load_experiment(owner_user_id, &spec.experiment_id)
                    .await
                {
                    Ok(existing) => {
                        ensure_registration_identity(
                            &existing,
                            &spec_fingerprint,
                            submission_idempotency_key,
                        )?;
                        let existing_trials =
                            self.list_trials(owner_user_id, &spec.experiment_id).await?;
                        if existing_trials.len() != trials.len() {
                            return Err(EvaluationPersistenceError::Conflict(format!(
                                "experiment {} has {} persisted trials but plan requires {}",
                                spec.experiment_id,
                                existing_trials.len(),
                                trials.len()
                            )));
                        }
                        return Ok(existing);
                    }
                    Err(EvaluationPersistenceError::NotFound(_)) => {}
                    Err(error) => return Err(error),
                }
                if let Some(existing) = self
                    .load_experiment_by_submission(owner_user_id, submission_idempotency_key)
                    .await?
                {
                    if existing.experiment_id == spec.experiment_id {
                        ensure_registration_identity(
                            &existing,
                            &spec_fingerprint,
                            submission_idempotency_key,
                        )?;
                        let existing_trials =
                            self.list_trials(owner_user_id, &spec.experiment_id).await?;
                        if existing_trials.len() != trials.len() {
                            return Err(EvaluationPersistenceError::Conflict(format!(
                                "experiment {} has {} persisted trials but plan requires {}",
                                spec.experiment_id,
                                existing_trials.len(),
                                trials.len()
                            )));
                        }
                        return Ok(existing);
                    }
                    return Err(EvaluationPersistenceError::Conflict(format!(
                        "submission key already belongs to experiment {} ({})",
                        existing.experiment_id, existing.spec_fingerprint
                    )));
                }
            }
            return Err(EvaluationPersistenceError::Database {
                operation: "insert_evaluation_experiment",
                source,
            });
        }

        for (trial, trial_json) in trials.iter().zip(trial_jsons) {
            sqlx::query(
                "INSERT INTO evaluation_trial_bindings
                 (owner_user_id, trial_id, experiment_id, spec_fingerprint,
                  sequence_num, trial_json, binding_status, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, 'planned', NOW(6), NOW(6))",
            )
            .bind(owner_user_id)
            .bind(&trial.trial_id)
            .bind(&trial.experiment_id)
            .bind(&trial.spec_fingerprint)
            .bind(i64::from(trial.sequence))
            .bind(trial_json)
            .execute(&mut *tx)
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "insert_evaluation_trial",
                source,
            })?;
        }

        tx.commit()
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "commit_evaluation_registration",
                source,
            })?;
        self.load_experiment(owner_user_id, &spec.experiment_id)
            .await
    }

    pub async fn load_experiment(
        &self,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<EvaluationExperimentRecord, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_bounded("experiment_id", experiment_id, 128)?;
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            EvaluationPersistenceError::Database {
                operation: "begin_load_evaluation_experiment",
                source,
            }
        })?;
        let record = load_experiment_read_tx(&mut tx, owner_user_id, experiment_id)
            .await?
            .ok_or_else(|| EvaluationPersistenceError::NotFound(experiment_id.to_string()))?;
        let trial_count = count_trials_tx(&mut tx, owner_user_id, experiment_id).await?;
        if trial_count != record.planned_trial_count {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "experiment {experiment_id} trial count is inconsistent"
            )));
        }
        tx.commit()
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "commit_load_evaluation_experiment",
                source,
            })?;
        Ok(record)
    }

    pub async fn load_experiment_by_submission(
        &self,
        owner_user_id: &str,
        submission_idempotency_key: &str,
    ) -> Result<Option<EvaluationExperimentRecord>, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_submission_key(submission_idempotency_key)?;
        let experiment_id = sqlx::query_scalar::<_, String>(
            "SELECT experiment_id
             FROM evaluation_experiments
             WHERE owner_user_id = ? AND submission_idempotency_key = ?
             LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(submission_idempotency_key)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_evaluation_experiment_by_submission",
            source,
        })?;
        match experiment_id {
            Some(experiment_id) => self
                .load_experiment(owner_user_id, &experiment_id)
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub async fn list_trials(
        &self,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<Vec<EvaluationTrialBindingRecord>, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_bounded("experiment_id", experiment_id, 128)?;
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            EvaluationPersistenceError::Database {
                operation: "begin_list_evaluation_trials",
                source,
            }
        })?;
        let experiment = load_experiment_read_tx(&mut tx, owner_user_id, experiment_id)
            .await?
            .ok_or_else(|| EvaluationPersistenceError::NotFound(experiment_id.to_string()))?;
        let trials = load_trial_bindings_tx(&mut tx, owner_user_id, experiment_id).await?;
        let canonical = canonical_trial_index(&experiment)?;
        validate_trial_set(&trials, &experiment, &canonical)?;
        tx.commit()
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "commit_list_evaluation_trials",
                source,
            })?;
        Ok(trials)
    }

    /// Read the current canonical Run status for every bound trial in one
    /// owner-scoped query. A missing row is intentionally omitted: the
    /// projection layer distinguishes an unobserved terminal Run from a Run
    /// whose status could not be read, rather than turning either into a
    /// successful result.
    pub async fn list_trial_run_statuses(
        &self,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<BTreeMap<String, String>, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_bounded("experiment_id", experiment_id, 128)?;
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            EvaluationPersistenceError::Database {
                operation: "begin_list_evaluation_trial_run_statuses",
                source,
            }
        })?;
        let statuses =
            Self::list_trial_run_statuses_tx(&mut tx, owner_user_id, experiment_id).await?;
        tx.commit()
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "commit_list_evaluation_trial_run_statuses",
                source,
            })?;
        Ok(statuses)
    }
    pub(crate) async fn list_trial_run_statuses_tx(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<BTreeMap<String, String>, EvaluationPersistenceError> {
        let rows = sqlx::query(
            "SELECT b.trial_id, r.status
             FROM evaluation_trial_bindings b
             LEFT JOIN agent_runs r
               ON r.user_id = b.owner_user_id
              AND r.run_id = b.run_id
              AND r.session_id = b.session_id
             WHERE b.owner_user_id = ? AND b.experiment_id = ?
             ORDER BY b.sequence_num ASC, b.trial_id ASC
             LIMIT 4096",
        )
        .bind(owner_user_id)
        .bind(experiment_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "list_evaluation_trial_run_statuses",
            source,
        })?;
        let mut statuses = BTreeMap::new();
        for row in rows {
            let trial_id = row_string(&row, "trial_id", "decode_evaluation_trial_run_status")?;
            let status = row
                .try_get::<Option<String>, _>("status")
                .map_err(|source| EvaluationPersistenceError::Database {
                    operation: "decode_evaluation_trial_run_status",
                    source,
                })?;
            if let Some(status) = status {
                statuses.insert(trial_id, status);
            }
        }
        Ok(statuses)
    }

    /// Load one owner-scoped persisted trial without scanning every persisted
    /// binding. The frozen plan is still validated before the result is used;
    /// this keeps settlement's database read bounded even when many trials
    /// share the experiment.
    pub async fn load_trial(
        &self,
        owner_user_id: &str,
        trial_id: &str,
    ) -> Result<EvaluationTrialBindingRecord, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_bounded("trial_id", trial_id, MAX_TRIAL_ID_BYTES)?;
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            EvaluationPersistenceError::Database {
                operation: "begin_load_evaluation_trial",
                source,
            }
        })?;
        let row = sqlx::query(
            "SELECT owner_user_id, trial_id, experiment_id, spec_fingerprint,
                    sequence_num, trial_json, binding_status, session_id, run_id,
                    run_generation,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM evaluation_trial_bindings
             WHERE owner_user_id = ? AND trial_id = ? LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(trial_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_evaluation_trial",
            source,
        })?
        .ok_or_else(|| EvaluationPersistenceError::NotFound(trial_id.to_string()))?;
        let binding = decode_trial_binding(row)?;
        let experiment = load_experiment_read_tx(&mut tx, owner_user_id, &binding.experiment_id)
            .await?
            .ok_or_else(|| {
                EvaluationPersistenceError::Conflict(format!(
                    "trial {trial_id} references a missing experiment {}",
                    binding.experiment_id
                ))
            })?;
        validate_single_trial_against_spec(&experiment, &binding)?;
        tx.commit()
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "commit_load_evaluation_trial",
                source,
            })?;
        Ok(binding)
    }

    /// Confirm the binding created atomically with the canonical Run. This
    /// entrypoint never binds a planned trial and still fences the current generation.
    pub async fn bind_trial_run(
        &self,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<EvaluationTrialBindingRecord, EvaluationPersistenceError> {
        validate_owner(owner_user_id)?;
        validate_bounded("trial_id", trial_id, MAX_TRIAL_ID_BYTES)?;
        validate_bounded("session_id", session_id, MAX_SESSION_ID_BYTES)?;
        validate_bounded("run_id", run_id, MAX_RUN_ID_BYTES)?;
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            EvaluationPersistenceError::Database {
                operation: "begin_bind_evaluation_trial",
                source,
            }
        })?;
        let (_, existing, run_generation) =
            Self::lock_trial_run(&mut tx, owner_user_id, trial_id, session_id, run_id).await?;
        if let (Some(existing_session), Some(existing_run)) =
            (existing.session_id.as_deref(), existing.run_id.as_deref())
        {
            if existing_session == session_id && existing_run == run_id {
                if existing.run_generation != Some(run_generation) {
                    return Err(EvaluationPersistenceError::Conflict(format!(
                        "run {run_id} generation changed while its evaluation binding was reused"
                    )));
                }
                tx.commit()
                    .await
                    .map_err(|source| EvaluationPersistenceError::Database {
                        operation: "commit_idempotent_evaluation_trial_binding",
                        source,
                    })?;
                return Ok(existing);
            }
            return Err(EvaluationPersistenceError::Conflict(format!(
                "trial {trial_id} is already bound to another run"
            )));
        }
        Err(EvaluationPersistenceError::Conflict(
            "evaluation binding must be created atomically with its canonical Run".into(),
        ))
    }

    /// Called only after the shared locks and caller-specific admission validation.
    pub(crate) async fn bind_locked_trial(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
        run_id: &str,
        run_generation: u64,
    ) -> Result<EvaluationTrialBindingRecord, EvaluationPersistenceError> {
        let result = match sqlx::query(
            "UPDATE evaluation_trial_bindings
             SET binding_status = 'bound', session_id = ?, run_id = ?, run_generation = ?, updated_at = NOW(6)
             WHERE owner_user_id = ? AND trial_id = ? AND binding_status = 'planned'
               AND session_id IS NULL AND run_id IS NULL AND run_generation IS NULL",
        )
        .bind(session_id)
        .bind(run_id)
        .bind(i64::try_from(run_generation).map_err(|_| {
            EvaluationPersistenceError::InvalidInput("run generation exceeds BIGINT".to_string())
        })?)
        .bind(owner_user_id)
        .bind(trial_id)
        .execute(&mut **tx)
        .await
        {
            Ok(result) => result,
            Err(source) if is_duplicate_key(&source) => {
                return Err(EvaluationPersistenceError::Conflict(format!(
                    "run {run_id} is already bound to another evaluation trial"
                )));
            }
            Err(source) => {
                return Err(EvaluationPersistenceError::Database {
                    operation: "bind_evaluation_trial_run",
                    source,
                });
            }
        };
        if result.rows_affected() != 1 {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "trial {trial_id} was bound concurrently"
            )));
        }
        let bound = sqlx::query(
            "SELECT owner_user_id, trial_id, experiment_id, spec_fingerprint,
                    sequence_num, trial_json, binding_status, session_id, run_id,
                    run_generation,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM evaluation_trial_bindings
             WHERE owner_user_id = ? AND trial_id = ? LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(trial_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_bound_evaluation_trial",
            source,
        })?;
        let bound = decode_trial_binding(bound)?;
        Ok(bound)
    }

    /// Shared lock order for binding, materialization and observation: Session -> Run -> Trial.
    /// This only locks and validates identities/frozen plan; callers choose their generation fence.
    pub(crate) async fn lock_trial_run(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<
        (
            EvaluationExperimentRecord,
            EvaluationTrialBindingRecord,
            u64,
        ),
        EvaluationPersistenceError,
    > {
        Self::lock_trial_run_identity(tx, owner_user_id, trial_id, session_id, run_id, true).await
    }

    /// Historical assessment locks identities without requiring an active Session.
    pub(crate) async fn lock_historical_trial_run(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<
        (
            EvaluationExperimentRecord,
            EvaluationTrialBindingRecord,
            u64,
        ),
        EvaluationPersistenceError,
    > {
        Self::lock_trial_run_identity(tx, owner_user_id, trial_id, session_id, run_id, false).await
    }

    async fn lock_trial_run_identity(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
        run_id: &str,
        require_active_session: bool,
    ) -> Result<
        (
            EvaluationExperimentRecord,
            EvaluationTrialBindingRecord,
            u64,
        ),
        EvaluationPersistenceError,
    > {
        let session_exists = sqlx::query(
            "SELECT status FROM agent_sessions
             WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "validate_evaluation_trial_session",
            source,
        })?;
        let Some(session_row) = session_exists else {
            return Err(EvaluationPersistenceError::NotFound(format!(
                "session {session_id}"
            )));
        };
        let session_status =
            row_string(&session_row, "status", "validate_evaluation_trial_session")?;
        if require_active_session && session_status != "active" {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "session {session_id} is not active"
            )));
        }
        let run_exists = sqlx::query(
            "SELECT session_id, run_generation FROM agent_runs
             WHERE user_id = ? AND run_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(run_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "validate_evaluation_trial_run",
            source,
        })?;
        let Some(run_row) = run_exists else {
            return Err(EvaluationPersistenceError::NotFound(format!(
                "run {run_id}"
            )));
        };
        if row_string(&run_row, "session_id", "validate_evaluation_trial_run")? != session_id {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "run {run_id} belongs to another session"
            )));
        }
        let run_generation = row_i64(&run_row, "run_generation", "validate_evaluation_trial_run")?;
        let run_generation = u64::try_from(run_generation).map_err(|_| {
            EvaluationPersistenceError::Conflict(format!("run {run_id} has an invalid generation"))
        })?;

        let row = sqlx::query(
            "SELECT owner_user_id, trial_id, experiment_id, spec_fingerprint,
                    sequence_num, trial_json, binding_status, session_id, run_id,
                    run_generation,
                    CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM evaluation_trial_bindings
             WHERE owner_user_id = ? AND trial_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(trial_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_evaluation_trial_for_binding",
            source,
        })?;
        let Some(row) = row else {
            return Err(EvaluationPersistenceError::NotFound(format!(
                "trial {trial_id}"
            )));
        };
        let existing = decode_trial_binding(row)?;
        let experiment = load_experiment_read_tx(tx, owner_user_id, &existing.experiment_id)
            .await?
            .ok_or_else(|| {
                EvaluationPersistenceError::Conflict(format!(
                    "trial {trial_id} references a missing experiment {}",
                    existing.experiment_id
                ))
            })?;
        validate_single_trial_against_spec(&experiment, &existing)?;
        Ok((experiment, existing, run_generation))
    }

    /// Lock and validate the immutable owner-scoped trial binding for a
    /// materializer. The caller must keep the transaction open while it
    /// records a receipt so a receipt can never be attached to a trial that
    /// is concurrently rebound or partially repaired.
    pub(crate) async fn lock_bound_trial_for_receipt(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
    ) -> Result<
        (EvaluationExperimentRecord, EvaluationTrialBindingRecord),
        EvaluationPersistenceError,
    > {
        validate_owner(owner_user_id)?;
        validate_bounded("trial_id", trial_id, MAX_TRIAL_ID_BYTES)?;
        validate_bounded("session_id", session_id, MAX_SESSION_ID_BYTES)?;
        // Discover the canonical Run without taking a lock first. The lock
        // order below then matches bind_trial_run: Session -> Run -> Trial.
        // That keeps a first bind and a receipt attempt from deadlocking.
        let trial_run_id = sqlx::query_scalar::<_, Option<String>>(
            "SELECT run_id FROM evaluation_trial_bindings
             WHERE owner_user_id = ? AND trial_id = ? LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(trial_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "discover_evaluation_trial_run_for_receipt",
            source,
        })?;
        let Some(trial_run_id) = trial_run_id else {
            return Err(EvaluationPersistenceError::NotFound(format!(
                "trial {trial_id}"
            )));
        };
        let Some(trial_run_id) = trial_run_id else {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "trial {trial_id} is not bound to a canonical run"
            )));
        };
        let (experiment, binding, current_generation) =
            Self::lock_trial_run(tx, owner_user_id, trial_id, session_id, &trial_run_id).await?;
        if binding.binding_status != "bound"
            || binding.session_id.as_deref() != Some(session_id)
            || binding.run_id.as_deref() != Some(trial_run_id.as_str())
            || binding.run_generation != Some(current_generation)
        {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "trial {trial_id} is not bound to the current run generation for session {session_id}"
            )));
        }
        Ok((experiment, binding))
    }
}

async fn load_experiment_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
) -> Result<Option<EvaluationExperimentRecord>, EvaluationPersistenceError> {
    load_experiment_tx_with_lock(tx, owner_user_id, experiment_id, true).await
}

pub(crate) async fn load_experiment_read_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
) -> Result<Option<EvaluationExperimentRecord>, EvaluationPersistenceError> {
    load_experiment_tx_with_lock(tx, owner_user_id, experiment_id, false).await
}

async fn load_experiment_tx_with_lock(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
    lock: bool,
) -> Result<Option<EvaluationExperimentRecord>, EvaluationPersistenceError> {
    let sql = if lock {
        "SELECT owner_user_id, experiment_id, spec_fingerprint, spec_json,
                submission_idempotency_key,
                CAST(created_at AS CHAR) AS created_at,
                CAST(updated_at AS CHAR) AS updated_at
         FROM evaluation_experiments
         WHERE owner_user_id = ? AND experiment_id = ? LIMIT 1 FOR UPDATE"
    } else {
        "SELECT owner_user_id, experiment_id, spec_fingerprint, spec_json,
                submission_idempotency_key,
                CAST(created_at AS CHAR) AS created_at,
                CAST(updated_at AS CHAR) AS updated_at
         FROM evaluation_experiments
         WHERE owner_user_id = ? AND experiment_id = ? LIMIT 1"
    };
    let row = sqlx::query(sql)
        .bind(owner_user_id)
        .bind(experiment_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_evaluation_experiment",
            source,
        })?;
    row.map(decode_experiment).transpose()
}

pub(crate) async fn count_trials_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
) -> Result<usize, EvaluationPersistenceError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM evaluation_trial_bindings
         WHERE owner_user_id = ? AND experiment_id = ?",
    )
    .bind(owner_user_id)
    .bind(experiment_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| EvaluationPersistenceError::Database {
        operation: "count_evaluation_trials",
        source,
    })?;
    usize::try_from(count).map_err(|_| {
        EvaluationPersistenceError::InvalidInput("evaluation trial count overflow".to_string())
    })
}

pub(crate) async fn load_trial_bindings_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
) -> Result<Vec<EvaluationTrialBindingRecord>, EvaluationPersistenceError> {
    load_trial_bindings_with_lock_tx(tx, owner_user_id, experiment_id, false).await
}

async fn load_trial_bindings_with_lock_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
    lock: bool,
) -> Result<Vec<EvaluationTrialBindingRecord>, EvaluationPersistenceError> {
    let lock = if lock { " FOR UPDATE" } else { "" };
    let sql = format!(
        "SELECT owner_user_id, trial_id, experiment_id, spec_fingerprint,
                    sequence_num, trial_json, binding_status, session_id, run_id,
                    run_generation,
                CAST(created_at AS CHAR) AS created_at,
                CAST(updated_at AS CHAR) AS updated_at
         FROM evaluation_trial_bindings
         WHERE owner_user_id = ? AND experiment_id = ?
         ORDER BY sequence_num ASC, trial_id ASC{lock}",
    );
    let rows = sqlx::query(&sql)
        .bind(owner_user_id)
        .bind(experiment_id)
        .fetch_all(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "list_evaluation_trials",
            source,
        })?;
    rows.into_iter()
        .map(decode_trial_binding)
        .collect::<Result<Vec<_>, _>>()
}

fn decode_experiment(
    row: sqlx::mysql::MySqlRow,
) -> Result<EvaluationExperimentRecord, EvaluationPersistenceError> {
    let owner_user_id = row_string(&row, "owner_user_id", "decode_evaluation_experiment")?;
    let experiment_id = row_string(&row, "experiment_id", "decode_evaluation_experiment")?;
    validate_bounded_value("owner_user_id", &owner_user_id, MAX_OWNER_ID_BYTES)?;
    validate_bounded_value("experiment_id", &experiment_id, 128)?;
    let spec_json = row_string(&row, "spec_json", "decode_evaluation_experiment")?;
    let spec: ExperimentSpec =
        serde_json::from_str(&spec_json).map_err(|source| EvaluationPersistenceError::Json {
            operation: "deserialize_evaluation_spec",
            source,
        })?;
    let spec_fingerprint = row_string(&row, "spec_fingerprint", "decode_evaluation_experiment")?;
    let computed = spec
        .spec_fingerprint()
        .map_err(EvaluationPersistenceError::InvalidInput)?;
    if computed != spec_fingerprint {
        return Err(EvaluationPersistenceError::Conflict(
            "persisted evaluation spec fingerprint does not match its content".to_string(),
        ));
    }
    if spec.experiment_id != experiment_id {
        return Err(EvaluationPersistenceError::Conflict(
            "persisted experiment identity does not match its key".to_string(),
        ));
    }
    let planned_trial_count = spec
        .planned_trial_count()
        .map_err(EvaluationPersistenceError::InvalidInput)?;
    Ok(EvaluationExperimentRecord {
        owner_user_id,
        experiment_id,
        spec_fingerprint,
        spec,
        submission_idempotency_key: row_string(
            &row,
            "submission_idempotency_key",
            "decode_evaluation_experiment",
        )?,
        planned_trial_count,
        created_at: row_string(&row, "created_at", "decode_evaluation_experiment")?,
        updated_at: row_string(&row, "updated_at", "decode_evaluation_experiment")?,
    })
}

fn decode_trial_binding(
    row: sqlx::mysql::MySqlRow,
) -> Result<EvaluationTrialBindingRecord, EvaluationPersistenceError> {
    let trial_json = row_string(&row, "trial_json", "decode_evaluation_trial")?;
    let trial: TrialUnit =
        serde_json::from_str(&trial_json).map_err(|source| EvaluationPersistenceError::Json {
            operation: "deserialize_evaluation_trial",
            source,
        })?;
    let trial_id = row_string(&row, "trial_id", "decode_evaluation_trial")?;
    if trial.trial_id != trial_id {
        return Err(EvaluationPersistenceError::Conflict(
            "persisted trial identity does not match its key".to_string(),
        ));
    }
    let owner_user_id = row_string(&row, "owner_user_id", "decode_evaluation_trial")?;
    let experiment_id = row_string(&row, "experiment_id", "decode_evaluation_trial")?;
    let spec_fingerprint = row_string(&row, "spec_fingerprint", "decode_evaluation_trial")?;
    validate_bounded_value("owner_user_id", &owner_user_id, MAX_OWNER_ID_BYTES)?;
    validate_bounded_value("trial_id", &trial_id, MAX_TRIAL_ID_BYTES)?;
    validate_bounded_value("experiment_id", &experiment_id, 128)?;
    if trial.experiment_id != experiment_id || trial.spec_fingerprint != spec_fingerprint {
        return Err(EvaluationPersistenceError::Conflict(
            "persisted trial metadata does not match its content".to_string(),
        ));
    }
    let sequence_num: i64 =
        row.try_get("sequence_num")
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "decode_evaluation_trial",
                source,
            })?;
    let sequence_num = u32::try_from(sequence_num).map_err(|_| {
        EvaluationPersistenceError::Conflict("persisted trial sequence is invalid".to_string())
    })?;
    if sequence_num == 0 || trial.sequence != sequence_num {
        return Err(EvaluationPersistenceError::Conflict(
            "persisted trial sequence does not match its content".to_string(),
        ));
    }
    let binding_status = row_string(&row, "binding_status", "decode_evaluation_trial")?;
    let session_id: Option<String> =
        row.try_get("session_id")
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "decode_evaluation_trial",
                source,
            })?;
    let run_id: Option<String> =
        row.try_get("run_id")
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "decode_evaluation_trial",
                source,
            })?;
    let run_generation: Option<i64> =
        row.try_get("run_generation")
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "decode_evaluation_trial",
                source,
            })?;
    let run_generation = run_generation
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                EvaluationPersistenceError::Conflict(
                    "persisted evaluation run generation is invalid".to_string(),
                )
            })
        })
        .transpose()?;
    match binding_status.as_str() {
        "planned" if session_id.is_none() && run_id.is_none() && run_generation.is_none() => {}
        "bound" if session_id.is_some() && run_id.is_some() && run_generation.is_some() => {}
        "planned" | "bound" => {
            return Err(EvaluationPersistenceError::Conflict(
                "persisted evaluation trial binding is incomplete".to_string(),
            ));
        }
        _ => {
            return Err(EvaluationPersistenceError::Conflict(
                "persisted evaluation trial binding status is invalid".to_string(),
            ));
        }
    }
    Ok(EvaluationTrialBindingRecord {
        owner_user_id,
        trial_id,
        experiment_id,
        spec_fingerprint,
        trial,
        binding_status,
        session_id,
        run_id,
        run_generation,
        created_at: row_string(&row, "created_at", "decode_evaluation_trial")?,
        updated_at: row_string(&row, "updated_at", "decode_evaluation_trial")?,
    })
}

fn canonical_trial_index(
    experiment: &EvaluationExperimentRecord,
) -> Result<HashMap<String, TrialUnit>, EvaluationPersistenceError> {
    let planned = experiment
        .spec
        .plan_trials()
        .map_err(EvaluationPersistenceError::InvalidInput)?;
    if planned.len() != experiment.planned_trial_count {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "experiment {} has an invalid planned trial count",
            experiment.experiment_id
        )));
    }
    Ok(planned
        .into_iter()
        .map(|trial| (trial.trial_id.clone(), trial))
        .collect())
}

fn validate_single_trial_against_spec(
    experiment: &EvaluationExperimentRecord,
    binding: &EvaluationTrialBindingRecord,
) -> Result<(), EvaluationPersistenceError> {
    experiment
        .spec
        .validate_trial_identity(&binding.trial)
        .map_err(EvaluationPersistenceError::Conflict)?;
    let expected_sequence = experiment
        .spec
        .canonical_trial_sequence(&binding.trial)
        .map_err(EvaluationPersistenceError::Conflict)?;
    if binding.trial.sequence != expected_sequence {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "trial {} sequence {} does not match canonical sequence {expected_sequence}",
            binding.trial.trial_id, binding.trial.sequence
        )));
    }
    Ok(())
}

fn validate_trial_set(
    trials: &[EvaluationTrialBindingRecord],
    experiment: &EvaluationExperimentRecord,
    canonical: &HashMap<String, TrialUnit>,
) -> Result<(), EvaluationPersistenceError> {
    if trials.len() != canonical.len() {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "experiment {} has {} persisted trials but plan requires {}",
            experiment.experiment_id,
            trials.len(),
            canonical.len()
        )));
    }
    let mut seen = HashSet::with_capacity(trials.len());
    for trial in trials {
        validate_trial_against_experiment(trial, experiment, canonical)?;
        if !seen.insert(trial.trial_id.as_str()) {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "experiment {} contains duplicate trial {}",
                experiment.experiment_id, trial.trial_id
            )));
        }
    }
    Ok(())
}

fn validate_trial_against_experiment(
    trial: &EvaluationTrialBindingRecord,
    experiment: &EvaluationExperimentRecord,
    canonical: &HashMap<String, TrialUnit>,
) -> Result<(), EvaluationPersistenceError> {
    if trial.owner_user_id != experiment.owner_user_id
        || trial.experiment_id != experiment.experiment_id
        || trial.spec_fingerprint != experiment.spec_fingerprint
    {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "trial {} is not bound to its owner-scoped experiment",
            trial.trial_id
        )));
    }
    let expected = canonical.get(&trial.trial_id).ok_or_else(|| {
        EvaluationPersistenceError::Conflict(format!(
            "trial {} is not part of the canonical evaluation plan",
            trial.trial_id
        ))
    })?;
    if &trial.trial != expected {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "trial {} does not match the canonical evaluation plan",
            trial.trial_id
        )));
    }
    Ok(())
}

fn ensure_registration_identity(
    existing: &EvaluationExperimentRecord,
    spec_fingerprint: &str,
    submission_idempotency_key: &str,
) -> Result<(), EvaluationPersistenceError> {
    if existing.spec_fingerprint != spec_fingerprint {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "experiment {} already exists with another spec fingerprint",
            existing.experiment_id
        )));
    }
    if existing.submission_idempotency_key != submission_idempotency_key {
        return Err(EvaluationPersistenceError::Conflict(format!(
            "experiment {} already exists with another submission key",
            existing.experiment_id
        )));
    }
    Ok(())
}

pub(crate) fn validate_owner(owner_user_id: &str) -> Result<(), EvaluationPersistenceError> {
    validate_bounded("owner_user_id", owner_user_id, MAX_OWNER_ID_BYTES)
}

fn validate_submission_key(key: &str) -> Result<(), EvaluationPersistenceError> {
    validate_bounded("submission_idempotency_key", key, MAX_SUBMISSION_KEY_BYTES)
}

fn validate_non_empty(label: &str, value: &str) -> Result<(), EvaluationPersistenceError> {
    if value.trim().is_empty() {
        return Err(EvaluationPersistenceError::InvalidInput(format!(
            "{label} must not be empty"
        )));
    }
    Ok(())
}

pub(crate) fn validate_bounded(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), EvaluationPersistenceError> {
    validate_non_empty(label, value)?;
    if value.len() > max_bytes {
        return Err(EvaluationPersistenceError::InvalidInput(format!(
            "{label} must be at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn validate_bounded_value(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), EvaluationPersistenceError> {
    validate_bounded(label, value, max_bytes)
}

fn validate_persistence_budget(
    spec_json: &str,
    trial_jsons: &[String],
) -> Result<(), EvaluationPersistenceError> {
    if spec_json.len() > MAX_PERSISTED_SPEC_BYTES {
        return Err(EvaluationPersistenceError::InvalidInput(format!(
            "evaluation spec is {} bytes; persistence limit is {MAX_PERSISTED_SPEC_BYTES}",
            spec_json.len()
        )));
    }
    let mut total_bytes = spec_json.len();
    for trial_json in trial_jsons {
        if trial_json.len() > MAX_PERSISTED_TRIAL_BYTES {
            return Err(EvaluationPersistenceError::InvalidInput(format!(
                "evaluation trial is {} bytes; persistence limit is {MAX_PERSISTED_TRIAL_BYTES}",
                trial_json.len()
            )));
        }
        total_bytes = total_bytes.checked_add(trial_json.len()).ok_or_else(|| {
            EvaluationPersistenceError::InvalidInput(
                "evaluation plan serialization size overflow".to_string(),
            )
        })?;
    }
    if total_bytes > MAX_PERSISTED_PLAN_BYTES {
        return Err(EvaluationPersistenceError::InvalidInput(format!(
            "evaluation plan is {total_bytes} bytes; persistence limit is {MAX_PERSISTED_PLAN_BYTES}"
        )));
    }
    Ok(())
}

pub(crate) fn is_duplicate_key(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::Database(database_error)
            if database_error.code().as_deref() == Some("1062")
                || database_error.message().contains("Duplicate")
    )
}

fn row_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
    operation: &'static str,
) -> Result<String, EvaluationPersistenceError> {
    row.try_get(column)
        .map_err(|source| EvaluationPersistenceError::Database { operation, source })
}

fn row_i64(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
    operation: &'static str,
) -> Result<i64, EvaluationPersistenceError> {
    row.try_get(column)
        .map_err(|source| EvaluationPersistenceError::Database { operation, source })
}
