//! SandboxRunnerProvider — sandboxed execution provider.
//!
//! In L1 this is a stub; in L2 it bridges to the sandbox runner pool
//! via a pluggable delegate (e.g. ToolExecutionService wired through
//! the RunnerRpc or SandboxResidentAgent transport).

use astra_runtime_env::IsolationIntent;
use async_trait::async_trait;
use futures_util::future::BoxFuture;
use std::sync::Arc;

use super::traits::{CapabilityProvider, ProviderError, ToolRequest, ToolResult};
use super::types::{ProviderKind, ToolCapability, ToolCategory};

// ---------------------------------------------------------------------------
// SandboxRunnerProvider
// ---------------------------------------------------------------------------

/// Provider that delegates to sandboxed execution environments
/// (Firecracker microVMs, Docker containers, etc.).
///
/// Sandbox tools are routed through a pluggable delegate set at construction
/// time.  When no delegate is set, `execute()` returns an error.
#[derive(Clone)]
pub struct SandboxRunnerProvider {
    /// Priority for routing (lower = preferred).
    priority: u8,
    /// The isolation level this sandbox provides.
    isolation: IsolationIntent,
    /// Pluggable sandbox delegate — bridges to RunnerRpc or
    /// SandboxResidentAgent transport.
    delegate: Option<Arc<dyn Fn(ToolRequest) -> BoxFuture<'static, ToolResult> + Send + Sync>>,
}

impl std::fmt::Debug for SandboxRunnerProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SandboxRunnerProvider")
            .field("priority", &self.priority)
            .field("isolation", &self.isolation)
            .field("delegate", &self.delegate.as_ref().map(|_| "<fn>"))
            .finish()
    }
}

impl SandboxRunnerProvider {
    /// Create a new provider with the given routing priority and isolation.
    pub fn new(priority: u8, isolation: IsolationIntent) -> Self {
        Self {
            priority,
            isolation,
            delegate: None,
        }
    }

    /// Attach a sandbox delegate that will handle every `execute()` call.
    pub fn with_delegate<F>(mut self, delegate: F) -> Self
    where
        F: Fn(ToolRequest) -> BoxFuture<'static, ToolResult> + Send + Sync + 'static,
    {
        self.delegate = Some(Arc::new(delegate));
        self
    }
}

#[async_trait]
impl CapabilityProvider for SandboxRunnerProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::SandboxRunner
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
        match &self.delegate {
            Some(_) => Ok(()),
            None => Err(ProviderError::Unhealthy(
                "SandboxRunnerProvider: no delegate configured — provider is a stub".into(),
            )),
        }
    }

    async fn execute(&self, request: ToolRequest) -> ToolResult {
        match &self.delegate {
            Some(delegate) => delegate(request).await,
            None => ToolResult::Error {
                message: "SandboxRunnerProvider is not wired yet (no delegate)".into(),
                retryable: false,
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
        // Sandbox has workspace access via mounted user volume.
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::FutureExt;
    use serde_json;

    #[tokio::test]
    async fn kind_is_sandbox_runner() {
        let provider = SandboxRunnerProvider::new(20, IsolationIntent::Container);
        assert_eq!(provider.kind(), ProviderKind::SandboxRunner);
    }

    #[tokio::test]
    async fn capabilities_declared() {
        let provider = SandboxRunnerProvider::new(20, IsolationIntent::Container);
        let caps = provider.capabilities().await;
        assert!(!caps.is_empty());
        assert!(caps.len() >= 3);
    }

    #[tokio::test]
    async fn execute_without_delegate_returns_error() {
        let provider = SandboxRunnerProvider::new(20, IsolationIntent::Container);
        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash".into(),
            tool_call_id: "call-1".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::Container,
            storage: None,
        };
        let result = provider.execute(request).await;
        match result {
            ToolResult::Error { message, .. } => {
                assert!(message.contains("no delegate"));
            }
            _ => panic!("expected Error, got Success"),
        }
    }

    #[tokio::test]
    async fn execute_with_delegate_forwards_to_delegate() {
        let provider =
            SandboxRunnerProvider::new(20, IsolationIntent::Container).with_delegate(|request| {
                async move {
                    ToolResult::Success {
                        data: serde_json::Value::String(format!("ok: {}", request.tool_name)),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    }
                }
                .boxed()
            });

        let request = ToolRequest {
            capability: ToolCapability::Named("bash".into()),
            tool_name: "bash_test".into(),
            tool_call_id: "call-2".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::Container,
            storage: None,
        };
        let result = provider.execute(request).await;
        match result {
            ToolResult::Success { data, .. } => {
                assert_eq!(data, serde_json::Value::String("ok: bash_test".into()));
            }
            _ => panic!("expected Success, got Error"),
        }
    }

    #[tokio::test]
    async fn health_check_fails_without_delegate() {
        let provider = SandboxRunnerProvider::new(20, IsolationIntent::Container);
        assert!(provider.health_check().await.is_err());
    }

    #[tokio::test]
    async fn health_check_ok_with_delegate() {
        let provider =
            SandboxRunnerProvider::new(20, IsolationIntent::Container).with_delegate(|_req| {
                Box::pin(async {
                    ToolResult::Success {
                        data: serde_json::Value::String("ok".into()),
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: 0,
                    }
                })
            });
        assert!(provider.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn isolation_level_preserved() {
        let provider = SandboxRunnerProvider::new(20, IsolationIntent::Sandbox);
        assert_eq!(provider.isolation_level(), IsolationIntent::Sandbox);
    }
}
