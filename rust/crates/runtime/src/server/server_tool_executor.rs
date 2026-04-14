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

use crate::tool_sandbox::{SandboxMode, SandboxPolicy};
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

        Self {
            workspace_root,
            user_id,
            session_id,
            sandbox_policy,
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

    /// Execute a tool call and return the result string.
    ///
    /// This is the main entry point called by the headless round when
    /// no edge agent is available.
    pub async fn execute(&self, name: &str, args: &Value) -> String {
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
            // ── File operations (delegated to sandbox) ─────────────────
            "read_file" => self.server_read_file(args),
            "write_file" => self.server_write_file(args),
            "str_replace" => self.server_str_replace(args),
            "delete_file" => self.server_delete_file(args),
            "list_dir" => self.server_list_dir(args),
            // ── Shell operations (sandboxed) ───────────────────────────
            "bash" => self.server_bash(args).await,
            "grep" => self.server_grep(args),
            "glob" => self.server_glob(args),
            // ── Git operations (read-only safe for server) ─────────────
            "git_status" => self.server_git_status(),
            "git_diff" => self.server_git_diff(args),
            "git_log" => self.server_git_log(args),
            "git_show" => self.server_git_show(args),
            "git_blame" => self.server_git_blame(args),
            "git_commit" => self.server_git_commit(args),
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

    fn server_read_file(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'path' parameter".to_string(),
        };
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error: Cannot read file: {e}"),
        };

        let start_line = args
            .get("start_line")
            .and_then(|v| v.as_u64())
            .map(|l| l as usize);
        let end_line = args
            .get("end_line")
            .and_then(|v| v.as_u64())
            .map(|l| l as usize);

        let lines: Vec<&str> = content.lines().collect();
        let start = start_line.unwrap_or(1).saturating_sub(1);
        let end = end_line.unwrap_or(lines.len()).min(lines.len());

        if start >= lines.len() {
            return format!(
                "Error: start_line {} exceeds file length {}",
                start + 1,
                lines.len()
            );
        }

        let mut result = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            result.push_str(&format!("{}\t{}\n", start + i + 1, line));
        }
        result
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

    fn server_list_dir(&self, args: &Value) -> String {
        let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return e,
        };

        let entries = match std::fs::read_dir(&path) {
            Ok(entries) => entries,
            Err(e) => return format!("Error: Cannot list directory: {e}"),
        };

        let mut result = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                result.push(format!("{name}/"));
            } else {
                result.push(name);
            }
        }
        result.sort();
        result.join("\n")
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

    fn server_grep(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'pattern' parameter".to_string(),
        };
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");

        let resolved = match self.resolve_path(path) {
            Ok(p) => p,
            Err(e) => return e,
        };

        let mut cmd = std::process::Command::new("grep");
        cmd.arg("-rn")
            .arg("--include=*.rs")
            .arg("--include=*.ts")
            .arg("--include=*.tsx")
            .arg("--include=*.js")
            .arg("--include=*.jsx")
            .arg("--include=*.py")
            .arg("--include=*.go")
            .arg("--include=*.java")
            .arg("--include=*.toml")
            .arg("--include=*.json")
            .arg("--include=*.yaml")
            .arg("--include=*.yml")
            .arg("--include=*.md")
            .arg(pattern)
            .arg(&resolved)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        match cmd.output() {
            Ok(out) => {
                let result = String::from_utf8_lossy(&out.stdout).to_string();
                if result.is_empty() {
                    format!("No matches found for pattern: {pattern}")
                } else {
                    result
                }
            }
            Err(e) => format!("Error: grep failed: {e}"),
        }
    }

    fn server_glob(&self, args: &Value) -> String {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'pattern' parameter".to_string(),
        };

        // Use find for basic glob matching
        let mut cmd = std::process::Command::new("find");
        cmd.arg(&self.workspace_root)
            .arg("-name")
            .arg(pattern)
            .arg("-type")
            .arg("f")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        match cmd.output() {
            Ok(out) => {
                let result = String::from_utf8_lossy(&out.stdout).to_string();
                if result.is_empty() {
                    format!("No files found matching pattern: {pattern}")
                } else {
                    result
                }
            }
            Err(e) => format!("Error: glob failed: {e}"),
        }
    }

    // ────────────────────────────────────────────────────────────────────────
    // Git operations
    // ────────────────────────────────────────────────────────────────────────

    fn git_command(&self, git_args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args(git_args)
            .current_dir(&self.workspace_root)
            .output();

        match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                if out.status.success() {
                    stdout
                } else {
                    format!("Error: git {}: {}", git_args.join(" "), stderr)
                }
            }
            Err(e) => format!("Error: git command failed: {e}"),
        }
    }

    fn server_git_status(&self) -> String {
        self.git_command(&["status", "--porcelain", "-b"])
    }

    fn server_git_diff(&self, args: &Value) -> String {
        let mut git_args = vec!["diff"];
        if let Some(true) = args.get("staged").and_then(|v| v.as_bool()) {
            git_args.push("--cached");
        }
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            git_args.push("--");
            git_args.push(path);
        }
        self.git_command(&git_args)
    }

    fn server_git_log(&self, args: &Value) -> String {
        let n = args
            .get("n")
            .and_then(|v| v.as_u64())
            .unwrap_or(10)
            .min(100);
        let n_str = format!("-{n}");
        let mut git_args = vec!["log", "--oneline", &n_str];
        if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
            git_args.push("--");
            git_args.push(path);
        }
        self.git_command(&git_args)
    }

    fn server_git_show(&self, args: &Value) -> String {
        let revision = args
            .get("revision")
            .and_then(|v| v.as_str())
            .unwrap_or("HEAD");
        self.git_command(&["show", "--stat", revision])
    }

    fn server_git_blame(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return "Error: Missing 'path' parameter".to_string(),
        };
        self.git_command(&["blame", "--line-porcelain", path])
    }

    fn server_git_commit(&self, args: &Value) -> String {
        let message = match args.get("message").and_then(|v| v.as_str()) {
            Some(m) => m,
            None => return "Error: Missing 'message' parameter".to_string(),
        };
        // Stage all changes first
        let _ = self.git_command(&["add", "-A"]);
        self.git_command(&["commit", "-m", message])
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
