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

use crate::CancellationSafeTransaction;
use astra_core::SharedPool;
use astra_turn_types::SessionKeyV1;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, Notify, OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

pub(crate) const DISTRIBUTED_ADMISSION_PARTITIONS: usize = 16;
pub(crate) fn distributed_admission_partition(owner: &str) -> usize {
    let digest = Sha256::digest(owner.as_bytes());
    usize::from(digest[0]) % DISTRIBUTED_ADMISSION_PARTITIONS
}

pub(crate) fn distributed_admission_scope(partition: usize) -> String {
    format!("canonical_turn_v2_{partition:02}")
}

fn owner_admission_scope(key: &SessionKeyV1) -> String {
    distributed_admission_scope(distributed_admission_partition(&key.owner_user_id))
}
const DISTRIBUTED_ADMISSION_MAX_TTL: Duration = Duration::from_secs(15 * 60);
const DISTRIBUTED_ADMISSION_IDEMPOTENCY_DOMAIN: &[u8] =
    b"astra.distributed-weighted-admission-idempotency.v1\0";
const DISTRIBUTED_ADMISSION_CAPACITY_DOMAIN: &[u8] =
    b"astra.distributed-weighted-admission-capacity.v2\0";
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AdmissionWork {
    pub resident_bytes: u64,
    pub context_tokens: u64,
    pub provider_slots: u32,
    pub cpu_units: u64,
    pub io_bytes: u64,
}

impl AdmissionWork {
    fn components(self) -> [u64; 5] {
        [
            self.resident_bytes,
            self.context_tokens,
            u64::from(self.provider_slots),
            self.cpu_units,
            self.io_bytes,
        ]
    }

    fn from_components(values: [u64; 5]) -> Option<Self> {
        Some(Self {
            resident_bytes: values[0],
            context_tokens: values[1],
            provider_slots: values[2].try_into().ok()?,
            cpu_units: values[3],
            io_bytes: values[4],
        })
    }
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
        if result.is_ok() {
            $tx.mark_io_complete();
        }
        result
    }};
}

#[derive(Clone)]
pub struct DatabaseWeightedAdmissionController {
    pool: SharedPool,
    limits: WeightedAdmissionLimits,
    /// Queue before checking out SQL connections. Unrelated owner partitions
    /// proceed independently without holding a checkout while queued.
    reservation_gates: Arc<[Arc<Semaphore>; DISTRIBUTED_ADMISSION_PARTITIONS]>,
    /// Coalesce refills before probing, without holding owner gates or SQL.
    refill_gate: Arc<Semaphore>,
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
            reservation_gates: Arc::new(std::array::from_fn(|_| Arc::new(Semaphore::new(1)))),
            refill_gate: Arc::new(Semaphore::new(1)),
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
    /// Owner partitions serialize only their short admission transactions;
    /// credit borrowing coordinates the selected durable partitions.
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
        let idempotency_hash = distributed_idempotency_hash(idempotency_key);
        let partition = distributed_admission_partition(&key.owner_user_id);
        let requested_hash = admission_capacity_hash(self.limits);
        let initial = {
            let _owner = self
                .acquire_reservation_gate(&self.reservation_gates[partition], deadline)
                .await?;
            self.attempt_reservation(
                key,
                work,
                ttl,
                &idempotency_hash,
                &requested_hash,
                &[partition],
                deadline,
            )
            .await?
        };
        let needs_credit = match initial {
            ReservationAttempt::Granted(reservation) => {
                return Ok(DistributedAdmissionPermit::new(self.clone(), reservation));
            }
            ReservationAttempt::ConfigurationNeeded => false,
            ReservationAttempt::CreditNeeded => true,
        };
        let _refill = self
            .acquire_reservation_gate(&self.refill_gate, deadline)
            .await?;
        let plan = if needs_credit {
            match self
                .probe_partition_capacity(key, work, &idempotency_hash, &requested_hash, deadline)
                .await?
            {
                CapacityProbe::Exhausted => {
                    return Err(WeightedAdmissionError::GlobalExhausted.into());
                }
                CapacityProbe::Donors(partitions) => partitions,
                CapacityProbe::Uncertain => vec![partition],
            }
        } else {
            vec![partition]
        };
        for partitions in [plan, (0..DISTRIBUTED_ADMISSION_PARTITIONS).collect()] {
            match self
                .attempt_reservation(
                    key,
                    work,
                    ttl,
                    &idempotency_hash,
                    &requested_hash,
                    &partitions,
                    deadline,
                )
                .await?
            {
                ReservationAttempt::Granted(reservation) => {
                    return Ok(DistributedAdmissionPermit::new(self.clone(), reservation));
                }
                ReservationAttempt::ConfigurationNeeded | ReservationAttempt::CreditNeeded => {}
            }
        }
        unreachable!("full-inventory admission returns a terminal decision")
    }

    // One transaction owner for point, selected-donor and full-inventory
    // attempts. A nonterminal result has already rolled back its connection.
    #[allow(clippy::too_many_arguments)]
    async fn attempt_reservation(
        &self,
        key: &SessionKeyV1,
        work: AdmissionWork,
        ttl: Duration,
        idempotency_hash: &str,
        requested_hash: &str,
        partitions: &[usize],
        deadline: std::time::Instant,
    ) -> Result<ReservationAttempt, DistributedAdmissionError> {
        let partition = distributed_admission_partition(&key.owner_user_id);
        let target = partitions
            .iter()
            .position(|index| *index == partition)
            .expect("owner partition is always locked");
        let all_partitions = partitions.len() == DISTRIBUTED_ADMISSION_PARTITIONS;
        let mut tx = self
            .begin_transaction(deadline, "begin_reservation")
            .await?;
        let mut gates = Vec::with_capacity(partitions.len());
        for &index in partitions {
            gates.push(self.lock_admission_gate(&mut tx, index, deadline).await?);
        }
        let now = read_admission_clock(&mut tx).await?;
        if !all_partitions
            && gates
                .iter()
                .any(|gate| gate.capacity_hash.as_deref() != Some(requested_hash))
        {
            tracing::debug!(target: "astra_services::weighted_admission", partition, reason = "configuration", "admission partition escalation");
            tx.rollback()
                .await
                .map_err(|source| distributed_database_error("rollback_escalation", source))?;
            return Ok(ReservationAttempt::ConfigurationNeeded);
        }
        for gate in &mut gates {
            reconcile_expired_reservations(&mut tx, gate, now).await?;
            ensure_materialized_admission_usage(&mut tx, gate).await?;
        }
        if all_partitions {
            ensure_partition_allocations(&mut tx, &mut gates, self.limits, requested_hash).await?;
        } else if gates
            .iter()
            .any(|gate| !gate.used.fits_within(gate.allocated))
        {
            return Err(DistributedAdmissionError::Invalid(
                "partition usage exceeds its allocation".into(),
            ));
        }
        if let Some(existing) = load_distributed_reservation(&mut tx, key, idempotency_hash).await?
        {
            if existing.work != work {
                return Err(DistributedAdmissionError::IdempotencyMismatch);
            }
            tx.commit()
                .await
                .map_err(|source| distributed_database_error("commit_replay", source))?;
            return Ok(ReservationAttempt::Granted(existing));
        }
        let owner_used = load_materialized_owner_usage(&mut tx, key).await?;
        if !owner_used
            .checked_add(work)
            .is_some_and(|next| next.fits_within(self.limits.per_owner))
        {
            tx.commit()
                .await
                .map_err(|source| distributed_database_error("commit_owner_denial", source))?;
            return Err(WeightedAdmissionError::OwnerExhausted.into());
        }
        if !gates[target]
            .used
            .checked_add(work)
            .is_some_and(|next| next.fits_within(gates[target].allocated))
            && !transfer_partition_credit(&mut tx, &mut gates, target, work, self.limits.global)
                .await?
        {
            if all_partitions {
                tx.commit().await.map_err(|source| {
                    distributed_database_error("commit_capacity_denial", source)
                })?;
                return Err(WeightedAdmissionError::GlobalExhausted.into());
            }
            tracing::debug!(target: "astra_services::weighted_admission", partition, reason = "credit", "admission partition escalation");
            tx.rollback()
                .await
                .map_err(|source| distributed_database_error("rollback_escalation", source))?;
            return Ok(ReservationAttempt::CreditNeeded);
        }
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
            idempotency_hash: idempotency_hash.to_owned(),
        };
        insert_distributed_reservation(&mut tx, &reservation, expires_at).await?;
        add_materialized_admission_usage(&mut tx, key, work, expires_at).await?;
        tx.commit()
            .await
            .map_err(|source| distributed_database_error("commit_reservation", source))?;
        tracing::debug!(target: "astra_services::weighted_admission", partition, locked_partitions = gates.len(), "admission reservation committed");
        Ok(ReservationAttempt::Granted(reservation))
    }

    /// One fresh snapshot couples counters, expiry and identity absence.
    /// It may reject or suggest locks, but never authorizes execution.
    async fn probe_partition_capacity(
        &self,
        key: &SessionKeyV1,
        work: AdmissionWork,
        idempotency_hash: &str,
        requested_hash: &str,
        deadline: std::time::Instant,
    ) -> Result<CapacityProbe, DistributedAdmissionError> {
        let mut tx = self
            .begin_transaction(deadline, "begin_capacity_snapshot")
            .await?;
        let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new("SELECT scope_name, ");
        query.push(ADMISSION_GATE_COLUMNS).push(", CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) AS database_now, CAST(EXISTS(SELECT 1 FROM session_weighted_admission_reservations WHERE scope_name = ")
            .push_bind(owner_admission_scope(key)).push(" AND isolation_domain = ").push_bind(&key.isolation_domain)
            .push(" AND owner_user_id = ").push_bind(&key.owner_user_id)
            .push(" AND idempotency_hash = ").push_bind(idempotency_hash)
            .push(") AS SIGNED) AS identity_exists FROM session_weighted_admission_gates WHERE scope_name IN (");
        {
            let mut scopes = query.separated(", ");
            for partition in 0..DISTRIBUTED_ADMISSION_PARTITIONS {
                scopes.push_bind(distributed_admission_scope(partition));
            }
        }
        query.push(") ORDER BY scope_name");
        let rows = admission_io!(tx, query.build().fetch_all(&mut *tx))
            .map_err(|source| distributed_database_error("read_capacity_snapshot", source))?;
        tx.commit()
            .await
            .map_err(|source| distributed_database_error("commit_capacity_snapshot", source))?;
        if rows.len() != DISTRIBUTED_ADMISSION_PARTITIONS {
            return Ok(CapacityProbe::Uncertain);
        }
        let mut gates = Vec::with_capacity(rows.len());
        for (partition, row) in rows.iter().enumerate() {
            let scope: String = row
                .try_get("scope_name")
                .map_err(|source| distributed_database_error("decode_snapshot_scope", source))?;
            let identity_exists: i64 = row
                .try_get("identity_exists")
                .map_err(|source| distributed_database_error("decode_snapshot_identity", source))?;
            let unix_ms: i64 = row
                .try_get("database_now")
                .map_err(|source| distributed_database_error("decode_snapshot_time", source))?;
            let Some(now) =
                chrono::DateTime::from_timestamp_millis(unix_ms).map(|time| time.naive_utc())
            else {
                return Ok(CapacityProbe::Uncertain);
            };
            if scope != distributed_admission_scope(partition) || identity_exists != 0 {
                return Ok(CapacityProbe::Uncertain);
            }
            let Ok(gate) = decode_distributed_admission_gate(row, scope) else {
                return Ok(CapacityProbe::Uncertain);
            };
            if gate.capacity_hash.as_deref() != Some(requested_hash)
                || !gate.usage_initialized
                || !gate.used.fits_within(gate.allocated)
                || gate.earliest_expiry_at.is_some_and(|at| at <= now)
            {
                return Ok(CapacityProbe::Uncertain);
            }
            gates.push(gate);
        }
        let (Ok(allocated), Ok(used)) = (
            sum_admission_work(gates.iter().map(|gate| gate.allocated)),
            sum_admission_work(gates.iter().map(|gate| gate.used)),
        ) else {
            return Ok(CapacityProbe::Uncertain);
        };
        if allocated != self.limits.global {
            return Ok(CapacityProbe::Uncertain);
        }
        if !used
            .checked_add(work)
            .is_some_and(|next| next.fits_within(self.limits.global))
        {
            return Ok(CapacityProbe::Exhausted);
        }
        let Some(free) = gates
            .iter()
            .map(|gate| gate.allocated.checked_sub(gate.used))
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(CapacityProbe::Uncertain);
        };
        Ok(donor_partitions(
            &free,
            distributed_admission_partition(&key.owner_user_id),
            work,
        )
        .map_or(CapacityProbe::Uncertain, CapacityProbe::Donors))
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
        let partition = distributed_admission_partition(&reservation.key.owner_user_id);
        let _reservation_gate = self
            .acquire_reservation_gate(&self.reservation_gates[partition], deadline)
            .await?;
        let mut tx = self.begin_transaction(deadline, "begin_renewal").await?;
        let gate = self
            .lock_admission_gate(&mut tx, partition, deadline)
            .await?;
        let now = read_admission_clock(&mut tx).await?;
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
            .bind(&gate.scope)
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
        // TTL may be shortened. Extending or releasing a lease may leave a
        // stale-low frontier, but never allow it to miss an earlier expiry.
        if gate.usage_initialized && gate.earliest_expiry_at.is_none_or(|at| expires_at < at) {
            store_expiry_frontier(&mut tx, &gate.scope, Some(expires_at)).await?;
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
        let partition = distributed_admission_partition(&reservation.key.owner_user_id);
        let _reservation_gate = self
            .acquire_reservation_gate(&self.reservation_gates[partition], deadline)
            .await?;
        let mut tx = self.begin_transaction(deadline, "begin_release").await?;
        let gate = self
            .lock_admission_gate(&mut tx, partition, deadline)
            .await?;
        let deleted = admission_io!(
            tx,
            sqlx::query(
                "DELETE FROM session_weighted_admission_reservations
                 WHERE scope_name = ? AND reservation_id = ?
                   AND isolation_domain = ? AND owner_user_id = ?
                   AND session_id = ? AND branch_id = ? AND idempotency_hash = ?",
            )
            .bind(&gate.scope)
            .bind(&reservation.reservation_id)
            .bind(&reservation.key.isolation_domain)
            .bind(&reservation.key.owner_user_id)
            .bind(&reservation.key.session_id)
            .bind(&reservation.key.branch_id)
            .bind(&reservation.idempotency_hash)
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
        gate: &Arc<Semaphore>,
        deadline: std::time::Instant,
    ) -> Result<OwnedSemaphorePermit, DistributedAdmissionError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tokio::time::timeout(remaining, gate.clone().acquire_owned())
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
    ) -> Result<CancellationSafeTransaction, DistributedAdmissionError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tokio::time::timeout(
            remaining,
            CancellationSafeTransaction::begin(self.pool.get()),
        )
        .await
        .map_err(|_| admission_timeout_error(self.admission_wait_timeout))?
        .map_err(|source| distributed_database_error(operation, source))
    }

    async fn lock_admission_gate(
        &self,
        tx: &mut CancellationSafeTransaction,
        partition: usize,
        deadline: std::time::Instant,
    ) -> Result<LockedDistributedAdmissionGate, DistributedAdmissionError> {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        tx.mark_io_in_flight();
        match tokio::time::timeout(remaining, lock_distributed_admission_gate(tx, partition)).await
        {
            Ok(result) => {
                if result.is_ok() {
                    tx.mark_io_complete();
                }
                result
            }
            Err(_) => {
                tx.mark_io_in_flight();
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

#[derive(Debug)]
enum CapacityProbe {
    Exhausted,
    Donors(Vec<usize>),
    Uncertain,
}

enum ReservationAttempt {
    Granted(DistributedAdmissionReservation),
    ConfigurationNeeded,
    CreditNeeded,
}

fn donor_partitions(
    free: &[AdmissionWork],
    target: usize,
    work: AdmissionWork,
) -> Option<Vec<usize>> {
    if target >= free.len() || free.len() > DISTRIBUTED_ADMISSION_PARTITIONS {
        return None;
    }
    let target_free = free[target].components();
    let mut remaining = work.components();
    for (need, available) in remaining.iter_mut().zip(target_free) {
        *need = need.saturating_sub(available);
    }
    let mut plan = vec![target];
    for offset in 1..free.len() {
        if remaining.iter().all(|need| *need == 0) {
            break;
        }
        let donor = (target + offset) % free.len();
        let available = free[donor].components();
        if !remaining
            .iter()
            .zip(available)
            .any(|(need, free)| *need != 0 && free != 0)
        {
            continue;
        }
        plan.push(donor);
        for (need, free) in remaining.iter_mut().zip(available) {
            *need = need.saturating_sub(free);
        }
    }
    if remaining.iter().any(|need| *need != 0) {
        return None;
    }
    plan.sort_unstable();
    Some(plan)
}

struct LockedDistributedAdmissionGate {
    scope: String,
    capacity_hash: Option<String>,
    usage_initialized: bool,
    used: AdmissionWork,
    allocated: AdmissionWork,
    earliest_expiry_at: Option<chrono::NaiveDateTime>,
}

const ADMISSION_GATE_COLUMNS: &str = "capacity_hash, usage_initialized, earliest_expiry_at,
    CAST(used_resident_bytes AS CHAR) AS used_resident_bytes,
    CAST(used_context_tokens AS CHAR) AS used_context_tokens,
    CAST(used_provider_slots AS CHAR) AS used_provider_slots,
    CAST(used_cpu_units AS CHAR) AS used_cpu_units,
    CAST(used_io_bytes AS CHAR) AS used_io_bytes,
    CAST(allocated_resident_bytes AS CHAR) AS allocated_resident_bytes,
    CAST(allocated_context_tokens AS CHAR) AS allocated_context_tokens,
    CAST(allocated_provider_slots AS CHAR) AS allocated_provider_slots,
    CAST(allocated_cpu_units AS CHAR) AS allocated_cpu_units,
    CAST(allocated_io_bytes AS CHAR) AS allocated_io_bytes";

async fn lock_distributed_admission_gate(
    tx: &mut CancellationSafeTransaction,
    partition: usize,
) -> Result<LockedDistributedAdmissionGate, DistributedAdmissionError> {
    let scope = distributed_admission_scope(partition);
    let statement = format!(
        "SELECT {ADMISSION_GATE_COLUMNS} FROM session_weighted_admission_gates WHERE scope_name = ? FOR UPDATE"
    );
    let row = sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(&scope)
        .fetch_one(&mut **tx)
        .await
        .map_err(|source| distributed_database_error("lock_gate", source))?;
    decode_distributed_admission_gate(&row, scope)
}

fn decode_distributed_admission_gate(
    row: &sqlx::mysql::MySqlRow,
    scope: String,
) -> Result<LockedDistributedAdmissionGate, DistributedAdmissionError> {
    let capacity_hash = row
        .try_get::<Option<String>, _>("capacity_hash")
        .map_err(|source| distributed_database_error("decode_gate_capacity_hash", source))?;
    let usage_initialized = row
        .try_get::<i64, _>("usage_initialized")
        .map_err(|source| distributed_database_error("decode_gate_usage_initialized", source))?
        != 0;
    let used = decode_aggregate_admission_work(row, "used_")?;
    let allocated = decode_aggregate_admission_work(row, "allocated_")?;
    let earliest_expiry_at: Option<chrono::NaiveDateTime> = row
        .try_get("earliest_expiry_at")
        .map_err(|source| distributed_database_error("decode_gate_expiry_frontier", source))?;
    if usage_initialized && used != AdmissionWork::default() && earliest_expiry_at.is_none() {
        return Err(DistributedAdmissionError::Invalid(
            "initialized admission usage has no expiry frontier".into(),
        ));
    }
    Ok(LockedDistributedAdmissionGate {
        scope,
        capacity_hash,
        usage_initialized,
        used,
        allocated,
        earliest_expiry_at,
    })
}

// Called only while every partition is locked, before any grant or transfer.
async fn ensure_partition_allocations(
    tx: &mut CancellationSafeTransaction,
    gates: &mut [LockedDistributedAdmissionGate],
    limits: WeightedAdmissionLimits,
    requested: &str,
) -> Result<(), DistributedAdmissionError> {
    let used = sum_admission_work(gates.iter().map(|gate| gate.used))?;
    let mut reconfigure = false;
    for gate in gates.iter() {
        match capacity_gate_transition(
            gate.capacity_hash.as_deref(),
            i64::from(used.provider_slots),
            requested,
        ) {
            CapacityGateTransition::Reject { active, requested } => {
                return Err(DistributedAdmissionError::ConfigurationMismatch { active, requested });
            }
            CapacityGateTransition::Accept => {}
            CapacityGateTransition::Initialize | CapacityGateTransition::Rotate => {
                reconfigure = true
            }
        }
    }
    if reconfigure {
        for (index, gate) in gates.iter_mut().enumerate() {
            let values = limits.global.components().map(|value| {
                value / DISTRIBUTED_ADMISSION_PARTITIONS as u64
                    + u64::from((index as u64) < value % DISTRIBUTED_ADMISSION_PARTITIONS as u64)
            });
            gate.allocated = AdmissionWork::from_components(values).ok_or_else(|| {
                DistributedAdmissionError::Invalid("partition allocation overflow".into())
            })?;
            store_partition_allocation(tx, gate, requested).await?;
            gate.capacity_hash = Some(requested.to_owned());
        }
    }
    if sum_admission_work(gates.iter().map(|gate| gate.allocated))? != limits.global
        || gates
            .iter()
            .any(|gate| !gate.used.fits_within(gate.allocated))
    {
        return Err(DistributedAdmissionError::Invalid(
            "partition capacity conservation violated".into(),
        ));
    }
    Ok(())
}

fn sum_admission_work(
    values: impl IntoIterator<Item = AdmissionWork>,
) -> Result<AdmissionWork, DistributedAdmissionError> {
    values
        .into_iter()
        .try_fold(AdmissionWork::default(), |total, work| {
            total.checked_add(work).ok_or_else(|| {
                DistributedAdmissionError::Invalid("partition usage overflow".into())
            })
        })
}

async fn store_partition_allocation(
    tx: &mut CancellationSafeTransaction,
    gate: &LockedDistributedAdmissionGate,
    hash: &str,
) -> Result<(), DistributedAdmissionError> {
    admission_io!(tx, sqlx::query(
        "UPDATE session_weighted_admission_gates SET capacity_hash = ?, allocated_resident_bytes = ?, allocated_context_tokens = ?, allocated_provider_slots = ?, allocated_cpu_units = ?, allocated_io_bytes = ?, updated_at = NOW(6) WHERE scope_name = ?"
    ).bind(hash).bind(gate.allocated.resident_bytes.to_string()).bind(gate.allocated.context_tokens.to_string())
        .bind(u64::from(gate.allocated.provider_slots).to_string()).bind(gate.allocated.cpu_units.to_string()).bind(gate.allocated.io_bytes.to_string()).bind(&gate.scope).execute(&mut **tx))
        .map_err(|source| distributed_database_error("store_partition_allocation", source))?;
    Ok(())
}

async fn transfer_partition_credit(
    tx: &mut CancellationSafeTransaction,
    gates: &mut [LockedDistributedAdmissionGate],
    target: usize,
    work: AdmissionWork,
    global: AdmissionWork,
) -> Result<bool, DistributedAdmissionError> {
    let total = sum_admission_work(gates.iter().map(|gate| gate.used))?;
    let capacity = sum_admission_work(gates.iter().map(|gate| gate.allocated))?;
    if !total
        .checked_add(work)
        .is_some_and(|next| next.fits_within(capacity))
    {
        return Ok(false);
    }
    let required = gates[target]
        .used
        .checked_add(work)
        .ok_or_else(|| DistributedAdmissionError::Invalid("partition request overflow".into()))?
        .components();
    let mut allocations: Vec<_> = gates
        .iter()
        .map(|gate| gate.allocated.components())
        .collect();
    for (dimension, global_limit) in global.components().into_iter().enumerate() {
        let deficit = required[dimension].saturating_sub(allocations[target][dimension]);
        if deficit == 0 {
            continue;
        }
        let mut remaining =
            deficit.max(global_limit.div_ceil(DISTRIBUTED_ADMISSION_PARTITIONS as u64));
        for donor in 0..gates.len() {
            if donor == target {
                continue;
            }
            let free = allocations[donor][dimension] - gates[donor].used.components()[dimension];
            let transfer = free.min(remaining);
            allocations[donor][dimension] -= transfer;
            allocations[target][dimension] += transfer;
            remaining -= transfer;
            if remaining == 0 {
                break;
            }
        }
        if allocations[target][dimension] < required[dimension] {
            return Err(DistributedAdmissionError::Invalid(
                "partition credit transfer lost capacity".into(),
            ));
        }
    }
    let mut changed_rows = 0usize;
    for (gate, values) in gates.iter_mut().zip(allocations) {
        let allocated = AdmissionWork::from_components(values).ok_or_else(|| {
            DistributedAdmissionError::Invalid("partition allocation overflow".into())
        })?;
        if gate.allocated != allocated {
            changed_rows += 1;
            gate.allocated = allocated;
            store_partition_allocation(
                tx,
                gate,
                gate.capacity_hash.as_deref().expect("configured partition"),
            )
            .await?;
        }
    }
    tracing::debug!(target: "astra_services::weighted_admission", changed_rows, "admission credit transfer prepared");
    Ok(true)
}

async fn read_admission_clock(
    tx: &mut CancellationSafeTransaction,
) -> Result<chrono::NaiveDateTime, DistributedAdmissionError> {
    // NOW is sampled before lock waits. Read it only after ALL required locks.
    let unix_ms: i64 = admission_io!(
        tx,
        sqlx::query_scalar("SELECT CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED)")
            .fetch_one(&mut **tx)
    )
    .map_err(|source| distributed_database_error("read_locked_gate_database_time", source))?;
    chrono::DateTime::from_timestamp_millis(unix_ms)
        .map(|time| time.naive_utc())
        .ok_or_else(|| {
            DistributedAdmissionError::Invalid("database time is outside chrono range".into())
        })
}

fn admission_capacity_hash(limits: WeightedAdmissionLimits) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DISTRIBUTED_ADMISSION_CAPACITY_DOMAIN);
    hasher.update((DISTRIBUTED_ADMISSION_PARTITIONS as u64).to_be_bytes());
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
    tx: &mut CancellationSafeTransaction,
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
        .bind(owner_admission_scope(key))
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

async fn store_expiry_frontier(
    tx: &mut CancellationSafeTransaction,
    scope: &str,
    frontier: Option<chrono::NaiveDateTime>,
) -> Result<(), DistributedAdmissionError> {
    admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_gates
             SET earliest_expiry_at = ?, updated_at = NOW(6)
             WHERE scope_name = ?",
        )
        .bind(frontier)
        .bind(scope)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("store_expiry_frontier", source))?;
    Ok(())
}

async fn reconcile_expired_reservations(
    tx: &mut CancellationSafeTransaction,
    gate: &mut LockedDistributedAdmissionGate,
    now: chrono::NaiveDateTime,
) -> Result<(), DistributedAdmissionError> {
    if gate.usage_initialized && gate.earliest_expiry_at.is_none_or(|at| at > now) {
        return Ok(());
    }
    let deleted = admission_io!(
        tx,
        sqlx::query(
            "DELETE FROM session_weighted_admission_reservations
         WHERE scope_name = ? AND expires_at <= NOW(6)",
        )
        .bind(&gate.scope)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("cleanup_expired", source))?;
    if deleted.rows_affected() != 0 {
        // The same transaction publishes rebuilt counters and frontier before
        // any successful commit, so no intermediate dirty UPDATE is needed.
        gate.usage_initialized = false;
    } else if gate.usage_initialized {
        gate.earliest_expiry_at = admission_io!(tx, sqlx::query_scalar(
            "SELECT MIN(expires_at) FROM session_weighted_admission_reservations WHERE scope_name = ?",
        ).bind(&gate.scope).fetch_one(&mut **tx))
            .map_err(|source| distributed_database_error("refresh_expiry_frontier", source))?;
        store_expiry_frontier(tx, &gate.scope, gate.earliest_expiry_at).await?;
    }
    Ok(())
}

/// Rebuild O(1) global and per-owner totals from reservation rows. This is a
/// repair path, used on fresh initialization or after an out-of-band delete;
/// normal reserve/release paths update the totals incrementally.
async fn ensure_materialized_admission_usage(
    tx: &mut CancellationSafeTransaction,
    gate: &mut LockedDistributedAdmissionGate,
) -> Result<(), DistributedAdmissionError> {
    if gate.usage_initialized {
        return Ok(());
    }

    // MatrixOne sums BIGINT inputs as int64. Widen before SUM, not after it:
    // multiple valid reservation rows can exceed i64::MAX while fitting u64.
    let row = admission_io!(
        tx,
        sqlx::query(
            "SELECT
                 MIN(expires_at) AS earliest_expiry_at,
                 CAST(COALESCE(SUM(CAST(resident_bytes AS DECIMAL(38, 0))), 0) AS CHAR) AS used_resident_bytes,
                 CAST(COALESCE(SUM(CAST(context_tokens AS DECIMAL(38, 0))), 0) AS CHAR) AS used_context_tokens,
                 CAST(COALESCE(SUM(CAST(provider_slots AS DECIMAL(38, 0))), 0) AS CHAR) AS used_provider_slots,
                 CAST(COALESCE(SUM(CAST(cpu_units AS DECIMAL(38, 0))), 0) AS CHAR) AS used_cpu_units,
                 CAST(COALESCE(SUM(CAST(io_bytes AS DECIMAL(38, 0))), 0) AS CHAR) AS used_io_bytes,
                 CAST(COALESCE(SUM(CASE
                     WHEN resident_bytes < 0 OR context_tokens < 0 OR provider_slots <= 0
                       OR provider_slots > 4294967295 OR cpu_units < 0 OR io_bytes < 0
                     THEN 1 ELSE 0 END), 0) AS SIGNED) AS invalid_rows
             FROM session_weighted_admission_reservations
             WHERE scope_name = ?",
        )
        .bind(&gate.scope)
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
    let global = decode_aggregate_admission_work(&row, "used_")?;
    let earliest_expiry_at: Option<chrono::NaiveDateTime> = row
        .try_get("earliest_expiry_at")
        .map_err(|source| distributed_database_error("decode_usage_expiry_frontier", source))?;

    admission_io!(
        tx,
        sqlx::query(
            "DELETE FROM session_weighted_admission_owner_usage
             WHERE scope_name = ?",
        )
        .bind(&gate.scope)
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
                    COALESCE(SUM(CAST(resident_bytes AS DECIMAL(38, 0))), 0),
                    COALESCE(SUM(CAST(context_tokens AS DECIMAL(38, 0))), 0),
                    COALESCE(SUM(CAST(provider_slots AS DECIMAL(38, 0))), 0),
                    COALESCE(SUM(CAST(cpu_units AS DECIMAL(38, 0))), 0),
                    COALESCE(SUM(CAST(io_bytes AS DECIMAL(38, 0))), 0)
             FROM session_weighted_admission_reservations
             WHERE scope_name = ?
             GROUP BY scope_name, isolation_domain, owner_user_id",
        )
        .bind(&gate.scope)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("rebuild_owner_usage", source))?;

    admission_io!(
        tx,
        sqlx::query(
            "UPDATE session_weighted_admission_gates
             SET usage_initialized = 1,
                 earliest_expiry_at = ?,
                 used_resident_bytes = ?,
                 used_context_tokens = ?,
                 used_provider_slots = ?,
                 used_cpu_units = ?,
                 used_io_bytes = ?,
                 updated_at = NOW(6)
             WHERE scope_name = ?",
        )
        .bind(earliest_expiry_at)
        .bind(global.resident_bytes.to_string())
        .bind(global.context_tokens.to_string())
        .bind(u64::from(global.provider_slots).to_string())
        .bind(global.cpu_units.to_string())
        .bind(global.io_bytes.to_string())
        .bind(&gate.scope)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("publish_materialized_usage", source))?;
    gate.usage_initialized = true;
    gate.used = global;
    gate.earliest_expiry_at = earliest_expiry_at;
    Ok(())
}

async fn load_materialized_owner_usage(
    tx: &mut CancellationSafeTransaction,
    key: &SessionKeyV1,
) -> Result<AdmissionWork, DistributedAdmissionError> {
    // Native equality enables the compound-key lookup; the binary guard keeps
    // exact owner identity. A cast-only predicate scans the whole scope in MO.
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
               AND owner_user_id = ? AND BINARY owner_user_id = BINARY ?",
        )
        .bind(owner_admission_scope(key))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.owner_user_id)
        .fetch_optional(&mut **tx)
    )
    .map_err(|source| distributed_database_error("load_owner_usage", source))?;
    row.map(|row| decode_aggregate_admission_work(&row, "owner_"))
        .transpose()
        .map(|usage| usage.unwrap_or_default())
}

async fn add_materialized_admission_usage(
    tx: &mut CancellationSafeTransaction,
    key: &SessionKeyV1,
    work: AdmissionWork,
    expires_at: chrono::NaiveDateTime,
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
             SET used_resident_bytes = used_resident_bytes + ?,
                 earliest_expiry_at = CASE
                     WHEN earliest_expiry_at IS NULL OR earliest_expiry_at > ? THEN ?
                     ELSE earliest_expiry_at END,
                 used_context_tokens = used_context_tokens + ?,
                 used_provider_slots = used_provider_slots + ?,
                 used_cpu_units = used_cpu_units + ?,
                 used_io_bytes = used_io_bytes + ?,
                 updated_at = NOW(6)
             WHERE scope_name = ? AND usage_initialized = 1",
        )
        .bind(resident_bytes)
        .bind(expires_at)
        .bind(expires_at)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .bind(owner_admission_scope(key))
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("increment_used_usage", source))?;
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
        .bind(owner_admission_scope(key))
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
    tx: &mut CancellationSafeTransaction,
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
             SET used_resident_bytes = used_resident_bytes - ?,
                 used_context_tokens = used_context_tokens - ?,
                 used_provider_slots = used_provider_slots - ?,
                 used_cpu_units = used_cpu_units - ?,
                 used_io_bytes = used_io_bytes - ?,
                 updated_at = NOW(6)
             WHERE scope_name = ? AND usage_initialized = 1
               AND used_resident_bytes >= ?
               AND used_context_tokens >= ?
               AND used_provider_slots >= ?
               AND used_cpu_units >= ?
               AND used_io_bytes >= ?",
        )
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .bind(owner_admission_scope(key))
        .bind(resident_bytes)
        .bind(context_tokens)
        .bind(provider_slots)
        .bind(cpu_units)
        .bind(io_bytes)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("decrement_used_usage", source))?;
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
               AND owner_user_id = ? AND BINARY owner_user_id = BINARY ?
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
        .bind(owner_admission_scope(key))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
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
               AND owner_user_id = ? AND BINARY owner_user_id = BINARY ?
               AND resident_bytes = 0 AND context_tokens = 0
               AND provider_slots = 0 AND cpu_units = 0 AND io_bytes = 0",
        )
        .bind(owner_admission_scope(key))
        .bind(&key.isolation_domain)
        .bind(&key.owner_user_id)
        .bind(&key.owner_user_id)
        .execute(&mut **tx)
    )
    .map_err(|source| distributed_database_error("delete_empty_owner_usage", source))?;
    Ok(())
}

async fn insert_distributed_reservation(
    tx: &mut CancellationSafeTransaction,
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
        .bind(owner_admission_scope(&reservation.key))
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

    async fn admission_test_pool() -> SharedPool {
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let settings = astra_core::MatrixOneSettings::from_env();
        crate::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
        let retained: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM session_weighted_admission_reservations")
                .fetch_one(pool.get())
                .await
                .unwrap();
        assert_eq!(
            retained, 0,
            "admission fixture must have no retained reservations"
        );
        pool
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::file_serial(admission_scope)]
    async fn refill_probe_observes_post_queue_credit_and_recovers_after_timeout() {
        let pool = admission_test_pool().await;
        let mut settings = pool.settings().clone();
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 0;
        let one_pool = SharedPool::new(&settings).await.unwrap();
        let unit = AdmissionWork {
            provider_slots: 1,
            ..AdmissionWork::default()
        };
        let global = AdmissionWork {
            provider_slots: 16,
            ..unit
        };
        let limits = WeightedAdmissionLimits {
            global,
            per_owner: global,
        };
        let controller =
            DatabaseWeightedAdmissionController::new(one_pool.clone(), limits).unwrap();
        let key_for = |partition| {
            let owner = (0..10000)
                .map(|n| format!("coalesced-owner-{n}"))
                .find(|owner| distributed_admission_partition(owner) == partition)
                .unwrap();
            SessionKeyV1::owner_session("server", &owner, "coalesced-preflight", "main")
        };
        let key = key_for(15);
        controller
            .try_reserve(&key, unit, Duration::from_secs(30), "warm")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let connection: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(one_pool.get())
            .await
            .unwrap();
        let refill = controller
            .refill_gate
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let mut holder = CancellationSafeTransaction::begin(pool.get())
            .await
            .unwrap();
        lock_distributed_admission_gate(&mut holder, 15)
            .await
            .unwrap();
        let pending = tokio::spawn({
            let controller = controller.clone();
            let key = key.clone();
            async move {
                controller
                    .try_reserve(
                        &key,
                        AdmissionWork {
                            provider_slots: 2,
                            ..unit
                        },
                        Duration::from_secs(30),
                        "queued",
                    )
                    .await
                    .unwrap()
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.processlist WHERE db = ? AND conn_id = ? AND info LIKE '%canonical_turn_v2_15%' AND info NOT LIKE '%information_schema.processlist%'")
                .bind(&settings.database).bind(connection).fetch_one(pool.get()).await.unwrap();
            if waiting == 1 { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("public initial attempt must own its local queue before donor changes");
        holder.rollback().await.unwrap();
        let owner = tokio::time::timeout(
            Duration::from_secs(3),
            controller.reservation_gates[15].clone().acquire_owned(),
        )
        .await
        .unwrap()
        .unwrap();
        drop(owner);
        assert!(!pending.is_finished());
        tokio::time::timeout(
            Duration::from_secs(1),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(one_pool.get()),
        )
        .await
        .unwrap()
        .unwrap();
        let other = DatabaseWeightedAdmissionController::new(pool.clone(), limits).unwrap();
        let donor = other
            .try_reserve(&key_for(0), unit, Duration::from_secs(30), "occupy-donor")
            .await
            .unwrap();
        let mut donor_lock = CancellationSafeTransaction::begin(pool.get())
            .await
            .unwrap();
        lock_distributed_admission_gate(&mut donor_lock, 0)
            .await
            .unwrap();
        drop(refill);
        let granted = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("post-queue snapshot must skip the now-occupied/locked donor")
            .unwrap();
        granted.release().await.unwrap();
        donor_lock.rollback().await.unwrap();
        donor.release().await.unwrap();
        let refill = controller
            .refill_gate
            .clone()
            .acquire_owned()
            .await
            .unwrap();
        let mut short = controller.clone();
        short.with_admission_wait_timeout(Duration::from_millis(50));
        assert!(matches!(
            short
                .try_reserve(&key, global, Duration::from_secs(30), "queue-timeout")
                .await,
            Err(DistributedAdmissionError::AdmissionTimeout { .. })
        ));
        tokio::time::timeout(
            Duration::from_secs(1),
            sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(one_pool.get()),
        )
        .await
        .unwrap()
        .unwrap();
        drop(refill);
        controller
            .try_reserve(&key, global, Duration::from_secs(30), "queue-timeout")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let state: (String, String) = sqlx::query_as("SELECT CAST(SUM(allocated_provider_slots) AS CHAR), CAST(SUM(used_provider_slots) AS CHAR) FROM session_weighted_admission_gates")
        .fetch_one(pool.get()).await.unwrap();
        assert_eq!(state, ("16".into(), "0".into()));
        one_pool.close().await;
        pool.close().await;
    }

    #[test]
    fn donor_plan_is_bounded_multidimensional_and_sorted() {
        let unit = AdmissionWork {
            resident_bytes: 1,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        };
        let mut free = [AdmissionWork::default(); 4];
        free[1] = AdmissionWork {
            resident_bytes: 5,
            provider_slots: 1,
            ..AdmissionWork::default()
        };
        free[3] = AdmissionWork {
            context_tokens: 5,
            ..AdmissionWork::default()
        };
        let requested = AdmissionWork {
            resident_bytes: 5,
            context_tokens: 5,
            provider_slots: 1,
            ..AdmissionWork::default()
        };
        assert_eq!(donor_partitions(&free, 2, requested), Some(vec![1, 2, 3]));
        free[2] = requested;
        assert_eq!(donor_partitions(&free, 2, requested), Some(vec![2]));
        assert_eq!(donor_partitions(&[], 0, unit), None);
        assert_eq!(
            donor_partitions(&[AdmissionWork::default(); 4], 2, unit),
            None
        );
        assert_eq!(
            donor_partitions(
                &[unit; 16],
                10,
                AdmissionWork {
                    provider_slots: 16,
                    ..unit
                }
            ),
            Some((0..16).collect())
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::file_serial(admission_scope)]
    async fn selected_donors_skip_unrelated_locks_and_revalidate_stale_credit() {
        let pool = admission_test_pool().await;
        let unit = AdmissionWork {
            provider_slots: 1,
            ..AdmissionWork::default()
        };
        let global = AdmissionWork {
            provider_slots: 16,
            ..unit
        };
        let key_for = |partition| {
            let owner = (0..10000)
                .map(|n| format!("stale-donor-{n}"))
                .find(|owner| distributed_admission_partition(owner) == partition)
                .unwrap();
            SessionKeyV1::owner_session("server", &owner, "stale-donor", "main")
        };
        let key = key_for(15);
        let reset = DatabaseWeightedAdmissionController::new(
            pool.clone(),
            WeightedAdmissionLimits {
                global: unit,
                per_owner: unit,
            },
        )
        .unwrap();
        reset
            .try_reserve(&key, unit, Duration::from_secs(30), "reset")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let controller = DatabaseWeightedAdmissionController::new(
            pool.clone(),
            WeightedAdmissionLimits {
                global,
                per_owner: global,
            },
        )
        .unwrap();
        controller
            .try_reserve(&key, unit, Duration::from_secs(30), "warm")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let work = AdmissionWork {
            provider_slots: 2,
            ..unit
        };
        let mut unrelated = CancellationSafeTransaction::begin(pool.get())
            .await
            .unwrap();
        controller
            .lock_admission_gate(
                &mut unrelated,
                8,
                std::time::Instant::now() + Duration::from_secs(3),
            )
            .await
            .unwrap();
        let borrowed = tokio::time::timeout(
            Duration::from_secs(1),
            controller.try_reserve(&key_for(5), work, Duration::from_secs(30), "unrelated-lock"),
        )
        .await
        .expect("borrowing must not wait for an unselected partition")
        .unwrap();
        borrowed.release().await.unwrap();
        unrelated.rollback().await.unwrap();
        let hash = distributed_idempotency_hash("stale");
        let capacity = admission_capacity_hash(controller.limits);
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let CapacityProbe::Donors(partitions) = controller
            .probe_partition_capacity(&key, work, &hash, &capacity, deadline)
            .await
            .unwrap()
        else {
            panic!("fresh snapshot must suggest donors")
        };
        assert_eq!(partitions, vec![0, 15]);
        let donor = controller
            .try_reserve(&key_for(0), unit, Duration::from_secs(30), "occupy-donor")
            .await
            .unwrap();
        assert!(matches!(
            controller
                .attempt_reservation(
                    &key,
                    work,
                    Duration::from_secs(30),
                    &hash,
                    &capacity,
                    &partitions,
                    deadline
                )
                .await
                .unwrap(),
            ReservationAttempt::CreditNeeded
        ));
        let all: Vec<_> = (0..DISTRIBUTED_ADMISSION_PARTITIONS).collect();
        let ReservationAttempt::Granted(reservation) = controller
            .attempt_reservation(
                &key,
                work,
                Duration::from_secs(30),
                &hash,
                &capacity,
                &all,
                deadline,
            )
            .await
            .unwrap()
        else {
            panic!("full inventory must recover the stale suggestion")
        };
        DistributedAdmissionPermit::new(controller.clone(), reservation)
            .release()
            .await
            .unwrap();
        donor.release().await.unwrap();
        pool.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::file_serial(admission_scope)]
    async fn cancelled_borrow_rolls_back_transferred_credit() {
        let pool = admission_test_pool().await;
        let mut settings = pool.settings().clone();
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 0;
        let waiter_pool = SharedPool::new(&settings).await.unwrap();
        let global = AdmissionWork {
            resident_bytes: 16,
            context_tokens: 16,
            provider_slots: 16,
            cpu_units: 16,
            io_bytes: 16,
        };
        let unit = AdmissionWork {
            resident_bytes: 1,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        };
        let controller = DatabaseWeightedAdmissionController::new(
            waiter_pool.clone(),
            WeightedAdmissionLimits {
                global,
                per_owner: global,
            },
        )
        .unwrap();
        let key = SessionKeyV1::owner_session(
            "server",
            "borrow-cancel-owner",
            "borrow-cancel-session",
            "main",
        );
        // Rotate an empty inventory through a different real configuration so
        // this test also exercises a transfer when rerun on the same database.
        let reset = DatabaseWeightedAdmissionController::new(
            waiter_pool.clone(),
            WeightedAdmissionLimits {
                global: unit,
                per_owner: unit,
            },
        )
        .unwrap();
        reset
            .try_reserve(&key, unit, Duration::from_secs(30), "reset")
            .await
            .unwrap()
            .release()
            .await
            .unwrap();
        let warm = controller
            .try_reserve(&key, unit, Duration::from_secs(30), "warm")
            .await
            .unwrap();
        warm.release().await.unwrap();
        let connection_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(waiter_pool.get())
            .await
            .unwrap();
        let mut holder = CancellationSafeTransaction::begin(pool.get())
            .await
            .unwrap();
        let expires_at =
            read_admission_clock(&mut holder).await.unwrap() + chrono::Duration::seconds(60);
        let work = AdmissionWork {
            provider_slots: 2,
            ..unit
        };
        let conflict = DistributedAdmissionReservation {
            reservation_id: Uuid::new_v4().to_string(),
            key: key.clone(),
            work,
            expires_at_unix_ms: expires_at.and_utc().timestamp_millis(),
            idempotency_hash: distributed_idempotency_hash("cancel-after-transfer"),
        };
        // An uncommitted conflicting identity pauses the actual INSERT after
        // borrowing. It is never published as a valid reservation.
        insert_distributed_reservation(&mut holder, &conflict, expires_at)
            .await
            .unwrap();
        let pending = tokio::spawn({
            let controller = controller.clone();
            let key = key.clone();
            async move {
                controller
                    .try_reserve(&key, work, Duration::from_secs(30), "cancel-after-transfer")
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let waiting: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.processlist WHERE db = ? AND conn_id = ? AND info LIKE '%INSERT INTO session_weighted_admission_reservations%' AND info NOT LIKE '%information_schema.processlist%'")
                    .bind(&settings.database).bind(connection_id).fetch_one(pool.get()).await.unwrap();
                if waiting == 1 { break; }
                tokio::task::yield_now().await;
            }
        }).await.expect("the public request must reach INSERT after credit transfer");
        pending.abort();
        assert!(matches!(pending.await, Err(error) if error.is_cancelled()));
        holder.rollback().await.unwrap();
        let mut check = CancellationSafeTransaction::begin(pool.get())
            .await
            .unwrap();
        for partition in 0..DISTRIBUTED_ADMISSION_PARTITIONS {
            let gate = controller
                .lock_admission_gate(
                    &mut check,
                    partition,
                    std::time::Instant::now() + Duration::from_secs(3),
                )
                .await
                .unwrap();
            assert_eq!(
                gate.allocated, unit,
                "aborted credit transfer must be rolled back"
            );
            assert_eq!(gate.used, AdmissionWork::default());
        }
        check.commit().await.unwrap();
        let recovered = controller
            .try_reserve(&key, work, Duration::from_secs(30), "cancel-after-transfer")
            .await
            .unwrap();
        recovered.release().await.unwrap();
        let retained: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM session_weighted_admission_reservations")
                .fetch_one(pool.get())
                .await
                .unwrap();
        assert_eq!(retained, 0);
        waiter_pool.close().await;
        pool.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "requires MatrixOne; run with ASTRA_TEST_DB_IT=1"]
    #[serial_test::file_serial(admission_scope)]
    async fn capacity_snapshot_observes_atomic_identity_and_usage() {
        let pool = admission_test_pool().await;
        let work = AdmissionWork {
            resident_bytes: 1,
            context_tokens: 1,
            provider_slots: 1,
            cpu_units: 1,
            io_bytes: 1,
        };
        let controller = DatabaseWeightedAdmissionController::new(
            pool.clone(),
            WeightedAdmissionLimits {
                global: work,
                per_owner: work,
            },
        )
        .unwrap();
        let key = SessionKeyV1::owner_session(
            "server",
            "snapshot-atomic-owner",
            "snapshot-atomic-session",
            "main",
        );
        let permit = controller
            .try_reserve(&key, work, Duration::from_secs(900), "atomic")
            .await
            .unwrap();
        let hash = admission_capacity_hash(controller.limits);
        let identity_hash = permit.reservation().idempotency_hash.clone();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let writer = tokio::spawn({
            let controller = controller.clone();
            let pool = pool.clone();
            let reservation = permit.reservation().clone();
            let barrier = barrier.clone();
            async move {
                let partition = distributed_admission_partition(&reservation.key.owner_user_id);
                let expires_at =
                    chrono::DateTime::from_timestamp_millis(reservation.expires_at_unix_ms)
                        .unwrap()
                        .naive_utc();
                barrier.wait().await;
                for _ in 0..64 {
                    let mut tx = CancellationSafeTransaction::begin(pool.get())
                        .await
                        .unwrap();
                    controller
                        .lock_admission_gate(
                            &mut tx,
                            partition,
                            std::time::Instant::now() + Duration::from_secs(10),
                        )
                        .await
                        .unwrap();
                    let deleted = admission_io!(tx, sqlx::query("DELETE FROM session_weighted_admission_reservations WHERE scope_name = ? AND reservation_id = ?")
                        .bind(owner_admission_scope(&reservation.key)).bind(&reservation.reservation_id).execute(&mut *tx)).unwrap();
                    assert_eq!(deleted.rows_affected(), 1);
                    subtract_materialized_admission_usage(&mut tx, &reservation.key, work)
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();
                    tokio::task::yield_now().await;
                    let mut tx = CancellationSafeTransaction::begin(pool.get())
                        .await
                        .unwrap();
                    controller
                        .lock_admission_gate(
                            &mut tx,
                            partition,
                            std::time::Instant::now() + Duration::from_secs(10),
                        )
                        .await
                        .unwrap();
                    insert_distributed_reservation(&mut tx, &reservation, expires_at)
                        .await
                        .unwrap();
                    add_materialized_admission_usage(&mut tx, &reservation.key, work, expires_at)
                        .await
                        .unwrap();
                    tx.commit().await.unwrap();
                }
            }
        });
        barrier.wait().await;
        let probe = || {
            controller.probe_partition_capacity(
                &key,
                work,
                &identity_hash,
                &hash,
                std::time::Instant::now() + Duration::from_secs(10),
            )
        };
        for _ in 0..256 {
            // Full + present and empty + absent both forbid rejection.
            assert!(!matches!(probe().await.unwrap(), CapacityProbe::Exhausted));
        }
        writer.await.unwrap();
        permit.release().await.unwrap();
        sqlx::query("UPDATE session_weighted_admission_gates SET used_resident_bytes = 1, earliest_expiry_at = NULL WHERE scope_name = ?")
            .bind(owner_admission_scope(&key)).execute(pool.get()).await.unwrap();
        assert!(
            matches!(probe().await.unwrap(), CapacityProbe::Uncertain),
            "zero slots with charged bytes is not a valid empty partition"
        );
        sqlx::query("UPDATE session_weighted_admission_gates SET used_resident_bytes = 0 WHERE scope_name = ?")
            .bind(owner_admission_scope(&key)).execute(pool.get()).await.unwrap();
        assert!(
            !matches!(probe().await.unwrap(), CapacityProbe::Exhausted),
            "a completed release must be visible to the next fresh snapshot"
        );
        pool.close().await;
    }
}
