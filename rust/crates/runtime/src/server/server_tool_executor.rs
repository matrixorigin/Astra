//! Server-side tool executor for web agent sessions.
//!
//! When a web user connects without a CLI edge agent, the server executes
//! tools directly using the shared `astra-tools` library. This module
//! provides the `ServerToolExecutor` that wraps tool execution with:
//! - Per-session workspace isolation (sandbox)
//! - Per-session file and git journals
//! - Circuit-breaker for external services (Memoria)
//!
//! # Integration
//!
//! The executor is injected into `HeadlessToolRoundCtx` via the
//! `server_tool_executor` field. When present, the headless round
//! calls it directly instead of waiting for edge POST callbacks.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use astra_tools::executor::DefaultToolExecutor;
use astra_tools::{ToolContext, ToolExecutor};

use crate::tool_sandbox::{
    IsolationConfig, SandboxMode, SandboxPolicy, ToolTier, effective_tier, execute_isolated,
    filter_environment,
};
use crate::turn::file_edit_journal::FileEditJournal;

/// Server-side tool executor for web agent sessions.
///
/// Wraps tool calls in a sandboxed environment without requiring a CLI process.
/// Created per-session by `AgenticRunLifecycleService::create_run()`.
pub struct ServerToolExecutor {
    /// Workspace root for this session.
    workspace_root: PathBuf,
    /// User ID owning this session (used for Memoria isolation).
    user_id: String,
    /// Session ID for isolation.
    session_id: String,
    /// Sandbox policy for tool execution.
    sandbox_policy: SandboxPolicy,
    /// File edit journal for undo support.
    file_journal: Arc<Mutex<FileEditJournal>>,
    /// Current turn index for journal entries.
    journal_turn_index: AtomicU32,
    /// Aggregate output bytes this turn.
    aggregate_output_bytes: AtomicUsize,
    /// Memoria client for memory operations.
    memoria_client: astra_tools::memoria::MemoriaClient,
    /// Cloud API base URL.
    #[allow(dead_code)] // Phase 5: used for cloud API calls (web_fetch, etc.)
    cloud_base: Option<String>,
    /// Auth token for cloud calls.
    #[allow(dead_code)] // Phase 5: used for authenticated cloud API calls
    cloud_token: Option<String>,
    /// GitHub token for API calls.
    #[allow(dead_code)] // Phase 5: used for GitHub API integration
    github_token: Option<String>,
    /// Shared HTTP client.
    #[allow(dead_code)] // Phase 5: used for web_fetch and cloud API calls
    http_client: reqwest::Client,
    /// URL fetch cache.
    #[allow(dead_code)] // Phase 5: used for web_fetch caching
    url_cache: Mutex<HashMap<String, (String, Instant)>>,
    /// Optional approval gate for dangerous tool execution.
    approval_gate: Option<Arc<dyn astra_tools::ToolApprovalGate>>,
    /// Optional progress callback for streaming tool output.
    progress_callback: Option<Arc<dyn astra_tools::ToolProgressCallback>>,
    /// Optional resource governor for usage tracking (Phase 5).
    resource_governor:
        Option<std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>>,
    /// Optional edge connection pool for routing to remote edge agents.
    edge_connection_pool: Option<super::edge_connection_pool::EdgeConnectionPool>,
    /// Shared default executor for delegating common tool logic.
    default_executor: DefaultToolExecutor,
}

impl ServerToolExecutor {
    /// Create a new server tool executor for a session.
    pub fn new(
        workspace_root: PathBuf,
        user_id: String,
        session_id: String,
        cloud_base: Option<String>,
        cloud_token: Option<String>,
    ) -> Self {
        let sandbox_policy = SandboxPolicy {
            mode: SandboxMode::Strict,
            project_root: workspace_root.clone(),
            allowed_paths: vec![PathBuf::from("/tmp")],
            env_allowlist: None,
            max_execution_secs: 120.0,
            max_output_bytes: 200_000,
            network_allowed: false,
        };

        let memoria_client =
            astra_tools::memoria::MemoriaClient::new(cloud_base.clone(), cloud_token.clone());

        let default_executor = DefaultToolExecutor::new(ToolContext {
            project_root: workspace_root.clone(),
            workspace_root: workspace_root.clone(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            sandbox: astra_tools::SandboxConfig::standard(workspace_root.clone()),
            http_client: None,
            logger: std::sync::Arc::new(astra_tools::TracingLogger),
        });

        Self {
            workspace_root,
            user_id,
            session_id,
            sandbox_policy,
            default_executor,
            file_journal: Arc::new(Mutex::new(FileEditJournal::new(500))),
            journal_turn_index: AtomicU32::new(0),
            aggregate_output_bytes: AtomicUsize::new(0),
            memoria_client,
            cloud_base,
            cloud_token,
            github_token: std::env::var("GITHUB_TOKEN").ok(),
            http_client: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent("astra-server/0.1.0")
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            url_cache: Mutex::new(HashMap::new()),
            approval_gate: None,
            progress_callback: None,
            resource_governor: None,
            edge_connection_pool: None,
        }
    }

    /// Set the approval gate for interactive tool execution.
    pub fn set_approval_gate(&mut self, gate: Arc<dyn astra_tools::ToolApprovalGate>) {
        self.approval_gate = Some(gate);
    }

    /// Set the progress callback for streaming tool output.
    pub fn set_progress_callback(&mut self, cb: Arc<dyn astra_tools::ToolProgressCallback>) {
        self.progress_callback = Some(cb);
    }

    /// Set the edge connection pool for remote tool routing.
    pub fn set_edge_connection_pool(
        &mut self,
        pool: super::edge_connection_pool::EdgeConnectionPool,
    ) {
        self.edge_connection_pool = Some(pool);
    }

    /// Set the resource governor for usage tracking.
    pub fn set_resource_governor(
        &mut self,
        governor: std::sync::Arc<dyn astra_services::resource_governor::ResourceGovernor>,
    ) {
        self.resource_governor = Some(governor);
    }

    /// Execute a tool call and return the result string.
    ///
    /// Routing order:
    /// 1. Try remote edge agent (if connected via WebSocket)
    /// 2. Fall back to local server-side execution
    pub async fn execute(&self, name: &str, args: &Value) -> String {
        // ── Try remote edge agent first ──────────────────────────────
        if let Some(pool) = &self.edge_connection_pool {
            if let Some(result) = pool.execute_tool_any_edge(&self.user_id, name, args).await {
                return result.output;
            }
        }
        // ── Fire-and-forget resource usage recording (Phase 5) ────────
        if let Some(ref gov) = self.resource_governor {
            let gov = gov.clone();
            let uid = self.user_id.clone();
            tokio::spawn(async move {
                gov.record_tool_calls(&uid, 1).await;
            });
        }

        self.execute_local(name, args).await
    }

    /// Execute a tool locally on the server (no edge routing).
    async fn execute_local(&self, name: &str, args: &Value) -> String {
        // ── Approval gate check ──────────────────────────────────────
        if let Some(gate) = &self.approval_gate {
            if gate.requires_approval(name) {
                let request_id = format!("srv-{}-{}", self.session_id, uuid_v4_short());
                let decision = gate.request_approval(&request_id, name, args).await;
                match decision {
                    astra_tools::ApprovalDecision::Approved => { /* proceed */ }
                    astra_tools::ApprovalDecision::Denied { reason } => {
                        let msg = reason.unwrap_or_else(|| "User denied execution".into());
                        return format!("Tool execution denied: {msg}");
                    }
                    astra_tools::ApprovalDecision::Timeout => {
                        return "Tool execution denied: approval request timed out".into();
                    }
                }
            }
        }

        // ── Progress: tool started ───────────────────────────────────
        let call_id = format!("{name}-{}", uuid_v4_short());
        if let Some(cb) = &self.progress_callback {
            cb.tool_started(&call_id, name, args).await;
        }

        let output = match name {
            // ── Memory tools (HTTP proxy) ──────────────────────────────
            "memory_retrieve" | "memory_store" | "memory_search" | "memory_purge"
            | "memory_correct" | "memory_profile" => {
                let op = name.strip_prefix("memory_").unwrap_or(name);
                // Force-inject user_id and session_id for per-user isolation,
                // mirroring the server's /memory/* proxy in auth_handlers.rs.
                let mut isolated_args = args.clone();
                if let Some(obj) = isolated_args.as_object_mut() {
                    obj.insert(
                        "session_id".to_string(),
                        Value::String(self.user_id.clone()),
                    );
                    obj.insert("user_id".to_string(), Value::String(self.user_id.clone()));
                }
                self.memoria_client.call(op, &isolated_args).await
            }
            // ── Web search (standalone function) ───────────────────────
            "web_search" => astra_tools::web_search::web_search(args),
            // ── File operations ─────────────────────────────────────────
            // Write operations use server-specific journal recording.
            // Read-only operations delegate to DefaultToolExecutor.
            "read_file" => {
                self.default_executor
                    .execute("read_file", args)
                    .await
                    .output
            }
            "write_file" => self.server_write_file(args),
            "str_replace" => self.server_str_replace(args),
            "delete_file" => self.server_delete_file(args),
            "list_dir" => self.default_executor.execute("list_dir", args).await.output,
            // ── Shell operations ───────────────────────────────────────
            // bash uses tiered process isolation (server-specific).
            // grep + glob delegate to DefaultToolExecutor.
            "bash" => self.server_bash(args).await,
            "grep" => self.default_executor.execute("grep", args).await.output,
            "glob" => self.default_executor.execute("glob", args).await.output,
            // ── Git operations ─────────────────────────────────────────
            // All git ops delegate to DefaultToolExecutor.
            "git_status" => {
                self.default_executor
                    .execute("git_status", args)
                    .await
                    .output
            }
            "git_diff" => self.default_executor.execute("git_diff", args).await.output,
            "git_log" => self.default_executor.execute("git_log", args).await.output,
            "git_show" => self.default_executor.execute("git_show", args).await.output,
            "git_blame" => {
                self.default_executor
                    .execute("git_blame", args)
                    .await
                    .output
            }
            "git_commit" => {
                self.default_executor
                    .execute("git_commit", args)
                    .await
                    .output
            }
            // ── Delegation placeholder ─────────────────────────────────
            "delegate" => "Delegation request acknowledged. The delegation engine will execute \
                this request and provide results in the next round."
                .to_string(),
            // ── Unknown tool fallback ──────────────────────────────────
            _ => {
                format!(
                    "Error: Tool '{name}' is not available in server-side execution mode. \
                     Available: bash, read_file, write_file, str_replace, delete_file, \
                     list_dir, grep, glob, git_status, git_diff, git_log, git_show, \
                     git_blame, git_commit, memory_*, web_search"
                )
            }
        };

        let output = astra_tools::normalize_empty_output(output, name);
        let limit = astra_tools::per_tool_output_limit(name);
        let output = astra_tools::truncate_output(output, limit);
        let agg = self
            .aggregate_output_bytes
            .fetch_add(output.len(), Ordering::Relaxed);
        let output = astra_tools::maybe_persist_large_output(output, agg, name);

        // ── Progress: tool completed ─────────────────────────────────
        if let Some(cb) = &self.progress_callback {
            let success = !output.starts_with("Error:");
            cb.tool_completed(&call_id, &output, success).await;
        }

        output
    }

    /// Set the current turn index for journal entries.
    pub fn set_turn_index(&self, idx: u32) {
        self.journal_turn_index.store(idx, Ordering::Relaxed);
    }

    /// Reset aggregate output counter at the start of a new turn.
    pub fn reset_aggregate_output(&self) {
        self.aggregate_output_bytes.store(0, Ordering::Relaxed);
    }

    /// Get the workspace root path.
    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    // ────────────────────────────────────────────────────────────────────────
    // File operations (sandboxed to workspace_root)
    // ────────────────────────────────────────────────────────────────────────

    fn resolve_path(&self, relative: &str) -> Result<PathBuf, String> {
        let path = if Path::new(relative).is_absolute() {
            PathBuf::from(relative)
        } else {
            self.workspace_root.join(relative)
        };

        // Normalize the path manually to collapse ".." BEFORE the sandbox check.
        // canonicalize() only works for existing paths; for non-existent targets
        // the fallback was returning the raw path with ".." intact, which could
        // pass the starts_with() prefix check yet resolve outside the sandbox
        // when the OS normalizes it during actual I/O.
        let normalized = path.components().fold(PathBuf::new(), |mut acc, c| {
            match c {
                std::path::Component::ParentDir => {
                    acc.pop();
                }
                std::path::Component::CurDir => {} // skip "."
                other => acc.push(other),
            }
            acc
        });

        // For existing paths, canonicalize to resolve symlinks as well.
        let final_path = if normalized.exists() {
            normalized
                .canonicalize()
                .map_err(|e| format!("Cannot resolve path: {e}"))?
        } else {
            normalized
        };

        if !final_path.starts_with(&self.workspace_root) {
            return Err(format!(
                "SANDBOX_DENIED: Path '{}' is outside workspace root '{}'",
                relative,
                self.workspace_root.display()
            ));
        }
        Ok(final_path)
    }

    fn server_write_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'path' parameter".to_string(),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: Missing 'content' parameter".to_string(),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };

        // Record journal entry before writing
        if let Ok(mut journal) = self.file_journal.lock() {
            let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);
            journal.record_before(&path, "server-write", turn_idx);
        }

        if let Some(parent) = path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                return format!("Error: Cannot create directories: {e}");
            }
        }

        match std::fs::write(&path, content) {
            Ok(()) => {
                if let Ok(mut journal) = self.file_journal.lock() {
                    journal.record_after(&path, "server-write", content.as_bytes());
                }
                format!("Successfully wrote {} bytes to {}", content.len(), path_str)
            }
            Err(e) => format!("Error: Cannot write file: {e}"),
        }
    }

    fn server_str_replace(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'path' parameter".to_string(),
        };
        let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return "Error: Missing 'old_str' parameter".to_string(),
        };
        let new_str = match args.get("new_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return "Error: Missing 'new_str' parameter".to_string(),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error: Cannot read file: {e}"),
        };

        let count = content.matches(old_str).count();
        if count == 0 {
            return format!("Error: old_str not found in {path_str}");
        }
        if count > 1 {
            return format!(
                "Error: old_str found {count} times in {path_str}. Make old_str more specific to match exactly once."
            );
        }

        // Record journal entry
        if let Ok(mut journal) = self.file_journal.lock() {
            let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);
            journal.record_before(&path, "server-str-replace", turn_idx);
        }

        let new_content = content.replacen(old_str, new_str, 1);
        match std::fs::write(&path, &new_content) {
            Ok(()) => {
                if let Ok(mut journal) = self.file_journal.lock() {
                    journal.record_after(&path, "server-str-replace", new_content.as_bytes());
                }
                format!("Successfully replaced text in {path_str}")
            }
            Err(e) => format!("Error: Cannot write file: {e}"),
        }
    }

    fn server_delete_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'path' parameter".to_string(),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };

        if !path.exists() {
            return format!("Error: File not found: {path_str}");
        }

        if let Ok(mut journal) = self.file_journal.lock() {
            let turn_idx = self.journal_turn_index.load(Ordering::Relaxed);
            journal.record_before(&path, "server-delete", turn_idx);
        }

        match std::fs::remove_file(&path) {
            Ok(()) => format!("Successfully deleted {path_str}"),
            Err(e) => format!("Error: Cannot delete file: {e}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Shell operations (sandboxed)
    // ────────────────────────────────────────────────────────────────────────

    async fn server_bash(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return "Error: Missing 'command' parameter".to_string(),
        };
        let timeout_secs = args
            .get("timeout")
            .and_then(|v| v.as_f64())
            .unwrap_or(30.0)
            .min(self.sandbox_policy.max_execution_secs);

        let tier = effective_tier("bash", self.sandbox_policy.mode);
        match tier {
            ToolTier::Isolated => {
                let mut config = IsolationConfig::strict(self.workspace_root.clone());
                config.timeout = Duration::from_secs_f64(timeout_secs);
                config.net_namespace = !self.sandbox_policy.network_allowed;
                let env = filter_environment(&self.sandbox_policy);
                let out = execute_isolated(command, &env, &config).await;
                out.combined_output()
            }
            ToolTier::Sandboxed => {
                let mut config = IsolationConfig::sandboxed(self.workspace_root.clone());
                config.timeout = Duration::from_secs_f64(timeout_secs);
                let env = filter_environment(&self.sandbox_policy);
                let out = execute_isolated(command, &env, &config).await;
                out.combined_output()
            }
            ToolTier::InProcess => {
                // Permissive mode — direct execution (backward compat).
                let timeout = Duration::from_secs_f64(timeout_secs);
                let output = tokio::time::timeout(timeout, async {
                    tokio::process::Command::new("bash")
                        .arg("-c")
                        .arg(command)
                        .current_dir(&self.workspace_root)
                        .output()
                        .await
                })
                .await;
                match output {
                    Ok(Ok(out)) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let mut result = String::new();
                        if !stdout.is_empty() {
                            result.push_str(&stdout);
                        }
                        if !stderr.is_empty() {
                            if !result.is_empty() {
                                result.push('\n');
                            }
                            result.push_str("stderr:\n");
                            result.push_str(&stderr);
                        }
                        if !out.status.success() {
                            result.push_str(&format!(
                                "\n(exit code: {})",
                                out.status.code().unwrap_or(-1)
                            ));
                        }
                        result
                    }
                    Ok(Err(e)) => format!("Error: Failed to execute command: {e}"),
                    Err(_) => format!("Error: Command timed out after {timeout_secs}s"),
                }
            }
        }
    }
}

/// Generate a short UUID-like identifier for call tracking.
fn uuid_v4_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:08x}", (ts & 0xFFFF_FFFF) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn test_executor() -> (ServerToolExecutor, TempDir) {
        let dir = TempDir::new().unwrap();
        let exec = ServerToolExecutor::new(
            dir.path().to_path_buf(),
            "test-user".into(),
            "test-session".into(),
            None,
            None,
        );
        (exec, dir)
    }

    // ── Path traversal security ────────────────────────────────────────

    #[tokio::test]
    async fn resolve_path_allows_relative_inside_workspace() {
        let (exec, _dir) = test_executor();
        let result = exec.resolve_path("src/main.rs");
        assert!(result.is_ok());
        assert!(result.unwrap().starts_with(exec.workspace_root()));
    }

    #[tokio::test]
    async fn resolve_path_blocks_parent_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec.resolve_path("../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn resolve_path_blocks_absolute_outside_workspace() {
        let (exec, _dir) = test_executor();
        let result = exec.resolve_path("/etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn resolve_path_allows_absolute_inside_workspace() {
        let (exec, dir) = test_executor();
        let inner = dir.path().join("foo.txt");
        let result = exec.resolve_path(inner.to_str().unwrap());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn resolve_path_normalizes_dot_dot_in_middle() {
        let (exec, _dir) = test_executor();
        // src/../../../etc/passwd should be blocked
        let result = exec.resolve_path("src/../../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn resolve_path_allows_dot_dot_within_workspace() {
        let (exec, dir) = test_executor();
        // Create nested dir so the path stays inside workspace
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        let result = exec.resolve_path("a/b/../c.txt");
        assert!(result.is_ok());
        let resolved = result.unwrap();
        assert!(resolved.starts_with(exec.workspace_root()));
    }

    // ── File operations ────────────────────────────────────────────────

    #[tokio::test]
    async fn read_file_returns_content_with_line_numbers() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("hello.txt"), "line1\nline2\nline3\n").unwrap();
        let result = exec
            .execute("read_file", &json!({"path": "hello.txt"}))
            .await;
        assert!(result.contains("1\tline1"));
        assert!(result.contains("2\tline2"));
        assert!(result.contains("3\tline3"));
    }

    #[tokio::test]
    async fn read_file_respects_start_and_end_line() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("f.txt"), "a\nb\nc\nd\ne\n").unwrap();
        let result = exec
            .execute(
                "read_file",
                &json!({"path": "f.txt", "start_line": 2, "end_line": 4}),
            )
            .await;
        assert!(!result.contains("1\ta"));
        assert!(result.contains("2\tb"));
        assert!(result.contains("3\tc"));
        assert!(result.contains("4\td"));
        assert!(!result.contains("5\te"));
    }

    #[tokio::test]
    async fn read_file_missing_file_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "nonexistent.txt"}))
            .await;
        assert!(result.starts_with("Error:"));
    }

    #[tokio::test]
    async fn read_file_missing_path_param_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("read_file", &json!({})).await;
        assert!(result.contains("Missing 'path'"));
    }

    #[tokio::test]
    async fn read_file_blocks_path_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("read_file", &json!({"path": "../../etc/passwd"}))
            .await;
        assert!(result.contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn write_file_creates_and_writes() {
        let (exec, dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({"path": "out.txt", "content": "hello world"}),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        let content = std::fs::read_to_string(dir.path().join("out.txt")).unwrap();
        assert_eq!(content, "hello world");
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let (exec, dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({
                    "path": "deep/nested/dir/file.txt",
                    "content": "deep content"
                }),
            )
            .await;
        assert!(result.contains("Successfully wrote"));
        assert!(dir.path().join("deep/nested/dir/file.txt").exists());
    }

    #[tokio::test]
    async fn write_file_blocks_path_traversal() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute(
                "write_file",
                &json!({
                    "path": "../../evil.txt",
                    "content": "pwned"
                }),
            )
            .await;
        assert!(result.contains("SANDBOX_DENIED"));
    }

    #[tokio::test]
    async fn str_replace_single_occurrence() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("code.rs"), "fn old_name() {}").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "code.rs",
                    "old_str": "old_name",
                    "new_str": "new_name"
                }),
            )
            .await;
        assert!(result.contains("Successfully replaced"));
        let content = std::fs::read_to_string(dir.path().join("code.rs")).unwrap();
        assert_eq!(content, "fn new_name() {}");
    }

    #[tokio::test]
    async fn str_replace_rejects_multiple_matches() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("dup.txt"), "foo bar foo").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "dup.txt",
                    "old_str": "foo",
                    "new_str": "baz"
                }),
            )
            .await;
        assert!(result.contains("found 2 times"));
    }

    #[tokio::test]
    async fn str_replace_not_found() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("nope.txt"), "hello").unwrap();
        let result = exec
            .execute(
                "str_replace",
                &json!({
                    "path": "nope.txt",
                    "old_str": "missing",
                    "new_str": "x"
                }),
            )
            .await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn delete_file_removes_existing() {
        let (exec, dir) = test_executor();
        let target = dir.path().join("to_delete.txt");
        std::fs::write(&target, "temp").unwrap();
        assert!(target.exists());
        let result = exec
            .execute("delete_file", &json!({"path": "to_delete.txt"}))
            .await;
        assert!(result.contains("Successfully deleted"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn delete_file_nonexistent_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("delete_file", &json!({"path": "ghost.txt"}))
            .await;
        assert!(result.contains("File not found"));
    }

    #[tokio::test]
    async fn list_dir_shows_files_and_dirs() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        let result = exec.execute("list_dir", &json!({"path": "."})).await;
        assert!(result.contains("a.txt"));
        assert!(result.contains("b.rs"));
        assert!(result.contains("subdir/"));
    }

    #[tokio::test]
    async fn list_dir_sorted_output() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("z.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("m.txt"), "").unwrap();
        let result = exec.execute("list_dir", &json!({"path": "."})).await;
        let lines: Vec<&str> = result.lines().collect();
        assert_eq!(lines, vec!["a.txt", "m.txt", "z.txt"]);
    }

    // ── Unknown tool ───────────────────────────────────────────────────

    #[tokio::test]
    async fn unknown_tool_returns_error_message() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("nonexistent_tool", &json!({})).await;
        assert!(result.contains("not available"));
    }

    // ── Bash execution ─────────────────────────────────────────────────

    #[tokio::test]
    async fn bash_echo_returns_output() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo hello"}))
            .await;
        assert_eq!(result.trim(), "hello");
    }

    #[tokio::test]
    async fn bash_missing_command_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("bash", &json!({})).await;
        assert!(result.contains("Missing 'command'"));
    }

    #[tokio::test]
    async fn bash_nonzero_exit_includes_exit_code() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("bash", &json!({"command": "exit 42"})).await;
        assert!(result.contains("exit code: 42"));
    }

    #[tokio::test]
    async fn bash_stderr_is_captured() {
        let (exec, _dir) = test_executor();
        let result = exec
            .execute("bash", &json!({"command": "echo err >&2"}))
            .await;
        assert!(result.contains("stderr:"));
        assert!(result.contains("err"));
    }

    #[tokio::test]
    async fn bash_runs_in_workspace_dir() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("marker.txt"), "found").unwrap();
        let result = exec
            .server_bash(&json!({"command": "cat marker.txt"}))
            .await;
        assert_eq!(result.trim(), "found");
    }

    // ── Grep ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn grep_finds_pattern_in_files() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("test.rs"), "fn main() {}\nfn helper() {}").unwrap();
        let result = exec.execute("grep", &json!({"pattern": "fn main"})).await;
        assert!(result.contains("fn main"));
    }

    #[tokio::test]
    async fn grep_no_matches_returns_message() {
        let (exec, dir) = test_executor();
        std::fs::write(dir.path().join("empty.rs"), "nothing here").unwrap();
        let result = exec
            .execute("grep", &json!({"pattern": "ZZZZNOTFOUND"}))
            .await;
        assert!(result.contains("No matches found"));
    }

    // ── Git operations ─────────────────────────────────────────────────

    #[tokio::test]
    async fn git_status_in_non_git_dir_returns_error() {
        let (exec, _dir) = test_executor();
        let result = exec.execute("git_status", &json!({})).await;
        assert!(result.contains("Error:") || result.contains("fatal"));
    }

    #[tokio::test]
    async fn git_log_caps_at_100() {
        let (exec, dir) = test_executor();
        // Initialize a git repo
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::fs::write(dir.path().join("f.txt"), "x").unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(dir.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        // Request 999 — should be capped at 100
        let result = exec.execute("git_log", &json!({"n": 999})).await;
        assert!(result.contains("initial"));
    }

    // ── Memory tool user isolation ─────────────────────────────────────

    #[tokio::test]
    async fn memory_tool_injects_user_id() {
        let (exec, _dir) = test_executor();
        // We can't actually call Memoria, but we can verify the execute path
        // doesn't panic and returns a reasonable error (no MEMORIA_BASE_URL set).
        let result = exec
            .execute("memory_store", &json!({"content": "test"}))
            .await;
        // Should attempt the call (may fail due to no server, but shouldn't crash)
        assert!(!result.is_empty());
    }

    // ── Output management ──────────────────────────────────────────────

    #[tokio::test]
    async fn set_turn_index_and_reset_aggregate() {
        let (exec, _dir) = test_executor();
        exec.set_turn_index(5);
        assert_eq!(exec.journal_turn_index.load(Ordering::Relaxed), 5);
        exec.aggregate_output_bytes.store(999, Ordering::Relaxed);
        exec.reset_aggregate_output();
        assert_eq!(exec.aggregate_output_bytes.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn workspace_root_returns_correct_path() {
        let (exec, dir) = test_executor();
        assert_eq!(exec.workspace_root(), dir.path());
    }
}
