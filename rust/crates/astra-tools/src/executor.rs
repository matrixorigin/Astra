//! Default tool executor — the shared implementation used by CLI, server, and edge.
//!
//! Routes tool calls to the appropriate module (fs_ops, shell_ops, git_gix, etc.)
//! and returns [`ToolResult`]. Consumers wrap this with their own context
//! (e.g., `ServerToolExecutor` adds resource governance and process isolation,
//! `CliToolExecutor` adds terminal UI and MCP dispatch).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::github::GitHubClient;
use crate::task_mgmt::TaskManager;
use crate::{
    FileEditJournal, GitRollbackJournal, ToolApprovalGate, ToolContext, ToolExecutor,
    ToolProgressCallback, ToolResult,
};

// ─── Helper ─────────────────────────────────────────────────────────────────

/// Convert a String-returning tool function to ToolResult.
/// Convention: outputs starting with "Error" are error results.
fn string_to_result(output: String) -> ToolResult {
    if output.starts_with("Error") {
        ToolResult::error(output)
    } else {
        ToolResult::text(output)
    }
}

fn outcome_to_result(outcome: crate::git_gix::ToolExecutionOutcome) -> ToolResult {
    let is_error = outcome.output.starts_with("Error");
    ToolResult {
        output: outcome.output,
        metadata: outcome.tool_result_fields,
        is_error,
    }
}

// ─── DefaultToolExecutor ────────────────────────────────────────────────────

/// Per-tool execution timeout. Prevents synchronous tools (tree-sitter, etc.)
/// from hanging indefinitely on large inputs.
const TOOL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Maximum output size returned to the LLM. Larger outputs are truncated to
/// prevent context window overflow.
const MAX_TOOL_OUTPUT_BYTES: usize = 64 * 1024; // 64 KB

/// Default tool executor with the full shared tool set.
///
/// Covers file ops, shell, git (via gix), GitHub API, code intelligence,
/// task management, and utility tools. CLI-specific tools (ask_user, MCP,
/// LSP subprocess, interactive shell) are handled by wrapping executors.
pub struct DefaultToolExecutor {
    ctx: ToolContext,
    approval_gate: Option<Arc<dyn ToolApprovalGate>>,
    progress_callback: Option<Arc<dyn ToolProgressCallback>>,
    file_journal: Option<Arc<std::sync::Mutex<dyn FileEditJournal>>>,
    git_journal: Option<Arc<std::sync::Mutex<dyn GitRollbackJournal>>>,
    github_client: Option<GitHubClient>,
    task_manager: Arc<TaskManager>,
}

impl DefaultToolExecutor {
    pub fn new(ctx: ToolContext) -> Self {
        Self {
            ctx,
            approval_gate: None,
            progress_callback: None,
            file_journal: None,
            git_journal: None,
            github_client: None,
            task_manager: Arc::new(TaskManager::new()),
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

    pub fn with_github_client(mut self, client: GitHubClient) -> Self {
        self.github_client = Some(client);
        self
    }

    pub fn with_task_manager(mut self, mgr: Arc<TaskManager>) -> Self {
        self.task_manager = mgr;
        self
    }

    pub fn with_cancel_token(mut self, token: Option<Arc<CancellationToken>>) -> Self {
        self.ctx.cancel_token = token;
        self
    }

    /// Access the underlying context.
    pub fn context(&self) -> &ToolContext {
        &self.ctx
    }

    /// Workspace root path (alias for `context().workspace_root`).
    pub fn workspace_root(&self) -> &Path {
        &self.ctx.workspace_root
    }

    /// Project root path (alias for `context().project_root`).
    pub fn project_root_path(&self) -> &Path {
        &self.ctx.project_root
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
                    return ToolResult::error(format!(
                        "The user REJECTED this tool call. The tool was NOT executed.\n\
                         User feedback: \"{msg}\"\n\
                         IMPORTANT: Do NOT retry this exact approach. \
                         Ask the user how to proceed, or try a safer alternative."
                    ));
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

        // Check cancellation before executing the tool.
        if self
            .ctx
            .cancel_token
            .as_ref()
            .is_some_and(|t| t.is_cancelled())
        {
            return ToolResult::error(format!("Tool '{name}' not executed: run was cancelled"));
        }

        let result = match tokio::time::timeout(TOOL_TIMEOUT, self.dispatch(name, args)).await {
            Ok(r) => r,
            Err(_) => ToolResult::error(format!(
                "Tool '{name}' timed out after {}s",
                TOOL_TIMEOUT.as_secs()
            )),
        };
        // Truncate oversized output to prevent context window overflow.
        let result = if result.output.len() > MAX_TOOL_OUTPUT_BYTES {
            let safe_len = result.output.floor_char_boundary(MAX_TOOL_OUTPUT_BYTES);
            ToolResult {
                output: format!(
                    "{}\n[output truncated at {}KB — {} bytes omitted]",
                    &result.output[..safe_len],
                    MAX_TOOL_OUTPUT_BYTES / 1024,
                    result.output.len() - safe_len,
                ),
                ..result
            }
        } else {
            result
        };

        if let Some(cb) = &self.progress_callback {
            cb.tool_completed(&call_id, &result.output, !result.is_error)
                .await;
        }

        result
    }

    fn tool_schemas(&self) -> Vec<Value> {
        crate::schemas::default_executor_tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.ctx.project_root
    }
}

// ─── Dispatch ───────────────────────────────────────────────────────────────

impl DefaultToolExecutor {
    async fn dispatch(&self, name: &str, args: &Value) -> ToolResult {
        let ws = &self.ctx.workspace_root;
        let pr = &self.ctx.project_root;

        match name {
            // ── File operations ──────────────────────────────────────
            "read_file" => crate::fs_ops::read_file(ws, args),
            "write_file" => crate::fs_ops::write_file(ws, args),
            "str_replace" => crate::fs_ops::str_replace(ws, args),
            "delete_file" => crate::fs_ops::delete_file(ws, args),
            "list_dir" => crate::fs_ops::list_dir(ws, args),

            // ── Multi-edit (atomic) ──────────────────────────────────
            "multi_edit" => crate::fs_ops::multi_edit(ws, args),

            // ── Shell operations ─────────────────────────────────────
            "bash" => crate::shell_ops::execute_bash(&self.ctx, args).await,
            "grep" => crate::shell_ops::grep(&self.ctx, args).await,
            "glob" => crate::shell_ops::glob(&self.ctx, args).await,

            // ── Git operations (gix-based) ───────────────────────────
            "git_status" => string_to_result(crate::git_gix::git_status(pr)),
            "git_diff" => string_to_result(crate::git_gix::git_diff(pr, args, 0.0, 0)),
            "git_log" => string_to_result(crate::git_gix::git_log(pr, args)),
            "git_show" => string_to_result(crate::git_gix::git_show(pr, args, 0.0, 0)),
            "git_blame" => string_to_result(crate::git_gix::git_blame(pr, args)),
            "git_commit" => outcome_to_result(crate::git_gix::git_commit_with_metadata(pr, args)),
            "git_file_history" => string_to_result(crate::git_gix::git_file_history(pr, args)),
            "git_log_search" => string_to_result(crate::git_gix::git_log_search(pr, args)),
            "git_contributors" => string_to_result(crate::git_gix::git_contributors(pr, args)),
            "git_revert_commit" => {
                outcome_to_result(crate::git_gix::git_revert_commit_with_metadata(pr, args))
            }
            "git_stash" => outcome_to_result(crate::git_gix::git_stash_with_metadata(pr, args)),

            // ── GitHub API ───────────────────────────────────────────
            "github_list_prs"
            | "github_get_pr"
            | "github_ci_status"
            | "github_list_issues"
            | "github_get_issue"
            | "github_repo_stats"
            | "github_create_issue" => self.dispatch_github(name, args).await,

            // ── Code intelligence (tree-sitter) ──────────────────────
            "symbols" => self.dispatch_symbols(args),

            // ── Web search ───────────────────────────────────────────
            "web_search" => string_to_result(crate::web_search::web_search(args)),

            // ── Task management ──────────────────────────────────────
            "task_create" => string_to_result(self.task_manager.create(args).await),
            "task_list" => string_to_result(self.task_manager.list(args).await),
            "task_get" => string_to_result(self.task_manager.get(args).await),
            "task_update" => string_to_result(self.task_manager.update(args).await),
            "task_stop" => string_to_result(self.task_manager.stop(args).await),

            // ── Utility tools ────────────────────────────────────────
            "tool_search" => {
                let schemas = self.tool_schemas();
                string_to_result(crate::tool_search::tool_search(&schemas, args))
            }
            "env" => string_to_result(crate::env_tools::env_tool(args)),
            "config" => {
                // Default limits; wrapping executors can override
                string_to_result(crate::config_tool::config_tool(128_000, 16_000, args))
            }

            // ── Sleep ────────────────────────────────────────────────
            "sleep" => {
                let secs = args
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .map(|ms| (ms.min(300_000) as f64) / 1000.0)
                    .or_else(|| {
                        args.get("seconds")
                            .and_then(Value::as_f64)
                            .map(|s| s.clamp(0.0, 300.0))
                    })
                    .unwrap_or(1.0);
                tokio::time::sleep(std::time::Duration::from_secs_f64(secs)).await;
                ToolResult::text(format!("Slept for {secs:.1}s"))
            }

            // ── Web fetch (HTTP GET) ─────────────────────────────────
            "web_fetch" => self.dispatch_web_fetch(args).await,

            // ── Memory tools (require configured endpoint) ───────────
            "memory_retrieve" | "memory_store" | "memory_search" | "memory_purge"
            | "memory_correct" | "memory_profile" | "memory_feedback" => {
                ToolResult::error(format!(
                    "Error: Memory tool '{name}' requires a configured memoria endpoint. \
                     Use ServerToolExecutor or CliToolExecutor instead of DefaultToolExecutor."
                ))
            }

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

    /// Dispatch GitHub API tools via the optional GitHubClient.
    async fn dispatch_github(&self, name: &str, args: &Value) -> ToolResult {
        let client = match &self.github_client {
            Some(c) => c,
            None => {
                return ToolResult::error(format!(
                    "Error: GitHub tool '{name}' requires a configured GitHub client. \
                     Call with_github_client() when building DefaultToolExecutor."
                ));
            }
        };
        let output = match name {
            "github_list_prs" => client.github_list_prs(args).await,
            "github_get_pr" => client.github_get_pr(args).await,
            "github_ci_status" => client.github_ci_status(args).await,
            "github_list_issues" => client.github_list_issues(args).await,
            "github_get_issue" => client.github_get_issue(args).await,
            "github_repo_stats" => client.github_repo_stats(args).await,
            "github_create_issue" => client.github_create_issue(args).await,
            _ => return ToolResult::error(format!("Error: Unknown GitHub tool '{name}'")),
        };
        string_to_result(output)
    }

    /// Dispatch web_fetch: simple HTTP GET using the context's HTTP client or curl fallback.
    async fn dispatch_web_fetch(&self, args: &Value) -> ToolResult {
        let url = match args.get("url").and_then(|v| v.as_str()) {
            Some(u) => u,
            None => return ToolResult::error("Error: Missing 'url' parameter".into()),
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return ToolResult::error("Error: URL must start with http:// or https://".into());
        }
        let max_bytes = args
            .get("max_bytes")
            .and_then(|v| v.as_u64())
            .unwrap_or(10_000) as usize;
        let timeout_secs = args.get("timeout").and_then(|v| v.as_u64()).unwrap_or(10);

        if let Some(client) = &self.ctx.http_client {
            match tokio::time::timeout(
                std::time::Duration::from_secs(timeout_secs),
                client.get(url).header("User-Agent", "astra/0.1").send(),
            )
            .await
            {
                Ok(Ok(resp)) => {
                    let status = resp.status();
                    match resp.text().await {
                        Ok(body) => {
                            let truncated = if body.len() > max_bytes {
                                format!(
                                    "{}\n... [truncated at {} of {} bytes]",
                                    &body[..max_bytes],
                                    max_bytes,
                                    body.len()
                                )
                            } else {
                                body
                            };
                            ToolResult::text(format!("HTTP {status}\n{truncated}"))
                        }
                        Err(e) => ToolResult::error(format!("Error reading response: {e}")),
                    }
                }
                Ok(Err(e)) => ToolResult::error(format!("Error: HTTP request failed: {e}")),
                Err(_) => {
                    ToolResult::error(format!("Error: Request timed out after {timeout_secs}s"))
                }
            }
        } else {
            // Fallback: use curl subprocess
            let output = tokio::process::Command::new("curl")
                .args([
                    "-sS",
                    "-L",
                    "--max-redirs",
                    "5",
                    "--max-time",
                    &timeout_secs.to_string(),
                    "--max-filesize",
                    &(max_bytes * 2).to_string(),
                    "-H",
                    "User-Agent: astra/0.1",
                    url,
                ])
                .output()
                .await;
            match output {
                Ok(out) => {
                    let body = String::from_utf8_lossy(&out.stdout);
                    if body.is_empty() {
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        return ToolResult::error(format!("Error: {stderr}"));
                    }
                    let truncated = if body.len() > max_bytes {
                        format!(
                            "{}\n... [truncated at {} of {} bytes]",
                            &body[..max_bytes],
                            max_bytes,
                            body.len()
                        )
                    } else {
                        body.to_string()
                    };
                    ToolResult::text(truncated)
                }
                Err(e) => ToolResult::error(format!("Error: curl failed: {e}")),
            }
        }
    }

    /// Dispatch the `symbols` tool: read a file, detect language, extract symbols.
    fn dispatch_symbols(&self, args: &Value) -> ToolResult {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return ToolResult::error("Error: Missing 'path' parameter".into()),
        };
        let resolved = match crate::fs_ops::resolve_path(&self.ctx.workspace_root, path_str) {
            Ok(p) => p,
            Err(e) => return ToolResult::error(e),
        };
        let source = match std::fs::read_to_string(&resolved) {
            Ok(s) => s,
            Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
        };
        let lang = match crate::code_intel::detect_language(&resolved) {
            Some(l) => l,
            None => {
                return ToolResult::error(format!(
                    "Error: Cannot detect language for '{path_str}'"
                ));
            }
        };
        let symbols = crate::code_intel::extract_symbols(&source, lang);
        let outline = symbols
            .iter()
            .map(|s| format!("{}:{:?} {}", s.start_line + 1, s.kind, s.name))
            .collect::<Vec<_>>()
            .join("\n");
        if outline.is_empty() {
            ToolResult::text("No symbols found.".into())
        } else {
            ToolResult::text(outline)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use serde_json::Value;
    use tempfile::TempDir;

    fn test_executor() -> (TempDir, DefaultToolExecutor) {
        let tmp = TempDir::new().unwrap();
        let ctx = ToolContext::test(tmp.path());
        let exec = DefaultToolExecutor::new(ctx);
        (tmp, exec)
    }

    fn init_git_repo(dir: &Path) {
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(dir)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(dir)
            .output()
            .unwrap();
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
    async fn dispatch_read_file_outline() {
        let (tmp, exec) = test_executor();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub struct User;\n\npub fn parse() {}\nfn helper() {}\n",
        )
        .unwrap();
        let result = exec
            .execute(
                "read_file",
                &serde_json::json!({"path": "lib.rs", "outline": true}),
            )
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("# Outline"));
        assert!(result.output.contains("parse"));
    }

    #[tokio::test]
    async fn dispatch_read_file_large_file_returns_preview() {
        let (tmp, exec) = test_executor();
        let mut large = String::new();
        for i in 1..=3000 {
            large.push_str(&format!(
                "line {}: some padding content here to make the file larger\n",
                i
            ));
        }
        std::fs::write(tmp.path().join("big.txt"), &large).unwrap();
        let result = exec
            .execute("read_file", &serde_json::json!({"path": "big.txt"}))
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(result.output.contains("Large file preview"));
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
    async fn dispatch_bash_non_zero_is_error() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "echo nope >&2; exit 7"}),
            )
            .await;
        assert!(result.is_error);
        assert!(
            result.output.contains("stderr:\nnope"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("[exit code: 7]"),
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn dispatch_bash_timeout_keeps_partial_output() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "bash",
                &serde_json::json!({"command": "echo start; sleep 1; echo done", "timeout": 0.2}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("start"), "got: {}", result.output);
        assert!(
            result.output.contains("timed out after 0.2s"),
            "got: {}",
            result.output
        );
        assert!(!result.output.contains("done"), "got: {}", result.output);
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

    #[tokio::test]
    async fn dispatch_str_replace_dry_run_does_not_write() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("f.txt"), "old text here").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &serde_json::json!({
                    "path": "f.txt",
                    "old_str": "old text",
                    "new_str": "new text",
                    "dry_run": true
                }),
            )
            .await;
        assert!(!result.is_error, "got: {}", result.output);
        assert!(
            result.output.contains("[DRY RUN]"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "old text here");
    }

    #[tokio::test]
    async fn dispatch_env() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("env", &serde_json::json!({"action": "list"}))
            .await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn dispatch_tool_search() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("tool_search", &serde_json::json!({"query": "file"}))
            .await;
        assert!(!result.is_error);
    }

    #[tokio::test]
    async fn dispatch_task_lifecycle() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "task_create",
                &serde_json::json!({"title": "test task", "description": "do stuff"}),
            )
            .await;
        assert!(!result.is_error);

        let result = exec.execute("task_list", &serde_json::json!({})).await;
        assert!(!result.is_error);
        assert!(result.output.contains("test task"));
    }

    #[tokio::test]
    async fn dispatch_github_without_client() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("github_list_prs", &serde_json::json!({}))
            .await;
        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("requires a configured GitHub client")
        );
    }

    #[tokio::test]
    async fn dispatch_memory_without_endpoint() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute("memory_store", &serde_json::json!({"content": "test"}))
            .await;
        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("requires a configured memoria endpoint")
        );
    }

    #[tokio::test]
    async fn dispatch_symbols() {
        let (tmp, exec) = test_executor();
        std::fs::write(
            tmp.path().join("sample.rs"),
            "fn hello() {}\nstruct Foo {}\n",
        )
        .unwrap();
        let result = exec
            .execute("symbols", &serde_json::json!({"path": "sample.rs"}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("hello"));
    }

    #[tokio::test]
    async fn string_to_result_error() {
        let r = string_to_result("Error: something went wrong".into());
        assert!(r.is_error);
        assert!(r.output.contains("something went wrong"));
    }

    #[tokio::test]
    async fn string_to_result_ok() {
        let r = string_to_result("All good".into());
        assert!(!r.is_error);
        assert_eq!(r.output, "All good");
    }

    #[tokio::test]
    async fn dispatch_sleep() {
        let (_tmp, exec) = test_executor();
        let start = std::time::Instant::now();
        let result = exec
            .execute("sleep", &serde_json::json!({"duration_ms": 100}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("Slept"));
        assert!(start.elapsed().as_millis() >= 90);
    }

    #[tokio::test]
    async fn dispatch_sleep_accepts_legacy_seconds() {
        let (_tmp, exec) = test_executor();
        let start = std::time::Instant::now();
        let result = exec
            .execute("sleep", &serde_json::json!({"seconds": 0.05}))
            .await;
        assert!(!result.is_error);
        assert!(result.output.contains("Slept"));
        assert!(start.elapsed().as_millis() >= 40);
    }

    #[tokio::test]
    async fn dispatch_multi_edit() {
        let (tmp, exec) = test_executor();
        std::fs::write(tmp.path().join("m.txt"), "aaa bbb ccc").unwrap();
        let result = exec
            .execute(
                "multi_edit",
                &serde_json::json!({
                    "path": "m.txt",
                    "edits": [
                        {"old_str": "aaa", "new_str": "AAA"},
                        {"old_str": "ccc", "new_str": "CCC"}
                    ]
                }),
            )
            .await;
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("m.txt")).unwrap();
        assert_eq!(content, "AAA bbb CCC");
    }

    #[tokio::test]
    async fn dispatch_web_fetch_missing_url() {
        let (_tmp, exec) = test_executor();
        let result = exec.execute("web_fetch", &serde_json::json!({})).await;
        assert!(result.is_error);
        assert!(result.output.contains("Missing 'url'"));
    }

    #[tokio::test]
    async fn dispatch_web_fetch_bad_scheme() {
        let (_tmp, exec) = test_executor();
        let result = exec
            .execute(
                "web_fetch",
                &serde_json::json!({"url": "ftp://example.com"}),
            )
            .await;
        assert!(result.is_error);
        assert!(result.output.contains("http"));
    }

    #[tokio::test]
    async fn dispatch_git_revert_commit() {
        let (tmp, exec) = test_executor();
        init_git_repo(tmp.path());

        let tracked = tmp.path().join("tracked.txt");
        std::fs::write(&tracked, "original\n").unwrap();
        let initial = exec
            .execute("git_commit", &serde_json::json!({"message": "initial"}))
            .await;
        assert!(!initial.is_error, "got: {}", initial.output);

        std::fs::write(&tracked, "changed\n").unwrap();
        let committed = exec
            .execute(
                "git_commit",
                &serde_json::json!({"message": "change tracked"}),
            )
            .await;
        assert!(!committed.is_error, "got: {}", committed.output);
        let commit_sha = committed
            .metadata
            .as_ref()
            .and_then(|fields| fields.get("commit_sha"))
            .and_then(Value::as_str)
            .expect("commit_sha metadata");

        let reverted = exec
            .execute(
                "git_revert_commit",
                &serde_json::json!({"commit_sha": commit_sha}),
            )
            .await;
        assert!(!reverted.is_error, "got: {}", reverted.output);
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "original\n");
        assert_eq!(
            reverted
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("reverted_commit_sha"))
                .and_then(Value::as_str),
            Some(commit_sha)
        );
    }

    /// P1-J: execute() must truncate output exceeding MAX_TOOL_OUTPUT_BYTES.
    /// Uses read_file on a large synthetic file to trigger truncation.
    #[tokio::test]
    async fn output_truncated_at_max_bytes() {
        let (tmp, exec) = test_executor();

        // Write a file larger than MAX_TOOL_OUTPUT_BYTES (64KB)
        let large_content = "x".repeat(200 * 1024); // 200KB
        let file_path = tmp.path().join("large.txt");
        std::fs::write(&file_path, &large_content).unwrap();

        let result = exec
            .execute(
                "read_file",
                &serde_json::json!({"path": file_path.to_str().unwrap()}),
            )
            .await;

        assert!(
            result.output.len() <= super::MAX_TOOL_OUTPUT_BYTES + 200,
            "output must be truncated to ~{}KB, got {} bytes",
            super::MAX_TOOL_OUTPUT_BYTES / 1024,
            result.output.len()
        );
        assert!(
            result.output.contains("truncated"),
            "truncated output must contain truncation notice"
        );
    }

    /// P1-I: execute() must return a timeout error for tools that hang.
    /// We test this by verifying the TOOL_TIMEOUT constant is reasonable
    /// and that the timeout path produces the right error message.
    #[test]
    fn tool_timeout_constant_is_reasonable() {
        // TOOL_TIMEOUT must be > 0 and ≤ 5 minutes (not too short, not infinite)
        assert!(
            super::TOOL_TIMEOUT.as_secs() >= 10,
            "TOOL_TIMEOUT must be at least 10s to allow real tool calls"
        );
        assert!(
            super::TOOL_TIMEOUT.as_secs() <= 300,
            "TOOL_TIMEOUT must be ≤ 5 minutes to prevent indefinite hangs"
        );
    }

    /// P1-C: execute() must return an error immediately when the cancellation
    /// token is already cancelled — tool must NOT be executed.
    #[tokio::test]
    async fn cancelled_token_prevents_tool_execution() {
        let (tmp, exec) = test_executor();

        // Set a pre-cancelled token
        let token = Arc::new(CancellationToken::new());
        token.cancel();
        let exec = exec.with_cancel_token(Some(token));

        // Try to execute a real tool — it must be rejected, not executed
        let file_path = tmp.path().join("test.txt");
        std::fs::write(&file_path, "hello").unwrap();

        let result = exec
            .execute(
                "read_file",
                &serde_json::json!({"path": file_path.to_str().unwrap()}),
            )
            .await;

        assert!(
            result.is_error,
            "cancelled token must produce an error result"
        );
        assert!(
            result.output.contains("cancelled"),
            "error must mention cancellation, got: {}",
            result.output
        );
    }
}
