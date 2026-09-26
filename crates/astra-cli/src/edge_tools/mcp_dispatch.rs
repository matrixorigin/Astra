//! MCP (Model Context Protocol) tool dispatch with connection recovery.
//!
//! Routes tool calls to the appropriate MCP server, handling connection
//! failures without replaying a call whose result may have been lost.

use serde_json::Value;

use super::{ToolExecutionOutcome, ToolExecutor};

fn mcp_result_to_tool_outcome(result: astra_mcp::McpToolCallResult) -> ToolExecutionOutcome {
    let mut fields = serde_json::Map::new();
    if let Some(content) = result.structured_content {
        fields.insert("mcp_structured_content".to_string(), content);
    }
    if let Some(metadata) = result.protocol_metadata {
        fields.insert("mcp_protocol_metadata".to_string(), metadata);
    }
    ToolExecutionOutcome {
        output: result.output,
        is_error: result.is_error,
        tool_result_fields: (!fields.is_empty()).then_some(fields),
    }
}

impl ToolExecutor {
    pub(super) async fn execute_mcp_tool(
        &self,
        mcp_name: &str,
        args: &Value,
    ) -> ToolExecutionOutcome {
        let manager_arc = match self.mcp_runtime_snapshot("mcp_runtime_dispatch").manager {
            Some(m) => m.clone(),
            None => {
                return ToolExecutionOutcome::error(format!(
                    "Error: MCP not available. Tool '{mcp_name}' cannot be executed."
                ));
            }
        };

        // Resolve the sanitized MCP name to server + original tool name, and get the
        // connection Arc — all in a single read lock to avoid TOCTOU races.
        let (server_name, original_name, conn) = {
            let mgr = manager_arc.read().await;
            let (srv, tool) = match mgr.find_tool_by_mcp_name(mcp_name) {
                Some((s, t)) => (s.to_string(), t.to_string()),
                None => {
                    return ToolExecutionOutcome::error(format!(
                        "Error: MCP tool '{mcp_name}' not found on any connected server."
                    ));
                }
            };
            let c = match mgr.get(&srv) {
                Some(c) => c,
                None => {
                    return ToolExecutionOutcome::error(format!(
                        "Error: MCP server '{srv}' not connected."
                    ));
                }
            };
            (srv, tool, c)
        };

        // Once call_tool starts, a transport error cannot prove that the MCP
        // server did not apply the operation. Reconnect may restore the server
        // for later invocations, but must never replay this invocation.
        match conn.call_tool(&original_name, args.clone()).await {
            Ok(result) => {
                mcp_result_to_tool_outcome(astra_mcp::extract_tool_call_result_with_limit(
                    &result,
                    crate::mcp_client::MAX_RESULT_CONTENT_LENGTH,
                ))
            }
            Err(e) => {
                let error = astra_mcp::McpError::Service(e);
                if error.side_effects_maybe() {
                    let mut mgr = manager_arc.write().await;
                    // Another caller may already have replaced this connection.
                    if mgr
                        .get(&server_name)
                        .is_some_and(|current| std::sync::Arc::ptr_eq(&current, &conn))
                    {
                        if let Err(reconnect_error) = mgr.reconnect(&server_name).await {
                            tracing::warn!(
                                server = %server_name,
                                error = %reconnect_error,
                                "MCP connection recovery failed after an uncertain call"
                            );
                        }
                    }
                }

                let mut outcome = ToolExecutionOutcome::error(format!(
                    "Error: MCP tool '{original_name}' on server '{server_name}' has an unknown outcome ({error}). Check the server state before deciding whether to call it again."
                ));
                outcome.tool_result_fields = Some(serde_json::Map::from_iter([
                    (
                        "error_kind".into(),
                        Value::String("mcp_outcome_unknown".into()),
                    ),
                    ("dispatch_certainty".into(), Value::String("unknown".into())),
                    ("execution_fact".into(), Value::String("unknown".into())),
                    ("side_effects_maybe".into(), Value::Bool(true)),
                    ("retryable".into(), Value::Bool(false)),
                    ("resumable".into(), Value::Bool(true)),
                ]));
                outcome
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::edge_tools::ToolExecutor;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    /// Create a bare ToolExecutor without an MCP runtime snapshot.
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
    async fn dispatch_without_mcp_runtime() {
        let executor = executor_no_mcp();
        let result = executor
            .execute_mcp_tool("mcp_test_tool", &serde_json::Value::Null)
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("MCP not available"));
        assert!(result.output.contains("mcp_test_tool"));
    }

    // ── Error path: tool not found ────────────────────────────────────────

    #[tokio::test]
    async fn dispatch_tool_not_found() {
        let executor = executor_empty_mcp();
        let result = executor
            .execute_mcp_tool("mcp_nonexistent_tool", &serde_json::Value::Null)
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("not found on any connected server"));
        assert!(result.output.contains("mcp_nonexistent_tool"));
    }

    #[tokio::test]
    async fn pr887_review_mcp_lost_ack_does_not_repeat_mutation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let counter = dir.path().join("applied.txt");
        let binary = crate::mcp_client::ensure_mock_mcp_server_binary();
        let mut manager = crate::mcp_client::McpClientManager::new();
        manager
            .connect(crate::mcp_client::McpServerConfig {
                name: "lost_ack".into(),
                transport: crate::mcp_client::Transport::Stdio {
                    command: vec![binary.to_string_lossy().into_owned()],
                    args: vec![],
                    env: Default::default(),
                },
                description: String::new(),
                enabled: true,
                retry: Default::default(),
            })
            .await
            .expect("connect real stdio MCP process");
        let schemas = manager.all_tool_schemas();
        let mut executor = ToolExecutor::new(dir.path());
        executor.install_mcp_bundle(Arc::new(RwLock::new(manager)), schemas.clone());
        executor.set_current_visible_tool_schemas(&schemas);

        let result = astra_tools::ToolExecutor::execute(
            &executor,
            "mcp__lost_ack__apply_then_drop_ack",
            &json!({"path": counter}),
        )
        .await;
        assert!(result.is_error, "{result:?}");
        assert!(result.output.contains("unknown outcome"), "{result:?}");
        let metadata = result.metadata.as_ref().expect("typed uncertainty");
        assert_eq!(metadata["side_effects_maybe"], true);
        assert_eq!(metadata["retryable"], false);
        assert_eq!(metadata["dispatch_certainty"], "unknown");
        assert_eq!(metadata["error_kind"], "mcp_outcome_unknown");
        assert_eq!(
            std::fs::read_to_string(&counter).expect("durable fixture mutation"),
            "applied\n",
            "the lost acknowledgement must not cause a second physical execution"
        );

        let next = astra_tools::ToolExecutor::execute(
            &executor,
            "mcp__lost_ack__echo",
            &json!({"message": "reconnected"}),
        )
        .await;
        assert!(
            !next.is_error,
            "later MCP calls should use the recovered connection: {next:?}"
        );
        assert!(next.output.contains("reconnected"), "{next:?}");
        assert_eq!(
            std::fs::read_to_string(&counter).expect("durable fixture mutation"),
            "applied\n"
        );
    }

    #[test]
    fn mcp_status_is_preserved_independently_of_result_text() {
        let successful = super::mcp_result_to_tool_outcome(astra_mcp::McpToolCallResult {
            output: "Error: this is successful server-provided content".to_string(),
            structured_content: None,
            protocol_metadata: None,
            is_error: false,
        });
        assert!(!successful.is_error);

        let failed = super::mcp_result_to_tool_outcome(astra_mcp::McpToolCallResult {
            output: r#"{"status":"completed","data":"partial"}"#.to_string(),
            structured_content: None,
            protocol_metadata: None,
            is_error: true,
        });
        assert!(failed.is_error);
    }
}
