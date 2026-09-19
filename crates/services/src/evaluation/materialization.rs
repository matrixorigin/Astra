//! Owner-scoped, append-only evidence for evaluation snapshot materialization.
//!
//! A receipt is a fact emitted by a trusted materializer. It is deliberately
//! narrower than a scheduler: it cannot start a run, choose a branch, or
//! replace the canonical Session/Run lifecycle. Consumers must hold the exact
//! receipt identities they intend to use; this module never selects a latest
//! receipt implicitly.

use super::durable::{
    DatabaseEvaluationPlanStore, EvaluationPersistenceError, EvaluationTrialBindingRecord,
    is_duplicate_key,
};
use super::experiment::{DataIsolation, ExperimentSpec, MemoryIsolation, SnapshotEnvelope};
use astra_core::SharedPool;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use std::collections::HashSet;
use thiserror::Error;
use uuid::Uuid;

pub const MATERIALIZATION_RECEIPT_SCHEMA_VERSION: u32 = 1;
const MAX_OWNER_ID_BYTES: usize = 128;
const MAX_RECEIPT_ID_BYTES: usize = 64;
const MAX_EXPERIMENT_ID_BYTES: usize = 128;
const MAX_TRIAL_ID_BYTES: usize = 128;
const MAX_SESSION_ID_BYTES: usize = 64;
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;
const MAX_COMPONENT_REF_BYTES: usize = 2_048;
const MAX_COMPONENT_FINGERPRINT_BYTES: usize = 128;
const MAX_FAILURE_CODE_BYTES: usize = 128;
const MAX_MATERIALIZER_KIND_BYTES: usize = 128;
const MAX_PROVIDER_BINDING_BYTES: usize = 128;
const MAX_RUNNER_BINDING_BYTES: usize = 128;

#[derive(Debug, Error)]
pub enum MaterializationReceiptError {
    #[error("materialization receipt persistence failed: {0}")]
    Persistence(#[from] EvaluationPersistenceError),
    #[error("invalid materialization receipt: {0}")]
    InvalidInput(String),
    #[error("materialization receipt conflict: {0}")]
    Conflict(String),
    #[error("materialization receipt not found: {0}")]
    NotFound(String),
    #[error("materialization receipt database operation failed: {operation}: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
    #[error("materialization receipt JSON operation failed: {operation}: {source}")]
    Json {
        operation: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationComponentKind {
    Context,
    Policy,
    Memory,
    Data,
    Workspace,
}

impl MaterializationComponentKind {
    pub const ALL: [Self; 5] = [
        Self::Context,
        Self::Policy,
        Self::Memory,
        Self::Data,
        Self::Workspace,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Policy => "policy",
            Self::Memory => "memory",
            Self::Data => "data",
            Self::Workspace => "workspace",
        }
    }

    fn parse(value: &str) -> Result<Self, MaterializationReceiptError> {
        match value {
            "context" => Ok(Self::Context),
            "policy" => Ok(Self::Policy),
            "memory" => Ok(Self::Memory),
            "data" => Ok(Self::Data),
            "workspace" => Ok(Self::Workspace),
            _ => Err(MaterializationReceiptError::Conflict(format!(
                "unknown materialization component kind {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterializationOutcome {
    Available,
    Unavailable,
    Failed,
}

impl MaterializationOutcome {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Available => "available",
            Self::Unavailable => "unavailable",
            Self::Failed => "failed",
        }
    }

    fn parse(value: &str) -> Result<Self, MaterializationReceiptError> {
        match value {
            "available" => Ok(Self::Available),
            "unavailable" => Ok(Self::Unavailable),
            "failed" => Ok(Self::Failed),
            _ => Err(MaterializationReceiptError::Conflict(format!(
                "unknown materialization outcome {value}"
            ))),
        }
    }
}

/// Identity supplied by the trusted server-side materializer adapter. A user
/// request cannot choose these fields through this API.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TrustedMaterializerContext {
    pub owner_user_id: String,
    pub materializer_kind: String,
    pub provider_binding_id: Option<String>,
    pub execution_run_id: Option<String>,
    pub execution_run_generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializationReceiptRequest {
    pub trial_id: String,
    pub session_id: String,
    pub envelope: SnapshotEnvelope,
    pub component_kind: MaterializationComponentKind,
    /// Address of the materialized component. Failed/unavailable outcomes may
    /// omit it because no usable snapshot was produced.
    pub component_snapshot_ref: Option<String>,
    /// For branch-backed Memory/Data components, the immutable base from
    /// which the materializer derived the component.
    pub component_base_snapshot_ref: Option<String>,
    /// Content identity is separate from the component address. It is
    /// optional for address-only components such as a workspace checkout.
    pub component_content_fingerprint: Option<String>,
    pub outcome: MaterializationOutcome,
    pub failure_code: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
    pub idempotency_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaterializationReceiptRecord {
    pub schema_version: u32,
    pub receipt_id: String,
    pub owner_user_id: String,
    pub experiment_id: String,
    pub trial_id: String,
    pub session_id: String,
    pub spec_fingerprint: String,
    pub envelope_id: String,
    pub envelope_fingerprint: String,
    pub component_kind: MaterializationComponentKind,
    pub component_snapshot_ref: Option<String>,
    pub component_base_snapshot_ref: Option<String>,
    pub component_content_fingerprint: Option<String>,
    pub outcome: MaterializationOutcome,
    pub failure_code: Option<String>,
    pub materializer_kind: String,
    pub provider_binding_id: Option<String>,
    pub execution_run_id: Option<String>,
    pub execution_run_generation: Option<u64>,
    pub request_fingerprint: String,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum MaterializationValidationError {
    #[error("materialization receipt set is missing component {0:?}")]
    MissingComponent(MaterializationComponentKind),
    #[error("materialization receipt set contains duplicate component {0:?}")]
    DuplicateComponent(MaterializationComponentKind),
    #[error("materialization receipt {receipt_id} does not match the expected binding: {reason}")]
    BindingMismatch { receipt_id: String, reason: String },
    #[error(
        "materialization receipt {receipt_id} is {outcome:?} for required component {component:?}"
    )]
    ComponentUnavailable {
        receipt_id: String,
        component: MaterializationComponentKind,
        outcome: MaterializationOutcome,
    },
    #[error("materialization receipt {receipt_id} expired at {expires_at}")]
    Expired {
        receipt_id: String,
        expires_at: DateTime<Utc>,
    },
    #[error("materialization receipt {receipt_id} has invalid shape: {reason}")]
    InvalidReceipt { receipt_id: String, reason: String },
}

#[derive(Clone)]
pub struct DatabaseMaterializationReceiptStore {
    pool: SharedPool,
}

/// Derive the exact component set an executor must prove for a frozen spec.
/// Context and Policy are always part of the controlled input; Memory and
/// Data are required only when the spec explicitly enables a per-trial branch.
pub fn required_components_for_spec(spec: &ExperimentSpec) -> Vec<MaterializationComponentKind> {
    let mut required = vec![
        MaterializationComponentKind::Context,
        MaterializationComponentKind::Policy,
    ];
    if matches!(
        spec.conditions.memory_isolation,
        MemoryIsolation::BranchPerTrial { .. }
    ) {
        required.push(MaterializationComponentKind::Memory);
    }
    if matches!(
        spec.conditions.data_isolation,
        DataIsolation::MatrixOneBranchPerTrial { .. }
    ) {
        required.push(MaterializationComponentKind::Data);
    }
    required
}

impl DatabaseMaterializationReceiptStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Record one immutable fact from a trusted materializer. The trial row is
    /// locked for the duration of the insert, so concurrent session rebinding
    /// cannot race a receipt into a different execution identity.
    pub async fn record_receipt(
        &self,
        trusted: &TrustedMaterializerContext,
        request: &MaterializationReceiptRequest,
    ) -> Result<MaterializationReceiptRecord, MaterializationReceiptError> {
        validate_trusted_context(trusted)?;
        validate_request_shape(request)?;
        let request_fingerprint = request_fingerprint(trusted, request)?;
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            MaterializationReceiptError::Database {
                operation: "begin_materialization_receipt",
                source,
            }
        })?;
        // A retry of an already committed immutable fact must remain
        // idempotent even after the canonical Run has advanced to a newer
        // generation. It returns the old fact; it never grants permission to
        // create another receipt under the stale execution identity.
        if let Some(existing) = load_receipt_by_idempotency_tx(
            &mut tx,
            &trusted.owner_user_id,
            &request.idempotency_key,
        )
        .await?
        {
            ensure_idempotent_receipt(&existing, &request_fingerprint)?;
            tx.commit()
                .await
                .map_err(|source| MaterializationReceiptError::Database {
                    operation: "commit_existing_materialization_receipt",
                    source,
                })?;
            return Ok(existing);
        }
        let (experiment, binding) = DatabaseEvaluationPlanStore::lock_bound_trial_for_receipt(
            &mut tx,
            &trusted.owner_user_id,
            &request.trial_id,
            &request.session_id,
        )
        .await?;
        request
            .envelope
            .validate_for(
                &trusted.owner_user_id,
                &experiment.experiment_id,
                Some(&binding.trial_id),
                Some(&request.session_id),
            )
            .map_err(MaterializationReceiptError::InvalidInput)?;
        validate_envelope_against_spec(&experiment.spec, &request.envelope)?;
        validate_trusted_binding(trusted, &experiment.spec, &binding)?;
        let required_components = required_components_for_spec(&experiment.spec);
        if !required_components.contains(&request.component_kind) {
            return Err(MaterializationReceiptError::Conflict(format!(
                "component {} is disabled or undeclared by the frozen isolation profile",
                request.component_kind.as_str()
            )));
        }
        validate_component_against_envelope(
            request.component_kind,
            request.component_snapshot_ref.as_deref(),
            request.component_base_snapshot_ref.as_deref(),
            request.component_content_fingerprint.as_deref(),
            request.outcome,
            &request.envelope,
            &experiment.spec,
        )?;

        if let Some(existing) = load_receipt_by_idempotency_tx(
            &mut tx,
            &trusted.owner_user_id,
            &request.idempotency_key,
        )
        .await?
        {
            ensure_idempotent_receipt(&existing, &request_fingerprint)?;
            tx.commit()
                .await
                .map_err(|source| MaterializationReceiptError::Database {
                    operation: "commit_existing_materialization_receipt",
                    source,
                })?;
            return Ok(existing);
        }
        validate_new_expiry(request.expires_at)?;

        let receipt_id = Uuid::now_v7().to_string();
        let insert_result = sqlx::query(
            "INSERT INTO evaluation_materialization_receipts
             (owner_user_id, receipt_id, experiment_id, trial_id, session_id,
              spec_fingerprint, envelope_id, envelope_fingerprint,
              component_kind, component_snapshot_ref, component_base_snapshot_ref,
              component_content_fingerprint,
              outcome, failure_code, materializer_kind, provider_binding_id,
              execution_run_id, execution_run_generation, request_fingerprint,
              idempotency_key, created_at, expires_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), ?)",
        )
        .bind(&trusted.owner_user_id)
        .bind(&receipt_id)
        .bind(&experiment.experiment_id)
        .bind(&binding.trial_id)
        .bind(&request.session_id)
        .bind(&experiment.spec_fingerprint)
        .bind(&request.envelope.snapshot_id)
        .bind(&request.envelope.snapshot_fingerprint)
        .bind(request.component_kind.as_str())
        .bind(&request.component_snapshot_ref)
        .bind(&request.component_base_snapshot_ref)
        .bind(&request.component_content_fingerprint)
        .bind(request.outcome.as_str())
        .bind(&request.failure_code)
        .bind(&trusted.materializer_kind)
        .bind(&trusted.provider_binding_id)
        .bind(&trusted.execution_run_id)
        .bind(trusted.execution_run_generation.map(|generation| {
            i64::try_from(generation).expect("validated runner generation fits in BIGINT")
        }))
        .bind(&request_fingerprint)
        .bind(&request.idempotency_key)
        .bind(request.expires_at)
        .execute(&mut *tx)
        .await;
        if let Err(source) = insert_result {
            let duplicate = is_duplicate_key(&source);
            let _ = tx.rollback().await;
            if duplicate
                && let Some(existing) = self
                    .load_by_idempotency(&trusted.owner_user_id, &request.idempotency_key)
                    .await?
            {
                ensure_idempotent_receipt(&existing, &request_fingerprint)?;
                return Ok(existing);
            }
            return Err(MaterializationReceiptError::Database {
                operation: "insert_materialization_receipt",
                source,
            });
        }
        let receipt = load_receipt_by_id_tx(&mut tx, &trusted.owner_user_id, &receipt_id)
            .await?
            .ok_or_else(|| {
                MaterializationReceiptError::Conflict(format!(
                    "inserted receipt {receipt_id} could not be read"
                ))
            })?;
        tx.commit()
            .await
            .map_err(|source| MaterializationReceiptError::Database {
                operation: "commit_materialization_receipt",
                source,
            })?;
        Ok(receipt)
    }

    /// Read a receipt only when the caller supplies the exact expected trial,
    /// session, and snapshot envelope. Owner filtering is part of the query,
    /// so a receipt from another tenant is indistinguishable from not found.
    pub async fn load_receipt(
        &self,
        owner_user_id: &str,
        receipt_id: &str,
        expected_trial_id: &str,
        expected_session_id: &str,
        expected_envelope: &SnapshotEnvelope,
    ) -> Result<MaterializationReceiptRecord, MaterializationReceiptError> {
        validate_identifier("owner_user_id", owner_user_id, MAX_OWNER_ID_BYTES)?;
        validate_identifier("receipt_id", receipt_id, MAX_RECEIPT_ID_BYTES)?;
        validate_identifier("trial_id", expected_trial_id, MAX_TRIAL_ID_BYTES)?;
        validate_identifier("session_id", expected_session_id, MAX_SESSION_ID_BYTES)?;
        let receipt = sqlx::query(
            "SELECT schema_version, receipt_id, owner_user_id, experiment_id,
                    trial_id, session_id, spec_fingerprint, envelope_id,
                    envelope_fingerprint, component_kind, component_snapshot_ref,
                    component_base_snapshot_ref, component_content_fingerprint,
                    outcome, failure_code,
                    materializer_kind, provider_binding_id, execution_run_id,
                    execution_run_generation, request_fingerprint, idempotency_key,
                    created_at, expires_at
             FROM evaluation_materialization_receipts
             WHERE owner_user_id = ? AND receipt_id = ? LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(receipt_id)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| MaterializationReceiptError::Database {
            operation: "load_materialization_receipt",
            source,
        })?
        .map(decode_receipt)
        .transpose()?
        .ok_or_else(|| MaterializationReceiptError::NotFound(receipt_id.to_string()))?;
        if receipt.trial_id != expected_trial_id || receipt.session_id != expected_session_id {
            return Err(MaterializationReceiptError::Conflict(
                "receipt does not match the requested trial session".to_string(),
            ));
        }
        expected_envelope
            .validate_for(
                owner_user_id,
                &receipt.experiment_id,
                Some(expected_trial_id),
                Some(expected_session_id),
            )
            .map_err(MaterializationReceiptError::InvalidInput)?;
        if receipt.envelope_id != expected_envelope.snapshot_id
            || receipt.envelope_fingerprint != expected_envelope.snapshot_fingerprint
        {
            return Err(MaterializationReceiptError::Conflict(
                "receipt does not match the requested snapshot envelope".to_string(),
            ));
        }
        Ok(receipt)
    }

    /// Validate receipt identities while holding the current canonical Run
    /// generation lock. This is the admission-facing read path; a plain
    /// `load_receipt` is intentionally only an owner-scoped lookup and must
    /// not be used to claim that a trial is executable.
    #[allow(clippy::too_many_arguments)]
    pub async fn validate_receipts_for_execution(
        &self,
        owner_user_id: &str,
        trial_id: &str,
        session_id: &str,
        spec: &ExperimentSpec,
        envelope: &SnapshotEnvelope,
        receipt_ids: &[String],
        now: DateTime<Utc>,
    ) -> Result<Vec<MaterializationReceiptRecord>, MaterializationReceiptError> {
        validate_identifier("owner_user_id", owner_user_id, MAX_OWNER_ID_BYTES)?;
        validate_identifier("trial_id", trial_id, MAX_TRIAL_ID_BYTES)?;
        validate_identifier("session_id", session_id, MAX_SESSION_ID_BYTES)?;
        if receipt_ids.is_empty() || receipt_ids.len() > MaterializationComponentKind::ALL.len() {
            return Err(MaterializationReceiptError::InvalidInput(format!(
                "receipt set must contain between 1 and {} entries",
                MaterializationComponentKind::ALL.len()
            )));
        }
        let mut seen = HashSet::with_capacity(receipt_ids.len());
        for receipt_id in receipt_ids {
            validate_identifier("receipt_id", receipt_id, MAX_RECEIPT_ID_BYTES)?;
            if !seen.insert(receipt_id.as_str()) {
                return Err(MaterializationReceiptError::Conflict(format!(
                    "receipt {receipt_id} was supplied more than once"
                )));
            }
        }
        let mut tx = self.pool.get().begin().await.map_err(|source| {
            MaterializationReceiptError::Database {
                operation: "begin_validate_materialization_receipts",
                source,
            }
        })?;
        let (experiment, binding) = DatabaseEvaluationPlanStore::lock_bound_trial_for_receipt(
            &mut tx,
            owner_user_id,
            trial_id,
            session_id,
        )
        .await?;
        let expected_spec_fingerprint = spec
            .spec_fingerprint()
            .map_err(MaterializationReceiptError::InvalidInput)?;
        if expected_spec_fingerprint != experiment.spec_fingerprint {
            return Err(MaterializationReceiptError::Conflict(
                "execution spec does not match the persisted experiment".to_string(),
            ));
        }
        envelope
            .validate_for(
                owner_user_id,
                &experiment.experiment_id,
                Some(trial_id),
                Some(session_id),
            )
            .map_err(MaterializationReceiptError::InvalidInput)?;
        validate_envelope_against_spec(&experiment.spec, envelope)?;
        let mut receipts = Vec::with_capacity(receipt_ids.len());
        for receipt_id in receipt_ids {
            let receipt = load_receipt_by_id_tx(&mut tx, owner_user_id, receipt_id)
                .await?
                .ok_or_else(|| MaterializationReceiptError::NotFound(receipt_id.clone()))?;
            receipts.push(receipt);
        }
        validate_receipt_set(&binding, &experiment.spec, envelope, &receipts, now)
            .map_err(|error| MaterializationReceiptError::Conflict(error.to_string()))?;
        tx.commit()
            .await
            .map_err(|source| MaterializationReceiptError::Database {
                operation: "commit_validate_materialization_receipts",
                source,
            })?;
        Ok(receipts)
    }

    async fn load_by_idempotency(
        &self,
        owner_user_id: &str,
        idempotency_key: &str,
    ) -> Result<Option<MaterializationReceiptRecord>, MaterializationReceiptError> {
        sqlx::query(
            "SELECT schema_version, receipt_id, owner_user_id, experiment_id,
                    trial_id, session_id, spec_fingerprint, envelope_id,
                    envelope_fingerprint, component_kind, component_snapshot_ref,
                    component_base_snapshot_ref, component_content_fingerprint,
                    outcome, failure_code,
                    materializer_kind, provider_binding_id, execution_run_id,
                    execution_run_generation, request_fingerprint, idempotency_key,
                    created_at, expires_at
             FROM evaluation_materialization_receipts
             WHERE owner_user_id = ? AND idempotency_key = ? LIMIT 1",
        )
        .bind(owner_user_id)
        .bind(idempotency_key)
        .fetch_optional(self.pool.get())
        .await
        .map_err(|source| MaterializationReceiptError::Database {
            operation: "load_materialization_receipt_by_idempotency",
            source,
        })?
        .map(decode_receipt)
        .transpose()
    }
}

/// Validate the exact evidence set before an executor is allowed to claim a
/// trial is isolated. Required dimensions come from the frozen spec; disabled
/// dimensions cannot be smuggled in as synthetic "available" receipts.
pub fn validate_receipt_set(
    binding: &EvaluationTrialBindingRecord,
    spec: &ExperimentSpec,
    envelope: &SnapshotEnvelope,
    receipts: &[MaterializationReceiptRecord],
    now: DateTime<Utc>,
) -> Result<(), MaterializationValidationError> {
    if binding.binding_status != "bound" {
        return Err(MaterializationValidationError::BindingMismatch {
            receipt_id: "<set>".to_string(),
            reason: "trial is not bound to a canonical run".to_string(),
        });
    }
    envelope
        .validate_for(
            &binding.owner_user_id,
            &binding.experiment_id,
            Some(&binding.trial_id),
            binding.session_id.as_deref(),
        )
        .map_err(|reason| MaterializationValidationError::BindingMismatch {
            receipt_id: "<set>".to_string(),
            reason,
        })?;
    let expected_spec_fingerprint = spec.spec_fingerprint().map_err(|reason| {
        MaterializationValidationError::BindingMismatch {
            receipt_id: "<set>".to_string(),
            reason: format!("invalid frozen spec: {reason}"),
        }
    })?;
    if binding.spec_fingerprint != expected_spec_fingerprint {
        return Err(MaterializationValidationError::BindingMismatch {
            receipt_id: "<set>".to_string(),
            reason: "trial binding does not match the supplied frozen spec".to_string(),
        });
    }
    validate_envelope_against_spec(spec, envelope).map_err(|error| {
        MaterializationValidationError::BindingMismatch {
            receipt_id: "<set>".to_string(),
            reason: error.to_string(),
        }
    })?;
    let required_components = required_components_for_spec(spec);
    let required = required_components.iter().copied().collect::<HashSet<_>>();
    let mut seen = HashSet::with_capacity(receipts.len());
    for receipt in receipts {
        validate_record_shape(receipt).map_err(|reason| {
            MaterializationValidationError::InvalidReceipt {
                receipt_id: receipt.receipt_id.clone(),
                reason,
            }
        })?;
        if !seen.insert(receipt.component_kind) {
            return Err(MaterializationValidationError::DuplicateComponent(
                receipt.component_kind,
            ));
        }
        let mismatch = if receipt.owner_user_id != binding.owner_user_id {
            Some("owner differs")
        } else if receipt.experiment_id != binding.experiment_id {
            Some("experiment differs")
        } else if receipt.trial_id != binding.trial_id {
            Some("trial differs")
        } else if receipt.spec_fingerprint != binding.spec_fingerprint {
            Some("spec fingerprint differs")
        } else if receipt.session_id != binding.session_id.as_deref().unwrap_or_default() {
            Some("session differs")
        } else if receipt.provider_binding_id.as_deref()
            != Some(spec.conditions.provider_binding.as_str())
        {
            Some("provider binding differs")
        } else if receipt.execution_run_id.as_deref() != binding.run_id.as_deref()
            || receipt.execution_run_generation != binding.run_generation
        {
            Some("runner identity or generation differs")
        } else if receipt.envelope_id != envelope.snapshot_id
            || receipt.envelope_fingerprint != envelope.snapshot_fingerprint
        {
            Some("snapshot envelope differs")
        } else {
            None
        };
        if let Some(reason) = mismatch {
            return Err(MaterializationValidationError::BindingMismatch {
                receipt_id: receipt.receipt_id.clone(),
                reason: reason.to_string(),
            });
        }
        if !required.contains(&receipt.component_kind) {
            return Err(MaterializationValidationError::BindingMismatch {
                receipt_id: receipt.receipt_id.clone(),
                reason: format!(
                    "component {} is disabled or undeclared by the frozen isolation profile",
                    receipt.component_kind.as_str()
                ),
            });
        }
        if receipt.outcome == MaterializationOutcome::Available {
            validate_component_against_envelope(
                receipt.component_kind,
                receipt.component_snapshot_ref.as_deref(),
                receipt.component_base_snapshot_ref.as_deref(),
                receipt.component_content_fingerprint.as_deref(),
                receipt.outcome,
                envelope,
                spec,
            )
            .map_err(|error| MaterializationValidationError::InvalidReceipt {
                receipt_id: receipt.receipt_id.clone(),
                reason: error.to_string(),
            })?;
        }
        if let Some(expires_at) = receipt.expires_at
            && expires_at <= now
        {
            return Err(MaterializationValidationError::Expired {
                receipt_id: receipt.receipt_id.clone(),
                expires_at,
            });
        }
    }
    for component in required_components {
        let Some(receipt) = receipts
            .iter()
            .find(|receipt| receipt.component_kind == component)
        else {
            return Err(MaterializationValidationError::MissingComponent(component));
        };
        if receipt.outcome != MaterializationOutcome::Available {
            return Err(MaterializationValidationError::ComponentUnavailable {
                receipt_id: receipt.receipt_id.clone(),
                component,
                outcome: receipt.outcome,
            });
        }
    }
    Ok(())
}

fn validate_envelope_against_spec(
    spec: &ExperimentSpec,
    envelope: &SnapshotEnvelope,
) -> Result<(), MaterializationReceiptError> {
    if envelope.context_snapshot_hash != spec.conditions.context_snapshot_hash {
        return Err(MaterializationReceiptError::Conflict(
            "snapshot envelope context hash does not match the frozen context".to_string(),
        ));
    }
    // `tool_policy_hash` is the frozen policy identity in ExperimentSpec;
    // SnapshotEnvelope calls the same identity `policy_snapshot_hash`.
    if envelope.policy_snapshot_hash != spec.conditions.tool_policy_hash {
        return Err(MaterializationReceiptError::Conflict(
            "snapshot envelope policy hash does not match the frozen tool policy".to_string(),
        ));
    }
    Ok(())
}

fn validate_trusted_binding(
    trusted: &TrustedMaterializerContext,
    spec: &ExperimentSpec,
    binding: &EvaluationTrialBindingRecord,
) -> Result<(), MaterializationReceiptError> {
    if trusted.provider_binding_id.as_deref() != Some(spec.conditions.provider_binding.as_str()) {
        return Err(MaterializationReceiptError::Conflict(
            "materializer provider binding does not match the frozen provider".to_string(),
        ));
    }
    if trusted.execution_run_id.as_deref() != binding.run_id.as_deref()
        || trusted.execution_run_generation != binding.run_generation
    {
        return Err(MaterializationReceiptError::Conflict(
            "materializer runner identity or generation does not match the bound run".to_string(),
        ));
    }
    Ok(())
}

fn validate_component_against_envelope(
    component_kind: MaterializationComponentKind,
    component_snapshot_ref: Option<&str>,
    component_base_snapshot_ref: Option<&str>,
    component_content_fingerprint: Option<&str>,
    outcome: MaterializationOutcome,
    envelope: &SnapshotEnvelope,
    spec: &ExperimentSpec,
) -> Result<(), MaterializationReceiptError> {
    if outcome != MaterializationOutcome::Available {
        return Ok(());
    }
    if component_snapshot_ref.is_none() {
        return Err(MaterializationReceiptError::InvalidInput(
            "available materialization requires a component snapshot address".to_string(),
        ));
    }
    match component_kind {
        MaterializationComponentKind::Context => {
            if component_base_snapshot_ref.is_some()
                || component_content_fingerprint != Some(envelope.context_snapshot_hash.as_str())
            {
                return Err(MaterializationReceiptError::Conflict(
                    "context receipt must carry the frozen context content hash".to_string(),
                ));
            }
        }
        MaterializationComponentKind::Policy => {
            if component_base_snapshot_ref.is_some()
                || component_content_fingerprint != Some(envelope.policy_snapshot_hash.as_str())
            {
                return Err(MaterializationReceiptError::Conflict(
                    "policy receipt must carry the frozen policy content hash".to_string(),
                ));
            }
        }
        MaterializationComponentKind::Memory => {
            let _ = (component_base_snapshot_ref, component_content_fingerprint);
            let _ = (&spec.conditions.memory_isolation, envelope);
            return Err(MaterializationReceiptError::Conflict(
                "memory available receipts require a verified materializer adapter".to_string(),
            ));
        }
        MaterializationComponentKind::Data => {
            let _ = (component_base_snapshot_ref, component_content_fingerprint);
            let _ = (&spec.conditions.data_isolation, envelope);
            return Err(MaterializationReceiptError::Conflict(
                "data available receipts require a verified materializer adapter".to_string(),
            ));
        }
        MaterializationComponentKind::Workspace => {
            return Err(MaterializationReceiptError::Conflict(
                "workspace materialization is not declared by this frozen isolation profile"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn validate_new_expiry(
    expires_at: Option<DateTime<Utc>>,
) -> Result<(), MaterializationReceiptError> {
    if let Some(expires_at) = expires_at
        && expires_at <= Utc::now()
    {
        return Err(MaterializationReceiptError::InvalidInput(
            "expires_at must be in the future".to_string(),
        ));
    }
    Ok(())
}

async fn load_receipt_by_id_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    receipt_id: &str,
) -> Result<Option<MaterializationReceiptRecord>, MaterializationReceiptError> {
    load_receipt_query(&mut **tx, owner_user_id, Some(receipt_id), None).await
}

async fn load_receipt_by_idempotency_tx(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
    idempotency_key: &str,
) -> Result<Option<MaterializationReceiptRecord>, MaterializationReceiptError> {
    load_receipt_query(&mut **tx, owner_user_id, None, Some(idempotency_key)).await
}

async fn load_receipt_query<'a, E>(
    executor: E,
    owner_user_id: &str,
    receipt_id: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<Option<MaterializationReceiptRecord>, MaterializationReceiptError>
where
    E: sqlx::Executor<'a, Database = MySql>,
{
    let (predicate, value) = match (receipt_id, idempotency_key) {
        (Some(receipt_id), None) => ("receipt_id = ?", receipt_id),
        (None, Some(idempotency_key)) => ("idempotency_key = ?", idempotency_key),
        _ => {
            return Err(MaterializationReceiptError::InvalidInput(
                "receipt lookup requires exactly one identity key".to_string(),
            ));
        }
    };
    let query = format!(
        "SELECT schema_version, receipt_id, owner_user_id, experiment_id,
                trial_id, session_id, spec_fingerprint, envelope_id,
                envelope_fingerprint, component_kind, component_snapshot_ref,
                component_base_snapshot_ref, component_content_fingerprint,
                outcome, failure_code,
                materializer_kind, provider_binding_id, execution_run_id,
                execution_run_generation, request_fingerprint, idempotency_key,
                created_at, expires_at
         FROM evaluation_materialization_receipts
         WHERE owner_user_id = ? AND {predicate} LIMIT 1"
    );
    sqlx::query(&query)
        .bind(owner_user_id)
        .bind(value)
        .fetch_optional(executor)
        .await
        .map_err(|source| MaterializationReceiptError::Database {
            operation: "load_materialization_receipt_identity",
            source,
        })?
        .map(decode_receipt)
        .transpose()
}

fn decode_receipt(
    row: sqlx::mysql::MySqlRow,
) -> Result<MaterializationReceiptRecord, MaterializationReceiptError> {
    let schema_version = u32::try_from(
        row.try_get::<i64, _>("schema_version")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
    )
    .map_err(|_| {
        MaterializationReceiptError::Conflict(
            "persisted materialization receipt schema version is invalid".to_string(),
        )
    })?;
    if schema_version != MATERIALIZATION_RECEIPT_SCHEMA_VERSION {
        return Err(MaterializationReceiptError::Conflict(format!(
            "unsupported materialization receipt schema version {schema_version}"
        )));
    }
    let receipt = MaterializationReceiptRecord {
        schema_version,
        receipt_id: required_string(&row, "receipt_id")?,
        owner_user_id: required_string(&row, "owner_user_id")?,
        experiment_id: required_string(&row, "experiment_id")?,
        trial_id: required_string(&row, "trial_id")?,
        session_id: required_string(&row, "session_id")?,
        spec_fingerprint: required_string(&row, "spec_fingerprint")?,
        envelope_id: required_string(&row, "envelope_id")?,
        envelope_fingerprint: required_string(&row, "envelope_fingerprint")?,
        component_kind: MaterializationComponentKind::parse(&required_string(
            &row,
            "component_kind",
        )?)?,
        component_snapshot_ref: row
            .try_get("component_snapshot_ref")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        component_base_snapshot_ref: row
            .try_get("component_base_snapshot_ref")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        component_content_fingerprint: row
            .try_get("component_content_fingerprint")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        outcome: MaterializationOutcome::parse(&required_string(&row, "outcome")?)?,
        failure_code: row
            .try_get("failure_code")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        materializer_kind: required_string(&row, "materializer_kind")?,
        provider_binding_id: row
            .try_get("provider_binding_id")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        execution_run_id: row
            .try_get("execution_run_id")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        execution_run_generation: decode_execution_run_generation(&row)?,
        request_fingerprint: required_string(&row, "request_fingerprint")?,
        idempotency_key: required_string(&row, "idempotency_key")?,
        created_at: row
            .try_get("created_at")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
        expires_at: row
            .try_get("expires_at")
            .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?,
    };
    validate_record_shape(&receipt).map_err(|reason| {
        MaterializationReceiptError::Conflict(format!(
            "persisted receipt {} is invalid: {reason}",
            receipt.receipt_id
        ))
    })?;
    Ok(receipt)
}

fn decode_execution_run_generation(
    row: &sqlx::mysql::MySqlRow,
) -> Result<Option<u64>, MaterializationReceiptError> {
    let generation: Option<i64> = row
        .try_get("execution_run_generation")
        .map_err(|source| receipt_database_error("decode_materialization_receipt", source))?;
    generation
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                MaterializationReceiptError::Conflict(
                    "persisted runner generation is negative".to_string(),
                )
            })
        })
        .transpose()
}

fn required_string(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
) -> Result<String, MaterializationReceiptError> {
    row.try_get(column)
        .map_err(|source| receipt_database_error("decode_materialization_receipt", source))
}

fn receipt_database_error(
    operation: &'static str,
    source: sqlx::Error,
) -> MaterializationReceiptError {
    MaterializationReceiptError::Database { operation, source }
}

fn ensure_idempotent_receipt(
    existing: &MaterializationReceiptRecord,
    request_fingerprint: &str,
) -> Result<(), MaterializationReceiptError> {
    if existing.request_fingerprint != request_fingerprint {
        return Err(MaterializationReceiptError::Conflict(format!(
            "idempotency key {} was already used for another materialization request",
            existing.idempotency_key
        )));
    }
    Ok(())
}

fn validate_trusted_context(
    trusted: &TrustedMaterializerContext,
) -> Result<(), MaterializationReceiptError> {
    validate_identifier("owner_user_id", &trusted.owner_user_id, MAX_OWNER_ID_BYTES)?;
    validate_identifier(
        "materializer_kind",
        &trusted.materializer_kind,
        MAX_MATERIALIZER_KIND_BYTES,
    )?;
    validate_optional_identifier(
        "provider_binding_id",
        trusted.provider_binding_id.as_deref(),
        MAX_PROVIDER_BINDING_BYTES,
    )?;
    validate_optional_identifier(
        "execution_run_id",
        trusted.execution_run_id.as_deref(),
        MAX_RUNNER_BINDING_BYTES,
    )?;
    if trusted.execution_run_generation > Some(i64::MAX as u64) {
        return Err(MaterializationReceiptError::InvalidInput(
            "execution_run_generation exceeds BIGINT capacity".to_string(),
        ));
    }
    Ok(())
}

fn validate_request_shape(
    request: &MaterializationReceiptRequest,
) -> Result<(), MaterializationReceiptError> {
    validate_identifier("trial_id", &request.trial_id, MAX_TRIAL_ID_BYTES)?;
    validate_identifier("session_id", &request.session_id, MAX_SESSION_ID_BYTES)?;
    validate_identifier(
        "idempotency_key",
        &request.idempotency_key,
        MAX_IDEMPOTENCY_KEY_BYTES,
    )?;
    request
        .envelope
        .validate()
        .map_err(MaterializationReceiptError::InvalidInput)?;
    validate_optional_identifier(
        "component_snapshot_ref",
        request.component_snapshot_ref.as_deref(),
        MAX_COMPONENT_REF_BYTES,
    )?;
    validate_optional_identifier(
        "component_base_snapshot_ref",
        request.component_base_snapshot_ref.as_deref(),
        MAX_COMPONENT_REF_BYTES,
    )?;
    validate_optional_identifier(
        "component_content_fingerprint",
        request.component_content_fingerprint.as_deref(),
        MAX_COMPONENT_FINGERPRINT_BYTES,
    )?;
    validate_optional_identifier(
        "failure_code",
        request.failure_code.as_deref(),
        MAX_FAILURE_CODE_BYTES,
    )?;
    match request.outcome {
        MaterializationOutcome::Available => {
            if request.component_snapshot_ref.is_none() {
                return Err(MaterializationReceiptError::InvalidInput(
                    "available materialization requires component_snapshot_ref".to_string(),
                ));
            }
            if request.failure_code.is_some() {
                return Err(MaterializationReceiptError::InvalidInput(
                    "available materialization cannot carry failure_code".to_string(),
                ));
            }
        }
        MaterializationOutcome::Unavailable | MaterializationOutcome::Failed => {
            if request.failure_code.is_none() {
                return Err(MaterializationReceiptError::InvalidInput(
                    "unavailable or failed materialization requires failure_code".to_string(),
                ));
            }
            if request.expires_at.is_some() {
                return Err(MaterializationReceiptError::InvalidInput(
                    "unavailable or failed materialization cannot expire".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_record_shape(receipt: &MaterializationReceiptRecord) -> Result<(), String> {
    if receipt.schema_version != MATERIALIZATION_RECEIPT_SCHEMA_VERSION {
        return Err("unsupported schema version".to_string());
    }
    for (label, value, max) in [
        (
            "receipt_id",
            receipt.receipt_id.as_str(),
            MAX_RECEIPT_ID_BYTES,
        ),
        (
            "owner_user_id",
            receipt.owner_user_id.as_str(),
            MAX_OWNER_ID_BYTES,
        ),
        (
            "experiment_id",
            receipt.experiment_id.as_str(),
            MAX_EXPERIMENT_ID_BYTES,
        ),
        ("trial_id", receipt.trial_id.as_str(), MAX_TRIAL_ID_BYTES),
        (
            "session_id",
            receipt.session_id.as_str(),
            MAX_SESSION_ID_BYTES,
        ),
        (
            "materializer_kind",
            receipt.materializer_kind.as_str(),
            MAX_MATERIALIZER_KIND_BYTES,
        ),
        (
            "request_fingerprint",
            receipt.request_fingerprint.as_str(),
            MAX_COMPONENT_FINGERPRINT_BYTES,
        ),
        (
            "idempotency_key",
            receipt.idempotency_key.as_str(),
            MAX_IDEMPOTENCY_KEY_BYTES,
        ),
    ] {
        if value.trim().is_empty() || value.len() > max {
            return Err(format!("{label} is empty or exceeds {max} bytes"));
        }
    }
    if let Some(component_ref) = receipt.component_snapshot_ref.as_deref()
        && (component_ref.trim().is_empty() || component_ref.len() > MAX_COMPONENT_REF_BYTES)
    {
        return Err("component_snapshot_ref is empty or too large".to_string());
    }
    if let Some(base_ref) = receipt.component_base_snapshot_ref.as_deref()
        && (base_ref.trim().is_empty() || base_ref.len() > MAX_COMPONENT_REF_BYTES)
    {
        return Err("component_base_snapshot_ref is empty or too large".to_string());
    }
    if let Some(content_fingerprint) = receipt.component_content_fingerprint.as_deref()
        && (content_fingerprint.trim().is_empty()
            || content_fingerprint.len() > MAX_COMPONENT_FINGERPRINT_BYTES)
    {
        return Err("component_content_fingerprint is empty or too large".to_string());
    }
    if let Some(failure_code) = receipt.failure_code.as_deref()
        && (failure_code.trim().is_empty() || failure_code.len() > MAX_FAILURE_CODE_BYTES)
    {
        return Err("failure_code is empty or too large".to_string());
    }
    match receipt.outcome {
        MaterializationOutcome::Available if receipt.component_snapshot_ref.is_none() => {
            return Err("available receipt has no component snapshot reference".to_string());
        }
        MaterializationOutcome::Available if receipt.failure_code.is_some() => {
            return Err("available receipt has a failure code".to_string());
        }
        MaterializationOutcome::Unavailable | MaterializationOutcome::Failed
            if receipt.failure_code.is_none() =>
        {
            return Err("failed or unavailable receipt has no failure code".to_string());
        }
        MaterializationOutcome::Unavailable | MaterializationOutcome::Failed
            if receipt.expires_at.is_some() =>
        {
            return Err("failed or unavailable receipt cannot expire".to_string());
        }
        _ => {}
    }
    Ok(())
}

fn request_fingerprint(
    trusted: &TrustedMaterializerContext,
    request: &MaterializationReceiptRequest,
) -> Result<String, MaterializationReceiptError> {
    let envelope = serde_json::to_value(&request.envelope).map_err(|source| {
        MaterializationReceiptError::Json {
            operation: "serialize_materialization_envelope",
            source,
        }
    })?;
    let expires_at = request
        .expires_at
        .map(|value| value.to_rfc3339_opts(chrono::SecondsFormat::Micros, true));
    let payload = serde_json::json!({
        "schema_version": MATERIALIZATION_RECEIPT_SCHEMA_VERSION,
        "owner_user_id": trusted.owner_user_id,
        "materializer_kind": trusted.materializer_kind,
        "provider_binding_id": trusted.provider_binding_id,
        "execution_run_id": trusted.execution_run_id,
        "execution_run_generation": trusted.execution_run_generation,
        "trial_id": request.trial_id,
        "session_id": request.session_id,
        "envelope": envelope,
        "component_kind": request.component_kind.as_str(),
        "component_snapshot_ref": request.component_snapshot_ref,
        "component_base_snapshot_ref": request.component_base_snapshot_ref,
        "component_content_fingerprint": request.component_content_fingerprint,
        "outcome": request.outcome.as_str(),
        "failure_code": request.failure_code,
        "expires_at": expires_at,
        "idempotency_key": request.idempotency_key,
    });
    let canonical = astra_core::canonical_json_string(&payload);
    let digest = Sha256::digest(canonical.as_bytes());
    Ok(format!("sha256:{digest:x}"))
}

fn validate_identifier(
    label: &str,
    value: &str,
    max_bytes: usize,
) -> Result<(), MaterializationReceiptError> {
    if value.trim().is_empty() {
        return Err(MaterializationReceiptError::InvalidInput(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(MaterializationReceiptError::InvalidInput(format!(
            "{label} must be at most {max_bytes} bytes"
        )));
    }
    Ok(())
}

fn validate_optional_identifier(
    label: &str,
    value: Option<&str>,
    max_bytes: usize,
) -> Result<(), MaterializationReceiptError> {
    if let Some(value) = value {
        validate_identifier(label, value, max_bytes)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::assessment::ComparisonArm;
    use super::super::experiment::{
        DataIsolation, EvaluationCase, EvaluationTarget, EvaluationTargetKind, FrozenConditions,
        MemoryIsolation, RevisionRef, TrialOrder, TrialUnit,
    };
    use super::*;
    use astra_core::composite_snapshot::CompositeSnapshot;

    fn envelope() -> SnapshotEnvelope {
        SnapshotEnvelope::new(
            "owner-a",
            "experiment-a",
            Some("trial-a".to_string()),
            CompositeSnapshot {
                snapshot_id: "composite-a".to_string(),
                session_id: "session-a".to_string(),
                turn: 1,
                created_at: "2026-09-18T00:00:00Z".to_string(),
                version: 1,
                label: None,
                refs: vec![],
            },
            "ctx-a",
            "policy-a",
        )
        .expect("envelope")
    }

    fn binding() -> EvaluationTrialBindingRecord {
        let spec = test_spec();
        let spec_fingerprint = spec.spec_fingerprint().expect("spec fingerprint");
        EvaluationTrialBindingRecord {
            owner_user_id: "owner-a".to_string(),
            trial_id: "trial-a".to_string(),
            experiment_id: "experiment-a".to_string(),
            spec_fingerprint: spec_fingerprint.clone(),
            trial: TrialUnit {
                trial_id: "trial-a".to_string(),
                experiment_id: "experiment-a".to_string(),
                spec_fingerprint,
                arm: ComparisonArm::Baseline,
                case_id: "case-a".to_string(),
                repetition: 1,
                sequence: 1,
                input_snapshot_ref: "input-a".to_string(),
                input_content_hash: "hash-a".to_string(),
                verifier_id: "verifier-a".to_string(),
                verifier_version: "1".to_string(),
                holdout: false,
                memory_base_snapshot_ref: None,
                data_base_snapshot_ref: None,
            },
            binding_status: "bound".to_string(),
            session_id: Some("session-a".to_string()),
            run_id: Some("run-a".to_string()),
            run_generation: Some(0),
            created_at: "2026-09-18 00:00:00.000000".to_string(),
            updated_at: "2026-09-18 00:00:00.000000".to_string(),
        }
    }

    fn test_spec() -> ExperimentSpec {
        ExperimentSpec {
            schema_version: 1,
            experiment_id: "experiment-a".to_string(),
            target: EvaluationTarget {
                kind: EvaluationTargetKind::Skill,
                baseline: RevisionRef {
                    revision_id: "baseline".to_string(),
                    content_hash: "sha256:baseline".to_string(),
                    content: None,
                },
                candidate: RevisionRef {
                    revision_id: "candidate".to_string(),
                    content_hash: "sha256:candidate".to_string(),
                    content: None,
                },
                skill_name: Some("sample-skill".to_string()),
            },
            cases: vec![EvaluationCase {
                case_id: "case-a".to_string(),
                input_snapshot_ref: "input-a".to_string(),
                input_content_hash: "hash-a".to_string(),
                verifier_id: "verifier-a".to_string(),
                verifier_version: "1".to_string(),
                holdout: false,
                input_content: None,
            }],
            repetitions: 1,
            order: TrialOrder::BaselineFirst,
            conditions: FrozenConditions {
                isolation_profile: "prompt_only_private".to_string(),
                model_binding: "model-v1".to_string(),
                provider_binding: "provider-a".to_string(),
                context_snapshot_hash: "ctx-a".to_string(),
                tool_policy_hash: "policy-a".to_string(),
                cache_policy: "recorded".to_string(),
                memory_isolation: MemoryIsolation::Disabled,
                data_isolation: DataIsolation::Disabled,
            },
            budget: super::super::experiment::EvaluationBudget {
                max_trials: 2,
                max_concurrency: 1,
                max_wall_time_secs: 60,
            },
            adapter_profile_version: None,
        }
    }

    fn record(
        envelope: &SnapshotEnvelope,
        kind: MaterializationComponentKind,
        outcome: MaterializationOutcome,
    ) -> MaterializationReceiptRecord {
        MaterializationReceiptRecord {
            schema_version: MATERIALIZATION_RECEIPT_SCHEMA_VERSION,
            receipt_id: format!("receipt-{}", kind.as_str()),
            owner_user_id: "owner-a".to_string(),
            experiment_id: "experiment-a".to_string(),
            trial_id: "trial-a".to_string(),
            session_id: "session-a".to_string(),
            spec_fingerprint: test_spec().spec_fingerprint().expect("spec fingerprint"),
            envelope_id: envelope.snapshot_id.clone(),
            envelope_fingerprint: envelope.snapshot_fingerprint.clone(),
            component_kind: kind,
            component_snapshot_ref: (outcome == MaterializationOutcome::Available)
                .then(|| format!("{}/snapshot", kind.as_str())),
            component_base_snapshot_ref: None,
            component_content_fingerprint: (outcome == MaterializationOutcome::Available).then(
                || match kind {
                    MaterializationComponentKind::Context => envelope.context_snapshot_hash.clone(),
                    MaterializationComponentKind::Policy => envelope.policy_snapshot_hash.clone(),
                    _ => format!("sha256:{}-content", kind.as_str()),
                },
            ),
            outcome,
            failure_code: (outcome != MaterializationOutcome::Available)
                .then(|| "not_ready".to_string()),
            materializer_kind: "test".to_string(),
            provider_binding_id: Some("provider-a".to_string()),
            execution_run_id: Some("run-a".to_string()),
            execution_run_generation: Some(0),
            request_fingerprint: "sha256:request".to_string(),
            idempotency_key: format!("key-{}", kind.as_str()),
            created_at: Utc::now(),
            expires_at: (outcome == MaterializationOutcome::Available)
                .then(|| Utc::now() + chrono::Duration::minutes(5)),
        }
    }

    #[test]
    fn receipt_set_requires_only_declared_components() {
        let binding = binding();
        let spec = test_spec();
        let envelope = envelope();
        let receipts = vec![
            record(
                &envelope,
                MaterializationComponentKind::Context,
                MaterializationOutcome::Available,
            ),
            record(
                &envelope,
                MaterializationComponentKind::Policy,
                MaterializationOutcome::Available,
            ),
        ];
        assert!(validate_receipt_set(&binding, &spec, &envelope, &receipts, Utc::now(),).is_ok());
    }

    #[test]
    fn receipt_set_rejects_expired_and_wrong_envelope() {
        let binding = binding();
        let spec = test_spec();
        let envelope = envelope();
        let mut receipt = record(
            &envelope,
            MaterializationComponentKind::Context,
            MaterializationOutcome::Available,
        );
        receipt.expires_at = Some(Utc::now() - chrono::Duration::seconds(1));
        assert!(matches!(
            validate_receipt_set(&binding, &spec, &envelope, &[receipt], Utc::now()),
            Err(MaterializationValidationError::Expired { .. })
        ));
        let mut wrong = record(
            &envelope,
            MaterializationComponentKind::Policy,
            MaterializationOutcome::Available,
        );
        wrong.envelope_id = "other".to_string();
        assert!(matches!(
            validate_receipt_set(&binding, &spec, &envelope, &[wrong], Utc::now()),
            Err(MaterializationValidationError::BindingMismatch { .. })
        ));
    }

    #[test]
    fn receipt_set_rejects_missing_policy_and_disabled_memory() {
        let binding = binding();
        let spec = test_spec();
        let envelope = envelope();
        let context = record(
            &envelope,
            MaterializationComponentKind::Context,
            MaterializationOutcome::Available,
        );
        assert!(matches!(
            validate_receipt_set(&binding, &spec, &envelope, &[context.clone()], Utc::now(),),
            Err(MaterializationValidationError::MissingComponent(
                MaterializationComponentKind::Policy
            ))
        ));
        let memory = record(
            &envelope,
            MaterializationComponentKind::Memory,
            MaterializationOutcome::Available,
        );
        assert!(matches!(
            validate_receipt_set(&binding, &spec, &envelope, &[context, memory], Utc::now(),),
            Err(MaterializationValidationError::BindingMismatch { .. })
        ));
    }

    #[test]
    fn receipt_set_rejects_wrong_provider_or_execution_generation() {
        let binding = binding();
        let spec = test_spec();
        let envelope = envelope();
        let mut wrong_provider = record(
            &envelope,
            MaterializationComponentKind::Context,
            MaterializationOutcome::Available,
        );
        wrong_provider.provider_binding_id = Some("provider-other".to_string());
        assert!(matches!(
            validate_receipt_set(&binding, &spec, &envelope, &[wrong_provider], Utc::now(),),
            Err(MaterializationValidationError::BindingMismatch { .. })
        ));
        let mut wrong_generation = record(
            &envelope,
            MaterializationComponentKind::Context,
            MaterializationOutcome::Available,
        );
        wrong_generation.execution_run_generation = Some(2);
        assert!(matches!(
            validate_receipt_set(&binding, &spec, &envelope, &[wrong_generation], Utc::now(),),
            Err(MaterializationValidationError::BindingMismatch { .. })
        ));
    }

    #[test]
    fn request_shape_fails_closed_for_success_without_address() {
        let request = MaterializationReceiptRequest {
            trial_id: "trial-a".to_string(),
            session_id: "session-a".to_string(),
            envelope: envelope(),
            component_kind: MaterializationComponentKind::Memory,
            component_snapshot_ref: None,
            component_base_snapshot_ref: None,
            component_content_fingerprint: None,
            outcome: MaterializationOutcome::Available,
            failure_code: None,
            expires_at: Some(Utc::now() + chrono::Duration::minutes(5)),
            idempotency_key: "key".to_string(),
        };
        assert!(validate_request_shape(&request).is_err());
    }
}
