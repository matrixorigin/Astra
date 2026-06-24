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
use super::types::{ProviderKind, ToolCapability};

/// Exact tools provided by the server builtin provider by default.
///
/// This is intentionally a named-tool inventory rather than category
/// capabilities: category matching is broad enough to include tools such as
/// `lsp` and `find_definition` that are valid runtime tools but are not
/// server-builtin handlers.
pub const SERVER_BUILTIN_TOOL_NAMES: &[&str] = &[
    // Shell
    "bash",
    // FileSystem
    "read_file",
    "write_file",
    "str_replace",
    "list_dir",
    "grep",
    "glob",
    // VersionControl
    "git",
    // ExternalApi
    "web_search",
    "web_fetch",
    "github",
    "tool_search",
    // StateManagement
    "memory",
    "session",
    "task",
    "mo_query",
    "rollback_database_snapshots",
    "rollback_session_state",
    // AgentDelegation
    "agent",
    "agent_fanout",
    // User interaction
    "ask_user",
    "notify",
    // Plan mode
    "enter_plan_mode",
    "exit_plan_mode",
    // Symbols / introspection
    "get_agent_info",
    "symbols",
    "introspect",
    // Tool preference
    "prioritize_tool",
    "deprioritize_tool",
    // Context management
    "compress_context",
    // Artifact publishing
    "publish_artifact",
    // Programmatic scripting
    "run_script",
];

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
/// which tools are available. When `None` (default), the provider declares the
/// exact server-builtin inventory in [`SERVER_BUILTIN_TOOL_NAMES`].
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
    /// tool names. Pass `None` for the exact default server-builtin inventory.
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
        match self.tools.as_deref() {
            Some(tools) => tools
                .iter()
                .map(|tool| ToolCapability::Named(tool.clone()))
                .collect(),
            None => SERVER_BUILTIN_TOOL_NAMES
                .iter()
                .map(|tool| ToolCapability::Named((*tool).to_string()))
                .collect(),
        }
    }

    async fn health_check(&self) -> Result<(), ProviderError> {
        // Server-builtin is always healthy (in-process).
        Ok(())
    }

    async fn execute(
        &self,
        request: ToolRequest,
        _cancel_token: Option<&std::sync::Arc<tokio_util::sync::CancellationToken>>,
    ) -> ToolResult {
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
            isolation_required: IsolationIntent::None,
            storage: None,
            user_id: "test-user".into(),
            run_id: "test-run".into(),
            session_id: "test-session".into(),
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
        assert_eq!(caps.len(), SERVER_BUILTIN_TOOL_NAMES.len());
        assert!(caps.contains(&ToolCapability::Named("symbols".into())));
        assert!(!caps.contains(&ToolCapability::Named("lsp".into())));
    }

    #[tokio::test]
    async fn execute_delegates_to_runtime() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        let request = test_request("memory");
        let result = provider.execute(request, None).await;
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
    async fn tools_none_returns_exact_named_inventory() {
        let provider = ServerBuiltinProvider::new(10, Arc::new(TestRuntime), None);
        let caps = provider.capabilities().await;
        assert!(caps.iter().all(|c| matches!(c, ToolCapability::Named(_))));
        assert!(
            SERVER_BUILTIN_TOOL_NAMES
                .iter()
                .all(|tool| caps.contains(&ToolCapability::Named((*tool).to_string())))
        );
    }
}
