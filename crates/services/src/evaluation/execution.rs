//! Canonical evaluation admission and terminal observation persistence.
//!
//! This module does not run an agent or create a second lifecycle.  It carries
//! the small amount of immutable evaluation metadata through the existing Run
//! backbone and records the terminal fact produced by that Run.  The runtime
//! is responsible for materializing the actual evaluation context/policy and
//! for calling this store at the canonical admission and settlement fences.

use super::assessment::{
    ComparisonArm, EvidenceAvailability, EvidenceRef, Measurement, MeasurementStatus,
    TrialObservation, TrialStatus,
};
use super::durable::{
    DatabaseEvaluationPlanStore, EvaluationPersistenceError, EvaluationTrialBindingRecord,
    is_duplicate_key,
};
use super::experiment::{ExperimentSpec, SnapshotEnvelope};
use astra_core::composite_snapshot::CompositeSnapshot;
use astra_core::{SharedPool, canonical_json_string};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

pub const EVALUATION_EXECUTION_SCHEMA_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 64;
const MAX_RUN_ID_BYTES: usize = 128;
const MAX_RECEIPTS: usize = 8;
const MAX_EVIDENCE: usize = 32;
const MAX_MEASUREMENTS: usize = 32;
const MAX_TEXT_BYTES: usize = 512;
const MAX_OBSERVATION_BYTES: usize = 256 * 1024;

/// Immutable owner-scoped identity for a Skill target revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationSkillRevision {
    pub skill_name: String,
    pub revision_id: String,
    pub content_hash: String,
}

impl EvaluationSkillRevision {
    pub fn validate_shape(&self) -> Result<(), String> {
        validate_id("skill_name", &self.skill_name, MAX_ID_BYTES)?;
        validate_id("skill revision_id", &self.revision_id, MAX_ID_BYTES)?;
        validate_hash("skill content_hash", &self.content_hash)
    }
}

/// Metadata injected only by a trusted evaluation entrypoint. It is not a
/// client wire field and is deliberately separate from the prompt so the
/// experiment identity cannot perturb prompt caching or model context.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationRunAdmission {
    pub experiment_id: String,
    pub trial_id: String,
    pub input_content_hash: String,
    pub revision_content_hash: String,
    /// A Skill target carries the owner-scoped immutable revision identity.
    /// Prompt targets leave this unset and use the frozen system prompt as the
    /// revision material instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_revision: Option<EvaluationSkillRevision>,
    /// A caller may provide receipts from a trusted external materializer.
    /// The runtime adapter fills this list before execution when
    /// it can prove the frozen Context/Policy hashes locally.
    #[serde(default)]
    pub receipt_ids: Vec<String>,
    /// Filled by the admission boundary after the actual run/session identity
    /// is known.  A settlement must carry the exact same envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_envelope: Option<SnapshotEnvelope>,
}

impl EvaluationRunAdmission {
    pub fn validate_shape(&self) -> Result<(), String> {
        validate_id("experiment_id", &self.experiment_id, MAX_ID_BYTES)?;
        validate_id("trial_id", &self.trial_id, MAX_ID_BYTES)?;
        validate_hash("input_content_hash", &self.input_content_hash)?;
        validate_hash("revision_content_hash", &self.revision_content_hash)?;
        if let Some(skill_revision) = &self.skill_revision {
            skill_revision.validate_shape()?;
            if skill_revision.content_hash != self.revision_content_hash {
                return Err(
                    "skill revision content_hash must match revision_content_hash".to_string(),
                );
            }
        }
        validate_receipt_ids(&self.receipt_ids).map_err(|error| error.to_string())?;
        if let Some(envelope) = &self.snapshot_envelope {
            envelope.validate()?;
        }
        Ok(())
    }
}

/// The only write accepted at the evaluation result boundary.  A terminal
/// Run may be settled more than once by retries, but every exact retry must
/// return the immutable first result and a conflicting result is rejected.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationObservationRequest {
    pub session_id: String,
    pub execution_run_id: String,
    pub execution_run_generation: u64,
    pub observation: TrialObservation,
    #[serde(default)]
    pub materialization_receipt_ids: Vec<String>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationObservationRecord {
    pub schema_version: u32,
    pub owner_user_id: String,
    pub observation_id: String,
    pub experiment_id: String,
    pub trial_id: String,
    pub session_id: String,
    pub execution_run_id: String,
    pub execution_run_generation: u64,
    pub observation: TrialObservation,
    pub materialization_receipt_ids: Vec<String>,
    pub request_fingerprint: String,
    pub idempotency_key: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Bounded marker proof used by status/projection repair before hydrating a
/// canonical Run event stream.  The admission marker identifies the exact
/// trial and generation; the settlement bit proves that terminal accounting
/// and the durable settlement fence have both been committed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvaluationAdmissionMarker {
    pub admission: EvaluationRunAdmission,
    pub execution_run_generation: u64,
    pub settlement_finished: bool,
}

#[derive(Debug, Error)]
pub enum EvaluationExecutionError {
    #[error("invalid evaluation execution input: {0}")]
    InvalidInput(String),
    #[error("evaluation execution database operation failed: {operation}: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("evaluation execution JSON operation failed: {operation}: {source}")]
    Json {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("evaluation execution identity conflict: {0}")]
    Conflict(String),
    #[error("evaluation execution record not found: {0}")]
    NotFound(String),
    #[error(transparent)]
    Persistence(#[from] EvaluationPersistenceError),
}

#[derive(Clone)]
pub struct DatabaseEvaluationObservationStore {
    pool: SharedPool,
}

impl DatabaseEvaluationObservationStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Record the terminal fact for one currently fenced canonical Run.
    ///
    /// The first idempotency lookup intentionally precedes the current-run
    /// lock: an old retry may recover an already committed immutable result,
    /// but a new or conflicting result must pass the live generation fence.
    pub async fn record_observation(
        &self,
        owner_user_id: &str,
        request: &EvaluationObservationRequest,
    ) -> Result<EvaluationObservationRecord, EvaluationExecutionError> {
        validate_id("owner_user_id", owner_user_id, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        validate_request_shape(request)?;
        let request_fingerprint = request_fingerprint(owner_user_id, request)?;
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "begin_evaluation_observation",
                    source,
                })?;

        if let Some(existing) =
            load_observation_by_idempotency_tx(&mut tx, owner_user_id, &request.idempotency_key)
                .await?
        {
            ensure_observation_identity(&existing, &request_fingerprint)?;
            tx.commit()
                .await
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "commit_existing_evaluation_observation",
                    source,
                })?;
            return Ok(existing);
        }

        let (experiment, binding) = DatabaseEvaluationPlanStore::lock_bound_trial_for_receipt(
            &mut tx,
            owner_user_id,
            &request.observation.trial_id,
            &request.session_id,
        )
        .await
        .map_err(EvaluationExecutionError::Persistence)?;
        validate_binding_for_observation(&experiment.spec, &binding, request)?;
        validate_canonical_run_and_receipts(&mut tx, owner_user_id, &binding, request).await?;

        let observation_id = Uuid::now_v7().to_string();
        let observation_json = serde_json::to_string(&request.observation).map_err(|source| {
            EvaluationExecutionError::Json {
                operation: "serialize_evaluation_observation",
                source,
            }
        })?;
        if observation_json.len() > MAX_OBSERVATION_BYTES {
            return Err(EvaluationExecutionError::InvalidInput(format!(
                "observation is {} bytes; limit is {MAX_OBSERVATION_BYTES}",
                observation_json.len()
            )));
        }
        let receipt_json =
            serde_json::to_string(&request.materialization_receipt_ids).map_err(|source| {
                EvaluationExecutionError::Json {
                    operation: "serialize_evaluation_observation_receipts",
                    source,
                }
            })?;
        let result = sqlx::query(
            "INSERT INTO evaluation_trial_observations
             (schema_version, owner_user_id, observation_id, experiment_id, trial_id,
              session_id, execution_run_id, execution_run_generation, spec_fingerprint,
              observation_json, materialization_receipt_ids_json, request_fingerprint,
              idempotency_key, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6))",
        )
        .bind(i64::from(EVALUATION_EXECUTION_SCHEMA_VERSION))
        .bind(owner_user_id)
        .bind(&observation_id)
        .bind(&experiment.experiment_id)
        .bind(&binding.trial_id)
        .bind(&request.session_id)
        .bind(&request.execution_run_id)
        .bind(
            i64::try_from(request.execution_run_generation).map_err(|_| {
                EvaluationExecutionError::InvalidInput(
                    "execution run generation exceeds BIGINT".to_string(),
                )
            })?,
        )
        .bind(&experiment.spec_fingerprint)
        .bind(observation_json)
        .bind(receipt_json)
        .bind(&request_fingerprint)
        .bind(&request.idempotency_key)
        .execute(&mut *tx)
        .await;
        if let Err(source) = result {
            let duplicate = is_duplicate_key(&source);
            let _ = tx.rollback().await;
            if duplicate {
                if let Some(existing) = self
                    .load_by_trial(owner_user_id, &request.observation.trial_id)
                    .await?
                {
                    ensure_observation_identity(&existing, &request_fingerprint)?;
                    return Ok(existing);
                }
                if let Some(existing) = self
                    .load_by_idempotency(owner_user_id, &request.idempotency_key)
                    .await?
                {
                    ensure_observation_identity(&existing, &request_fingerprint)?;
                    return Ok(existing);
                }
            }
            return Err(EvaluationExecutionError::Database {
                operation: "insert_evaluation_observation",
                source,
            });
        }

        let record = load_observation_by_id_tx(&mut tx, owner_user_id, &observation_id)
            .await?
            .ok_or_else(|| {
                EvaluationExecutionError::Conflict(
                    "inserted evaluation observation could not be reloaded".to_string(),
                )
            })?;
        tx.commit()
            .await
            .map_err(|source| EvaluationExecutionError::Database {
                operation: "commit_evaluation_observation",
                source,
            })?;
        Ok(record)
    }

    pub async fn list_observations(
        &self,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<Vec<EvaluationObservationRecord>, EvaluationExecutionError> {
        let mut tx =
            self.pool
                .get()
                .begin()
                .await
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "begin_list_evaluation_observations",
                    source,
                })?;
        let observations = list_observations_tx(&mut tx, owner_user_id, experiment_id).await?;
        tx.commit()
            .await
            .map_err(|source| EvaluationExecutionError::Database {
                operation: "commit_list_evaluation_observations",
                source,
            })?;
        Ok(observations)
    }

    pub(crate) async fn list_observations_in_transaction(
        tx: &mut Transaction<'_, MySql>,
        owner_user_id: &str,
        experiment_id: &str,
    ) -> Result<Vec<EvaluationObservationRecord>, EvaluationExecutionError> {
        list_observations_tx(tx, owner_user_id, experiment_id).await
    }

    pub async fn load_by_trial(
        &self,
        owner_user_id: &str,
        trial_id: &str,
    ) -> Result<Option<EvaluationObservationRecord>, EvaluationExecutionError> {
        validate_id("owner_user_id", owner_user_id, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        validate_id("trial_id", trial_id, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        let row = sqlx::query(
            "SELECT schema_version, owner_user_id, observation_id, experiment_id, trial_id,
                    session_id, execution_run_id, execution_run_generation, spec_fingerprint,
                    observation_json, materialization_receipt_ids_json, request_fingerprint,
                    idempotency_key, CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM evaluation_trial_observations
             WHERE owner_user_id = ? AND trial_id = ?
             LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(trial_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| EvaluationExecutionError::Database {
            operation: "load_evaluation_observation_by_trial",
            source,
        })?;
        row.map(decode_observation).transpose()
    }

    /// Load only the indexed evaluation admission and settlement markers for a
    /// Run.  This is deliberately bounded and never hydrates the full event
    /// history, so ordinary terminal status polling stays on the lightweight
    /// status path.  A `run_started` admission is retained for explicit
    /// pre-spawn failures; successful runs must also carry the enriched
    /// `evaluation_admitted` generation marker.
    pub async fn load_admission_marker_for_run(
        &self,
        owner_user_id: &str,
        run_id: &str,
        expected_run_generation: u64,
    ) -> Result<Option<EvaluationAdmissionMarker>, EvaluationExecutionError> {
        validate_id("owner_user_id", owner_user_id, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        validate_id("run_id", run_id, MAX_RUN_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        let rows = sqlx::query(
            "SELECT event_type, idempotency_key, payload_json
             FROM agent_run_events FORCE INDEX (idx_agent_run_events_control_type_idx)
             WHERE user_id = ? AND run_id = ?
               AND event_type IN ('evaluation_admitted', 'run_started',
                                  'run_settlement_finished')
             ORDER BY event_idx DESC
             LIMIT 128",
        )
        .bind(owner_user_id)
        .bind(run_id)
        .fetch_all(self.pool.get())
        .await
        .map_err(|source| EvaluationExecutionError::Database {
            operation: "load_evaluation_admission_marker_for_run",
            source,
        })?;

        let mut settlement_generations = HashSet::new();
        let mut enriched = None;
        let mut fallback = None;
        for row in rows {
            let event_type: String =
                row.try_get("event_type")
                    .map_err(|source| EvaluationExecutionError::Database {
                        operation: "decode_evaluation_marker_event_type",
                        source,
                    })?;
            match event_type.as_str() {
                "run_settlement_finished" => {
                    let key = row
                        .try_get::<Option<String>, _>("idempotency_key")
                        .map_err(|source| EvaluationExecutionError::Database {
                            operation: "decode_evaluation_settlement_marker_key",
                            source,
                        })?;
                    if let Some(generation) = key
                        .as_deref()
                        .and_then(|key| key.strip_prefix("run-settlement-finished:"))
                        .and_then(|generation| generation.parse::<u64>().ok())
                    {
                        settlement_generations.insert(generation);
                    }
                }
                "evaluation_admitted" | "run_started" => {
                    let payload: String = row.try_get("payload_json").map_err(|source| {
                        EvaluationExecutionError::Database {
                            operation: "decode_evaluation_admission_marker_payload",
                            source,
                        }
                    })?;
                    let event: Value = serde_json::from_str(&payload).map_err(|source| {
                        EvaluationExecutionError::Json {
                            operation: "decode_evaluation_admission_marker_payload",
                            source,
                        }
                    })?;
                    let (generation, admission_value) = if event_type == "evaluation_admitted" {
                        (
                            Some(
                                event
                                    .get("run_generation")
                                    .and_then(Value::as_u64)
                                    .ok_or_else(|| {
                                        EvaluationExecutionError::Conflict(
                                            "evaluation admission marker has no execution generation"
                                                .to_string(),
                                        )
                                    })?,
                            ),
                            event.pointer("/data/admission"),
                        )
                    } else {
                        (None, event.pointer("/data/evaluation_admission"))
                    };
                    let Some(admission_value) = admission_value else {
                        continue;
                    };
                    let admission =
                        serde_json::from_value::<EvaluationRunAdmission>(admission_value.clone())
                            .map_err(|source| EvaluationExecutionError::Json {
                            operation: "deserialize_evaluation_admission_marker",
                            source,
                        })?;
                    admission
                        .validate_shape()
                        .map_err(EvaluationExecutionError::Conflict)?;
                    if event_type == "evaluation_admitted" {
                        if enriched.is_none() {
                            let generation = generation.ok_or_else(|| {
                                EvaluationExecutionError::Conflict(
                                    "evaluation admission marker has no execution generation"
                                        .to_string(),
                                )
                            })?;
                            enriched = Some((generation, admission));
                        }
                    } else if fallback.is_none() {
                        let generation = event
                            .pointer("/data/owner_generation")
                            .and_then(Value::as_u64)
                            .ok_or_else(|| {
                                EvaluationExecutionError::Conflict(
                                    "run_started evaluation admission has no owner generation"
                                        .to_string(),
                                )
                            })?;
                        fallback = Some((generation, admission));
                    }
                }
                _ => {}
            }
        }

        let (execution_run_generation, admission) = if let Some((generation, admission)) = enriched
        {
            if generation != expected_run_generation {
                return Ok(None);
            }
            (generation, admission)
        } else if let Some((generation, admission)) = fallback {
            if generation != expected_run_generation {
                return Ok(None);
            }
            (generation, admission)
        } else {
            return Ok(None);
        };
        Ok(Some(EvaluationAdmissionMarker {
            admission,
            execution_run_generation,
            settlement_finished: settlement_generations.contains(&expected_run_generation),
        }))
    }

    pub async fn load_by_idempotency(
        &self,
        owner_user_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<EvaluationObservationRecord>, EvaluationExecutionError> {
        validate_id("owner_user_id", owner_user_id, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        validate_id("idempotency_key", idempotency_key, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        let row = sqlx::query(
            "SELECT schema_version, owner_user_id, observation_id, experiment_id, trial_id,
                    session_id, execution_run_id, execution_run_generation, spec_fingerprint,
                    observation_json, materialization_receipt_ids_json, request_fingerprint,
                    idempotency_key, CAST(created_at AS CHAR) AS created_at,
                    CAST(updated_at AS CHAR) AS updated_at
             FROM evaluation_trial_observations
             WHERE owner_user_id = ? AND idempotency_key = ?
             LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(idempotency_key)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| EvaluationExecutionError::Database {
            operation: "load_evaluation_observation_by_idempotency",
            source,
        })?;
        row.map(decode_observation).transpose()
    }
}

async fn list_observations_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    experiment_id: &str,
) -> Result<Vec<EvaluationObservationRecord>, EvaluationExecutionError> {
    validate_id("owner_user_id", owner_user_id, MAX_ID_BYTES)
        .map_err(EvaluationExecutionError::InvalidInput)?;
    validate_id("experiment_id", experiment_id, MAX_ID_BYTES)
        .map_err(EvaluationExecutionError::InvalidInput)?;
    let rows = sqlx::query(
        "SELECT schema_version, owner_user_id, observation_id, experiment_id, trial_id,
                session_id, execution_run_id, execution_run_generation, spec_fingerprint,
                observation_json, materialization_receipt_ids_json, request_fingerprint,
                idempotency_key, CAST(created_at AS CHAR) AS created_at,
                CAST(updated_at AS CHAR) AS updated_at
         FROM evaluation_trial_observations
         WHERE owner_user_id = ? AND experiment_id = ?
         ORDER BY created_at ASC, observation_id ASC
         LIMIT 4096",
    )
    .bind(owner_user_id)
    .bind(experiment_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(|source| EvaluationExecutionError::Database {
        operation: "list_evaluation_observations",
        source,
    })?;
    rows.into_iter().map(decode_observation).collect()
}

fn validate_binding_for_observation(
    spec: &ExperimentSpec,
    binding: &EvaluationTrialBindingRecord,
    request: &EvaluationObservationRequest,
) -> Result<(), EvaluationExecutionError> {
    if binding.experiment_id != spec.experiment_id
        || binding.spec_fingerprint
            != spec.spec_fingerprint().map_err(|error| {
                EvaluationExecutionError::InvalidInput(format!("invalid frozen spec: {error}"))
            })?
    {
        return Err(EvaluationExecutionError::Conflict(
            "trial binding does not match its frozen experiment".to_string(),
        ));
    }
    if binding.session_id.as_deref() != Some(request.session_id.as_str())
        || binding.run_id.as_deref() != Some(request.execution_run_id.as_str())
        || binding.run_generation != Some(request.execution_run_generation)
    {
        return Err(EvaluationExecutionError::Conflict(
            "observation execution identity does not match the current trial binding".to_string(),
        ));
    }
    let trial = &binding.trial;
    let observation = &request.observation;
    if observation.experiment_fingerprint != binding.spec_fingerprint
        || observation.trial_id != trial.trial_id
        || observation.case_id != trial.case_id
        || observation.arm != trial.arm
        || observation.repetition != trial.repetition
    {
        return Err(EvaluationExecutionError::Conflict(
            "observation does not match the planned trial identity".to_string(),
        ));
    }
    if matches!(observation.status, TrialStatus::Completed)
        && request.materialization_receipt_ids.is_empty()
    {
        return Err(EvaluationExecutionError::InvalidInput(
            "a completed evaluation trial must retain materialization receipt ids".to_string(),
        ));
    }
    Ok(())
}

async fn validate_canonical_run_and_receipts(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    binding: &EvaluationTrialBindingRecord,
    request: &EvaluationObservationRequest,
) -> Result<(), EvaluationExecutionError> {
    let status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM agent_runs
         WHERE user_id = ? AND run_id = ? AND session_id = ? AND run_generation = ?
         LIMIT 1 FOR UPDATE",
    )
    .bind(owner_user_id)
    .bind(&request.execution_run_id)
    .bind(&request.session_id)
    .bind(
        i64::try_from(request.execution_run_generation).map_err(|_| {
            EvaluationExecutionError::InvalidInput(
                "execution run generation exceeds BIGINT".to_string(),
            )
        })?,
    )
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| EvaluationExecutionError::Database {
        operation: "validate_evaluation_observation_run_status",
        source,
    })?
    .ok_or_else(|| {
        EvaluationExecutionError::Conflict(
            "observation run is not the current owner/session/generation".to_string(),
        )
    })?;
    let expected_status = match status.as_str() {
        "completed" => TrialStatus::Completed,
        "failed" => TrialStatus::Failed,
        "cancelled" => TrialStatus::Cancelled,
        "delegated" => TrialStatus::Unavailable,
        other => {
            return Err(EvaluationExecutionError::Conflict(format!(
                "observation run is not terminal (status {other})"
            )));
        }
    };
    if request.observation.status != expected_status {
        return Err(EvaluationExecutionError::Conflict(format!(
            "observation status {:?} does not match canonical run status {status}",
            request.observation.status
        )));
    }
    let admitted_event = sqlx::query_scalar::<_, String>(
        "SELECT payload_json FROM agent_run_events
         WHERE user_id = ? AND run_id = ? AND session_id = ?
           AND event_type = 'evaluation_admitted'
         ORDER BY event_idx DESC LIMIT 1 FOR UPDATE",
    )
    .bind(owner_user_id)
    .bind(&request.execution_run_id)
    .bind(&request.session_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| EvaluationExecutionError::Database {
        operation: "load_evaluation_admission_marker",
        source,
    })?;
    let admitted = admitted_event
        .map(|payload| {
            let event: Value = serde_json::from_str(&payload).map_err(|source| {
                EvaluationExecutionError::Json {
                    operation: "decode_evaluation_admission_marker",
                    source,
                }
            })?;
            let marker_generation = event
                .get("run_generation")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    EvaluationExecutionError::Conflict(
                        "evaluation admission marker has no execution generation".to_string(),
                    )
                })?;
            if marker_generation != request.execution_run_generation {
                return Err(EvaluationExecutionError::Conflict(
                    "evaluation admission marker belongs to another execution generation"
                        .to_string(),
                ));
            }
            let value = event.pointer("/data/admission").ok_or_else(|| {
                EvaluationExecutionError::Conflict(
                    "evaluation admission marker has no admission payload".to_string(),
                )
            })?;
            let admission = serde_json::from_value::<EvaluationRunAdmission>(value.clone())
                .map_err(|source| EvaluationExecutionError::Json {
                    operation: "deserialize_evaluation_admission_marker",
                    source,
                })?;
            admission
                .validate_shape()
                .map_err(EvaluationExecutionError::Conflict)?;
            Ok::<_, EvaluationExecutionError>(admission)
        })
        .transpose()?;
    if expected_status == TrialStatus::Completed && admitted.is_none() {
        return Err(EvaluationExecutionError::Conflict(
            "completed evaluation run has no durable admission marker".to_string(),
        ));
    }
    if let Some(admitted) = admitted.as_ref() {
        if admitted.experiment_id != binding.experiment_id || admitted.trial_id != binding.trial_id
        {
            return Err(EvaluationExecutionError::Conflict(
                "durable admission marker does not match the bound trial".to_string(),
            ));
        }
        let mut expected_receipts = admitted.receipt_ids.clone();
        expected_receipts.sort_unstable();
        let mut actual_receipts = request.materialization_receipt_ids.clone();
        actual_receipts.sort_unstable();
        if expected_receipts != actual_receipts {
            return Err(EvaluationExecutionError::Conflict(
                "observation receipt set does not match the durable admission".to_string(),
            ));
        }
    }
    if request.materialization_receipt_ids.is_empty() {
        return Ok(());
    }
    let expected_generation = i64::try_from(request.execution_run_generation).map_err(|_| {
        EvaluationExecutionError::InvalidInput(
            "execution run generation exceeds BIGINT".to_string(),
        )
    })?;
    for receipt_id in &request.materialization_receipt_ids {
        let row = sqlx::query(
            "SELECT trial_id, session_id, execution_run_id,
                    execution_run_generation, spec_fingerprint,
                    envelope_id, envelope_fingerprint, outcome
             FROM evaluation_materialization_receipts
             WHERE owner_user_id = ? AND receipt_id = ?
             LIMIT 1 FOR UPDATE",
        )
        .bind(owner_user_id)
        .bind(receipt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|source| EvaluationExecutionError::Database {
            operation: "validate_evaluation_observation_receipt",
            source,
        })?
        .ok_or_else(|| {
            EvaluationExecutionError::Conflict(format!(
                "materialization receipt {receipt_id} is missing or belongs to another owner"
            ))
        })?;
        let trial_id: String =
            row.try_get("trial_id")
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "decode_evaluation_observation_receipt_trial",
                    source,
                })?;
        let session_id: String =
            row.try_get("session_id")
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "decode_evaluation_observation_receipt_session",
                    source,
                })?;
        let execution_run_id: Option<String> =
            row.try_get("execution_run_id").map_err(|source| {
                EvaluationExecutionError::Database {
                    operation: "decode_evaluation_observation_receipt_run",
                    source,
                }
            })?;
        let execution_run_generation: Option<i64> = row
            .try_get("execution_run_generation")
            .map_err(|source| EvaluationExecutionError::Database {
                operation: "decode_evaluation_observation_receipt_generation",
                source,
            })?;
        let outcome: String =
            row.try_get("outcome")
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "decode_evaluation_observation_receipt_outcome",
                    source,
                })?;
        let spec_fingerprint: String = row.try_get("spec_fingerprint").map_err(|source| {
            EvaluationExecutionError::Database {
                operation: "decode_evaluation_observation_receipt_spec",
                source,
            }
        })?;
        let envelope_id: String =
            row.try_get("envelope_id")
                .map_err(|source| EvaluationExecutionError::Database {
                    operation: "decode_evaluation_observation_receipt_envelope_id",
                    source,
                })?;
        let envelope_fingerprint: String =
            row.try_get("envelope_fingerprint").map_err(|source| {
                EvaluationExecutionError::Database {
                    operation: "decode_evaluation_observation_receipt_envelope_fingerprint",
                    source,
                }
            })?;
        let expected_envelope = admitted
            .as_ref()
            .and_then(|admission| admission.snapshot_envelope.as_ref());
        if trial_id != binding.trial_id
            || session_id != request.session_id
            || execution_run_id.as_deref() != Some(request.execution_run_id.as_str())
            || execution_run_generation != Some(expected_generation)
            || outcome != "available"
            || spec_fingerprint != binding.spec_fingerprint
            || expected_envelope.is_some_and(|envelope| {
                envelope_id != envelope.snapshot_id
                    || envelope_fingerprint != envelope.snapshot_fingerprint
            })
        {
            return Err(EvaluationExecutionError::Conflict(format!(
                "materialization receipt {receipt_id} does not belong to this admitted run"
            )));
        }
    }
    Ok(())
}

fn validate_request_shape(
    request: &EvaluationObservationRequest,
) -> Result<(), EvaluationExecutionError> {
    validate_id("session_id", &request.session_id, MAX_SESSION_ID_BYTES)
        .map_err(EvaluationExecutionError::InvalidInput)?;
    validate_id(
        "execution_run_id",
        &request.execution_run_id,
        MAX_RUN_ID_BYTES,
    )
    .map_err(EvaluationExecutionError::InvalidInput)?;
    validate_id("idempotency_key", &request.idempotency_key, MAX_ID_BYTES)
        .map_err(EvaluationExecutionError::InvalidInput)?;
    validate_receipt_ids(&request.materialization_receipt_ids)?;
    if request.observation.trial_id.trim().is_empty()
        || request.observation.case_id.trim().is_empty()
    {
        return Err(EvaluationExecutionError::InvalidInput(
            "observation trial_id and case_id must not be empty".to_string(),
        ));
    }
    if request.observation.measurements.len() > MAX_MEASUREMENTS {
        return Err(EvaluationExecutionError::InvalidInput(format!(
            "too many measurements (max {MAX_MEASUREMENTS})"
        )));
    }
    if request.observation.evidence.len() > MAX_EVIDENCE {
        return Err(EvaluationExecutionError::InvalidInput(format!(
            "too many evidence references (max {MAX_EVIDENCE})"
        )));
    }
    let mut measurement_names = HashSet::new();
    for measurement in &request.observation.measurements {
        validate_text("measurement.name", &measurement.name, MAX_TEXT_BYTES)?;
        validate_text("measurement.unit", &measurement.unit, MAX_TEXT_BYTES)?;
        if !measurement_names.insert(&measurement.name) {
            return Err(EvaluationExecutionError::InvalidInput(
                "duplicate measurement name".to_string(),
            ));
        }
        if measurement.value.is_some_and(|value| !value.is_finite()) {
            return Err(EvaluationExecutionError::InvalidInput(
                "measurement values must be finite when present".to_string(),
            ));
        }
    }
    let mut evidence_ids = HashSet::new();
    for evidence in &request.observation.evidence {
        validate_text(
            "evidence.evidence_id",
            &evidence.evidence_id,
            MAX_TEXT_BYTES,
        )?;
        if !evidence_ids.insert(&evidence.evidence_id) {
            return Err(EvaluationExecutionError::InvalidInput(
                "duplicate evidence id".to_string(),
            ));
        }
        if evidence.availability == EvidenceAvailability::Available
            && evidence.locator.as_deref().is_none_or(str::is_empty)
        {
            return Err(EvaluationExecutionError::InvalidInput(
                "available evidence must have a locator".to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_receipt_ids(receipt_ids: &[String]) -> Result<(), EvaluationExecutionError> {
    if receipt_ids.len() > MAX_RECEIPTS {
        return Err(EvaluationExecutionError::InvalidInput(format!(
            "too many materialization receipts (max {MAX_RECEIPTS})"
        )));
    }
    let mut seen = HashSet::new();
    for receipt_id in receipt_ids {
        validate_id("receipt_id", receipt_id, MAX_ID_BYTES)
            .map_err(EvaluationExecutionError::InvalidInput)?;
        if !seen.insert(receipt_id) {
            return Err(EvaluationExecutionError::InvalidInput(
                "duplicate materialization receipt id".to_string(),
            ));
        }
    }
    Ok(())
}

fn request_fingerprint(
    owner_user_id: &str,
    request: &EvaluationObservationRequest,
) -> Result<String, EvaluationExecutionError> {
    let payload = serde_json::json!({
        "schema_version": EVALUATION_EXECUTION_SCHEMA_VERSION,
        "owner_user_id": owner_user_id,
        "session_id": request.session_id,
        "execution_run_id": request.execution_run_id,
        "execution_run_generation": request.execution_run_generation,
        "observation": request.observation,
        "materialization_receipt_ids": request.materialization_receipt_ids,
        "idempotency_key": request.idempotency_key,
    });
    let canonical = canonical_json_string(&payload);
    Ok(format!("sha256:{:x}", Sha256::digest(canonical.as_bytes())))
}

fn ensure_observation_identity(
    existing: &EvaluationObservationRecord,
    request_fingerprint: &str,
) -> Result<(), EvaluationExecutionError> {
    if existing.request_fingerprint == request_fingerprint {
        Ok(())
    } else {
        Err(EvaluationExecutionError::Conflict(format!(
            "idempotency key already belongs to observation {} with different content",
            existing.observation_id
        )))
    }
}

async fn load_observation_by_idempotency_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<EvaluationObservationRecord>, EvaluationExecutionError> {
    let row = sqlx::query(&observation_select_sql(
        "owner_user_id = ? AND idempotency_key = ?",
    ))
    .bind(owner_user_id)
    .bind(idempotency_key)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| EvaluationExecutionError::Database {
        operation: "load_existing_evaluation_observation",
        source,
    })?;
    row.map(decode_observation).transpose()
}

async fn load_observation_by_id_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    observation_id: &str,
) -> Result<Option<EvaluationObservationRecord>, EvaluationExecutionError> {
    let row = sqlx::query(&observation_select_sql(
        "owner_user_id = ? AND observation_id = ?",
    ))
    .bind(owner_user_id)
    .bind(observation_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| EvaluationExecutionError::Database {
        operation: "reload_evaluation_observation",
        source,
    })?;
    row.map(decode_observation).transpose()
}

fn observation_select_sql(predicate: &str) -> String {
    format!(
        "SELECT schema_version, owner_user_id, observation_id, experiment_id, trial_id,
                session_id, execution_run_id, execution_run_generation, spec_fingerprint,
                observation_json, materialization_receipt_ids_json, request_fingerprint,
                idempotency_key, CAST(created_at AS CHAR) AS created_at,
                CAST(updated_at AS CHAR) AS updated_at
         FROM evaluation_trial_observations WHERE {predicate} LIMIT 1"
    )
}

fn decode_observation(
    row: sqlx::mysql::MySqlRow,
) -> Result<EvaluationObservationRecord, EvaluationExecutionError> {
    let schema_version: i64 =
        row.try_get("schema_version")
            .map_err(|source| EvaluationExecutionError::Database {
                operation: "decode_evaluation_observation",
                source,
            })?;
    if u32::try_from(schema_version).ok() != Some(EVALUATION_EXECUTION_SCHEMA_VERSION) {
        return Err(EvaluationExecutionError::Conflict(
            "unsupported evaluation observation schema version".to_string(),
        ));
    }
    let observation_json: String =
        row.try_get("observation_json")
            .map_err(|source| EvaluationExecutionError::Database {
                operation: "decode_evaluation_observation",
                source,
            })?;
    let observation: TrialObservation =
        serde_json::from_str(&observation_json).map_err(|source| {
            EvaluationExecutionError::Json {
                operation: "deserialize_evaluation_observation",
                source,
            }
        })?;
    let receipt_json: String =
        row.try_get("materialization_receipt_ids_json")
            .map_err(|source| EvaluationExecutionError::Database {
                operation: "decode_evaluation_observation_receipts",
                source,
            })?;
    let materialization_receipt_ids: Vec<String> =
        serde_json::from_str(&receipt_json).map_err(|source| EvaluationExecutionError::Json {
            operation: "deserialize_evaluation_observation_receipts",
            source,
        })?;
    let generation: i64 = row.try_get("execution_run_generation").map_err(|source| {
        EvaluationExecutionError::Database {
            operation: "decode_evaluation_observation",
            source,
        }
    })?;
    let owner_user_id = row_string(&row, "owner_user_id")?;
    let session_id = row_string(&row, "session_id")?;
    let execution_run_id = row_string(&row, "execution_run_id")?;
    let experiment_id = row_string(&row, "experiment_id")?;
    let trial_id = row_string(&row, "trial_id")?;
    let spec_fingerprint = row_string(&row, "spec_fingerprint")?;
    let stored_request_fingerprint = row_string(&row, "request_fingerprint")?;
    let idempotency_key = row_string(&row, "idempotency_key")?;
    let reconstructed_request = EvaluationObservationRequest {
        session_id: session_id.clone(),
        execution_run_id: execution_run_id.clone(),
        execution_run_generation: u64::try_from(generation).map_err(|_| {
            EvaluationExecutionError::Conflict(
                "evaluation observation has an invalid run generation".to_string(),
            )
        })?,
        observation: observation.clone(),
        materialization_receipt_ids: materialization_receipt_ids.clone(),
        idempotency_key: idempotency_key.clone(),
    };
    validate_request_shape(&reconstructed_request)?;
    let expected_request_fingerprint = request_fingerprint(&owner_user_id, &reconstructed_request)?;
    if expected_request_fingerprint != stored_request_fingerprint {
        return Err(EvaluationExecutionError::Conflict(
            "evaluation observation request fingerprint does not match its content".to_string(),
        ));
    }
    if observation.trial_id != trial_id || observation.experiment_fingerprint != spec_fingerprint {
        return Err(EvaluationExecutionError::Conflict(
            "evaluation observation outer identity does not match its stored content".to_string(),
        ));
    }
    Ok(EvaluationObservationRecord {
        schema_version: EVALUATION_EXECUTION_SCHEMA_VERSION,
        owner_user_id,
        observation_id: row_string(&row, "observation_id")?,
        experiment_id,
        trial_id,
        session_id,
        execution_run_id,
        execution_run_generation: u64::try_from(generation).map_err(|_| {
            EvaluationExecutionError::Conflict(
                "evaluation observation has an invalid run generation".to_string(),
            )
        })?,
        observation,
        materialization_receipt_ids,
        request_fingerprint: stored_request_fingerprint,
        idempotency_key,
        created_at: row_string(&row, "created_at")?,
        updated_at: row_string(&row, "updated_at")?,
    })
}

fn row_string(
    row: &sqlx::mysql::MySqlRow,
    column: &str,
) -> Result<String, EvaluationExecutionError> {
    row.try_get(column)
        .map_err(|source| EvaluationExecutionError::Database {
            operation: "decode_evaluation_observation_string",
            source,
        })
}

fn validate_id(label: &str, value: &str, max_bytes: usize) -> Result<(), String> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(format!("{label} must be 1..={max_bytes} bytes"));
    }
    if !value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':' | b'/')
    }) {
        return Err(format!("{label} contains an unsupported character"));
    }
    Ok(())
}

fn validate_text(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), EvaluationExecutionError> {
    if value.is_empty() || value.len() > max_bytes {
        return Err(EvaluationExecutionError::InvalidInput(format!(
            "{label} must be 1..={max_bytes} bytes"
        )));
    }
    Ok(())
}

fn validate_hash(label: &str, value: &str) -> Result<(), String> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!("{label} must be a sha256:<64 hex> identity"));
    }
    Ok(())
}

/// Hash the fixed task input without including the candidate revision.  This
/// is the prompt-only adapter's Context identity; the revision is carried in
/// the stable system prompt and is compared separately.
pub fn prompt_context_fingerprint(
    message: &str,
    parts: &[Value],
    attachments: &[Value],
    context: Option<&serde_json::Map<String, Value>>,
) -> String {
    let payload = serde_json::json!({
        "message": message,
        "parts": parts,
        "attachments": attachments,
        "context": context,
    });
    let canonical = canonical_json_string(&payload);
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

/// Hash policy facts already resolved by the normal runtime admission.  The
/// caller supplies a redacted JSON value; secrets and credentials must never
/// be included in it.
pub fn prompt_policy_fingerprint(policy_facts: &Value) -> String {
    let canonical = canonical_json_string(policy_facts);
    format!("sha256:{:x}", Sha256::digest(canonical.as_bytes()))
}

pub fn content_fingerprint(content: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(content.as_bytes()))
}

pub fn evaluation_component_idempotency_key(
    trial_id: &str,
    run_id: &str,
    generation: u64,
    component: &str,
) -> String {
    let payload = format!("{trial_id}\0{run_id}\0{generation}\0{component}");
    format!(
        "eval-receipt:sha256:{:x}",
        Sha256::digest(payload.as_bytes())
    )
}

/// Construct the identity envelope for a prompt-only trial.  There are no
/// mutable Memory/Data/Workspace dimensions in this first adapter, so the
/// composite contains only the canonical session identity and is not treated
/// as a branch proof.
#[allow(clippy::too_many_arguments)]
pub fn prompt_only_snapshot_envelope(
    owner_user_id: &str,
    experiment_id: &str,
    trial_id: &str,
    session_id: &str,
    turn: u32,
    created_at: &str,
    context_snapshot_hash: &str,
    policy_snapshot_hash: &str,
) -> Result<SnapshotEnvelope, String> {
    // This is a logical prompt-only identity, not a wall-clock checkpoint.
    // Deriving it from the frozen inputs makes an admission retry reproduce
    // the exact envelope and receipt request after a process/DB interruption.
    let seed = format!(
        "{owner_user_id}\0{experiment_id}\0{trial_id}\0{session_id}\0{turn}\0{context_snapshot_hash}\0{policy_snapshot_hash}"
    );
    let snapshot_id = format!("eval-snapshot-{:x}", Sha256::digest(seed.as_bytes()));
    let composite = CompositeSnapshot {
        snapshot_id,
        session_id: session_id.to_string(),
        turn,
        created_at: created_at.to_string(),
        version: 0,
        label: Some(format!("evaluation:{experiment_id}:{trial_id}")),
        refs: vec![],
    };
    let mut envelope = SnapshotEnvelope::new(
        owner_user_id,
        experiment_id,
        Some(trial_id.to_string()),
        composite,
        context_snapshot_hash,
        policy_snapshot_hash,
    )?;

    // SnapshotEnvelope addresses are UUIDv7 by contract.  The generic
    // constructor intentionally allocates a fresh address, but an admission
    // retry must reproduce the same request before any receipt is issued.
    // Derive a stable UUID-shaped v7 value from the frozen seed while keeping
    // the UUID variant/version bits valid.
    let timestamp = chrono::DateTime::parse_from_rfc3339(created_at)
        .map_err(|error| format!("snapshot created_at is not RFC3339: {error}"))?
        .timestamp_millis();
    if !(0..(1_i64 << 48)).contains(&timestamp) {
        return Err("snapshot created_at is outside UUIDv7 timestamp range".to_string());
    }
    let digest = Sha256::digest(format!("envelope\0{seed}").as_bytes());
    let mut uuid_bytes = [0_u8; 16];
    let timestamp = timestamp as u64;
    uuid_bytes[0] = (timestamp >> 40) as u8;
    uuid_bytes[1] = (timestamp >> 32) as u8;
    uuid_bytes[2] = (timestamp >> 24) as u8;
    uuid_bytes[3] = (timestamp >> 16) as u8;
    uuid_bytes[4] = (timestamp >> 8) as u8;
    uuid_bytes[5] = timestamp as u8;
    uuid_bytes[6] = (digest[0] & 0x0f) | 0x70;
    uuid_bytes[7] = digest[1];
    uuid_bytes[8] = (digest[2] & 0x3f) | 0x80;
    uuid_bytes[9..].copy_from_slice(&digest[3..10]);
    envelope.snapshot_id = Uuid::from_bytes(uuid_bytes).to_string();
    envelope.validate()?;
    Ok(envelope)
}

/// Build measurements that are facts about the Run itself.  Task quality is
/// intentionally not inferred from terminal status; a verifier must add its
/// own measurement before a report can claim a quality improvement.
#[allow(clippy::too_many_arguments)]
pub fn terminal_run_observation(
    experiment_fingerprint: String,
    trial_id: String,
    case_id: String,
    arm: ComparisonArm,
    repetition: u32,
    status: TrialStatus,
    prompt_tokens: Option<u64>,
    completion_tokens: Option<u64>,
    tool_calls: Option<u32>,
    evidence: Vec<EvidenceRef>,
) -> TrialObservation {
    let measurement = |name: &str, value: Option<f64>, unit: &str| Measurement {
        name: name.to_string(),
        value,
        unit: unit.to_string(),
        status: if value.is_some() {
            MeasurementStatus::Observed
        } else {
            MeasurementStatus::Missing
        },
        basis: Some("canonical_run_settlement".to_string()),
    };
    TrialObservation {
        experiment_fingerprint,
        trial_id,
        case_id,
        arm,
        repetition,
        status,
        measurements: vec![
            measurement(
                "prompt_tokens",
                prompt_tokens.map(|value| value as f64),
                "tokens",
            ),
            measurement(
                "completion_tokens",
                completion_tokens.map(|value| value as f64),
                "tokens",
            ),
            measurement("tool_calls", tool_calls.map(|value| value as f64), "calls"),
        ],
        evidence,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn context_fingerprint_excludes_revision_but_changes_with_input() {
        let first = prompt_context_fingerprint("hello", &[], &[], None);
        let second = prompt_context_fingerprint("hello", &[], &[], None);
        let changed = prompt_context_fingerprint("goodbye", &[], &[], None);
        assert_eq!(first, second);
        assert_ne!(first, changed);
        assert!(first.starts_with("sha256:"));
    }

    #[test]
    fn policy_fingerprint_is_canonical() {
        let first = prompt_policy_fingerprint(&json!({"b": 2, "a": 1}));
        let second = prompt_policy_fingerprint(&json!({"a": 1, "b": 2}));
        assert_eq!(first, second);
    }

    #[test]
    fn prompt_only_envelope_is_stable_across_admission_retries() {
        let first = prompt_only_snapshot_envelope(
            "owner-a",
            "experiment-a",
            "trial-a",
            "session-a",
            0,
            "2026-09-18T00:00:00Z",
            "sha256:context",
            "sha256:policy",
        )
        .expect("stable prompt-only envelope");
        let second = prompt_only_snapshot_envelope(
            "owner-a",
            "experiment-a",
            "trial-a",
            "session-a",
            0,
            "2026-09-18T00:00:00Z",
            "sha256:context",
            "sha256:policy",
        )
        .expect("stable prompt-only envelope retry");
        assert_eq!(first, second);
    }

    #[test]
    fn admission_rejects_non_sha256_identity() {
        let admission = EvaluationRunAdmission {
            experiment_id: "exp-1".to_string(),
            trial_id: "trial-1".to_string(),
            input_content_hash: "not-a-hash".to_string(),
            revision_content_hash:
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_string(),
            skill_revision: None,
            receipt_ids: vec![],
            snapshot_envelope: None,
        };
        assert!(admission.validate_shape().is_err());
    }
}
