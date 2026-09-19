use astra_core::SharedPool;
use chrono::NaiveDateTime;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use uuid::Uuid;

/// TTL for sweeper leader-election leases.
/// The local coordinator refreshes the durable lease within this window.
const SWEEPER_LEASE_TTL_SECS: u64 = 60;
/// Refresh at half the durable lease TTL. This leaves enough time for another
/// process to take over after this process disappears while suppressing
/// duplicate checks from the six in-process sweepers.
const SWEEPER_LEASE_REFRESH_SECS: u64 = SWEEPER_LEASE_TTL_SECS / 2;
/// A lease refresh is background coordination, not user work. Bound the
/// database round-trip so a degraded database cannot pin every sweeper on the
/// shared coordination mutex indefinitely.
const SWEEPER_LEASE_REFRESH_TIMEOUT_SECS: u64 = 5;
/// Non-leaders check more frequently than leaders so takeover is not delayed
/// by the local observation cache after the current lease expires.
const SWEEPER_NON_LEADER_REFRESH_SECS: u64 = 5;
const SWEEPER_LEASE_EXPIRY_SAFETY_SECS: i64 = 1;

/// Result of a leadership check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LeaderStatus {
    /// This pod owns the lease — proceed with work.
    Leader,
    /// Another pod owns the lease (or no row exists).
    NotLeader,
    /// Could not contact the database to determine leadership.
    Unavailable(String),
}

#[derive(Debug, Clone)]
struct LeaseObservation {
    status: LeaderStatus,
    valid_until: Instant,
}

struct LeaseRefresh {
    status: LeaderStatus,
    lease_remaining: Option<Duration>,
}

impl LeaseRefresh {
    fn unavailable(message: impl Into<String>) -> Self {
        Self {
            status: LeaderStatus::Unavailable(message.into()),
            lease_remaining: None,
        }
    }
}

/// Leader-election lease for background sweepers.
///
/// The database lease remains the cross-process authority. The in-process
/// observation is only a short-lived singleflight/cache so all sweepers in one
/// server do not repeatedly update the same database row. An unavailable
/// observation is cached for only one second so a transient database failure
/// cannot suppress recovery for the whole durable lease interval.
#[derive(Clone)]
pub(crate) struct SweeperLease {
    pub(crate) pool: SharedPool,
    pub(crate) pod_id: String,
    pub(crate) lease_name: String,
    pub(crate) table_name: String,
    coordination: Arc<Mutex<Option<LeaseObservation>>>,
}

impl SweeperLease {
    pub(crate) fn new(
        pool: SharedPool,
        pod_id: String,
        lease_name: String,
        table_name: String,
    ) -> Self {
        Self {
            pool,
            pod_id,
            lease_name,
            table_name,
            coordination: Arc::new(Mutex::new(None)),
        }
    }

    /// Check whether this pod holds the lease. Uses INSERT IGNORE followed by
    /// conditional UPDATE to acquire or refresh. Returns the leader status
    /// after a SELECT back-read.
    ///
    /// **Callers must inspect the result.** Ignoring it silently skips the
    /// lease guard — the caller is expected to match on `LeaderStatus`.
    #[must_use = "leadership status must be checked; ignoring it bypasses the lease guard"]
    pub(crate) async fn check_leader(&self) -> LeaderStatus {
        // Hold the coordination lock through the database refresh so concurrent
        // sweepers share one in-flight check instead of serially issuing the
        // same INSERT/UPDATE/SELECT sequence. The cached interval is shorter
        // than the durable lease TTL, so failover remains database-driven.
        let mut observation = self.coordination.lock().await;
        let now = Instant::now();
        if let Some(cached) = observation.as_ref()
            && now < cached.valid_until
        {
            return cached.status.clone();
        }

        let refresh_started = Instant::now();
        let refresh = match tokio::time::timeout(
            Duration::from_secs(SWEEPER_LEASE_REFRESH_TIMEOUT_SECS),
            self.refresh_from_database(),
        )
        .await
        {
            Ok(refresh) => refresh,
            Err(_) => LeaseRefresh::unavailable(format!(
                "lease refresh timed out after {SWEEPER_LEASE_REFRESH_TIMEOUT_SECS}s"
            )),
        };
        let validity = lease_observation_validity(
            &refresh.status,
            refresh.lease_remaining,
            refresh_started.elapsed(),
        );
        let status = if matches!(refresh.status, LeaderStatus::Leader) && validity.is_zero() {
            LeaderStatus::Unavailable("lease expired before observation was safe".to_string())
        } else {
            refresh.status
        };
        let validity = if matches!(status, LeaderStatus::Unavailable(_)) {
            Duration::from_secs(1)
        } else {
            validity
        };
        *observation = Some(LeaseObservation {
            status: status.clone(),
            valid_until: Instant::now() + validity,
        });
        status
    }

    async fn refresh_from_database(&self) -> LeaseRefresh {
        // Acquire a single connection for all operations
        let mut conn = match self.pool.get().acquire().await {
            Ok(c) => c,
            Err(e) => {
                return LeaseRefresh::unavailable(format!("failed to acquire connection: {e}"));
            }
        };

        let ttl_secs = SWEEPER_LEASE_TTL_SECS as i64;

        // Step 1: Try to insert (succeeds if row doesn't exist).
        // Uses DB-side NOW(6) to eliminate clock-skew risk.
        // MatrixOne INSERT IGNORE always reports rows_affected=0, so we
        // always fall through to the UPDATE path. The back-read SELECT
        // is the sole authority on ownership.
        let insert_sql = format!(
            "INSERT IGNORE INTO {} (sweeper_name, owner_pod_id, expires_at) \
             VALUES (?, ?, NOW(6) + INTERVAL ? SECOND)",
            self.table_name
        );
        let insert_result = sqlx::query(&insert_sql)
            .bind(&self.lease_name)
            .bind(&self.pod_id)
            .bind(ttl_secs)
            .execute(&mut *conn)
            .await;

        if let Err(e) = &insert_result {
            return LeaseRefresh::unavailable(format!("INSERT failed: {e}"));
        }

        // Always attempt the conditional UPDATE: MatrixOne INSERT IGNORE
        // rows_affected=0 gives us no signal. The WHERE clause CAS-guards
        // the update (only if expired OR we already own the row).
        // Uses DB-side NOW(6) + INTERVAL to set expires_at.
        let update_sql = format!(
            "UPDATE {} SET owner_pod_id = ?, expires_at = NOW(6) + INTERVAL ? SECOND \
             WHERE sweeper_name = ? AND (expires_at < NOW(6) OR owner_pod_id = ?)",
            self.table_name
        );
        let update_result = sqlx::query(&update_sql)
            .bind(&self.pod_id)
            .bind(ttl_secs)
            .bind(&self.lease_name)
            .bind(&self.pod_id)
            .execute(&mut *conn)
            .await;

        if let Err(e) = &update_result {
            return LeaseRefresh::unavailable(format!("UPDATE failed: {e}"));
        }

        // Step 2: Verify ownership via back-read on the SAME connection.
        // This is the ONLY reliable signal in MatrixOne (rows_affected is
        // unreliable for both INSERT and UPDATE).
        let select_sql = format!(
            "SELECT owner_pod_id, expires_at, CAST(NOW(6) AS DATETIME(6)) FROM {} WHERE sweeper_name = ?",
            self.table_name
        );
        let row = sqlx::query_as::<_, (String, NaiveDateTime, NaiveDateTime)>(&select_sql)
            .bind(&self.lease_name)
            .fetch_optional(&mut *conn)
            .await;

        match row {
            Ok(Some((owner, expires_at, database_now))) if owner == self.pod_id => {
                let lease_remaining = lease_remaining(expires_at, database_now);
                if lease_remaining.is_zero() {
                    LeaseRefresh {
                        status: LeaderStatus::Unavailable(
                            "lease expired before ownership verification".to_string(),
                        ),
                        lease_remaining: Some(lease_remaining),
                    }
                } else {
                    LeaseRefresh {
                        status: LeaderStatus::Leader,
                        lease_remaining: Some(lease_remaining),
                    }
                }
            }
            Ok(Some((_, expires_at, database_now))) => LeaseRefresh {
                status: LeaderStatus::NotLeader,
                lease_remaining: Some(lease_remaining(expires_at, database_now)),
            },
            Ok(None) => LeaseRefresh {
                status: LeaderStatus::NotLeader,
                lease_remaining: None,
            },
            Err(e) => LeaseRefresh::unavailable(format!("SELECT failed: {e}")),
        }
    }
}

fn lease_observation_validity(
    status: &LeaderStatus,
    lease_remaining: Option<Duration>,
    refresh_elapsed: Duration,
) -> Duration {
    let default = match status {
        LeaderStatus::Leader => Duration::from_secs(SWEEPER_LEASE_REFRESH_SECS),
        LeaderStatus::NotLeader => Duration::from_secs(SWEEPER_NON_LEADER_REFRESH_SECS),
        LeaderStatus::Unavailable(_) => Duration::from_secs(1),
    };
    let Some(lease_remaining) = lease_remaining else {
        return default;
    };
    let safe_remaining = lease_remaining
        .saturating_sub(refresh_elapsed)
        .saturating_sub(Duration::from_secs(SWEEPER_LEASE_EXPIRY_SAFETY_SECS as u64));
    default.min(safe_remaining)
}

fn lease_remaining(expires_at: NaiveDateTime, database_now: NaiveDateTime) -> Duration {
    (expires_at - database_now).to_std().unwrap_or_default()
}

pub(crate) fn spawn_runtime_sweepers(
    shared_pool: SharedPool,
    fork_coordinator: Option<std::sync::Arc<astra_services::DatabaseSessionForkCoordinator>>,
    cancel: tokio_util::sync::CancellationToken,
    // The run lifecycle's owner identity. The recovery sweeper must share
    // this identity; generating a second random identity would split lease
    // authority inside one process.
    run_owner_pod_id: Option<String>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let pod_id = std::env::var("ASTRA_POD_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("astra-runtime-{}", Uuid::new_v4()));

    let lease = Arc::new(SweeperLease::new(
        shared_pool.clone(),
        pod_id,
        "runtime_sweepers".to_string(),
        "sweeper_leases".to_string(),
    ));

    // Each sweeper checks the shared coordinator before every work cycle. The
    // coordinator keeps one durable lease authority without duplicate local
    // INSERT/UPDATE/SELECT sequences or a master task that could be duplicated.
    let handles = vec![
        crate::server::runtime_maintenance_sweeper::spawn_runtime_maintenance_sweeper(
            shared_pool.clone(),
            fork_coordinator,
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::device_lease_sweeper::spawn_device_lease_expiry_sweeper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::artifact_retention_sweeper::spawn_artifact_retention_sweeper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::tool_invocation_compactor::spawn_tool_invocation_compactor(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::run::engine::spawn_active_run_recovery_sweeper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
            run_owner_pod_id,
        ),
        crate::server::inference_settlement_sweeper::spawn_inference_settlement_sweeper(
            shared_pool,
            lease,
            cancel,
        ),
    ];
    handles
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mutex to serialize tests that modify the sweeper_leases table.
    static TEST_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    async fn setup_sweeper_test_pool() -> SharedPool {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let catalog = std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG")
            .unwrap_or_else(|_| "mysql".to_string());
        astra_services::ensure_core_schema(&settings, &catalog)
            .await
            .expect("ensure canonical core schema");
        SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne")
    }

    /// Integration test: validates sweeper lease acquisition and ownership.
    /// Requires a running MatrixOne instance. Run with:
    ///   ASTRA_TEST_DB_IT=1 cargo test -p astra-runtime --lib sweeper_lease -- --ignored
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn sweeper_lease_is_leader_acquires_and_confirms() {
        let _guard = astra_core::sync_poison::recover_mutex_lock(&TEST_SERIAL);
        let pool = setup_sweeper_test_pool().await;

        // The production bootstrap owns the table; this test owns only its row.
        let lease_name = "test-leader-acquire";
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();

        let lease = SweeperLease::new(
            pool.clone(),
            "test-pod-1".to_string(),
            lease_name.to_string(),
            "sweeper_leases".to_string(),
        );

        use super::LeaderStatus;

        // Fresh lease: should become leader.
        assert_eq!(
            lease.check_leader().await,
            LeaderStatus::Leader,
            "first call must acquire lease"
        );

        // Second call within TTL: still leader.
        assert_eq!(
            lease.check_leader().await,
            LeaderStatus::Leader,
            "second call must retain lease"
        );

        // Another pod tries while lease is held: must not become leader.
        let other = SweeperLease::new(
            pool.clone(),
            "test-pod-2".to_string(),
            lease_name.to_string(),
            "sweeper_leases".to_string(),
        );
        assert_eq!(
            other.check_leader().await,
            LeaderStatus::NotLeader,
            "other pod must not acquire active lease"
        );

        // Cleanup
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();
    }

    /// Verifies that an expired lease can be taken over by another pod,
    /// and that expires_at is refreshed on successful acquisition.
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn sweeper_lease_expiry_takeover() {
        let _guard = astra_core::sync_poison::recover_mutex_lock(&TEST_SERIAL);
        let pool = setup_sweeper_test_pool().await;
        let lease_name = "test-leader-expiry";
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();

        use super::LeaderStatus;

        // Pod 1 acquires lease
        let pod1 = SweeperLease::new(
            pool.clone(),
            "pod-expiry-1".to_string(),
            lease_name.to_string(),
            "sweeper_leases".to_string(),
        );
        assert_eq!(
            pod1.check_leader().await,
            LeaderStatus::Leader,
            "pod1 must acquire lease"
        );

        // Force-expire the lease by setting expires_at in the past
        let expired = chrono::Utc::now() - chrono::Duration::seconds(10);
        sqlx::query("UPDATE sweeper_leases SET expires_at = ? WHERE sweeper_name = ?")
            .bind(expired.naive_utc())
            .bind(lease_name)
            .execute(pool.get())
            .await
            .expect("force-expire lease");

        // Pod 2 should now be able to take over the expired lease
        let pod2 = SweeperLease::new(
            pool.clone(),
            "pod-expiry-2".to_string(),
            lease_name.to_string(),
            "sweeper_leases".to_string(),
        );
        assert_eq!(
            pod2.check_leader().await,
            LeaderStatus::Leader,
            "pod2 must acquire expired lease"
        );

        // Verify expires_at was refreshed to a future time
        let (new_expires,): (chrono::NaiveDateTime,) =
            sqlx::query_as("SELECT expires_at FROM sweeper_leases WHERE sweeper_name = ?")
                .bind(lease_name)
                .fetch_one(pool.get())
                .await
                .expect("read back expires_at");
        let now = chrono::Utc::now().naive_utc();
        assert!(
            new_expires > now,
            "expires_at must be refreshed to future: {new_expires} <= {now}"
        );

        // Pod 1 must no longer be leader
        // The original pod keeps its short-lived local observation. A fresh
        // coordinator must consult the durable row and see pod2's takeover.
        let pod1_after_takeover = SweeperLease::new(
            pool.clone(),
            "pod-expiry-1".to_string(),
            lease_name.to_string(),
            "sweeper_leases".to_string(),
        );
        assert_eq!(
            pod1_after_takeover.check_leader().await,
            LeaderStatus::NotLeader,
            "pod1 must lose expired lease"
        );

        // Cleanup
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();
    }

    /// Verifies that pods racing to acquire the lease results in exactly one leader.
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn sweeper_lease_concurrent_cas_single_leader() {
        let _guard = astra_core::sync_poison::recover_mutex_lock(&TEST_SERIAL);
        let pool = setup_sweeper_test_pool().await;
        let lease_name = "test-leader-cas";
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();

        let pool = std::sync::Arc::new(pool);

        use super::LeaderStatus;

        // Spawn 5 "pods" trying to acquire simultaneously
        let results = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<LeaderStatus>::new()));
        let mut handles = Vec::new();
        for i in 0..5 {
            let pool = pool.clone();
            let results = results.clone();
            let pod_id = format!("pod-race-{i}");
            handles.push(tokio::spawn(async move {
                let lease = SweeperLease::new(
                    (*pool).clone(),
                    pod_id,
                    lease_name.to_string(),
                    "sweeper_leases".to_string(),
                );
                let status = lease.check_leader().await;
                results.lock().await.push(status);
            }));
        }

        for h in handles {
            h.await.unwrap();
        }

        let leader_count = {
            let guard = results.lock().await;
            guard
                .iter()
                .filter(|s| matches!(s, LeaderStatus::Leader))
                .count()
        };
        assert_eq!(
            leader_count,
            1,
            "exactly one pod must hold the lease; got {:?}",
            *results.lock().await
        );

        // Cleanup
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();
    }

    /// Verifies that check_leader returns Unavailable when DB is unreachable,
    /// never panicking or blocking indefinitely.
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn sweeper_lease_error_path_returns_unavailable() {
        let _guard = astra_core::sync_poison::recover_mutex_lock(&TEST_SERIAL);
        let pool = setup_sweeper_test_pool().await;

        // Use a non-existent table name so that check_leader() hits the error path
        // without affecting the shared sweeper_leases table used by other tests.
        let lease_name = "test-leader-error";
        let lease = SweeperLease::new(
            pool.clone(),
            "pod-error".to_string(),
            lease_name.to_string(),
            "sweeper_leases_error".to_string(),
        );

        use super::LeaderStatus;
        let status = lease.check_leader().await;
        assert!(
            matches!(status, LeaderStatus::Unavailable(_)),
            "check_leader must return Unavailable on DB error, got {status:?}"
        );

        // Legacy is_leader() must still return false for Unavailable.
        assert!(matches!(
            lease.check_leader().await,
            LeaderStatus::Unavailable(_)
        ));
    }

    #[test]
    fn lease_observation_never_outlives_the_durable_expiry() {
        let lease_remaining = Duration::from_secs(4);
        assert_eq!(
            lease_observation_validity(
                &LeaderStatus::Leader,
                Some(lease_remaining),
                Duration::ZERO,
            ),
            Duration::from_secs(3)
        );
        assert_eq!(
            lease_observation_validity(
                &LeaderStatus::NotLeader,
                Some(lease_remaining),
                Duration::from_secs(2),
            ),
            Duration::from_secs(1)
        );
        assert_eq!(
            lease_observation_validity(
                &LeaderStatus::Leader,
                Some(lease_remaining),
                Duration::from_secs(4),
            ),
            Duration::ZERO
        );
        assert_eq!(
            lease_observation_validity(&LeaderStatus::NotLeader, None, Duration::ZERO),
            Duration::from_secs(SWEEPER_NON_LEADER_REFRESH_SECS)
        );
        assert_eq!(
            lease_observation_validity(
                &LeaderStatus::Leader,
                Some(Duration::from_secs(1)),
                Duration::ZERO,
            ),
            Duration::ZERO
        );
        /*
         * The database's remaining duration is measured at the SQL snapshot,
         * so an unusually slow response consumes the same budget before the
         * observation is cached.
         */
        assert_eq!(
            lease_observation_validity(
                &LeaderStatus::Leader,
                Some(Duration::from_secs(10)),
                Duration::from_secs(9),
            ),
            Duration::ZERO
        );
    }
}
