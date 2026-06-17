//! ServerBuiltinProvider — routes server-in-process tool calls.
//!
//! In L1, this was a stub that returned NotCapable for every `execute()` call.
//! In L2, this is wired to the actual `ServerToolExecutor` via the
//! `ServerToolRuntime` trait, so server-local tools execute directly.

use astra_runtime_env::IsolationIntent;
use async_trait::async_trait;
use serde_json;
use std::sync::Arc;

use super::traits::{
    CapabilityProvider, ProviderError, ServerToolRuntime, ToolRequest, ToolResult,
};
use super::types::{ProviderKind, ToolCapability, ToolCategory};

// ---------------------------------------------------------------------------
// ServerBuiltinProvider
// ---------------------------------------------------------------------------

/// Provider for server-builtin tools.
///
/// Declares all server-side capabilities (memory, task, session, agent, MCP,
/// web fetch/search, github, symbols) and delegates execution to a
/// `ServerToolRuntime` (backed by `ServerToolExecutor`).
///
/// ## Tool filtering
///
/// When `tools` is `Some(list)`, `capabilities()` returns only `Named`
/// entries for those tools — enabling deployment profiles to control exactly
/// which tools are available.  When `None` (default), the full category-based
/// capabilities are declared (backward-compatible).
pub struct ServerBuiltinProvider {
    /// Priority for routing (lower = preferred).
    priority: u8,
    /// Runtime that actually executes the tool call on the server.
    runtime: Arc<dyn ServerToolRuntime>,
    /// Optional tool-name whitelist.  When set, only these tools are offered.
    tools: Option<Vec<String>>,
}

impl std::fmt::Debug for ServerBuiltinProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerBuiltinProvider")
            .field("priority", &self.priority)
            .finish_non_exhaustive()
    }
}

impl ServerBuiltinProvider {
    /// Create a new provider with the given priority and execution runtime.
    ///
    /// `tools`, when provided, restricts this provider to only those
    /// tool names.  Pass `None` for the full category-based set.
    pub fn new(
        priority: u8,
        runtime: Arc<dyn ServerToolRuntime>,
        tools: Option<Vec<String>>,
    ) -> Self {
        Self {
            priority,
            runtime,
            tools,
        }
    }
}

#[async_trait]
impl CapabilityProvider for ServerBuiltinProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::ServerBuiltin
    }

    async fn capabilities(&self) -> Vec<ToolCapability> {
        if let Some(ref tools) = self.tools {
            tools
                .iter()
                .map(|t| ToolCapability::Named(t.clone()))
                .collect()
        } else {
            vec![
                // ── State management (server-owned) ──
                ToolCapability::Category(ToolCategory::StateManagement),
                // ── Agent delegation ──
                ToolCapability::Category(ToolCategory::AgentDelegation),
                // ── MCP protocol ──
                ToolCapability::Category(ToolCategory::McpProtocol),
                // ── External API ──
                ToolCapability::Category(ToolCategory::ExternalApi),
                // ── Symbols / LSP (can run in-server when no workspace needed) ──
                ToolCapability::Category(ToolCategory::Symbols),
            ]
        }
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        // Server-builtin is always healthy (in-process).
        Ok(())
    }

    async fn execute(&self, request: ToolRequest) -> ToolResult {
        self.runtime
            .execute_local_tool(&request.tool_name, &request.parameters)
            .await
    }

    fn priority(&self) -> u8 {
        self.priority
    }

    fn isolation_level(&self) -> IsolationIntent {
        // Server-builtin runs in-process — no isolation.
        IsolationIntent::None
    }

    async fn storage_accessible(&self) -> bool {
        // Server process has access to the workspace directory.
        true
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::types::ToolCapability;

    /// Minimal test runtime that returns a canned response.
    struct TestRuntime;
    #[async_trait]
    impl ServerToolRuntime for TestRuntime {
        async fn execute_local_tool(&self, name: &str, _args: &serde_json::Value) -> ToolResult {
            ToolResult::Success {
                data: serde_json::Value::String(format!("executed {}", name)),
                stdout: format!("executed {}\n", name),
                stderr: String::new(),
                exit_code: 0,
            }
        }
    }

    fn test_request(tool_name: &str) -> ToolRequest {
        ToolRequest {
            capability: ToolCapability::Named(tool_name.to_string()),
            tool_name: tool_name.to_string(),
            tool_call_id: "call-1".into(),
            parameters: serde_json::Value::Null,
            isolation_required: IsolationIntent::None,
            storage: None,
        }
    }

    #[tokio::test]
    async fn kind_is_server_builtin() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        assert_eq!(provider.kind(), ProviderKind::ServerBuiltin);
    }

    #[tokio::test]
    async fn capabilities_declared() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        let caps = provider.capabilities().await;
        assert!(!caps.is_empty());
        assert!(caps.len() >= 4);
    }

    #[tokio::test]
    async fn execute_delegates_to_runtime() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        let request = test_request("memory");
        let result = provider.execute(request).await;
        match result {
            ToolResult::Success { data, .. } => {
                let s = data.as_str().unwrap_or("");
                assert!(s.contains("executed memory"), "got: {}", s);
            }
            _ => panic!("expected Success, got {:?}", result),
        }
    }

    #[tokio::test]
    async fn health_check_ok() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        assert!(provider.health_check().await.is_ok());
    }

    #[tokio::test]
    async fn tools_whitelist_restricts_capabilities() {
        let provider = ServerBuiltinProvider::new(
            10,
            Arc::new(TestRuntime),
            Some(vec!["bash".into(), "read_file".into()]),
        );
        let caps = provider.capabilities().await;
        assert_eq!(caps.len(), 2);
        assert!(caps.contains(&ToolCapability::Named("bash".into())));
        assert!(caps.contains(&ToolCapability::Named("read_file".into())));
        assert!(!caps.contains(&ToolCapability::Named("write_file".into())));
    }

    #[tokio::test]
    async fn tools_none_returns_categories() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        let caps = provider.capabilities().await;
        // Without whitelist, returns categories, not named tools.
        assert!(
            caps.iter()
                .any(|c| matches!(c, ToolCapability::Category(_)))
        );
    }
}
