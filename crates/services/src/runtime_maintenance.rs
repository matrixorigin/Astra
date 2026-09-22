//! Background maintenance for bounded runtime diagnostics and explicit delete
//! intents.
//!
//! This worker is deliberately limited to explicit user delete intents,
//! expired diagnostics, and runtime objects whose owner was already removed.
//! It does not transition or end agent_sessions, and it does not expire
//! session traces.
//! Durable conversation history remains available until the user deletes the
//! session through the normal API.

use crate::CancellationSafePoolConnection;

/// Bounded work configuration for runtime-storage maintenance.
#[derive(Debug, Clone)]
pub struct RuntimeMaintenancePolicy {
    /// Maximum rows examined or collected by each maintenance operation.
    pub batch_limit: u32,
    /// How long an explicit delete intent must remain untouched before a
    /// background sweep retries an interrupted hard delete.
    pub delete_intent_grace_secs: u64,
}

impl Default for RuntimeMaintenancePolicy {
    fn default() -> Self {
        Self {
            batch_limit: 500,
            delete_intent_grace_secs:
                crate::session_lifecycle::SESSION_DELETE_INTENT_RETRY_GRACE_SECS,
        }
    }
}

/// Summary of one runtime-storage maintenance sweep.
#[derive(Debug, Default)]
pub struct RuntimeMaintenanceSweepResult {
    /// Explicitly requested deletes that completed after an interrupted
    /// foreground hard-delete attempt.
    pub explicit_delete_intents_reconciled: u64,
    /// Old diagnostic request-context records removed after retention.
    pub model_request_context_events_expired: u64,
    /// Expired aggregate observation-identity collision diagnostics.
    pub observation_collision_receipts_expired: u64,
    /// Expired prompt-assembly request diagnostics removed after retention.
    pub prompt_request_records_expired: u64,
    /// Child prompt delta diagnostics removed before their parent requests.
    pub prompt_deltas_expired: u64,
    /// Expired fork-retention pins released after their grace period.
    pub expired_fork_pins_released: u64,
    /// Fork records whose child session no longer exists.
    pub orphaned_forks_collected: u64,
    /// Unpinned manifest nodes whose owner session no longer exists.
    pub orphaned_manifests_collected: u64,
    /// Segment references whose manifest node no longer exists.
    pub orphaned_manifest_references_collected: u64,
    /// Owner-deduplicated segment payloads with no remaining reference.
    pub unreferenced_segments_collected: u64,
    /// Terminal branch-control operation receipts removed after retention.
    pub work_branch_control_operations_expired: u64,
    /// Terminal or abandoned branch-creation operation receipts removed after
    /// retention.
    pub work_branch_creation_operations_expired: u64,
    /// Read attachments removed after their Server-issued expiry.
    pub expired_session_attachments_deleted: u64,
    /// Individual maintenance operations that failed after the other bounded
    /// operations in the sweep had completed.
    pub cleanup_errors: Vec<String>,
}

/// Reconcile durable delete intents, expire bounded diagnostics, and reclaim
/// runtime objects that are no longer reachable from a user session.
pub async fn maintain_runtime_storage(
    pool: &astra_core::SharedPool,
    fork_coordinator: Option<&crate::DatabaseSessionForkCoordinator>,
    policy: &RuntimeMaintenancePolicy,
) -> RuntimeMaintenanceSweepResult {
    let mut result = RuntimeMaintenanceSweepResult::default();
    match crate::session_lifecycle::reconcile_explicit_session_delete_intents(
        pool.get(),
        policy.delete_intent_grace_secs,
        policy.batch_limit,
    )
    .await
    {
        Ok(reconciliation) => {
            result.explicit_delete_intents_reconciled = reconciliation.completed;
            result.cleanup_errors.extend(reconciliation.cleanup_errors);
        }
        Err(error) => {
            record_runtime_storage_maintenance(
                &mut result.cleanup_errors,
                "reconcile_explicit_session_delete_intents",
                Err(error),
            );
        }
    }
    result.model_request_context_events_expired = record_runtime_storage_maintenance(
        &mut result.cleanup_errors,
        "expire_model_request_context_events",
        crate::model_request_context::expire_model_request_context_events(pool, policy.batch_limit)
            .await,
    );
    result.observation_collision_receipts_expired = record_runtime_storage_maintenance(
        &mut result.cleanup_errors,
        "expire_observation_collision_receipts",
        expire_observation_collision_receipts(pool, policy.batch_limit).await,
    );
    match crate::prompt_delta::expire_prompt_diagnostics(pool, policy.batch_limit).await {
        Ok(expiry) => {
            result.prompt_request_records_expired = expiry.prompt_request_records;
            result.prompt_deltas_expired = expiry.prompt_deltas;
        }
        Err(error) => {
            record_runtime_storage_maintenance(
                &mut result.cleanup_errors,
                "expire_prompt_diagnostics",
                Err(error),
            );
        }
    }
    result.work_branch_control_operations_expired = record_runtime_storage_maintenance(
        &mut result.cleanup_errors,
        "expire_work_branch_control_operations",
        expire_work_branch_control_operations(pool, policy.batch_limit).await,
    );
    result.work_branch_creation_operations_expired = record_runtime_storage_maintenance(
        &mut result.cleanup_errors,
        "expire_work_branch_creation_operations",
        expire_work_branch_creation_operations(pool, policy.batch_limit).await,
    );
    result.expired_session_attachments_deleted = record_runtime_storage_maintenance(
        &mut result.cleanup_errors,
        "expire_session_attachments",
        expire_session_attachments(pool, policy.batch_limit).await,
    );
    if let Some(coordinator) = fork_coordinator {
        result.expired_fork_pins_released = record_runtime_storage_maintenance(
            &mut result.cleanup_errors,
            "release_expired_fork_pins",
            coordinator
                .release_expired_grace_pins(policy.batch_limit)
                .await,
        );
        result.orphaned_forks_collected = record_runtime_storage_maintenance(
            &mut result.cleanup_errors,
            "collect_orphaned_fork_records",
            coordinator
                .collect_orphaned_fork_records(policy.batch_limit)
                .await,
        );
        result.orphaned_manifests_collected = record_runtime_storage_maintenance(
            &mut result.cleanup_errors,
            "collect_unpinned_orphan_manifests",
            coordinator
                .collect_unpinned_orphan_manifests(policy.batch_limit)
                .await,
        );
        result.orphaned_manifest_references_collected = record_runtime_storage_maintenance(
            &mut result.cleanup_errors,
            "collect_orphaned_manifest_segment_references",
            coordinator
                .collect_orphaned_manifest_segment_references(policy.batch_limit)
                .await,
        );
        result.unreferenced_segments_collected = record_runtime_storage_maintenance(
            &mut result.cleanup_errors,
            "collect_unreferenced_conversation_segments",
            coordinator
                .collect_unreferenced_conversation_segments(policy.batch_limit)
                .await,
        );
    }
    result
}

impl RuntimeMaintenanceSweepResult {
    #[must_use]
    pub fn total_processed(&self) -> u64 {
        self.explicit_delete_intents_reconciled
            .saturating_add(self.model_request_context_events_expired)
            .saturating_add(self.observation_collision_receipts_expired)
            .saturating_add(self.prompt_request_records_expired)
            .saturating_add(self.prompt_deltas_expired)
            .saturating_add(self.expired_fork_pins_released)
            .saturating_add(self.orphaned_forks_collected)
            .saturating_add(self.orphaned_manifests_collected)
            .saturating_add(self.orphaned_manifest_references_collected)
            .saturating_add(self.unreferenced_segments_collected)
            .saturating_add(self.work_branch_control_operations_expired)
            .saturating_add(self.work_branch_creation_operations_expired)
            .saturating_add(self.expired_session_attachments_deleted)
    }
}

const WORK_BRANCH_OPERATION_RETENTION_DAYS: u32 = 30;
const ABANDONED_WORK_BRANCH_OPERATION_RETENTION_DAYS: u32 = 1;

async fn expire_observation_collision_receipts(
    pool: &astra_core::SharedPool,
    batch_limit: u32,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "DELETE FROM observation_identity_collisions
         WHERE expires_at <= NOW(6)
         ORDER BY expires_at, user_id, identity_kind, identity_id
         LIMIT ?",
    )
    .bind(batch_limit)
    .execute(pool.get())
    .await
    .map(|outcome| outcome.rows_affected())
}

async fn expire_work_branch_control_operations(
    pool: &astra_core::SharedPool,
    batch_limit: u32,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "DELETE FROM work_branch_control_operations
         WHERE completed_at < DATE_SUB(NOW(6), INTERVAL ? DAY)
            OR (operation_state = 'pending'
                AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY))
         ORDER BY COALESCE(completed_at, created_at) ASC, operation_id ASC
         LIMIT ?",
    )
    .bind(WORK_BRANCH_OPERATION_RETENTION_DAYS)
    .bind(ABANDONED_WORK_BRANCH_OPERATION_RETENTION_DAYS)
    .bind(batch_limit)
    .execute(pool.get())
    .await
    .map(|outcome| outcome.rows_affected())
}

async fn expire_work_branch_creation_operations(
    pool: &astra_core::SharedPool,
    batch_limit: u32,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "DELETE FROM work_branch_creation_operations
         WHERE completed_at < DATE_SUB(NOW(6), INTERVAL ? DAY)
            OR (operation_state = 'pending'
                AND created_at < DATE_SUB(NOW(6), INTERVAL ? DAY))
         ORDER BY COALESCE(completed_at, created_at) ASC, operation_id ASC
         LIMIT ?",
    )
    .bind(WORK_BRANCH_OPERATION_RETENTION_DAYS)
    .bind(ABANDONED_WORK_BRANCH_OPERATION_RETENTION_DAYS)
    .bind(batch_limit)
    .execute(pool.get())
    .await
    .map(|outcome| outcome.rows_affected())
}

async fn expire_session_attachments(
    pool: &astra_core::SharedPool,
    batch_limit: u32,
) -> Result<u64, sqlx::Error> {
    let mut connection = CancellationSafePoolConnection::acquire(pool.get()).await?;
    let mut tx = connection.begin().await?;
    let database_now_ms = crate::db_row::database_now_unix_ms(&mut tx).await?;
    let rows_affected = sqlx::query(
        "DELETE FROM session_attachments
         WHERE expires_at_ms <= ?
         ORDER BY expires_at_ms ASC
         LIMIT ?",
    )
    .bind(database_now_ms)
    .bind(batch_limit)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    connection.release();
    Ok(rows_affected)
}

fn record_runtime_storage_maintenance<E: std::fmt::Display>(
    errors: &mut Vec<String>,
    operation: &'static str,
    outcome: Result<u64, E>,
) -> u64 {
    match outcome {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                target: "astra_services::runtime_maintenance",
                %operation,
                error = %error,
                "runtime storage maintenance step failed"
            );
            errors.push(format!("{operation}: {error}"));
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires MatrixOne; creates and removes an isolated collision-expiry database"]
    async fn collision_expiry_removes_only_expired_receipts_within_limit() {
        use sqlx::Connection;
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let mut settings = astra_core::MatrixOneSettings::from_env();
        assert_eq!(
            std::env::var("ASTRA_TEST_DATABASE").as_deref(),
            Ok(settings.database.as_str())
        );
        let source = settings.database.clone();
        assert!(
            source
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        );
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        crate::storage::ensure_core_schema(&settings, &catalog)
            .await
            .unwrap();
        let mut admin = sqlx::MySqlConnection::connect(&settings.database_url_with_password())
            .await
            .unwrap();
        settings.database = format!("astra_test_collision_ttl_{}", uuid::Uuid::new_v4().simple());
        settings.db_pool_max_connections = 1;
        settings.db_pool_min_connections = 1;
        sqlx::query(&format!("CREATE DATABASE `{}`", settings.database))
            .execute(&mut admin)
            .await
            .unwrap();
        // Clone only the production table: unrelated expired fixtures cannot
        // consume LIMIT 1, and no unrelated maintenance work enters this test.
        let result: Result<_, sqlx::Error> = async {
            sqlx::query(&format!("CREATE TABLE `{}`.observation_identity_collisions LIKE `{source}`.observation_identity_collisions", settings.database))
                .execute(&mut admin).await?;
            let pool = astra_core::SharedPool::new(&settings).await?;
            sqlx::query("INSERT INTO observation_identity_collisions (user_id, identity_kind, identity_id, stored_payload_hash, attempted_payload_hash, source, expires_at) VALUES ('expired-older','agent_event','one','a','b','ttl-test',DATE_SUB(NOW(6),INTERVAL 2 SECOND)), ('expired','agent_event','one','a','b','ttl-test',DATE_SUB(NOW(6),INTERVAL 1 SECOND)), ('retained','agent_event','one','a','b','ttl-test',DATE_ADD(NOW(6),INTERVAL 7 DAY))")
                .execute(pool.get()).await?;
            let deleted = expire_observation_collision_receipts(&pool, 1).await?;
            let after_first: Vec<String> = sqlx::query_scalar("SELECT user_id FROM observation_identity_collisions ORDER BY user_id")
                .fetch_all(pool.get()).await?;
            let repeated = expire_observation_collision_receipts(&pool, 1).await?;
            let exhausted = expire_observation_collision_receipts(&pool, 1).await?;
            let owners: Vec<String> = sqlx::query_scalar("SELECT user_id FROM observation_identity_collisions ORDER BY user_id")
                .fetch_all(pool.get()).await?;
            pool.close().await;
            Ok((deleted, after_first, repeated, exhausted, owners))
        }.await;
        // The destructive target is exactly the randomly named database above.
        sqlx::query(&format!("DROP DATABASE `{}`", settings.database))
            .execute(&mut admin)
            .await
            .unwrap();
        let (deleted, after_first, repeated, exhausted, owners) = result.unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(after_first, ["expired", "retained"]);
        assert_eq!(repeated, 1);
        assert_eq!(exhausted, 0);
        assert_eq!(owners, ["retained"]);
    }

    #[test]
    fn default_policy_is_bounded() {
        assert_eq!(RuntimeMaintenancePolicy::default().batch_limit, 500);
        assert_eq!(
            RuntimeMaintenancePolicy::default().delete_intent_grace_secs,
            crate::session_lifecycle::SESSION_DELETE_INTENT_RETRY_GRACE_SECS
        );
    }
}
