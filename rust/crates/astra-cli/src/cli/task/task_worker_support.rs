/// Resolves the worker agent id used for task lease ownership.
pub(crate) fn default_task_agent_id() -> String {
    static DEFAULT_TASK_AGENT_ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    DEFAULT_TASK_AGENT_ID
        .get_or_init(|| {
            std::env::var("ASTRA_EDGE_AGENT_ID")
                .or_else(|_| std::env::var("HOSTNAME").map(|host| format!("astra-{host}")))
                .unwrap_or_else(|_| format!("astra-worker-{}", std::process::id()))
        })
        .clone()
}

const LEASE_RELEASE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

#[derive(Clone)]
struct LeaseReleaseContext {
    lease_svc: std::sync::Arc<dyn astra_services::TaskLeaseService>,
    user_id: std::sync::Arc<String>,
    task_id: std::sync::Arc<String>,
    agent_id: std::sync::Arc<String>,
    timeout: std::time::Duration,
}

/// Releases a claimed lease on normal completion, and falls back to an
/// asynchronous release attempt if the owning worker future is dropped.
pub(crate) struct ClaimedTaskLeaseGuard {
    ctx: Option<LeaseReleaseContext>,
}

impl ClaimedTaskLeaseGuard {
    pub(crate) fn new(
        lease_svc: std::sync::Arc<dyn astra_services::TaskLeaseService>,
        user_id: std::sync::Arc<String>,
        task_id: std::sync::Arc<String>,
        agent_id: std::sync::Arc<String>,
    ) -> Self {
        Self {
            ctx: Some(LeaseReleaseContext {
                lease_svc,
                user_id,
                task_id,
                agent_id,
                timeout: LEASE_RELEASE_TIMEOUT,
            }),
        }
    }

    pub(crate) async fn release_and_disarm(mut self) -> Result<(), String> {
        let ctx = self
            .ctx
            .as_ref()
            .expect("lease release guard should contain context")
            .clone();
        let result = release_lease_with_timeout(&ctx, "task execution").await;
        self.ctx = None;
        result
    }
}

impl Drop for ClaimedTaskLeaseGuard {
    fn drop(&mut self) {
        let Some(ctx) = self.ctx.take() else {
            return;
        };
        tokio::spawn(async move {
            if let Err(error) = release_lease_with_timeout(&ctx, "worker future drop").await {
                tracing::warn!(
                    task_id = %ctx.task_id.as_str(),
                    %error,
                    "lease release fallback failed"
                );
            }
        });
    }
}

async fn release_lease_with_timeout(
    ctx: &LeaseReleaseContext,
    reason: &'static str,
) -> Result<(), String> {
    match tokio::time::timeout(
        ctx.timeout,
        ctx.lease_svc.release_lease(
            ctx.user_id.as_str(),
            ctx.task_id.as_str(),
            ctx.agent_id.as_str(),
        ),
    )
    .await
    {
        Ok(Ok(true)) => {
            tracing::debug!(task_id = %ctx.task_id.as_str(), reason, "lease released");
            Ok(())
        }
        Ok(Ok(false)) => {
            tracing::warn!(
                task_id = %ctx.task_id.as_str(),
                reason,
                "release_lease returned false"
            );
            Err(format!(
                "task {reason} finished but lease was not released because it was already expired or stolen: {}",
                ctx.task_id
            ))
        }
        Ok(Err(e)) => {
            tracing::warn!(
                task_id = %ctx.task_id.as_str(),
                error = %e,
                reason,
                "lease release failed"
            );
            Err(format!(
                "task {reason} finished but lease release failed: {e}"
            ))
        }
        Err(_) => {
            tracing::warn!(
                task_id = %ctx.task_id.as_str(),
                timeout_ms = ctx.timeout.as_millis(),
                reason,
                "lease release timed out"
            );
            Err(format!(
                "task {reason} finished but lease release timed out after {}ms",
                ctx.timeout.as_millis()
            ))
        }
    }
}

/// Lease metadata returned when a worker successfully claims a task.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WorkerClaimGrant {
    pub(crate) task_id: String,
    pub(crate) lease_version: i64,
    pub(crate) expires_at: String,
}

/// Why a worker poll returned no immediately-runnable task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerClaimIdleReason {
    NoClaimableTasks,
    AllClaimableTasksLeased,
}

impl WorkerClaimIdleReason {
    pub(crate) fn json_reason(self) -> &'static str {
        match self {
            Self::NoClaimableTasks => "no_claimable_tasks",
            Self::AllClaimableTasksLeased => "all_claimable_tasks_leased",
        }
    }

    pub(crate) fn human_message(self) -> &'static str {
        match self {
            Self::NoClaimableTasks => "No claimable cloud jobs.",
            Self::AllClaimableTasksLeased => "All claimable cloud jobs are currently leased.",
        }
    }
}

/// Result of asking the lease service for the next task a worker should run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WorkerClaim {
    Granted(WorkerClaimGrant),
    Idle(WorkerClaimIdleReason),
}

/// Claims the next available task lease for a worker poll iteration.
#[tracing::instrument(skip(lease_svc), fields(user_id = %user_id, agent_id = %agent_id, edge_id = %edge_id, ttl_sec))]
pub(crate) async fn claim_task_for_worker(
    lease_svc: &dyn astra_services::TaskLeaseService,
    user_id: &str,
    agent_id: &str,
    edge_id: &str,
    ttl_sec: i64,
) -> Result<WorkerClaim, String> {
    let result = lease_svc
        .claim_next_claimable_lease(user_id, agent_id, edge_id, ttl_sec)
        .await?;
    Ok(match result {
        astra_services::NextClaimableLeaseClaimResult::Granted {
            task_id,
            lease_version,
            expires_at,
        } => WorkerClaim::Granted(WorkerClaimGrant {
            task_id,
            lease_version,
            expires_at,
        }),
        astra_services::NextClaimableLeaseClaimResult::NoClaimableTasks => {
            WorkerClaim::Idle(WorkerClaimIdleReason::NoClaimableTasks)
        }
        astra_services::NextClaimableLeaseClaimResult::AllClaimableTasksLeased => {
            WorkerClaim::Idle(WorkerClaimIdleReason::AllClaimableTasksLeased)
        }
    })
}

/// Loads the claimed job and releases the lease if the task vanished mid-claim.
pub(crate) async fn get_claimed_task_or_release(
    task_svc: &dyn astra_services::TaskService,
    lease_svc: &dyn astra_services::TaskLeaseService,
    user_id: &str,
    claimed_task_id: &str,
    agent_id: &str,
) -> Result<astra_services::TaskRecord, String> {
    match task_svc.get_task(claimed_task_id).await {
        Ok(Some(task)) => Ok(task),
        Ok(None) => {
            release_claimed_task_after_lookup_failure(
                lease_svc,
                user_id,
                claimed_task_id,
                agent_id,
                "get_task returned None",
            )
            .await?;
            Err(format!("claimed job disappeared: {claimed_task_id}"))
        }
        Err(e) => {
            release_claimed_task_after_lookup_failure(
                lease_svc,
                user_id,
                claimed_task_id,
                agent_id,
                "get_task error",
            )
            .await?;
            Err(format!("get_task failed after claim: {e}"))
        }
    }
}

async fn release_claimed_task_after_lookup_failure(
    lease_svc: &dyn astra_services::TaskLeaseService,
    user_id: &str,
    task_id: &str,
    agent_id: &str,
    lookup_failure: &'static str,
) -> Result<(), String> {
    match lease_svc.release_lease(user_id, task_id, agent_id).await {
        Ok(true) => Ok(()),
        Ok(false) => Err(format!(
            "claimed job lookup failed ({lookup_failure}) and lease was already expired or stolen: {task_id}"
        )),
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                lookup_failure,
                "release_lease failed after claimed job lookup failure"
            );
            Err(format!(
                "claimed job lookup failed ({lookup_failure}) and lease release failed for {task_id}: {e}"
            ))
        }
    }
}

/// Releases the worker's lease after task execution, tolerating already-lost leases.
#[tracing::instrument(skip(lease_svc), fields(user_id = %user_id, task_id = %task_id, agent_id = %agent_id))]
pub(crate) async fn release_claimed_task_after_execution(
    lease_svc: &dyn astra_services::TaskLeaseService,
    user_id: &str,
    task_id: &str,
    agent_id: &str,
) -> Result<(), String> {
    match tokio::time::timeout(
        LEASE_RELEASE_TIMEOUT,
        lease_svc.release_lease(user_id, task_id, agent_id),
    )
    .await
    {
        Ok(Ok(true)) => {
            tracing::debug!(task_id = %task_id, "lease released");
            Ok(())
        }
        Ok(Ok(false)) => {
            tracing::warn!(
                task_id = %task_id,
                "release_lease returned false after task execution"
            );
            Err(format!(
                "task execution finished but lease was not released because it was already expired or stolen: {task_id}"
            ))
        }
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "lease release failed after task execution");
            Err(format!(
                "task execution finished but lease release failed: {e}"
            ))
        }
        Err(_) => {
            tracing::warn!(
                task_id = %task_id,
                timeout_ms = LEASE_RELEASE_TIMEOUT.as_millis(),
                "lease release timed out after task execution"
            );
            Err(format!(
                "task execution finished but lease release timed out after {}ms",
                LEASE_RELEASE_TIMEOUT.as_millis()
            ))
        }
    }
}

/// Reverts an interrupted task back to pending when the worker still owns its lease.
#[tracing::instrument(skip(task_svc, lease_svc), fields(user_id = %user_id, task_id = %task_id, agent_id = %agent_id, timeout_ms = timeout.as_millis()))]
pub(crate) async fn revert_interrupted_task_to_pending_if_still_owned(
    task_svc: &dyn astra_services::TaskService,
    lease_svc: &dyn astra_services::TaskLeaseService,
    user_id: &str,
    task_id: &str,
    agent_id: &str,
    timeout: std::time::Duration,
) {
    let still_ours =
        match tokio::time::timeout(timeout, lease_svc.get_lease(user_id, task_id)).await {
            Ok(Ok(Some(view))) => view.holder_agent_id == agent_id,
            Ok(Ok(None)) => false,
            Ok(Err(e)) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "get_lease before revert failed; skipping revert"
                );
                false
            }
            Err(_) => {
                tracing::warn!(
                    task_id = %task_id,
                    "get_lease timed out before revert; skipping revert"
                );
                false
            }
        };

    if !still_ours {
        tracing::debug!("skipping revert because lease is no longer owned by this worker");
        return;
    }

    let revert = tokio::time::timeout(
        timeout,
        task_svc.update_status(task_id, astra_services::TaskStatus::Pending),
    )
    .await;
    match revert {
        Ok(Ok(())) => {
            tracing::debug!(task_id = %task_id, "interrupted task reverted to Pending");
        }
        Ok(Err(e)) if e.starts_with("invalid task status transition") => {
            tracing::debug!(
                task_id = %task_id,
                error = %e,
                "task already in terminal state; skipping revert"
            );
        }
        Ok(Err(e)) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "failed to revert interrupted task to Pending (task remains claimable but may still look in_progress)"
            );
        }
        Err(_) => {
            tracing::warn!(
                task_id = %task_id,
                "update_status revert timed out (task remains claimable but may still look in_progress)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ClaimedTaskLeaseGuard, WorkerClaim, WorkerClaimGrant, WorkerClaimIdleReason,
        claim_task_for_worker, get_claimed_task_or_release, release_claimed_task_after_execution,
        revert_interrupted_task_to_pending_if_still_owned,
    };
    use astra_services::{TaskCreateRequest, TaskService};
    use std::sync::Arc;
    use std::time::Duration;

    struct StubTaskLeaseService {
        next_result: astra_services::NextClaimableLeaseClaimResult,
    }

    #[async_trait::async_trait]
    impl astra_services::TaskLeaseService for StubTaskLeaseService {
        async fn claim_next_claimable_lease(
            &self,
            _user_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<astra_services::NextClaimableLeaseClaimResult, String> {
            Ok(self.next_result.clone())
        }

        async fn try_claim_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<astra_services::LeaseClaimResult, String> {
            unreachable!("worker claim path should not fall back to per-task try_claim_lease");
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
        ) -> Result<Option<astra_services::TaskLeaseView>, String> {
            unreachable!("not used in this test");
        }

        async fn renew_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<Option<astra_services::TaskLeaseView>, String> {
            unreachable!("not used in this test");
        }
    }

    #[tokio::test]
    async fn claim_task_for_worker_maps_granted_result() {
        let lease_svc = StubTaskLeaseService {
            next_result: astra_services::NextClaimableLeaseClaimResult::Granted {
                task_id: "task-201".into(),
                lease_version: 9,
                expires_at: "2025-01-03T00:00:00Z".into(),
            },
        };

        let result = claim_task_for_worker(&lease_svc, "task-user", "agent-1", "edge-1", 300)
            .await
            .unwrap();

        assert_eq!(
            result,
            WorkerClaim::Granted(WorkerClaimGrant {
                task_id: "task-201".into(),
                lease_version: 9,
                expires_at: "2025-01-03T00:00:00Z".into(),
            })
        );
    }

    #[tokio::test]
    async fn claim_task_for_worker_maps_idle_results() {
        for (next_result, idle_reason) in [
            (
                astra_services::NextClaimableLeaseClaimResult::NoClaimableTasks,
                WorkerClaimIdleReason::NoClaimableTasks,
            ),
            (
                astra_services::NextClaimableLeaseClaimResult::AllClaimableTasksLeased,
                WorkerClaimIdleReason::AllClaimableTasksLeased,
            ),
        ] {
            let lease_svc = StubTaskLeaseService { next_result };

            let result = claim_task_for_worker(&lease_svc, "task-user", "agent-1", "edge-1", 300)
                .await
                .unwrap();

            assert_eq!(result, WorkerClaim::Idle(idle_reason));
        }
    }

    struct RecordingReleaseLeaseService {
        released: Arc<std::sync::Mutex<Vec<(String, String, String)>>>,
        lease_view: Option<astra_services::TaskLeaseView>,
        release_result: Result<bool, String>,
    }

    impl RecordingReleaseLeaseService {
        fn new() -> Self {
            Self {
                released: Arc::new(std::sync::Mutex::new(Vec::new())),
                lease_view: None,
                release_result: Ok(true),
            }
        }

        fn with_holder(task_id: &str, holder_agent_id: &str) -> Self {
            Self {
                released: Arc::new(std::sync::Mutex::new(Vec::new())),
                lease_view: Some(astra_services::TaskLeaseView {
                    task_id: task_id.to_string(),
                    holder_agent_id: holder_agent_id.to_string(),
                    holder_edge_id: Some("edge-1".to_string()),
                    expires_at: "2026-01-01T00:00:00Z".to_string(),
                    lease_version: 1,
                }),
                release_result: Ok(true),
            }
        }

        fn with_release_result(release_result: Result<bool, String>) -> Self {
            Self {
                released: Arc::new(std::sync::Mutex::new(Vec::new())),
                lease_view: None,
                release_result,
            }
        }

        fn released(&self) -> Vec<(String, String, String)> {
            self.released.lock().unwrap().clone()
        }
    }

    #[async_trait::async_trait]
    impl astra_services::TaskLeaseService for RecordingReleaseLeaseService {
        async fn claim_next_claimable_lease(
            &self,
            _user_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<astra_services::NextClaimableLeaseClaimResult, String> {
            unreachable!("not used in this test");
        }

        async fn try_claim_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<astra_services::LeaseClaimResult, String> {
            unreachable!("not used in this test");
        }

        async fn release_lease(
            &self,
            user_id: &str,
            task_id: &str,
            agent_id: &str,
        ) -> Result<bool, String> {
            self.released.lock().unwrap().push((
                user_id.to_string(),
                task_id.to_string(),
                agent_id.to_string(),
            ));
            self.release_result.clone()
        }

        async fn get_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
        ) -> Result<Option<astra_services::TaskLeaseView>, String> {
            Ok(self.lease_view.clone())
        }

        async fn renew_lease(
            &self,
            _user_id: &str,
            _task_id: &str,
            _agent_id: &str,
            _edge_id: &str,
            _ttl_sec: i64,
        ) -> Result<Option<astra_services::TaskLeaseView>, String> {
            unreachable!("not used in this test");
        }
    }

    #[tokio::test]
    async fn get_claimed_task_or_release_releases_missing_claimed_task() {
        let tmp = tempfile::tempdir().unwrap();
        let task_svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let lease_svc = RecordingReleaseLeaseService::new();

        let err = get_claimed_task_or_release(
            &task_svc,
            &lease_svc,
            "task-user",
            "missing-task",
            "agent-1",
        )
        .await
        .unwrap_err();

        assert_eq!(err, "claimed job disappeared: missing-task");
        assert_eq!(
            lease_svc.released(),
            vec![(
                "task-user".to_string(),
                "missing-task".to_string(),
                "agent-1".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn get_claimed_task_or_release_propagates_release_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let task_svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let lease_svc =
            RecordingReleaseLeaseService::with_release_result(Err("db unavailable".to_string()));

        let err = get_claimed_task_or_release(
            &task_svc,
            &lease_svc,
            "task-user",
            "missing-task",
            "agent-1",
        )
        .await
        .unwrap_err();

        assert_eq!(
            err,
            "claimed job lookup failed (get_task returned None) and lease release failed for missing-task: db unavailable"
        );
        assert_eq!(
            lease_svc.released(),
            vec![(
                "task-user".to_string(),
                "missing-task".to_string(),
                "agent-1".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn get_claimed_task_or_release_keeps_lease_for_found_task() {
        let tmp = tempfile::tempdir().unwrap();
        let task_svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let lease_svc = RecordingReleaseLeaseService::new();
        let task_id = task_svc
            .create_task(
                "task-user",
                "session-1",
                TaskCreateRequest {
                    title: "claimed job".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();

        let task =
            get_claimed_task_or_release(&task_svc, &lease_svc, "task-user", &task_id, "agent-1")
                .await
                .unwrap();

        assert_eq!(task.task_id, task_id);
        assert!(lease_svc.released().is_empty());
    }

    #[tokio::test]
    async fn revert_interrupted_task_sets_pending_when_lease_is_still_owned() {
        let tmp = tempfile::tempdir().unwrap();
        let task_svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let task_id = task_svc
            .create_task(
                "task-user",
                "session-1",
                TaskCreateRequest {
                    title: "interruptible task".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        task_svc
            .update_status(&task_id, astra_services::TaskStatus::InProgress)
            .await
            .unwrap();
        let lease_svc = RecordingReleaseLeaseService::with_holder(&task_id, "agent-1");

        revert_interrupted_task_to_pending_if_still_owned(
            &task_svc,
            &lease_svc,
            "task-user",
            &task_id,
            "agent-1",
            Duration::from_secs(1),
        )
        .await;

        let task = task_svc.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(task.status, astra_services::TaskStatus::Pending);
    }

    #[tokio::test]
    async fn revert_interrupted_task_skips_pending_when_lease_moved() {
        let tmp = tempfile::tempdir().unwrap();
        let task_svc = astra_services::LocalTaskService::new(tmp.path().to_path_buf());
        let task_id = task_svc
            .create_task(
                "task-user",
                "session-1",
                TaskCreateRequest {
                    title: "stolen task".to_string(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        task_svc
            .update_status(&task_id, astra_services::TaskStatus::InProgress)
            .await
            .unwrap();
        let lease_svc = RecordingReleaseLeaseService::with_holder(&task_id, "agent-2");

        revert_interrupted_task_to_pending_if_still_owned(
            &task_svc,
            &lease_svc,
            "task-user",
            &task_id,
            "agent-1",
            Duration::from_secs(1),
        )
        .await;

        let task = task_svc.get_task(&task_id).await.unwrap().unwrap();
        assert_eq!(task.status, astra_services::TaskStatus::InProgress);
    }

    #[tokio::test]
    async fn release_claimed_task_after_execution_records_success() {
        let lease_svc = RecordingReleaseLeaseService::with_release_result(Ok(true));

        release_claimed_task_after_execution(&lease_svc, "task-user", "task-1", "agent-1")
            .await
            .unwrap();

        assert_eq!(
            lease_svc.released(),
            vec![(
                "task-user".to_string(),
                "task-1".to_string(),
                "agent-1".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn claimed_task_lease_guard_drop_releases_lease() {
        let lease_svc = Arc::new(RecordingReleaseLeaseService::with_release_result(Ok(true)));
        let guard = ClaimedTaskLeaseGuard::new(
            lease_svc.clone(),
            Arc::new("task-user".to_string()),
            Arc::new("task-1".to_string()),
            Arc::new("agent-1".to_string()),
        );

        drop(guard);
        for _ in 0..10 {
            if !lease_svc.released().is_empty() {
                break;
            }
            tokio::task::yield_now().await;
        }

        assert_eq!(
            lease_svc.released(),
            vec![(
                "task-user".to_string(),
                "task-1".to_string(),
                "agent-1".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn release_claimed_task_after_execution_reports_missing_lease() {
        let lease_svc = RecordingReleaseLeaseService::with_release_result(Ok(false));

        let err =
            release_claimed_task_after_execution(&lease_svc, "task-user", "task-1", "agent-1")
                .await
                .unwrap_err();

        assert_eq!(
            err,
            "task execution finished but lease was not released because it was already expired or stolen: task-1"
        );
        assert_eq!(lease_svc.released().len(), 1);
    }

    #[tokio::test]
    async fn release_claimed_task_after_execution_reports_release_error() {
        let lease_svc =
            RecordingReleaseLeaseService::with_release_result(Err("mo unavailable".to_string()));

        let err =
            release_claimed_task_after_execution(&lease_svc, "task-user", "task-1", "agent-1")
                .await
                .unwrap_err();

        assert_eq!(
            err,
            "task execution finished but lease release failed: mo unavailable"
        );
        assert_eq!(lease_svc.released().len(), 1);
    }
}
