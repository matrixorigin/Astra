//! Byte/token-weighted multi-tenant admission.
//!
//! Billing quotas remain separate. This controller protects resident memory,
//! provider concurrency, CPU-heavy serialization/compaction, and canonical
//! I/O using deterministic work estimates.

use std::{
    collections::HashMap,
    ops::{Deref, DerefMut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use astra_core::{SharedPool, is_duplicate_key_error};
use astra_turn_types::SessionKeyV1;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{
    MySql, MySqlConnection, Row, TransactionManager, mysql::MySqlTransactionManager,
    pool::PoolConnection,
};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore};
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
    #[error("distributed admission wait exceeded {timeout_ms} ms")]
    AdmissionTimeout { timeout_ms: u64 },
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

macro_rules! admission_io {
    ($tx:expr, $future:expr) => {{
        $tx.mark_io_in_flight();
        let result = $future.await;
        $tx.mark_io_complete();
        result
    }};
}

/// A transaction backed by a checked-out pool connection.
///
/// SQLx's `Transaction` drop handler only queues `ROLLBACK`; if a cancelled
/// MySQL future is still waiting for a row lock, returning that connection to
/// the pool can keep the pool slot occupied until the old response arrives.
/// This guard lets timeout paths mark the physical connection for close while
/// preserving normal rollback-and-reuse for ordinary capacity errors.
struct AdmissionTransaction {
    connection: Option<PoolConnection<MySql>>,
    transaction_open: bool,
    discard_on_drop: bool,
}

impl AdmissionTransaction {
    async fn begin(
        pool: &SharedPool,
        deadline: std::time::Instant,
        operation: &'static str,
        timeout: Duration,
    ) -> Result<Self, DistributedAdmissionError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let connection = tokio::time::timeout(remaining, pool.get().acquire())
            .await
            .map_err(|_| admission_timeout_error(timeout))?
            .map_err(|source| distributed_database_error(operation, source))?;
        let mut transaction = Self {
            connection: Some(connection),
            transaction_open: false,
            discard_on_drop: true,
        };
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(
            remaining,
            MySqlTransactionManager::begin(&mut *transaction.connection_mut(), None),
        )
        .await
        {
            Ok(Ok(())) => {
                transaction.transaction_open = true;
                transaction.discard_on_drop = false;
                Ok(transaction)
            }
            Ok(Err(source)) => Err(distributed_database_error(operation, source)),
            Err(_) => Err(admission_timeout_error(timeout)),
        }
    }

    fn connection_mut(&mut self) -> &mut MySqlConnection {
        self.connection
            .as_deref_mut()
            .expect("admission transaction connection already released")
    }

    fn discard_on_drop(&mut self) {
        self.discard_on_drop = true;
        if let Some(connection) = self.connection.as_mut() {
            connection.close_on_drop();
        }
    }

    fn mark_io_in_flight(&mut self) {
        self.discard_on_drop = true;
    }

    fn mark_io_complete(&mut self) {
        self.discard_on_drop = false;
    }

    async fn commit(mut self) -> Result<(), sqlx::Error> {
        if self.transaction_open {
            self.mark_io_in_flight();
            let result = MySqlTransactionManager::commit(self.connection_mut()).await;
            match result {
                Ok(()) => {
                    self.transaction_open = false;
                    self.mark_io_complete();
                }
                Err(error) => return Err(error),
            }
        }
        self.discard_on_drop = false;
        self.connection.take();
        Ok(())
    }
}

impl Deref for AdmissionTransaction {
    type Target = MySqlConnection;

    fn deref(&self) -> &Self::Target {
        self.connection
            .as_deref()
            .expect("admission transaction connection already released")
    }
}

impl DerefMut for AdmissionTransaction {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.connection_mut()
    }
}

impl Drop for AdmissionTransaction {
    fn drop(&mut self) {
        let Some(connection) = self.connection.as_mut() else {
            return;
        };
        if self.discard_on_drop {
            connection.close_on_drop();
        } else if self.transaction_open {
            // This mirrors SQLx's normal Transaction drop behavior. It is only
            // used after a query future has completed, so the protocol is
            // synchronized and the connection can safely return to the pool.
            MySqlTransactionManager::start_rollback(connection);
        }
    }
}

#[derive(Clone)]
pub struct DatabaseWeightedAdmissionController {
    pool: SharedPool,
    limits: WeightedAdmissionLimits,
    /// A durable scope has one gate row, so concurrent callers in this
    /// process would only queue on the same database lock after already
    /// consuming a pool connection. Keep that queue in Tokio instead. This
    /// preserves the durable gate as the cross-process authority while
    /// preventing a burst of idle requests from exhausting every connection
    /// and timing out before the gate can run.
    reservation_gate: Arc<Semaphore>,
    /// One deadline covers both the in-process gate queue and the subsequent
    /// pool acquire. A request that cannot reach the durable gate in this
    /// budget fails explicitly instead of occupying an unbounded waiter.
    admission_wait_timeout: Duration,
}

impl DatabaseWeightedAdmissionController {
    pub fn new(
        pool: SharedPool,
        limits: WeightedAdmissionLimits,
    ) -> Result<Self, WeightedAdmissionError> {
        if !limits.per_owner.fits_within(limits.global) {
            return Err(WeightedAdmissionError::RequestExceedsGlobal);
        }
        let admission_wait_timeout =
            Duration::from_secs(pool.settings().db_pool_acquire_timeout_secs);
        Ok(Self {
            pool,
            limits,
            // One local permit mirrors the single durable gate row. Waiting
            // callers stay in Tokio instead of holding a database connection
            // while queued behind the same cross-process lock.
            reservation_gate: Arc::new(Semaphore::new(1)),
            admission_wait_timeout,
        })
    }

    /// Set the end-to-end wait budget for local gate and database-pool
    /// admission. Runtime composition uses the existing run admission budget;
    /// direct service users inherit the configured pool acquire timeout.
    pub fn with_admission_wait_timeout(&mut self, timeout: Duration) {
        self.admission_wait_timeout = timeout;
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

        // The deadline covers the whole admission transaction, not only the
        // queue, pool checkout, BEGIN, and durable gate lock. Dropping a
        // timed-out transaction closes its connection while a cancelled SQL
        // future is still in flight, so a slow repair/insert/commit cannot
        // silently extend the caller's admission budget.
        let deadline = self.admission_deadline();
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.try_reserve_until(key, work, ttl, idempotency_key, deadline),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(self.admission_timeout()),
        }
    }

    async fn try_reserve_until(
        &self,
        key: &SessionKeyV1,
        work: AdmissionWork,
        ttl: Duration,
        idempotency_key: &str,
        deadline: std::time::Instant,
    ) -> Result<DistributedAdmissionPermit, DistributedAdmissionError> {
        // Do not acquire a SQL connection while waiting for the single
        // durable gate row. All callers sharing this controller use the same
        // FIFO async queue; independent server processes still contend only
        // on the durable row and retain the same global invariant. The same
        // deadline is then applied to pool acquisition, so queueing cannot
        // silently extend the database wait budget.
        let _reservation_gate = self.acquire_reservation_gate(deadline).await?;
        let idempotency_hash = distributed_idempotency_hash(idempotency_key);
        let mut tx = self
            .begin_transaction(deadline, "begin_reservation")
            .await?;
        let mut gate = self.lock_admission_gate(&mut tx, deadline).await?;
        let cleanup_result = admission_io!(
            tx,
            sqlx::query(
                "DELETE FROM session_weighted_admission_reservations
                 WHERE scope_name = ? AND expires_at <= NOW(6)",
            )
            .bind(DISTRIBUTED_ADMISSION_SCOPE)
            .execute(&mut *tx)
        )
        .map_err(|source| distributed_database_error("cleanup_expired", source))?;
        if cleanup_result.rows_affected() != 0 {
            mark_materialized_usage_dirty(&mut tx).await?;
            gate.usage_initialized = false;
        }
        ensure_distributed_admission_capacity(&mut tx, self.limits, gate.capacity_hash.as_deref())
            .await?;

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
        // Repair materialized totals before inserting the provisional row so
        // the repair aggregate cannot count this request twice.
        ensure_materialized_admission_usage(&mut tx, &mut gate).await?;
        // The idempotency index is the durable fast path for new requests.
        // Attempt the insert before reading usage so the common path avoids a
        // second indexed SELECT. A duplicate is then resolved from the same
        // transaction and replayed without consuming capacity again; a
        // capacity rejection rolls the provisional insert back with the
        // transaction.
        match insert_distributed_reservation(&mut tx, &reservation, expires_at).await {
            Ok(()) => {}
            Err(DistributedAdmissionError::Database { operation, source })
                if is_duplicate_key_error(&source) =>
            {
                let Some(existing) =
                    load_distributed_reservation(&mut tx, key, &reservation.idempotency_hash)
                        .await?
                else {
                    return Err(DistributedAdmissionError::Database { operation, source });
                };
                if existing.work != work {
                    return Err(DistributedAdmissionError::IdempotencyMismatch);
                }
                tx.commit()
                    .await
                    .map_err(|source| distributed_database_error("commit_replay", source))?;
                return Ok(DistributedAdmissionPermit::new(self.clone(), existing));
            }
            Err(error) => return Err(error),
        }

        let owner_used = load_materialized_owner_usage(&mut tx, key).await?;
        let global_used = gate.global_used;
        validate_available_work(self.limits, global_used, owner_used, work)?;
        add_materialized_admission_usage(&mut tx, key, work).await?;
        gate.global_used =
            gate.global_used
                .checked_add(work)
                .ok_or(DistributedAdmissionError::Capacity(
                    WeightedAdmissionError::GlobalExhausted,
                ))?;
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
        let deadline = self.admission_deadline();
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.renew_until(reservation, ttl, deadline),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(self.admission_timeout()),
        }
    }

    async fn renew_until(
        &self,
        reservation: &DistributedAdmissionReservation,
        ttl: Duration,
        deadline: std::time::Instant,
    ) -> Result<DistributedAdmissionReservation, DistributedAdmissionError> {
        let _reservation_gate = self.acquire_reservation_gate(deadline).await?;
        let mut tx = self.begin_transaction(deadline, "begin_renewal").await?;
        let now = self.lock_admission_gate(&mut tx, deadline).await?.now;
        let expires_at = now
            .checked_add_signed(chrono::Duration::from_std(ttl).map_err(|_| {
                DistributedAdmissionError::Invalid("admission TTL is outside clock range".into())
            })?)
            .ok_or_else(|| {
                DistributedAdmissionError::Invalid("admission expiry overflows clock".into())
            })?;
        let result = admission_io!(
            tx,
            sqlx::query(
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
        )
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
        let deadline = self.admission_deadline();
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(deadline),
            self.release_until(reservation, deadline),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(self.admission_timeout()),
        }
    }

    async fn release_until(
        &self,
        reservation: &DistributedAdmissionReservation,
        deadline: std::time::Instant,
    ) -> Result<(), DistributedAdmissionError> {
        let _reservation_gate = self.acquire_reservation_gate(deadline).await?;
        let mut tx = self.begin_transaction(deadline, "begin_release").await?;
        let gate = self.lock_admission_gate(&mut tx, deadline).await?;
        let deleted = admission_io!(
            tx,
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
        )
        .map_err(|source| distributed_database_error("release_reservation", source))?;
        if deleted.rows_affected() == 1 {
            if gate.usage_initialized {
                subtract_materialized_admission_usage(&mut tx, &reservation.key, reservation.work)
                    .await?;
            } else {
                // A dirty gate is rebuilt from the reservation rows on the
                // next admission; no counter update is needed for this
                // release because the rows are already the source of truth.
            }
        }
        tx.commit()
            .await
            .map_err(|source| distributed_database_error("commit_release", source))?;
        Ok(())
    }

    fn admission_deadline(&self) -> std::time::Instant {
        std::time::Instant::now()
            .checked_add(self.admission_wait_timeout)
            .unwrap_or_else(std::time::Instant::now)
    }

    async fn acquire_reservation_gate(
        &self,
        deadline: std::time::Instant,
    ) -> Result<OwnedSemaphorePermit, DistributedAdmissionError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tokio::time::timeout(remaining, self.reservation_gate.clone().acquire_owned())
            .await
            .map_err(|_| self.admission_timeout())?
            .map_err(|_| {
                DistributedAdmissionError::Invalid("distributed admission gate was closed".into())
            })
    }

    async fn begin_transaction(
        &self,
        deadline: std::time::Instant,
        operation: &'static str,
    ) -> Result<AdmissionTransaction, DistributedAdmissionError> {
        AdmissionTransaction::begin(&self.pool, deadline, operation, self.admission_wait_timeout)
            .await
    }

    async fn lock_admission_gate(
        &self,
        tx: &mut AdmissionTransaction,
        deadline: std::time::Instant,
    ) -> Result<LockedDistributedAdmissionGate, DistributedAdmissionError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tx.mark_io_in_flight();
        match tokio::time::timeout(remaining, lock_distributed_admission_gate(tx)).await {
            Ok(result) => {
                tx.mark_io_complete();
                result
            }
            Err(_) => {
                tx.discard_on_drop();
                Err(self.admission_timeout())
            }
        }
    }

    fn admission_timeout(&self) -> DistributedAdmissionError {
        DistributedAdmissionError::AdmissionTimeout {
            timeout_ms: self
                .admission_wait_timeout
                .as_millis()
                .min(u128::from(u64::MAX)) as u64,
        }
    }
}

struct DistributedAdmissionReleaseState {
    completed: AtomicBool,
    attempt_lock: AsyncMutex<()>,
}

impl DistributedAdmissionReleaseState {
    fn new() -> Self {
        Self {
            completed: AtomicBool::new(false),
            attempt_lock: AsyncMutex::new(()),
        }
    }
}

pub struct DistributedAdmissionPermit {
    controller: DatabaseWeightedAdmissionController,
    reservation: DistributedAdmissionReservation,
    release_state: Arc<DistributedAdmissionReleaseState>,
}

impl DistributedAdmissionPermit {
    fn new(
        controller: DatabaseWeightedAdmissionController,
        reservation: DistributedAdmissionReservation,
    ) -> Self {
        Self {
            controller,
            reservation,
            release_state: Arc::new(DistributedAdmissionReleaseState::new()),
        }
    }

    pub fn reservation(&self) -> &DistributedAdmissionReservation {
        &self.reservation
    }

    pub async fn release(&self) -> Result<(), DistributedAdmissionError> {
        if self.release_state.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        let _attempt = self.release_state.attempt_lock.lock().await;
        if self.release_state.completed.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = self.controller.release(&self.reservation).await;
        if result.is_ok() {
            self.release_state.completed.store(true, Ordering::Release);
        }
        result
    }
}

impl Drop for DistributedAdmissionPermit {
    fn drop(&mut self) {
        if self.release_state.completed.load(Ordering::Acquire) {
            return;
        }
        let controller = self.controller.clone();
        let reservation = self.reservation.clone();
        let release_state = Arc::clone(&self.release_state);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _attempt = release_state.attempt_lock.lock().await;
                if release_state.completed.load(Ordering::Acquire) {
                    return;
                }
                match controller.release(&reservation).await {
                    Ok(()) => {
                        release_state.completed.store(true, Ordering::Release);
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "astra_services::weighted_admission",
                            error = %error,
                            "failed to release distributed weighted admission; retry or TTL cleanup will reclaim it"
                        );
                    }
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
    usage_initialized: bool,
    global_used: AdmissionWork,
}

async fn lock_distributed_admission_gate(
    tx: &mut AdmissionTransaction,
) -> Result<LockedDistributedAdmissionGate, DistributedAdmissionError> {
    let row = sqlx::query(
        "SELECT capacity_hash, usage_initialized,
                CAST(global_resident_bytes AS CHAR) AS global_resident_bytes,
                CAST(global_context_tokens AS CHAR) AS global_context_tokens,
                CAST(global_provider_slots AS CHAR) AS global_provider_slots,
                CAST(global_cpu_units AS CHAR) AS global_cpu_units,
                CAST(global_io_bytes AS CHAR) AS global_io_bytes,
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
    let usage_initialized = row
        .try_get::<i64, _>("usage_initialized")
        .map_err(|source| distributed_database_error("decode_gate_usage_initialized", source))?
        != 0;
    let global_used = AdmissionWork {
        resident_bytes: aggregate_admission_u64(&row, "global_resident_bytes")?,
        context_tokens: aggregate_admission_u64(&row, "global_context_tokens")?,
        provider_slots: u32::try_from(aggregate_admission_u64(&row, "global_provider_slots")?)
            .map_err(|_| {
                DistributedAdmissionError::Invalid(
                    "materialized distributed provider slots exceed u32".into(),
                )
            })?,
        cpu_units: aggregate_admission_u64(&row, "global_cpu_units")?,
        io_bytes: aggregate_admission_u64(&row, "global_io_bytes")?,
    };
    Ok(LockedDistributedAdmissionGate {
        now,
        capacity_hash,
        usage_initialized,
        global_used,
    })
}

/// Bind the durable admission scope to one capacity configuration.
///
/// Every server sharing this scope must use the same capacity snapshot. The
/// first server records the snapshot; later servers fail closed while
/// reservations are active and may adopt a new snapshot only after the active
/// reservations have drained. A NULL hash is the uninitialized state of the
/// current capacity protocol.
async fn ensure_distributed_admission_capacity(
    tx: &mut AdmissionTransaction,
    limits: WeightedAdmissionLimits,
    active: Option<&str>,
) -> Result<(), DistributedAdmissionError> {
    let requested = admission_capacity_hash(limits);
    if active == Some(requested.as_str()) {
        return Ok(());
    }
    let active_reservations: i64 = admission_io!(
        tx,
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM session_weighted_admission_reservations
             WHERE scope_name = ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .fetch_one(&mut **tx)
    )
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
            admission_io!(
                tx,
                sqlx::query(
                    "UPDATE session_weighted_admission_gates
                     SET capacity_hash = ?, updated_at = NOW(6)
                     WHERE scope_name = ?",
                )
                .bind(&requested)
                .bind(DISTRIBUTED_ADMISSION_SCOPE)
                .execute(&mut **tx)
            )
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
    tx: &mut AdmissionTransaction,
    key: &SessionKeyV1,
    idempotency_hash: &str,
) -> Result<Option<DistributedAdmissionReservation>, DistributedAdmissionError> {
    let row = admission_io!(
        tx,
        sqlx::query(
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
    )
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

/// Mark the materialized totals dirty after a mutation that does not update
/// them in the same code path (for example expiry cleanup or session delete).
/// The next admission rebuilds the totals while holding the durable gate.
async fn mark_materialized_usage_dirty(
    tx: &mut AdmissionTransaction,
) -> Result<(), DistributedAdmissionError> {
    admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_gates
             SET usage_initialized = 0, updated_at = NOW(6)
             WHERE scope_name = ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("mark_usage_dirty", source))?;
    Ok(())
}

/// Rebuild O(1) global and per-owner totals from reservation rows. This is a
/// repair path, used once after schema upgrade or after an out-of-band delete;
/// normal reserve/release paths update the totals incrementally.
async fn ensure_materialized_admission_usage(
    tx: &mut AdmissionTransaction,
    gate: &mut LockedDistributedAdmissionGate,
) -> Result<(), DistributedAdmissionError> {
    if gate.usage_initialized {
        return Ok(());
    }

    let row = admission_io!(
        tx,
        sqlx::query(
            "SELECT
                 CAST(COALESCE(SUM(resident_bytes), 0) AS CHAR) AS global_resident_bytes,
                 CAST(COALESCE(SUM(context_tokens), 0) AS CHAR) AS global_context_tokens,
                 CAST(COALESCE(SUM(provider_slots), 0) AS CHAR) AS global_provider_slots,
                 CAST(COALESCE(SUM(cpu_units), 0) AS CHAR) AS global_cpu_units,
                 CAST(COALESCE(SUM(io_bytes), 0) AS CHAR) AS global_io_bytes,
                 CAST(COALESCE(SUM(CASE
                     WHEN resident_bytes < 0 OR context_tokens < 0 OR provider_slots < 0
                       OR provider_slots > 4294967295 OR cpu_units < 0 OR io_bytes < 0
                     THEN 1 ELSE 0 END), 0) AS SIGNED) AS invalid_rows
             FROM session_weighted_admission_reservations
             WHERE scope_name = ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .fetch_one(&mut **tx)
    )
    .map_err(|source| distributed_database_error("rebuild_active_usage", source))?;
    let invalid_rows: i64 = row
        .try_get("invalid_rows")
        .map_err(|source| distributed_database_error("decode_usage_invalid_rows", source))?;
    if invalid_rows > 0 {
        return Err(DistributedAdmissionError::Invalid(format!(
            "distributed admission contains {invalid_rows} invalid stored reservation rows"
        )));
    }
    let global = decode_aggregate_admission_work(&row, "global_")?;

    admission_io!(
        tx,
        sqlx::query(
            "DELETE FROM session_weighted_admission_owner_usage
             WHERE scope_name = ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("clear_owner_usage", source))?;
    admission_io!(
        tx,
        sqlx::query(
            "INSERT INTO session_weighted_admission_owner_usage
             (scope_name, isolation_domain, owner_user_id, resident_bytes,
              context_tokens, provider_slots, cpu_units, io_bytes)
             SELECT scope_name, isolation_domain, owner_user_id,
                    COALESCE(SUM(resident_bytes), 0),
                    COALESCE(SUM(context_tokens), 0),
                    COALESCE(SUM(provider_slots), 0),
                    COALESCE(SUM(cpu_units), 0),
                    COALESCE(SUM(io_bytes), 0)
             FROM session_weighted_admission_reservations
             WHERE scope_name = ?
             GROUP BY scope_name, isolation_domain, owner_user_id",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("rebuild_owner_usage", source))?;

    admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_gates
             SET usage_initialized = 1,
                 global_resident_bytes = ?,
                 global_context_tokens = ?,
                 global_provider_slots = ?,
                 global_cpu_units = ?,
                 global_io_bytes = ?,
                 updated_at = NOW(6)
             WHERE scope_name = ?",
        )
        .bind(global.resident_bytes.to_string())
        .bind(global.context_tokens.to_string())
        .bind(u64::from(global.provider_slots).to_string())
        .bind(global.cpu_units.to_string())
        .bind(global.io_bytes.to_string())
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("publish_materialized_usage", source))?;
    gate.usage_initialized = true;
    gate.global_used = global;
    Ok(())
}

async fn load_materialized_owner_usage(
    tx: &mut AdmissionTransaction,
    key: &SessionKeyV1,
) -> Result<AdmissionWork, DistributedAdmissionError> {
    let row = admission_io!(
        tx,
        sqlx::query(
            "SELECT CAST(resident_bytes AS CHAR) AS owner_resident_bytes,
                    CAST(context_tokens AS CHAR) AS owner_context_tokens,
                    CAST(provider_slots AS CHAR) AS owner_provider_slots,
                    CAST(cpu_units AS CHAR) AS owner_cpu_units,
                    CAST(io_bytes AS CHAR) AS owner_io_bytes
             FROM session_weighted_admission_owner_usage
             WHERE scope_name = ? AND isolation_domain = ?
               AND BINARY owner_user_id = BINARY ?",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .fetch_optional(&mut **tx)
    )
    .map_err(|source| distributed_database_error("load_owner_usage", source))?;
    row.map(|row| decode_aggregate_admission_work(&row, "owner_"))
        .transpose()
        .map(|usage| usage.unwrap_or_default())
}

async fn add_materialized_admission_usage(
    tx: &mut AdmissionTransaction,
    key: &SessionKeyV1,
    work: AdmissionWork,
) -> Result<(), DistributedAdmissionError> {
    let resident_bytes = admission_i64("resident_bytes", work.resident_bytes)?;
    let context_tokens = admission_i64("context_tokens", work.context_tokens)?;
    let provider_slots = i64::from(work.provider_slots);
    let cpu_units = admission_i64("cpu_units", work.cpu_units)?;
    let io_bytes = admission_i64("io_bytes", work.io_bytes)?;
    admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_gates
             SET global_resident_bytes = global_resident_bytes + ?,
                 global_context_tokens = global_context_tokens + ?,
                 global_provider_slots = global_provider_slots + ?,
                 global_cpu_units = global_cpu_units + ?,
                 global_io_bytes = global_io_bytes + ?,
                 updated_at = NOW(6)
             WHERE scope_name = ? AND usage_initialized = 1",
        )
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("increment_global_usage", source))?;
    admission_io!(
        tx,
        sqlx::query(
            "INSERT INTO session_weighted_admission_owner_usage
             (scope_name, isolation_domain, owner_user_id, resident_bytes,
              context_tokens, provider_slots, cpu_units, io_bytes)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON DUPLICATE KEY UPDATE
                 resident_bytes = resident_bytes + VALUES(resident_bytes),
                 context_tokens = context_tokens + VALUES(context_tokens),
                 provider_slots = provider_slots + VALUES(provider_slots),
                 cpu_units = cpu_units + VALUES(cpu_units),
                 io_bytes = io_bytes + VALUES(io_bytes),
                 updated_at = NOW(6)",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("increment_owner_usage", source))?;
    Ok(())
}

async fn subtract_materialized_admission_usage(
    tx: &mut AdmissionTransaction,
    key: &SessionKeyV1,
    work: AdmissionWork,
) -> Result<(), DistributedAdmissionError> {
    let resident_bytes = admission_i64("resident_bytes", work.resident_bytes)?;
    let context_tokens = admission_i64("context_tokens", work.context_tokens)?;
    let provider_slots = i64::from(work.provider_slots);
    let cpu_units = admission_i64("cpu_units", work.cpu_units)?;
    let io_bytes = admission_i64("io_bytes", work.io_bytes)?;
    let global = admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_gates
             SET global_resident_bytes = global_resident_bytes - ?,
                 global_context_tokens = global_context_tokens - ?,
                 global_provider_slots = global_provider_slots - ?,
                 global_cpu_units = global_cpu_units - ?,
                 global_io_bytes = global_io_bytes - ?,
                 updated_at = NOW(6)
             WHERE scope_name = ? AND usage_initialized = 1
               AND global_resident_bytes >= ?
               AND global_context_tokens >= ?
               AND global_provider_slots >= ?
               AND global_cpu_units >= ?
               AND global_io_bytes >= ?",
        )
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("decrement_global_usage", source))?;
    if global.rows_affected() != 1 {
        return Err(DistributedAdmissionError::Invalid(
            "materialized global admission usage underflow".into(),
        ));
    }
    let owner = admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_owner_usage
             SET resident_bytes = resident_bytes - ?,
                 context_tokens = context_tokens - ?,
                 provider_slots = provider_slots - ?,
                 cpu_units = cpu_units - ?,
                 io_bytes = io_bytes - ?,
                 updated_at = NOW(6)
             WHERE scope_name = ? AND isolation_domain = ?
               AND BINARY owner_user_id = BINARY ?
               AND resident_bytes >= ?
               AND context_tokens >= ?
               AND provider_slots >= ?
               AND cpu_units >= ?
               AND io_bytes >= ?",
        )
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("decrement_owner_usage", source))?;
    if owner.rows_affected() != 1 {
        return Err(DistributedAdmissionError::Invalid(
            "materialized owner admission usage underflow".into(),
        ));
    }
    admission_io!(
        tx,
        sqlx::query(
            "DELETE FROM session_weighted_admission_owner_usage
             WHERE scope_name = ? AND isolation_domain = ?
               AND BINARY owner_user_id = BINARY ?
               AND resident_bytes = 0 AND context_tokens = 0
               AND provider_slots = 0 AND cpu_units = 0 AND io_bytes = 0",
        )
        .bind(DISTRIBUTED_ADMISSION_SCOPE)
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("delete_empty_owner_usage", source))?;
    Ok(())
}

async fn insert_distributed_reservation(
    tx: &mut AdmissionTransaction,
    reservation: &DistributedAdmissionReservation,
    expires_at: chrono::NaiveDateTime,
) -> Result<(), DistributedAdmissionError> {
    admission_io!(
        tx,
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
    )
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

fn admission_timeout_error(timeout: Duration) -> DistributedAdmissionError {
    DistributedAdmissionError::AdmissionTimeout {
        timeout_ms: timeout.as_millis().min(u128::from(u64::MAX)) as u64,
    }
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
