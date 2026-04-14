//! Default tool executor — the shared implementation used by CLI, server, and edge.
//!
//! Routes tool calls to the appropriate module (fs_ops, shell_ops, git_ops, etc.)
//! and returns [`ToolResult`]. Consumers wrap this with their own context
//! (e.g., `ServerToolExecutor` adds resource governance and process isolation,
//! `CliToolExecutor` adds terminal UI and MCP dispatch).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    FileEditJournal, GitRollbackJournal, ToolApprovalGate, ToolContext, ToolExecutor,
    ToolProgressCallback, ToolResult,
};

/// Default tool executor with the full shared tool set.
pub struct DefaultToolExecutor {
    ctx: ToolContext,
    approval_gate: Option<Arc<dyn ToolApprovalGate>>,
    progress_callback: Option<Arc<dyn ToolProgressCallback>>,
    file_journal: Option<Arc<std::sync::Mutex<dyn FileEditJournal>>>,
    git_journal: Option<Arc<std::sync::Mutex<dyn GitRollbackJournal>>>,
}

impl DefaultToolExecutor {
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            approval_gate: None,
            progress_callback: None,
            file_journal: None,
            git_journal: None,
        }
    }

    pub fn with_approval_gate(mut self, gate: Arc<dyn ToolApprovalGate>) -> Self {
        self.approval_gate = Some(gate);
        self
    }

    pub fn with_progress_callback(mut self, cb: Arc<dyn ToolProgressCallback>) -> Self {
        self.progress_callback = Some(cb);
        self
    }

    pub fn with_file_journal(mut self, j: Arc<std::sync::Mutex<dyn FileEditJournal>>) -> Self {
        self.file_journal = Some(j);
        self
    }

    pub fn with_git_journal(mut self, j: Arc<std::sync::Mutex<dyn GitRollbackJournal>>) -> Self {
        self.git_journal = Some(j);
        self
    }

    /// Access the underlying context.
    pub fn context(&self) -> &ToolContext {
        &self.ctx
    }

    /// Workspace root path.
    pub fn workspace_root(&self) -> &Path {
        &self.ctx.workspace_root
    }
}

#[async_trait]
impl ToolExecutor for DefaultToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> ToolResult {
        // ── Approval gate ────────────────────────────────────────────
        if let Some(gate) = &self.approval_gate
            && gate.requires_approval(name)
        {
            let request_id = uuid::Uuid::new_v4().to_string();
            let decision = gate.request_approval(&request_id, name, args).await;
            match decision {
                crate::ApprovalDecision::Approved => {}
                crate::ApprovalDecision::Denied { reason } => {
                    let msg = reason.unwrap_or_else(|| "denied by user".into());
                    return ToolResult::error(format!("Tool execution denied: {msg}"));
                }
                crate::ApprovalDecision::Timeout => {
                    return ToolResult::error(
                        "Tool execution denied: approval request timed out".into(),
                    );
                }
            }
        }

        // ── Progress notification ────────────────────────────────────
        let call_id = uuid::Uuid::new_v4().to_string();
        if let Some(cb) = &self.progress_callback {
            cb.tool_started(&call_id, name, args).await;
        }

        let result = self.dispatch(name, args).await;

        if let Some(cb) = &self.progress_callback {
            cb.tool_completed(&call_id, &result.output, !result.is_error)
                .await;
        }

        result
    }

    fn tool_schemas(&self) -> Vec<Value> {
        crate::schemas::all_tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.ctx.project_root
    }
}

impl DefaultToolExecutor {
    async fn dispatch(&self, name: &str, args: &Value) -> ToolResult {
        let ws = &self.ctx.workspace_root;
        match name {
            // ── Memory tools (HTTP proxy) ────────────────────────────
            "memory_retrieve" | "memory_store" | "memory_search" | "memory_purge"
            | "memory_correct" | "memory_profile" | "memory_feedback" => {
                // Memoria tools are dispatched by the wrapping executor
                // (ServerToolExecutor / CliToolExecutor) which has HTTP client config.
                ToolResult::error(format!(
                    "Error: Memory tool '{name}' requires a configured memoria endpoint.                      Use ServerToolExecutor or CliToolExecutor instead of DefaultToolExecutor."
                ))
            }

            // ── Web search ───────────────────────────────────────────
            "web_search" => {
                let text = crate::web_search::web_search(args);
                if text.starts_with("Error") {
                    ToolResult::error(text)
                } else {
                    ToolResult::text(text)
                }
            }

            // ── File operations ──────────────────────────────────────
            "read_file" => crate::fs_ops::read_file(ws, args),
            "write_file" => crate::fs_ops::write_file(ws, args),
            "str_replace" => crate::fs_ops::str_replace(ws, args),
            "delete_file" => crate::fs_ops::delete_file(ws, args),
            "list_dir" => crate::fs_ops::list_dir(ws, args),

            // ── Shell operations ─────────────────────────────────────
            "bash" => crate::shell_ops::execute_bash(ws, args).await,
            "grep" => crate::shell_ops::grep(ws, args),
            "glob" => crate::shell_ops::glob(ws, args),

            // ── Git operations ───────────────────────────────────────
            "git_status" => crate::git_ops::status(ws),
            "git_diff" => crate::git_ops::diff(ws, args),
            "git_log" => crate::git_ops::log(ws, args),
            "git_show" => crate::git_ops::show(ws, args),
            "git_blame" => crate::git_ops::blame(ws, args),
            "git_commit" => crate::git_ops::commit(ws, args),

            // ── Delegation placeholder ───────────────────────────────
            "delegate" => ToolResult::text(
                "Delegation request acknowledged. Sub-agent will be spawned.".into(),
            ),

            // ── Unknown tool ─────────────────────────────────────────
            _ => ToolResult::error(format!(
                "Error: Tool '{name}' not available in DefaultToolExecutor"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_executor() -> (TempDir, DefaultToolExecutor) {
        let tmp = TempDir::new().unwrap();
        let ctx = ToolContext::test(tmp.path());
        let exec = DefaultToolExecutor::new(ctx);
        (tmp, exec)
    }

    #[tokio::test]
    async fn dispatch_read_file() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("hello.txt"), "world").unwrap();
        let result = exec
            .execute("read_file", &serde_json::json!({"path": "hello.txt"}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("world"));
    }

    #[tokio::test]
    async fn dispatch_write_file() {
        let (tmp, exec) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &serde_json::json!({"path": "out.txt", "content": "data"}),
            )
            .await;
        assert!(!result.is_error);
        assert!(tmp.path().join("out.txt").exists());
    }

    #[tokio::test]
    async fn dispatch_unknown_tool() {
        let (_tmp, exec) = test_executor();
        let result = exec.execute("nonexistent", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.output.contains("not available"));
    }

    #[tokio::test]
    async fn dispatch_list_dir() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("a.rs"), "").unwrap();
        let result = exec
            .execute("list_dir", &serde_json::json!({"path": "."}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("a.rs"));
    }

    #[tokio::test]
    async fn dispatch_bash_echo() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("bash", &serde_json::json!({"command": "echo hello"}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn dispatch_str_replace() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("f.txt"), "old text here").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &serde_json::json!({
                    "path": "f.txt", "old_str": "old text", "new_str": "new text"
                }),
            )
            .await;
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "new text here");
    }
}
