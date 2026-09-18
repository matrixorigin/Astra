//! Owner-scoped persistence for generic evaluation registrations and trial
//! bindings. This module records execution identity; it does not run a model
//! or materialize a branch.

use super::experiment::{ExperimentSpec, TrialUnit};
use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use sqlx::{MySql, Row, Transaction};
use std::collections::{HashMap, HashSet};
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

#[derive(Debug, Error)]
pub enum EvaluationPersistenceError {
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

    async fn load_experiment_by_submission(
        &self,
        owner_user_id: &str,
        submission_idempotency_key: &str,
    ) -> Result<Option<EvaluationExperimentRecord>, EvaluationPersistenceError> {
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

    /// Bind an existing canonical Run to one planned trial with a CAS on the
    /// unbound row. The session and run are checked under the same transaction
    /// and must belong to the requesting owner.
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
        let session_exists = sqlx::query(
            "SELECT status FROM agent_sessions
             WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(session_id)
        .fetch_optional(&mut *tx)
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
        if session_status != "active" {
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
        .fetch_optional(&mut *tx)
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
        .fetch_optional(&mut *tx)
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
        let experiment = load_experiment_read_tx(&mut tx, owner_user_id, &existing.experiment_id)
            .await?
            .ok_or_else(|| {
                EvaluationPersistenceError::Conflict(format!(
                    "trial {trial_id} references a missing experiment {}",
                    existing.experiment_id
                ))
            })?;
        let canonical = canonical_trial_index(&experiment)?;
        validate_trial_against_experiment(&existing, &experiment, &canonical)?;
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
        if existing.session_id.is_some() || existing.run_id.is_some() {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "trial {trial_id} has a partial binding"
            )));
        }
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
        .execute(&mut *tx)
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
        .fetch_one(&mut *tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "load_bound_evaluation_trial",
            source,
        })?;
        let bound = decode_trial_binding(bound)?;
        tx.commit()
            .await
            .map_err(|source| EvaluationPersistenceError::Database {
                operation: "commit_evaluation_trial_binding",
                source,
            })?;
        Ok(bound)
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
        let session_row = sqlx::query(
            "SELECT status FROM agent_sessions
             WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(session_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "validate_evaluation_receipt_session",
            source,
        })?
        .ok_or_else(|| EvaluationPersistenceError::NotFound(format!("session {session_id}")))?;
        if row_string(
            &session_row,
            "status",
            "validate_evaluation_receipt_session",
        )? != "active"
        {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "session {session_id} is not active"
            )));
        }
        let run_row = sqlx::query(
            "SELECT session_id, run_generation FROM agent_runs
             WHERE user_id = ? AND run_id = ? LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(&trial_run_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationPersistenceError::Database {
            operation: "validate_evaluation_receipt_run",
            source,
        })?
        .ok_or_else(|| EvaluationPersistenceError::NotFound(format!("run {trial_run_id}")))?;
        if row_string(&run_row, "session_id", "validate_evaluation_receipt_run")? != session_id {
            return Err(EvaluationPersistenceError::Conflict(format!(
                "run {trial_run_id} belongs to another session"
            )));
        }
        let current_generation = u64::try_from(row_i64(
            &run_row,
            "run_generation",
            "validate_evaluation_receipt_run",
        )?)
        .map_err(|_| {
            EvaluationPersistenceError::Conflict(format!(
                "run {trial_run_id} has an invalid generation"
            ))
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
            operation: "lock_evaluation_trial_for_receipt",
            source,
        })?;
        let Some(row) = row else {
            return Err(EvaluationPersistenceError::NotFound(format!(
                "trial {trial_id}"
            )));
        };
        let binding = decode_trial_binding(row)?;
        let experiment = load_experiment_read_tx(tx, owner_user_id, &binding.experiment_id)
            .await?
            .ok_or_else(|| {
                EvaluationPersistenceError::Conflict(format!(
                    "trial {trial_id} references a missing experiment {}",
                    binding.experiment_id
                ))
            })?;
        let canonical = canonical_trial_index(&experiment)?;
        validate_trial_against_experiment(&binding, &experiment, &canonical)?;
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

async fn load_experiment_read_tx(
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

async fn count_trials_tx(
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

async fn load_trial_bindings_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
) -> Result<Vec<EvaluationTrialBindingRecord>, EvaluationPersistenceError> {
    let rows = sqlx::query(
        "SELECT owner_user_id, trial_id, experiment_id, spec_fingerprint,
                    sequence_num, trial_json, binding_status, session_id, run_id,
                    run_generation,
                CAST(created_at AS CHAR) AS created_at,
                CAST(updated_at AS CHAR) AS updated_at
         FROM evaluation_trial_bindings
         WHERE owner_user_id = ? AND experiment_id = ?
         ORDER BY sequence_num ASC, trial_id ASC",
    )
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

fn validate_owner(owner_user_id: &str) -> Result<(), EvaluationPersistenceError> {
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

fn validate_bounded(
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
