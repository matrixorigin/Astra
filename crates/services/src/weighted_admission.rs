//! Byte/token-weighted multi-tenant admission.
//!
//! Billing quotas remain separate. This controller protects resident memory,
//! provider concurrency, CPU-heavy serialization/compaction, and canonical
//! I/O using deterministic work estimates.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use astra_core::SharedPool;
use astra_turn_types::SessionKeyV1;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;
use tokio::sync::Notify;
use uuid::Uuid;

pub(crate) const DISTRIBUTED_ADMISSION_SCOPE: &str = "canonical_turn_v1";
const DISTRIBUTED_ADMISSION_MAX_TTL: Duration = Duration::from_secs(15 * 60);
const DISTRIBUTED_ADMISSION_IDEMPOTENCY_DOMAIN: &[u8] =
    b"astra.distributed-weighted-admission-idempotency.v1\0";
const DISTRIBUTED_ADMISSION_CAPACITY_DOMAIN: &[u8] =
    b"astra.distributed-weighted-admission-capacity.v1\0";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionWork {
    pub resident_bytes: u64,
    pub context_tokens: u64,
    pub provider_slots: u32,
    pub cpu_units: u64,
    pub io_bytes: u64,
}

impl AdmissionWork {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            resident_bytes: self.resident_bytes.checked_add(other.resident_bytes)?,
            context_tokens: self.context_tokens.checked_add(other.context_tokens)?,
            provider_slots: self.provider_slots.checked_add(other.provider_slots)?,
            cpu_units: self.cpu_units.checked_add(other.cpu_units)?,
            io_bytes: self.io_bytes.checked_add(other.io_bytes)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            resident_bytes: self.resident_bytes.checked_sub(other.resident_bytes)?,
            context_tokens: self.context_tokens.checked_sub(other.context_tokens)?,
            provider_slots: self.provider_slots.checked_sub(other.provider_slots)?,
            cpu_units: self.cpu_units.checked_sub(other.cpu_units)?,
            io_bytes: self.io_bytes.checked_sub(other.io_bytes)?,
        })
    }

    pub fn fits_within(self, capacity: Self) -> bool {
        self.resident_bytes <= capacity.resident_bytes
            && self.context_tokens <= capacity.context_tokens
            && self.provider_slots <= capacity.provider_slots
            && self.cpu_units <= capacity.cpu_units
            && self.io_bytes <= capacity.io_bytes
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct WeightedAdmissionLimits {
    pub global: AdmissionWork,
    /// Hard burst ceiling for one owner. Keeping this below `global` reserves
    /// capacity for another tenant even when the first tenant is noisy.
    pub per_owner: AdmissionWork,
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum WeightedAdmissionError {
    #[error("requested work exceeds the global admission capacity")]
    RequestExceedsGlobal,
    #[error("requested work exceeds the per-owner admission capacity")]
    RequestExceedsOwner,
    #[error("global weighted admission capacity is currently exhausted")]
    GlobalExhausted,
    #[error("owner weighted admission share is currently exhausted")]
    OwnerExhausted,
}

#[derive(Debug, Error)]
pub enum DistributedAdmissionError {
    #[error(transparent)]
    Capacity(#[from] WeightedAdmissionError),
    #[error("distributed admission request is invalid: {0}")]
    Invalid(String),
    #[error("distributed admission reservation was fenced or expired")]
    Fenced,
    #[error("distributed admission idempotency key was reused for different work")]
    IdempotencyMismatch,
    #[error(
        "distributed admission capacity configuration mismatch (active={active}, requested={requested})"
    )]
    ConfigurationMismatch { active: String, requested: String },
    #[error("distributed admission database operation {operation} failed: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: sqlx::Error,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DistributedAdmissionReservation {
    pub reservation_id: String,
    pub key: SessionKeyV1,
    pub work: AdmissionWork,
    pub expires_at_unix_ms: i64,
    idempotency_hash: String,
}

#[derive(Clone)]
pub struct DatabaseWeightedAdmissionController {
    pool: SharedPool,
    limits: WeightedAdmissionLimits,
}

impl DatabaseWeightedAdmissionController {
    pub fn new(
        pool: SharedPool,
        limits: WeightedAdmissionLimits,
    ) -> Result<Self, WeightedAdmissionError> {
        if !limits.per_owner.fits_within(limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        Ok(Self { pool, limits })
    }

    /// Replace the capacity snapshot used by subsequent reservations.
    ///
    /// The durable gate validates the new snapshot while its row is locked, so
    /// changing a controller after it has been attached to a pool remains
    /// fail-closed during a live rollout. This also keeps the public runtime
    /// builder order-independent (`with_pool(...).with_admission_limits(...)`).
    pub fn with_limits(
        &mut self,
        limits: WeightedAdmissionLimits,
    ) -> Result<(), WeightedAdmissionError> {
        if !limits.per_owner.fits_within(limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        self.limits = limits;
        Ok(())
    }

    /// Reserve hard capacity across all pods.
    ///
    /// The single gate row serializes only this short admission transaction.
    /// Inference, tools, materialization, and network I/O happen after it is
    /// released. Active rows are bounded by the global provider-slot budget,
    /// so calculating byte/token totals cannot grow with session history.
    pub async fn try_reserve(
        &self,
        key: &SessionKeyV1,
        work: AdmissionWork,
        ttl: Duration,
        idempotency_key: &str,
    ) -> Result<DistributedAdmissionPermit, DistributedAdmissionError> {
        validate_distributed_request(key, work, ttl, idempotency_key)?;
        validate_requested_work(self.limits, work)?;

        let idempotency_hash = distributed_idempotency_hash(idempotency_key);
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| distributed_database_error("begin_reservation", source))?;
        let gate = lock_distributed_admission_gate(&mut tx).await?;
        sqlx::query(
            "DELETE FROM session_weighted_admission_reservations
             WHERE scope_name = ? AND expires_at <= NOW(6)",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut *tx)
        .await
        .map_err(|source| distributed_database_error("cleanup_expired", source))?;
        ensure_distributed_admission_capacity(&mut tx, self.limits, gate.capacity_hash.as_deref())
            .await?;

        if let Some(existing) =
            load_distributed_reservation(&mut tx, key, &idempotency_hash).await?
        {
            if existing.work != work {
                return Err(DistributedAdmissionError::IdempotencyMismatch);
            }
            tx.commit()
                .await
                .map_err(|source| distributed_database_error("commit_replay", source))?;
            return Ok(DistributedAdmissionPermit::new(self.clone(), existing));
        }

        let (global_used, owner_used) = load_distributed_usage(&mut tx, &key.owner_user_id).await?;
        validate_available_work(self.limits, global_used, owner_used, work)?;

        let expires_at = gate
            .now
            .checked_add_signed(chrono::Duration::from_std(ttl).map_err(|_| {
                DistributedAdmissionError::Invalid("admission TTL is outside clock range".into())
            })?)
            .ok_or_else(|| {
                DistributedAdmissionError::Invalid("admission expiry overflows clock".into())
            })?;
        let reservation = DistributedAdmissionReservation {
            reservation_id: Uuid::new_v4().to_string(),
            key: key.clone(),
            work,
            expires_at_unix_ms: expires_at.and_utc().timestamp_millis(),
            idempotency_hash,
        };
        insert_distributed_reservation(&mut tx, &reservation, expires_at).await?;
        tx.commit()
            .await
            .map_err(|source| distributed_database_error("commit_reservation", source))?;
        Ok(DistributedAdmissionPermit::new(self.clone(), reservation))
    }

    pub async fn renew(
        &self,
        reservation: &DistributedAdmissionReservation,
        ttl: Duration,
    ) -> Result<DistributedAdmissionReservation, DistributedAdmissionError> {
        validate_distributed_request(
            &reservation.key,
            reservation.work,
            ttl,
            &reservation.idempotency_hash,
        )?;
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| distributed_database_error("begin_renewal", source))?;
        let now = lock_distributed_admission_gate(&mut tx).await?.now;
        let expires_at = now
            .checked_add_signed(chrono::Duration::from_std(ttl).map_err(|_| {
                DistributedAdmissionError::Invalid("admission TTL is outside clock range".into())
            })?)
            .ok_or_else(|| {
                DistributedAdmissionError::Invalid("admission expiry overflows clock".into())
            })?;
        let result = sqlx::query(
            "UPDATE session_weighted_admission_reservations
             SET expires_at = ?
             WHERE scope_name = ? AND reservation_id = ?
               AND isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?
               AND idempotency_hash = ? AND expires_at > ?",
        )
        .bind(expires_at)
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(&reservation.reservation_id)
        .bind(&reservation.key.isolation_domain)
        .bind(&reservation.key.owner_user_id)
        .bind(&reservation.key.session_id)
        .bind(&reservation.key.branch_id)
        .bind(&reservation.idempotency_hash)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|source| distributed_database_error("renew_reservation", source))?;
        if result.rows_affected() != 1 {
            return Err(DistributedAdmissionError::Fenced);
        }
        tx.commit()
            .await
            .map_err(|source| distributed_database_error("commit_renewal", source))?;
        let mut renewed = reservation.clone();
        renewed.expires_at_unix_ms = expires_at.and_utc().timestamp_millis();
        Ok(renewed)
    }

    async fn release(
        &self,
        reservation: &DistributedAdmissionReservation,
    ) -> Result<(), DistributedAdmissionError> {
        let mut tx = self
            .pool
            .get()
            .begin()
            .await
            .map_err(|source| distributed_database_error("begin_release", source))?;
        let _gate = lock_distributed_admission_gate(&mut tx).await?;
        sqlx::query(
            "DELETE FROM session_weighted_admission_reservations
             WHERE scope_name = ? AND reservation_id = ?
               AND isolation_domain = ? AND owner_user_id = ?
               AND session_id = ? AND branch_id = ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(&reservation.reservation_id)
        .bind(&reservation.key.isolation_domain)
        .bind(&reservation.key.owner_user_id)
        .bind(&reservation.key.session_id)
        .bind(&reservation.key.branch_id)
        .execute(&mut *tx)
        .await
        .map_err(|source| distributed_database_error("release_reservation", source))?;
        tx.commit()
            .await
            .map_err(|source| distributed_database_error("commit_release", source))?;
        Ok(())
    }
}

pub struct DistributedAdmissionPermit {
    controller: DatabaseWeightedAdmissionController,
    reservation: DistributedAdmissionReservation,
    release_started: Arc<AtomicBool>,
}

impl DistributedAdmissionPermit {
    fn new(
        controller: DatabaseWeightedAdmissionController,
        reservation: DistributedAdmissionReservation,
    ) -> Self {
        Self {
            controller,
            reservation,
            release_started: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn reservation(&self) -> &DistributedAdmissionReservation {
        &self.reservation
    }

    pub async fn release(&self) -> Result<(), DistributedAdmissionError> {
        if self.release_started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.controller.release(&self.reservation).await
    }
}

impl Drop for DistributedAdmissionPermit {
    fn drop(&mut self) {
        if self.release_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let controller = self.controller.clone();
        let reservation = self.reservation.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = controller.release(&reservation).await {
                    tracing::warn!(
                        target: "astra_services::weighted_admission",
                        error = %error,
                        "failed to release distributed weighted admission; TTL cleanup will reclaim it"
                    );
                }
            });
        }
    }
}

fn validate_distributed_request(
    key: &SessionKeyV1,
    work: AdmissionWork,
    ttl: Duration,
    idempotency_key: &str,
) -> Result<(), DistributedAdmissionError> {
    key.validate()
        .map_err(|error| DistributedAdmissionError::Invalid(error.to_string()))?;
    if ttl.is_zero() || ttl > DISTRIBUTED_ADMISSION_MAX_TTL {
        return Err(DistributedAdmissionError::Invalid(
            "admission TTL must be between 1 ms and 15 minutes".into(),
        ));
    }
    if idempotency_key.is_empty()
        || idempotency_key.len() > 512
        || idempotency_key.chars().any(char::is_control)
    {
        return Err(DistributedAdmissionError::Invalid(
            "idempotency key must be non-empty and at most 512 bytes".into(),
        ));
    }
    if work.provider_slots == 0 {
        return Err(DistributedAdmissionError::Invalid(
            "distributed turn admission requires at least one provider slot".into(),
        ));
    }
    Ok(())
}

fn validate_requested_work(
    limits: WeightedAdmissionLimits,
    work: AdmissionWork,
) -> Result<(), WeightedAdmissionError> {
    if !work.fits_within(limits.global) {
        return Err(WeightedAdmissionError::RequestExceedsGlobal);
    }
    if !work.fits_within(limits.per_owner) {
        return Err(WeightedAdmissionError::RequestExceedsOwner);
    }
    Ok(())
}

fn validate_available_work(
    limits: WeightedAdmissionLimits,
    global_used: AdmissionWork,
    owner_used: AdmissionWork,
    work: AdmissionWork,
) -> Result<(), WeightedAdmissionError> {
    let global_next = global_used
        .checked_add(work)
        .ok_or(WeightedAdmissionError::GlobalExhausted)?;
    if !global_next.fits_within(limits.global) {
        return Err(WeightedAdmissionError::GlobalExhausted);
    }
    let owner_next = owner_used
        .checked_add(work)
        .ok_or(WeightedAdmissionError::OwnerExhausted)?;
    if !owner_next.fits_within(limits.per_owner) {
        return Err(WeightedAdmissionError::OwnerExhausted);
    }
    Ok(())
}

struct LockedDistributedAdmissionGate {
    now: chrono::NaiveDateTime,
    capacity_hash: Option<String>,
}

async fn lock_distributed_admission_gate(
    tx: &mut Transaction<'_, MySql>,
) -> Result<LockedDistributedAdmissionGate, DistributedAdmissionError> {
    let row = sqlx::query(
        "SELECT capacity_hash,
                CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) AS database_now_unix_ms
         FROM session_weighted_admission_gates
         WHERE scope_name = ? FOR UPDATE",
    )
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("lock_gate", source))?;
    let capacity_hash = row
        .try_get::<Option<String>, _>("capacity_hash")
        .map_err(|source| distributed_database_error("decode_gate_capacity_hash", source))?;
    let unix_ms = row
        .try_get::<i64, _>("database_now_unix_ms")
        .map_err(|source| distributed_database_error("decode_gate_database_time", source))?;
    let now = chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|timestamp| timestamp.naive_utc())
        .ok_or_else(|| {
            DistributedAdmissionError::Invalid("database time is outside chrono range".into())
        })?;
    Ok(LockedDistributedAdmissionGate { now, capacity_hash })
}

/// Bind the durable admission scope to one capacity configuration.
///
/// Every server sharing this scope must use the same capacity snapshot. The
/// first server records the snapshot; later servers fail closed while
/// reservations are active and may adopt a new snapshot only after the active
/// reservations have drained. A NULL hash is the uninitialized state of the
/// current capacity protocol.
async fn ensure_distributed_admission_capacity(
    tx: &mut Transaction<'_, MySql>,
    limits: WeightedAdmissionLimits,
    active: Option<&str>,
) -> Result<(), DistributedAdmissionError> {
    let requested = admission_capacity_hash(limits);
    if active == Some(requested.as_str()) {
        return Ok(());
    }
    let active_reservations: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM session_weighted_admission_reservations
         WHERE scope_name = ?",
    )
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("count_capacity_reservations", source))?;
    let transition = capacity_gate_transition(active, active_reservations, &requested);
    let update_operation = match &transition {
        CapacityGateTransition::Initialize => Some("initialize_capacity_configuration"),
        CapacityGateTransition::Rotate => Some("rotate_capacity_configuration"),
        CapacityGateTransition::Accept | CapacityGateTransition::Reject { .. } => None,
    };
    match transition {
        CapacityGateTransition::Accept => Ok(()),
        CapacityGateTransition::Reject { active, requested } => {
            Err(DistributedAdmissionError::ConfigurationMismatch { active, requested })
        }
        CapacityGateTransition::Initialize | CapacityGateTransition::Rotate => {
            sqlx::query(
                "UPDATE session_weighted_admission_gates
                 SET capacity_hash = ?, updated_at = NOW(6)
                 WHERE scope_name = ?",
            )
            .bind(&requested)
            .bind(DISTRIBUTED_ADMISSION_SCOPE)
            .execute(&mut **tx)
            .await
            .map_err(|source| {
                distributed_database_error(
                    update_operation.expect("capacity update operation"),
                    source,
                )
            })?;
            Ok(())
        }
    }
}

fn admission_capacity_hash(limits: WeightedAdmissionLimits) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DISTRIBUTED_ADMISSION_CAPACITY_DOMAIN);
    for value in [
        limits.global.resident_bytes,
        limits.global.context_tokens,
        u64::from(limits.global.provider_slots),
        limits.global.cpu_units,
        limits.global.io_bytes,
        limits.per_owner.resident_bytes,
        limits.per_owner.context_tokens,
        u64::from(limits.per_owner.provider_slots),
        limits.per_owner.cpu_units,
        limits.per_owner.io_bytes,
    ] {
        hasher.update(value.to_le_bytes());
    }
    format!("{:x}", hasher.finalize())
}

#[derive(Debug, PartialEq, Eq)]
enum CapacityGateTransition {
    Accept,
    Initialize,
    Rotate,
    Reject { active: String, requested: String },
}

/// Decide how a locked gate row may adopt a requested capacity snapshot.
///
/// A NULL hash means that this scope has not initialized its capacity snapshot.
/// It may only be initialized when no reservation is active: an active
/// reservation has unknown provenance and must never be silently mixed with a
/// newly declared budget.
fn capacity_gate_transition(
    active: Option<&str>,
    active_reservations: i64,
    requested: &str,
) -> CapacityGateTransition {
    if active == Some(requested) {
        return CapacityGateTransition::Accept;
    }
    if active_reservations > 0 {
        return CapacityGateTransition::Reject {
            active: active.unwrap_or("unset").to_string(),
            requested: requested.to_string(),
        };
    }
    if active.is_none() {
        CapacityGateTransition::Initialize
    } else {
        CapacityGateTransition::Rotate
    }
}

async fn load_distributed_reservation(
    tx: &mut Transaction<'_, MySql>,
    key: &SessionKeyV1,
    idempotency_hash: &str,
) -> Result<Option<DistributedAdmissionReservation>, DistributedAdmissionError> {
    let row = sqlx::query(
        "SELECT reservation_id, session_id, branch_id, resident_bytes,
                context_tokens, provider_slots, cpu_units, io_bytes, expires_at
         FROM session_weighted_admission_reservations
         WHERE scope_name = ? AND isolation_domain = ? AND owner_user_id = ?
           AND idempotency_hash = ?",
    )
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .bind(&key.isolation_domain)
    .bind(&key.owner_user_id)
    .bind(idempotency_hash)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("load_idempotent_reservation", source))?;
    row.map(|row| {
        let stored_session_id = row
            .try_get::<String, _>("session_id")
            .map_err(|source| distributed_database_error("decode_reservation_session", source))?;
        let stored_branch_id = row
            .try_get::<String, _>("branch_id")
            .map_err(|source| distributed_database_error("decode_reservation_branch", source))?;
        if stored_session_id != key.session_id || stored_branch_id != key.branch_id {
            return Err(DistributedAdmissionError::IdempotencyMismatch);
        }
        let expires_at = row
            .try_get::<chrono::NaiveDateTime, _>("expires_at")
            .map_err(|source| distributed_database_error("decode_reservation_expiry", source))?;
        let work = decode_admission_work(&row)?;
        Ok(DistributedAdmissionReservation {
            reservation_id: row.try_get("reservation_id").map_err(|source| {
                distributed_database_error("decode_reservation_identity", source)
            })?,
            key: key.clone(),
            work,
            expires_at_unix_ms: expires_at.and_utc().timestamp_millis(),
            idempotency_hash: idempotency_hash.to_owned(),
        })
    })
    .transpose()
}

async fn load_distributed_usage(
    tx: &mut Transaction<'_, MySql>,
    owner_user_id: &str,
) -> Result<(AdmissionWork, AdmissionWork), DistributedAdmissionError> {
    // The gate row already serializes this transaction. Aggregate the bounded
    // reservation set in MatrixOne so a thousand-session deployment transfers
    // one row instead of materializing every active reservation into Rust for
    // every admission attempt.
    let row = sqlx::query(
        "SELECT
             CAST(COALESCE(SUM(resident_bytes), 0) AS CHAR) AS global_resident_bytes,
             CAST(COALESCE(SUM(context_tokens), 0) AS CHAR) AS global_context_tokens,
             CAST(COALESCE(SUM(provider_slots), 0) AS CHAR) AS global_provider_slots,
             CAST(COALESCE(SUM(cpu_units), 0) AS CHAR) AS global_cpu_units,
             CAST(COALESCE(SUM(io_bytes), 0) AS CHAR) AS global_io_bytes,
             CAST(COALESCE(SUM(CASE WHEN BINARY owner_user_id = BINARY ? THEN resident_bytes ELSE 0 END), 0) AS CHAR) AS owner_resident_bytes,
             CAST(COALESCE(SUM(CASE WHEN BINARY owner_user_id = BINARY ? THEN context_tokens ELSE 0 END), 0) AS CHAR) AS owner_context_tokens,
             CAST(COALESCE(SUM(CASE WHEN BINARY owner_user_id = BINARY ? THEN provider_slots ELSE 0 END), 0) AS CHAR) AS owner_provider_slots,
             CAST(COALESCE(SUM(CASE WHEN BINARY owner_user_id = BINARY ? THEN cpu_units ELSE 0 END), 0) AS CHAR) AS owner_cpu_units,
             CAST(COALESCE(SUM(CASE WHEN BINARY owner_user_id = BINARY ? THEN io_bytes ELSE 0 END), 0) AS CHAR) AS owner_io_bytes,
             CAST(COALESCE(SUM(CASE
                 WHEN resident_bytes < 0 OR context_tokens < 0 OR provider_slots < 0
                   OR provider_slots > 4294967295 OR cpu_units < 0 OR io_bytes < 0
                 THEN 1 ELSE 0 END), 0) AS SIGNED) AS invalid_rows
         FROM session_weighted_admission_reservations
         WHERE scope_name = ?",
    )
    .bind(owner_user_id)
    .bind(owner_user_id)
    .bind(owner_user_id)
    .bind(owner_user_id)
    .bind(owner_user_id)
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("load_active_usage", source))?;
    let invalid_rows: i64 = row
        .try_get("invalid_rows")
        .map_err(|source| distributed_database_error("decode_usage_invalid_rows", source))?;
    if invalid_rows > 0 {
        return Err(DistributedAdmissionError::Invalid(format!(
            "distributed admission contains {invalid_rows} invalid stored reservation rows"
        )));
    }
    let global = decode_aggregate_admission_work(&row, "global_")?;
    let owner = decode_aggregate_admission_work(&row, "owner_")?;
    Ok((global, owner))
}

async fn insert_distributed_reservation(
    tx: &mut Transaction<'_, MySql>,
    reservation: &DistributedAdmissionReservation,
    expires_at: chrono::NaiveDateTime,
) -> Result<(), DistributedAdmissionError> {
    sqlx::query(
        "INSERT INTO session_weighted_admission_reservations
         (scope_name, reservation_id, isolation_domain, owner_user_id, session_id,
          branch_id, idempotency_hash, resident_bytes, context_tokens,
          provider_slots, cpu_units, io_bytes, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .bind(&reservation.reservation_id)
    .bind(&reservation.key.isolation_domain)
    .bind(&reservation.key.owner_user_id)
    .bind(&reservation.key.session_id)
    .bind(&reservation.key.branch_id)
    .bind(&reservation.idempotency_hash)
    .bind(admission_i64(
        "resident_bytes",
        reservation.work.resident_bytes,
    )?)
    .bind(admission_i64(
        "context_tokens",
        reservation.work.context_tokens,
    )?)
    .bind(i64::from(reservation.work.provider_slots))
    .bind(admission_i64("cpu_units", reservation.work.cpu_units)?)
    .bind(admission_i64("io_bytes", reservation.work.io_bytes)?)
    .bind(expires_at)
    .execute(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("insert_reservation", source))?;
    Ok(())
}

fn decode_admission_work(
    row: &sqlx::mysql::MySqlRow,
) -> Result<AdmissionWork, DistributedAdmissionError> {
    Ok(AdmissionWork {
        resident_bytes: admission_u64(row, "resident_bytes")?,
        context_tokens: admission_u64(row, "context_tokens")?,
        provider_slots: u32::try_from(admission_u64(row, "provider_slots")?).map_err(|_| {
            DistributedAdmissionError::Invalid(
                "stored distributed provider slots exceed u32".into(),
            )
        })?,
        cpu_units: admission_u64(row, "cpu_units")?,
        io_bytes: admission_u64(row, "io_bytes")?,
    })
}

fn decode_aggregate_admission_work(
    row: &sqlx::mysql::MySqlRow,
    prefix: &str,
) -> Result<AdmissionWork, DistributedAdmissionError> {
    Ok(AdmissionWork {
        resident_bytes: aggregate_admission_u64(row, &format!("{prefix}resident_bytes"))?,
        context_tokens: aggregate_admission_u64(row, &format!("{prefix}context_tokens"))?,
        provider_slots: u32::try_from(aggregate_admission_u64(
            row,
            &format!("{prefix}provider_slots"),
        )?)
        .map_err(|_| {
            DistributedAdmissionError::Invalid(
                "aggregated distributed provider slots exceed u32".into(),
            )
        })?,
        cpu_units: aggregate_admission_u64(row, &format!("{prefix}cpu_units"))?,
        io_bytes: aggregate_admission_u64(row, &format!("{prefix}io_bytes"))?,
    })
}

fn aggregate_admission_u64(
    row: &sqlx::mysql::MySqlRow,
    column: &str,
) -> Result<u64, DistributedAdmissionError> {
    let value: String = row
        .try_get(column)
        .map_err(|source| distributed_database_error("decode_usage", source))?;
    value.parse::<u64>().map_err(|_| {
        DistributedAdmissionError::Invalid(format!(
            "aggregated distributed admission {column} is outside u64"
        ))
    })
}

fn admission_u64(
    row: &sqlx::mysql::MySqlRow,
    column: &str,
) -> Result<u64, DistributedAdmissionError> {
    let value = row
        .try_get::<i64, _>(column)
        .map_err(|source| distributed_database_error("decode_usage", source))?;
    u64::try_from(value).map_err(|_| {
        DistributedAdmissionError::Invalid(format!(
            "stored distributed admission {column} is negative"
        ))
    })
}

fn admission_i64(field: &'static str, value: u64) -> Result<i64, DistributedAdmissionError> {
    i64::try_from(value).map_err(|_| {
        DistributedAdmissionError::Invalid(format!("distributed admission {field} exceeds BIGINT"))
    })
}

fn distributed_idempotency_hash(idempotency_key: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(DISTRIBUTED_ADMISSION_IDEMPOTENCY_DOMAIN);
    digest.update((idempotency_key.len() as u64).to_be_bytes());
    digest.update(idempotency_key.as_bytes());
    format!("{:x}", digest.finalize())
}

fn distributed_database_error(
    operation: &'static str,
    source: sqlx::Error,
) -> DistributedAdmissionError {
    DistributedAdmissionError::Database { operation, source }
}

#[derive(Default)]
struct AdmissionState {
    global_used: AdmissionWork,
    owner_used: HashMap<String, AdmissionWork>,
}

struct WeightedAdmissionInner {
    limits: WeightedAdmissionLimits,
    state: Mutex<AdmissionState>,
    released: Notify,
}

#[derive(Clone)]
pub struct WeightedAdmissionController {
    inner: Arc<WeightedAdmissionInner>,
}

impl WeightedAdmissionController {
    pub fn new(limits: WeightedAdmissionLimits) -> Result<Self, WeightedAdmissionError> {
        if !limits.per_owner.fits_within(limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        Ok(Self {
            inner: Arc::new(WeightedAdmissionInner {
                limits,
                state: Mutex::new(AdmissionState::default()),
                released: Notify::new(),
            }),
        })
    }

    pub fn try_admit(
        &self,
        owner_user_id: impl Into<String>,
        work: AdmissionWork,
    ) -> Result<WeightedAdmissionPermit, WeightedAdmissionError> {
        if !work.fits_within(self.inner.limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        if !work.fits_within(self.inner.limits.per_owner) {
            return Err(WeightedAdmissionError::RequestExceedsOwner);
        }
        let owner_user_id = owner_user_id.into();
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let global_next = state
            .global_used
            .checked_add(work)
            .ok_or(WeightedAdmissionError::GlobalExhausted)?;
        if !global_next.fits_within(self.inner.limits.global) {
            return Err(WeightedAdmissionError::GlobalExhausted);
        }
        let owner_next = state
            .owner_used
            .get(&owner_user_id)
            .copied()
            .unwrap_or_default()
            .checked_add(work)
            .ok_or(WeightedAdmissionError::OwnerExhausted)?;
        if !owner_next.fits_within(self.inner.limits.per_owner) {
            return Err(WeightedAdmissionError::OwnerExhausted);
        }
        state.global_used = global_next;
        state.owner_used.insert(owner_user_id.clone(), owner_next);
        Ok(WeightedAdmissionPermit {
            inner: Arc::clone(&self.inner),
            owner_user_id,
            work,
            released: false,
        })
    }

    pub async fn admit_until(
        &self,
        owner_user_id: impl Into<String>,
        work: AdmissionWork,
        deadline: tokio::time::Instant,
    ) -> Result<WeightedAdmissionPermit, WeightedAdmissionError> {
        let owner_user_id = owner_user_id.into();
        loop {
            let notified = self.inner.released.notified();
            match self.try_admit(owner_user_id.clone(), work) {
                Ok(permit) => return Ok(permit),
                Err(
                    error @ (WeightedAdmissionError::RequestExceedsGlobal
                    | WeightedAdmissionError::RequestExceedsOwner),
                ) => return Err(error),
                Err(error) => {
                    if tokio::time::timeout_at(deadline, notified).await.is_err() {
                        return Err(error);
                    }
                }
            }
        }
    }

    pub fn usage(&self, owner_user_id: &str) -> (AdmissionWork, AdmissionWork) {
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        (
            state.global_used,
            state
                .owner_used
                .get(owner_user_id)
                .copied()
                .unwrap_or_default(),
        )
    }
}

pub struct WeightedAdmissionPermit {
    inner: Arc<WeightedAdmissionInner>,
    owner_user_id: String,
    work: AdmissionWork,
    released: bool,
}

impl Drop for WeightedAdmissionPermit {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.global_used = state
            .global_used
            .checked_sub(self.work)
            .expect("admission permit cannot release more than it acquired");
        let remove_owner = {
            let owner = state
                .owner_used
                .get_mut(&self.owner_user_id)
                .expect("admitted owner usage must exist");
            *owner = owner
                .checked_sub(self.work)
                .expect("owner permit cannot release more than it acquired");
            *owner == AdmissionWork::default()
        };
        if remove_owner {
            state.owner_used.remove(&self.owner_user_id);
        }
        drop(state);
        self.inner.released.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> WeightedAdmissionLimits {
        WeightedAdmissionLimits {
            global: AdmissionWork {
                resident_bytes: 1_000,
                context_tokens: 1_000,
                provider_slots: 10,
                cpu_units: 1_000,
                io_bytes: 1_000,
            },
            per_owner: AdmissionWork {
                resident_bytes: 750,
                context_tokens: 750,
                provider_slots: 8,
                cpu_units: 750,
                io_bytes: 750,
            },
        }
    }

    #[test]
    fn noisy_owner_cannot_consume_another_owners_reserved_share() {
        let controller = WeightedAdmissionController::new(limits()).unwrap();
        let noisy = controller
            .try_admit(
                "owner-a",
                AdmissionWork {
                    resident_bytes: 750,
                    context_tokens: 750,
                    provider_slots: 8,
                    cpu_units: 750,
                    io_bytes: 750,
                },
            )
            .unwrap();
        assert!(matches!(
            controller.try_admit(
                "owner-a",
                AdmissionWork {
                    resident_bytes: 1,
                    context_tokens: 1,
                    provider_slots: 1,
                    cpu_units: 1,
                    io_bytes: 1,
                }
            ),
            Err(WeightedAdmissionError::OwnerExhausted)
        ));
        let other = controller
            .try_admit(
                "owner-b",
                AdmissionWork {
                    resident_bytes: 250,
                    context_tokens: 250,
                    provider_slots: 2,
                    cpu_units: 250,
                    io_bytes: 250,
                },
            )
            .expect("another owner retains its configured share");
        drop((noisy, other));
        assert_eq!(
            controller.usage("owner-a"),
            (AdmissionWork::default(), AdmissionWork::default())
        );
    }

    #[test]
    fn large_context_consumes_proportional_units() {
        let controller = WeightedAdmissionController::new(limits()).unwrap();
        let _small = controller
            .try_admit(
                "owner-a",
                AdmissionWork {
                    context_tokens: 100,
                    resident_bytes: 100,
                    provider_slots: 1,
                    cpu_units: 100,
                    io_bytes: 100,
                },
            )
            .unwrap();
        assert!(matches!(
            controller.try_admit(
                "owner-a",
                AdmissionWork {
                    context_tokens: 700,
                    resident_bytes: 700,
                    provider_slots: 1,
                    cpu_units: 700,
                    io_bytes: 700,
                }
            ),
            Err(WeightedAdmissionError::OwnerExhausted)
        ));
    }

    #[test]
    fn capacity_hash_is_stable_and_changes_when_any_budget_changes() {
        let baseline = admission_capacity_hash(limits());
        assert_eq!(baseline, admission_capacity_hash(limits()));

        let mut changed = limits();
        changed.global.provider_slots += 1;
        assert_ne!(baseline, admission_capacity_hash(changed));

        let mut changed = limits();
        changed.per_owner.context_tokens += 1;
        assert_ne!(baseline, admission_capacity_hash(changed));
    }

    #[test]
    fn capacity_gate_rejects_unknown_or_changed_budget_while_reservations_are_active() {
        assert_eq!(
            capacity_gate_transition(None, 1, "requested"),
            CapacityGateTransition::Reject {
                active: "unset".into(),
                requested: "requested".into(),
            }
        );
        assert_eq!(
            capacity_gate_transition(Some("active"), 1, "requested"),
            CapacityGateTransition::Reject {
                active: "active".into(),
                requested: "requested".into(),
            }
        );
    }

    #[test]
    fn capacity_gate_initializes_or_rotates_only_after_reservations_drain() {
        assert_eq!(
            capacity_gate_transition(None, 0, "requested"),
            CapacityGateTransition::Initialize
        );
        assert_eq!(
            capacity_gate_transition(Some("active"), 0, "requested"),
            CapacityGateTransition::Rotate
        );
        assert_eq!(
            capacity_gate_transition(Some("requested"), 999, "requested"),
            CapacityGateTransition::Accept
        );
    }
}
