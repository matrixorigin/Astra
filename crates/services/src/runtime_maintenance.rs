//! Background maintenance for bounded runtime diagnostics and explicit delete
//! intents.
//!
//! This worker is deliberately limited to explicit user delete intents,
//! expired diagnostics, and runtime objects whose owner was already removed.
//! It also cancels expired, unused provider fork preparations through their
//! existing abort and Session deletion owners. It does not expire
//! session traces.
//! Durable conversation history remains available until the user deletes the
//! session through the normal API.

use crate::CancellationSafePoolConnection;

/// Bounded work configuration for runtime-storage maintenance.
#[derive(Debug, Clone)]
pub struct RuntimeMaintenancePolicy {
    /// Maximum candidate rows collected by each maintenance operation.
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
    /// Raw old diagnostic rows examined in this page, including pinned rows.
    pub model_request_context_events_examined: u64,
    /// Continue after the last examined row, even when no deletion was eligible.
    pub model_request_context_more: bool,
    /// Expired fork-retention pins released after their grace period.
    pub expired_fork_pins_released: u64,
    /// Unactivated provider forks cancelled after their retry window.
    pub expired_provider_prepares_aborted: u64,
    /// Fork records whose child session no longer exists.
    pub orphaned_forks_collected: u64,
    /// Unpinned manifest nodes whose owner session no longer exists.
    pub orphaned_manifests_collected: u64,
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
    context_cursor: &mut crate::model_request_context::ModelRequestContextExpiryCursor,
    notices: Option<&crate::runs::RunTreeNotices>,
) -> RuntimeMaintenanceSweepResult {
    let mut result = RuntimeMaintenanceSweepResult::default();
    match crate::session_lifecycle::reconcile_explicit_session_delete_intents(
        pool.get(),
        policy.delete_intent_grace_secs,
        policy.batch_limit,
        notices,
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
    match crate::model_request_context::expire_model_request_context_events(
        pool,
        policy.batch_limit,
        context_cursor,
    )
    .await
    {
        Ok(page) => {
            result.model_request_context_events_expired = page.deleted;
            result.model_request_context_events_examined = page.examined;
            result.model_request_context_more = page.more;
        }
        Err(error) => {
            record_runtime_storage_maintenance(
                &mut result.cleanup_errors,
                "expire_model_request_context_events",
                Err::<u64, _>(error),
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
        result.expired_provider_prepares_aborted = record_runtime_storage_maintenance(
            &mut result.cleanup_errors,
            "abort_expired_provider_prepares",
            coordinator
                .abort_expired_provider_prepares(policy.batch_limit)
                .await,
        );
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
    }
    result
}

impl RuntimeMaintenanceSweepResult {
    #[must_use]
    pub fn total_processed(&self) -> u64 {
        self.explicit_delete_intents_reconciled
            .saturating_add(self.model_request_context_events_expired)
            .saturating_add(self.expired_fork_pins_released)
            .saturating_add(self.expired_provider_prepares_aborted)
            .saturating_add(self.orphaned_forks_collected)
            .saturating_add(self.orphaned_manifests_collected)
            .saturating_add(self.work_branch_control_operations_expired)
            .saturating_add(self.work_branch_creation_operations_expired)
            .saturating_add(self.expired_session_attachments_deleted)
    }
}

const WORK_BRANCH_OPERATION_RETENTION_DAYS: u32 = 30;
const ABANDONED_WORK_BRANCH_OPERATION_RETENTION_DAYS: u32 = 1;

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

    #[test]
    fn default_policy_is_bounded() {
        assert_eq!(RuntimeMaintenancePolicy::default().batch_limit, 500);
        assert_eq!(
            RuntimeMaintenancePolicy::default().delete_intent_grace_secs,
            crate::session_lifecycle::SESSION_DELETE_INTENT_RETRY_GRACE_SECS
        );
    }
}
