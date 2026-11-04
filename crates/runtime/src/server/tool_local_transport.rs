use std::sync::Arc;
use std::{future::Future, pin::Pin};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::tool_execution_binding::{
    ExecutorBinding, ToolExecutionRequest, ToolTransportKind, WorkspaceBinding,
};
use super::tool_transport_metadata::cancelled_runtime_tool_result_for_binding;

#[cfg(not(test))]
const LOCAL_OWNER_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
#[cfg(test)]
const LOCAL_OWNER_CLEANUP_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);

#[async_trait]
pub trait ServerLocalToolTransport<R = astra_tools::ToolResult>: Send + Sync {
    /// Retain the same executor while a cancelled invocation finishes cleanup.
    fn owned_transport(&self) -> Option<Arc<dyn ServerLocalToolTransport<R>>>;

    /// Workspace effects must be owned by the admitted workspace lease until settled.
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
        cancel_token: Option<&CancellationToken>,
    ) -> R;
}

// A dropped caller must cancel the invocation without dropping its live owner.
// Normal execution stays in the caller task; only interrupted writers are
// handed to a cleanup task.
struct LocalExecution<R: Send + 'static> {
    execution: Option<Pin<Box<dyn Future<Output = R> + Send>>>,
    cancel: CancellationToken,
    retain_on_drop: bool,
}

impl<R: Send + 'static> LocalExecution<R> {
    fn retain_cleanup(&mut self) -> Option<tokio::task::JoinHandle<R>> {
        let execution = self.execution.take()?;
        self.cancel.cancel();
        tokio::runtime::Handle::try_current()
            .ok()
            .map(|runtime| runtime.spawn(execution))
    }
}

impl<R: Send + 'static> Drop for LocalExecution<R> {
    fn drop(&mut self) {
        if self.execution.is_some() {
            self.cancel.cancel();
            if self.retain_on_drop && !std::thread::panicking() {
                drop(self.retain_cleanup());
            }
        }
    }
}

/// Private live execution return. Public results, cache hits and remote
/// callbacks can only enter through `From`, which carries no send authority.
pub(crate) struct RuntimeToolExecutionResult {
    pub(crate) result: astra_tools::ToolResult,
    pub(crate) accepted_send: Option<crate::orchestration::agent_tool::AcceptedAgentSend>,
}

impl From<astra_tools::ToolResult> for RuntimeToolExecutionResult {
    fn from(result: astra_tools::ToolResult) -> Self {
        Self {
            result,
            accepted_send: None,
        }
    }
}

pub(crate) trait ToolTransportResult:
    From<astra_tools::ToolResult> + Send + 'static
{
    fn result_mut(&mut self) -> &mut astra_tools::ToolResult;
}

impl ToolTransportResult for astra_tools::ToolResult {
    fn result_mut(&mut self) -> &mut astra_tools::ToolResult {
        self
    }
}

impl ToolTransportResult for RuntimeToolExecutionResult {
    fn result_mut(&mut self) -> &mut astra_tools::ToolResult {
        &mut self.result
    }
}

pub(crate) async fn execute_local_transport<L, R>(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    result_binding: (&WorkspaceBinding, &ExecutorBinding, ToolTransportKind),
    local_transport: &L,
    cancel_token: Option<CancellationToken>,
    is_observation: bool,
) -> R
where
    L: ServerLocalToolTransport<R> + ?Sized,
    R: ToolTransportResult,
{
    let (result_workspace, result_executor, result_transport) = result_binding;
    if cancel_token
        .as_ref()
        .is_some_and(|token| token.is_cancelled())
    {
        return cancelled_runtime_tool_result_for_binding(
            result_workspace,
            result_executor,
            &request.tool_name,
            binding,
            result_transport,
            false,
        )
        .into();
    }
    let owner = local_transport.owned_transport();
    if owner.is_none() && cancel_token.is_none() {
        return local_transport
            .execute_server_local_tool(request, None)
            .await;
    }
    let Some(owner) = owner else {
        return astra_tools::ToolResult::error(
            "Tool execution is unavailable in this environment.".to_string(),
        )
        .with_failure_evidence(astra_core::ToolFailureEvidence::from_error_kind(
            astra_core::ErrorKind::ToolUnavailable,
        ))
        .with_execution_not_started()
        .into();
    };
    let token = cancel_token.unwrap_or_default();
    let owned_request = request.clone();
    let owned_token = token.child_token();
    let execution_token = owned_token.clone();
    let future = Box::pin(async move {
        owner
            .execute_server_local_tool(&owned_request, Some(&execution_token))
            .await
    });
    let mut execution = LocalExecution {
        execution: Some(future),
        cancel: owned_token,
        retain_on_drop: !is_observation,
    };
    tokio::select! {
            biased;
            result = execution.execution.as_mut().expect("live execution").as_mut() => {
                execution.execution.take();
                result
            },
            _ = token.cancelled() => {
                let Some(mut cleanup) = execution.retain_cleanup() else {
                    return astra_tools::cancelled_tool_result(&request.tool_name, true).into();
                };
                let _abort_observation = is_observation
                    .then(|| tokio_util::task::AbortOnDrop::new(cleanup.abort_handle()));
                match tokio::time::timeout(LOCAL_OWNER_CLEANUP_TIMEOUT, &mut cleanup).await {
                    Ok(Ok(settled)) => settled,
                    Ok(Err(error)) => {
                        tracing::error!(%error, "local tool cleanup failed");
                        let mut result = astra_tools::ToolResult::error(
                            "The interrupted operation could not be confirmed stopped.".to_string(),
                        ).with_failure_evidence(astra_core::ToolFailureEvidence::from_error_kind(
                            astra_core::ErrorKind::ToolOutcomeUnknown,
                        ));
                        astra_tools::execution_outcome::insert_started_execution_fact(
                            result.metadata.get_or_insert_default(),
                        );
                        result.into()
                    }
                    Err(_) => {
                        // A declared pure read cannot leave a workspace writer
                        // behind. Its stalled future may be dropped without
                        // fencing unrelated sessions or retaining its resources.
                        if is_observation {
                            return cancelled_runtime_tool_result_for_binding(
                                result_workspace,
                                result_executor,
                                &request.tool_name,
                                binding,
                                result_transport,
                                true,
                            ).into();
                        }
                        let mut result = cancelled_runtime_tool_result_for_binding(
                            result_workspace,
                            result_executor,
                            &request.tool_name,
                            binding,
                            result_transport,
                            true,
                        );
                        if let Ok(serde_json::Value::Object(mut output)) =
                            serde_json::from_str::<serde_json::Value>(&result.output)
                        {
                            output.insert("error".into(), serde_json::json!(format!(
                                "Tool '{}' was cancelled and is still stopping. Its result is unverified; wait for it to stop before retrying.",
                                request.tool_name,
                            )));
                            result.output = serde_json::Value::Object(output).to_string();
                        }
                        result.into()
                    }
                }
            },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{PolicyIntent, RunBinding, RuntimeBinding};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Clone)]
    struct CooperativeOwnerTransport {
        workspace_root: PathBuf,
        started: Arc<Notify>,
        cleanup_started: Arc<Notify>,
        cleanup_finished: Arc<Notify>,
    }

    struct DropWitness(Arc<std::sync::atomic::AtomicBool>);

    impl Drop for DropWitness {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    #[derive(Clone)]
    struct NonCooperativeOwnerTransport {
        workspace_root: Option<PathBuf>,
        started: Arc<Notify>,
        cleanup_started: Arc<Notify>,
        release: Arc<Notify>,
        future_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ServerLocalToolTransport for NonCooperativeOwnerTransport {
        fn owned_transport(&self) -> Option<Arc<dyn ServerLocalToolTransport>> {
            Some(Arc::new(self.clone()))
        }

        async fn execute_server_local_tool(
            &self,
            _request: &ToolExecutionRequest,
            cancel_token: Option<&CancellationToken>,
        ) -> astra_tools::ToolResult {
            let mut owner = if let Some(root) = &self.workspace_root {
                Some(
                    astra_tools::workspace_observation::begin_workspace_writer_with_options(
                        root,
                        cancel_token,
                        Duration::from_secs(1),
                    )
                    .await
                    .expect("terminal owner owns the workspace"),
                )
            } else {
                None
            };
            if let Some(owner) = &mut owner {
                owner.arm_abandonment_fence();
            }
            let _drop_witness = DropWitness(self.future_dropped.clone());
            self.started.notify_one();
            if let Some(token) = cancel_token {
                token.cancelled().await;
            } else {
                std::future::pending::<()>().await;
            }
            self.cleanup_started.notify_one();
            self.release.notified().await;
            if let Some(owner) = &mut owner {
                owner.disarm_abandonment_fence();
            }
            astra_tools::cancelled_tool_result("owned-local-tool", true)
        }
    }

    #[async_trait]
    impl ServerLocalToolTransport for CooperativeOwnerTransport {
        fn owned_transport(&self) -> Option<Arc<dyn ServerLocalToolTransport>> {
            Some(Arc::new(self.clone()))
        }

        async fn execute_server_local_tool(
            &self,
            _request: &ToolExecutionRequest,
            cancel_token: Option<&CancellationToken>,
        ) -> astra_tools::ToolResult {
            let _owner = astra_tools::workspace_observation::begin_workspace_writer_with_options(
                &self.workspace_root,
                cancel_token,
                Duration::from_secs(1),
            )
            .await
            .expect("first writer owns the workspace");
            self.started.notify_one();
            cancel_token
                .expect("transport receives cancellation authority")
                .cancelled()
                .await;
            self.cleanup_started.notify_one();
            tokio::time::sleep(Duration::from_millis(100)).await;
            std::fs::write(self.workspace_root.join("descendant-settled"), "done")
                .expect("cooperative descendant cleanup marker");
            self.cleanup_finished.notify_one();
            astra_tools::cancelled_tool_result("owned-local-tool", true)
        }
    }

    fn test_binding() -> RunBinding {
        RunBinding::resolve(
            astra_runtime_env::WorkspaceBinding::server_sandbox("session-1"),
            astra_runtime_env::ExecutorBinding::local_cli(),
            RuntimeBinding::host_process("test-host".to_string()),
            PolicyIntent::local_developer(),
            &astra_runtime_env::ToolRegistry::default(),
        )
    }

    fn test_request(tool_name: &str, workspace_root: &std::path::Path) -> ToolExecutionRequest {
        ToolExecutionRequest {
            user_id: "user-1".to_string(),
            run_id: "run-1".to_string(),
            turn_chain_id: "chain-1".to_string(),
            session_id: "session-1".to_string(),
            tool_call_id: "call-1".to_string(),
            tool_name: tool_name.to_string(),
            args: serde_json::json!({}),
            workspace: WorkspaceBinding::server_sandbox(workspace_root),
            workspace_record: None,
            executor: ExecutorBinding::server_local(),
            runtime: None,
            selected_offer: None,
            policy: Default::default(),
            runtime_process_authorization: None,
            runtime_process_authorization_required: false,
            runtime_edge_dispatch_authorization: None,
            runtime_edge_dispatch_authorization_required: false,
            provider_stage_dispatch: None,
        }
    }

    async fn cancelled_owner_is_awaited_before_next_writer(tool_name: &str) {
        let workspace = tempfile::tempdir().expect("workspace");
        let started = Arc::new(Notify::new());
        let cleanup_started = Arc::new(Notify::new());
        let cleanup_finished = Arc::new(Notify::new());
        let transport = Arc::new(CooperativeOwnerTransport {
            workspace_root: workspace.path().to_path_buf(),
            started: started.clone(),
            cleanup_started: cleanup_started.clone(),
            cleanup_finished: cleanup_finished.clone(),
        });
        let request = test_request(tool_name, workspace.path());
        let binding = test_binding();
        let result_workspace = request.workspace.clone();
        let result_executor = request.executor.clone();
        let cancel = CancellationToken::new();
        let execution_cancel = cancel.clone();
        let execution = tokio::spawn(async move {
            execute_local_transport(
                &request,
                &binding,
                (
                    &result_workspace,
                    &result_executor,
                    ToolTransportKind::ServerLocal,
                ),
                transport.as_ref(),
                Some(execution_cancel),
                false,
            )
            .await
        });
        tokio::time::timeout(Duration::from_millis(250), started.notified())
            .await
            .expect("first writer starts");
        cancel.cancel();
        tokio::time::timeout(Duration::from_millis(250), cleanup_started.notified())
            .await
            .expect("cancel must be propagated into cooperative cleanup");

        let second_root = workspace.path().to_path_buf();
        let mut second_writer = tokio::spawn(async move {
            astra_tools::workspace_observation::begin_workspace_writer_with_options(
                &second_root,
                None,
                Duration::from_secs(1),
            )
            .await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut second_writer)
                .await
                .is_err(),
            "a second writer must wait while cancelled {tool_name} descendants settle"
        );
        let result = tokio::time::timeout(Duration::from_millis(500), execution)
            .await
            .expect("cooperative cleanup is bounded")
            .expect("transport task");
        assert!(result.is_error, "cancellation remains user-visible");
        tokio::time::timeout(Duration::from_millis(50), cleanup_finished.notified())
            .await
            .expect("transport completed receipt-side cleanup");
        assert!(workspace.path().join("descendant-settled").is_file());
        assert!(
            second_writer.await.expect("second writer task").is_ok(),
            "the next writer enters only after the prior owner settles"
        );
    }

    #[tokio::test]
    async fn cancelled_local_bash_keeps_owner_until_cooperative_cleanup() {
        cancelled_owner_is_awaited_before_next_writer("bash").await;
    }

    #[tokio::test]
    async fn cancelled_local_run_script_keeps_owner_until_cooperative_cleanup() {
        cancelled_owner_is_awaited_before_next_writer("run_script").await;
    }

    #[tokio::test]
    async fn stalled_read_cleanup_does_not_fence_workspace() {
        let workspace = tempfile::tempdir().unwrap();
        let started = Arc::new(Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(NonCooperativeOwnerTransport {
            workspace_root: None,
            started: started.clone(),
            cleanup_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            future_dropped: dropped.clone(),
        });
        let request = test_request("read_file", workspace.path());
        let binding = test_binding();
        let result_workspace = request.workspace.clone();
        let result_executor = request.executor.clone();
        let cancel = CancellationToken::new();
        let execution_cancel = cancel.clone();
        let execution = tokio::spawn(async move {
            execute_local_transport(
                &request,
                &binding,
                (
                    &result_workspace,
                    &result_executor,
                    ToolTransportKind::ServerLocal,
                ),
                transport.as_ref(),
                Some(execution_cancel),
                true,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("read starts");
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .expect("read cancellation must return")
            .unwrap();
        assert!(result.is_error);
        tokio::time::timeout(Duration::from_secs(2), async {
            while !dropped.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stalled read is dropped");
        assert_eq!(
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(workspace.path()),
            Some(false)
        );
        assert!(
            astra_tools::workspace_observation::begin_workspace_writer_with_options(
                workspace.path(),
                None,
                Duration::from_secs(1),
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn caller_drop_during_read_cleanup_releases_execution() {
        let workspace = tempfile::tempdir().unwrap();
        let started = Arc::new(Notify::new());
        let cleanup_started = Arc::new(Notify::new());
        let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(NonCooperativeOwnerTransport {
            workspace_root: None,
            started: started.clone(),
            cleanup_started: cleanup_started.clone(),
            release: Arc::new(Notify::new()),
            future_dropped: dropped.clone(),
        });
        let request = test_request("read_file", workspace.path());
        let binding = test_binding();
        let cancel = CancellationToken::new();
        let execution_cancel = cancel.clone();
        let execution = tokio::spawn(async move {
            execute_local_transport(
                &request,
                &binding,
                (
                    &request.workspace,
                    &request.executor,
                    ToolTransportKind::ServerLocal,
                ),
                transport.as_ref(),
                Some(execution_cancel),
                true,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(2), cleanup_started.notified())
            .await
            .unwrap();
        execution.abort();
        assert!(execution.await.unwrap_err().is_cancelled());
        let released = tokio::time::timeout(Duration::from_millis(500), async {
            while !dropped.load(std::sync::atomic::Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await;
        assert!(
            released.is_ok(),
            "read-only cleanup must not outlive its caller indefinitely"
        );
        assert_eq!(
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(workspace.path()),
            Some(false)
        );
    }

    #[derive(Clone, Copy)]
    enum CallerInterruption {
        WaitForDeadline,
        DropAfterCancel,
        DropBeforeCancel,
        DropWithoutToken,
    }

    async fn stalled_writer_cleanup_keeps_workspace_fenced(
        tool_name: &str,
        args: serde_json::Value,
        interruption: CallerInterruption,
    ) {
        let workspace = tempfile::tempdir().expect("workspace");
        let started = Arc::new(Notify::new());
        let cleanup_started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let future_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(NonCooperativeOwnerTransport {
            workspace_root: Some(workspace.path().to_path_buf()),
            started: started.clone(),
            cleanup_started: cleanup_started.clone(),
            release: release.clone(),
            future_dropped: future_dropped.clone(),
        });
        let mut request = test_request(tool_name, workspace.path());
        request.args = args;
        let binding = test_binding();
        let result_workspace = request.workspace.clone();
        let result_executor = request.executor.clone();
        let cancel = CancellationToken::new();
        let execution_cancel =
            (!matches!(interruption, CallerInterruption::DropWithoutToken)).then(|| cancel.clone());
        let execution = tokio::spawn(async move {
            execute_local_transport(
                &request,
                &binding,
                (
                    &result_workspace,
                    &result_executor,
                    ToolTransportKind::ServerLocal,
                ),
                transport.as_ref(),
                execution_cancel,
                false,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .expect("terminal owner starts");
        match interruption {
            CallerInterruption::WaitForDeadline => {
                cancel.cancel();
                let result = tokio::time::timeout(Duration::from_secs(2), execution)
                    .await
                    .expect("cleanup deadline is bounded")
                    .expect("transport task");
                assert!(result.is_error);
                let output: serde_json::Value = serde_json::from_str(&result.output).unwrap();
                assert_eq!(output["retryable"], false);
                assert!(output["error"].as_str().unwrap().contains("still stopping"));
            }
            CallerInterruption::DropAfterCancel => {
                cancel.cancel();
                tokio::time::timeout(Duration::from_secs(2), cleanup_started.notified())
                    .await
                    .expect("cleanup starts");
                execution.abort();
                assert!(execution.await.unwrap_err().is_cancelled());
            }
            CallerInterruption::DropBeforeCancel | CallerInterruption::DropWithoutToken => {
                execution.abort();
                assert!(execution.await.unwrap_err().is_cancelled());
                tokio::time::timeout(Duration::from_secs(2), cleanup_started.notified())
                    .await
                    .expect("dropping the caller cancels its invocation");
                assert!(!cancel.is_cancelled(), "the parent run remains active");
            }
        }
        assert!(
            !future_dropped.load(std::sync::atomic::Ordering::Acquire),
            "a cleanup deadline must retain the live ownership future"
        );
        assert_eq!(
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(workspace.path()),
            Some(false)
        );
        assert!(
            astra_tools::workspace_observation::begin_workspace_writer_with_options(
                workspace.path(),
                None,
                Duration::from_millis(50),
            )
            .await
            .is_err(),
            "no second writer may enter while the previous owner is still stopping"
        );
        release.notify_one();
        let next = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            workspace.path(),
            None,
            Duration::from_secs(2),
        )
        .await;
        assert!(
            next.is_ok(),
            "the next writer enters after cleanup finishes"
        );
        assert!(future_dropped.load(std::sync::atomic::Ordering::Acquire));
    }

    #[tokio::test]
    async fn delayed_cleanup_releases_workspace_after_owner_stops() {
        stalled_writer_cleanup_keeps_workspace_fenced(
            "bash",
            serde_json::json!({}),
            CallerInterruption::WaitForDeadline,
        )
        .await;
    }

    #[tokio::test]
    async fn dropped_caller_does_not_abandon_cleanup() {
        stalled_writer_cleanup_keeps_workspace_fenced(
            "bash",
            serde_json::json!({}),
            CallerInterruption::DropAfterCancel,
        )
        .await;
    }

    #[tokio::test]
    async fn dropped_caller_cancels_and_retains_an_active_writer() {
        stalled_writer_cleanup_keeps_workspace_fenced(
            "bash",
            serde_json::json!({}),
            CallerInterruption::DropBeforeCancel,
        )
        .await;
    }

    #[tokio::test]
    async fn dropped_caller_without_token_cancels_and_retains_an_active_writer() {
        stalled_writer_cleanup_keeps_workspace_fenced(
            "bash",
            serde_json::json!({}),
            CallerInterruption::DropWithoutToken,
        )
        .await;
    }

    #[tokio::test]
    async fn stalled_lsp_rename_does_not_get_read_only_cleanup() {
        stalled_writer_cleanup_keeps_workspace_fenced(
            "lsp",
            serde_json::json!({"operation": "rename", "dry_run": false}),
            CallerInterruption::WaitForDeadline,
        )
        .await;
    }

    #[tokio::test]
    async fn abandoning_cleanup_fences_before_releasing_the_writer() {
        let workspace = tempfile::tempdir().unwrap();
        let markers =
            astra_tools::workspace_observation::workspace_unsettled_paths_for_diagnostics(
                workspace.path(),
            )
            .unwrap();
        let mut lease = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            workspace.path(),
            None,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        lease.arm_abandonment_fence();
        drop(lease);
        let next = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            workspace.path(),
            None,
            Duration::from_millis(50),
        )
        .await;
        for marker in markers {
            let _ = std::fs::remove_file(marker);
        }
        assert!(matches!(
            next,
            Err(astra_tools::workspace_observation::WorkspaceLeaseFailure::OwnershipUnsettled)
        ));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn abandoned_writer_fences_workspace_admitted_before_retarget() {
        let parent = tempfile::tempdir().unwrap();
        let original = parent.path().join("original");
        let other = parent.path().join("other");
        let link = parent.path().join("workspace");
        std::fs::create_dir(&original).unwrap();
        std::fs::create_dir(&other).unwrap();
        std::os::unix::fs::symlink(&original, &link).unwrap();
        let mut markers = Vec::new();
        for root in [&original, &other, &link] {
            markers.extend(
                astra_tools::workspace_observation::workspace_unsettled_paths_for_diagnostics(root)
                    .unwrap(),
            );
        }
        let _other_state =
            astra_tools::workspace_observation::WorkspaceAttributionState::capture(&other).unwrap();
        let mut lease = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            &link,
            None,
            Duration::from_secs(1),
        )
        .await
        .unwrap();
        lease.arm_abandonment_fence();
        let execution = Box::pin(async move {
            let _lease = lease;
            std::future::pending::<astra_tools::ToolResult>().await
        });
        let mut owner = LocalExecution {
            execution: Some(execution),
            cancel: CancellationToken::new(),
            retain_on_drop: true,
        };
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&other, &link).unwrap();
        let cleanup = owner.retain_cleanup().unwrap();
        cleanup.abort();
        let _ = cleanup.await;
        let next = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            &original,
            None,
            Duration::from_millis(50),
        )
        .await;
        let fenced = matches!(
            next,
            Err(astra_tools::workspace_observation::WorkspaceLeaseFailure::OwnershipUnsettled)
        );
        let other_admitted =
            astra_tools::workspace_observation::begin_workspace_writer_with_options(
                &other,
                None,
                Duration::from_secs(1),
            )
            .await
            .is_ok();
        for marker in markers {
            let _ = std::fs::remove_file(marker);
        }
        assert!(fenced, "the admitted workspace must remain fenced");
        assert!(other_admitted, "the unrelated target must remain usable");
    }

    #[tokio::test]
    async fn dropped_unowned_writer_fences_before_releasing_its_lease() {
        struct Unowned(NonCooperativeOwnerTransport);
        #[async_trait]
        impl ServerLocalToolTransport for Unowned {
            fn owned_transport(&self) -> Option<Arc<dyn ServerLocalToolTransport>> {
                None
            }

            async fn execute_server_local_tool(
                &self,
                request: &ToolExecutionRequest,
                cancel: Option<&CancellationToken>,
            ) -> astra_tools::ToolResult {
                self.0.execute_server_local_tool(request, cancel).await
            }
        }

        let workspace = tempfile::tempdir().unwrap();
        let markers =
            astra_tools::workspace_observation::workspace_unsettled_paths_for_diagnostics(
                workspace.path(),
            )
            .unwrap();
        let started = Arc::new(Notify::new());
        let transport = Unowned(NonCooperativeOwnerTransport {
            workspace_root: Some(workspace.path().to_path_buf()),
            started: started.clone(),
            cleanup_started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            future_dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        });
        let request = test_request("bash", workspace.path());
        let binding = test_binding();
        let execution = tokio::spawn(async move {
            execute_local_transport(
                &request,
                &binding,
                (
                    &request.workspace,
                    &request.executor,
                    ToolTransportKind::ServerLocal,
                ),
                &transport,
                None,
                false,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        execution.abort();
        assert!(execution.await.unwrap_err().is_cancelled());
        let next = astra_tools::workspace_observation::begin_workspace_writer_with_options(
            workspace.path(),
            None,
            Duration::from_millis(50),
        )
        .await;
        for marker in markers {
            let _ = std::fs::remove_file(marker);
        }
        assert!(matches!(
            next,
            Err(astra_tools::workspace_observation::WorkspaceLeaseFailure::OwnershipUnsettled)
        ));
    }
}
