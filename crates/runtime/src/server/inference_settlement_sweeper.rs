use std::sync::Arc;

use astra_core::SharedPool;

const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);
const SWEEP_BATCH_LIMIT: u32 = 256;

pub(crate) fn spawn_inference_settlement_sweeper(
    pool: SharedPool,
    lease: Arc<crate::server::sweeper_lease::SweeperLease>,
    cancel: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(SWEEP_INTERVAL);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    match lease.check_leader().await {
                        crate::server::sweeper_lease::LeaderStatus::Leader => {}
                        crate::server::sweeper_lease::LeaderStatus::NotLeader => continue,
                        crate::server::sweeper_lease::LeaderStatus::Unavailable(error) => {
                            tracing::warn!(
                                target: "astra_runtime::inference_settlement_sweeper",
                                %error,
                                "inference settlement leadership unavailable; retrying later"
                            );
                            continue;
                        }
                    }
                    match astra_services::reconcile_inference_settlements(
                        &pool,
                        SWEEP_BATCH_LIMIT,
                    )
                    .await
                    {
                        Ok(0) => {}
                        Ok(reconciled) => tracing::info!(
                            target: "astra_runtime::inference_settlement_sweeper",
                            reconciled,
                            "reconciled durable inference settlements"
                        ),
                        Err(error) => tracing::warn!(
                            target: "astra_runtime::inference_settlement_sweeper",
                            %error,
                            "inference settlement sweep failed; retrying later"
                        ),
                    }
                }
            }
        }
    })
}
