use std::sync::Arc;

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
pub trait ServerLocalToolTransport: Send + Sync {
    async fn execute_server_local_tool(
        &self,
        request: &ToolExecutionRequest,
        cancel_token: Option<&CancellationToken>,
    ) -> astra_tools::ToolResult;
}

pub(crate) async fn execute_local_transport<L>(
    request: &ToolExecutionRequest,
    binding: &astra_runtime_env::RunBinding,
    result_workspace: &WorkspaceBinding,
    result_executor: &ExecutorBinding,
    result_transport: ToolTransportKind,
    local_transport: &L,
    cancel_token: Option<CancellationToken>,
) -> astra_tools::ToolResult
where
    L: ServerLocalToolTransport + ?Sized,
{
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
        );
    }
    let mut execution =
        Box::pin(local_transport.execute_server_local_tool(request, cancel_token.as_ref()));
    if let Some(ref token) = cancel_token {
        tokio::select! {
            biased;
            result = execution.as_mut() => result,
            _ = token.cancelled() => {
                match tokio::time::timeout(LOCAL_OWNER_CLEANUP_TIMEOUT, execution.as_mut()).await {
                    Ok(settled) => settled,
                    Err(_) => {
                        if let Some(root) = result_workspace.cwd.as_deref() {
                            astra_tools::workspace_observation::mark_workspace_observation_unsettled(
                                std::path::Path::new(root),
                            );
                        }
                        let mut result = cancelled_runtime_tool_result_for_binding(
                            result_workspace,
                            result_executor,
                            &request.tool_name,
                            binding,
                            result_transport,
                            true,
                        );
                        if !result.output.is_empty() {
                            result.output.push_str("\n\n");
                        }
                        result.output.push_str(
                            "Error: cancelled server-local execution did not settle its ownership boundary before the cleanup deadline; the workspace is quarantined and no later writer may be admitted.",
                        );
                        result
                            .metadata
                            .get_or_insert_with(Default::default)
                            .insert(
                                "workspace_observation_quarantined".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        // The cancellation contract was violated: dropping
                        // this future would release the live writer guard
                        // while descendants may still exist. Retain the
                        // ownership future as a terminal process-lifetime
                        // barrier. The sticky unsettled state above ensures
                        // at most one such terminal owner per workspace and
                        // prevents every later writer from entering.
                        std::mem::forget(execution);
                        result
                    }
                }
            },
        }
    } else {
        execution.await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_runtime_env::{PolicyIntent, RunBinding, RuntimeBinding};
    use std::path::PathBuf;
    use std::time::Duration;
    use tokio::sync::Notify;

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

    struct NonCooperativeOwnerTransport {
        workspace_root: PathBuf,
        started: Arc<Notify>,
        future_dropped: Arc<std::sync::atomic::AtomicBool>,
    }

    #[async_trait]
    impl ServerLocalToolTransport for NonCooperativeOwnerTransport {
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
            .expect("terminal owner owns the workspace");
            let _drop_witness = DropWitness(self.future_dropped.clone());
            self.started.notify_one();
            std::future::pending().await
        }
    }

    #[async_trait]
    impl ServerLocalToolTransport for CooperativeOwnerTransport {
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
                &result_workspace,
                &result_executor,
                ToolTransportKind::ServerLocal,
                transport.as_ref(),
                Some(execution_cancel),
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
            second_writer.await.expect("second writer task").is_some(),
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
    async fn cleanup_deadline_retains_terminal_owner_and_rejects_second_writer() {
        let workspace = tempfile::tempdir().expect("workspace");
        let started = Arc::new(Notify::new());
        let future_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let transport = Arc::new(NonCooperativeOwnerTransport {
            workspace_root: workspace.path().to_path_buf(),
            started: started.clone(),
            future_dropped: future_dropped.clone(),
        });
        let request = test_request("bash", workspace.path());
        let binding = test_binding();
        let result_workspace = request.workspace.clone();
        let result_executor = request.executor.clone();
        let cancel = CancellationToken::new();
        let execution_cancel = cancel.clone();
        let execution = tokio::spawn(async move {
            execute_local_transport(
                &request,
                &binding,
                &result_workspace,
                &result_executor,
                ToolTransportKind::ServerLocal,
                transport.as_ref(),
                Some(execution_cancel),
            )
            .await
        });
        tokio::time::timeout(Duration::from_millis(250), started.notified())
            .await
            .expect("terminal owner starts");
        cancel.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), execution)
            .await
            .expect("cleanup deadline is bounded")
            .expect("transport task");
        assert!(result.is_error);
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_observation_quarantined"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert!(
            !future_dropped.load(std::sync::atomic::Ordering::Acquire),
            "a terminal cleanup deadline must retain, not drop, the live ownership future"
        );
        assert_eq!(
            astra_tools::workspace_observation::workspace_ownership_is_unsettled(workspace.path()),
            Some(true)
        );
        assert!(
            astra_tools::workspace_observation::begin_workspace_writer_with_options(
                workspace.path(),
                None,
                Duration::from_millis(50),
            )
            .await
            .is_none(),
            "no second writer may enter after a terminal ownership timeout"
        );
    }
}
