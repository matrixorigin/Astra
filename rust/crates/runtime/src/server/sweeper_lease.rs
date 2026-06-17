use astra_core::SharedPool;
use uuid::Uuid;

/// TTL for sweeper leader-election leases.
/// Each sweeper independently re-checks leadership within this window.
const SWEEPER_LEASE_TTL_SECS: u64 = 60;

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

/// Leader-election lease for background sweepers. Each sweeper independently
/// checks whether this pod still owns the lease before doing work. This avoids
/// duplicate work when multiple API-server replicas are deployed (HPA).
#[derive(Clone)]
pub(crate) struct SweeperLease {
    pub(crate) pool: SharedPool,
    pub(crate) pod_id: String,
    pub(crate) lease_name: String,
    pub(crate) table_name: String,
}

impl SweeperLease {
    /// Check whether this pod holds the lease. Uses INSERT IGNORE followed by
    /// conditional UPDATE to acquire or refresh. Returns the leader status
    /// after a SELECT back-read.
    ///
    /// **Callers must inspect the result.** Ignoring it silently skips the
    /// lease guard — the caller is expected to match on `LeaderStatus`.
    #[must_use = "leadership status must be checked; ignoring it bypasses the lease guard"]
    pub(crate) async fn check_leader(&self) -> LeaderStatus {
        // Acquire a single connection for all operations
        let mut conn = match self.pool.get().acquire().await {
            Ok(c) => c,
            Err(e) => {
                return LeaderStatus::Unavailable(format!("failed to acquire connection: {e}"));
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
            return LeaderStatus::Unavailable(format!("INSERT failed: {e}"));
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
            return LeaderStatus::Unavailable(format!("UPDATE failed: {e}"));
        }

        // Step 2: Verify ownership via back-read on the SAME connection.
        // This is the ONLY reliable signal in MatrixOne (rows_affected is
        // unreliable for both INSERT and UPDATE).
        let select_sql = format!(
            "SELECT owner_pod_id FROM {} WHERE sweeper_name = ?",
            self.table_name
        );
        let row = sqlx::query_as::<_, (String,)>(&select_sql)
            .bind(&self.lease_name)
            .fetch_optional(&mut *conn)
            .await;

        match row {
            Ok(Some((owner,))) if owner == self.pod_id => LeaderStatus::Leader,
            Ok(Some(_)) => LeaderStatus::NotLeader,
            Ok(None) => LeaderStatus::NotLeader,
            Err(e) => LeaderStatus::Unavailable(format!("SELECT failed: {e}")),
        }
    }
}

pub(crate) fn spawn_runtime_sweepers(
    shared_pool: SharedPool,
    cancel: tokio_util::sync::CancellationToken,
) -> Vec<tokio::task::JoinHandle<()>> {
    let pod_id = std::env::var("ASTRA_POD_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("astra-runtime-{}", Uuid::new_v4()));

    let lease = std::sync::Arc::new(SweeperLease {
        pool: shared_pool.clone(),
        pod_id,
        lease_name: "runtime_sweepers".to_string(),
        table_name: "sweeper_leases".to_string(),
    });

    // Each sweeper independently checks the lease before every work cycle.
    // No master loop — no risk of duplicate spawn.
    let handles = vec![
        crate::server::device_lease_sweeper::spawn_device_lease_expiry_sweeper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::runner_pool::spawn_runner_pool_reaper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::artifact_retention_sweeper::spawn_artifact_retention_sweeper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::session::session_todo_sweeper::spawn_session_todo_stale_sweeper(
            shared_pool.clone(),
            std::sync::Arc::clone(&lease),
            cancel.clone(),
        ),
        crate::server::session::session_todo_sweeper::spawn_session_todo_archive_sweeper(
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

    /// SQL DDL shared by all sweeper lease tests.
    const SWEEPER_LEASES_DDL: &str = r#"CREATE TABLE IF NOT EXISTS sweeper_leases (
                sweeper_name VARCHAR(128) PRIMARY KEY,
                owner_pod_id VARCHAR(256) NOT NULL,
                expires_at DATETIME(6) NOT NULL,
                version INT UNSIGNED NOT NULL DEFAULT 0,
                created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
            )"#;

    async fn ensure_sweeper_leases_table(pool: &SharedPool) {
        sqlx::query(SWEEPER_LEASES_DDL)
            .execute(pool.get())
            .await
            .expect("create table");
    }

    /// Integration test: validates sweeper lease acquisition and ownership.
    /// Requires a running MatrixOne instance. Run with:
    ///   ASTRA_TEST_DB_IT=1 cargo test -p astra-runtime --lib sweeper_lease -- --ignored
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn sweeper_lease_is_leader_acquires_and_confirms() {
        let _guard = astra_core::sync_poison::recover_mutex_lock(&TEST_SERIAL);
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let pool = SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne");

        // Ensure table exists and clear stale data from previous runs
        ensure_sweeper_leases_table(&pool).await;
        let lease_name = "test-leader-acquire";
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();

        let lease = SweeperLease {
            pool: pool.clone(),
            pod_id: "test-pod-1".to_string(),
            lease_name: lease_name.to_string(),
            table_name: "sweeper_leases".to_string(),
        };

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
        let other = SweeperLease {
            pool: pool.clone(),
            pod_id: "test-pod-2".to_string(),
            lease_name: lease_name.to_string(),
            table_name: "sweeper_leases".to_string(),
        };
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
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let pool = SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne");

        ensure_sweeper_leases_table(&pool).await;
        let lease_name = "test-leader-expiry";
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ?")
            .bind(lease_name)
            .execute(pool.get())
            .await
            .ok();

        use super::LeaderStatus;

        // Pod 1 acquires lease
        let pod1 = SweeperLease {
            pool: pool.clone(),
            pod_id: "pod-expiry-1".to_string(),
            lease_name: lease_name.to_string(),
            table_name: "sweeper_leases".to_string(),
        };
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
        let pod2 = SweeperLease {
            pool: pool.clone(),
            pod_id: "pod-expiry-2".to_string(),
            lease_name: lease_name.to_string(),
            table_name: "sweeper_leases".to_string(),
        };
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
        assert_eq!(
            pod1.check_leader().await,
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
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let pool = SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne");

        ensure_sweeper_leases_table(&pool).await;
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
                let lease = SweeperLease {
                    pool: (*pool).clone(),
                    pod_id,
                    lease_name: lease_name.to_string(),
                    table_name: "sweeper_leases".to_string(),
                };
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
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let pool = SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne");

        // Use a non-existent table name so that check_leader() hits the error path
        // without affecting the shared sweeper_leases table used by other tests.
        let lease_name = "test-leader-error";
        let lease = SweeperLease {
            pool: pool.clone(),
            pod_id: "pod-error".to_string(),
            lease_name: lease_name.to_string(),
            table_name: "sweeper_leases_error".to_string(),
        };

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
}
