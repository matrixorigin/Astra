//! EdgeConnectionProvider — wired edge-connected CLI process execution.
//!
//! Delegates edge-bound tool calls (bash, file ops, git, etc.) through
//! the existing edge-connection WebSocket transport via `execute_edge_bound`.

use std::sync::Arc;

use astra_runtime_env::IsolationIntent;
use async_trait::async_trait;
use serde_json;

use super::traits::{CapabilityProvider, ProviderError, ToolRequest, ToolResult};
use super::types::{ProviderKind, ToolCapability, ToolCategory};
use crate::server::tool_edge_transport::execute_edge_bound;
use crate::server::tool_execution_binding::{
    ExecutorBinding, ExecutorStatus, ToolExecutionRequest, ToolPolicySnapshot, ToolTransportKind,
    WorkspaceAuthority, WorkspaceBinding,
};

// ---------------------------------------------------------------------------
// EdgeConnectionProvider
// ---------------------------------------------------------------------------

/// Provider for edge-connected (CLI) tool execution.
///
/// Holds references to edge transport services and delegates `execute()`
/// to `execute_edge_bound`.  If no transport is configured, returns a
/// descriptive error rather than panicking.
#[derive(Clone)]
pub struct EdgeConnectionProvider {
    /// Priority for routing (lower = preferred).
    priority: u8,
    /// Edge WebSocket connection pool (for try_edge_websocket).
    edge_connection_pool: Option<astra_server_types::edge_connection_pool::EdgeConnectionPool>,
    /// Edge dispatch service (registers tool-call futures).
    edge_dispatch_service: Option<Arc<dyn astra_services::multi_agent::EdgeDispatchService>>,
    /// Edge registry service (looks up edge-side tools).
    edge_registry_service: Option<Arc<dyn astra_services::multi_agent::EdgeRegistryService>>,
    /// Tool registry (maps tool names → ToolSpec for runtime binding).
    tool_registry: astra_runtime_env::ToolRegistry,
}

impl EdgeConnectionProvider {
    /// Create a provider with the given routing priority.
    /// Transport services must be set via the builder methods before
    /// `execute()` will succeed.
    pub fn new(priority: u8) -> Self {
        Self {
            priority,
            edge_connection_pool: None,
            edge_dispatch_service: None,
            edge_registry_service: None,
            tool_registry: astra_runtime_env::ToolRegistry::default(),
        }
    }

    /// Set the edge WebSocket connection pool.
    pub fn with_edge_connection_pool(
        mut self,
        pool: astra_server_types::edge_connection_pool::EdgeConnectionPool,
    ) -> Self {
        self.edge_connection_pool = Some(pool);
        self
    }

    /// Set the edge dispatch service.
    pub fn with_edge_dispatch_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeDispatchService>,
    ) -> Self {
        self.edge_dispatch_service = Some(svc);
        self
    }

    /// Set the edge registry service.
    pub fn with_edge_registry_service(
        mut self,
        svc: Arc<dyn astra_services::multi_agent::EdgeRegistryService>,
    ) -> Self {
        self.edge_registry_service = Some(svc);
        self
    }

    /// Set the tool registry for runtime binding resolution.
    pub fn with_tool_registry(mut self, registry: astra_runtime_env::ToolRegistry) -> Self {
        self.tool_registry = registry;
        self
    }
}

#[async_trait]
impl CapabilityProvider for EdgeConnectionProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::EdgeConnection
    }

    async fn capabilities(&self) -> Vec<ToolCapability> {
        vec![
            // ── Shell / process ──
            ToolCapability::Category(ToolCategory::Shell),
            // ── File system ──
            ToolCapability::Category(ToolCategory::FileSystem),
            // ── Version control ──
            ToolCapability::Category(ToolCategory::VersionControl),
        ]
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        // Verify transport services are wired.
        if self.edge_connection_pool.is_none() {
            return Err(ProviderError::Unhealthy(
                "EdgeConnectionProvider: edge_connection_pool not configured".into(),
            ));
        }
        if self.edge_dispatch_service.is_none() {
            return Err(ProviderError::Unhealthy(
                "EdgeConnectionProvider: edge_dispatch_service not configured".into(),
            ));
        }
        if self.edge_registry_service.is_none() {
            return Err(ProviderError::Unhealthy(
                "EdgeConnectionProvider: edge_registry_service not configured".into(),
            ));
        }
        // Check at least one live edge connection exists for any user.
        let pool = self.edge_connection_pool.as_ref().unwrap();
        if pool.connection_count() == 0 {
            return Err(ProviderError::Unhealthy(
                "EdgeConnectionProvider: no active edge connections".into(),
            ));
        }
        Ok(())
    }

    async fn execute(
        &self,
        request: ToolRequest,
        cancel_token: Option<&std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> ToolResult {
        // Build a ToolExecutionRequest from the provider's ToolRequest.
        let exec_request = ToolExecutionRequest {
            user_id: request.user_id.clone(),
            run_id: request.run_id.clone(),
            session_id: request.session_id.clone(),
            tool_name: request.tool_name.clone(),
            tool_call_id: request.tool_call_id.clone(),
            args: request.parameters.clone(),
            workspace: WorkspaceBinding::edge_workspace("edge", "/", WorkspaceAuthority::None),
            workspace_record: None,
            executor: ExecutorBinding::edge_agent(
                "edge-agent",
                "Edge Agent",
                ToolTransportKind::EdgeWs,
                ExecutorStatus::Online,
            ),
            runtime: None,
            policy: ToolPolicySnapshot::default(),
        };

        // Resolve the runtime environment binding.
        let binding = exec_request.runtime_environment_binding(&self.tool_registry);

        // Delegate to the edge WebSocket transport.
        let result = execute_edge_bound(
            exec_request,
            &binding,
            self.edge_connection_pool.clone(),
            self.edge_dispatch_service.clone(),
            self.edge_registry_service.clone(),
            &self.tool_registry,
            cancel_token.cloned(),
        )
        .await;

        // Convert astra_tools::ToolResult → provider ToolResult.
        convert_tool_result(result)
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    fn isolation_level(&self) -> IsolationIntent {
        IsolationIntent::Process
    }

    async fn storage_accessible(&self) -> bool {
        // Edge has direct access to the user's workspace.
        true
    }
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

/// Convert an `astra_tools::ToolResult` into the provider's `ToolResult` enum.
fn convert_tool_result(result: astra_tools::ToolResult) -> ToolResult {
    let exit_code = result
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("exit_code"))
        .and_then(serde_json::Value::as_i64)
        .and_then(|code| i32::try_from(code).ok());
    if result.is_error {
        ToolResult::Error {
            message: result.output,
            retryable: true,
            exit_code,
            metadata: result.metadata,
        }
    } else {
        ToolResult::Success {
            data: serde_json::Value::Null,
            stdout: result.output,
            stderr: String::new(),
            exit_code: exit_code.unwrap_or(0),
            metadata: result.metadata,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kind_is_edge_connection() {
        let provider = EdgeConnectionProvider::new(5);
        assert_eq!(provider.kind(), ProviderKind::EdgeConnection);
    }

    #[tokio::test]
    async fn capabilities_declared() {
        let provider = EdgeConnectionProvider::new(5);
        let caps = provider.capabilities().await;
        assert!(!caps.is_empty());
        assert!(caps.len() >= 3);
    }

    #[test]
    fn conversion_preserves_error_exit_code_and_metadata() {
        let result = astra_tools::ToolResult::error("failed".to_string())
            .with_exit_code(42)
            .with_result_class(astra_tools::exit_semantics::CommandResultClass::ExecutionError);

        match convert_tool_result(result) {
            ToolResult::Error {
                message,
                exit_code,
                metadata,
                ..
            } => {
                assert_eq!(message, "failed");
                assert_eq!(exit_code, Some(42));
                let metadata = metadata.expect("provider metadata");
                assert_eq!(metadata["exit_code"], 42);
                assert_eq!(metadata["result_class"], "execution_error");
            }
            other => panic!("expected provider error, got {other:?}"),
        }
    }

    #[test]
    fn conversion_preserves_non_error_nonzero_exit_code() {
        let result = astra_tools::ToolResult::text("no match".to_string())
            .with_exit_code(1)
            .with_exit_semantics(astra_tools::exit_semantics::ExitSemantics::DomainNegative);

        match convert_tool_result(result) {
            ToolResult::Success {
                stdout,
                exit_code,
                metadata,
                ..
            } => {
                assert_eq!(stdout, "no match");
                assert_eq!(exit_code, 1);
                let metadata = metadata.expect("provider metadata");
                assert_eq!(metadata["exit_code"], 1);
                assert_eq!(metadata["exit_semantics"], "domain_negative");
            }
            other => panic!("expected provider success, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn execute_without_transport_returns_error() {
        // Without transport services configured, execute() delegates to
        // execute_edge_bound which will fail due to missing pool/dispatch.
        let provider = EdgeConnectionProvider::new(5);
        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-1".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::Process,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        };
        let result = provider.execute(request, None).await;
        match result {
            ToolResult::Error { .. } => {
                // Expected — no transport is configured.
            }
            _ => panic!("expected Error, got Success"),
        }
    }

    #[tokio::test]
    async fn health_check_fails_without_transport() {
        let provider = EdgeConnectionProvider::new(5);
        let result = provider.health_check().await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, ProviderError::Unhealthy(_)),
            "expected Unhealthy error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn health_check_ok_with_transport() {
        use astra_server_types::edge_connection_pool::EdgeConnectionPool;
        use astra_services::multi_agent::{EdgeDispatchService, EdgeRegistryService};
        use std::sync::Arc;

        let pool = EdgeConnectionPool::new();
        // Register a connection so connection_count() > 0
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        pool.register("user-1", "edge-1", None, None, tx);

        struct MockDispatch;
        #[async_trait::async_trait]
        impl EdgeDispatchService for MockDispatch {
            async fn insert_dispatch(
                &self,
                _user_id: &str,
                _edge_agent_id: &str,
                _request_id: &str,
                _payload_json: &str,
            ) -> Result<(), String> {
                Err("mock".into())
            }
            async fn poll_pending(
                &self,
                _user_id: &str,
                _edge_agent_id: &str,
            ) -> Result<Vec<astra_services::multi_agent::EdgeDispatchRow>, String> {
                Ok(vec![])
            }
            async fn deliver_result(
                &self,
                _user_id: &str,
                _request_id: &str,
                _edge_agent_id: &str,
                _result_json: &str,
            ) -> Result<bool, String> {
                Ok(false)
            }
            async fn fail_dispatch(
                &self,
                _user_id: &str,
                _request_id: &str,
                _reason: &str,
            ) -> Result<bool, String> {
                Ok(false)
            }
            async fn wait_result(
                &self,
                _user_id: &str,
                _request_id: &str,
                _timeout: std::time::Duration,
            ) -> Result<Option<String>, String> {
                Ok(None)
            }
            async fn cleanup_stale(&self, _older_than: std::time::Duration) -> Result<u64, String> {
                Ok(0)
            }
        }
        struct MockRegistry;
        #[async_trait::async_trait]
        impl EdgeRegistryService for MockRegistry {
            async fn register_or_update(
                &self,
                _user_id: &str,
                _edge_agent_id: &str,
                _edge_id_header: &str,
                _hostname: Option<&str>,
                _worktree_path: Option<&str>,
                _capabilities: Option<serde_json::Value>,
            ) -> Result<astra_services::multi_agent::EdgeAgentRecord, String> {
                Err("mock".into())
            }
            async fn heartbeat(
                &self,
                _user_id: &str,
                _edge_agent_id: &str,
                _edge_id_header: &str,
            ) -> Result<(), String> {
                Ok(())
            }
            async fn list_by_user(
                &self,
                _user_id: &str,
            ) -> Result<Vec<astra_services::multi_agent::EdgeAgentRecord>, String> {
                Ok(vec![])
            }
            async fn unregister(&self, _user_id: &str, _edge_agent_id: &str) -> Result<(), String> {
                Ok(())
            }
        }

        let provider = EdgeConnectionProvider::new(5)
            .with_edge_connection_pool(pool)
            .with_edge_dispatch_service(Arc::new(MockDispatch))
            .with_edge_registry_service(Arc::new(MockRegistry));
        assert!(provider.health_check().await.is_ok());
    }
}
