use std::{sync::Arc, time::Duration};

use astra_core::SharedPool;

pub(crate) fn spawn_runtime_maintenance_sweeper(
    pool: SharedPool,
    fork_coordinator: Option<Arc<astra_services::DatabaseSessionForkCoordinator>>,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let policy = astra_services::runtime_maintenance::RuntimeMaintenancePolicy::default();
        let mut interval = tokio::time::interval(Duration::from_secs(5 * 60));
        interval.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {}
            }

            match lease.check_leader().await {
                crate::server::sweeper_lease::LeaderStatus::Leader => {}
                crate::server::sweeper_lease::LeaderStatus::NotLeader => continue,
                crate::server::sweeper_lease::LeaderStatus::Unavailable(error) => {
                    tracing::warn!(
                        target: "astra_runtime::runtime_maintenance_sweeper",
                        %error,
                        "runtime maintenance lease check unavailable, skipping sweep"
                    );
                    continue;
                }
            }

            let result = astra_services::runtime_maintenance::maintain_runtime_storage(
                &pool,
                fork_coordinator.as_deref(),
                &policy,
            )
            .await;
            if result.total_processed() > 0 || !result.cleanup_errors.is_empty() {
                tracing::info!(
                    target: "astra_runtime::runtime_maintenance_sweeper",
                    explicit_delete_intents_reconciled = result.explicit_delete_intents_reconciled,
                    model_request_context_events_expired = result.model_request_context_events_expired,
                    prompt_request_records_expired = result.prompt_request_records_expired,
                    prompt_deltas_expired = result.prompt_deltas_expired,
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
