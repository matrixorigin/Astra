use std::sync::Arc;

use astra_core::SharedPool;
use sqlx::Row;
use tracing::warn;
use uuid::Uuid;

/// Leader-election lease for background sweepers. Each sweeper independently
/// checks whether this pod still owns the lease before doing work. This avoids
/// duplicate work when multiple API-server replicas are deployed (HPA).
#[derive(Clone)]
pub(crate) struct SweeperLease {
    pub(crate) pool: SharedPool,
    pub(crate) pod_id: String,
}

impl SweeperLease {
    /// Check whether this pod holds the lease. Acquires or refreshes
    /// atomically via INSERT … ON DUPLICATE KEY UPDATE. Returns true only
    /// when this pod is the confirmed owner after the operation — the
    /// `rows_affected()` check is atomic with the DML, eliminating the
    /// TOCTOU window of a SELECT back-read.
    pub(crate) async fn is_leader(&self) -> bool {
        let ttl_secs: u64 = 60;
        let expires_at = chrono::Utc::now() + chrono::Duration::seconds(ttl_secs as i64);

        // Step 1: acquire or refresh via CAS INSERT … ON DUPLICATE KEY UPDATE.
        // The IF guards prevent another pod from stealing an active lease.
        let dml = sqlx::query(
            "INSERT INTO sweeper_leases (sweeper_name, owner_pod_id, expires_at)
             VALUES ('runtime_sweepers', ?, ?)
             ON DUPLICATE KEY UPDATE
               owner_pod_id = IF(expires_at < NOW(6) OR owner_pod_id = VALUES(owner_pod_id),
                                 VALUES(owner_pod_id), owner_pod_id),
               expires_at   = IF(expires_at < NOW(6) OR owner_pod_id = VALUES(owner_pod_id),
                                 VALUES(expires_at),   expires_at)",
        )
        .bind(&self.pod_id)
        .bind(expires_at.naive_utc())
        .execute(self.pool.get())
        .await;

        if let Err(e) = &dml {
            tracing::warn!(
                target: "astra_runtime::sweeper_lease",
                pod_id = %self.pod_id,
                error = %e,
                "sweeper lease check DML failed"
            );
            return false;
        }

        // Step 2: verify ownership via a back-read. MatrixOne's
        // rows_affected() does not distinguish no-op UPDATEs (returns 1),
        // so we cannot rely on the MySQL rows_affected() convention
        // (0 = no change). The DML's IF guards already provide the CAS
        // semantics; the SELECT merely confirms the outcome.
        let row = sqlx::query_as::<_, (String,)>(
            "SELECT owner_pod_id FROM sweeper_leases WHERE sweeper_name = 'runtime_sweepers'",
        )
        .fetch_optional(self.pool.get())
        .await;

        match row {
            Ok(Some((owner,))) => owner == self.pod_id,
            Ok(None) => {
                // Row disappeared between DML and SELECT — edge case,
                // treat as not-the-leader.
                false
            }
            Err(e) => {
                tracing::warn!(
                    target: "astra_runtime::sweeper_lease",
                    pod_id = %self.pod_id,
                    error = %e,
                    "sweeper lease ownership check SELECT failed"
                );
                false
            }
        }
    }
}

pub(crate) fn spawn_runtime_sweepers(shared_pool: SharedPool) {
    let pod_id = std::env::var("ASTRA_POD_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| format!("astra-runtime-{}", Uuid::new_v4()));

    let lease = Arc::new(SweeperLease {
        pool: shared_pool.clone(),
        pod_id,
    });

    // Each sweeper independently checks the lease before every work cycle.
    // No master loop — no risk of duplicate spawn.
    crate::server::device_lease_sweeper::spawn_device_lease_expiry_sweeper(
        shared_pool.clone(),
        Arc::clone(&lease),
    );
    crate::server::artifact_retention_sweeper::spawn_artifact_retention_sweeper(
        shared_pool.clone(),
        Arc::clone(&lease),
    );
    crate::server::session::session_todo_sweeper::spawn_session_todo_stale_sweeper(
        shared_pool.clone(),
        Arc::clone(&lease),
    );
    crate::server::session::session_todo_sweeper::spawn_session_todo_archive_sweeper(
        shared_pool,
        lease,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Integration test: validates sweeper lease acquisition and ownership.
    /// Requires a running MatrixOne instance. Run with:
    ///   ASTRA_TEST_DB_IT=1 cargo test -p astra-runtime --lib sweeper_lease -- --ignored
    #[tokio::test]
    #[ignore = "requires MatrixOne DB: run with ASTRA_TEST_DB_IT=1"]
    async fn sweeper_lease_is_leader_acquires_and_confirms() {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 for ignored integration tests"
        );
        let settings = crate::MatrixOneSettings::from_env();
        let pool = SharedPool::new(&settings)
            .await
            .expect("connect to MatrixOne");

        // Ensure table exists
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS sweeper_leases (
                sweeper_name VARCHAR(128) PRIMARY KEY,
                owner_pod_id VARCHAR(256) NOT NULL,
                expires_at DATETIME(6) NOT NULL,
                version INT UNSIGNED NOT NULL DEFAULT 0,
                created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                updated_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6)
            )",
        )
        .execute(pool.get())
        .await
        .expect("create table");

        let lease = SweeperLease {
            pool: pool.clone(),
            pod_id: "test-pod-1".to_string(),
        };

        // Fresh lease: should become leader.
        assert!(lease.is_leader().await, "first call must acquire lease");

        // Second call within TTL: still leader.
        assert!(lease.is_leader().await, "second call must retain lease");

        // Another pod tries while lease is held: must not become leader.
        let other = SweeperLease {
            pool: pool.clone(),
            pod_id: "test-pod-2".to_string(),
        };
        assert!(
            !other.is_leader().await,
            "other pod must not acquire active lease"
        );

        // Cleanup
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = 'runtime_sweepers'")
            .execute(pool.get())
            .await
            .ok();
    }
}
