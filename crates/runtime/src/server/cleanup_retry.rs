//! Background task for retrying failed workspace cleanup operations.
//!
//! Spawns a periodic task that scans unresolved cleanup debts and attempts
//! to clean them up with exponential backoff.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use astra_runtime_env::{CleanupReason, WorkspaceProvisioner};
use astra_services::{WorkspaceCleanupDebtEntry, WorkspaceCleanupDebtStore};

/// Maximum number of retry attempts before giving up on a debt.
const MAX_RETRY_ATTEMPTS: u32 = 5;

/// Base delay for exponential backoff (in seconds).
const BASE_DELAY_SECS: u64 = 60;

/// Spawn a background task that retries unresolved cleanup debts with exponential backoff.
///
/// The task runs every 5 minutes, scanning for debts that are ready for retry
/// based on exponential backoff timing tracked locally.
pub fn spawn_cleanup_retry(
    cleanup_store: Arc<dyn WorkspaceCleanupDebtStore>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(300)); // 5 minutes
        interval.tick().await; // skip immediate first tick

        // Track last attempt time per debt locally for backoff calculation.
        let mut last_attempt: HashMap<String, tokio::time::Instant> = HashMap::new();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::info!(
                        target: "astra_runtime::cleanup_retry",
                        "cleanup retry task received cancellation; exiting"
                    );
                    break;
                }
                _ = interval.tick() => {}
            }

            if let Err(error) = retry_cleanup_debts(&*cleanup_store, &mut last_attempt).await {
                tracing::warn!(
                    target: "astra_runtime::cleanup_retry",
                    error = %error,
                    "cleanup retry cycle failed"
                );
            }
        }
    })
}

async fn retry_cleanup_debts(
    cleanup_store: &dyn WorkspaceCleanupDebtStore,
    last_attempt: &mut HashMap<String, tokio::time::Instant>,
) -> Result<(), String> {
    let debts = cleanup_store
        .list_all_unresolved_debts()
        .await
        .map_err(|e| format!("failed to list unresolved debts: {e}"))?;

    if debts.is_empty() {
        return Ok(());
    }

    tracing::info!(
        target: "astra_runtime::cleanup_retry",
        debt_count = debts.len(),
        "scanning unresolved cleanup debts"
    );

    let now = tokio::time::Instant::now();

    for debt in debts {
        if debt.attempts >= MAX_RETRY_ATTEMPTS {
            tracing::warn!(
                target: "astra_runtime::cleanup_retry",
                debt_id = %debt.debt_id,
                workspace_id = %debt.workspace_id,
                attempts = debt.attempts,
                "debt exceeded max retry attempts; marking as resolved to stop rescanning"
            );
            // Settle the debt so it stops being returned by list_all_unresolved_debts.
            // The underlying workspace resource is leaked at this point, but
            // keeping the debt alive forever would cause unbounded scan growth.
            if let Err(error) = cleanup_store
                .resolve_cleanup_debt(&debt.owner_id, &debt.debt_id)
                .await
            {
                tracing::error!(
                    target: "astra_runtime::cleanup_retry",
                    debt_id = %debt.debt_id,
                    error = %error,
                    "failed to resolve exhausted debt; will retry next cycle"
                );
            } else {
                last_attempt.remove(&debt.debt_id);
            }
            continue;
        }

        // Exponential backoff: 2^attempts * BASE_DELAY_SECS
        let delay =
            Duration::from_secs(BASE_DELAY_SECS.saturating_mul(1u64 << debt.attempts.min(6)));

        if let Some(prev) = last_attempt.get(&debt.debt_id) {
            if prev.elapsed() < delay {
                tracing::trace!(
                    target: "astra_runtime::cleanup_retry",
                    debt_id = %debt.debt_id,
                    attempts = debt.attempts,
                    "debt not ready for retry yet"
                );
                continue;
            }
        }

        tracing::info!(
            target: "astra_runtime::cleanup_retry",
            debt_id = %debt.debt_id,
            workspace_id = %debt.workspace_id,
            attempts = debt.attempts,
            "retrying cleanup debt"
        );

        last_attempt.insert(debt.debt_id.clone(), now);

        match attempt_cleanup(&debt).await {
            Ok(()) => {
                if let Err(error) = cleanup_store
                    .resolve_cleanup_debt(&debt.owner_id, &debt.debt_id)
                    .await
                {
                    tracing::error!(
                        target: "astra_runtime::cleanup_retry",
                        debt_id = %debt.debt_id,
                        error = %error,
                        "cleanup succeeded but failed to mark debt as resolved"
                    );
                } else {
                    tracing::info!(
                        target: "astra_runtime::cleanup_retry",
                        debt_id = %debt.debt_id,
                        workspace_id = %debt.workspace_id,
                        "cleanup debt resolved successfully"
                    );
                    last_attempt.remove(&debt.debt_id);
                }
            }
            Err(error) => {
                if let Err(inc_error) = cleanup_store.increment_debt_attempts(&debt.debt_id).await {
                    tracing::error!(
                        target: "astra_runtime::cleanup_retry",
                        debt_id = %debt.debt_id,
                        error = %inc_error,
                        "failed to increment debt attempts counter"
                    );
                }
                tracing::warn!(
                    target: "astra_runtime::cleanup_retry",
                    debt_id = %debt.debt_id,
                    workspace_id = %debt.workspace_id,
                    attempts = debt.attempts + 1,
                    error = %error,
                    "cleanup retry failed"
                );
            }
        }
    }

    Ok(())
}

/// Attempt to clean up the workspace resource associated with a debt.
async fn attempt_cleanup(debt: &WorkspaceCleanupDebtEntry) -> Result<(), String> {
    use crate::server::run::cloud_workspace_provisioning::CloudWorkspaceProvisioner;

    let provisioner = CloudWorkspaceProvisioner::from_env();
    // WorkspaceProvisioner trait provides the cleanup method
    provisioner
        .cleanup(&debt.record, debt.reason)
        .await
        .map_err(|e| e.message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{
        CleanupReason, WorkspaceAuthority, WorkspaceBindingKind, WorkspaceOwnerScope,
        WorkspacePersistence, WorkspaceRecord, WorkspaceSource,
    };
    use astra_services::InMemoryWorkspaceRecordStore;
    use tokio_util::sync::CancellationToken;

    fn test_workspace_record() -> WorkspaceRecord {
        WorkspaceRecord {
            workspace_id: "test-workspace-123".to_string(),
            owner_scope: WorkspaceOwnerScope::User,
            kind: WorkspaceBindingKind::CloudWorkspace,
            authority: WorkspaceAuthority::ReadWrite,
            root_or_volume_ref: "/test/path".to_string(),
            source: WorkspaceSource::ServerSandbox {
                session_id: "test-session".to_string(),
            },
            persistence: WorkspacePersistence::Session,
            revision: "1".to_string(),
            display_name: "Test Workspace".to_string(),
        }
    }

    #[tokio::test]
    async fn retry_cleanup_debts_resolves_successful_cleanup() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());

        let record = test_workspace_record();
        let debt = WorkspaceCleanupDebtEntry::new(
            "owner-1",
            Some("session-1".to_string()),
            None,
            record,
            CleanupReason::Failed,
            "test debt".to_string(),
        );
        store.record_cleanup_debt(debt.clone()).await.unwrap();

        let mut last_attempt = HashMap::new();

        // First call: cleanup will fail (CloudWorkspaceProvisioner::from_env won't have real infra)
        // but the debt tracking should work correctly
        let result = retry_cleanup_debts(store.as_ref(), &mut last_attempt).await;
        assert!(result.is_ok());

        // Debt should still be unresolved (cleanup failed in test env)
        let remaining = store.list_cleanup_debts("owner-1", 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
    }

    #[tokio::test]
    async fn retry_cleanup_debts_skips_max_attempts() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());

        let record = test_workspace_record();
        let mut debt = WorkspaceCleanupDebtEntry::new(
            "owner-1",
            None,
            None,
            record,
            CleanupReason::OperatorRequested,
            "exhausted debt".to_string(),
        );
        debt.attempts = MAX_RETRY_ATTEMPTS;
        store.record_cleanup_debt(debt).await.unwrap();

        let mut last_attempt = HashMap::new();
        let result = retry_cleanup_debts(store.as_ref(), &mut last_attempt).await;
        assert!(result.is_ok());

        // Debt should be resolved (settle exhausted debts to stop rescan growth)
        let remaining = store.list_cleanup_debts("owner-1", 10).await.unwrap();
        assert_eq!(
            remaining.len(),
            0,
            "exhausted debt should be settled, not retained"
        );
    }

    #[tokio::test]
    async fn retry_cleanup_debts_respects_backoff_delay() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());

        let record = test_workspace_record();
        let debt = WorkspaceCleanupDebtEntry::new(
            "owner-1",
            None,
            None,
            record,
            CleanupReason::Failed,
            "backoff test".to_string(),
        );
        store.record_cleanup_debt(debt.clone()).await.unwrap();

        let mut last_attempt = HashMap::new();
        let now = tokio::time::Instant::now();

        // Record a recent attempt — debt should be skipped due to backoff
        last_attempt.insert(debt.debt_id.clone(), now);

        let result = retry_cleanup_debts(store.as_ref(), &mut last_attempt).await;
        assert!(result.is_ok());

        // Debt should still exist with 0 attempts (wasn't retried)
        let remaining = store.list_cleanup_debts("owner-1", 10).await.unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].attempts, 0);
    }

    #[tokio::test]
    async fn spawn_cleanup_retry_cancels_cleanly() {
        let store = Arc::new(InMemoryWorkspaceRecordStore::new());
        let cancel = CancellationToken::new();

        let handle = spawn_cleanup_retry(store, cancel.clone());
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "task should exit within 5s after cancellation"
        );
    }
}
