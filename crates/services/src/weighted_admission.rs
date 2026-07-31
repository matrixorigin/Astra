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

const DISTRIBUTED_ADMISSION_SCOPE: &str = "canonical_turn_v1";
const DISTRIBUTED_ADMISSION_MAX_TTL: Duration = Duration::from_secs(15 * 60);
const DISTRIBUTED_ADMISSION_IDEMPOTENCY_DOMAIN: &[u8] =
    b"astra.distributed-weighted-admission-idempotency.v1\0";

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
        lock_distributed_admission_gate(&mut tx).await?;
        let now = distributed_database_now(&mut tx).await?;
        sqlx::query(
            "DELETE FROM session_weighted_admission_reservations
             WHERE scope_name = ? AND expires_at <= ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(now)
        .execute(&mut *tx)
        .await
        .map_err(|source| distributed_database_error("cleanup_expired", source))?;

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

        let expires_at = now
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
        lock_distributed_admission_gate(&mut tx).await?;
        let now = distributed_database_now(&mut tx).await?;
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
        lock_distributed_admission_gate(&mut tx).await?;
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

async fn lock_distributed_admission_gate(
    tx: &mut Transaction<'_, MySql>,
) -> Result<(), DistributedAdmissionError> {
    sqlx::query("INSERT IGNORE INTO session_weighted_admission_gates (scope_name) VALUES (?)")
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut **tx)
        .await
        .map_err(|source| distributed_database_error("ensure_gate", source))?;
    sqlx::query(
        "SELECT scope_name FROM session_weighted_admission_gates
         WHERE scope_name = ? FOR UPDATE",
    )
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .fetch_one(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("lock_gate", source))?;
    Ok(())
}

async fn distributed_database_now(
    tx: &mut Transaction<'_, MySql>,
) -> Result<chrono::NaiveDateTime, DistributedAdmissionError> {
    sqlx::query("SELECT NOW(6) AS admission_now")
        .fetch_one(&mut **tx)
        .await
        .map_err(|source| distributed_database_error("load_database_time", source))?
        .try_get("admission_now")
        .map_err(|source| distributed_database_error("decode_database_time", source))
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
    let rows = sqlx::query(
        "SELECT owner_user_id, resident_bytes, context_tokens, provider_slots,
                cpu_units, io_bytes
         FROM session_weighted_admission_reservations
         WHERE scope_name = ?",
    )
    .bind(DISTRIBUTED_ADMISSION_SCOPE)
    .fetch_all(&mut **tx)
    .await
    .map_err(|source| distributed_database_error("load_active_usage", source))?;
    let mut global = AdmissionWork::default();
    let mut owner = AdmissionWork::default();
    for row in rows {
        let work = decode_admission_work(&row)?;
        global = global.checked_add(work).ok_or_else(|| {
            DistributedAdmissionError::Invalid("global admission usage overflow".into())
        })?;
        if row
            .try_get::<String, _>("owner_user_id")
            .map_err(|source| distributed_database_error("decode_usage_owner", source))?
            == owner_user_id
        {
            owner = owner.checked_add(work).ok_or_else(|| {
                DistributedAdmissionError::Invalid("owner admission usage overflow".into())
            })?;
        }
    }
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

fn admission_u64(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
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
}
