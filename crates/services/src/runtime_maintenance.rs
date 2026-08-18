//! Background maintenance for bounded runtime diagnostics and explicit delete
//! intents.
//!
//! This worker is deliberately limited to explicit user delete intents,
//! expired diagnostics, and runtime objects whose owner was already removed.
//! It does not transition or end agent_sessions, and it does not expire
//! session traces.
//! Durable conversation history remains available until the user deletes the
//! session through the normal API.

use std::time::Duration;

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

/// Spawn the long-lived runtime-storage maintenance worker.
///
/// The worker receives the shared pool only for two narrow owner operations:
/// retrying durable deleting intents and expiring diagnostics. It never
/// derives deletion from session inactivity.
pub fn spawn_runtime_maintenance(
    pool: astra_core::SharedPool,
    fork_coordinator: Option<std::sync::Arc<crate::DatabaseSessionForkCoordinator>>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    let maintenance_interval = Duration::from_secs(5 * 60);

    tokio::spawn(async move {
        let policy = RuntimeMaintenancePolicy::default();
        let mut interval = tokio::time::interval(maintenance_interval);
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: "astra_services::runtime_maintenance",
                        "runtime storage maintenance received cancellation; exiting"
                    );
                    break;
                }
                _ = interval.tick() => {}
            }

            let result =
                maintain_runtime_storage(&pool, fork_coordinator.as_deref(), &policy).await;
            let total = result
                .explicit_delete_intents_reconciled
                .saturating_add(result.model_request_context_events_expired)
                .saturating_add(result.expired_fork_pins_released)
                .saturating_add(result.orphaned_forks_collected)
                .saturating_add(result.orphaned_manifests_collected)
                .saturating_add(result.orphaned_manifest_references_collected)
                .saturating_add(result.unreferenced_segments_collected);
            if total > 0 {
                tracing::info!(
                    target: "astra_services::runtime_maintenance",
                    explicit_delete_intents_reconciled = result.explicit_delete_intents_reconciled,
                    model_request_context_events_expired = result.model_request_context_events_expired,
                    expired_fork_pins_released = result.expired_fork_pins_released,
                    orphaned_forks_collected = result.orphaned_forks_collected,
                    orphaned_manifests_collected = result.orphaned_manifests_collected,
                    orphaned_manifest_references_collected = result.orphaned_manifest_references_collected,
                    unreferenced_segments_collected = result.unreferenced_segments_collected,
                    cleanup_error_count = result.cleanup_errors.len(),
                    "runtime storage maintenance sweep"
                );
            }
        }
    })
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
