//! MCP (Model Context Protocol) tool dispatch with auto-reconnect.
//!
//! Routes tool calls to the appropriate MCP server, handling connection
//! failures with automatic reconnect and retry.

use serde_json::Value;

use super::ToolExecutor;

impl ToolExecutor {
    pub(super) async fn execute_mcp_tool(&self, mcp_name: &str, args: &Value) -> String {
        let manager_arc = match &self.mcp_manager {
            Some(m) => m.clone(),
            None => {
                return format!("Error: MCP not available. Tool '{mcp_name}' cannot be executed.");
            }
        };

        // Resolve the sanitized MCP name to server + original tool name, and get the
        // connection Arc — all in a single read lock to avoid TOCTOU races.
        let (server_name, original_name, conn) = {
            let mgr = manager_arc.read().await;
            let (srv, tool) = match mgr.find_tool_by_mcp_name(mcp_name) {
                Some((s, t)) => (s.to_string(), t.to_string()),
                None => {
                    return format!(
                        "Error: MCP tool '{mcp_name}' not found on any connected server."
                    );
                }
            };
            let c = match mgr.get(&srv) {
                Some(c) => c,
                None => return format!("Error: MCP server '{srv}' not connected."),
            };
            (srv, tool, c)
        };

        // Call tool (no lock held during await)
        match conn.call_tool(&original_name, args.clone()).await {
            Ok(result) => {
                return crate::mcp_client::extract_result_text_with_limit(
                    &result,
                    crate::mcp_client::MAX_RESULT_CONTENT_LENGTH,
                );
            }
            Err(e) => {
                eprintln!(
                    "  ↻ MCP tool '{}' failed on '{}': {e}, attempting reconnect…",
                    original_name, server_name
                );
            }
        }

        // Reconnect and retry — with tokio RwLock we can hold write lock across await
        {
            let mut mgr = manager_arc.write().await;
            match mgr.reconnect(&server_name).await {
                Ok(tool_count) => {
                    eprintln!(
                        "  ✓ Reconnected to '{}' ({} tools), retrying…",
                        server_name, tool_count
                    );
                }
                Err(e) => {
                    return format!(
                        "Error: MCP tool '{}' failed and reconnect to '{}' also failed: {e}",
                        original_name, server_name
                    );
                }
            }
        }

        // Retry the call with fresh connection
        let conn = {
            let mgr = manager_arc.read().await;
            match mgr.get(&server_name) {
                Some(c) => c,
                None => return format!("Error: MCP server '{server_name}' lost after reconnect."),
            }
        };

        match conn.call_tool(&original_name, args.clone()).await {
            Ok(result) => crate::mcp_client::extract_result_text_with_limit(
                &result,
                crate::mcp_client::MAX_RESULT_CONTENT_LENGTH,
            ),
            Err(e) => {
                format!(
                    "Error calling MCP tool '{original_name}' on server '{server_name}' after reconnect: {e}"
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::edge_tools::ToolExecutor;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Create a bare ToolExecutor with mcp_manager = None.
    fn executor_no_mcp() -> ToolExecutor {
        ToolExecutor::new("/tmp")
    }

    /// Create a ToolExecutor with an empty McpClientManager (no tools, no servers).
    fn executor_empty_mcp() -> ToolExecutor {
        let manager = Arc::new(RwLock::new(crate::mcp_client::McpClientManager::new()));
        let mut executor = ToolExecutor::new("/tmp");
        executor.install_mcp_bundle(manager, Vec::new());
        executor
    }

    // ── Error path: MCP not available ─────────────────────────────────────

    #[tokio::test]
    async fn dispatch_no_mcp_manager() {
        let executor = executor_no_mcp();
        let result = executor
            .execute_mcp_tool("mcp_test_tool", &serde_json::Value::Null)
            .await;
        assert!(result.contains("MCP not available"));
        assert!(result.contains("mcp_test_tool"));
    }

    // ── Error path: tool not found ────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_tool_not_found() {
        let executor = executor_empty_mcp();
        let result = executor
            .execute_mcp_tool("mcp_nonexistent_tool", &serde_json::Value::Null)
            .await;
        assert!(result.contains("not found on any connected server"));
        assert!(result.contains("mcp_nonexistent_tool"));
    }
}
