//! SandboxRuntimeProvider — sandboxed tool execution (Firecracker, Docker, etc.).
//!
//! Provides capability-driven routing for tools that require stronger
//! isolation than the server process or edge CLI can provide.  The actual
//! sandbox lifecycle (create, start, stop, snapshot) is managed by the
//! sandbox runner infrastructure; this provider is the CapabilityProvider
//! facet that plugs into the capability registry and routing engine.

use std::sync::Arc;

use astra_runtime_env::IsolationIntent;
use async_trait::async_trait;

use super::traits::{CapabilityProvider, ProviderError, ToolRequest, ToolResult};
use super::types::{ProviderKind, ToolCapability};

// ---------------------------------------------------------------------------
// SandboxRuntimeProvider
// ---------------------------------------------------------------------------

/// Provider for sandboxed tool execution.
///
/// Offers Shell, FileSystem, and VersionControl capabilities at Container
/// isolation level.  When no sandbox runtime service is configured, health
/// checks fail gracefully rather than panicking.
///
/// ## Architecture note
///
/// In the current L1/L2 snapshot, the sandbox runtime service wire-up is
/// still in progress.  `execute()` returns a descriptive `NotImplemented`
/// error until the full sandbox lifecycle (create → start → exec → stop →
/// destroy) is integrated.
#[derive(Clone)]
pub struct SandboxRuntimeProvider {
    /// Priority for routing (lower = preferred).
    priority: u8,
    /// Isolation level this sandbox provides.
    isolation: IsolationIntent,
    /// Optional sandbox runtime service handle (wired in L3+).
    sandbox_service: Option<Arc<dyn SandboxRuntimeService>>,
}

/// Service trait for sandbox lifecycle operations.
///
/// Implemented by the sandbox runner infrastructure (Firecracker
/// microVM manager, Docker runtime bridge, etc.).  The provider
/// delegates `execute()` to this service when it is available.
#[async_trait]
pub trait SandboxRuntimeService: Send + Sync {
    /// Check whether the sandbox daemon is reachable and responsive.
    async fn health_check(&self) -> Result<(), String>;

    /// Execute a tool call inside a sandbox.
    async fn execute_sandboxed(
        &self,
        tool_name: &str,
        args: &serde_json::Value,
        user_id: &str,
        run_id: &str,
        session_id: &str,
    ) -> ToolResult;
}

// ---------------------------------------------------------------------------
// Construction
// ---------------------------------------------------------------------------

impl SandboxRuntimeProvider {
    /// Create a provider with Container-level isolation.
    pub fn new(priority: u8) -> Self {
        Self {
            priority,
            isolation: IsolationIntent::Container,
            sandbox_service: None,
        }
    }

    /// Create a provider with a custom isolation level.
    pub fn with_isolation(mut self, isolation: IsolationIntent) -> Self {
        self.isolation = isolation;
        self
    }

    /// Attach a sandbox runtime service for actual execution.
    pub fn with_sandbox_service(mut self, service: Arc<dyn SandboxRuntimeService>) -> Self {
        self.sandbox_service = Some(service);
        self
    }
}

// ---------------------------------------------------------------------------
// CapabilityProvider impl
// ---------------------------------------------------------------------------

#[async_trait]
impl CapabilityProvider for SandboxRuntimeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::SandboxRuntime
    }

    async fn capabilities(&self) -> Vec<ToolCapability> {
        let registry = astra_runtime_env::ToolRegistry::builtins();
        astra_runtime_env::runtime_workspace_provider(
            astra_runtime_env::CapacityProviderType::Sandbox,
            "sandbox-runtime",
            &registry,
        )
        .tool_names
        .into_iter()
        .map(ToolCapability::Named)
        .collect()
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        match &self.sandbox_service {
            Some(svc) => svc.health_check().await.map_err(|e| {
                ProviderError::Unhealthy(format!("sandbox daemon unreachable: {}", e))
            }),
            None => Err(ProviderError::Unhealthy(
                "SandboxRuntimeProvider: no sandbox service configured".into(),
            )),
        }
    }

    async fn execute(
        &self,
        request: ToolRequest,
        cancel_token: Option<&std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> ToolResult {
        // Cooperative cancellation gate: bail out before the (expensive)
        // sandbox dispatch if the caller already cancelled. Mid-flight
        // cancellation is unsafe here because the sandboxed process may have
        // already mutated state — so we only honor preflight cancellation.
        if let Some(token) = cancel_token {
            if token.is_cancelled() {
                return ToolResult::Error {
                    message: format!(
                        "SandboxRuntimeProvider: tool '{}' cancelled before dispatch",
                        request.tool_name
                    ),
                    retryable: false,
                    exit_code: None,
                    metadata: None,
                };
            }
        }
        match &self.sandbox_service {
            Some(svc) => {
                svc.execute_sandboxed(
                    &request.tool_name,
                    &request.parameters,
                    &request.user_id,
                    &request.run_id,
                    &request.session_id,
                )
                .await
            }
            None => ToolResult::Error {
                message: "SandboxRuntimeProvider: sandbox service not configured. \
                          Wire a SandboxRuntimeService via with_sandbox_service()."
                    .into(),
                retryable: false,
                exit_code: None,
                metadata: None,
            },
        }
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    fn isolation_level(&self) -> IsolationIntent {
        self.isolation
    }

    async fn storage_accessible(&self) -> bool {
        // Sandbox has storage access only when a volume mount is configured.
        // The sandbox service can verify this at runtime.
        self.sandbox_service.is_some()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::types::ToolCapability;

    /// Stub sandbox service that returns canned responses.
    struct StubSandboxService;
    #[async_trait]
    impl SandboxRuntimeService for StubSandboxService {
        async fn health_check(&self) -> Result<(), String> {
            Ok(())
        }
        async fn execute_sandboxed(
            &self,
            tool_name: &str,
            _args: &serde_json::Value,
            _user_id: &str,
            _run_id: &str,
            _session_id: &str,
        ) -> ToolResult {
            ToolResult::Success {
                data: serde_json::Value::String(format!("sandboxed {}", tool_name)),
                stdout: format!("sandboxed {}\n", tool_name),
                stderr: String::new(),
                exit_code: 0,
                metadata: None,
            }
        }
    }

    fn test_request(tool_name: &str) -> ToolRequest {
        ToolRequest {
            capability: ToolCapability::Named(tool_name.to_string()),
            tool_name: tool_name.to_string(),
            tool_call_id: "call-1".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::Container,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
        }
    }

    #[test]
    fn kind_is_sandbox_runtime() {
        let provider = SandboxRuntimeProvider::new(10);
        assert_eq!(provider.kind(), ProviderKind::SandboxRuntime);
    }

    #[test]
    fn isolation_is_container_by_default() {
        let provider = SandboxRuntimeProvider::new(10);
        assert_eq!(provider.isolation_level(), IsolationIntent::Container);
    }

    #[test]
    fn custom_isolation_level() {
        let provider = SandboxRuntimeProvider::new(10).with_isolation(IsolationIntent::Process);
        assert_eq!(provider.isolation_level(), IsolationIntent::Process);
    }

    #[tokio::test]
    async fn capabilities_are_named_runtime_workspace_tools() {
        let provider = SandboxRuntimeProvider::new(10);
        let caps = provider.capabilities().await;
        assert!(caps.contains(&ToolCapability::Named("bash".into())));
        assert!(caps.contains(&ToolCapability::Named("read_file".into())));
        assert!(caps.contains(&ToolCapability::Named("git".into())));
        assert!(!caps.contains(&ToolCapability::Named("memory".into())));
    }

    #[tokio::test]
    async fn health_check_fails_without_service() {
        let provider = SandboxRuntimeProvider::new(10);
        let result = provider.health_check().await;
        assert!(result.is_err());
        match result {
            Err(ProviderError::Unhealthy(msg)) => {
                assert!(msg.contains("no sandbox service"));
            }
            _ => panic!("expected Unhealthy error"),
        }
    }

    #[tokio::test]
    async fn health_check_passes_with_service() {
        let provider =
            SandboxRuntimeProvider::new(10).with_sandbox_service(Arc::new(StubSandboxService));
        let result = provider.health_check().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn execute_without_service_returns_error() {
        let provider = SandboxRuntimeProvider::new(10);
        let request = test_request("bash");
        let result = provider.execute(request, None).await;
        match result {
            ToolResult::Error { message, .. } => {
                assert!(message.contains("sandbox service not configured"));
            }
            _ => panic!("expected Error, got Success"),
        }
    }

    #[tokio::test]
    async fn execute_with_service_delegates() {
        let provider =
            SandboxRuntimeProvider::new(10).with_sandbox_service(Arc::new(StubSandboxService));
        let request = test_request("bash");
        let result = provider.execute(request, None).await;
        match result {
            ToolResult::Success { data, .. } => {
                assert_eq!(data, serde_json::Value::String("sandboxed bash".into()));
            }
            _ => panic!("expected Success"),
        }
    }

    #[test]
    fn storage_accessible_false_without_service() {
        let provider = SandboxRuntimeProvider::new(10);
        // sync wrappers may not exist; test the async method instead
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(!rt.block_on(provider.storage_accessible()));
    }

    #[test]
    fn storage_accessible_true_with_service() {
        let provider =
            SandboxRuntimeProvider::new(10).with_sandbox_service(Arc::new(StubSandboxService));
        let rt = tokio::runtime::Runtime::new().unwrap();
        assert!(rt.block_on(provider.storage_accessible()));
    }
}
