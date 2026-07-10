use crate::cli::cloud_sync;
use crate::cli::session::session_state::SessionState;
use crate::{cli_dim, cli_info, cli_ok, cli_section, cli_warn};

/// Handle `/sync` without direct DB access.
///
/// Cloud transport is server-owned; the CLI owns the local durable outbox that
/// proves which edge facts still need cloud acknowledgement.
pub(crate) async fn handle_sync_command(arg: &str, _state: &SessionState) {
    let sub = arg.trim();
    let store = astra_services::SyncOutboxStore::local();
    cli_section!("Sync Outbox");
    eprintln!();
    match sub {
        "" | "status" => match store.status() {
            Ok(status) => display_outbox_status(&status),
            Err(error) => {
                cli_warn!("Local sync outbox is unreadable: {error}");
                cli_dim!(
                    "Local work is still safe in the session journal; run `/sync repair` after fixing the local file error."
                );
            }
        },
        "log" => {
            cli_info!("Durable outbox file: {}", store.path().display());
            cli_dim!(
                "Cloud delivery is server-owned; this file is the local edge-to-cloud boundary."
            );
        }
        "retry" => match store.retry_deferred_now() {
            Ok(count) => {
                let report = cloud_sync::try_drain_sync_outbox(64).await;
                cli_ok!(
                    "Queued {count} retryable sync record(s); attempted {}, acked {}, retryable failures {}, terminal {}.",
                    report.attempted,
                    report.acked,
                    report.failed,
                    report.terminal
                );
                if !report.cloud_configured {
                    cli_dim!("ASTRA_API_URL is not configured; records remain durable locally.");
                }
            }
            Err(error) => {
                cli_warn!("Failed to update retry state: {error}");
            }
        },
        "repair" => match store.repair_retry_exhausted_poison() {
            Ok(count) => {
                let report = cloud_sync::try_drain_sync_outbox(64).await;
                cli_ok!(
                    "Repaired {count} retry-exhausted sync record(s); attempted {}, acked {}, retryable failures {}, terminal {}.",
                    report.attempted,
                    report.acked,
                    report.failed,
                    report.terminal
                );
                cli_dim!(
                    "Payload hash mismatch records remain poisoned until the conflicting source is inspected."
                );
                if !report.cloud_configured {
                    cli_dim!("ASTRA_API_URL is not configured; records remain durable locally.");
                }
            }
            Err(error) => {
                cli_warn!("Failed to repair sync outbox: {error}");
            }
        },
        _ => {
            cli_info!("Usage: /sync [status|log|retry|repair]");
            cli_dim!(
                "The CLI does not connect directly to MatrixOne; it manages the local durable outbox."
            );
        }
    }
    eprintln!();
}

fn display_outbox_status(status: &astra_services::SyncOutboxStatus) {
    if status.total == 0 && status.skipped == 0 {
        cli_ok!("Clean: no local sync records are queued.");
        return;
    }
    if status.degraded {
        cli_warn!("Degraded: local records need attention before cloud convergence is complete.");
    } else if status.pending > 0 || status.in_flight > 0 {
        cli_info!("Syncing: local records are waiting for cloud acknowledgement.");
    } else {
        cli_ok!("Clean: all local records are acknowledged.");
    }
    eprintln!("  total: {}", status.total);
    eprintln!("  pending: {} (ready: {})", status.pending, status.ready);
    eprintln!("  in_flight: {}", status.in_flight);
    if status.stale_in_flight > 0 {
        eprintln!("  stale_in_flight: {}", status.stale_in_flight);
    }
    eprintln!("  acked: {}", status.acked);
    if status.ack_tombstones > 0 {
        eprintln!("  ack_tombstones: {}", status.ack_tombstones);
    }
    if status.skipped > 0 {
        eprintln!("  skipped_local_only: {}", status.skipped);
        if let Some(event_type) = &status.last_skipped_event_type {
            eprintln!("  last_skipped_event_type: {event_type}");
        }
        if let Some(reason) = &status.last_skipped_reason {
            eprintln!("  last_skipped_reason: {reason}");
        }
    }
    eprintln!("  poisoned: {}", status.poisoned);
    eprintln!("  ack_watermark: {}", status.ack_watermark);
    if status.retry_deferred > 0 {
        eprintln!("  retry_deferred: {}", status.retry_deferred);
    }
    if let Some(error) = &status.last_error {
        eprintln!("  last_error: {error}");
    }
}
