use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::metrics::SharedMultiAgentMetrics;
use super::metrics::saturating_decrement;
use super::task_lease::TaskLeaseService;

/// Identity and dependencies for a single lease renewal loop.
pub struct LeaseRenewalConfig {
    pub lease_svc: Arc<dyn TaskLeaseService>,
    pub user_id: Arc<String>,
    pub task_id: Arc<String>,
    pub agent_id: Arc<String>,
    pub edge_id: Arc<String>,
    pub ttl_sec: i64,
    pub metrics: Option<SharedMultiAgentMetrics>,
}

/// Keeps a task lease alive in the background for a single claimed task.
pub struct LeaseRenewalTask {
    cancel: Arc<AtomicBool>,
    cancel_notify: Arc<tokio::sync::Notify>,
    handle: Option<tokio::task::JoinHandle<()>>,
    metrics: Option<SharedMultiAgentMetrics>,
}

impl LeaseRenewalTask {
    /// Spawns a renewal loop that refreshes the lease until cancelled.
    pub fn spawn(config: LeaseRenewalConfig) -> Self {
        let LeaseRenewalConfig {
            lease_svc,
            user_id,
            task_id,
            agent_id,
            edge_id,
            ttl_sec,
            metrics,
        } = config;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_notify = Arc::new(tokio::sync::Notify::new());
        let interval_secs = (ttl_sec / 2).max(2) as u64;
        if let Some(metrics) = metrics.as_ref() {
            metrics
                .active_lease_renewals
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let handle = {
            let cancel = cancel.clone();
            let cancel_notify = cancel_notify.clone();
            tokio::spawn(async move {
                loop {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(std::time::Duration::from_secs(interval_secs)) => {}
                        _ = cancel_notify.notified() => {}
                    }
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    match lease_svc
                        .renew_lease_held(
                            user_id.as_str(),
                            task_id.as_str(),
                            agent_id.as_str(),
                            edge_id.as_str(),
                            ttl_sec,
                        )
                        .await
                    {
                        Ok(true) => {}
                        Ok(false) => tracing::warn!(
                            task_id = %task_id.as_str(),
                            "lease renewal lost ownership or lease expired"
                        ),
                        Err(e) => tracing::warn!(
                            task_id = %task_id.as_str(),
                            error = %e,
                            "lease renewal failed"
                        ),
                    }
                }
            })
        };

        Self {
            cancel,
            cancel_notify,
            handle: Some(handle),
            metrics,
        }
    }

    /// Stops the renewal loop cooperatively and waits for it to exit.
    pub async fn cancel_and_wait(&mut self) -> Result<(), tokio::task::JoinError> {
        self.cancel.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
        if let Some(handle) = self.handle.take() {
            handle.await?;
        }
        Ok(())
    }
}

impl Drop for LeaseRenewalTask {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        self.cancel_notify.notify_waiters();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
        if let Some(metrics) = self.metrics.as_ref() {
            saturating_decrement(&metrics.active_lease_renewals);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{LeaseRenewalConfig, LeaseRenewalTask};
    use crate::TaskLeaseService;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use tokio::time::{Duration, timeout};

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn cooperative_cancel_and_await_has_no_late_renew() {
        let cancel = Arc::new(AtomicBool::new(false));
        let notify = Arc::new(tokio::sync::Notify::new());
        let renew_count = Arc::new(AtomicU32::new(0));
        let renew_fired = Arc::new(tokio::sync::Notify::new());

        let handle = {
            let cancel = cancel.clone();
            let notify = notify.clone();
            let renew_count = renew_count.clone();
            let renew_fired = renew_fired.clone();
            tokio::spawn(async move {
                loop {
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(50)) => {}
                        _ = notify.notified() => {}
                    }
                    if cancel.load(Ordering::Acquire) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    renew_count.fetch_add(1, Ordering::SeqCst);
                    renew_fired.notify_one();
                }
            })
        };

        timeout(Duration::from_secs(1), renew_fired.notified())
            .await
            .expect("test scaffolding: renew should have fired at least once");

        cancel.store(true, Ordering::Release);
        notify.notify_waiters();
        let _ = handle.await;

        let after_await = renew_count.load(Ordering::SeqCst);
        let after_wait = timeout(Duration::from_millis(150), renew_fired.notified()).await;
        assert!(
            after_wait.is_err(),
            "renew fired after the awaited handle completed"
        );
        assert_eq!(
            after_await,
            renew_count.load(Ordering::SeqCst),
            "renew counter changed after the awaited handle completed"
        );
    }

    struct CountingRenewLeaseService {
        renew_count: Arc<AtomicU32>,
        renew_fired: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl TaskLeaseService for CountingRenewLeaseService {
        async fn claim_next_claimable_lease(
            &self,
            _user_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<crate::NextClaimableLeaseClaimResult, String> {
            unreachable!("not used in this test");
        }

        async fn try_claim_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<crate::LeaseClaimResult, String> {
            unreachable!("not used in this test");
        }

        async fn release_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
        ) -> Result<bool, String> {
            unreachable!("not used in this test");
        }

        async fn get_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
        ) -> Result<Option<crate::TaskLeaseView>, String> {
            unreachable!("not used in this test");
        }

        async fn renew_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<Option<crate::TaskLeaseView>, String> {
            self.renew_count.fetch_add(1, Ordering::SeqCst);
            self.renew_fired.notify_one();
            Ok(None)
        }
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn lease_renewal_task_cancel_and_wait_has_no_late_renew() {
        let renew_count = Arc::new(AtomicU32::new(0));
        let renew_fired = Arc::new(tokio::sync::Notify::new());
        let lease_svc = Arc::new(CountingRenewLeaseService {
            renew_count: renew_count.clone(),
            renew_fired: renew_fired.clone(),
        });
        let mut task = LeaseRenewalTask::spawn(LeaseRenewalConfig {
            lease_svc,
            user_id: Arc::new("task-user".to_string()),
            task_id: Arc::new("task-1".to_string()),
            agent_id: Arc::new("agent-1".to_string()),
            edge_id: Arc::new("edge-1".to_string()),
            ttl_sec: 1,
            metrics: None,
        });

        timeout(Duration::from_secs(3), renew_fired.notified())
            .await
            .expect("test scaffolding: renewal should fire at least once");

        task.cancel_and_wait()
            .await
            .expect("lease renewal task should stop cleanly");
        let after_cancel = renew_count.load(Ordering::SeqCst);
        let after_wait = timeout(Duration::from_millis(150), renew_fired.notified()).await;
        assert!(
            after_wait.is_err(),
            "renew fired after cancel_and_wait returned"
        );
        assert_eq!(
            after_cancel,
            renew_count.load(Ordering::SeqCst),
            "renew fired after cancel_and_wait returned"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn lease_renewal_task_drop_aborts_background_loop() {
        let renew_count = Arc::new(AtomicU32::new(0));
        let renew_fired = Arc::new(tokio::sync::Notify::new());
        let lease_svc = Arc::new(CountingRenewLeaseService {
            renew_count: renew_count.clone(),
            renew_fired: renew_fired.clone(),
        });
        let task = LeaseRenewalTask::spawn(LeaseRenewalConfig {
            lease_svc,
            user_id: Arc::new("task-user".to_string()),
            task_id: Arc::new("task-1".to_string()),
            agent_id: Arc::new("agent-1".to_string()),
            edge_id: Arc::new("edge-1".to_string()),
            ttl_sec: 1,
            metrics: None,
        });

        drop(task);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;

        assert_eq!(renew_count.load(Ordering::SeqCst), 0);
    }
}
