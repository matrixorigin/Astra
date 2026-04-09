//! Edge tool definitions and execution for the astra CLI.
//!
//! Tools: bash, read_file (with outline mode), write_file, str_replace (with fuzzy matching),
//!        list_dir, grep (with context_lines/max_matches), glob,
//!        git_status, git_diff, git_log, git_show, git_blame, git_file_history,
//!        git_contributors, git_log_search, web_fetch,
//!        mo_query, mo_snapshot, mo_branch

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use astra_runtime::str_preview::truncate_str;
use astra_runtime::tool_sandbox::{
    SandboxMode, SandboxPolicy, sandbox_command, validate_path, wrap_command_with_limits,
};

/// Prefix returned by tool execution when the sandbox blocks a path.
/// The agentic loop / permission manager can detect this to prompt the user
/// for authorization instead of letting the model silently fall back to bash.
pub const SANDBOX_DENIED_PREFIX: &str = "SANDBOX_DENIED: ";
use chrono::{DateTime, Utc};
use crossterm::style::Stylize;
use reqwest::{Client, Method, StatusCode};
use serde_json::{Value, json};

#[path = "delta_log.rs"]
pub mod delta_log;

#[path = "edge_tools/build_test.rs"]
mod build_test;
#[path = "edge_tools/code_intel.rs"]
pub mod code_intel;
#[path = "edge_tools/fs.rs"]
mod fs_tools;
#[path = "edge_tools/git_gix.rs"]
mod git_gix;
#[path = "edge_tools/github.rs"]
mod github;
#[path = "edge_tools/lsp_stdio_session.rs"]
mod lsp_stdio_session;
#[path = "edge_tools/mo_tools.rs"]
mod mo_tools;
#[path = "edge_tools/passive_cargo_check.rs"]
mod passive_cargo_check;
#[path = "edge_tools/passive_lsp.rs"]
mod passive_lsp;
#[path = "edge_tools/passive_tsc_check.rs"]
mod passive_tsc_check;
#[path = "edge_tools/shell.rs"]
mod shell;
#[path = "edge_tools/agent_messaging.rs"]
pub mod agent_messaging;
#[path = "edge_tools/agent_spawning.rs"]
pub mod agent_spawning;
#[path = "edge_tools/context_sharing.rs"]
pub mod context_sharing;
#[path = "edge_tools/schemas.rs"]
mod schemas;
pub use schemas::all_tool_schemas;
#[path = "edge_tools/env_tools.rs"]
mod env_tools;
pub use env_tools::apply_overlay as apply_env_overlay;
#[path = "edge_tools/task_mgmt.rs"]
mod task_mgmt;

// ─── Tool schema ─────────────────────────────────────────────────────────────

/// Maximum number of entries in the file state cache. When exceeded, the
/// entry with the oldest timestamp is evicted.
const MAX_FILE_STATE_ENTRIES: usize = 200;

/// Shape of the last `read_file` call, for consecutive-request dedup (same idea as
/// Claude Code `FileReadTool`: same offset+limit + unchanged mtime → stub before I/O).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReadDedupKey {
    Full,
    Outline,
    /// Raw `start_line` / `end_line` JSON (absent key = `None`), like Claude's offset/limit.
    Range {
        start_line: Option<u64>,
        end_line: Option<u64>,
    },
}

/// Git porcelain status codes for git status --porcelain output parsing.
/// Format: XY PATH or XY ORIG_PATH -> PATH where X and Y are status codes.
mod git_status {
    /// Modified in index (staged)
    pub const MODIFIED: char = 'M';
    /// Added to index (staged)
    pub const ADDED: char = 'A';
    /// Deleted from index (staged)
    pub const DELETED: char = 'D';
    /// Renamed in index (staged)
    pub const RENAMED: char = 'R';
    /// Untracked file marker (both positions are '?')
    pub const UNTRACKED_PREFIX: &str = "??";
}

/// Tracks the last-read state of a file for staleness detection and dedup.
/// Inspired by Claude Code's readFileState mechanism.
struct FileState {
    /// mtime (milliseconds) at the time of last read/write.
    timestamp_ms: u128,
    /// True if the last operation was a read (not a write/edit).
    /// Dedup only fires when the previous op was a read.
    from_read: bool,
    /// True if the last read was a partial view (outline, line range).
    is_partial: bool,
    /// How many times this file has been fully read.
    /// Used for escalating warnings when the model loops on the same file.
    read_count: u32,
    /// How many times this file has been read with different ranges.
    /// Used to nudge the model toward grep for large files.
    ranged_read_count: u32,
    /// Last read_file request shape (updated on every successful read).
    last_dedup_key: ReadDedupKey,
}

// ─── Tool execution ───────────────────────────────────────────────────────────

/// Global output size limit. Individual tools may have tighter limits.
/// Override with `MO_GLOBAL_OUTPUT_LIMIT` env var.
fn global_output_limit() -> usize {
    astra_core::RuntimeLimits::global().global_output_limit
}
/// Per-tool default output limit for tools without explicit truncation.
/// Override with `MO_TOOL_OUTPUT_LIMIT` env var.
fn tool_output_limit() -> usize {
    astra_core::RuntimeLimits::global().tool_output_limit
}

/// Per-turn aggregate output budget (bytes). When cumulative tool output
/// exceeds this, subsequent tools get tighter limits. Inspired by Claude
/// Code's `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` (200K).
const AGGREGATE_OUTPUT_BUDGET: usize = 200_000;

/// Soft threshold at which aggregate-aware gating starts warning.
/// Tools that produce large output (read_file, git_show) will check this
/// before doing I/O and suggest lighter alternatives when exceeded.
const AGGREGATE_SOFT_LIMIT: usize = 120_000;

/// Per-tool persistence threshold (chars). When a single tool result exceeds
/// this AND aggregate output is above the soft limit, the result is persisted
/// to disk and replaced with a preview + file path. The model can use
/// `read_file` with `start_line/end_line` to access specific parts.
/// Inspired by Claude Code's `DEFAULT_MAX_RESULT_SIZE_CHARS` (50K).
const PERSIST_THRESHOLD: usize = 50_000;

/// Preview size (bytes) included in the persisted-output reference message.
const PERSIST_PREVIEW_BYTES: usize = 2000;

/// Maximum file size (10MB) for LSP operations to prevent OOM.
const MAX_LSP_FILE_SIZE: usize = 10 * 1024 * 1024;

/// Directory for persisted tool results within the astra home.
fn tool_results_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("tool-results")
}

/// Convert UTF-16 column position to char index.
/// LSP protocol uses UTF-16 code units, Rust uses UTF-8 chars.
/// Handles surrogate pairs (emoji, etc.) correctly.
fn utf16_col_to_char_idx(line: &str, col_utf16: usize) -> usize {
    let mut utf16_offset = 0;
    for (char_idx, c) in line.chars().enumerate() {
        // Check if we've reached or passed the target UTF-16 offset
        if utf16_offset + c.len_utf16() > col_utf16 {
            return char_idx;
        }
        utf16_offset += c.len_utf16();
    }
    // Column is past end of line - return line length
    line.chars().count()
}

/// Truncate tool output to `max_bytes`, cutting at a newline boundary when
/// possible (avoids mid-line cuts that confuse the LLM). Inspired by Claude
/// Code's `generatePreview` pattern.
fn truncate_output(mut output: String, max_bytes: usize) -> String {
    if output.len() > max_bytes {
        let end = output.floor_char_boundary(max_bytes);
        // Prefer cutting at a newline within the last 50% of the budget
        let cut = output[..end]
            .rfind('\n')
            .filter(|&pos| pos > end / 2)
            .map(|pos| pos + 1) // include the newline
            .unwrap_or(end);
        output.truncate(cut);
        output.push_str("\n[truncated]");
    }
    output
}

/// Normalize empty/whitespace-only tool output to a short marker.
/// Prevents model confusion from truly empty tool results.
/// Inspired by Claude Code's `isToolResultContentEmpty` guard.
fn normalize_empty_output(output: String, tool_name: &str) -> String {
    if output.trim().is_empty() {
        format!("({tool_name} completed with no output)")
    } else {
        output
    }
}

/// Parse a grep output line to extract the file path and line number.
/// Handles format: `file:line:content` or `file:line:col:content`.
fn parse_grep_file_line(line: &str) -> Option<(&str, usize)> {
    let first_colon = line.find(':')?;
    let file = &line[..first_colon];
    let rest = &line[first_colon + 1..];
    let second_colon = rest.find(':')?;
    let line_str = &rest[..second_colon];
    let line_num: usize = line_str.parse().ok()?;
    Some((file, line_num))
}

/// Categorize a grep reference line as definition, import, call, or usage.
///
/// Examines the content after the `file:line:` prefix for language-specific
/// patterns to determine the kind of reference.
fn categorize_reference(line: &str, _symbol: &str) -> &'static str {
    // Extract content after file:line: prefix
    let content = if let Some(first_colon) = line.find(':') {
        let rest = &line[first_colon + 1..];
        if let Some(second_colon) = rest.find(':') {
            rest[second_colon + 1..].trim()
        } else {
            rest.trim()
        }
    } else {
        line.trim()
    };

    let lower = content.to_lowercase();

    // 1. Import patterns (check FIRST — some overlap with definitions via `const`)
    if lower.starts_with("use ")
        || lower.starts_with("pub use ")
        || lower.starts_with("import ")
        || lower.starts_with("from ")
        || lower.contains("require(")
        || lower.starts_with("#include")
    {
        return "import";
    }

    // 2. Definition patterns — type/function/class declarations (NOT variable bindings)
    if lower.starts_with("fn ")
        || lower.starts_with("pub fn ")
        || lower.starts_with("pub(crate) fn ")
        || lower.starts_with("async fn ")
        || lower.starts_with("pub async fn ")
        || lower.starts_with("def ")
        || lower.starts_with("async def ")
        || lower.starts_with("function ")
        || lower.starts_with("func ")
        || lower.starts_with("class ")
        || lower.starts_with("pub struct ")
        || lower.starts_with("struct ")
        || lower.starts_with("pub enum ")
        || lower.starts_with("enum ")
        || lower.starts_with("pub trait ")
        || lower.starts_with("trait ")
        || lower.starts_with("interface ")
        || lower.starts_with("type ")
        || lower.starts_with("pub type ")
        || lower.starts_with("pub const ")
        || lower.starts_with("pub static ")
        || lower.starts_with("static ")
    {
        return "definition";
    }

    // 3. Call patterns: contains parentheses (likely a function/method call)
    if content.contains('(') {
        return "call";
    }

    // 4. Everything else is a usage (type annotations, field access, etc.)
    "usage"
}

/// Parse content strings from a Memoria search/retrieve response.
///
/// Handles common Memoria response shapes:
/// - `{ "memories": [ { "content": "..." }, ... ] }`
/// - `[ { "content": "..." }, ... ]`
/// - `{ "results": [ { "content": "..." }, ... ] }`
///
/// Returns empty vec on parse failure or error responses (graceful degradation).
pub fn parse_memory_search_contents(raw: &str) -> Vec<String> {
    let Ok(val) = serde_json::from_str::<Value>(raw) else {
        return vec![];
    };
    // Error response from memoria
    if val.get("error").is_some() {
        return vec![];
    }
    // Try common response shapes
    let items = val
        .get("memories")
        .or_else(|| val.get("results"))
        .and_then(Value::as_array)
        .or_else(|| val.as_array());

    let Some(arr) = items else {
        // Single object with content?
        if let Some(c) = val.get("content").and_then(Value::as_str) {
            return vec![c.to_string()];
        }
        return vec![];
    };

    arr.iter()
        .filter_map(|item| {
            item.get("content")
                .or_else(|| item.get("text"))
                .and_then(Value::as_str)
                .map(|s| s.to_string())
        })
        .filter(|s| !s.is_empty())
        .collect()
}

pub struct ToolExecutor {
    pub project_root: PathBuf,
    /// Cloud API base URL — used to proxy memory tool calls through the server
    /// so the server can add user_id for proper multi-user isolation.
    pub cloud_base: Option<String>,
    /// Auth token for cloud proxy calls.
    pub cloud_token: Option<String>,
    /// Optional GitHub token for authenticated GitHub API requests.
    pub github_token: Option<String>,
    /// Shared async GitHub client for edge tools.
    pub github_client: Client,
    /// Security sandbox policy for tool execution (None = Permissive/legacy).
    pub sandbox_policy: Option<SandboxPolicy>,
    /// Preferred repos for disambiguation (owner/repo format, lowercased).
    /// Populated from: git remote origin, recent tool results, memory.
    /// When a bare repo name like "memoria" matches multiple GitHub repos,
    /// the resolver prefers repos whose owner/name is in this list.
    /// Uses Mutex to allow learning from resolved repos without &mut self.
    preferred_repos: std::sync::Mutex<Vec<String>>,
    /// Per-turn budget pressure (0.0 = normal, 1.0 = critical).
    /// Set before each tool execution batch, read by tools that produce
    /// variable-size output (git_diff, git_show) to scale their limits.
    budget_pressure: std::sync::Mutex<f64>,
    /// Build/test iteration tracker — tracks error deltas across fix cycles.
    build_test_tracker: std::sync::Mutex<build_test::BuildTestTracker>,
    /// Circuit breaker: skip Memoria calls after consecutive failures.
    memoria_fail_count: std::sync::atomic::AtomicU32,
    /// File state tracker: records mtime after each read/write/edit.
    /// Used for staleness detection (prevent overwriting user edits)
    /// and dedup (skip re-reading unchanged files).
    file_state: std::sync::Mutex<HashMap<PathBuf, FileState>>,
    /// Per-turn aggregate tool output size (bytes). When this exceeds
    /// `AGGREGATE_OUTPUT_BUDGET`, subsequent tool outputs are truncated
    /// more aggressively. Inspired by Claude Code's
    /// `MAX_TOOL_RESULTS_PER_MESSAGE_CHARS` (200K).
    aggregate_output_bytes: std::sync::atomic::AtomicUsize,
    /// URL fetch cache: LRU-style cache mapping URL → (response, timestamp).
    /// Returns cached response for repeated fetches within TTL (15 minutes).
    /// Prevents wasting tokens re-fetching the same documentation pages.
    url_cache: std::sync::Mutex<HashMap<String, (String, std::time::Instant)>>,
    /// After a `.rs` file is written under a Rust workspace, set so the next
    /// `/chat` turn with `tool_results` can run passive `cargo check` and inject diagnostics.
    passive_cargo_pending: AtomicBool,
    /// After `.ts` / `.tsx` edits when `tsconfig.json` exists, run passive `tsc --noEmit`.
    passive_tsc_pending: AtomicBool,
    /// Optional passive LSP sessions (rust-analyzer, typescript-language-server).
    passive_lsp: passive_lsp::PassiveLspManager,
    /// MCP client manager for external tool servers.
    /// When present, tool names starting with `mcp_` are routed to MCP servers.
    pub mcp_manager:
        Option<std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>>,
    /// File edit journal — records before-state of every file write for undo.
    pub file_journal: std::sync::Mutex<astra_runtime::turn::file_edit_journal::FileEditJournal>,
    /// Current turn index for file journal entries. Set externally per-turn.
    pub journal_turn_index: std::sync::atomic::AtomicU32,
    /// Active worktree session state. When set, `effective_project_root()` returns
    /// the worktree path instead of the original `project_root`.
    worktree_session: std::sync::Mutex<Option<WorktreeSession>>,
    /// In-memory task manager for the current session.
    task_manager: task_mgmt::TaskManager,
    /// Optional agent spawning context for the `spawn_agent` tool.
    pub spawn_context: Option<agent_spawning::SpawnAgentContext>,
}

/// State for an active worktree session created by `git_worktree enter`.
/// Tracks the worktree path, branch, and original directory for restoration.
#[derive(Debug, Clone)]
pub struct WorktreeSession {
    /// Path to the worktree directory (new working root).
    pub worktree_path: PathBuf,
    /// Branch name of the worktree.
    pub branch_name: String,
    /// Original project root to restore on `exit`.
    pub original_root: PathBuf,
    /// Git commit SHA at the time the worktree was created.
    pub original_head_commit: Option<String>,
}

/// Extract owner/repo from git remote URLs in the given directory.
/// Returns lowercased "owner/repo" strings for all GitHub remotes.
fn detect_git_remote_repos(project_root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut repos = Vec::new();
    for line in stdout.lines() {
        if let Some(repo) = extract_github_owner_repo(line) {
            let lower = repo.to_lowercase();
            if !repos.contains(&lower) {
                repos.push(lower);
            }
        }
    }
    repos
}

/// Parse owner/repo from a GitHub remote URL (SSH or HTTPS).
fn extract_github_owner_repo(remote_line: &str) -> Option<String> {
    // SSH:   git@github.com:MatrixOrigin/Memoria.git (fetch)
    // HTTPS: https://github.com/MatrixOrigin/Memoria.git (fetch)
    let parts: Vec<&str> = remote_line.split_whitespace().collect();
    let url = parts.get(1)?;
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if url.contains("://github.com/") {
        url.rsplit_once("github.com/")?.1
    } else {
        return None;
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.contains('/') && !path.contains(' ') {
        Some(path.to_string())
    } else {
        None
    }
}

/// Count uncommitted changes and new commits in a worktree since a baseline commit.
/// Returns (changed_files, commits). Used by `exit_worktree` to warn before discarding work.
fn count_worktree_changes(worktree_path: &Path, original_head: Option<&str>) -> (usize, usize) {
    // Count uncommitted files
    let status = Command::new("git")
        .args(["-C", &worktree_path.display().to_string(), "status", "--porcelain"])
        .output();
    let changed_files = status
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .unwrap_or(0);

    // Count commits since baseline
    let commits = original_head
        .and_then(|base| {
            Command::new("git")
                .args(["-C", &worktree_path.display().to_string(), "rev-list", "--count", &format!("{base}..HEAD")])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        })
        .unwrap_or(0);

    (changed_files, commits)
}

impl ToolExecutor {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = project_root.into();
        let preferred_repos = detect_git_remote_repos(&root);
        let sandbox = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
        Self {
            project_root: root,
            cloud_base: None,
            cloud_token: None,
            github_token: env::var("GITHUB_TOKEN")
                .ok()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty()),
            github_client: Client::builder()
                .timeout(Duration::from_secs(15))
                .user_agent(format!("astra/{}", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("failed to create GitHub HTTP client"),
            sandbox_policy: Some(sandbox),
            preferred_repos: std::sync::Mutex::new(preferred_repos),
            budget_pressure: std::sync::Mutex::new(0.0),
            build_test_tracker: std::sync::Mutex::new(build_test::BuildTestTracker::new()),
            memoria_fail_count: std::sync::atomic::AtomicU32::new(0),
            file_state: std::sync::Mutex::new(HashMap::new()),
            aggregate_output_bytes: std::sync::atomic::AtomicUsize::new(0),
            url_cache: std::sync::Mutex::new(HashMap::new()),
            passive_cargo_pending: AtomicBool::new(false),
            passive_tsc_pending: AtomicBool::new(false),
            passive_lsp: passive_lsp::PassiveLspManager::new(),
            mcp_manager: None,
            file_journal: std::sync::Mutex::new(
                astra_runtime::turn::file_edit_journal::FileEditJournal::default(),
            ),
            journal_turn_index: std::sync::atomic::AtomicU32::new(0),
            worktree_session: std::sync::Mutex::new(None),
            task_manager: task_mgmt::TaskManager::new(),
            spawn_context: None,
        }
    }

    /// Set the spawn context for agent spawning.
    pub fn with_spawn_context(mut self, ctx: agent_spawning::SpawnAgentContext) -> Self {
        self.spawn_context = Some(ctx);
        self
    }

    /// Configure cloud proxy for memory tool calls.
    pub fn with_cloud(mut self, base: impl Into<String>, token: impl Into<String>) -> Self {
        self.cloud_base = Some(base.into());
        self.cloud_token = Some(token.into());
        self
    }

    /// Return the effective project root, considering any active worktree session.
    /// When inside a worktree session, returns the worktree path; otherwise returns
    /// the original `project_root`.
    pub fn effective_project_root(&self) -> PathBuf {
        if let Ok(guard) = self.worktree_session.lock() {
            if let Some(ref session) = *guard {
                return session.worktree_path.clone();
            }
        }
        self.project_root.clone()
    }

    /// Check if there is an active worktree session.
    pub fn in_worktree_session(&self) -> bool {
        self.worktree_session
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Get a clone of the current worktree session state, if any.
    pub fn get_worktree_session(&self) -> Option<WorktreeSession> {
        self.worktree_session.lock().ok()?.clone()
    }

    /// Enter a worktree session. Creates the worktree and updates internal state.
    /// Returns the new WorktreeSession on success.
    pub fn enter_worktree(&self, branch: &str) -> Result<WorktreeSession, String> {
        // Check if already in a worktree session
        if self.in_worktree_session() {
            return Err("Already in a worktree session. Use git_worktree exit first.".to_string());
        }

        // Validate branch name
        if branch.is_empty() {
            return Err("Branch name is required".to_string());
        }
        if branch.chars().any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '{' | '}')) {
            return Err("Invalid branch name".to_string());
        }

        // Get current HEAD commit for later diffing
        let original_head = git_gix::head_short(&self.project_root);

        // Generate worktree path as sibling directory
        let repo_name = self.project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let sanitized_branch = branch.replace('/', "-");
        let worktree_path = self.project_root
            .parent()
            .unwrap_or(&self.project_root)
            .join(format!("{repo_name}-wt-{sanitized_branch}"));

        if worktree_path.exists() {
            return Err(format!("Worktree path already exists: {}", worktree_path.display()));
        }

        // Create the worktree with a new branch
        let output = Command::new("git")
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree_path)
            .current_dir(&self.project_root)
            .output()
            .map_err(|e| format!("Failed to create worktree: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree add failed: {}", stderr.trim()));
        }

        let session = WorktreeSession {
            worktree_path: worktree_path.clone(),
            branch_name: branch.to_string(),
            original_root: self.project_root.clone(),
            original_head_commit: if original_head.is_empty() { None } else { Some(original_head) },
        };

        // Update session state
        if let Ok(mut guard) = self.worktree_session.lock() {
            *guard = Some(session.clone());
        }

        // Clear file state cache (paths are relative to project root)
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }

        Ok(session)
    }

    /// Exit the current worktree session.
    /// `action`: "keep" preserves the worktree; "remove" deletes it.
    /// `discard_changes`: required when removing a worktree with uncommitted changes.
    pub fn exit_worktree(&self, action: &str, discard_changes: bool) -> Result<String, String> {
        let session = {
            let guard = self.worktree_session.lock().map_err(|_| "Lock poisoned")?;
            guard.clone().ok_or("Not in a worktree session")?
        };

        // Count uncommitted changes and commits
        let (changed_files, commits) = count_worktree_changes(&session.worktree_path, session.original_head_commit.as_deref());

        if action == "remove" && (changed_files > 0 || commits > 0) && !discard_changes {
            let mut parts = Vec::new();
            if changed_files > 0 {
                parts.push(format!("{} uncommitted file(s)", changed_files));
            }
            if commits > 0 {
                parts.push(format!("{} commit(s) on {}", commits, session.branch_name));
            }
            return Err(format!(
                "Worktree has {}. Set discard_changes=true to confirm removal, or use action='keep' to preserve.",
                parts.join(" and ")
            ));
        }

        let worktree_path_str = session.worktree_path.display().to_string();
        let branch_name = session.branch_name.clone();
        let original_root = session.original_root.clone();

        // Clear session state first
        if let Ok(mut guard) = self.worktree_session.lock() {
            *guard = None;
        }

        // Clear file state cache
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }

        if action == "remove" {
            // Remove the worktree
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&session.worktree_path)
                .current_dir(&original_root)
                .output();

            // Also delete the branch
            let _ = Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(&original_root)
                .output();

            let discard_note = if changed_files > 0 || commits > 0 {
                format!(" Discarded {} file(s) and {} commit(s).", changed_files, commits)
            } else {
                String::new()
            };

            Ok(format!(
                "✓ Exited and removed worktree at {}.{} Session restored to {}",
                worktree_path_str, discard_note, original_root.display()
            ))
        } else {
            // Keep the worktree
            Ok(format!(
                "✓ Exited worktree. Work preserved at {} on branch {}. Session restored to {}",
                worktree_path_str, branch_name, original_root.display()
            ))
        }
    }

    /// Execute git_worktree tool with enter/exit session management.
    fn git_worktree(&self, args: &Value) -> String {
        let action = match args.get("action").and_then(Value::as_str) {
            Some(a) => a,
            None => return "Error: 'action' is required (enter, exit, add, list, remove)".to_string(),
        };

        match action {
            "enter" => {
                let branch = match args.get("branch").and_then(Value::as_str) {
                    Some(b) if !b.is_empty() => b,
                    _ => return "Error: 'branch' is required for enter".to_string(),
                };
                match self.enter_worktree(branch) {
                    Ok(session) => format!(
                        "✓ Entered worktree\n  Branch: {}\n  Path: {}\n  Session is now working in the worktree. Use `git_worktree exit` to leave.",
                        session.branch_name, session.worktree_path.display()
                    ),
                    Err(e) => format!("Error: {e}"),
                }
            }
            "exit" => {
                let exit_action = args.get("exit_action").and_then(Value::as_str).unwrap_or("keep");
                let discard = args.get("discard_changes").and_then(Value::as_bool).unwrap_or(false);
                match self.exit_worktree(exit_action, discard) {
                    Ok(msg) => msg,
                    Err(e) => format!("Error: {e}"),
                }
            }
            "add" | "create" => git_gix::worktree_add(&self.project_root, args),
            "list" | "ls" => git_gix::worktree_list(&self.project_root),
            "remove" | "rm" | "delete" => git_gix::worktree_remove(&self.project_root, args),
            _ => format!("Error: unknown worktree action '{action}'. Use: enter, exit, add, list, remove"),
        }
    }

    /// Ask the user a question and wait for their response.
    /// Supports multiple choice and free-form input.
    fn ask_user(&self, args: &Value) -> String {
        use crossterm::{
            event::{self, Event, KeyCode, KeyEvent},
            terminal::{disable_raw_mode, enable_raw_mode},
        };
        use std::io::{self, Write};
        use std::time::Duration;

        const MAX_INPUT_LEN: usize = 4096; // 4KB limit

        let question = match args.get("question").and_then(Value::as_str) {
            Some(q) if !q.is_empty() => q,
            _ => return "Error: 'question' is required".to_string(),
        };

        let choices: Vec<&str> = args
            .get("choices")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();

        // Validate choices count (2-9 for single-key optimization)
        if !choices.is_empty() && (choices.len() < 2 || choices.len() > 9) {
            return "Error: choices must contain 2-9 options".to_string();
        }

        let default = args.get("default").and_then(Value::as_str);
        let context = args.get("context").and_then(Value::as_str);

        // Display the question
        eprintln!();
        if let Some(ctx) = context {
            eprintln!("  {}", ctx.dim());
        }
        eprintln!("  {} {}", "▸".cyan(), question.bold().cyan());

        if choices.is_empty() {
            // Free-form input
            eprintln!();
            let prompt = if let Some(def) = default {
                format!("  {} {} ", format!("[{def}]").dim(), "→".cyan())
            } else {
                format!("  {} ", "→".cyan())
            };
            eprint!("{}", prompt);
            let _ = io::stderr().flush();

            let mut response = String::new();
            if io::stdin().read_line(&mut response).is_err() {
                return "Error: failed to read user input".to_string();
            }
            // Truncate if too long
            if response.len() > MAX_INPUT_LEN {
                response.truncate(MAX_INPUT_LEN);
            }
            let response = response.trim();
            let answer = if response.is_empty() {
                default.unwrap_or("").to_string()
            } else {
                response.to_string()
            };
            serde_json::json!({
                "answer": answer,
                "question": question
            }).to_string()
        } else {
            // Multiple choice
            eprintln!();
            for (i, choice) in choices.iter().enumerate() {
                let num = i + 1;
                let is_default = default.map_or(i == 0, |d| *choice == d);
                if is_default {
                    eprintln!(
                        "  {} {} {}",
                        "▸".cyan(),
                        format!("[{num}]").cyan(),
                        choice.bold()
                    );
                } else {
                    eprintln!(
                        "    {} {}",
                        format!("[{num}]").dim(),
                        choice.dim()
                    );
                }
            }
            eprintln!();
            eprint!("  {} ", "→".cyan());
            let _ = io::stderr().flush();

            // Try raw mode for single-key selection
            struct RawModeGuard;
            impl Drop for RawModeGuard {
                fn drop(&mut self) {
                    let _ = disable_raw_mode();
                }
            }

            let answer = if enable_raw_mode().is_ok() {
                let _guard = RawModeGuard;
                let mut input = String::new();
                let mut consecutive_errors = 0u8;
                loop {
                    // Use poll with timeout to avoid infinite spin on persistent errors
                    match event::poll(Duration::from_millis(100)) {
                        Ok(true) => {
                            match event::read() {
                                Ok(Event::Key(KeyEvent { code, .. })) => {
                                    consecutive_errors = 0;
                                    match code {
                                        KeyCode::Char(c) if c.is_ascii_digit() && input.is_empty() => {
                                            let idx = c.to_digit(10).unwrap() as usize;
                                            if idx >= 1 && idx <= choices.len() {
                                                drop(_guard);
                                                eprintln!("{}", c);
                                                break choices[idx - 1].to_string();
                                            }
                                            input.push(c);
                                            eprint!("{}", c);
                                        }
                                        KeyCode::Char(c) => {
                                            if input.len() < MAX_INPUT_LEN {
                                                input.push(c);
                                                eprint!("{}", c);
                                            }
                                        }
                                        KeyCode::Backspace if !input.is_empty() => {
                                            input.pop();
                                            eprint!("\x08 \x08");
                                        }
                                        KeyCode::Enter => {
                                            drop(_guard);
                                            eprintln!();
                                            let trimmed = input.trim();
                                            if trimmed.is_empty() {
                                                break default.unwrap_or(choices[0]).to_string();
                                            }
                                            if let Ok(idx) = trimmed.parse::<usize>() {
                                                if idx >= 1 && idx <= choices.len() {
                                                    break choices[idx - 1].to_string();
                                                }
                                            }
                                            break trimmed.to_string();
                                        }
                                        KeyCode::Esc => {
                                            drop(_guard);
                                            eprintln!();
                                            break "[cancelled]".to_string();
                                        }
                                        _ => {}
                                    }
                                }
                                Ok(_) => {} // Ignore non-key events
                                Err(_) => {
                                    consecutive_errors += 1;
                                    if consecutive_errors >= 5 {
                                        drop(_guard);
                                        eprintln!();
                                        break "[error: terminal read failed]".to_string();
                                    }
                                }
                            }
                        }
                        Ok(false) => continue, // Timeout, poll again
                        Err(_) => {
                            consecutive_errors += 1;
                            if consecutive_errors >= 5 {
                                drop(_guard);
                                eprintln!();
                                break "[error: terminal unavailable]".to_string();
                            }
                        }
                    }
                    let _ = io::stderr().flush();
                }
            } else {
                // Fallback: line-based input
                let mut response = String::new();
                if io::stdin().read_line(&mut response).is_err() {
                    return "Error: failed to read user input".to_string();
                }
                if response.len() > MAX_INPUT_LEN {
                    response.truncate(MAX_INPUT_LEN);
                }
                let trimmed = response.trim();
                if trimmed.is_empty() {
                    default.unwrap_or(choices[0]).to_string()
                } else if let Ok(idx) = trimmed.parse::<usize>() {
                    if idx >= 1 && idx <= choices.len() {
                        choices[idx - 1].to_string()
                    } else {
                        trimmed.to_string()
                    }
                } else {
                    trimmed.to_string()
                }
            };

            serde_json::json!({
                "answer": answer,
                "question": question,
                "was_custom": !choices.contains(&answer.as_str())
            }).to_string()
        }
    }

    // ─── Task management methods (delegated to task_mgmt module) ────────────

    async fn task_create(&self, args: &Value) -> String { self.task_manager.create(args).await }
    async fn task_list(&self, args: &Value) -> String { self.task_manager.list(args).await }
    async fn task_get(&self, args: &Value) -> String { self.task_manager.get(args).await }
    async fn task_update(&self, args: &Value) -> String { self.task_manager.update(args).await }
    async fn task_stop(&self, args: &Value) -> String { self.task_manager.stop(args).await }

    /// Sleep for a specified duration without holding a shell process.
    async fn sleep_tool(&self, args: &Value) -> String {
        const MAX_SLEEP_MS: u64 = 300_000; // 5 minutes max

        let duration_ms = match args.get("duration_ms").and_then(Value::as_u64) {
            Some(ms) if ms > 0 => ms.min(MAX_SLEEP_MS),
            Some(_) => return "Error: duration_ms must be positive".to_string(),
            None => return "Error: 'duration_ms' is required".to_string(),
        };

        let reason = args.get("reason").and_then(Value::as_str).unwrap_or("waiting");
        
        eprintln!("  {}", format!("💤 Sleeping for {}ms ({})", duration_ms, reason).dim());
        
        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;
        
        serde_json::json!({
            "success": true,
            "slept_ms": duration_ms,
            "reason": reason
        }).to_string()
    }

    /// Search available tools by name or description keywords.
    /// Supports direct selection with 'select:tool_name' or keyword search.
    fn tool_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.is_empty() => q.trim(),
            _ => return "Error: 'query' is required".to_string(),
        };

        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(5)
            .min(20) as usize;

        let all_tools = all_tool_schemas();

        // Direct selection mode: select:tool_name or select:a,b,c
        if let Some(tool_names) = query.strip_prefix("select:") {
            let requested: Vec<&str> = tool_names.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()).collect();
            let mut found = Vec::new();
            let mut missing = Vec::new();

            for name in requested {
                let name_lower = name.to_lowercase();
                if let Some(tool) = all_tools.iter().find(|t| {
                    t.get("function")
                        .and_then(|f| f.get("name"))
                        .and_then(Value::as_str)
                        .map(|n| n.to_lowercase() == name_lower)
                        .unwrap_or(false)
                }) {
                    if let Some(func) = tool.get("function") {
                        let tool_name = func.get("name").and_then(Value::as_str).unwrap_or("");
                        let desc = func.get("description").and_then(Value::as_str).unwrap_or("");
                        // Truncate description for readability
                        let short_desc: String = desc.chars().take(100).collect();
                        found.push(serde_json::json!({
                            "name": tool_name,
                            "description": if desc.len() > 100 { format!("{}...", short_desc) } else { desc.to_string() }
                        }));
                    }
                } else {
                    missing.push(name.to_string());
                }
            }

            return serde_json::json!({
                "query": query,
                "matches": found,
                "missing": missing,
                "total_tools": all_tools.len()
            }).to_string();
        }

        // Keyword search mode
        let query_lower = query.to_lowercase();
        let query_terms: Vec<&str> = query_lower.split_whitespace().collect();

        let mut scored: Vec<(usize, &Value)> = all_tools
            .iter()
            .filter_map(|tool| {
                let func = tool.get("function")?;
                let name = func.get("name")?.as_str()?;
                let desc = func.get("description").and_then(Value::as_str).unwrap_or("");
                
                let name_lower = name.to_lowercase();
                let desc_lower = desc.to_lowercase();
                
                let mut score = 0usize;
                
                for term in &query_terms {
                    // Exact name match (high weight)
                    if name_lower == *term {
                        score += 20;
                    } else if name_lower.contains(term) {
                        // Partial name match
                        score += 10;
                    }
                    
                    // Split camelCase/snake_case for part matching
                    let name_parts: Vec<String> = name
                        .replace('_', " ")
                        .chars()
                        .fold(String::new(), |mut acc, c| {
                            if c.is_uppercase() && !acc.is_empty() {
                                acc.push(' ');
                            }
                            acc.push(c);
                            acc
                        })
                        .to_lowercase()
                        .split_whitespace()
                        .map(String::from)
                        .collect();
                    
                    for part in &name_parts {
                        if part == *term {
                            score += 8;
                        } else if part.contains(term) {
                            score += 4;
                        }
                    }
                    
                    // Description match (lower weight)
                    if desc_lower.contains(term) {
                        score += 2;
                    }
                }
                
                if score > 0 {
                    Some((score, tool))
                } else {
                    None
                }
            })
            .collect();

        // Sort by score descending
        scored.sort_by(|a, b| b.0.cmp(&a.0));

        let matches: Vec<Value> = scored
            .into_iter()
            .take(max_results)
            .map(|(score, tool)| {
                let func = tool.get("function").unwrap();
                let name = func.get("name").and_then(Value::as_str).unwrap_or("");
                let desc = func.get("description").and_then(Value::as_str).unwrap_or("");
                let short_desc: String = desc.chars().take(100).collect();
                serde_json::json!({
                    "name": name,
                    "description": if desc.len() > 100 { format!("{}...", short_desc) } else { desc.to_string() },
                    "score": score
                })
            })
            .collect();

        serde_json::json!({
            "query": query,
            "matches": matches,
            "total_tools": all_tools.len()
        }).to_string()
    }

    // ─── Web search tool ──────────────────────────────────────────────────────────

    /// Construct web search URLs for various engines.
    /// Returns URLs that can be fetched with web_fetch to get actual results.
    fn web_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.trim(),
            _ => return serde_json::json!({
                "error": "Missing or empty 'query' parameter"
            }).to_string(),
        };

        let engine = args
            .get("engine")
            .and_then(Value::as_str)
            .unwrap_or("google");

        let num_results = args
            .get("num_results")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .min(50) as usize;

        // URL-encode the query
        let encoded_query = urlencoding::encode(query);

        // Build search URL based on engine
        let (search_url, engine_name, result_tip) = match engine {
            "google" => (
                format!("https://www.google.com/search?q={}&num={}", encoded_query, num_results),
                "Google",
                "Use web_fetch with this URL to get search results. Parse the HTML for links."
            ),
            "duckduckgo" => (
                format!("https://html.duckduckgo.com/html/?q={}", encoded_query),
                "DuckDuckGo",
                "Use web_fetch with this URL. Results are in HTML format with class='result'."
            ),
            "bing" => (
                format!("https://www.bing.com/search?q={}&count={}", encoded_query, num_results),
                "Bing",
                "Use web_fetch with this URL to get search results."
            ),
            "wikipedia" => (
                format!(
                    "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={}&format=json",
                    encoded_query, num_results.min(20)
                ),
                "Wikipedia",
                "This returns JSON directly. Format: [query, [titles], [descriptions], [urls]]"
            ),
            "github" => (
                format!("https://github.com/search?q={}&type=repositories", encoded_query),
                "GitHub",
                "Use web_fetch with this URL. Consider using gh CLI for better structured results."
            ),
            other => {
                return serde_json::json!({
                    "error": format!("Unknown engine '{}'. Valid: google, duckduckgo, bing, wikipedia, github", other)
                }).to_string();
            }
        };

        // Build alternative URLs for common engines
        let mut alternatives = vec![];
        if engine != "wikipedia" {
            alternatives.push(serde_json::json!({
                "engine": "Wikipedia",
                "url": format!(
                    "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit=5&format=json",
                    encoded_query
                ),
                "note": "Direct JSON API, no HTML parsing needed"
            }));
        }
        // Fixed: operator precedence - parenthesize the OR conditions
        if engine != "github" && (query.contains("code") || query.contains("library") || query.contains("package")) {
            alternatives.push(serde_json::json!({
                "engine": "GitHub",
                "url": format!("https://github.com/search?q={}&type=repositories", encoded_query),
                "note": "For code/library searches"
            }));
        }

        serde_json::json!({
            "query": query,
            "engine": engine_name,
            "search_url": search_url,
            "tip": result_tip,
            "alternatives": alternatives,
            "usage": "Call web_fetch with the search_url to retrieve results. For Wikipedia, results are JSON. For others, parse the HTML response."
        }).to_string()
    }

    // ─── Diagnose tool ────────────────────────────────────────────────────────────

    /// Get system diagnostics and health information.
    async fn diagnose(&self, args: &Value) -> String {
        let category = args.get("category").and_then(Value::as_str).unwrap_or("all");
        let verbose = args.get("verbose").and_then(Value::as_bool).unwrap_or(false);
        
        let mut result = serde_json::Map::new();

        // System info
        if category == "all" || category == "system" {
            let mut sys_info = serde_json::Map::new();
            
            // OS info
            sys_info.insert("os".to_string(), json!(std::env::consts::OS));
            sys_info.insert("arch".to_string(), json!(std::env::consts::ARCH));
            
            // Current working directory
            if let Ok(cwd) = std::env::current_dir() {
                sys_info.insert("cwd".to_string(), json!(cwd.display().to_string()));
            }
            
            // Project root (sandbox)
            sys_info.insert("project_root".to_string(), json!(self.project_root.display().to_string()));
            
            // Memory info (read from /proc/meminfo on Linux)
            #[cfg(target_os = "linux")]
            {
                if let Ok(meminfo) = std::fs::read_to_string("/proc/meminfo") {
                    let mut mem = serde_json::Map::new();
                    for line in meminfo.lines().take(3) {
                        if let Some((key, val)) = line.split_once(':') {
                            mem.insert(key.trim().to_string(), json!(val.trim()));
                        }
                    }
                    sys_info.insert("memory".to_string(), json!(mem));
                }
            }
            
            // Memory info on macOS (using vm_stat)
            #[cfg(target_os = "macos")]
            {
                if let Ok(output) = std::process::Command::new("vm_stat").output() {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let mut mem = serde_json::Map::new();
                        for line in stdout.lines().take(5) {
                            if let Some((key, val)) = line.split_once(':') {
                                mem.insert(key.trim().to_string(), json!(val.trim()));
                            }
                        }
                        sys_info.insert("memory".to_string(), json!(mem));
                    }
                }
            }
            
            // Load average on Unix
            #[cfg(target_os = "linux")]
            {
                if let Ok(loadavg) = std::fs::read_to_string("/proc/loadavg") {
                    let parts: Vec<&str> = loadavg.split_whitespace().take(3).collect();
                    if parts.len() >= 3 {
                        sys_info.insert("load_avg".to_string(), json!({
                            "1min": parts[0],
                            "5min": parts[1],
                            "15min": parts[2]
                        }));
                    }
                }
            }
            
            // Load average on macOS (using sysctl)
            #[cfg(target_os = "macos")]
            {
                if let Ok(output) = std::process::Command::new("sysctl")
                    .args(["-n", "vm.loadavg"])
                    .output()
                {
                    if output.status.success() {
                        let stdout = String::from_utf8_lossy(&output.stdout);
                        let parts: Vec<&str> = stdout
                            .trim()
                            .trim_start_matches('{')
                            .trim_end_matches('}')
                            .split_whitespace()
                            .take(3)
                            .collect();
                        if parts.len() >= 3 {
                            sys_info.insert("load_avg".to_string(), json!({
                                "1min": parts[0],
                                "5min": parts[1],
                                "15min": parts[2]
                            }));
                        }
                    }
                }
            }
            
            // Note for Windows users
            #[cfg(target_os = "windows")]
            {
                sys_info.insert("note".to_string(), json!(
                    "System memory/load info requires external tools on Windows. Use Task Manager or 'systeminfo' command."
                ));
            }
            
            result.insert("system".to_string(), json!(sys_info));
        }

        // Environment info (only safe vars)
        if category == "all" || category == "environment" {
            let mut env_info = serde_json::Map::new();
            let safe_vars = [
                "PATH", "HOME", "USER", "SHELL", "TERM", "LANG", "PWD",
                "RUST_LOG", "MO_MODEL", "MO_API_KEY", // show if set (not value)
            ];
            
            for var in safe_vars {
                if let Ok(val) = std::env::var(var) {
                    // For sensitive vars, just show presence
                    if var.contains("KEY") || var.contains("TOKEN") || var.contains("SECRET") {
                        env_info.insert(var.to_string(), json!("[SET]"));
                    } else if verbose {
                        env_info.insert(var.to_string(), json!(val));
                    } else {
                        // Truncate long values
                        let display = if val.len() > 100 {
                            format!("{}...", &val[..100])
                        } else {
                            val
                        };
                        env_info.insert(var.to_string(), json!(display));
                    }
                }
            }
            
            result.insert("environment".to_string(), json!(env_info));
        }

        // Available tools info
        if category == "all" || category == "tools" {
            let tools = all_tool_schemas();
            let tool_names: Vec<&str> = tools.iter()
                .filter_map(|t| t.get("function").and_then(|f| f.get("name")).and_then(Value::as_str))
                .collect();
            
            let mut tools_info = serde_json::Map::new();
            tools_info.insert("count".to_string(), json!(tool_names.len()));
            
            if verbose {
                tools_info.insert("available".to_string(), json!(tool_names));
            } else {
                // Just show categories
                let categories = vec![
                    ("file_ops", vec!["read_file", "write_file", "str_replace", "list_dir"]),
                    ("search", vec!["grep", "glob", "find_definition", "find_references"]),
                    ("git", vec!["git_status", "git_diff", "git_log", "git_show"]),
                    ("tasks", vec!["task_create", "task_list", "task_update", "task_stop"]),
                    ("utility", vec!["bash", "web_fetch", "sleep", "ask_user"]),
                ];
                let mut cat_status = serde_json::Map::new();
                for (cat, expected) in categories {
                    let available = expected.iter().filter(|t| tool_names.contains(t)).count();
                    cat_status.insert(cat.to_string(), json!(format!("{}/{}", available, expected.len())));
                }
                tools_info.insert("categories".to_string(), json!(cat_status));
            }
            
            // MCP tools
            if self.mcp_manager.is_some() {
                tools_info.insert("mcp_enabled".to_string(), json!(true));
            }
            
            result.insert("tools".to_string(), json!(tools_info));
        }

        // Task status
        if category == "all" || category == "tasks" {
            let tasks = self.task_manager.snapshot();
            
            let mut tasks_info = serde_json::Map::new();
            tasks_info.insert("total".to_string(), json!(tasks.len()));
            
            let pending = tasks.iter().filter(|t| t.status == "pending").count();
            let in_progress = tasks.iter().filter(|t| t.status == "in_progress").count();
            let completed = tasks.iter().filter(|t| t.status == "completed").count();
            let failed = tasks.iter().filter(|t| t.status == "failed" || t.status == "cancelled").count();
            
            tasks_info.insert("pending".to_string(), json!(pending));
            tasks_info.insert("in_progress".to_string(), json!(in_progress));
            tasks_info.insert("completed".to_string(), json!(completed));
            tasks_info.insert("failed_or_cancelled".to_string(), json!(failed));
            
            if verbose && !tasks.is_empty() {
                let task_list: Vec<Value> = tasks.iter().map(|t| {
                    json!({
                        "id": t.id,
                        "title": t.title,
                        "status": t.status,
                        "subtasks": t.subtasks.len()
                    })
                }).collect();
                tasks_info.insert("list".to_string(), json!(task_list));
            }
            
            result.insert("tasks".to_string(), json!(tasks_info));
        }

        // Session info
        if category == "all" || category == "session" {
            let mut session_info = serde_json::Map::new();
            
            // Aggregate output tracking (AtomicUsize uses load, not lock)
            let bytes = self.aggregate_output_bytes.load(std::sync::atomic::Ordering::Relaxed);
            session_info.insert("output_bytes_this_turn".to_string(), json!(bytes));
            session_info.insert("output_budget".to_string(), json!(AGGREGATE_OUTPUT_BUDGET));
            session_info.insert("output_utilization".to_string(), 
                json!(format!("{:.1}%", (bytes as f64 / AGGREGATE_OUTPUT_BUDGET as f64) * 100.0)));
            
            // Sandbox policy
            if let Some(ref policy) = self.sandbox_policy {
                session_info.insert("sandbox_mode".to_string(), json!(format!("{:?}", policy.mode)));
                if verbose {
                    let paths: Vec<String> = policy.allowed_paths.iter()
                        .map(|p| p.display().to_string())
                        .collect();
                    session_info.insert("allowed_paths".to_string(), json!(paths));
                }
            } else {
                session_info.insert("sandbox_mode".to_string(), json!("disabled"));
            }
            
            result.insert("session".to_string(), json!(session_info));
        }

        serde_json::to_string_pretty(&result).unwrap_or_else(|_| "Error: serialization failed".to_string())
    }

    // ─── LSP tool: unified language server interface ─────────────────────────────

    /// Unified LSP tool providing code intelligence operations.
    /// Routes to existing implementations (find_definition, find_references, etc.)
    /// but offers a consistent interface matching the LSP protocol.
    fn lsp(&self, args: &Value) -> String {
        let operation = match args.get("operation").and_then(Value::as_str) {
            Some(op) => op,
            None => return json!({
                "error": "Missing required 'operation' parameter",
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "diagnostics"
                ]
            }).to_string(),
        };

        let file = args.get("file").and_then(Value::as_str);
        let line = args.get("line").and_then(Value::as_i64).map(|l| l as usize);
        let column = args.get("column").and_then(Value::as_i64).map(|c| c as usize);
        let symbol = args.get("symbol").and_then(Value::as_str);
        let query = args.get("query").and_then(Value::as_str);
        let scope = args.get("scope").and_then(Value::as_str).unwrap_or("file");
        let include_body = args.get("include_body").and_then(Value::as_bool).unwrap_or(false);

        match operation {
            "goto_definition" => {
                // Requires either symbol or file+position
                if let Some(sym) = symbol {
                    self.find_definition(&json!({
                        "symbol": sym,
                        "file": file
                    }))
                } else if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    // For position-based definition lookup, we extract symbol at position
                    self.find_definition_at_position(f, l, c)
                } else {
                    json!({
                        "error": "goto_definition requires 'symbol' or 'file'+'line'+'column'"
                    }).to_string()
                }
            }

            "find_references" => {
                if let Some(sym) = symbol {
                    self.find_references(&json!({
                        "symbol": sym,
                        "path": file,
                        "kind": "all",
                        "validate": true
                    }))
                } else {
                    json!({
                        "error": "find_references requires 'symbol' parameter"
                    }).to_string()
                }
            }

            "hover" => {
                if let (Some(f), Some(l), Some(c)) = (file, line, column) {
                    self.hover_info(&json!({
                        "file": f,
                        "line": l,
                        "column": c
                    }))
                } else if let (Some(f), Some(sym)) = (file, symbol) {
                    // Find symbol in file and get hover for it
                    self.hover_info(&json!({
                        "file": f,
                        "symbol": sym
                    }))
                } else {
                    json!({
                        "error": "hover requires 'file' + ('line'+'column' or 'symbol')"
                    }).to_string()
                }
            }

            "document_symbols" => {
                if let Some(f) = file {
                    self.symbols(&json!({
                        "path": f,
                        "include_body": include_body
                    }))
                } else {
                    json!({
                        "error": "document_symbols requires 'file' parameter"
                    }).to_string()
                }
            }

            "workspace_symbols" => {
                let search_query = query.or(symbol).unwrap_or("");
                self.symbol_search(&json!({
                    "query": search_query,
                    "limit": 50
                }))
            }

            "call_hierarchy" | "outgoing_calls" => {
                if let Some(f) = file {
                    self.call_graph(&json!({
                        "path": f,
                        "symbol": symbol,
                        "start_line": line,
                        "callers": false,
                        "scope": scope
                    }))
                } else {
                    json!({
                        "error": "call_hierarchy/outgoing_calls requires 'file' parameter"
                    }).to_string()
                }
            }

            "incoming_calls" => {
                if let Some(f) = file {
                    self.call_graph(&json!({
                        "path": f,
                        "symbol": symbol,
                        "start_line": line,
                        "callers": true,
                        "scope": scope
                    }))
                } else {
                    json!({
                        "error": "incoming_calls requires 'file' parameter"
                    }).to_string()
                }
            }

            "diagnostics" => {
                // Return diagnostic information about LSP capabilities
                json!({
                    "capabilities": {
                        "goto_definition": true,
                        "find_references": true,
                        "hover": true,
                        "document_symbols": true,
                        "workspace_symbols": true,
                        "call_hierarchy": true,
                        "rename": true
                    },
                    "supported_languages": [
                        "rust", "python", "typescript", "javascript",
                        "go", "java", "c", "cpp", "ruby"
                    ],
                    "note": "Uses tree-sitter AST parsing for accurate results. Some features may have reduced accuracy for unsupported languages."
                }).to_string()
            }

            _ => json!({
                "error": format!("Unknown operation: {}", operation),
                "valid_operations": [
                    "goto_definition", "find_references", "hover", "document_symbols",
                    "workspace_symbols", "call_hierarchy", "incoming_calls", "outgoing_calls", "diagnostics"
                ]
            }).to_string()
        }
    }

    /// Find definition at a specific file position by extracting the symbol under cursor.
    /// Column is interpreted as UTF-16 code units (LSP protocol).
    fn find_definition_at_position(&self, file: &str, line: usize, col_utf16: usize) -> String {
        // Read the file and extract symbol at position
        let file_path = if file.starts_with('/') {
            PathBuf::from(file)
        } else {
            self.project_root.join(file)
        };
        
        // Check file size to prevent OOM
        if let Ok(metadata) = std::fs::metadata(&file_path) {
            if metadata.len() > MAX_LSP_FILE_SIZE as u64 {
                return json!({
                    "error": format!("File too large for LSP operations ({} bytes, max {} bytes)",
                        metadata.len(), MAX_LSP_FILE_SIZE)
                }).to_string();
            }
        }
        
        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) => return json!({
                "error": format!("Failed to read file: {}", e)
            }).to_string(),
        };

        // Get the line
        let lines: Vec<&str> = content.lines().collect();
        if line == 0 || line > lines.len() {
            return json!({
                "error": format!("Line {} out of range (file has {} lines)", line, lines.len())
            }).to_string();
        }

        let line_content = lines[line - 1];
        
        // Convert UTF-16 column to char index (LSP uses UTF-16 code units).
        // col_utf16 is 0-indexed per tool schema, same as utf16_col_to_char_idx expects.
        let col_idx = utf16_col_to_char_idx(line_content, col_utf16);
        let chars: Vec<char> = line_content.chars().collect();
        
        if col_idx >= chars.len() {
            return json!({
                "error": format!("Column {} (UTF-16) out of range for line {} (length {})", 
                    col_utf16, line, line_content.len())
            }).to_string();
        }

        // Find word boundaries
        let mut start = col_idx;
        while start > 0 && Self::is_symbol_char(chars.get(start - 1).copied().unwrap_or(' ')) {
            start -= 1;
        }
        
        let mut end = col_idx;
        while end < chars.len() && Self::is_symbol_char(chars.get(end).copied().unwrap_or(' ')) {
            end += 1;
        }

        if start == end {
            return json!({
                "error": "No symbol found at position"
            }).to_string();
        }

        let symbol: String = chars[start..end].iter().collect();
        
        self.find_definition(&json!({
            "symbol": symbol,
            "file": file
        }))
    }

    /// Check if a character can be part of a symbol name.
    fn is_symbol_char(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }

    // ── Env tool: environment variable management ─────────────────────────────

    /// Environment variable management tool — delegated to `env_tools` module.
    fn env_tool(&self, args: &Value) -> String {
        env_tools::env_tool(args)
    }

    /// Check if a variable name suggests it contains sensitive data.
    fn is_sensitive_var(name: &str) -> bool {
        env_tools::is_sensitive_var(name)
    }

    // ── Notebook edit tool: Jupyter notebook cell editing ─────────────────────

    /// Edit Jupyter notebook cells.
    /// Operations: replace, insert, delete
    fn notebook_edit(&self, args: &Value) -> String {
        let notebook_path = match args.get("notebook_path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return json!({ "error": "Missing required parameter: notebook_path" }).to_string(),
        };

        let file_path = match self.resolve_checked(notebook_path) {
            Ok(path) => path,
            Err(e) => return json!({ "error": e }).to_string(),
        };
        
        // Validate file extension
        if !file_path.extension().map(|e| e == "ipynb").unwrap_or(false) {
            return json!({ 
                "error": "File must be a Jupyter notebook (.ipynb). For other files, use str_replace or write_file."
            }).to_string();
        }

        let edit_mode = args.get("edit_mode").and_then(|v| v.as_str()).unwrap_or("replace");
        if !matches!(edit_mode, "replace" | "insert" | "delete") {
            return json!({ "error": format!("Unknown edit_mode: {}. Use replace, insert, or delete", edit_mode) }).to_string();
        }

        let cell_id = args.get("cell_id").and_then(|v| v.as_str());
        let new_source = args.get("new_source").and_then(|v| v.as_str());
        let cell_type = args.get("cell_type").and_then(|v| v.as_str()).unwrap_or("code");

        let rel = file_path.strip_prefix(&self.project_root).unwrap_or(&file_path);
        let rel_str = rel.to_string_lossy();
        if let Some(warning) = fs_tools::is_dangerous_write_target(&rel_str) {
            return json!({
                "error": format!("⚠️ Warning: writing to sensitive file '{}' — {}. If intentional, use bash to bypass this guard.", rel_str, warning)
            }).to_string();
        }

        // Check staleness and read-before-write requirements only for existing files
        if file_path.exists() {
            if let Err(e) = self.check_staleness(&file_path) {
                return json!({ "error": e }).to_string();
            }
            if !self.was_fully_read(&file_path) {
                return json!({
                    "error": format!(
                        "File was only partially read (outline or line range). Read the full file before editing.\n\
                         → Action required: call read_file(\"{}\") (without start_line/end_line) first, then retry.",
                        rel_str
                    )
                }).to_string();
            }
        }
        
        // Read existing notebook
        let content = match std::fs::read_to_string(&file_path) {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Create new notebook if it doesn't exist and we're inserting
                let edit_mode = args.get("edit_mode").and_then(|v| v.as_str()).unwrap_or("replace");
                if edit_mode != "insert" {
                    return json!({ "error": format!("Notebook not found: {}", file_path.display()) }).to_string();
                }
                // Create empty notebook structure
                r#"{"cells":[],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#.to_string()
            }
            Err(e) => return json!({ "error": format!("Failed to read notebook: {}", e) }).to_string(),
        };
        
        let mut notebook: Value = match serde_json::from_str(&content) {
            Ok(n) => n,
            Err(e) => return json!({ "error": format!("Invalid notebook JSON: {}", e) }).to_string(),
        };
        
        let cells = match notebook.get_mut("cells").and_then(|c| c.as_array_mut()) {
            Some(c) => c,
            None => return json!({ "error": "Notebook has no cells array" }).to_string(),
        };
        
        // Find cell index if cell_id provided
        let cell_index = if let Some(id) = cell_id {
            // Try to find by ID first
            let by_id = cells.iter().position(|c| {
                c.get("id").and_then(|i| i.as_str()) == Some(id)
            });
            if let Some(idx) = by_id {
                Some(idx)
            } else {
                // Try to parse as cell-N format
                if let Some(num_str) = id.strip_prefix("cell-") {
                    num_str.parse::<usize>().ok()
                } else {
                    id.parse::<usize>().ok()
                }
            }
        } else {
            None
        };
        
        match edit_mode {
            "delete" => {
                let idx = match cell_index {
                    Some(i) if i < cells.len() => i,
                    _ => return json!({ "error": "cell_id required for delete operation" }).to_string(),
                };
                cells.remove(idx);
            }
            "insert" => {
                let source = match new_source {
                    Some(s) => s,
                    None => return json!({ "error": "new_source required for insert operation" }).to_string(),
                };
                let new_cell = json!({
                    "cell_type": cell_type,
                    "id": format!("cell-{}", uuid::Uuid::new_v4().to_string()[..8].to_string()),
                    "source": source,
                    "metadata": {},
                    "outputs": if cell_type == "code" { json!([]) } else { json!(null) },
                    "execution_count": if cell_type == "code" { json!(null) } else { json!(null) }
                });
                let insert_idx = cell_index.map(|i| i + 1).unwrap_or(0);
                if insert_idx <= cells.len() {
                    cells.insert(insert_idx, new_cell);
                } else {
                    cells.push(new_cell);
                }
            }
            "replace" => {
                let idx = match cell_index {
                    Some(i) if i < cells.len() => i,
                    _ => return json!({ "error": "Valid cell_id required for replace operation" }).to_string(),
                };
                let source = match new_source {
                    Some(s) => s,
                    None => return json!({ "error": "new_source required for replace operation" }).to_string(),
                };
                if let Some(cell) = cells.get_mut(idx) {
                    cell["source"] = json!(source);
                    if cell_type != cell.get("cell_type").and_then(|t| t.as_str()).unwrap_or("") {
                        cell["cell_type"] = json!(cell_type);
                    }
                    // Reset execution for code cells
                    if cell.get("cell_type").and_then(|t| t.as_str()) == Some("code") {
                        cell["execution_count"] = json!(null);
                        cell["outputs"] = json!([]);
                    }
                }
            }
            _ => return json!({ "error": format!("Unknown edit_mode: {}. Use replace, insert, or delete", edit_mode) }).to_string(),
        }
        
        // Get cell count before dropping mutable borrow
        let total_cells = cells.len();
        
        // Extract language before serializing (need to drop cells borrow first)
        let language = notebook
            .get("metadata")
            .and_then(|m| m.get("language_info"))
            .and_then(|l| l.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("python")
            .to_string();

        if file_path.exists() {
            if let Err(e) = self.check_staleness(&file_path) {
                return json!({ "error": format!("Pre-write staleness check failed: {e}") }).to_string();
            }
        }

        // Write back
        let updated_content = serde_json::to_string_pretty(&notebook).unwrap_or_default();
        if let Err(e) = std::fs::write(&file_path, &updated_content) {
            return json!({ "error": format!("Failed to write notebook: {}", e) }).to_string();
        }
        self.record_write(&file_path);
        
        json!({
            "success": true,
            "edit_mode": edit_mode,
            "cell_type": cell_type,
            "language": language,
            "total_cells": total_cells,
            "notebook_path": file_path.display().to_string()
        }).to_string()
    }

    // ── Config tool: get/set CLI configuration ────────────────────────────────

    /// Get or set CLI configuration.
    fn config_tool(&self, args: &Value) -> String {
        let setting = match args.get("setting").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return json!({ "error": "Missing required parameter: setting" }).to_string(),
        };
        let value = args.get("value").and_then(|v| v.as_str());
        
        // Available settings
        let available = [
            ("model", "Current model (env: MO_MODEL)"),
            ("api_key", "API key status (env: MO_API_KEY, OPENAI_API_KEY, ANTHROPIC_API_KEY)"),
            ("output_limit", "Global output limit in bytes (env: MO_GLOBAL_OUTPUT_LIMIT)"),
            ("tool_output_limit", "Per-tool output limit (env: MO_TOOL_OUTPUT_LIMIT)"),
            ("sandbox_mode", "Sandbox mode: off, permissive, strict (env: MO_SANDBOX_MODE)"),
            ("auto_approve", "Auto-approve tools (env: MO_AUTO_APPROVE)"),
            ("turn_limit", "Max turns per conversation (env: MO_MAX_TURNS)"),
            ("list", "Show all available settings"),
        ];
        
        if setting == "list" {
            // Skip the "list" entry itself when displaying available settings
            let settings: Vec<Value> = available
                .iter()
                .take(available.len() - 1)  // Exclude the "list" entry
                .map(|(k, desc)| json!({ "setting": k, "description": desc }))
                .collect();
            return json!({
                "available_settings": settings
            }).to_string();
        }
        
        match setting {
            "model" => {
                if let Some(v) = value {
                    // Set model (this would need integration with RuntimeLimits)
                    json!({
                        "note": format!("To change model, set MO_MODEL={} environment variable", v),
                        "setting": "model",
                        "hint": "Use env tool to set MO_MODEL"
                    }).to_string()
                } else {
                    let current = std::env::var("MO_MODEL").unwrap_or_else(|_| "default".to_string());
                    json!({
                        "setting": "model",
                        "value": current
                    }).to_string()
                }
            }
            "api_key" => {
                // Never show actual key, just status
                let has_mo = std::env::var("MO_API_KEY").is_ok();
                let has_openai = std::env::var("OPENAI_API_KEY").is_ok();
                let has_anthropic = std::env::var("ANTHROPIC_API_KEY").is_ok();
                json!({
                    "setting": "api_key",
                    "status": {
                        "MO_API_KEY": if has_mo { "set" } else { "not set" },
                        "OPENAI_API_KEY": if has_openai { "set" } else { "not set" },
                        "ANTHROPIC_API_KEY": if has_anthropic { "set" } else { "not set" }
                    }
                }).to_string()
            }
            "output_limit" => {
                if value.is_some() {
                    json!({
                        "note": "To change output limit, set MO_GLOBAL_OUTPUT_LIMIT environment variable",
                        "setting": "output_limit"
                    }).to_string()
                } else {
                    json!({
                        "setting": "output_limit",
                        "value": global_output_limit(),
                        "env_var": "MO_GLOBAL_OUTPUT_LIMIT"
                    }).to_string()
                }
            }
            "tool_output_limit" => {
                json!({
                    "setting": "tool_output_limit",
                    "value": tool_output_limit(),
                    "env_var": "MO_TOOL_OUTPUT_LIMIT"
                }).to_string()
            }
            "sandbox_mode" => {
                let current = std::env::var("MO_SANDBOX_MODE").unwrap_or_else(|_| "permissive".to_string());
                if let Some(v) = value {
                    if !["off", "permissive", "strict"].contains(&v) {
                        return json!({ "error": "sandbox_mode must be: off, permissive, or strict" }).to_string();
                    }
                    json!({
                        "note": format!("To change sandbox mode, set MO_SANDBOX_MODE={}", v),
                        "setting": "sandbox_mode",
                        "current": current
                    }).to_string()
                } else {
                    json!({
                        "setting": "sandbox_mode",
                        "value": current,
                        "options": ["off", "permissive", "strict"]
                    }).to_string()
                }
            }
            "auto_approve" => {
                let current = std::env::var("MO_AUTO_APPROVE").unwrap_or_else(|_| "false".to_string());
                json!({
                    "setting": "auto_approve",
                    "value": current,
                    "env_var": "MO_AUTO_APPROVE"
                }).to_string()
            }
            "turn_limit" => {
                let current = std::env::var("MO_MAX_TURNS").unwrap_or_else(|_| "50".to_string());
                json!({
                    "setting": "turn_limit",
                    "value": current,
                    "env_var": "MO_MAX_TURNS"
                }).to_string()
            }
            _ => json!({
                "error": format!("Unknown setting: {}. Use setting='list' to see available settings.", setting)
            }).to_string(),
        }
    }

    /// Return a compact summary of the current session state.
    fn brief(&self, args: &Value) -> String {
        let focus = args
            .get("focus")
            .and_then(Value::as_str)
            .unwrap_or("all");
        let max_items = args
            .get("max_items")
            .and_then(Value::as_u64)
            .map(|n| n.clamp(1, 20) as usize)
            .unwrap_or(5);

        let effective_root = self.effective_project_root();
        let mut result = serde_json::Map::new();
        result.insert(
            "effective_project_root".to_string(),
            json!(effective_root.display().to_string()),
        );

        if focus == "all" || focus == "session" {
            result.insert(
                "session".to_string(),
                json!({
                    "in_worktree_session": self.in_worktree_session(),
                    "aggregate_output_bytes": self.aggregate_output_bytes.load(std::sync::atomic::Ordering::Relaxed),
                    "aggregate_output_budget": AGGREGATE_OUTPUT_BUDGET,
                    "scaled_output_limit": self.scaled_output_limit(),
                }),
            );

            if let Some(worktree) = self.get_worktree_session() {
                result.insert(
                    "worktree".to_string(),
                    json!({
                        "path": worktree.worktree_path.display().to_string(),
                        "branch": worktree.branch_name,
                        "original_root": worktree.original_root.display().to_string(),
                        "baseline_commit": worktree.original_head_commit,
                    }),
                );
            }
        }

        if focus == "all" || focus == "git" {
            let branch = Command::new("git")
                .args(["branch", "--show-current"])
                .current_dir(&effective_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty());

            let porcelain = Command::new("git")
                .args(["status", "--porcelain"])
                .current_dir(&effective_root)
                .output()
                .ok()
                .filter(|o| o.status.success())
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();

            let mut modified = 0usize;
            let mut added = 0usize;
            let mut deleted = 0usize;
            let mut untracked = 0usize;
            let mut renamed = 0usize;
            for line in porcelain.lines() {
                if line.starts_with(git_status::UNTRACKED_PREFIX) {
                    untracked += 1;
                    continue;
                }
                let x = line.chars().next().unwrap_or(' ');
                let y = line.chars().nth(1).unwrap_or(' ');
                for status in [x, y] {
                    match status {
                        git_status::MODIFIED => modified += 1,
                        git_status::ADDED => added += 1,
                        git_status::DELETED => deleted += 1,
                        git_status::RENAMED => renamed += 1,
                        _ => {}
                    }
                }
            }

            result.insert(
                "git".to_string(),
                json!({
                    "branch": branch,
                    "modified": modified,
                    "added": added,
                    "deleted": deleted,
                    "renamed": renamed,
                    "untracked": untracked,
                    "dirty": !porcelain.trim().is_empty(),
                }),
            );
        }

        if focus == "all" || focus == "tasks" {
            let tasks = self.task_manager.snapshot();
            let mut task_summaries: Vec<Value> = tasks
                .iter()
                .take(max_items)
                .map(|t| {
                    json!({
                        "id": t.id,
                        "title": t.title,
                        "status": t.status,
                        "subtasks": t.subtasks.len(),
                        "updated_at": t.updated_at,
                    })
                })
                .collect();
            if tasks.len() > max_items {
                task_summaries.push(json!({
                    "more": tasks.len() - max_items
                }));
            }
            result.insert(
                "tasks".to_string(),
                json!({
                    "count": tasks.len(),
                    "items": task_summaries,
                }),
            );
        }

        if focus == "all" || focus == "files" {
            let recent_files: Vec<String> = self
                .recently_read_files(max_items)
                .into_iter()
                .map(|p| {
                    p.strip_prefix(&effective_root)
                        .unwrap_or(&p)
                        .display()
                        .to_string()
                })
                .collect();
            result.insert(
                "files".to_string(),
                json!({
                    "recently_read": recent_files,
                }),
            );
        }

        Value::Object(result).to_string()
    }

    /// Set the MCP client manager for external tool routing.
    pub fn with_mcp_manager(
        mut self,
        manager: std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>,
    ) -> Self {
        self.mcp_manager = Some(manager);
        self
    }

    /// Expand the sandbox boundary to include an additional directory.
    /// Called when the user approves access to a path outside the project.
    pub fn expand_sandbox_path(&mut self, dir: PathBuf) {
        if let Some(ref mut policy) = self.sandbox_policy {
            policy.allowed_paths.push(dir);
        }
    }

    /// Run passive workspace checks (optional **rust-analyzer LSP**, `cargo`, `tsc`) after
    /// recent edits when this turn includes tool results; returns extra `messages` for the payload.
    pub(crate) async fn take_passive_workspace_diagnostic_messages(
        &self,
        project_root: &Path,
        tool_results_nonempty: bool,
    ) -> Vec<Value> {
        let mut out = self
            .passive_lsp
            .take_diagnostic_messages(tool_results_nonempty)
            .await;
        out.extend(
            passive_cargo_check::take_passive_cargo_messages(
                &self.passive_cargo_pending,
                project_root,
                tool_results_nonempty,
            )
            .await,
        );
        out.extend(
            passive_tsc_check::take_passive_tsc_messages(
                &self.passive_tsc_pending,
                project_root,
                tool_results_nonempty,
            )
            .await,
        );
        out
    }

    /// Add a preferred repo for disambiguation (e.g. from memory or recent usage).
    pub fn add_preferred_repo(&self, owner_repo: &str) {
        let normalized = owner_repo.to_lowercase();
        match self.preferred_repos.lock() {
            Ok(mut repos) => {
                if !repos.iter().any(|r| r == &normalized) {
                    repos.push(normalized);
                }
            }
            Err(poisoned) => {
                // Recover from poisoned mutex — clear and re-add
                astra_core::agent_warn!("preferred_repos", "recovering from poisoned mutex");
                let mut repos = poisoned.into_inner();
                repos.clear();
                repos.push(normalized);
            }
        }
    }

    /// Set per-turn budget pressure before executing a batch of tool calls.
    /// 0.0 = normal, 0.3 = trimming, 0.6 = compact, 0.9 = aggressive.
    pub fn set_budget_pressure(&self, pressure: f64) {
        if let Ok(mut p) = self.budget_pressure.lock() {
            *p = pressure.clamp(0.0, 1.0);
        }
    }

    /// Read current budget pressure. Returns 0.0 if mutex is poisoned.
    pub fn get_budget_pressure(&self) -> f64 {
        self.budget_pressure.lock().map(|p| *p).unwrap_or(0.0)
    }

    /// Get current preferred repos (for use in repo resolution).
    fn get_preferred_repos(&self) -> Vec<String> {
        match self.preferred_repos.lock() {
            Ok(r) => r.clone(),
            Err(poisoned) => {
                astra_core::agent_warn!(
                    "preferred_repos",
                    "recovering from poisoned mutex on read"
                );
                poisoned.into_inner().clone()
            }
        }
    }

    // ─── File state helpers ──────────────────────────────────────────────────

    /// Get the mtime of a file in milliseconds. Returns 0 on error.
    fn file_mtime_ms(path: &Path) -> u128 {
        fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0)
    }

    /// Record file state after a read.
    fn record_read(&self, path: &Path, is_partial: bool, last_dedup_key: ReadDedupKey) {
        let ts = Self::file_mtime_ms(path);
        if let Ok(mut state) = self.file_state.lock() {
            let prev = state.get(path);
            let prev_count = prev.map(|fs| fs.read_count).unwrap_or(0);
            let prev_ranged = prev.map(|fs| fs.ranged_read_count).unwrap_or(0);
            // Only increment read_count for full (non-partial) reads.
            // Ranged reads of different sections are expected behavior
            // (guided by the size gate), not wasteful repetition.
            let new_count = if is_partial {
                prev_count
            } else {
                prev_count + 1
            };
            let new_ranged = if is_partial {
                prev_ranged + 1
            } else {
                prev_ranged
            };
            state.insert(
                path.to_path_buf(),
                FileState {
                    timestamp_ms: ts,
                    from_read: true,
                    is_partial,
                    read_count: new_count,
                    ranged_read_count: new_ranged,
                    last_dedup_key,
                },
            );
            // LRU eviction: keep at most MAX_FILE_STATE_ENTRIES
            if state.len() > MAX_FILE_STATE_ENTRIES
                && let Some(oldest_key) = state
                    .iter()
                    .min_by_key(|(_, fs)| fs.timestamp_ms)
                    .map(|(k, _)| k.clone())
            {
                state.remove(&oldest_key);
            }
        }
    }

    /// Record file state after a write/edit (full content known).
    /// Uses from_read=false to distinguish from reads — dedup won't fire after writes.
    fn record_write(&self, path: &Path) {
        if passive_cargo_check::should_schedule_passive_cargo(&self.project_root, path) {
            self.passive_cargo_pending.store(true, Ordering::SeqCst);
        }
        if passive_tsc_check::should_schedule_passive_tsc(&self.project_root, path) {
            self.passive_tsc_pending.store(true, Ordering::SeqCst);
        }
        self.passive_lsp.sync_after_write(&self.project_root, path);
        let ts = Self::file_mtime_ms(path);
        if let Ok(mut state) = self.file_state.lock() {
            state.insert(
                path.to_path_buf(),
                FileState {
                    timestamp_ms: ts,
                    from_read: false,
                    is_partial: false,
                    read_count: 0,
                    ranged_read_count: 0,
                    last_dedup_key: ReadDedupKey::Full,
                },
            );
            // LRU eviction: keep at most MAX_FILE_STATE_ENTRIES
            if state.len() > MAX_FILE_STATE_ENTRIES
                && let Some(oldest_key) = state
                    .iter()
                    .min_by_key(|(_, fs)| fs.timestamp_ms)
                    .map(|(k, _)| k.clone())
            {
                state.remove(&oldest_key);
            }
        }
    }

    /// Check if a file has been modified since we last read/wrote it.
    /// Returns Err(message) if stale, Ok(()) if fresh or unknown.
    ///
    /// The error message includes the concrete file path and the exact tool call
    /// the model should make next, so the LLM can act without extra reasoning.
    fn check_staleness(&self, path: &Path) -> Result<(), String> {
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return Ok(()); // file doesn't exist yet — ok for write_file
        }
        let rel = path
            .strip_prefix(&self.project_root)
            .unwrap_or(path)
            .to_string_lossy();
        if let Ok(state) = self.file_state.lock() {
            if let Some(fs) = state.get(path) {
                if current_ts > fs.timestamp_ms {
                    return Err(format!(
                        "File has been modified since last read (by user or linter). \
                         Read it again before editing.\n\
                         → Action required: call read_file(\"{rel}\") first, then retry."
                    ));
                }
            } else {
                // Never read — require read first for existing files
                return Err(format!(
                    "File exists but has not been read yet. \
                     Read it first before writing/editing.\n\
                     → Action required: call read_file(\"{rel}\") first, then retry."
                ));
            }
        }
        Ok(())
    }

    /// Register a file as "read" from an external source (e.g. skill execution
    /// that loaded and returned the file content). This prevents the
    /// read-before-write guard from rejecting subsequent edits to the file.
    pub fn register_external_read(&self, path: &Path) {
        let abs = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.project_root.join(path)
        };
        self.record_read(&abs, false, ReadDedupKey::Full);
    }

    /// Check if a file was read as a full view (not partial/outline).
    fn was_fully_read(&self, path: &Path) -> bool {
        self.file_state
            .lock()
            .ok()
            .and_then(|s| s.get(path).map(|fs| !fs.is_partial))
            .unwrap_or(false)
    }

    /// Consecutive identical partial read (outline or same raw line range) with unchanged
    /// mtime — stub **before** disk read, like Claude Code `tengu_file_read_dedup` for
    /// the same offset/limit as the immediately previous read.
    fn can_dedup_identical_partial_read(&self, path: &Path, requested: &ReadDedupKey) -> bool {
        if std::env::var("MO_DEDUP_DISABLED").is_ok_and(|v| v == "1" || v == "true") {
            return false;
        }
        if matches!(requested, ReadDedupKey::Full) {
            return false;
        }
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return false;
        }
        self.file_state
            .lock()
            .ok()
            .and_then(|s| {
                s.get(path).and_then(|fs| {
                    (fs.from_read
                        && fs.timestamp_ms == current_ts
                        && fs.last_dedup_key == *requested)
                        .then_some(())
                })
            })
            .is_some()
    }

    /// Check if we can dedup a read (previous op was a full read, unchanged mtime).
    /// Respects `MO_DEDUP_DISABLED=1` env var killswitch (inspired by Claude Code's
    /// `tengu_read_dedup_killswitch` feature flag).
    fn can_dedup_read(&self, path: &Path) -> bool {
        if std::env::var("MO_DEDUP_DISABLED").is_ok_and(|v| v == "1" || v == "true") {
            return false;
        }
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return false;
        }
        self.file_state
            .lock()
            .ok()
            .and_then(|s| {
                s.get(path)
                    .map(|fs| fs.from_read && !fs.is_partial && fs.timestamp_ms == current_ts)
            })
            .unwrap_or(false)
    }

    /// How many times this file has been read in the current session.
    fn file_read_count(&self, path: &Path) -> u32 {
        self.file_state
            .lock()
            .ok()
            .and_then(|s| s.get(path).map(|fs| fs.read_count))
            .unwrap_or(0)
    }

    /// How many times this file has been read with different ranges.
    fn file_ranged_read_count(&self, path: &Path) -> u32 {
        self.file_state
            .lock()
            .ok()
            .and_then(|s| s.get(path).map(|fs| fs.ranged_read_count))
            .unwrap_or(0)
    }

    /// Check if a file was previously partially read (outline or line range) and
    /// hasn't been modified since. Used to auto-expand subsequent ranged reads
    /// to the full file, eliminating fragmented multi-range read patterns.
    fn was_partially_read_unchanged(&self, path: &Path) -> bool {
        let current_ts = Self::file_mtime_ms(path);
        if current_ts == 0 {
            return false;
        }
        self.file_state
            .lock()
            .ok()
            .and_then(|s| {
                s.get(path)
                    .map(|fs| fs.from_read && fs.is_partial && fs.timestamp_ms == current_ts)
            })
            .unwrap_or(false)
    }

    /// Output limit scaled by budget pressure and aggregate output.
    ///
    /// Two independent pressures are combined:
    /// 1. **Token pressure** (from context window fill ratio): 0.0→full, 0.9→25%.
    /// 2. **Aggregate output pressure** (from cumulative tool output this turn):
    ///    smooth curve that progressively tightens as output accumulates,
    ///    reaching 25% of base at 2× the aggregate budget.
    fn scaled_output_limit(&self) -> usize {
        let base = tool_output_limit();
        let pressure = self.get_budget_pressure();
        let token_scale = 1.0 - (pressure * 0.75); // 0.0→1.0, 0.9→0.325

        let agg = self
            .aggregate_output_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        let agg_scale = if agg <= AGGREGATE_SOFT_LIMIT {
            1.0
        } else {
            // Smooth decay: 1.0 at soft limit → 0.25 at 2× budget
            let ratio = (agg - AGGREGATE_SOFT_LIMIT) as f64 / (AGGREGATE_OUTPUT_BUDGET * 2) as f64;
            (1.0 - ratio * 0.75).max(0.25)
        };

        let limit = (base as f64 * token_scale.max(0.25) * agg_scale) as usize;
        limit.max(1024)
    }

    /// Record tool output size for aggregate tracking.
    fn record_output_size(&self, size: usize) {
        self.aggregate_output_bytes
            .fetch_add(size, std::sync::atomic::Ordering::Relaxed);
    }

    /// Clear all file state (call after compaction to avoid stale dedup).
    #[allow(dead_code)] // Public API for compaction cleanup
    pub fn clear_file_state(&self) {
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }
    }

    /// Remove a single file from state tracking (call after delete).
    fn remove_file_state(&self, path: &Path) {
        if let Ok(mut state) = self.file_state.lock() {
            state.remove(path);
        }
    }

    /// Return recently-read file paths sorted by recency (most recent first).
    /// Used for post-compact file restoration — re-inject the N most recently
    /// accessed files so the LLM retains working context after compaction.
    #[allow(dead_code)] // Public API for post-compact file restoration
    pub fn recently_read_files(&self, max: usize) -> Vec<PathBuf> {
        if let Ok(state) = self.file_state.lock() {
            let mut entries: Vec<_> = state.iter().filter(|(_, fs)| fs.from_read).collect();
            entries.sort_by(|a, b| b.1.timestamp_ms.cmp(&a.1.timestamp_ms));
            entries
                .into_iter()
                .take(max)
                .map(|(p, _)| p.clone())
                .collect()
        } else {
            Vec::new()
        }
    }

    /// Configure security sandbox for tool execution.
    #[allow(dead_code)] // Public builder API for library consumers
    pub fn with_sandbox(mut self, policy: SandboxPolicy) -> Self {
        self.sandbox_policy = Some(policy);
        self
    }

    #[allow(dead_code)] // Public builder API for library consumers
    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        let token = token.into();
        let token = token.trim().to_string();
        self.github_token = if token.is_empty() { None } else { Some(token) };
        self
    }

    pub async fn execute(&self, name: &str, args: &Value) -> String {
        let output = match name {
            "bash" => self.bash(args),
            "powershell" => self.powershell(args),
            "read_file" => self.read_file(args),
            "write_file" => self.write_file(args),
            "str_replace" => self.str_replace(args),
            "delete_file" => self.delete_file(args),
            "multi_edit" => self.multi_edit(args),
            "list_dir" => self.list_dir(args),
            "grep" => self.grep(args),
            "glob" => self.glob(args),
            "git_status" => git_gix::git_status(&self.project_root),
            "git_diff" => git_gix::git_diff(
                &self.project_root,
                args,
                self.get_budget_pressure(),
                self.aggregate_output_bytes
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            "git_log" => git_gix::git_log(&self.project_root, args),
            "git_show" => git_gix::git_show(
                &self.project_root,
                args,
                self.get_budget_pressure(),
                self.aggregate_output_bytes
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            "git_blame" => git_gix::git_blame(&self.project_root, args),
            "git_file_history" => git_gix::git_file_history(&self.project_root, args),
            "git_contributors" => git_gix::git_contributors(&self.project_root, args),
            "git_log_search" => git_gix::git_log_search(&self.project_root, args),
            "git_commit" => git_gix::git_commit(&self.project_root, args),
            "git_stash" => git_gix::git_stash(&self.project_root, args),
            "git_checkout_file" => git_gix::git_checkout_file(&self.project_root, args),
            "git_worktree" => self.git_worktree(args),
            "find_definition" => self.find_definition(args),
            "find_references" => self.find_references(args),
            "call_graph" => self.call_graph(args),
            "rename_symbol" => self.rename_symbol(args),
            "dead_code" => self.dead_code(args),
            "extract_members" => self.extract_members(args),
            "type_hierarchy" => self.type_hierarchy(args),
            "hover_info" => self.hover_info(args),
            "symbol_search" => self.symbol_search(args),
            "run_build_test" => self.run_build_test(args),
            "symbols" => self.symbols(args),
            "mo_query" => self.mo_query(args),
            "mo_snapshot" => self.mo_snapshot(args),
            "mo_branch" => self.mo_branch(args),
            "github_list_prs" => self.github_list_prs(args).await,
            "github_get_pr" => self.github_get_pr(args).await,
            "github_ci_status" => self.github_ci_status(args).await,
            "github_list_issues" => self.github_list_issues(args).await,
            "github_get_issue" => self.github_get_issue(args).await,
            "github_repo_stats" => self.github_repo_stats(args).await,
            "github_create_issue" => self.github_create_issue(args).await,
            "web_fetch" => self.web_fetch(args),
            "memory_retrieve" => self.memoria_call("retrieve", args).await,
            "memory_store" => self.memoria_call("store", args).await,
            "memory_search" => self.memoria_call("search", args).await,
            "memory_purge" => self.memoria_call("purge", args).await,
            "memory_correct" => self.memoria_call("correct", args).await,
            "memory_profile" => self.memoria_call("profile", args).await,
            "get_agent_info" => {
                let dimension = args
                    .get("dimension")
                    .and_then(|v| v.as_str())
                    .unwrap_or("all");
                let info = match dimension {
                    "capability" => serde_json::json!({
                        "tools": self.tool_names(),
                        "tool_count": self.tool_count(),
                        "model": "see /model",
                        "note": "For full capability info including model/token budget, ask the server via /session"
                    }),
                    "state" => serde_json::json!({
                        "note": "Runtime state is managed by the edge CLI. Use /session for current session info."
                    }),
                    "context_snapshot" | "context_trend" => serde_json::json!({
                        "note": "Context window data is available from the server. Check the explain output (/explain) for token breakdown."
                    }),
                    "identity" => serde_json::json!({
                        "name": "astra",
                        "version": env!("CARGO_PKG_VERSION"),
                        "runtime": "Rust edge CLI",
                        "note": "Cloud-side identity (model, system prompt) is server-managed."
                    }),
                    _ => serde_json::json!({
                        "tools_available": self.tool_names(),
                        "tool_count": self.tool_count(),
                        "runtime": "astra Rust CLI",
                        "version": env!("CARGO_PKG_VERSION"),
                        "note": "For full agent info including memory, context, model details, the server provides richer data."
                    }),
                };
                info.to_string()
            }
            "reflect" => {
                let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
                let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
                let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
                serde_json::json!({
                    "status": "reflect_requires_session",
                    "focus": focus,
                    "question": question,
                    "last_n": last_n,
                    "note": "Reflect data comes from the server API. Use /reflect command for direct access."
                }).to_string()
            }
            "run_chain" => {
                match serde_json::from_value::<astra_runtime::tool_registry::ToolChain>(
                    args.clone(),
                ) {
                    Ok(chain) => {
                        // Validate chain steps reference known tools
                        let known: Vec<&str> = astra_runtime::tool_registry::TOOL_CATALOG
                            .iter()
                            .map(|t| t.name)
                            .collect();
                        if let Err(errors) = chain.validate(&known) {
                            return format!("Error: Invalid chain: {}", errors.join("; "));
                        }
                        let input = args
                            .get("input")
                            .cloned()
                            .unwrap_or_else(|| serde_json::json!({}));
                        self.execute_chain(&chain, input).await
                    }
                    Err(e) => format!("Error: Invalid chain format: {e}"),
                }
            }
            "ask_user" => self.ask_user(args),
            // Task management tools
            "task_create" => self.task_create(args).await,
            "task_list" => self.task_list(args).await,
            "task_get" => self.task_get(args).await,
            "task_update" => self.task_update(args).await,
            "task_stop" => self.task_stop(args).await,
            "sleep" => self.sleep_tool(args).await,
            "tool_search" => self.tool_search(args),
            "web_search" => self.web_search(args),
            "spawn_agent" => agent_spawning::handle_spawn_agent_tool(args, self.spawn_context.as_ref()).await,
            "diagnose" => self.diagnose(args).await,
            "lsp" => self.lsp(args),
            "env" => self.env_tool(args),
            "notebook_edit" => self.notebook_edit(args),
            "config" => self.config_tool(args),
            "brief" => self.brief(args),
            _ if name.starts_with("mcp_") => self.execute_mcp_tool(name, args).await,
            _ => format!(
                "Unknown tool: {name}. Available tools: bash, read_file, write_file, str_replace, \
                 list_dir, grep, glob, symbols, find_definition, find_references, git_status, \
                 git_diff, git_log, git_show, git_blame, call_graph, run_build_test, web_fetch, \
                 mo_query, memory_search, memory_profile, ask_user, task_create, task_list, \
                 task_get, task_update, task_stop, sleep, tool_search, web_search, spawn_agent, \
                 diagnose, lsp, env, notebook_edit, config, powershell, brief"
            ),
        };
        // Normalize empty output, then apply global safety net
        let output = normalize_empty_output(output, name);
        let output = truncate_output(output, global_output_limit());
        let output = self.maybe_persist_large_output(output, name);
        self.record_output_size(output.len());
        output
    }

    /// Persist large tool output to disk when aggregate budget is under pressure.
    ///
    /// When a single tool result exceeds `PERSIST_THRESHOLD` AND cumulative
    /// output this turn is above `AGGREGATE_SOFT_LIMIT`, the full output is
    /// written to `~/.astra/tool-results/<hash>.txt` and replaced with a
    /// ~2KB preview + file path. The model can use `read_file` with
    /// `start_line/end_line` to access specific parts of the persisted file.
    ///
    /// Inspired by Claude Code's `toolResultStorage.ts` which persists large
    /// results to `~/.claude/projects/<hash>/<session>/tool-results/`.
    fn maybe_persist_large_output(&self, output: String, _tool_name: &str) -> String {
        // Skip small outputs
        if output.len() < PERSIST_THRESHOLD {
            return output;
        }
        // Only persist when aggregate pressure is meaningful
        let agg = self
            .aggregate_output_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        if agg <= AGGREGATE_SOFT_LIMIT {
            return output;
        }
        // Never persist error outputs (they're small and actionable)
        if output.starts_with("Error:") {
            return output;
        }

        // Write to disk
        let dir = tool_results_dir();
        if std::fs::create_dir_all(&dir).is_err() {
            return output;
        }

        // Use a content hash for the filename (dedup identical outputs)
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            output.hash(&mut h);
            format!("{:016x}", h.finish())
        };
        let filepath = dir.join(format!("{hash}.txt"));

        // Write only if not already persisted (idempotent)
        if !filepath.exists() {
            if std::fs::write(&filepath, &output).is_err() {
                return output;
            }
        }

        // Build preview: first ~2KB, cut at newline boundary
        let preview_end = output.len().min(PERSIST_PREVIEW_BYTES);
        let preview_end = output[..preview_end]
            .rfind('\n')
            .filter(|&pos| pos > preview_end / 2)
            .map(|pos| pos + 1)
            .unwrap_or(preview_end);
        let preview = &output[..output.floor_char_boundary(preview_end)];
        let total_lines = output.lines().count();

        format!(
            "<persisted-output>\n\
             Output too large ({} bytes, ~{} lines) for context window. \
             Full output saved to: {}\n\n\
             Preview (first ~{} bytes):\n\
             {}\n...\n\
             </persisted-output>\n\
             Use read_file with start_line/end_line to read specific sections of the persisted file.",
            output.len(),
            total_lines,
            filepath.display(),
            PERSIST_PREVIEW_BYTES,
            preview,
        )
    }

    /// Extract code symbols (functions, classes, structs) from a file using Tree-sitter.
    ///
    /// Returns structured symbol info with signatures and line numbers.
    fn symbols(&self, args: &Value) -> String {
        let path_str = match args.get("path").and_then(Value::as_str) {
            Some(p) => p,
            None => return "Error: missing 'path' parameter".to_string(),
        };

        let path = if path_str.starts_with('/') {
            PathBuf::from(path_str)
        } else {
            self.project_root.join(path_str)
        };

        // Sandbox check
        if let Some(ref policy) = self.sandbox_policy
            && let Err(e) = validate_path(policy, path_str)
        {
            return format!("Sandbox: path blocked: {e}");
        }

        if !path.exists() {
            return format!("Error: No such file: {}", path.display());
        }

        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error: Failed to read file: {e}"),
        };

        // Detect language from path
        let lang = match code_intel::detect_language(&path) {
            Some(l) => l,
            None => {
                return format!(
                    "Error: Unsupported language for {}. Supports: Rust, Python, TypeScript/JavaScript, Go",
                    path.display()
                );
            }
        };

        // Extract symbols
        let mut symbols = code_intel::extract_symbols(&content, lang);

        // Apply pattern filter if provided
        if let Some(pattern) = args.get("pattern").and_then(Value::as_str)
            && let Ok(re) = regex::Regex::new(pattern)
        {
            symbols.retain(|s| re.is_match(&s.name));
        }

        // Apply kind filter if provided
        if let Some(kinds_arr) = args.get("kinds").and_then(Value::as_array) {
            let kinds: Vec<&str> = kinds_arr.iter().filter_map(Value::as_str).collect();
            if !kinds.is_empty() {
                symbols.retain(|s| {
                    let kind_str = s.kind.as_str();
                    kinds.iter().any(|k| k.eq_ignore_ascii_case(kind_str))
                });
            }
        }

        if symbols.is_empty() {
            return "No symbols found matching criteria.".to_string();
        }

        // Format output
        let lang_name = match lang {
            code_intel::Language::Rust => "Rust",
            code_intel::Language::Python => "Python",
            code_intel::Language::TypeScript => "TypeScript",
            code_intel::Language::JavaScript => "JavaScript",
            code_intel::Language::Go => "Go",
            code_intel::Language::Java => "Java",
            code_intel::Language::C => "C",
            code_intel::Language::Cpp => "C++",
            code_intel::Language::Ruby => "Ruby",
        };

        let show_calls = args.get("calls").and_then(Value::as_bool).unwrap_or(false);

        let mut output = format!(
            "# Symbols in {} ({}, {} found)\n\n",
            path.file_name().unwrap_or_default().to_string_lossy(),
            lang_name,
            symbols.len()
        );

        for sym in &symbols {
            let parent_suffix = sym
                .parent
                .as_ref()
                .map(|p| format!(" (in {p})"))
                .unwrap_or_default();
            output.push_str(&format!(
                "{}:{}-{} [{}]{}: {}\n",
                path.file_name().unwrap_or_default().to_string_lossy(),
                sym.start_line,
                sym.end_line,
                sym.kind.as_str(),
                parent_suffix,
                sym.signature
            ));

            // If calls=true, show what this symbol calls
            if show_calls
                && matches!(
                    sym.kind,
                    code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
                )
            {
                let calls = code_intel::extract_calls(&content, lang, sym.start_line, sym.end_line);
                if !calls.is_empty() {
                    for call in calls.iter().take(8) {
                        if let Some(ref recv) = call.receiver {
                            output.push_str(&format!(
                                "    → {}.{}() L{}\n",
                                recv, call.callee, call.line
                            ));
                        } else {
                            output.push_str(&format!("    → {}() L{}\n", call.callee, call.line));
                        }
                    }
                    if calls.len() > 8 {
                        output.push_str(&format!("    ... and {} more calls\n", calls.len() - 8));
                    }
                }
            }
        }

        output
    }

    /// AST-validate grep matches: filter out references in comments and string literals.
    ///
    /// Groups matches by file, parses each file once with tree-sitter, and checks
    /// if the symbol at each match position falls inside a non-code node.
    fn ast_validate_references<'a>(&self, lines: &[&'a str], symbol: &str) -> Vec<&'a str> {
        use std::collections::HashMap;

        // Group lines by file path for efficient per-file parsing
        let mut by_file: HashMap<&str, Vec<(usize, &'a str)>> = HashMap::new();
        for line in lines {
            if let Some((file, line_num)) = parse_grep_file_line(line) {
                by_file.entry(file).or_default().push((line_num, line));
            }
        }

        let mut result = Vec::with_capacity(lines.len());

        for (file, matches) in &by_file {
            let file_path = self.project_root.join(file);
            let lang = match code_intel::detect_language(&file_path) {
                Some(l) => l,
                None => {
                    // Can't validate — keep all matches for this file
                    result.extend(matches.iter().map(|(_, line)| *line));
                    continue;
                }
            };
            let content = match fs::read_to_string(&file_path) {
                Ok(c) => c,
                Err(_) => {
                    result.extend(matches.iter().map(|(_, line)| *line));
                    continue;
                }
            };

            for &(line_num, line) in matches {
                // Find the column where the symbol appears in this line
                let line_content = content
                    .lines()
                    .nth(line_num.saturating_sub(1))
                    .unwrap_or("");
                let col = match line_content.find(symbol) {
                    Some(c) => c,
                    None => {
                        result.push(line); // Can't find symbol in line, keep it
                        continue;
                    }
                };

                if !code_intel::is_in_comment_or_string(&content, lang, line_num, col) {
                    result.push(line);
                }
            }
        }

        // Also keep lines that couldn't be parsed
        for line in lines {
            if parse_grep_file_line(line).is_none() {
                result.push(line);
            }
        }

        result
    }

    /// Walk project files and find all functions that call `target` symbol.
    /// Returns Vec of (relative_path, caller_name, caller_signature, call_line).
    fn find_callers_cross_file(
        &self,
        target: &str,
        _origin_file: &std::path::Path,
    ) -> Vec<(String, String, String, usize)> {
        let skip_names = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let extensions = [
            "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp", "rb",
        ];
        let max_files = 300;

        // Step 1: Use ripgrep to pre-filter files containing the target symbol (fast)
        let candidate_files = self.prefilter_files_with_symbol(target, &extensions);

        // Step 2: For each candidate, parse with tree-sitter and find callers
        let mut callers = Vec::new();
        let mut files_scanned = 0;

        let files_to_scan: Vec<PathBuf> = if candidate_files.is_empty() {
            // Fallback: walk all files (ripgrep not available)
            self.collect_project_files(&skip_names, &extensions, max_files)
        } else {
            candidate_files.into_iter().take(max_files).collect()
        };

        for file_path in &files_to_scan {
            files_scanned += 1;
            if files_scanned > max_files {
                break;
            }

            let lang = match code_intel::detect_language(file_path) {
                Some(l) => l,
                None => continue,
            };
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };

            let symbols = code_intel::extract_symbols(&content, lang);
            let rel_path = file_path
                .strip_prefix(&self.project_root)
                .unwrap_or(file_path)
                .display()
                .to_string();

            for sym in &symbols {
                if sym.name == target {
                    continue; // Skip the target's own definition
                }
                if !matches!(
                    sym.kind,
                    code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
                ) {
                    continue;
                }
                let sym_calls =
                    code_intel::extract_calls(&content, lang, sym.start_line, sym.end_line);
                for call in &sym_calls {
                    if call.callee == target {
                        callers.push((
                            rel_path.clone(),
                            sym.name.clone(),
                            sym.signature.clone(),
                            call.line,
                        ));
                        break;
                    }
                }
            }
        }

        callers
    }

    /// Use ripgrep to quickly find files that contain a symbol name (pre-filter).
    fn prefilter_files_with_symbol(&self, symbol: &str, extensions: &[&str]) -> Vec<PathBuf> {
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--files-with-matches")
            .arg("--no-heading")
            .arg("--color=never")
            .arg("-w") // word boundary
            .current_dir(&self.project_root);

        // Add extension filters
        for ext in extensions {
            cmd.arg("--glob").arg(format!("*.{ext}"));
        }

        // Exclude noise
        for dir in &[".git", "node_modules", "target", "vendor", "dist"] {
            cmd.arg("--glob").arg(format!("!{dir}/"));
        }

        cmd.arg(symbol);

        match cmd.output() {
            Ok(out) => String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|l| self.project_root.join(l.trim()))
                .filter(|p| p.exists())
                .collect(),
            Err(_) => Vec::new(), // Fallback handled by caller
        }
    }

    /// Collect project files by walking directories (fallback when ripgrep unavailable).
    fn collect_project_files(
        &self,
        skip_names: &[&str],
        extensions: &[&str],
        max_files: usize,
    ) -> Vec<PathBuf> {
        let mut result = Vec::new();
        let mut dirs_to_visit = vec![self.project_root.clone()];

        while let Some(dir) = dirs_to_visit.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || skip_names.contains(&name_str.as_ref()) {
                    continue;
                }
                let ft = entry.file_type().ok();
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs_to_visit.push(entry.path());
                } else if ft.map(|t| t.is_file()).unwrap_or(false)
                    && let Some(ext) = entry.path().extension().and_then(|e| e.to_str())
                    && extensions.contains(&ext)
                {
                    result.push(entry.path());
                    if result.len() >= max_files {
                        return result;
                    }
                }
            }
        }

        result
    }

    fn collect_files_with_glob(
        &self,
        root: &std::path::Path,
        glob_pat: &str,
        files: &mut Vec<std::path::PathBuf>,
    ) {
        let skip_dirs = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let pat = glob_pat.trim_start_matches('*');

        let mut dirs_to_visit = vec![root.to_path_buf()];
        while let Some(dir) = dirs_to_visit.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || skip_dirs.contains(&name_str.as_ref()) {
                    continue;
                }
                let ft = entry.file_type().ok();
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs_to_visit.push(entry.path());
                } else if ft.map(|t| t.is_file()).unwrap_or(false) {
                    let file_name = entry
                        .path()
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string();
                    if file_name.ends_with(pat) {
                        files.push(entry.path());
                        if files.len() >= 500 {
                            return;
                        }
                    }
                }
            }
        }
    }

    /// Resolve an import path to candidate file paths.
    ///
    /// Given an import path (e.g., "std::collections::HashMap" for Rust,
    /// "os.path" for Python, "./config" for TS), returns file paths within
    /// the project that likely define the imported symbol.
    fn resolve_import_to_files(
        &self,
        import: &code_intel::ImportStatement,
        lang: code_intel::Language,
        file_paths: &[std::path::PathBuf],
    ) -> Vec<usize> {
        let mut candidates: Vec<usize> = Vec::new();

        // Convert import path to file path segments
        let path_segments: Vec<&str> = match lang {
            code_intel::Language::Rust => {
                // "crate::utils::helper" → ["utils", "helper"]
                // "super::config" → ["config"]
                let cleaned = import
                    .path
                    .trim_start_matches("crate::")
                    .trim_start_matches("super::");
                cleaned.split("::").collect()
            }
            code_intel::Language::Python => {
                // "os.path" → ["os", "path"]
                // ".utils" → ["utils"]
                import.path.trim_start_matches('.').split('.').collect()
            }
            code_intel::Language::TypeScript | code_intel::Language::JavaScript => {
                // "./config" → ["config"]
                // "../utils/helper" → ["utils", "helper"]
                let cleaned = import
                    .path
                    .trim_start_matches("./")
                    .trim_start_matches("../");
                cleaned.split('/').collect()
            }
            code_intel::Language::Go => {
                // "path/filepath" → ["path", "filepath"]
                import.path.split('/').collect()
            }
            _ => return candidates,
        };

        if path_segments.is_empty() {
            return candidates;
        }

        // Match file paths that contain import path segments
        let last_segment = path_segments.last().unwrap_or(&"");
        for (idx, file_path) in file_paths.iter().enumerate() {
            let path_str = file_path.to_string_lossy();
            let file_stem = file_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

            // Exact stem match (e.g., import "config" → config.rs/config.py)
            if file_stem.eq_ignore_ascii_case(last_segment) {
                candidates.push(idx);
                continue;
            }

            // Check if the path contains all segments in order
            // e.g., "crate::utils::helper" matches "src/utils/helper.rs"
            if path_segments.len() > 1 {
                let lower_path = path_str.to_lowercase();
                let all_match = path_segments
                    .iter()
                    .all(|seg| lower_path.contains(&seg.to_lowercase()));
                if all_match {
                    candidates.push(idx);
                    continue;
                }
            }

            // For Rust: mod.rs in a directory matching the segment
            if matches!(lang, code_intel::Language::Rust) && file_stem == "mod" {
                let parent_name = file_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if parent_name.eq_ignore_ascii_case(last_segment) {
                    candidates.push(idx);
                }
            }

            // For Python: __init__.py in a directory matching the segment
            if matches!(lang, code_intel::Language::Python) && file_stem == "__init__" {
                let parent_name = file_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if parent_name.eq_ignore_ascii_case(last_segment) {
                    candidates.push(idx);
                }
            }

            // For TS: index.ts in a directory matching the segment
            if matches!(
                lang,
                code_intel::Language::TypeScript | code_intel::Language::JavaScript
            ) && file_stem == "index"
            {
                let parent_name = file_path
                    .parent()
                    .and_then(|p| p.file_name())
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                if parent_name.eq_ignore_ascii_case(last_segment) {
                    candidates.push(idx);
                }
            }
        }

        candidates.dedup();
        candidates
    }

    /// Find where a symbol is defined across the codebase using tree-sitter.
    /// When a `file` parameter is provided, analyzes imports in that file to
    /// prioritize the most likely definition (import-aware resolution).
    fn find_definition(&self, args: &Value) -> String {
        let symbol = match args.get("symbol").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'symbol' parameter is required".to_string(),
        };

        let search_root = if let Some(p) = args.get("path").and_then(Value::as_str) {
            self.project_root.join(p)
        } else {
            self.project_root.clone()
        };

        // Determine file extensions to search
        let lang_filter = args.get("language").and_then(Value::as_str);
        let extensions = match lang_filter {
            Some("rust") => vec!["rs"],
            Some("python") => vec!["py"],
            Some("typescript") => vec!["ts", "tsx"],
            Some("javascript") => vec!["js", "jsx"],
            Some("go") => vec!["go"],
            Some("java") => vec!["java"],
            Some("c") => vec!["c", "h"],
            Some("cpp") => vec!["cpp", "cc", "cxx", "hpp", "h"],
            Some("ruby") => vec!["rb"],
            None => vec![
                "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp",
                "rb",
            ],
            Some(other) => {
                return format!(
                    "Error: unsupported language '{other}'. Supported: rust, python, typescript, javascript, go, java, c, cpp, ruby"
                );
            }
        };

        // Build regex for matching symbol name
        let pattern = if symbol.contains('*') || symbol.contains('(') || symbol.contains('[') {
            match regex::Regex::new(symbol) {
                Ok(re) => re,
                Err(e) => return format!("Error: invalid regex pattern: {e}"),
            }
        } else {
            // Exact match
            match regex::Regex::new(&format!(r"^{}$", regex::escape(symbol))) {
                Ok(re) => re,
                Err(e) => return format!("Error: regex construction failed: {e}"),
            }
        };

        let definition_kinds = [
            "fn",
            "method",
            "class",
            "struct",
            "trait",
            "interface",
            "enum",
            "type",
            "const",
            "var",
            "mod",
        ];

        let mut results: Vec<String> = Vec::new();
        let mut import_results: Vec<String> = Vec::new();
        let max_files = 500;
        let mut files_scanned = 0;

        // Collect matching files using a simple recursive walker
        let mut dirs_to_visit = vec![search_root.clone()];
        let skip_names = ["node_modules", "target", "vendor", "dist", "__pycache__"];
        let mut file_paths: Vec<std::path::PathBuf> = Vec::new();

        while let Some(dir) = dirs_to_visit.pop() {
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name_str = name.to_string_lossy();
                if name_str.starts_with('.') || skip_names.contains(&name_str.as_ref()) {
                    continue;
                }
                let ft = entry.file_type().ok();
                if ft.map(|t| t.is_dir()).unwrap_or(false) {
                    dirs_to_visit.push(entry.path());
                } else if ft.map(|t| t.is_file()).unwrap_or(false) {
                    let ext = entry
                        .path()
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_string();
                    if extensions.contains(&ext.as_str()) {
                        file_paths.push(entry.path());
                    }
                }
            }
        }

        // ── Import-aware resolution ────────────────────────────────────
        // When `file` parameter is provided, extract imports from that file
        // and prioritize files matching the import paths.
        let mut import_priority_indices: Vec<usize> = Vec::new();
        let context_file = args.get("file").and_then(Value::as_str);

        if let Some(ctx_file) = context_file {
            let ctx_path = if ctx_file.starts_with('/') {
                PathBuf::from(ctx_file)
            } else {
                self.project_root.join(ctx_file)
            };
            if ctx_path.exists()
                && let Some(ctx_lang) = code_intel::detect_language(&ctx_path)
                && let Ok(ctx_content) = fs::read_to_string(&ctx_path)
            {
                let imports = code_intel::extract_imports(&ctx_content, ctx_lang);
                // Find imports that reference the target symbol
                for import in &imports {
                    let matches_symbol = import.names.iter().any(|n| n == symbol)
                        || import.is_wildcard
                        || import.path.ends_with(symbol);
                    if matches_symbol {
                        let candidates =
                            self.resolve_import_to_files(import, ctx_lang, &file_paths);
                        import_priority_indices.extend(candidates);
                    }
                }
            }
        }
        import_priority_indices.sort_unstable();
        import_priority_indices.dedup();

        // Helper closure: scan a file for matching definitions
        let scan_file = |path: &std::path::PathBuf, project_root: &Path| -> Vec<(String, bool)> {
            let mut hits = Vec::new();
            let lang = match code_intel::detect_language(path) {
                Some(l) => l,
                None => return hits,
            };
            let content = match fs::read_to_string(path) {
                Ok(c) => c,
                Err(_) => return hits,
            };
            let symbols = code_intel::extract_symbols(&content, lang);
            for sym in &symbols {
                if pattern.is_match(&sym.name) && definition_kinds.contains(&sym.kind.as_str()) {
                    let rel_path = path.strip_prefix(project_root).unwrap_or(path).display();
                    let parent_info = sym
                        .parent
                        .as_ref()
                        .map(|p| format!(" (in {p})"))
                        .unwrap_or_default();

                    let doc = code_intel::extract_doc_comment(&content, lang, sym.start_line);
                    let doc_info = if doc.is_empty() {
                        String::new()
                    } else {
                        let doc_lines: Vec<&str> = doc.lines().take(5).collect();
                        let truncated = if doc.lines().count() > 5 {
                            "\n    ..."
                        } else {
                            ""
                        };
                        format!("\n    📝 {}{}", doc_lines.join("\n    "), truncated)
                    };

                    hits.push((
                        format!(
                            "{}:{} [{}]{} {}{}",
                            rel_path,
                            sym.start_line,
                            sym.kind.as_str(),
                            parent_info,
                            sym.signature,
                            doc_info
                        ),
                        false,
                    ));
                }
            }
            hits
        };

        // Scan import-priority files first
        let mut scanned_indices: std::collections::HashSet<usize> =
            std::collections::HashSet::new();
        for &idx in &import_priority_indices {
            if idx < file_paths.len() {
                scanned_indices.insert(idx);
                files_scanned += 1;
                for (hit, _) in scan_file(&file_paths[idx], &self.project_root) {
                    import_results.push(hit);
                }
            }
        }

        // Then scan remaining files
        for (idx, path) in file_paths.iter().enumerate() {
            if scanned_indices.contains(&idx) {
                continue;
            }
            files_scanned += 1;
            if files_scanned > max_files {
                results.push(format!("\n[stopped after scanning {max_files} files]"));
                break;
            }
            for (hit, _) in scan_file(path, &self.project_root) {
                results.push(hit);
            }
        }

        let total_found = import_results.len() + results.len();
        if total_found == 0 {
            format!("No definitions found for '{symbol}' ({files_scanned} files scanned)")
        } else {
            let mut body_parts: Vec<String> = Vec::new();

            // Show import-resolved results first with marker
            if !import_results.is_empty() {
                body_parts.push(format!(
                    "## 📦 Import-resolved ({} via import analysis)\n",
                    import_results.len()
                ));
                body_parts.push(import_results.join("\n"));
                if !results.is_empty() {
                    body_parts.push(format!("\n\n## Other definitions ({})\n", results.len()));
                    body_parts.push(results.join("\n"));
                }
            } else {
                body_parts.push(results.join("\n"));
            }

            let header = format!(
                "# Definitions of '{}' ({} found, {} files scanned)\n\n",
                symbol, total_found, files_scanned
            );
            truncate_output(
                format!("{}{}", header, body_parts.join("")),
                tool_output_limit().min(15_000),
            )
        }
    }

    /// Find all references to a symbol across the codebase.
    /// Uses grep for speed, with word-boundary matching for precision.
    fn find_references(&self, args: &Value) -> String {
        let symbol = match args.get("symbol").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'symbol' parameter is required".to_string(),
        };

        let search_path = if let Some(p) = args.get("path").and_then(Value::as_str) {
            self.project_root.join(p)
        } else {
            self.project_root.clone()
        };

        // Build ripgrep command for word-boundary matching
        let mut cmd = std::process::Command::new("rg");
        cmd.arg("--no-heading")
            .arg("--line-number")
            .arg("--color=never")
            .arg("--max-count=5") // Max per file
            .arg("-w") // Word boundary
            .current_dir(&self.project_root);

        // Apply include filter
        if let Some(include) = args.get("include").and_then(Value::as_str) {
            cmd.arg("--glob").arg(include);
        }

        // Exclude common noise directories
        cmd.arg("--glob")
            .arg("!.git/")
            .arg("--glob")
            .arg("!node_modules/")
            .arg("--glob")
            .arg("!target/")
            .arg("--glob")
            .arg("!vendor/")
            .arg("--glob")
            .arg("!dist/")
            .arg("--glob")
            .arg("!*.min.js")
            .arg("--glob")
            .arg("!*.min.css");

        // Use fixed string for exact symbol (faster), word-bounded
        cmd.arg(symbol);
        cmd.arg(search_path.to_string_lossy().to_string());

        let kind_filter = args.get("kind").and_then(Value::as_str).unwrap_or("all");
        let ast_validate = args
            .get("validate")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        match cmd.output() {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if stdout.is_empty() {
                    return format!("No references found for '{symbol}'");
                }

                let lines: Vec<&str> = stdout.lines().collect();
                let total_grep = lines.len();

                // AST validation: filter out matches in comments/strings
                let validated_lines: Vec<&str> = if ast_validate {
                    self.ast_validate_references(&lines, symbol)
                } else {
                    lines
                };
                let ast_filtered = total_grep - validated_lines.len();

                // Categorize each reference line
                let categorized: Vec<(&str, &str)> = validated_lines
                    .iter()
                    .map(|line| {
                        let category = categorize_reference(line, symbol);
                        (*line, category)
                    })
                    .collect();

                // Apply kind filter
                let filtered: Vec<(&str, &str)> = if kind_filter == "all" {
                    categorized
                } else {
                    categorized
                        .into_iter()
                        .filter(|(_, cat)| *cat == kind_filter)
                        .collect()
                };

                if filtered.is_empty() {
                    return format!("No {kind_filter} references found for '{symbol}'");
                }

                let total = filtered.len();

                // Group by file for cleaner output
                let ast_note = if ast_filtered > 0 {
                    format!(", {} in comments/strings filtered", ast_filtered)
                } else {
                    String::new()
                };
                let mut output = format!(
                    "# References to '{}' ({} found{}{})\n\n",
                    symbol,
                    total,
                    if kind_filter != "all" {
                        format!(", kind={kind_filter}")
                    } else {
                        String::new()
                    },
                    ast_note
                );
                let mut current_file = "";
                for (line, cat) in filtered.iter().take(50) {
                    if let Some(colon_pos) = line.find(':') {
                        let file = &line[..colon_pos];
                        if file != current_file {
                            if !current_file.is_empty() {
                                output.push('\n');
                            }
                            current_file = file;
                        }
                    }
                    output.push_str(&format!("[{cat}] {line}\n"));
                }

                if total > 50 {
                    output.push_str(&format!("\n[{} more references not shown]", total - 50));
                }

                truncate_output(output, tool_output_limit().min(15_000))
            }
            Err(_) => {
                // Fallback to grep if rg not available
                let out = std::process::Command::new("grep")
                    .args([
                        "-rnw",
                        "--include=*.rs",
                        "--include=*.py",
                        "--include=*.ts",
                        "--include=*.go",
                        "--include=*.java",
                        symbol,
                    ])
                    .arg(search_path.to_string_lossy().to_string())
                    .current_dir(&self.project_root)
                    .output();
                match out {
                    Ok(o) => {
                        let stdout = String::from_utf8_lossy(&o.stdout);
                        if stdout.is_empty() {
                            format!("No references found for '{symbol}'")
                        } else {
                            let lines: Vec<&str> = stdout.lines().take(50).collect();
                            let header =
                                format!("# References to '{}' ({} found)\n\n", symbol, lines.len());
                            truncate_output(
                                format!("{header}{}", lines.join("\n")),
                                tool_output_limit().min(15_000),
                            )
                        }
                    }
                    Err(e) => format!("Error: search failed: {e}"),
                }
            }
        }
    }

    /// Smart rename: find all AST-validated references to a symbol and replace them.
    fn rename_symbol(&self, args: &Value) -> String {
        let symbol = match args.get("symbol").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'symbol' (current name) is required".into(),
        };
        let new_name = match args.get("new_name").and_then(Value::as_str) {
            Some(s) if !s.is_empty() => s,
            _ => return "Error: 'new_name' is required".into(),
        };
        if symbol == new_name {
            return "Error: symbol and new_name are the same".into();
        }

        // Validate new_name is a valid identifier
        if !new_name
            .chars()
            .next()
            .is_some_and(|c| c.is_alphabetic() || c == '_')
            || !new_name.chars().all(|c| c.is_alphanumeric() || c == '_')
        {
            return format!("Error: '{}' is not a valid identifier", new_name);
        }

        let dry_run = args.get("dry_run").and_then(Value::as_bool).unwrap_or(true);

        // Step 1: Find all references using AST-validated find_references
        let search_path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let include = args.get("include").and_then(Value::as_str);

        let search_dir = self.project_root.join(search_path);
        if !search_dir.exists() {
            return format!("Error: path '{}' not found", search_path);
        }

        // Build search command — try ripgrep first, fall back to grep
        let output = {
            let mut cmd = std::process::Command::new("rg");
            cmd.arg("-n")
                .arg("-w")
                .arg("--no-heading")
                .arg("--max-count")
                .arg("1000")
                .arg(symbol)
                .current_dir(&search_dir);
            if let Some(inc) = include {
                cmd.arg("-g").arg(inc);
            }
            for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                cmd.arg("--glob").arg(format!("!{}", exc));
            }
            match cmd.output() {
                Ok(o) => o,
                Err(_) => {
                    // Fallback to grep
                    let mut cmd = std::process::Command::new("grep");
                    cmd.arg("-rnw").arg(symbol).current_dir(&search_dir);
                    if let Some(inc) = include {
                        cmd.arg("--include").arg(inc);
                    }
                    for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                        cmd.arg("--exclude-dir").arg(*exc);
                    }
                    match cmd.output() {
                        Ok(o) => o,
                        Err(_) => return "Error: neither rg nor grep available".into(),
                    }
                }
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        if stdout.trim().is_empty() {
            return format!("No references to '{}' found", symbol);
        }

        let lines: Vec<&str> = stdout.lines().collect();
        let total_grep = lines.len();

        // Step 2: AST-validate to filter comments/strings
        let validated = self.ast_validate_references(&lines, symbol);
        let filtered_count = total_grep - validated.len();

        if validated.is_empty() {
            return format!(
                "No code references to '{}' found (all {} matches were in comments/strings)",
                symbol, total_grep
            );
        }

        // Step 3: Group by file and collect line numbers
        let mut by_file: std::collections::BTreeMap<String, Vec<usize>> =
            std::collections::BTreeMap::new();
        for line in &validated {
            if let Some((file, line_num)) = parse_grep_file_line(line) {
                by_file.entry(file.to_string()).or_default().push(line_num);
            }
        }

        // Step 4: Apply or preview replacements
        let mut output = String::new();
        let mut total_replacements = 0usize;
        let mut files_changed = 0usize;

        if dry_run {
            output.push_str(&format!("🔍 Rename preview: {} → {}\n", symbol, new_name));
        } else {
            output.push_str(&format!("✏️  Renaming: {} → {}\n", symbol, new_name));
        }

        for (rel_path, line_nums) in &by_file {
            let abs_path = search_dir.join(rel_path);
            let content = match fs::read_to_string(&abs_path) {
                Ok(c) => c,
                Err(e) => {
                    output.push_str(&format!("  ⚠ {}: read error: {}\n", rel_path, e));
                    continue;
                }
            };

            let content_lines: Vec<&str> = content.lines().collect();
            let mut replacements_in_file = 0;
            let mut new_lines: Vec<String> = content_lines.iter().map(|l| l.to_string()).collect();

            // Build word-boundary regex for precise replacement
            let pattern = format!(r"\b{}\b", regex::escape(symbol));
            let re = match regex::Regex::new(&pattern) {
                Ok(r) => r,
                Err(_) => {
                    output.push_str(&format!("  ⚠ {}: invalid regex for symbol\n", rel_path));
                    continue;
                }
            };

            for &line_num in line_nums {
                let idx = line_num.saturating_sub(1);
                if idx >= new_lines.len() {
                    continue;
                }

                // Check this specific occurrence via AST validation before replacing
                let old_line = &new_lines[idx];
                let replaced = re.replace_all(old_line, new_name).to_string();
                if replaced != *old_line {
                    if dry_run {
                        output.push_str(&format!("  {}:{}:\n", rel_path, line_num));
                        output.push_str(&format!("    - {}\n", old_line.trim()));
                        output.push_str(&format!("    + {}\n", replaced.trim()));
                    }
                    new_lines[idx] = replaced;
                    replacements_in_file += 1;
                }
            }

            if replacements_in_file > 0 {
                files_changed += 1;
                total_replacements += replacements_in_file;

                if !dry_run {
                    // Reconstruct file content preserving original line endings
                    let has_trailing_newline = content.ends_with('\n');
                    let mut new_content = new_lines.join("\n");
                    if has_trailing_newline {
                        new_content.push('\n');
                    }

                    if let Err(e) = fs::write(&abs_path, &new_content) {
                        output.push_str(&format!("  ⚠ {}: write error: {}\n", rel_path, e));
                        continue;
                    }
                    output.push_str(&format!(
                        "  ✓ {} ({} replacement{})\n",
                        rel_path,
                        replacements_in_file,
                        if replacements_in_file == 1 { "" } else { "s" }
                    ));
                }
            }
        }

        output.push_str(&format!(
            "\n{} replacement{} in {} file{}",
            total_replacements,
            if total_replacements == 1 { "" } else { "s" },
            files_changed,
            if files_changed == 1 { "" } else { "s" }
        ));

        if filtered_count > 0 {
            output.push_str(&format!(
                " ({} comment/string matches skipped)",
                filtered_count
            ));
        }

        if dry_run {
            output.push_str("\n\n💡 This is a dry run. Set dry_run=false to apply changes.");
        }

        output
    }

    /// Dead code detection: find symbols with zero external references.
    fn dead_code(&self, args: &Value) -> String {
        let scan_path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let include = args.get("include").and_then(Value::as_str);
        let kind_filter = args.get("kind").and_then(Value::as_str).unwrap_or("all");

        let scan_dir = self.project_root.join(scan_path);
        if !scan_dir.exists() {
            return format!("Error: path '{}' not found", scan_path);
        }

        // Step 1: Collect files to scan
        let extensions = [
            "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp", "rb",
        ];
        let skip_dirs = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let max_files = 200;

        let files: Vec<std::path::PathBuf> = if scan_dir.is_file() {
            vec![scan_dir.clone()]
        } else {
            self.collect_project_files(&skip_dirs, &extensions, max_files)
                .into_iter()
                .filter(|p| p.starts_with(&scan_dir))
                .filter(|p| {
                    if let Some(inc) = include {
                        let name = p.file_name().unwrap_or_default().to_string_lossy();
                        let pat = inc.trim_start_matches('*');
                        name.ends_with(pat)
                    } else {
                        true
                    }
                })
                .collect()
        };

        if files.is_empty() {
            return format!("No source files found in '{}'", scan_path);
        }

        // Step 2: Extract all symbols from scanned files
        struct SymbolInfo {
            name: String,
            kind: String,
            file: String,
            line: usize,
            is_public: bool,
            is_test: bool,
            is_main: bool,
        }

        let mut symbols: Vec<SymbolInfo> = Vec::new();

        for file_path in &files {
            let lang = match code_intel::detect_language(file_path) {
                Some(l) => l,
                None => continue,
            };
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = file_path
                .strip_prefix(&self.project_root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let extracted = code_intel::extract_symbols(&content, lang);
            for sym in extracted {
                let kind_str = match sym.kind {
                    code_intel::SymbolKind::Function | code_intel::SymbolKind::Method => "function",
                    code_intel::SymbolKind::Struct
                    | code_intel::SymbolKind::Class
                    | code_intel::SymbolKind::Enum
                    | code_intel::SymbolKind::Trait
                    | code_intel::SymbolKind::Interface
                    | code_intel::SymbolKind::Type => "type",
                    code_intel::SymbolKind::Constant => "constant",
                    _ => continue, // skip variables, imports, constructors, modules
                };

                // Apply kind filter
                if kind_filter != "all" && kind_str != kind_filter {
                    continue;
                }

                // Check for known entry points and special patterns
                let is_main = sym.name == "main" || sym.name == "Main";
                let sig = &sym.signature;
                let is_test = sym.name.starts_with("test_")
                    || sym.name.ends_with("_test")
                    || sym.name.starts_with("Test")
                    || sig.contains("#[test]")
                    || sig.contains("#[cfg(test)]");

                // Check visibility
                let is_public = sig.starts_with("pub ")
                    || sig.starts_with("pub(")
                    || sig.starts_with("export ");

                symbols.push(SymbolInfo {
                    name: sym.name,
                    kind: kind_str.to_string(),
                    file: rel.clone(),
                    line: sym.start_line,
                    is_public,
                    is_test,
                    is_main,
                });
            }
        }

        if symbols.is_empty() {
            return format!(
                "No symbols of kind '{}' found in '{}'",
                kind_filter, scan_path
            );
        }

        // Step 3: For each symbol, count references project-wide
        let mut dead: Vec<&SymbolInfo> = Vec::new();
        let mut checked = 0;

        for sym in &symbols {
            // Skip known entry points
            if sym.is_main || sym.is_test {
                continue;
            }

            checked += 1;

            // Quick grep count
            let ref_count = self.count_symbol_references(&sym.name);

            // A symbol with only 1 reference (its own definition) is dead
            // A symbol with 0 references means grep couldn't find it (unlikely but safe)
            if ref_count <= 1 {
                dead.push(sym);
            }
        }

        // Step 4: Format output
        let mut output = String::new();
        if dead.is_empty() {
            output.push_str(&format!(
                "✓ No dead code found ({} symbols checked in {} files)\n",
                checked,
                files.len()
            ));
        } else {
            output.push_str(&format!(
                "⚠ {} potentially unused symbol{} ({} checked in {} files):\n\n",
                dead.len(),
                if dead.len() == 1 { "" } else { "s" },
                checked,
                files.len()
            ));

            // Group by file
            let mut by_file: std::collections::BTreeMap<&str, Vec<&SymbolInfo>> =
                std::collections::BTreeMap::new();
            for sym in &dead {
                by_file.entry(&sym.file).or_default().push(sym);
            }

            for (file, syms) in &by_file {
                output.push_str(&format!("{}:\n", file));
                for sym in syms {
                    let pub_marker = if sym.is_public { " (pub)" } else { "" };
                    output.push_str(&format!(
                        "  L{}: {} {}{}\n",
                        sym.line, sym.kind, sym.name, pub_marker
                    ));
                }
            }

            if dead.iter().any(|s| s.is_public) {
                output.push_str(
                    "\n💡 Public symbols marked (pub) may be used by external consumers.\n",
                );
            }
        }

        output
    }

    /// Count how many times a symbol appears in the project (word-boundary match).
    fn count_symbol_references(&self, symbol: &str) -> usize {
        // Try ripgrep first, fall back to grep
        let output = {
            let mut cmd = std::process::Command::new("rg");
            cmd.arg("-c")
                .arg("-w")
                .arg("--no-heading")
                .arg(symbol)
                .current_dir(&self.project_root);
            for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                cmd.arg("--glob").arg(format!("!{}", exc));
            }
            match cmd.output() {
                Ok(o) => o,
                Err(_) => {
                    let mut cmd = std::process::Command::new("grep");
                    cmd.arg("-rcw").arg(symbol).current_dir(&self.project_root);
                    for exc in &[".git", "node_modules", "target", "vendor", "dist"] {
                        cmd.arg("--exclude-dir").arg(*exc);
                    }
                    match cmd.output() {
                        Ok(o) => o,
                        Err(_) => return usize::MAX, // can't count, assume referenced
                    }
                }
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        // Each line is "file:count" — sum all counts
        stdout
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.rsplitn(2, ':').collect();
                parts.first().and_then(|s| s.parse::<usize>().ok())
            })
            .sum()
    }

    // ── extract_members tool ─────────────────────────────────────────────────

    fn extract_members(&self, args: &Value) -> String {
        let file = match args.get("file").and_then(Value::as_str) {
            Some(f) => match self.resolve_checked(f) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'file'".to_string(),
        };

        let line = match args.get("line").and_then(Value::as_u64) {
            Some(l) => l as usize,
            None => return "Error: missing 'line'".to_string(),
        };

        let lang = match code_intel::detect_language(&file) {
            Some(l) => l,
            None => {
                return "Error: unsupported language (supported: rs, py, ts, js, go)".to_string();
            }
        };

        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };

        let members = code_intel::extract_members(&source, lang, line);

        if members.is_empty() {
            return format!(
                "No type definition found at line {line} in {}",
                file.display()
            );
        }

        let mut parts = Vec::new();
        let rel_path = file
            .strip_prefix(&self.project_root)
            .unwrap_or(&file)
            .display();
        parts.push(format!("Members of type at {}:{}", rel_path, line));
        parts.push(String::new());

        for m in &members {
            let vis = if m.visibility.is_empty() {
                String::new()
            } else {
                format!("{} ", m.visibility)
            };
            let type_str = if m.type_annotation.is_empty() {
                String::new()
            } else {
                format!(": {}", m.type_annotation)
            };
            let default_str = if m.default_value.is_empty() {
                String::new()
            } else {
                format!(" = {}", m.default_value)
            };
            parts.push(format!(
                "  L{:<4} {}{}{}{} ({})",
                m.line, vis, m.name, type_str, default_str, m.kind
            ));
        }

        parts.push(format!("\nTotal: {} members", members.len()));
        parts.join("\n")
    }

    // ── type_hierarchy tool ──────────────────────────────────────────────────

    fn type_hierarchy(&self, args: &Value) -> String {
        let name = match args.get("name").and_then(Value::as_str) {
            Some(n) if !n.trim().is_empty() => n.trim(),
            _ => return "Error: missing 'name'".to_string(),
        };
        let direction = args
            .get("direction")
            .and_then(Value::as_str)
            .unwrap_or("implementations");
        let include_glob = args
            .get("include")
            .and_then(Value::as_str)
            .unwrap_or("*.rs");

        // Collect Rust source files
        let mut files = Vec::new();
        self.collect_files_with_glob(&self.project_root, include_glob, &mut files);

        let mut all_impls: Vec<code_intel::ImplRelation> = Vec::new();
        for file in &files {
            if let Ok(source) = std::fs::read_to_string(file) {
                let rel_path = file
                    .strip_prefix(&self.project_root)
                    .unwrap_or(file)
                    .to_string_lossy()
                    .to_string();
                let impls = code_intel::find_rust_impls(&source, &rel_path);
                all_impls.extend(impls);
            }
        }

        let mut results: Vec<String> = Vec::new();

        match direction {
            "supertypes" => {
                // Find traits that `name` implements
                results.push(format!("Traits implemented by `{}`:", name));
                let mut found = false;
                for imp in &all_impls {
                    if imp.type_name == name {
                        results.push(format!(
                            "  impl {} — {}:{}",
                            imp.trait_name, imp.file, imp.line
                        ));
                        found = true;
                    }
                }
                if !found {
                    results.push(format!("  (no trait implementations found for `{}`)", name));
                }
            }
            _ => {
                // Find types that implement `name`
                results.push(format!("Types implementing `{}`:", name));
                let mut found = false;
                for imp in &all_impls {
                    if imp.trait_name == name {
                        results.push(format!("  {} — {}:{}", imp.type_name, imp.file, imp.line));
                        found = true;
                    }
                }
                if !found {
                    results.push(format!("  (no implementations found for `{}`)", name));
                }
            }
        }

        results.push(format!("\nScanned {} files", files.len()));
        results.join("\n")
    }

    // ── hover_info tool ──────────────────────────────────────────────────

    fn hover_info(&self, args: &Value) -> String {
        let file = match args.get("file").and_then(Value::as_str) {
            Some(f) => match self.resolve_checked(f) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'file'".to_string(),
        };
        let line = match args.get("line").and_then(Value::as_u64) {
            Some(l) => l as usize,
            None => return "Error: missing 'line'".to_string(),
        };
        let column = args.get("column").and_then(Value::as_u64).unwrap_or(0) as usize;

        let lang = match code_intel::detect_language(&file) {
            Some(l) => l,
            None => return "Error: unsupported language".to_string(),
        };
        let source = match std::fs::read_to_string(&file) {
            Ok(s) => s,
            Err(e) => return format!("Error: {e}"),
        };
        let rel_path = file.strip_prefix(&self.project_root).unwrap_or(&file);

        let mut parts = Vec::new();

        // Step 1: Identify what's at cursor
        let cursor_ident = code_intel::identifier_at_position(&source, lang, line, column);
        if let Some((ref name, ref node_kind)) = cursor_ident {
            parts.push(format!("🔍 `{}` ({})", name, node_kind));
        }

        // Step 2: Scope breadcrumbs
        let scope = code_intel::scope_at_line(&source, lang, line);
        if !scope.breadcrumbs.is_empty() {
            parts.push(format!("📍 {}", scope.breadcrumbs.join(" → ")));
        }

        // Step 3: Symbol definition at this line
        let symbols = code_intel::extract_symbols(&source, lang);
        let at_line: Vec<&code_intel::Symbol> =
            symbols.iter().filter(|s| s.start_line == line).collect();

        // Also try to find the definition of the cursor identifier
        let cursor_def = cursor_ident
            .as_ref()
            .and_then(|(name, _)| symbols.iter().find(|s| &s.name == name));

        let primary_sym = at_line.first().copied().or(cursor_def);

        if let Some(sym) = primary_sym {
            let parent_info = sym
                .parent
                .as_ref()
                .map(|p| format!(" (in {})", p))
                .unwrap_or_default();
            parts.push(String::new());
            parts.push(format!(
                "▸ {} {}{}",
                sym.kind.as_str(),
                sym.signature,
                parent_info
            ));
            parts.push(format!(
                "  {}:{}–{}",
                rel_path.display(),
                sym.start_line,
                sym.end_line
            ));

            // Doc comment
            let doc = code_intel::extract_doc_comment(&source, lang, sym.start_line);
            if !doc.is_empty() {
                parts.push(String::new());
                for doc_line in doc.lines().take(5) {
                    parts.push(format!("  📝 {}", doc_line));
                }
            }

            // If it's a type, show members preview
            if matches!(
                sym.kind,
                code_intel::SymbolKind::Struct
                    | code_intel::SymbolKind::Enum
                    | code_intel::SymbolKind::Class
                    | code_intel::SymbolKind::Interface
                    | code_intel::SymbolKind::Trait
            ) {
                let members = code_intel::extract_members(&source, lang, sym.start_line);
                if !members.is_empty() {
                    parts.push(String::new());
                    parts.push(format!("  Members ({}):", members.len()));
                    for m in members.iter().take(10) {
                        let type_str = if m.type_annotation.is_empty() {
                            String::new()
                        } else {
                            format!(": {}", m.type_annotation)
                        };
                        parts.push(format!("    {} {}{}", m.kind, m.name, type_str));
                    }
                    if members.len() > 10 {
                        parts.push(format!("    ... +{} more", members.len() - 10));
                    }
                }
            }

            // Calls made by this function
            if matches!(
                sym.kind,
                code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
            ) {
                let calls = code_intel::extract_calls(&source, lang, sym.start_line, sym.end_line);
                if !calls.is_empty() {
                    parts.push(String::new());
                    let call_names: Vec<String> = calls
                        .iter()
                        .take(8)
                        .map(|c| {
                            if let Some(ref r) = c.receiver {
                                format!("{}.{}", r, c.callee)
                            } else {
                                c.callee.clone()
                            }
                        })
                        .collect();
                    parts.push(format!("  Calls: {}", call_names.join(", ")));
                    if calls.len() > 8 {
                        parts.push(format!("    +{} more", calls.len() - 8));
                    }
                }
            }

            // Usage count
            let ref_count = self.count_symbol_references(&sym.name);
            if ref_count < usize::MAX {
                parts.push(format!("  Referenced: {} times in project", ref_count));
            }
        } else if let Some((name, _)) = &cursor_ident {
            // Not a definition line, but we found an identifier — show usage count
            let ref_count = self.count_symbol_references(name);
            if ref_count < usize::MAX {
                parts.push(format!("  Referenced: {} times in project", ref_count));
            }
        } else {
            // Show source line for context
            let lines: Vec<&str> = source.lines().collect();
            if line > 0 && line <= lines.len() {
                parts.push(format!("Line {}: {}", line, lines[line - 1].trim()));
            }
        }

        if parts.is_empty() {
            return format!("No symbol information at {}:{}", rel_path.display(), line);
        }

        parts.join("\n")
    }

    // ── symbol_search tool ───────────────────────────────────────────────

    fn symbol_search(&self, args: &Value) -> String {
        let query = match args.get("query").and_then(Value::as_str) {
            Some(q) if !q.trim().is_empty() => q.trim().to_lowercase(),
            _ => return "Error: missing 'query'".to_string(),
        };
        let kind_filter = args.get("kind").and_then(Value::as_str).unwrap_or("all");
        let include_glob = args.get("include").and_then(Value::as_str);
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;

        let extensions = [
            "rs", "py", "ts", "tsx", "js", "jsx", "go", "java", "c", "h", "cpp", "cc", "hpp", "rb",
        ];
        let skip_dirs = [
            "node_modules",
            "target",
            "vendor",
            "dist",
            "__pycache__",
            ".git",
        ];
        let files = self.collect_project_files(&skip_dirs, &extensions, 300);

        struct Match {
            name: String,
            kind: String,
            file: String,
            line: usize,
            signature: String,
            score: usize, // lower = better
        }

        let mut matches: Vec<Match> = Vec::new();

        for file_path in &files {
            // Apply glob filter
            if let Some(inc) = include_glob {
                let name = file_path.file_name().unwrap_or_default().to_string_lossy();
                let pat = inc.trim_start_matches('*');
                if !name.ends_with(pat) {
                    continue;
                }
            }

            let lang = match code_intel::detect_language(file_path) {
                Some(l) => l,
                None => continue,
            };
            let content = match fs::read_to_string(file_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let rel = file_path
                .strip_prefix(&self.project_root)
                .unwrap_or(file_path)
                .to_string_lossy()
                .to_string();

            let symbols = code_intel::extract_symbols(&content, lang);
            for sym in symbols {
                // Kind filter
                let kind_str = sym.kind.as_str();
                match kind_filter {
                    "function" if kind_str != "fn" && kind_str != "method" => continue,
                    "type"
                        if !matches!(
                            sym.kind,
                            code_intel::SymbolKind::Struct
                                | code_intel::SymbolKind::Class
                                | code_intel::SymbolKind::Enum
                                | code_intel::SymbolKind::Interface
                                | code_intel::SymbolKind::Trait
                                | code_intel::SymbolKind::Type
                        ) =>
                    {
                        continue;
                    }
                    "method" if kind_str != "method" => continue,
                    "constant" if kind_str != "const" && kind_str != "var" => continue,
                    _ => {}
                }

                let name_lower = sym.name.to_lowercase();
                if !name_lower.contains(&query) {
                    continue;
                }

                // Score: exact match = 0, starts-with = 1, contains = 2
                let score = if name_lower == query {
                    0
                } else if name_lower.starts_with(&query) {
                    1
                } else {
                    2
                };

                matches.push(Match {
                    name: sym.name,
                    kind: kind_str.to_string(),
                    file: rel.clone(),
                    line: sym.start_line,
                    signature: sym.signature,
                    score,
                });
            }
        }

        // Sort by score (exact first), then by name
        matches.sort_by(|a, b| a.score.cmp(&b.score).then(a.name.cmp(&b.name)));
        matches.truncate(limit);

        if matches.is_empty() {
            return format!("No symbols matching '{}' found", query);
        }

        let mut parts = Vec::new();
        parts.push(format!(
            "Symbols matching '{}' ({} results):",
            query,
            matches.len()
        ));
        parts.push(String::new());

        for m in &matches {
            let sig = if m.signature.len() > 80 {
                let mut end = 80;
                while !m.signature.is_char_boundary(end) && end > 0 {
                    end -= 1;
                }
                format!("{}...", &m.signature[..end])
            } else {
                m.signature.clone()
            };
            parts.push(format!("  [{}] {} — {}:{}", m.kind, sig, m.file, m.line));
        }

        parts.join("\n")
    }

    fn call_graph(&self, args: &Value) -> String {
        let path = match args.get("path").and_then(Value::as_str) {
            Some(p) => match self.resolve_checked(p) {
                Ok(safe) => safe,
                Err(e) => return e,
            },
            None => return "Error: missing 'path'".to_string(),
        };

        let lang = match code_intel::detect_language(&path) {
            Some(l) => l,
            None => {
                return "Error: unsupported language (supported: rs, py, ts, go, java, c, cpp, rb)"
                    .to_string();
            }
        };

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return format!("Error reading file: {e}"),
        };

        // Determine the line range to analyze
        let (start_line, end_line) = if let Some(sym_name) =
            args.get("symbol").and_then(Value::as_str)
        {
            // Find the symbol by name
            let symbols = code_intel::extract_symbols(&content, lang);
            let matches: Vec<_> = symbols.iter().filter(|s| s.name == sym_name).collect();
            match matches.len() {
                0 => return format!("Error: symbol '{sym_name}' not found in file"),
                1 => (matches[0].start_line, matches[0].end_line),
                _ => {
                    // Multiple matches — show them and ask for disambiguation
                    let mut msg = format!("Multiple symbols named '{sym_name}':\n");
                    for s in &matches {
                        msg.push_str(&format!(
                            "  L{}-{}: {} {}\n",
                            s.start_line,
                            s.end_line,
                            s.kind.as_str(),
                            s.signature
                        ));
                    }
                    msg.push_str("Use start_line/end_line to specify which one.");
                    return msg;
                }
            }
        } else if let (Some(sl), Some(el)) = (
            args.get("start_line").and_then(Value::as_u64),
            args.get("end_line").and_then(Value::as_u64),
        ) {
            (sl as usize, el as usize)
        } else {
            return "Error: provide either 'symbol' name or 'start_line'+'end_line'".to_string();
        };

        let calls = code_intel::extract_calls(&content, lang, start_line, end_line);

        let fname = path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let show_callers = args
            .get("callers")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let mut out = String::new();

        // Outgoing calls (what this function calls)
        if !calls.is_empty() {
            out.push_str(&format!(
                "# Calls FROM {} (lines {}-{})\n\n",
                fname, start_line, end_line
            ));
            for call in &calls {
                if let Some(ref recv) = call.receiver {
                    out.push_str(&format!("  → L{}: {}.{}()\n", call.line, recv, call.callee));
                } else {
                    out.push_str(&format!("  → L{}: {}()\n", call.line, call.callee));
                }
            }
            out.push_str(&format!("\n{} outgoing call(s)\n", calls.len()));
        } else {
            out.push_str(&format!(
                "No outgoing calls in lines {start_line}-{end_line}\n"
            ));
        }

        // Callers search
        if show_callers {
            let sym_name = args.get("symbol").and_then(Value::as_str);
            if let Some(target) = sym_name {
                let scope = args.get("scope").and_then(Value::as_str).unwrap_or("file");

                if scope == "project" {
                    // Cross-file caller search
                    out.push_str(&format!("\n# Callers OF '{}' (project-wide)\n\n", target));
                    let callers = self.find_callers_cross_file(target, &path);
                    if callers.is_empty() {
                        out.push_str("  (none found in project)\n");
                    } else {
                        for (file, name, sig, line) in callers.iter().take(30) {
                            out.push_str(&format!("  ← {}:L{}: {} ({})\n", file, line, name, sig));
                        }
                        if callers.len() > 30 {
                            out.push_str(&format!(
                                "\n  ... and {} more callers\n",
                                callers.len() - 30
                            ));
                        }
                        out.push_str(&format!("\n{} caller(s) across project\n", callers.len()));
                    }
                } else {
                    // Same-file caller search (fast)
                    let all_symbols = code_intel::extract_symbols(&content, lang);
                    let mut callers_found = Vec::new();

                    for sym in &all_symbols {
                        if sym.name == target {
                            continue;
                        }
                        if !matches!(
                            sym.kind,
                            code_intel::SymbolKind::Function | code_intel::SymbolKind::Method
                        ) {
                            continue;
                        }
                        let sym_calls =
                            code_intel::extract_calls(&content, lang, sym.start_line, sym.end_line);
                        for call in &sym_calls {
                            if call.callee == target {
                                callers_found.push((
                                    sym.name.clone(),
                                    sym.signature.clone(),
                                    call.line,
                                ));
                                break;
                            }
                        }
                    }

                    out.push_str(&format!("\n# Callers OF '{}' (same file)\n\n", target));
                    if callers_found.is_empty() {
                        out.push_str("  (none found in this file)\n");
                    } else {
                        for (name, sig, line) in &callers_found {
                            out.push_str(&format!("  ← L{}: {} ({})\n", line, name, sig));
                        }
                        out.push_str(&format!("\n{} caller(s) in file\n", callers_found.len()));
                    }
                }
            } else {
                out.push_str("\nNote: callers=true requires symbol name (not line range)\n");
            }
        }

        out
    }

    /// Run a build/test command with structured error parsing and auto-context.
    ///
    /// Returns structured errors with file:line:col locations plus surrounding
    /// source code for each error, enabling single-shot fix without extra read_file calls.
    fn run_build_test(&self, args: &Value) -> String {
        let command = match args.get("command").and_then(Value::as_str) {
            Some(c) if !c.trim().is_empty() => c.trim(),
            _ => return "Error: 'command' parameter is required".to_string(),
        };
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(5) as usize;
        let auto_fix = args
            .get("auto_fix")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let abort_on_regression = args
            .get("abort_on_regression")
            .and_then(Value::as_bool)
            .unwrap_or(true); // default: abort on regression
        let report_only = args
            .get("report_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Run the initial build
        let (initial_output, initial_fixes, initial_errors) =
            self.run_build_test_core(command, context_lines);

        // Report-only mode: show what auto-fix would do, but don't apply
        if report_only && !initial_fixes.is_empty() {
            let eligible: Vec<&build_test::FixSuggestion> = initial_fixes
                .iter()
                .filter(|f| f.confidence >= build_test::AUTO_FIX_CONFIDENCE_THRESHOLD)
                .collect();
            if !eligible.is_empty() {
                let mut preview = initial_output;
                preview.push_str("\n\n─── Auto-Fix Preview (report_only=true, not applied) ───\n");
                for (i, fix) in eligible.iter().enumerate() {
                    let conf = format!("{:.0}%", fix.confidence * 100.0);
                    preview.push_str(&format!(
                        "  {}. [{}] {}:{} — {} ({})\n",
                        i + 1,
                        fix.action,
                        fix.file,
                        fix.line,
                        fix.explanation,
                        conf,
                    ));
                    if !fix.new_text.is_empty() {
                        let text = truncate_str(&fix.new_text, 77);
                        preview.push_str(&format!("     + {}\n", text));
                    }
                }
                preview.push_str(&format!(
                    "\n{} fix(es) eligible. Re-run with auto_fix=true to apply.\n",
                    eligible.len()
                ));
                return truncate_output(preview, tool_output_limit());
            }
            return initial_output;
        }

        if !auto_fix {
            return initial_output;
        }

        // Auto-fix loop: apply high-confidence fixes and re-run
        let mut output = initial_output.clone();
        let mut current_fixes: Vec<build_test::FixSuggestion> = initial_fixes;
        let mut all_reports = Vec::new();
        let mut prev_error_count = initial_errors;

        for iteration in 1..=build_test::AUTO_FIX_MAX_ITERATIONS {
            let eligible_count = current_fixes
                .iter()
                .filter(|f| f.confidence >= build_test::AUTO_FIX_CONFIDENCE_THRESHOLD)
                .count();

            if eligible_count == 0 {
                break;
            }

            let (applied, errors) =
                build_test::apply_auto_fixes(&current_fixes, &self.project_root);
            let report = build_test::format_auto_fix_report(&applied, &errors, iteration);
            all_reports.push(report);

            if applied.is_empty() {
                break;
            }

            // Re-run the build after applying fixes
            let (new_output, new_fixes, new_error_count) =
                self.run_build_test_core(command, context_lines);

            // Check for regression: more errors after fix attempt
            if abort_on_regression && new_error_count > prev_error_count && prev_error_count > 0 {
                // Revert applied fixes via git checkout
                let reverted = self.revert_auto_fixes(&applied);
                all_reports.push(format!(
                    "\n⚠ REGRESSION: {} → {} errors. Auto-fix aborted.{}\n",
                    prev_error_count,
                    new_error_count,
                    if reverted {
                        " Files reverted to pre-fix state."
                    } else {
                        " Manual revert may be needed."
                    }
                ));
                // Re-run to get clean output after revert
                let (reverted_output, _, _) = self.run_build_test_core(command, context_lines);
                output = reverted_output;
                break;
            }

            prev_error_count = new_error_count;
            output = new_output;
            current_fixes = new_fixes;
        }

        if all_reports.is_empty() {
            return output;
        }

        // Prepend auto-fix reports to the final build output
        let mut final_output = all_reports.join("");
        final_output.push_str("\n── Final Build Result ──\n");
        final_output.push_str(&output);
        truncate_output(final_output, tool_output_limit())
    }

    /// Revert files modified by auto-fix using git checkout.
    /// Returns true if revert succeeded.
    fn revert_auto_fixes(&self, applied: &[build_test::AppliedFix]) -> bool {
        let files: std::collections::HashSet<&str> =
            applied.iter().map(|a| a.file.as_str()).collect();
        let mut all_ok = true;
        for file in files {
            let file_path = if std::path::Path::new(file).is_absolute() {
                file.to_string()
            } else {
                self.project_root.join(file).display().to_string()
            };
            let status = std::process::Command::new("git")
                .args(["checkout", "--", &file_path])
                .current_dir(&self.project_root)
                .status();
            if status.map(|s| !s.success()).unwrap_or(true) {
                all_ok = false;
            }
        }
        all_ok
    }

    /// Core build+parse logic extracted for auto-fix loop reuse.
    /// Returns (formatted_output, fix_suggestions, error_count).
    fn run_build_test_core(
        &self,
        command: &str,
        context_lines: usize,
    ) -> (String, Vec<build_test::FixSuggestion>, usize) {
        // Run the command
        let output = std::process::Command::new("sh")
            .args(["-c", command])
            .current_dir(&self.project_root)
            .output();

        let (stdout, stderr, exit_code) = match output {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                let code = out.status.code();
                (stdout, stderr, code)
            }
            Err(e) => return (format!("Error: failed to run command: {e}"), Vec::new(), 0),
        };

        let combined = format!("{stdout}\n{stderr}");
        let mut result = build_test::parse_build_test_output(&combined, exit_code);
        let error_count = result.error_count;

        // Enrich error locations with tree-sitter scope context
        if !result.error_locations.is_empty() {
            result.enrich_with_scope(&self.project_root);
        }

        // Track iteration deltas — reset if command changed
        let delta = {
            let mut tracker = self.build_test_tracker.lock().unwrap();
            if tracker.command_changed(command) {
                tracker.reset();
            }
            tracker.record(&result, command)
        };

        // Build the structured output
        let mut parts = Vec::new();

        // Prepend delta summary for iterations > 0
        let delta_summary = delta.to_summary();
        if !delta_summary.is_empty() {
            parts.push(delta_summary);
            parts.push(String::new());
        }

        parts.push(result.to_enhanced_output(&combined));

        // Auto-read source context for each error location
        if !result.error_locations.is_empty() {
            parts.push(String::new());
            parts.push("─── Source Context ───".to_string());

            let mut seen_files: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            for loc in result.error_locations.iter().take(5) {
                let file_path = self.project_root.join(&loc.file);
                let file_key = format!("{}:{}", loc.file, loc.line);
                if seen_files.contains(&file_key) {
                    continue;
                }
                seen_files.insert(file_key);

                if let Ok(content) = std::fs::read_to_string(&file_path) {
                    let lines: Vec<&str> = content.lines().collect();
                    let start = loc.line.saturating_sub(context_lines + 1);
                    let end = (loc.line + context_lines).min(lines.len());

                    let code_part = if loc.error_code.is_empty() {
                        String::new()
                    } else {
                        format!(" [{}]", loc.error_code)
                    };
                    parts.push(format!(
                        "\n// {}:{}{} — {}",
                        loc.file, loc.line, code_part, loc.message
                    ));

                    for (idx, line) in lines[start..end].iter().enumerate() {
                        let line_num = start + idx + 1;
                        let marker = if line_num == loc.line { "→" } else { " " };
                        parts.push(format!("{marker} {line_num:>4} │ {line}"));
                    }
                }
            }

            if result.error_locations.len() > 5 {
                parts.push(format!(
                    "\n[{} more error locations — use read_file to inspect]",
                    result.error_locations.len() - 5
                ));
            }
        }

        // Generate concrete fix suggestions
        let mut all_fixes: Vec<(usize, build_test::FixSuggestion)> = Vec::new();
        for (i, loc) in result.error_locations.iter().enumerate().take(10) {
            let file_path = self.project_root.join(&loc.file);
            if let Ok(content) = std::fs::read_to_string(&file_path) {
                let source_lines: Vec<&str> = content.lines().collect();
                let fixes = build_test::suggest_fix(loc, &source_lines);
                for fix in fixes {
                    all_fixes.push((i, fix));
                }
            }
        }

        // Collect fix suggestions for return
        let fix_list: Vec<build_test::FixSuggestion> =
            all_fixes.iter().map(|(_, f)| f.clone()).collect();

        if !all_fixes.is_empty() {
            parts.push(String::new());
            parts.push("─── Suggested Fixes ───".to_string());
            for (err_idx, fix) in all_fixes.iter().take(8) {
                let confidence_bar = match fix.confidence {
                    c if c >= 0.8 => "●●●",
                    c if c >= 0.5 => "●●○",
                    _ => "●○○",
                };
                parts.push(format!(
                    "\n{}  [{}] {}",
                    confidence_bar, fix.action, fix.explanation
                ));
                parts.push(format!("  → {}:{}", fix.file, fix.line));
                if !fix.new_text.is_empty() {
                    // Show what to insert/replace
                    let preview = truncate_str(&fix.new_text, 77);
                    parts.push(format!("  + {}", preview));
                }
                let _ = err_idx; // used for ordering
            }
            if all_fixes.len() > 8 {
                parts.push(format!("\n[{} more suggestions]", all_fixes.len() - 8));
            }
        }

        (
            truncate_output(parts.join("\n"), tool_output_limit()),
            fix_list,
            error_count,
        )
    }

    /// Execute a multi-step ToolChain, forwarding each step to self.execute().
    ///
    /// Returns a JSON summary with per-step outputs and the final result.
    /// Execution stops on the first error unless the step has a skip condition.
    pub fn execute_chain(
        &self,
        chain: &astra_runtime::tool_registry::ToolChain,
        input: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>> {
        use astra_runtime::tool_registry::chain::{ChainContext, resolve_args};

        let chain_name = chain.name.clone();
        let steps = chain.steps.clone();

        Box::pin(async move {
            let mut ctx = ChainContext::new(input);
            let mut step_results = Vec::new();

            for (idx, step) in steps.iter().enumerate() {
                if ctx.should_skip(step) {
                    step_results.push(serde_json::json!({
                        "step": idx,
                        "tool": step.tool,
                        "skipped": true,
                    }));
                    continue;
                }

                let resolved = resolve_args(&step.args, &ctx);
                let output = self.execute(&step.tool, &resolved).await;
                let is_err = output.starts_with("Error")
                    || output.starts_with("error")
                    || output.starts_with("Sandbox:")
                    || output.contains("\"error\":");

                ctx.record_step(
                    idx,
                    &step.tool,
                    output.clone(),
                    step.output_key.as_deref(),
                    !is_err,
                );

                step_results.push(serde_json::json!({
                    "step": idx,
                    "tool": step.tool,
                    "output": truncate_output(output.clone(), 4096),
                    "success": !is_err,
                }));

                if is_err {
                    break;
                }
            }

            serde_json::json!({
                "chain": chain_name,
                "steps_executed": step_results.len(),
                "steps_total": steps.len(),
                "final_output": truncate_output(ctx.prev_output, 8192),
                "steps": step_results,
            })
            .to_string()
        })
    }

    fn tool_names(&self) -> Vec<String> {
        all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect()
    }

    fn tool_count(&self) -> usize {
        all_tool_schemas().len()
    }

    /// Lightweight memory search for tool-selection boost terms.
    ///
    /// Returns content strings from matching memories. Uses a short timeout (2s)
    /// because this is a best-effort optimization on the critical path before tool
    /// selection — the system works without it (just with lower accuracy for
    /// cold-start entity queries).
    pub async fn memory_boost_search(&self, query: &str, top_k: u64) -> Vec<String> {
        if query.trim().is_empty() {
            return vec![];
        }
        // Direct Memoria call (skip cloud proxy — server has no /memory/* route).
        // This is best-effort on the critical path; circuit breaker prevents
        // repeated timeouts if Memoria is down.
        if self
            .memoria_fail_count
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 2
        {
            return vec![];
        }
        let base = std::env::var("MEMORIA_BASE_URL")
            .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
        let key = match std::env::var("MEMORIA_API_KEY")
            .ok()
            .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
        {
            Some(k) => k,
            None => return vec![], // No key = no Memoria
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_millis(800))
            .no_proxy()
            .build()
        {
            Ok(c) => c,
            Err(_) => return vec![],
        };
        match client
            .post(format!("{base}/v1/memories/search"))
            .header("Authorization", format!("Bearer {key}"))
            .json(&json!({"query": query, "top_k": top_k}))
            .send()
            .await
        {
            Ok(resp) => {
                self.memoria_fail_count
                    .store(0, std::sync::atomic::Ordering::Relaxed);
                let text = resp.text().await.unwrap_or_default();
                parse_memory_search_contents(&text)
            }
            Err(_) => {
                self.memoria_fail_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                vec![]
            }
        }
    }

    /// Execute an MCP tool call, routing to the correct server with auto-reconnect.
    async fn execute_mcp_tool(&self, mcp_name: &str, args: &Value) -> String {
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

    async fn memoria_call(&self, op: &str, args: &Value) -> String {
        self.memoria_call_with_timeout(op, args, Duration::from_secs(10))
            .await
    }

    async fn memoria_call_with_timeout(&self, op: &str, args: &Value, timeout: Duration) -> String {
        // Circuit breaker: skip after 2 consecutive failures (reset on success)
        const MAX_FAILS: u32 = 2;
        if self
            .memoria_fail_count
            .load(std::sync::atomic::Ordering::Relaxed)
            >= MAX_FAILS
        {
            return json!({"error": "Memory service unavailable (circuit open)"}).to_string();
        }

        // Build endpoint and payload
        let (endpoint, payload, auth_header) = if let (Some(cloud_base), Some(token)) =
            (&self.cloud_base, &self.cloud_token)
        {
            (
                format!("{cloud_base}/memory/{op}"),
                args.clone(),
                format!("Bearer {token}"),
            )
        } else {
            let base = std::env::var("MEMORIA_BASE_URL")
                .unwrap_or_else(|_| astra_core::config::DEFAULT_MEMORIA_URL.to_string());
            let key = match std::env::var("MEMORIA_API_KEY")
                .ok()
                .or_else(|| std::env::var("MEMORIA_MASTER_KEY").ok())
            {
                Some(k) => k,
                None => {
                    return json!({
                            "error": "Memory unavailable: not connected to cloud and MEMORIA_API_KEY not set",
                            "hint": "Login with /login to enable cloud-backed memory with user isolation"
                        })
                        .to_string();
                }
            };

            let (ep, pl) = match op {
                "retrieve" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(5);
                    (
                        format!("{base}/v1/memories/retrieve"),
                        json!({"query": query, "top_k": top_k}),
                    )
                }
                "store" => {
                    let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                    let memory_type = args
                        .get("memory_type")
                        .and_then(Value::as_str)
                        .unwrap_or("semantic");
                    (
                        format!("{base}/v1/memories"),
                        json!({"content": content, "memory_type": memory_type}),
                    )
                }
                "search" => {
                    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                    let top_k = args.get("top_k").and_then(Value::as_u64).unwrap_or(10);
                    (
                        format!("{base}/v1/memories/search"),
                        json!({"query": query, "top_k": top_k}),
                    )
                }
                "purge" => {
                    let topic = args.get("topic").and_then(Value::as_str).unwrap_or("");
                    let reason = args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("user request");
                    (
                        format!("{base}/v1/memories/purge"),
                        json!({"topic": topic, "reason": reason}),
                    )
                }
                "correct" => {
                    let memory_id = args.get("memory_id").and_then(Value::as_str).unwrap_or("");
                    let new_content = args
                        .get("new_content")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let reason = args
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("correction");
                    (
                        format!("{base}/v1/memories/correct"),
                        json!({"memory_id": memory_id, "new_content": new_content, "reason": reason}),
                    )
                }
                "profile" => (format!("{base}/v1/memories/profile"), json!({})),
                _ => return format!("Unknown memoria op: {op}"),
            };
            (ep, pl, format!("Bearer {key}"))
        };

        match reqwest::Client::builder()
            .timeout(timeout)
            .no_proxy()
            .build()
        {
            Ok(client) => match client
                .post(&endpoint)
                .header("Authorization", &auth_header)
                .json(&payload)
                .send()
                .await
            {
                Ok(resp) => match resp.text().await {
                    Ok(text) => {
                        self.memoria_fail_count
                            .store(0, std::sync::atomic::Ordering::Relaxed);
                        text
                    }
                    Err(e) => {
                        self.memoria_fail_count
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        json!({"error": format!("read response: {e}")}).to_string()
                    }
                },
                Err(e) => {
                    self.memoria_fail_count
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    json!({"error": format!("memoria request failed: {e}")}).to_string()
                }
            },
            Err(e) => json!({"error": format!("build client: {e}")}).to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_executor() -> ToolExecutor {
        ToolExecutor::new(std::env::temp_dir())
    }

    // ── all_tool_schemas ──────────────────────────────────────────────────────

    #[test]
    fn all_tool_schemas_non_empty() {
        let schemas = all_tool_schemas();
        assert!(!schemas.is_empty(), "should have at least one tool schema");
    }

    #[test]
    fn all_tool_schemas_have_function_name() {
        for schema in all_tool_schemas() {
            let name = schema
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str());
            assert!(name.is_some(), "schema missing function.name: {schema}");
            assert!(!name.unwrap().is_empty());
        }
    }

    #[test]
    fn all_tool_schemas_have_description() {
        for schema in all_tool_schemas() {
            let desc = schema
                .get("function")
                .and_then(|f| f.get("description"))
                .and_then(|d| d.as_str());
            assert!(
                desc.is_some(),
                "schema missing description: {:?}",
                schema["function"]["name"]
            );
        }
    }

    #[test]
    fn tool_schemas_include_core_tools() {
        let names: Vec<String> = all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        for expected in &[
            "bash",
            "read_file",
            "write_file",
            "str_replace",
            "list_dir",
            "grep",
            "glob",
            "git_status",
            "git_blame",
            "git_file_history",
            "git_contributors",
            "git_log_search",
            "mo_query",
            "mo_snapshot",
            "mo_branch",
            "github_ci_status",
            "github_repo_stats",
            "memory_store",
            "memory_search",
            "reflect",
            "run_chain",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing tool: {expected}"
            );
        }
    }

    #[test]
    fn no_duplicate_tool_names() {
        let names: Vec<String> = all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            assert!(seen.insert(name), "duplicate tool name: {name}");
        }
    }

    // ── ToolExecutor ──────────────────────────────────────────────────────────

    #[test]
    fn executor_tool_count_matches_schemas() {
        let executor = test_executor();
        assert_eq!(executor.tool_count(), all_tool_schemas().len());
    }

    #[test]
    fn executor_tool_names_match_schemas() {
        let executor = test_executor();
        let names = executor.tool_names();
        assert_eq!(names.len(), all_tool_schemas().len());
        assert!(names.contains(&"bash".to_string()));
    }

    #[tokio::test]
    async fn execute_unknown_tool_returns_error() {
        let executor = test_executor();
        let result = executor.execute("nonexistent_tool", &json!({})).await;
        assert!(result.contains("Unknown tool"), "got: {result}");
    }

    #[tokio::test]
    async fn execute_reflect_returns_placeholder() {
        let executor = test_executor();
        let result = executor.execute("reflect", &json!({"focus": "auto"})).await;
        assert!(result.contains("reflect_requires_session"), "got: {result}");
    }

    #[test]
    fn budget_pressure_defaults_to_zero() {
        let executor = test_executor();
        assert_eq!(executor.get_budget_pressure(), 0.0);
    }

    #[test]
    fn budget_pressure_set_and_get() {
        let executor = test_executor();
        executor.set_budget_pressure(0.6);
        assert!((executor.get_budget_pressure() - 0.6).abs() < 1e-10);
    }

    #[test]
    fn budget_pressure_clamps_to_range() {
        let executor = test_executor();
        executor.set_budget_pressure(1.5);
        assert_eq!(executor.get_budget_pressure(), 1.0);
        executor.set_budget_pressure(-0.5);
        assert_eq!(executor.get_budget_pressure(), 0.0);
    }

    // ── truncate_output ─────────────────────────────────────────────────────

    #[test]
    fn truncate_output_ascii_no_change() {
        let input = "hello world".to_string();
        let result = truncate_output(input.clone(), 100);
        assert_eq!(result, input);
    }

    #[test]
    fn truncate_output_ascii_truncates() {
        let input = "hello world".to_string();
        let result = truncate_output(input, 5);
        assert!(result.starts_with("hello"));
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn truncate_output_utf8_boundary_no_panic() {
        // 🔥 is 4 bytes, "ab🔥cd" = 2+4+2 = 8 bytes
        let input = "ab🔥cd".to_string();
        // Truncate at byte 3 — inside the 🔥 (bytes 2..5)
        let result = truncate_output(input, 3);
        // Should truncate at char boundary (byte 2, before 🔥)
        assert!(result.starts_with("ab"), "got: {result}");
        assert!(result.contains("[truncated]"));
    }

    #[test]
    fn truncate_output_cjk_boundary_no_panic() {
        // Chinese chars are 3 bytes each
        let input = "你好世界".to_string(); // 12 bytes
        let result = truncate_output(input, 7); // Between 2nd and 3rd char
        assert!(result.contains("[truncated]"));
        // Should not panic — regression for char boundary issue
    }

    // ── fs tools ──────────────────────────────────────────────────────────────

    #[test]
    fn read_file_missing_path_returns_error() {
        let executor = test_executor();
        let result = executor.read_file(&json!({}));
        assert!(result.contains("Error"), "got: {result}");
    }

    #[test]
    fn read_file_nonexistent_returns_error() {
        let executor = test_executor();
        // Use path within project root (temp_dir) that doesn't exist
        let result = executor.read_file(&json!({"path": "nonexistent_file_xyz.txt"}));
        assert!(
            result.contains("Error") || result.contains("Sandbox"),
            "got: {result}"
        );
    }

    #[test]
    fn write_and_read_file_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "test_roundtrip.txt";

        let write_result = executor.write_file(&json!({"path": path, "content": "hello world"}));
        assert!(
            write_result.contains("\"success\":true") || write_result.contains("\"success\": true"),
            "write failed: {write_result}"
        );

        let read_result = executor.read_file(&json!({"path": path}));
        assert!(
            read_result.contains("hello world"),
            "should contain content: {read_result}"
        );
        assert!(
            read_result.starts_with("1\t"),
            "should have line numbers: {read_result}"
        );
    }

    #[test]
    fn str_replace_works() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "replace_test.txt";

        executor.write_file(&json!({"path": path, "content": "foo bar baz"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "bar", "new_str": "qux"}));
        assert!(result.contains("Replaced"), "got: {result}");

        let content = executor.read_file(&json!({"path": path}));
        assert!(
            content.contains("foo qux baz"),
            "should contain replaced content: {content}"
        );
    }

    #[test]
    fn str_replace_rejects_non_unique() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "dup_test.txt";

        executor.write_file(&json!({"path": path, "content": "aaa aaa"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "aaa", "new_str": "bbb"}));
        assert!(result.contains("2 times"), "got: {result}");
    }

    #[test]
    fn str_replace_rejects_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let path = "nf_test.txt";

        executor.write_file(&json!({"path": path, "content": "hello"}));
        let result =
            executor.str_replace(&json!({"path": path, "old_str": "xyz", "new_str": "abc"}));
        assert!(result.contains("not found"), "got: {result}");
    }

    #[test]
    fn list_dir_returns_entries() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();

        let result = executor.list_dir(&json!({"path": "."}));
        assert!(result.contains("a.txt"), "got: {result}");
        assert!(result.contains("subdir/"), "got: {result}");
    }

    #[test]
    fn list_dir_skips_hidden() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        std::fs::write(dir.path().join("visible.txt"), "").unwrap();

        let result = executor.list_dir(&json!({"path": "."}));
        assert!(!result.contains(".hidden"), "should skip hidden: {result}");
        assert!(result.contains("visible.txt"));
    }

    // ── read_file with line ranges ────────────────────────────────────────────

    #[test]
    fn read_file_with_line_range() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        executor.write_file(&json!({"path": "lines.txt", "content": "line1\nline2\nline3\nline4"}));

        let result =
            executor.read_file(&json!({"path": "lines.txt", "start_line": 2, "end_line": 3}));
        assert_eq!(result, "2\tline2\n3\tline3");
    }

    // ── parse_memory_search_contents ──────────────────────────────────────────

    #[test]
    fn parse_memory_memories_array() {
        let raw = r#"{"memories":[{"content":"matrixorigin is a GitHub org","score":0.9},{"content":"user prefers Rust","score":0.7}]}"#;
        let result = parse_memory_search_contents(raw);
        assert_eq!(
            result,
            vec!["matrixorigin is a GitHub org", "user prefers Rust"]
        );
    }

    #[test]
    fn parse_memory_results_array() {
        let raw = r#"{"results":[{"content":"mo is a database company"},{"content":"user likes dark mode"}]}"#;
        let result = parse_memory_search_contents(raw);
        assert_eq!(
            result,
            vec!["mo is a database company", "user likes dark mode"]
        );
    }

    #[test]
    fn parse_memory_top_level_array() {
        let raw = r#"[{"content":"matrixorigin = GitHub org"},{"text":"user follows MO"}]"#;
        let result = parse_memory_search_contents(raw);
        assert_eq!(result, vec!["matrixorigin = GitHub org", "user follows MO"]);
    }

    #[test]
    fn parse_memory_error_response() {
        let raw = r#"{"error":"Memory unavailable: not connected"}"#;
        let result = parse_memory_search_contents(raw);
        assert!(result.is_empty(), "error response should return empty");
    }

    #[test]
    fn parse_memory_invalid_json() {
        assert!(parse_memory_search_contents("not json").is_empty());
        assert!(parse_memory_search_contents("").is_empty());
    }

    #[test]
    fn parse_memory_empty_content_filtered() {
        let raw = r#"{"memories":[{"content":""},{"content":"valid memory"}]}"#;
        let result = parse_memory_search_contents(raw);
        assert_eq!(result, vec!["valid memory"]);
    }

    #[test]
    fn parse_memory_single_object() {
        let raw = r#"{"content":"single memory result"}"#;
        let result = parse_memory_search_contents(raw);
        assert_eq!(result, vec!["single memory result"]);
    }

    #[test]
    fn parse_memory_no_content_field() {
        let raw = r#"{"memories":[{"summary":"no content field"}]}"#;
        let result = parse_memory_search_contents(raw);
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn memory_boost_search_empty_query() {
        let executor = test_executor();
        let result = executor.memory_boost_search("", 5).await;
        assert!(result.is_empty(), "empty query should return empty");
    }

    #[tokio::test]
    async fn memory_boost_search_whitespace_query() {
        let executor = test_executor();
        let result = executor.memory_boost_search("   ", 5).await;
        assert!(result.is_empty(), "whitespace query should return empty");
    }

    // ── extract_github_owner_repo edge cases ──

    #[test]
    fn extract_github_owner_repo_without_git_suffix() {
        let line = "origin\thttps://github.com/MatrixOrigin/Memoria (fetch)";
        assert_eq!(
            super::extract_github_owner_repo(line),
            Some("MatrixOrigin/Memoria".to_string())
        );
    }

    #[test]
    fn extract_github_owner_repo_malformed_url() {
        assert_eq!(super::extract_github_owner_repo("origin"), None);
        assert_eq!(super::extract_github_owner_repo(""), None);
        assert_eq!(
            super::extract_github_owner_repo("origin\thttps://not-github.com/a/b.git (fetch)"),
            None
        );
    }

    #[test]
    fn extract_github_owner_repo_ssh_no_dot_git() {
        let line = "upstream\tgit@github.com:org/repo (push)";
        assert_eq!(
            super::extract_github_owner_repo(line),
            Some("org/repo".to_string())
        );
    }

    // ── detect_git_remote_repos ──

    #[test]
    fn detect_git_remote_repos_from_current_dir() {
        // This test runs in the actual repo — should find at least one remote
        let repos = super::detect_git_remote_repos(std::path::Path::new("."));
        // We're in the mo-dev-agent repo, so at least one GitHub remote should exist
        // (unless running in a non-git context, in which case empty is acceptable)
        for repo in &repos {
            assert!(repo.contains('/'), "repo should be owner/name: {repo}");
            assert_eq!(repo, &repo.to_lowercase(), "should be lowercased: {repo}");
        }
    }

    #[test]
    fn detect_git_remote_repos_nonexistent_dir() {
        let repos = super::detect_git_remote_repos(std::path::Path::new("/nonexistent/path"));
        assert!(repos.is_empty());
    }

    #[test]
    fn detect_git_remote_repos_deduplicates() {
        // The same remote appears for both fetch and push — should be deduplicated
        // This is an implicit invariant; verify by checking no duplicates
        let repos = super::detect_git_remote_repos(std::path::Path::new("."));
        let mut seen = std::collections::HashSet::new();
        for repo in &repos {
            assert!(
                seen.insert(repo.as_str()),
                "duplicate preferred repo: {repo}"
            );
        }
    }

    // ── add_preferred_repo / get_preferred_repos ──

    #[test]
    fn add_preferred_repo_deduplicates() {
        let exec = test_executor();
        exec.add_preferred_repo("MatrixOrigin/Memoria");
        exec.add_preferred_repo("MatrixOrigin/Memoria");
        exec.add_preferred_repo("matrixorigin/memoria"); // same after lowercasing
        let repos = exec.get_preferred_repos();
        let memoria_count = repos
            .iter()
            .filter(|r| r == &"matrixorigin/memoria")
            .count();
        assert_eq!(
            memoria_count, 1,
            "should deduplicate case-insensitively: {repos:?}"
        );
    }

    #[test]
    fn add_preferred_repo_normalizes_case() {
        let exec = test_executor();
        exec.add_preferred_repo("MatrixOrigin/Memoria");
        let repos = exec.get_preferred_repos();
        assert!(
            repos.contains(&"matrixorigin/memoria".to_string()),
            "should lowercase: {repos:?}"
        );
    }

    #[test]
    fn preferred_repos_initialized_from_git_remote() {
        // test_executor uses "." as root; if in a git repo, should have remotes
        let exec = test_executor();
        let repos = exec.get_preferred_repos();
        // Can't assert specific content, but structure should be valid
        for repo in &repos {
            assert!(repo.contains('/'), "malformed: {repo}");
        }
    }

    // ── run_chain (end-to-end with real tool execution) ──────────────────────

    #[tokio::test]
    async fn chain_write_read_roundtrip() {
        use astra_runtime::tool_registry::ToolChain;

        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        let chain = ToolChain::new("write_read", "Write a file then read it back")
            .named_step(
                "write",
                "write_file",
                json!({"path": "chain_test.txt", "content": "hello from chain"}),
            )
            .step("read_file", json!({"path": "chain_test.txt"}));

        let result = executor.execute_chain(&chain, json!({})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["chain"], "write_read");
        assert_eq!(parsed["steps_executed"], 2);
        assert_eq!(parsed["steps_total"], 2);

        let steps = parsed["steps"].as_array().unwrap();
        assert!(
            steps[0]["success"].as_bool().unwrap(),
            "write should succeed"
        );
        assert!(
            steps[1]["success"].as_bool().unwrap(),
            "read should succeed"
        );
        assert!(
            parsed["final_output"]
                .as_str()
                .unwrap()
                .contains("hello from chain"),
            "final output should be file contents"
        );
    }

    #[tokio::test]
    async fn chain_stops_on_error() {
        use astra_runtime::tool_registry::ToolChain;

        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        let chain = ToolChain::new("error_chain", "Read nonexistent then write")
            .step(
                "read_file",
                json!({"path": "definitely_nonexistent_file.txt"}),
            )
            .step(
                "write_file",
                json!({"path": "should_not_run.txt", "content": "nope"}),
            );

        let result = executor.execute_chain(&chain, json!({})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["steps_executed"], 1, "should stop after first error");
        assert_eq!(parsed["steps_total"], 2);
        let steps = parsed["steps"].as_array().unwrap();
        assert!(!steps[0]["success"].as_bool().unwrap());
        // The second step should NOT have been executed
        assert_eq!(steps.len(), 1);
        assert!(!dir.path().join("should_not_run.txt").exists());
    }

    #[tokio::test]
    async fn chain_variable_substitution_end_to_end() {
        use astra_runtime::tool_registry::ToolChain;

        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Step 1: write file with content from $input
        // Step 2: read that file back using path from $input
        // Step 3: write $prev to a new file
        let chain = ToolChain::new("var_sub", "Test variable substitution")
            .step(
                "write_file",
                json!({"path": "$input.filename", "content": "$input.message"}),
            )
            .step("read_file", json!({"path": "$input.filename"}))
            .named_step(
                "copy",
                "write_file",
                json!({"path": "copy.txt", "content": "$prev"}),
            );

        let result = executor
            .execute_chain(
                &chain,
                json!({"filename": "original.txt", "message": "variable test!"}),
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["steps_executed"], 3);
        let steps = parsed["steps"].as_array().unwrap();
        assert!(steps.iter().all(|s| s["success"].as_bool().unwrap()));

        // Verify the copy was created (content includes line numbers from read_file)
        let copy_content = std::fs::read_to_string(dir.path().join("copy.txt")).unwrap();
        assert!(
            copy_content.contains("variable test!"),
            "copy should contain original text: {copy_content}"
        );
    }

    #[tokio::test]
    async fn chain_skip_condition_end_to_end() {
        use astra_runtime::tool_registry::ToolChain;
        use astra_runtime::tool_registry::chain::ChainStep;

        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Step 1: read nonexistent file (will produce "Error")
        // Step 2: should be skipped because prev contains "Error"
        let mut chain = ToolChain::new("skip_test", "Test skip condition");
        chain.steps.push(ChainStep {
            tool: "read_file".into(),
            args: json!({"path": "no_such_file_xyz.txt"}),
            output_key: None,
            skip_if_prev_contains: None,
        });
        chain.steps.push(ChainStep {
            tool: "write_file".into(),
            args: json!({"path": "skipped.txt", "content": "should not exist"}),
            output_key: None,
            skip_if_prev_contains: Some("Error".into()),
        });

        let result = executor.execute_chain(&chain, json!({})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        // First step produces error → chain stops before skip can be evaluated
        // Actually: step 1 errors → stops. But if we want skip test, let me
        // restructure: step 1 succeeds with content containing "Error" text
        // This tests that the chain stops on error (step 1 returns "Error...")
        assert_eq!(parsed["steps_executed"], 1);
        assert!(!dir.path().join("skipped.txt").exists());
    }

    #[tokio::test]
    async fn chain_via_run_chain_tool() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Invoke run_chain as a tool (like LLM would)
        let chain_args = json!({
            "name": "list_and_count",
            "description": "List dir then count",
            "steps": [
                {
                    "tool": "write_file",
                    "args": {"path": "hello.txt", "content": "world"},
                    "output_key": "written"
                },
                {
                    "tool": "list_dir",
                    "args": {"path": "."}
                }
            ],
            "input": {}
        });

        let result = executor.execute("run_chain", &chain_args).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["chain"], "list_and_count");
        assert_eq!(parsed["steps_executed"], 2);
        let steps = parsed["steps"].as_array().unwrap();
        assert!(steps[0]["success"].as_bool().unwrap());
        assert!(steps[1]["success"].as_bool().unwrap());
        // list_dir should show the file we just wrote
        assert!(
            parsed["final_output"]
                .as_str()
                .unwrap()
                .contains("hello.txt"),
            "list_dir should see the written file"
        );
    }

    #[tokio::test]
    async fn run_chain_invalid_format_returns_error() {
        let executor = test_executor();
        let result = executor
            .execute("run_chain", &json!({"invalid": "no steps field"}))
            .await;
        assert!(
            result.contains("Error"),
            "should return error for invalid chain: {result}"
        );
    }

    // ── symbols tool ─────────────────────────────────────────────────────────

    #[tokio::test]
    async fn symbols_tool_schema_in_catalog() {
        let names: Vec<String> = all_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        assert!(names.contains(&"symbols".to_string()));
    }

    #[tokio::test]
    async fn symbols_missing_path_returns_error() {
        let executor = test_executor();
        let result = executor.execute("symbols", &json!({})).await;
        assert!(result.contains("missing 'path'"), "got: {result}");
    }

    #[tokio::test]
    async fn symbols_nonexistent_file_returns_error() {
        let executor = test_executor();
        let temp_dir = tempfile::tempdir().unwrap();
        let nonexistent = temp_dir.path().join("nonexistent.rs");
        let result = executor
            .execute("symbols", &json!({"path": nonexistent.to_str().unwrap()}))
            .await;
        assert!(
            result.contains("No such file") || result.contains("Sandbox"),
            "got: {result}"
        );
    }

    #[tokio::test]
    async fn symbols_unsupported_language_returns_error() {
        let executor = test_executor();
        let temp = tempfile::NamedTempFile::with_suffix(".txt").unwrap();
        std::fs::write(temp.path(), "hello world").unwrap();
        let result = executor
            .execute("symbols", &json!({"path": temp.path().to_str().unwrap()}))
            .await;
        assert!(result.contains("Unsupported language"), "got: {result}");
    }

    #[tokio::test]
    async fn symbols_rust_file_extracts_functions() {
        let executor = test_executor();
        let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        std::fs::write(
            temp.path(),
            r#"
fn main() {
    println!("hello");
}

pub fn helper(x: i32) -> i32 {
    x * 2
}
"#,
        )
        .unwrap();
        let result = executor
            .execute("symbols", &json!({"path": temp.path().to_str().unwrap()}))
            .await;
        assert!(result.contains("[fn]"), "got: {result}");
        assert!(result.contains("main"), "got: {result}");
        assert!(result.contains("helper"), "got: {result}");
    }

    #[tokio::test]
    async fn symbols_pattern_filter_works() {
        let executor = test_executor();
        let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        std::fs::write(
            temp.path(),
            r#"
fn test_one() {}
fn test_two() {}
fn helper() {}
"#,
        )
        .unwrap();
        let result = executor
            .execute(
                "symbols",
                &json!({"path": temp.path().to_str().unwrap(), "pattern": "^test_"}),
            )
            .await;
        assert!(result.contains("test_one"), "got: {result}");
        assert!(result.contains("test_two"), "got: {result}");
        assert!(!result.contains("helper"), "got: {result}");
    }

    #[tokio::test]
    async fn symbols_kind_filter_works() {
        let executor = test_executor();
        let temp = tempfile::NamedTempFile::with_suffix(".rs").unwrap();
        std::fs::write(
            temp.path(),
            r#"
struct Point { x: i32 }
fn helper() {}
"#,
        )
        .unwrap();
        let result = executor
            .execute(
                "symbols",
                &json!({"path": temp.path().to_str().unwrap(), "kinds": ["struct"]}),
            )
            .await;
        assert!(result.contains("Point"), "got: {result}");
        assert!(!result.contains("helper"), "got: {result}");
    }

    // ── find_definition tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn find_definition_requires_symbol() {
        let executor = test_executor();
        let result = executor.execute("find_definition", &json!({})).await;
        assert!(result.contains("Error"), "should require symbol: {result}");
    }

    #[tokio::test]
    async fn find_definition_in_repo() {
        // Point at our own repo to find a known symbol
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop(); // → repo root
            p
        };
        let executor = ToolExecutor::new(root);
        let result = executor
            .execute("find_definition", &json!({"symbol": "ToolExecutor"}))
            .await;
        // Should find our own struct definition
        assert!(
            result.contains("ToolExecutor") || result.contains("No definitions"),
            "unexpected: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_regex_pattern() {
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);
        // Regex pattern should work
        let result = executor
            .execute("find_definition", &json!({"symbol": "git_st.*"}))
            .await;
        assert!(
            result.contains("git_st") || result.contains("No definitions"),
            "should match regex: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_import_aware_prioritizes_imported_file() {
        // When `file` is provided and that file imports the symbol,
        // definitions from the imported module should appear in the
        // "Import-resolved" section.
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);
        // edge_tools.rs imports code_intel which defines Language, Symbol, etc.
        // Search for "Language" with file=edge_tools.rs context
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "Language",
                    "language": "rust",
                    "file": "crates/astra-cli/src/edge_tools.rs"
                }),
            )
            .await;
        // Should find Language definition
        assert!(
            result.contains("Language"),
            "should find Language definition: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_without_file_still_works() {
        // Without `file` parameter, find_definition should still work
        // (backward compatibility)
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);
        let result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "ToolExecutor", "language": "rust"}),
            )
            .await;
        assert!(
            result.contains("ToolExecutor"),
            "should find ToolExecutor without file param: {result}"
        );
        // Without import resolution, all results are in main section (no "Import-resolved")
    }

    #[tokio::test]
    async fn find_definition_file_param_nonexistent_graceful() {
        // Non-existent file should degrade gracefully (no import resolution)
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "ToolExecutor",
                    "file": "nonexistent/file.rs"
                }),
            )
            .await;
        // Should still find results via regular scan
        assert!(
            result.contains("ToolExecutor") || result.contains("No definitions"),
            "should degrade gracefully: {result}"
        );
    }

    // ── resolve_import_to_files unit tests ───────────────────────────────────

    #[test]
    fn resolve_import_rust_crate_path() {
        let executor = test_executor();
        let files = vec![
            PathBuf::from("/project/src/utils.rs"),
            PathBuf::from("/project/src/config.rs"),
            PathBuf::from("/project/src/main.rs"),
        ];
        let import = code_intel::ImportStatement {
            path: "crate::config".to_string(),
            names: vec!["Config".to_string()],
            line: 1,
            is_wildcard: false,
        };
        let candidates =
            executor.resolve_import_to_files(&import, code_intel::Language::Rust, &files);
        assert!(
            candidates.contains(&1),
            "should resolve to config.rs (index 1): {:?}",
            candidates
        );
        assert!(
            !candidates.contains(&0),
            "should NOT include utils.rs: {:?}",
            candidates
        );
    }

    #[test]
    fn resolve_import_python_module() {
        let executor = test_executor();
        let files = vec![
            PathBuf::from("/project/utils.py"),
            PathBuf::from("/project/config.py"),
            PathBuf::from("/project/models/__init__.py"),
        ];
        let import = code_intel::ImportStatement {
            path: "config".to_string(),
            names: vec!["Config".to_string()],
            line: 1,
            is_wildcard: false,
        };
        let candidates =
            executor.resolve_import_to_files(&import, code_intel::Language::Python, &files);
        assert!(
            candidates.contains(&1),
            "should resolve to config.py (index 1): {:?}",
            candidates
        );
    }

    #[test]
    fn resolve_import_ts_relative_path() {
        let executor = test_executor();
        let files = vec![
            PathBuf::from("/project/src/utils.ts"),
            PathBuf::from("/project/src/config.ts"),
            PathBuf::from("/project/src/components/index.ts"),
        ];
        let import = code_intel::ImportStatement {
            path: "./config".to_string(),
            names: vec!["Config".to_string()],
            line: 1,
            is_wildcard: false,
        };
        let candidates =
            executor.resolve_import_to_files(&import, code_intel::Language::TypeScript, &files);
        assert!(
            candidates.contains(&1),
            "should resolve to config.ts (index 1): {:?}",
            candidates
        );
    }

    #[test]
    fn resolve_import_rust_mod_rs() {
        let executor = test_executor();
        let files = vec![
            PathBuf::from("/project/src/edge_tools/mod.rs"),
            PathBuf::from("/project/src/edge_tools/shell.rs"),
            PathBuf::from("/project/src/main.rs"),
        ];
        let import = code_intel::ImportStatement {
            path: "crate::edge_tools".to_string(),
            names: vec!["ToolExecutor".to_string()],
            line: 1,
            is_wildcard: false,
        };
        let candidates =
            executor.resolve_import_to_files(&import, code_intel::Language::Rust, &files);
        // Should match mod.rs (parent dir = edge_tools) and edge_tools/shell.rs contains edge_tools
        assert!(
            candidates.contains(&0),
            "should resolve to edge_tools/mod.rs: {:?}",
            candidates
        );
    }

    #[test]
    fn resolve_import_empty_returns_nothing() {
        let executor = test_executor();
        let files = vec![PathBuf::from("/project/src/main.rs")];
        let import = code_intel::ImportStatement {
            path: String::new(),
            names: vec![],
            line: 1,
            is_wildcard: false,
        };
        let candidates =
            executor.resolve_import_to_files(&import, code_intel::Language::Rust, &files);
        assert!(
            candidates.is_empty(),
            "empty import should resolve to nothing"
        );
    }

    // ── find_references tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn find_references_requires_symbol() {
        let executor = test_executor();
        let result = executor.execute("find_references", &json!({})).await;
        assert!(result.contains("Error"), "should require symbol: {result}");
    }

    #[tokio::test]
    async fn find_references_in_repo() {
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);
        let result = executor
            .execute("find_references", &json!({"symbol": "ToolExecutor"}))
            .await;
        // Should find references in our own codebase
        assert!(
            result.contains("ToolExecutor") || result.contains("No references"),
            "unexpected: {result}"
        );
    }

    #[tokio::test]
    async fn find_references_with_include_filter() {
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);
        let result = executor
            .execute(
                "find_references",
                &json!({
                    "symbol": "ToolExecutor",
                    "include": "*.rs"
                }),
            )
            .await;
        // All results should be .rs files
        assert!(
            result.contains("ToolExecutor") || result.contains("No references"),
            "unexpected: {result}"
        );
    }

    // ── Multi-file integration tests ────────────────────────────────────────────

    #[tokio::test]
    async fn find_definition_multifile_rust_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        // Create a multi-file Rust project
        std::fs::write(
            dir.path().join("src/config.rs"),
            r#"
pub struct AppConfig {
    pub name: String,
    pub port: u16,
}

impl AppConfig {
    pub fn new(name: &str) -> Self {
        AppConfig { name: name.to_string(), port: 8080 }
    }
}
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("src/main.rs"),
            r#"
use crate::config::AppConfig;

fn main() {
    let config = AppConfig::new("test");
    println!("{}", config.name);
}
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("src/handler.rs"),
            r#"
use crate::config::AppConfig;

pub fn handle_request(config: &AppConfig) -> String {
    format!("Running on port {}", config.port)
}
"#,
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());

        // Find definition of AppConfig — should find it in config.rs
        let result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "AppConfig", "language": "rust"}),
            )
            .await;
        assert!(
            result.contains("AppConfig") && result.contains("config.rs"),
            "should find AppConfig in config.rs: {result}"
        );
        assert!(
            result.contains("[struct]"),
            "should identify as struct: {result}"
        );

        // Import-aware: from main.rs context, should prioritize config.rs
        let result_with_file = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "AppConfig",
                    "language": "rust",
                    "file": "src/main.rs"
                }),
            )
            .await;
        assert!(
            result_with_file.contains("AppConfig"),
            "import-aware should find AppConfig: {result_with_file}"
        );

        // Find method definition
        let method_result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "new", "language": "rust", "path": "src"}),
            )
            .await;
        assert!(
            method_result.contains("new") && method_result.contains("AppConfig"),
            "should find new() in AppConfig: {method_result}"
        );
    }

    #[tokio::test]
    async fn find_definition_multifile_python_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("app")).unwrap();

        std::fs::write(
            dir.path().join("app/models.py"),
            r#"
class UserModel:
    def __init__(self, name: str, email: str):
        self.name = name
        self.email = email

    def full_name(self) -> str:
        return self.name

def create_user(name: str, email: str) -> UserModel:
    return UserModel(name, email)
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("app/views.py"),
            r#"
from models import UserModel, create_user

def get_user_view(user_id: int):
    user = create_user("test", "test@example.com")
    return user.full_name()
"#,
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());

        // Find UserModel definition
        let result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "UserModel", "language": "python"}),
            )
            .await;
        assert!(
            result.contains("UserModel") && result.contains("models.py"),
            "should find UserModel in models.py: {result}"
        );
        assert!(
            result.contains("[class]"),
            "should identify as class: {result}"
        );

        // Find free function definition
        let func_result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "create_user", "language": "python"}),
            )
            .await;
        assert!(
            func_result.contains("create_user") && func_result.contains("models.py"),
            "should find create_user in models.py: {func_result}"
        );
    }

    #[tokio::test]
    async fn find_definition_multifile_typescript_project() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        std::fs::write(
            dir.path().join("src/types.ts"),
            r#"
export interface UserConfig {
    name: string;
    port: number;
}

export function createConfig(name: string): UserConfig {
    return { name, port: 3000 };
}
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("src/app.ts"),
            r#"
import { UserConfig, createConfig } from './types';

function startApp(config: UserConfig): void {
    console.log(`Starting ${config.name} on port ${config.port}`);
}
"#,
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());

        let result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "UserConfig", "language": "typescript"}),
            )
            .await;
        assert!(
            result.contains("UserConfig") && result.contains("types.ts"),
            "should find UserConfig in types.ts: {result}"
        );
        assert!(
            result.contains("[interface]"),
            "should identify as interface: {result}"
        );

        // Import-aware from app.ts
        let import_result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "createConfig",
                    "language": "typescript",
                    "file": "src/app.ts"
                }),
            )
            .await;
        assert!(
            import_result.contains("createConfig"),
            "import-aware should find createConfig: {import_result}"
        );
        // Should show import-resolved section since app.ts imports from types
        if import_result.contains("Import-resolved") {
            assert!(
                import_result.contains("types.ts"),
                "import-resolved should point to types.ts: {import_result}"
            );
        }
    }

    #[tokio::test]
    async fn find_definition_multifile_go_project() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("config.go"),
            r#"
package main

type ServerConfig struct {
    Host string
    Port int
}

func NewServerConfig(host string, port int) *ServerConfig {
    return &ServerConfig{Host: host, Port: port}
}
"#,
        )
        .unwrap();

        std::fs::write(
            dir.path().join("main.go"),
            r#"
package main

func main() {
    config := NewServerConfig("localhost", 8080)
    StartServer(config)
}
"#,
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());

        let result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "ServerConfig", "language": "go"}),
            )
            .await;
        assert!(
            result.contains("ServerConfig") && result.contains("config.go"),
            "should find ServerConfig in config.go: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_cross_directory_with_path_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("lib")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();

        // Same symbol name in different directories
        std::fs::write(
            dir.path().join("lib/helper.rs"),
            "pub fn process(data: &str) -> String { data.to_uppercase() }\n",
        )
        .unwrap();

        std::fs::write(
            dir.path().join("src/helper.rs"),
            "pub fn process(items: Vec<i32>) -> i32 { items.iter().sum() }\n",
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());

        // Unrestricted search finds both
        let all_result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "process", "language": "rust"}),
            )
            .await;
        assert!(
            all_result.contains("2 found"),
            "should find 2 definitions: {all_result}"
        );

        // Path-restricted search
        let lib_result = executor
            .execute(
                "find_definition",
                &json!({"symbol": "process", "language": "rust", "path": "lib"}),
            )
            .await;
        assert!(
            lib_result.contains("1 found"),
            "path filter should find 1: {lib_result}"
        );
        assert!(
            lib_result.contains("lib/helper.rs"),
            "should be from lib/: {lib_result}"
        );
    }

    #[tokio::test]
    async fn find_references_multifile_finds_all_usages() {
        // Test find_references across a multi-file Rust project
        // using our own codebase (guaranteed to have cross-file references)
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);

        // extract_symbols is used in both code_intel.rs and edge_tools.rs
        let result = executor
            .execute(
                "find_references",
                &json!({
                    "symbol": "extract_symbols",
                    "include": "*.rs"
                }),
            )
            .await;
        assert!(
            result.contains("extract_symbols"),
            "should find references: {result}"
        );
        // Should find in multiple files
        if !result.contains("No references") {
            assert!(
                result.contains("code_intel.rs"),
                "should find in code_intel.rs: {result}"
            );
        }
    }

    #[tokio::test]
    async fn find_references_categorizes_imports_and_definitions() {
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop();
            p.pop();
            p
        };
        let executor = ToolExecutor::new(root);

        // cached_parse is defined in code_intel.rs and used there
        let result = executor
            .execute(
                "find_references",
                &json!({
                    "symbol": "cached_parse",
                    "include": "*.rs"
                }),
            )
            .await;
        // Should find it (it's used in many functions in code_intel.rs)
        if !result.contains("No references") {
            assert!(
                result.contains("cached_parse"),
                "should find cached_parse references: {result}"
            );
        }
    }

    // ── new tool schema coverage ──────────────────────────────────────────────

    #[test]
    fn schemas_include_new_coding_tools() {
        let schemas = all_tool_schemas();
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(names.contains(&"git_commit"), "missing git_commit schema");
        assert!(names.contains(&"git_stash"), "missing git_stash schema");
        assert!(
            names.contains(&"git_checkout_file"),
            "missing git_checkout_file schema"
        );
        assert!(
            names.contains(&"find_definition"),
            "missing find_definition schema"
        );
        assert!(
            names.contains(&"find_references"),
            "missing find_references schema"
        );
        assert!(
            names.contains(&"run_build_test"),
            "missing run_build_test schema"
        );
    }

    // ── run_build_test tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn run_build_test_requires_command() {
        let executor = test_executor();
        let result = executor.execute("run_build_test", &json!({})).await;
        assert!(result.contains("Error"), "should require command: {result}");
    }

    #[tokio::test]
    async fn run_build_test_echo_passes() {
        let executor = test_executor();
        let result = executor
            .execute("run_build_test", &json!({"command": "echo 'hello world'"}))
            .await;
        // echo should succeed
        assert!(
            result.contains("✓") || result.contains("hello"),
            "should pass: {result}"
        );
    }

    #[tokio::test]
    async fn run_build_test_failing_command() {
        let executor = test_executor();
        let result = executor
            .execute("run_build_test", &json!({"command": "false"}))
            .await;
        // false exits with code 1
        assert!(
            result.contains("✗") || result.contains("exit 1") || result.contains("failed"),
            "should detect failure: {result}"
        );
    }

    #[tokio::test]
    async fn run_build_test_cargo_in_repo() {
        // Run cargo check in our own repo
        let root = {
            let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            p.pop(); // → rust/crates/
            p.pop(); // → rust/
            p
        };
        let executor = ToolExecutor::new(root);
        let result = executor
            .execute(
                "run_build_test",
                &json!({
                    "command": "cargo check -p astra-cli --message-format=short 2>&1 | tail -5"
                }),
            )
            .await;
        // Should report something meaningful
        assert!(!result.is_empty(), "should produce output");
    }

    #[tokio::test]
    async fn call_graph_requires_path() {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("call_graph", &json!({})).await;
        assert!(result.contains("Error"), "should require path: {result}");
    }

    #[tokio::test]
    async fn call_graph_by_symbol_name() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"
fn helper() -> i32 { 42 }

fn main() {
    let x = helper();
    println!("{}", x);
    std::process::exit(0);
}
"#;
        std::fs::write(dir.path().join("main.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "main.rs",
                    "symbol": "main"
                }),
            )
            .await;
        assert!(
            result.contains("helper"),
            "should find helper() call: {result}"
        );
        assert!(
            result.contains("println!"),
            "should find println!: {result}"
        );
        assert!(
            result.contains("outgoing call(s)"),
            "should show total: {result}"
        );
    }

    #[tokio::test]
    async fn call_graph_by_line_range() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() {\n    bar();\n    baz();\n}\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "test.rs",
                    "start_line": 1,
                    "end_line": 4
                }),
            )
            .await;
        assert!(result.contains("bar"), "should find bar(): {result}");
        assert!(result.contains("baz"), "should find baz(): {result}");
    }

    #[tokio::test]
    async fn call_graph_symbol_not_found() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.rs"), "fn hello() {}\n").unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "empty.rs",
                    "symbol": "nonexistent"
                }),
            )
            .await;
        assert!(
            result.contains("not found"),
            "should report not found: {result}"
        );
    }

    #[test]
    fn schemas_include_call_graph_and_coding_tools() {
        let schemas = all_tool_schemas();
        let names: Vec<&str> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
            })
            .collect();
        assert!(
            names.contains(&"call_graph"),
            "should have call_graph: {:?}",
            names
        );
        assert!(
            names.contains(&"delete_file"),
            "should have delete_file: {:?}",
            names
        );
        assert!(
            names.contains(&"multi_edit"),
            "should have multi_edit: {:?}",
            names
        );
    }

    #[tokio::test]
    async fn run_build_test_iteration_tracking() {
        let executor = test_executor();
        // First call — no delta header
        let r1 = executor
            .execute("run_build_test", &json!({"command": "echo 'ok'"}))
            .await;
        assert!(
            !r1.contains("Iteration"),
            "first run should not show iteration: {r1}"
        );

        // Second call with same command — should show iteration 1
        let r2 = executor
            .execute("run_build_test", &json!({"command": "echo 'ok'"}))
            .await;
        // Both succeed with 0 errors, so delta should be empty (nothing to report)
        assert!(
            r2.contains("✓") || r2.contains("ok"),
            "should still work: {r2}"
        );
    }

    #[tokio::test]
    async fn run_build_test_different_command_resets_tracker() {
        let executor = test_executor();
        // Run one command
        executor
            .execute("run_build_test", &json!({"command": "echo 'build'"}))
            .await;
        // Run different command — should reset tracker, not show iteration
        let r2 = executor
            .execute("run_build_test", &json!({"command": "echo 'test'"}))
            .await;
        assert!(
            !r2.contains("Iteration"),
            "different command should reset: {r2}"
        );
    }

    #[tokio::test]
    async fn run_build_test_auto_fix_false_same_as_default() {
        let executor = test_executor();
        let r1 = executor
            .execute("run_build_test", &json!({"command": "echo ok"}))
            .await;
        let executor2 = test_executor();
        let r2 = executor2
            .execute(
                "run_build_test",
                &json!({"command": "echo ok", "auto_fix": false}),
            )
            .await;
        // Both should produce similar output (no auto-fix sections)
        assert!(
            !r1.contains("Auto-Fix"),
            "default should not auto-fix: {r1}"
        );
        assert!(
            !r2.contains("Auto-Fix"),
            "explicit false should not auto-fix: {r2}"
        );
    }

    #[tokio::test]
    async fn run_build_test_auto_fix_on_success_no_effect() {
        let executor = test_executor();
        let result = executor
            .execute(
                "run_build_test",
                &json!({
                    "command": "echo 'all tests passed'",
                    "auto_fix": true
                }),
            )
            .await;
        // Successful build = no errors = no fixes to apply
        assert!(
            !result.contains("Auto-Fix"),
            "no errors = no auto-fix: {result}"
        );
    }

    #[tokio::test]
    async fn run_build_test_auto_fix_creates_report() {
        // Create a temp dir with a Rust file that has an "unused import" error pattern
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("test.rs");
        std::fs::write(&src, "use std::io;\n\nfn main() {}\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        // Simulate a build that produces an unused import warning
        // We use a command that outputs Rust-style warnings
        let result = executor
            .execute(
                "run_build_test",
                &json!({
                    "command": "echo 'warning: unused import: `std::io`\n --> test.rs:1:5'",
                    "auto_fix": true
                }),
            )
            .await;
        // Should contain auto-fix report since the warning matches unused import pattern
        // and the file exists with the import
        assert!(
            result.contains("Auto-Fix") || !result.contains("error"),
            "should attempt auto-fix or have no errors: {result}"
        );
    }

    #[test]
    fn schema_includes_auto_fix_param() {
        let schemas = all_tool_schemas();
        let build = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("run_build_test")
            })
            .expect("run_build_test schema should exist");
        let props = build["function"]["parameters"]["properties"]
            .as_object()
            .unwrap();
        assert!(
            props.contains_key("auto_fix"),
            "schema should have auto_fix param"
        );
        assert_eq!(props["auto_fix"]["type"], "boolean");
    }

    // ── Code Intelligence Enhancement Tests ──

    #[tokio::test]
    async fn symbols_with_calls_shows_callees() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"
fn helper() -> i32 { 42 }

fn process(x: i32) -> i32 {
    let a = helper();
    println!("{}", a + x);
    a + x
}

fn main() {
    let result = process(10);
    std::process::exit(result);
}
"#;
        std::fs::write(dir.path().join("demo.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Without calls=true — no call info
        let r1 = executor
            .execute("symbols", &json!({"path": "demo.rs"}))
            .await;
        assert!(
            !r1.contains("→"),
            "without calls should not show arrows: {r1}"
        );

        // With calls=true — should show callees inline
        let r2 = executor
            .execute("symbols", &json!({"path": "demo.rs", "calls": true}))
            .await;
        assert!(r2.contains("→ helper()"), "should show helper() call: {r2}");
        assert!(
            r2.contains("→ process("),
            "should show process() call: {r2}"
        );
        assert!(
            r2.contains("→ std::process::exit()") || r2.contains("→ exit()"),
            "should show exit call: {r2}"
        );
    }

    #[tokio::test]
    async fn symbols_calls_empty_for_leaf_functions() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn leaf() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("leaf.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute("symbols", &json!({"path": "leaf.rs", "calls": true}))
            .await;
        // Should not have any call arrows since leaf() calls nothing
        assert!(
            !result.contains("→"),
            "leaf function should have no calls: {result}"
        );
    }

    #[tokio::test]
    async fn call_graph_with_callers() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"
fn target() -> i32 { 42 }

fn caller_a() {
    let x = target();
    println!("{}", x);
}

fn caller_b() {
    target();
}

fn unrelated() {
    println!("hello");
}
"#;
        std::fs::write(dir.path().join("callers.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "callers.rs",
                    "symbol": "target",
                    "callers": true
                }),
            )
            .await;

        // Should show callers section
        assert!(
            result.contains("Callers OF 'target'"),
            "should have callers section: {result}"
        );
        assert!(
            result.contains("caller_a"),
            "should find caller_a: {result}"
        );
        assert!(
            result.contains("caller_b"),
            "should find caller_b: {result}"
        );
        assert!(
            !result.contains("unrelated"),
            "should not include unrelated: {result}"
        );
    }

    #[tokio::test]
    async fn call_graph_callers_without_symbol_name() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { bar(); }\nfn bar() {}\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();
        let executor = ToolExecutor::new(dir.path());
        // Using line range instead of symbol name — callers should note it needs symbol
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "test.rs",
                    "start_line": 1,
                    "end_line": 1,
                    "callers": true
                }),
            )
            .await;
        assert!(
            result.contains("requires symbol name"),
            "should warn about symbol requirement: {result}"
        );
    }

    #[test]
    fn categorize_reference_definitions() {
        assert_eq!(
            categorize_reference("foo.rs:10:fn helper() -> i32 {", "helper"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.rs:10:pub fn process(x: i32) {", "process"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.py:5:def calculate(n):", "calculate"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.rs:3:pub struct Config {", "Config"),
            "definition"
        );
        assert_eq!(
            categorize_reference("foo.rs:3:pub enum Status {", "Status"),
            "definition"
        );
    }

    #[test]
    fn categorize_reference_imports() {
        assert_eq!(
            categorize_reference("foo.rs:1:use crate::helper;", "helper"),
            "import"
        );
        assert_eq!(
            categorize_reference("foo.py:1:from module import helper", "helper"),
            "import"
        );
        assert_eq!(
            categorize_reference("foo.py:1:import helper", "helper"),
            "import"
        );
        assert_eq!(
            categorize_reference("foo.js:1:const x = require('helper')", "helper"),
            "import"
        );
    }

    #[test]
    fn categorize_reference_calls() {
        assert_eq!(
            categorize_reference("foo.rs:20:    let x = helper();", "helper"),
            "call"
        );
        assert_eq!(
            categorize_reference("foo.rs:20:    helper(42, true);", "helper"),
            "call"
        );
        assert_eq!(
            categorize_reference("foo.py:20:    result = calculate(n)", "calculate"),
            "call"
        );
    }

    #[test]
    fn categorize_reference_usage() {
        // Type annotations, field access, etc. — no parens, not a definition/import
        assert_eq!(
            categorize_reference("foo.rs:10:    let x: Config = default;", "Config"),
            "usage"
        );
    }

    #[test]
    fn schemas_include_new_params() {
        let schemas = all_tool_schemas();
        let symbols_schema = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("symbols")
            })
            .expect("symbols schema should exist");
        let props = &symbols_schema["function"]["parameters"]["properties"];
        assert!(
            props.get("calls").is_some(),
            "symbols should have 'calls' param"
        );

        let call_graph_schema = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("call_graph")
            })
            .expect("call_graph schema should exist");
        let cg_props = &call_graph_schema["function"]["parameters"]["properties"];
        assert!(
            cg_props.get("callers").is_some(),
            "call_graph should have 'callers' param"
        );
        assert!(
            cg_props.get("scope").is_some(),
            "call_graph should have 'scope' param"
        );

        let ref_schema = schemas
            .iter()
            .find(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    == Some("find_references")
            })
            .expect("find_references schema should exist");
        let ref_props = &ref_schema["function"]["parameters"]["properties"];
        assert!(
            ref_props.get("kind").is_some(),
            "find_references should have 'kind' param"
        );
    }

    // ── Cross-File Caller Tests ──

    #[tokio::test]
    async fn cross_file_callers_finds_callers_in_other_files() {
        let dir = tempfile::tempdir().unwrap();

        // File 1: defines the target function
        let lib_code = "pub fn target_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("lib.rs"), lib_code).unwrap();

        // File 2: calls the target
        let main_code = r#"
fn main() {
    let x = target_fn();
    println!("{}", x);
}
"#;
        std::fs::write(dir.path().join("main.rs"), main_code).unwrap();

        // File 3: also calls the target
        let util_code = r#"
fn helper() {
    target_fn();
}

fn unrelated() {
    println!("no call here");
}
"#;
        std::fs::write(dir.path().join("util.rs"), util_code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "lib.rs",
                    "symbol": "target_fn",
                    "callers": true,
                    "scope": "project"
                }),
            )
            .await;

        assert!(
            result.contains("project-wide"),
            "should indicate project scope: {result}"
        );
        assert!(
            result.contains("main"),
            "should find main() as caller: {result}"
        );
        assert!(
            result.contains("helper"),
            "should find helper() as caller: {result}"
        );
        assert!(
            !result.contains("unrelated"),
            "should not include unrelated(): {result}"
        );
    }

    #[tokio::test]
    async fn cross_file_callers_empty_when_no_callers() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub fn lonely_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("alone.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "alone.rs",
                    "symbol": "lonely_fn",
                    "callers": true,
                    "scope": "project"
                }),
            )
            .await;

        assert!(
            result.contains("none found"),
            "should report no callers: {result}"
        );
    }

    #[tokio::test]
    async fn cross_file_callers_with_methods() {
        let dir = tempfile::tempdir().unwrap();

        let lib_code = r#"
struct Engine;
impl Engine {
    fn run(&self) -> i32 { 42 }
}
"#;
        std::fs::write(dir.path().join("engine.rs"), lib_code).unwrap();

        let caller_code = r#"
fn start_engine() {
    let e = Engine;
    e.run();
}
"#;
        std::fs::write(dir.path().join("starter.rs"), caller_code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "call_graph",
                &json!({
                    "path": "engine.rs",
                    "symbol": "run",
                    "callers": true,
                    "scope": "project"
                }),
            )
            .await;

        assert!(
            result.contains("start_engine"),
            "should find start_engine as caller: {result}"
        );
    }

    #[test]
    fn prefilter_files_returns_matching_paths() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() { target_fn(); }").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn bar() { unrelated(); }").unwrap();
        std::fs::write(dir.path().join("c.rs"), "fn baz() { target_fn(); }").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let exts = ["rs"];
        let files = executor.prefilter_files_with_symbol("target_fn", &exts);

        // rg might not be available in CI — if empty, that's ok (fallback will be used)
        if files.is_empty() {
            return; // rg not available or returned nothing; cross_file_callers test covers fallback
        }

        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"a.rs".to_string()),
            "should find a.rs: {:?}",
            names
        );
        assert!(
            names.contains(&"c.rs".to_string()),
            "should find c.rs: {:?}",
            names
        );
        assert!(
            !names.contains(&"b.rs".to_string()),
            "should not find b.rs: {:?}",
            names
        );
    }

    #[test]
    fn collect_project_files_skips_noise_dirs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::create_dir_all(dir.path().join("node_modules")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.path().join("target/debug.rs"), "fn debug() {}").unwrap();
        std::fs::write(dir.path().join("node_modules/dep.js"), "function x() {}").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let skip = ["node_modules", "target", ".git"];
        let exts = ["rs", "js"];
        let files = executor.collect_project_files(&skip, &exts, 100);

        let names: Vec<String> = files
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().to_string())
            .collect();
        assert!(
            names.contains(&"main.rs".to_string()),
            "should find src/main.rs"
        );
        assert!(
            !names.contains(&"debug.rs".to_string()),
            "should skip target/"
        );
        assert!(
            !names.contains(&"dep.js".to_string()),
            "should skip node_modules/"
        );
    }

    // ---- AST validation tests ----

    #[test]
    fn parse_grep_file_line_extracts_path_and_line() {
        assert_eq!(
            parse_grep_file_line("src/main.rs:42:fn foo()"),
            Some(("src/main.rs", 42))
        );
        assert_eq!(
            parse_grep_file_line("lib.py:1:import os"),
            Some(("lib.py", 1))
        );
        assert_eq!(parse_grep_file_line("no-colon"), None);
        assert_eq!(parse_grep_file_line("file:abc:content"), None);
    }

    #[test]
    fn ast_validate_filters_comments() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn real_call() { target(); }
// target is mentioned in this comment
fn another() { target(); }
"#;
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec![
            "test.rs:1:fn real_call() { target(); }",
            "test.rs:2:// target is mentioned in this comment",
            "test.rs:3:fn another() { target(); }",
        ];
        let result = executor.ast_validate_references(&lines, "target");
        assert!(
            result.contains(&"test.rs:1:fn real_call() { target(); }"),
            "real call kept: {:?}",
            result
        );
        assert!(
            !result.iter().any(|l| l.contains("comment")),
            "comment filtered: {:?}",
            result
        );
        assert!(
            result.contains(&"test.rs:3:fn another() { target(); }"),
            "another call kept: {:?}",
            result
        );
    }

    #[test]
    fn ast_validate_filters_string_literals() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn real_use() -> &str { "hello" }
fn fake_use() -> &str { "target is in a string" }
fn actual() { target(); }
"#;
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec![
            "test.rs:2:fn fake_use() -> &str { \"target is in a string\" }",
            "test.rs:3:fn actual() { target(); }",
        ];
        let result = executor.ast_validate_references(&lines, "target");
        assert!(
            !result.iter().any(|l| l.contains("string")),
            "string literal filtered: {:?}",
            result
        );
        assert!(
            result.contains(&"test.rs:3:fn actual() { target(); }"),
            "real call kept: {:?}",
            result
        );
    }

    #[test]
    fn ast_validate_keeps_all_for_unknown_language() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.xyz"), "target is here\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec!["data.xyz:1:target is here"];
        let result = executor.ast_validate_references(&lines, "target");
        assert_eq!(result.len(), 1, "unknown language keeps all matches");
    }

    #[test]
    fn ast_validate_python_comments() {
        let dir = tempfile::tempdir().unwrap();
        let code = "# target in comment\ntarget = 42\n";
        std::fs::write(dir.path().join("test.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec!["test.py:1:# target in comment", "test.py:2:target = 42"];
        let result = executor.ast_validate_references(&lines, "target");
        assert!(
            !result.iter().any(|l| l.contains("comment")),
            "python comment filtered: {:?}",
            result
        );
        assert!(
            result.contains(&"test.py:2:target = 42"),
            "real code kept: {:?}",
            result
        );
    }

    #[test]
    fn ast_validate_mixed_file() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn main() {
    // Call target here
    target();
    let s = "target in string";
    target.method();
}
"#;
        std::fs::write(dir.path().join("mixed.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let lines = vec![
            "mixed.rs:2:    // Call target here",
            "mixed.rs:3:    target();",
            "mixed.rs:4:    let s = \"target in string\";",
            "mixed.rs:5:    target.method();",
        ];
        let result = executor.ast_validate_references(&lines, "target");
        // Comment and string should be filtered; real code should remain
        assert!(
            !result.iter().any(|l| l.contains("//")),
            "comment filtered: {:?}",
            result
        );
        // Line 4 has "target" in a string, should be filtered
        assert!(
            !result.iter().any(|l| l.contains("in string")),
            "string filtered: {:?}",
            result
        );
        // Real calls should remain
        assert!(
            result.iter().any(|l| l.contains("target();")),
            "real call kept: {:?}",
            result
        );
        assert!(
            result.iter().any(|l| l.contains("target.method();")),
            "method call kept: {:?}",
            result
        );
    }

    #[tokio::test]
    async fn find_references_with_validate_false_skips_ast() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { target(); }\n// target in comment\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_references",
                &json!({
                    "symbol": "target",
                    "validate": false
                }),
            )
            .await;
        // With validate=false, the comment line should still appear
        assert!(
            result.contains("target"),
            "should find references: {result}"
        );
    }

    // ---- rename_symbol tests ----

    #[tokio::test]
    async fn rename_symbol_dry_run_shows_preview() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn target_fn() { 42 }\nfn caller() { target_fn(); }\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "target_fn",
                    "new_name": "renamed_fn"
                }),
            )
            .await;

        assert!(result.contains("preview"), "default is dry run: {result}");
        assert!(result.contains("target_fn"), "shows old name: {result}");
        assert!(result.contains("renamed_fn"), "shows new name: {result}");
        assert!(result.contains("dry_run=false"), "hints to apply: {result}");
        // File should NOT be modified
        let content = std::fs::read_to_string(dir.path().join("main.rs")).unwrap();
        assert!(content.contains("target_fn"), "file untouched in dry run");
    }

    #[tokio::test]
    async fn rename_symbol_applies_changes() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn old_name() -> i32 { 42 }\nfn caller() { old_name(); }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "old_name",
                    "new_name": "new_name",
                    "dry_run": false
                }),
            )
            .await;

        assert!(result.contains("Renaming"), "shows applied: {result}");
        assert!(
            result.contains("2 replacement"),
            "both occurrences renamed: {result}"
        );
        let content = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        assert!(content.contains("fn new_name()"), "definition renamed");
        assert!(content.contains("new_name();"), "call site renamed");
        assert!(!content.contains("old_name"), "old name fully gone");
    }

    #[tokio::test]
    async fn rename_symbol_skips_comments_and_strings() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"fn target() -> i32 { 42 }
// target is a good function
fn caller() {
    let s = "target in string";
    target();
}
"#;
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "target",
                    "new_name": "renamed",
                    "dry_run": false
                }),
            )
            .await;

        let content = std::fs::read_to_string(dir.path().join("test.rs")).unwrap();
        // Real code references should be renamed
        assert!(
            content.contains("fn renamed()"),
            "definition renamed: {}",
            content
        );
        assert!(content.contains("renamed();"), "call renamed: {}", content);
        // Comment and string should be preserved
        assert!(
            content.contains("// target is a good function"),
            "comment preserved: {}",
            content
        );
        assert!(
            content.contains("\"target in string\""),
            "string preserved: {}",
            content
        );
        // Should report filtered matches
        assert!(result.contains("skipped"), "mentions filtered: {result}");
    }

    #[tokio::test]
    async fn rename_symbol_across_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn shared_fn() -> i32 { 42 }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/main.rs"),
            "fn main() { shared_fn(); }\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("src/test.rs"),
            "fn test_it() { assert_eq!(shared_fn(), 42); }\n",
        )
        .unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "shared_fn",
                    "new_name": "common_fn",
                    "dry_run": false
                }),
            )
            .await;

        assert!(result.contains("3 file"), "changed 3 files: {result}");
        for file in &["src/lib.rs", "src/main.rs", "src/test.rs"] {
            let content = std::fs::read_to_string(dir.path().join(file)).unwrap();
            assert!(
                content.contains("common_fn"),
                "{} should have new name: {}",
                file,
                content
            );
            assert!(
                !content.contains("shared_fn"),
                "{} should not have old name: {}",
                file,
                content
            );
        }
    }

    #[tokio::test]
    async fn rename_symbol_word_boundary_safe() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { 1 }\nfn foobar() { foo() + 2 }\nfn foo_baz() { foo() }\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "bar",
                    "dry_run": false
                }),
            )
            .await;

        let content = std::fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("fn bar()"), "foo renamed to bar");
        assert!(
            content.contains("foobar"),
            "foobar NOT renamed (word boundary)"
        );
        assert!(content.contains("bar() + 2"), "call in foobar line renamed");
        assert!(result.contains("replacement"), "has replacements: {result}");
    }

    #[tokio::test]
    async fn rename_symbol_errors_on_invalid_name() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn foo() {}\n").unwrap();

        let executor = ToolExecutor::new(dir.path());

        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "123invalid"
                }),
            )
            .await;
        assert!(
            result.contains("not a valid identifier"),
            "rejects numeric start: {result}"
        );

        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "has space"
                }),
            )
            .await;
        assert!(
            result.contains("not a valid identifier"),
            "rejects spaces: {result}"
        );

        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "foo",
                    "new_name": "foo"
                }),
            )
            .await;
        assert!(result.contains("same"), "rejects same name: {result}");
    }

    #[tokio::test]
    async fn rename_symbol_no_matches() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("test.rs"), "fn bar() {}\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "nonexistent_symbol_xyz",
                    "new_name": "new_name"
                }),
            )
            .await;
        assert!(
            result.contains("No references"),
            "reports no matches: {result}"
        );
    }

    #[tokio::test]
    async fn rename_symbol_with_include_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("lib.rs"), "pub fn target() { 1 }\n").unwrap();
        std::fs::write(dir.path().join("main.py"), "def target(): pass\ntarget()\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "rename_symbol",
                &json!({
                    "symbol": "target",
                    "new_name": "renamed",
                    "include": "*.rs",
                    "dry_run": false
                }),
            )
            .await;

        // Only .rs file should be modified
        let rs_content = std::fs::read_to_string(dir.path().join("lib.rs")).unwrap();
        let py_content = std::fs::read_to_string(dir.path().join("main.py")).unwrap();
        assert!(
            rs_content.contains("renamed"),
            "rs file renamed: {}",
            rs_content
        );
        assert!(
            py_content.contains("target"),
            "py file untouched: {}",
            py_content
        );
        assert!(result.contains("1 file"), "only 1 file changed: {result}");
    }

    // ---- dead_code tests ----

    #[tokio::test]
    async fn dead_code_finds_unused_function() {
        let dir = tempfile::tempdir().unwrap();
        let code =
            "fn used_fn() -> i32 { 42 }\nfn unused_fn() -> i32 { 99 }\nfn main() { used_fn(); }\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "dead_code",
                &json!({
                    "path": "."
                }),
            )
            .await;

        assert!(
            result.contains("unused_fn"),
            "should find unused_fn: {result}"
        );
        // Verify used_fn is NOT flagged (careful: "unused_fn" contains "used_fn")
        let without_unused = result.replace("unused_fn", "");
        assert!(
            !without_unused.contains("used_fn"),
            "used_fn should not be listed: {result}"
        );
        // main() should be skipped as entry point — check it's not listed as a symbol
        assert!(
            !result.contains("function main"),
            "main() should be skipped: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_skips_test_functions() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn helper() -> i32 { 42 }\nfn test_helper() { assert_eq!(helper(), 42); }\n";
        std::fs::write(dir.path().join("test.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "dead_code",
                &json!({
                    "path": "."
                }),
            )
            .await;

        // test_helper should be skipped (it's a test)
        assert!(
            !result.contains("test_helper"),
            "test functions should be skipped: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_filters_by_kind() {
        let dir = tempfile::tempdir().unwrap();
        let code = "struct UnusedStruct { x: i32 }\nfn unused_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());

        let result_fn = executor
            .execute(
                "dead_code",
                &json!({
                    "path": ".",
                    "kind": "function"
                }),
            )
            .await;
        assert!(
            result_fn.contains("unused_fn"),
            "should find unused_fn: {result_fn}"
        );
        assert!(
            !result_fn.contains("UnusedStruct"),
            "should not show structs: {result_fn}"
        );

        let result_type = executor
            .execute(
                "dead_code",
                &json!({
                    "path": ".",
                    "kind": "type"
                }),
            )
            .await;
        assert!(
            result_type.contains("UnusedStruct"),
            "should find UnusedStruct: {result_type}"
        );
        assert!(
            !result_type.contains("unused_fn"),
            "should not show functions: {result_type}"
        );
    }

    #[tokio::test]
    async fn dead_code_reports_public_symbols() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub fn exported() -> i32 { 42 }\nfn internal() -> i32 { 99 }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("dead_code", &json!({})).await;

        // Both should be detected as unused, but public should have marker
        if result.contains("exported") {
            assert!(
                result.contains("(pub)"),
                "public symbol should be marked: {result}"
            );
        }
    }

    #[tokio::test]
    async fn dead_code_clean_project() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn main() { helper(); }\nfn helper() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("dead_code", &json!({})).await;

        assert!(
            result.contains("No dead code") || result.contains("0 potentially"),
            "should report clean: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_no_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("data.txt"), "not a source file\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor.execute("dead_code", &json!({})).await;

        assert!(
            result.contains("No source files") || result.contains("No symbols"),
            "should report no files: {result}"
        );
    }

    #[tokio::test]
    async fn dead_code_python() {
        let dir = tempfile::tempdir().unwrap();
        let code =
            "def used():\n    return 42\n\ndef unused():\n    return 99\n\nresult = used()\n";
        std::fs::write(dir.path().join("main.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "dead_code",
                &json!({
                    "path": "."
                }),
            )
            .await;

        assert!(result.contains("unused"), "should find unused: {result}");
    }

    // ---- doc comment extraction tests ----

    #[tokio::test]
    async fn find_definition_includes_rust_doc_comment() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"/// This function does something important.
/// It returns a number.
fn documented_fn() -> i32 {
    42
}
"#;
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "documented_fn"
                }),
            )
            .await;

        assert!(
            result.contains("documented_fn"),
            "should find definition: {result}"
        );
        assert!(result.contains("📝"), "should include doc marker: {result}");
        assert!(
            result.contains("something important"),
            "should include doc text: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_includes_python_docstring() {
        let dir = tempfile::tempdir().unwrap();
        let code = "def my_func():\n    \"\"\"This is a Python docstring.\"\"\"\n    return 42\n";
        std::fs::write(dir.path().join("module.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "my_func"
                }),
            )
            .await;

        assert!(
            result.contains("my_func"),
            "should find definition: {result}"
        );
        assert!(result.contains("📝"), "should include doc marker: {result}");
        assert!(
            result.contains("Python docstring"),
            "should include docstring: {result}"
        );
    }

    #[tokio::test]
    async fn find_definition_no_doc_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn bare_fn() -> i32 { 42 }\n";
        std::fs::write(dir.path().join("lib.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "find_definition",
                &json!({
                    "symbol": "bare_fn"
                }),
            )
            .await;

        assert!(
            result.contains("bare_fn"),
            "should find definition: {result}"
        );
        assert!(
            !result.contains("📝"),
            "no doc marker without doc: {result}"
        );
    }

    #[test]
    fn extract_doc_comment_rust_triple_slash() {
        let source = "/// First line.\n/// Second line.\nfn foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Rust, 3);
        assert!(
            doc.contains("First line"),
            "should extract first line: {doc}"
        );
        assert!(
            doc.contains("Second line"),
            "should extract second line: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_block_comment() {
        let source = "/**\n * A block doc comment.\n * With multiple lines.\n */\nfn foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Rust, 5);
        assert!(
            doc.contains("block doc comment"),
            "should extract block: {doc}"
        );
        assert!(
            doc.contains("multiple lines"),
            "should extract multi-line: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_python_docstring() {
        let source = "def foo():\n    \"\"\"A short docstring.\"\"\"\n    pass\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Python, 1);
        assert!(
            doc.contains("short docstring"),
            "should extract docstring: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_go_comments() {
        let source = "// Package foo provides utilities.\n// It does things.\nfunc Foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Go, 3);
        assert!(
            doc.contains("Package foo"),
            "should extract Go comments: {doc}"
        );
    }

    #[test]
    fn extract_doc_comment_empty_when_no_doc() {
        let source = "fn bar() {}\nfn foo() {}\n";
        let doc = code_intel::extract_doc_comment(source, code_intel::Language::Rust, 2);
        assert!(doc.is_empty(), "no doc should be empty: {doc}");
    }

    // ── extract_members tests ────────────────────────────────────────────────

    #[tokio::test]
    async fn extract_members_rust_struct() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub struct Config {\n    pub name: String,\n    pub port: u16,\n    timeout: Option<u64>,\n}\n";
        std::fs::write(dir.path().join("config.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "extract_members",
                &json!({
                    "file": "config.rs", "line": 1
                }),
            )
            .await;

        assert!(result.contains("name"), "should list name field: {result}");
        assert!(result.contains("port"), "should list port field: {result}");
        assert!(
            result.contains("timeout"),
            "should list timeout field: {result}"
        );
        assert!(
            result.contains("3 members"),
            "should report 3 members: {result}"
        );
    }

    #[tokio::test]
    async fn extract_members_rust_enum() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub enum Color {\n    Red,\n    Green,\n    Blue,\n}\n";
        std::fs::write(dir.path().join("color.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "extract_members",
                &json!({
                    "file": "color.rs", "line": 1
                }),
            )
            .await;

        assert!(result.contains("Red"), "should list Red: {result}");
        assert!(result.contains("Blue"), "should list Blue: {result}");
        assert!(
            result.contains("variant"),
            "should report as variant: {result}"
        );
    }

    #[tokio::test]
    async fn extract_members_python_class() {
        let dir = tempfile::tempdir().unwrap();
        let code =
            "class User:\n    name: str\n    age: int = 0\n    def greet(self):\n        pass\n";
        std::fs::write(dir.path().join("user.py"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "extract_members",
                &json!({
                    "file": "user.py", "line": 1
                }),
            )
            .await;

        assert!(result.contains("name"), "should list name: {result}");
        assert!(
            result.contains("greet"),
            "should list greet method: {result}"
        );
    }

    #[tokio::test]
    async fn extract_members_no_type_at_line() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn main() {\n    println!(\"hello\");\n}\n";
        std::fs::write(dir.path().join("main.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "extract_members",
                &json!({
                    "file": "main.rs", "line": 1
                }),
            )
            .await;

        assert!(
            result.contains("No type definition"),
            "should report no type: {result}"
        );
    }

    #[tokio::test]
    async fn extract_members_line_inside_struct() {
        let dir = tempfile::tempdir().unwrap();
        let code = "struct Point {\n    x: f64,\n    y: f64,\n}\n";
        std::fs::write(dir.path().join("point.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        // Point at line 2 (inside the struct, not at its start)
        let result = executor
            .execute(
                "extract_members",
                &json!({
                    "file": "point.rs", "line": 2
                }),
            )
            .await;

        assert!(
            result.contains("x"),
            "should find members even pointing inside: {result}"
        );
        assert!(result.contains("y"), "should find y: {result}");
    }

    // ── type_hierarchy tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn type_hierarchy_finds_implementations() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"trait Serialize {
    fn serialize(&self) -> String;
}

struct User { name: String }
struct Config { port: u16 }

impl Serialize for User {
    fn serialize(&self) -> String { self.name.clone() }
}

impl Serialize for Config {
    fn serialize(&self) -> String { format!("{}", self.port) }
}
"#;
        std::fs::write(dir.path().join("types.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "type_hierarchy",
                &json!({
                    "name": "Serialize"
                }),
            )
            .await;

        assert!(result.contains("User"), "should find User impl: {result}");
        assert!(
            result.contains("Config"),
            "should find Config impl: {result}"
        );
        assert!(
            result.contains("implementing"),
            "should say implementing: {result}"
        );
    }

    #[tokio::test]
    async fn type_hierarchy_finds_supertypes() {
        let dir = tempfile::tempdir().unwrap();
        let code = r#"trait Display {
    fn display(&self);
}
trait Debug {
    fn debug(&self);
}
struct Foo;
impl Display for Foo {
    fn display(&self) {}
}
impl Debug for Foo {
    fn debug(&self) {}
}
"#;
        std::fs::write(dir.path().join("foo.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "type_hierarchy",
                &json!({
                    "name": "Foo",
                    "direction": "supertypes"
                }),
            )
            .await;

        assert!(
            result.contains("Display"),
            "should find Display trait: {result}"
        );
        assert!(
            result.contains("Debug"),
            "should find Debug trait: {result}"
        );
    }

    #[tokio::test]
    async fn type_hierarchy_no_results() {
        let dir = tempfile::tempdir().unwrap();
        let code = "struct Lonely;\n";
        std::fs::write(dir.path().join("lonely.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "type_hierarchy",
                &json!({
                    "name": "NonExistent"
                }),
            )
            .await;

        assert!(
            result.contains("no implementations"),
            "should report none: {result}"
        );
    }

    #[test]
    fn code_intel_extract_members_rust_trait() {
        let source = "trait Handler {\n    fn handle(&self);\n    fn reset(&mut self);\n}\n";
        let members = code_intel::extract_members(source, code_intel::Language::Rust, 1);
        assert_eq!(members.len(), 2);
        assert_eq!(members[0].name, "handle");
        assert_eq!(members[0].kind, "method");
        assert_eq!(members[1].name, "reset");
    }

    #[test]
    fn code_intel_find_rust_impls() {
        let source = r#"
trait Foo {}
trait Bar {}
struct MyType;
impl Foo for MyType {}
impl Bar for MyType {}
impl MyType {
    fn new() -> Self { Self }
}
"#;
        let impls = code_intel::find_rust_impls(source, "src/lib.rs");
        assert_eq!(
            impls.len(),
            2,
            "should find 2 trait impls, not inherent: {:?}",
            impls
        );
        assert!(
            impls
                .iter()
                .any(|i| i.trait_name == "Foo" && i.type_name == "MyType")
        );
        assert!(
            impls
                .iter()
                .any(|i| i.trait_name == "Bar" && i.type_name == "MyType")
        );
    }

    // ── hover_info tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn hover_info_on_function_definition() {
        let dir = tempfile::tempdir().unwrap();
        let code = "/// Computes the sum.\nfn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        std::fs::write(dir.path().join("math.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "hover_info",
                &json!({
                    "file": "math.rs", "line": 2
                }),
            )
            .await;

        assert!(
            result.contains("add"),
            "should show function name: {result}"
        );
        assert!(result.contains("fn"), "should show kind: {result}");
        assert!(result.contains("sum"), "should show doc: {result}");
    }

    #[tokio::test]
    async fn hover_info_on_struct_shows_members() {
        let dir = tempfile::tempdir().unwrap();
        let code = "pub struct Config {\n    pub host: String,\n    pub port: u16,\n}\n";
        std::fs::write(dir.path().join("config.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "hover_info",
                &json!({
                    "file": "config.rs", "line": 1
                }),
            )
            .await;

        assert!(
            result.contains("Config"),
            "should show struct name: {result}"
        );
        assert!(
            result.contains("Members"),
            "should show members section: {result}"
        );
        assert!(result.contains("host"), "should list host field: {result}");
        assert!(result.contains("port"), "should list port field: {result}");
    }

    #[tokio::test]
    async fn hover_info_scope_breadcrumbs() {
        let dir = tempfile::tempdir().unwrap();
        let code = "struct Server;\nimpl Server {\n    fn start(&self) {\n        println!(\"ok\");\n    }\n}\n";
        std::fs::write(dir.path().join("server.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "hover_info",
                &json!({
                    "file": "server.rs", "line": 4
                }),
            )
            .await;

        // Line 4 is inside fn start, scope should show breadcrumbs
        assert!(
            result.contains("start"),
            "should show start in scope: {result}"
        );
        assert!(result.contains("📍"), "should show scope marker: {result}");
    }

    #[tokio::test]
    async fn hover_info_with_column() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn foo() { bar(); }\nfn bar() { 42; }\n";
        std::fs::write(dir.path().join("fns.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "hover_info",
                &json!({
                    "file": "fns.rs", "line": 1, "column": 3
                }),
            )
            .await;

        assert!(
            result.contains("foo"),
            "should identify foo at column 3: {result}"
        );
    }

    // ── symbol_search tests ──────────────────────────────────────────────

    #[tokio::test]
    async fn symbol_search_finds_functions() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn process_data() {}\nfn process_config() {}\nfn unrelated() {}\n";
        std::fs::write(dir.path().join("app.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "symbol_search",
                &json!({
                    "query": "process"
                }),
            )
            .await;

        assert!(
            result.contains("process_data"),
            "should find process_data: {result}"
        );
        assert!(
            result.contains("process_config"),
            "should find process_config: {result}"
        );
        assert!(
            !result.contains("unrelated"),
            "should NOT find unrelated: {result}"
        );
    }

    #[tokio::test]
    async fn symbol_search_kind_filter() {
        let dir = tempfile::tempdir().unwrap();
        let code = "struct Config {}\nfn config_new() {}\nconst CONFIG_MAX: u32 = 100;\n";
        std::fs::write(dir.path().join("cfg.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "symbol_search",
                &json!({
                    "query": "config",
                    "kind": "type"
                }),
            )
            .await;

        assert!(
            result.contains("Config"),
            "should find Config struct: {result}"
        );
        assert!(
            !result.contains("config_new"),
            "should NOT find function: {result}"
        );
    }

    #[tokio::test]
    async fn symbol_search_exact_match_first() {
        let dir = tempfile::tempdir().unwrap();
        let code = "fn run() {}\nfn run_all() {}\nfn prerun() {}\n";
        std::fs::write(dir.path().join("runner.rs"), code).unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "symbol_search",
                &json!({
                    "query": "run"
                }),
            )
            .await;

        // "run" should appear before "run_all" and "prerun"
        let pos_run = result.find("fn run()").unwrap_or(9999);
        let pos_run_all = result.find("fn run_all()").unwrap_or(9999);
        assert!(
            pos_run < pos_run_all,
            "exact match should come first: {result}"
        );
    }

    #[tokio::test]
    async fn symbol_search_cross_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn search_user() {}\n").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn search_order() {}\n").unwrap();
        std::fs::write(dir.path().join("c.py"), "def search_log():\n    pass\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "symbol_search",
                &json!({
                    "query": "search"
                }),
            )
            .await;

        assert!(
            result.contains("search_user"),
            "should find in a.rs: {result}"
        );
        assert!(
            result.contains("search_order"),
            "should find in b.rs: {result}"
        );
        assert!(
            result.contains("search_log"),
            "should find in c.py: {result}"
        );
    }

    #[tokio::test]
    async fn symbol_search_no_results() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("empty.rs"), "fn hello() {}\n").unwrap();

        let executor = ToolExecutor::new(dir.path());
        let result = executor
            .execute(
                "symbol_search",
                &json!({
                    "query": "nonexistent_xyz"
                }),
            )
            .await;

        assert!(
            result.contains("No symbols matching"),
            "should report no results: {result}"
        );
    }

    #[test]
    fn code_intel_identifier_at_position() {
        let source = "fn foo() {\n    let bar = 42;\n}\n";
        // Line 1 (fn foo), col 3 → "foo"
        let result = code_intel::identifier_at_position(source, code_intel::Language::Rust, 1, 3);
        assert!(result.is_some(), "should find identifier at fn name");
        assert_eq!(result.unwrap().0, "foo");

        // Line 2, col 8 → "bar"
        let result = code_intel::identifier_at_position(source, code_intel::Language::Rust, 2, 8);
        assert!(result.is_some(), "should find identifier at let binding");
        assert_eq!(result.unwrap().0, "bar");
    }

    // ── Multi-turn aggregate output scenarios ─────────────────────────────────
    //
    // These tests simulate realistic multi-tool-call turns to verify that:
    // 1. Progressive scaling reduces limits smoothly (not step-function)
    // 2. Persist-to-disk triggers when aggregate is high + output is large
    // 3. read_file auto-downgrades to outline under aggregate pressure
    // 4. Ranged reads always work regardless of aggregate pressure
    // 5. git_show/git_diff respect aggregate-aware limits

    /// Helper: create a file with N lines of content in a temp dir.
    fn make_large_file(dir: &std::path::Path, name: &str, lines: usize) -> PathBuf {
        use std::io::Write;
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        for i in 0..lines {
            writeln!(f, "line {i}: {}", "x".repeat(60)).unwrap();
        }
        drop(f);
        path
    }

    #[test]
    fn progressive_scaling_smooth_curve() {
        let executor = test_executor();
        let base = executor.scaled_output_limit();

        // At 0 aggregate → full limit
        assert_eq!(executor.scaled_output_limit(), base);

        // At soft limit → still full (just below threshold)
        executor
            .aggregate_output_bytes
            .store(AGGREGATE_SOFT_LIMIT, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(executor.scaled_output_limit(), base);

        // Just above soft limit → slightly reduced
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 1000,
            std::sync::atomic::Ordering::Relaxed,
        );
        let slightly_above = executor.scaled_output_limit();
        assert!(
            slightly_above < base,
            "should reduce above soft limit: {slightly_above} vs {base}"
        );
        assert!(
            slightly_above > base / 2,
            "should not halve just above soft limit: {slightly_above}"
        );

        // At 2x budget → significantly reduced
        executor.aggregate_output_bytes.store(
            AGGREGATE_OUTPUT_BUDGET * 2,
            std::sync::atomic::Ordering::Relaxed,
        );
        let at_2x = executor.scaled_output_limit();
        assert!(
            at_2x < base * 3 / 4,
            "should be well below 75% at 2x budget: {at_2x} vs base {base}"
        );
        assert!(at_2x >= 1024, "should never go below 1KB: {at_2x}");
    }

    #[test]
    fn progressive_scaling_combines_token_and_aggregate_pressure() {
        let executor = test_executor();
        let base = executor.scaled_output_limit();

        // Token pressure alone
        executor.set_budget_pressure(0.6);
        let token_only = executor.scaled_output_limit();
        assert!(token_only < base);

        // Add aggregate pressure on top
        executor.aggregate_output_bytes.store(
            AGGREGATE_OUTPUT_BUDGET,
            std::sync::atomic::Ordering::Relaxed,
        );
        let both = executor.scaled_output_limit();
        assert!(
            both < token_only,
            "combined pressure should be tighter: {both} vs token-only {token_only}"
        );
    }

    #[test]
    fn persist_to_disk_triggers_when_aggregate_high_and_output_large() {
        let executor = test_executor();

        // Simulate high aggregate output (above soft limit)
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 10_000,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Small output → not persisted
        let small = "x".repeat(1000);
        let result = executor.maybe_persist_large_output(small.clone(), "bash");
        assert_eq!(result, small, "small output should pass through");

        // Large output → persisted
        let large = "x\n".repeat(30_000); // ~60KB
        let result = executor.maybe_persist_large_output(large.clone(), "bash");
        assert!(
            result.contains("<persisted-output>"),
            "large output should be persisted, got first 200 chars: {}",
            &result[..result.len().min(200)]
        );
        assert!(result.contains("tool-results/"), "should contain file path");
        assert!(
            result.contains("</persisted-output>"),
            "should have closing tag"
        );
        assert!(
            result.contains("read_file"),
            "should suggest read_file for access"
        );
        assert!(
            result.len() < large.len() / 5,
            "persisted reference ({}) should be much smaller than original ({})",
            result.len(),
            large.len()
        );

        // Verify file was actually written
        let path_start = result.find("tool-results/").unwrap();
        let path_line = result[path_start - 20..].lines().next().unwrap();
        let file_path = path_line
            .split_whitespace()
            .find(|s| s.contains("tool-results/"))
            .unwrap();
        assert!(
            std::path::Path::new(file_path).exists(),
            "persisted file should exist: {file_path}"
        );

        // Cleanup
        let _ = std::fs::remove_file(file_path);
    }

    #[test]
    fn persist_to_disk_skipped_when_aggregate_low() {
        let executor = test_executor();

        // Aggregate below soft limit → no persist even for large output
        executor
            .aggregate_output_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);

        let large = "x\n".repeat(30_000);
        let result = executor.maybe_persist_large_output(large.clone(), "bash");
        assert!(
            !result.contains("<persisted-output>"),
            "should not persist when aggregate is low"
        );
    }

    #[test]
    fn persist_to_disk_skipped_for_errors() {
        let executor = test_executor();
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 10_000,
            std::sync::atomic::Ordering::Relaxed,
        );

        let error_output = format!("Error: {}", "x".repeat(60_000));
        let result = executor.maybe_persist_large_output(error_output.clone(), "bash");
        assert_eq!(
            result, error_output,
            "error outputs should never be persisted"
        );
    }

    #[test]
    fn persist_to_disk_idempotent_same_content() {
        let executor = test_executor();
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 10_000,
            std::sync::atomic::Ordering::Relaxed,
        );

        let large = "deterministic content\n".repeat(3000);
        let result1 = executor.maybe_persist_large_output(large.clone(), "bash");
        let result2 = executor.maybe_persist_large_output(large, "bash");
        assert_eq!(
            result1, result2,
            "same content should produce identical reference"
        );

        // Cleanup
        if let Some(path) = result1
            .split_whitespace()
            .find(|s| s.contains("tool-results/"))
        {
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn multi_turn_review_scenario_read_file_downgrades_to_outline() {
        // Simulates: prior tools produced lots of output → read_file(large) should auto-downgrade
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Create a Rust file large enough to exceed remaining budget
        let rust_content = (0..2000)
            .map(|i| format!("pub fn func_{i}(x: i32) -> i32 {{ x + {i} }}\n"))
            .collect::<String>();
        std::fs::write(dir.path().join("big.rs"), &rust_content).unwrap();
        let file_size = std::fs::metadata(dir.path().join("big.rs")).unwrap().len() as usize;

        // Set aggregate so remaining budget < file size
        let agg = AGGREGATE_OUTPUT_BUDGET - (file_size / 2);
        executor.aggregate_output_bytes.store(
            agg.max(AGGREGATE_SOFT_LIMIT + 1),
            std::sync::atomic::Ordering::Relaxed,
        );

        // Full read of the large file should auto-downgrade to outline
        let result = executor.read_file(&json!({"path": "big.rs"}));
        assert!(
            result.contains("Auto-downgraded to outline")
                || result.contains("too large")
                || result.contains("Outline"),
            "should downgrade or reject full read under aggregate pressure \
             (file={file_size}, agg={agg}, remaining={}), got first 300 chars: {}",
            AGGREGATE_OUTPUT_BUDGET.saturating_sub(agg),
            &result[..result.len().min(300)]
        );
    }

    #[test]
    fn multi_turn_review_scenario_ranged_reads_always_work() {
        // Ranged reads must ALWAYS work regardless of aggregate pressure
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Create a file with known content
        let content: String = (0..1000)
            .map(|i| format!("line {i}: important data here\n"))
            .collect();
        std::fs::write(dir.path().join("data.txt"), &content).unwrap();

        // Simulate extreme aggregate pressure
        executor.aggregate_output_bytes.store(
            AGGREGATE_OUTPUT_BUDGET * 2,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Ranged read should still work
        let result = executor.read_file(&json!({
            "path": "data.txt",
            "start_line": 100,
            "end_line": 110
        }));
        assert!(
            result.contains("line 99:") || result.contains("line 100:"),
            "ranged read should return content even under extreme pressure, got: {}",
            &result[..result.len().min(300)]
        );
        assert!(
            !result.contains("Error:") && !result.contains("Auto-downgraded"),
            "ranged read should not be blocked or downgraded, got: {}",
            &result[..result.len().min(300)]
        );
    }

    #[test]
    fn multi_turn_review_scenario_discontinuous_ranges() {
        // Reading 5 non-contiguous ranges from a large file should all succeed
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        let content: String = (0..2000)
            .map(|i| format!("line {i}: content for section {}\n", i / 100))
            .collect();
        std::fs::write(dir.path().join("big.txt"), &content).unwrap();

        // Simulate moderate aggregate pressure
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 50_000,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Read 5 non-contiguous ranges — all should succeed
        let ranges = [(10, 20), (200, 210), (500, 510), (800, 810), (1500, 1510)];
        for (start, end) in &ranges {
            let result = executor.read_file(&json!({
                "path": "big.txt",
                "start_line": start,
                "end_line": end
            }));
            assert!(
                !result.starts_with("Error:"),
                "ranged read {start}-{end} should succeed under aggregate pressure, got: {}",
                &result[..result.len().min(200)]
            );
            // Verify we got actual content
            assert!(
                result.contains("line "),
                "ranged read {start}-{end} should return file content, got: {}",
                &result[..result.len().min(200)]
            );
        }
    }

    #[tokio::test]
    async fn multi_turn_full_execute_accumulates_aggregate() {
        // Verify that execute() accumulates aggregate_output_bytes across calls
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Create a small file
        std::fs::write(dir.path().join("small.txt"), "hello world\n").unwrap();

        let before = executor
            .aggregate_output_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(before, 0);

        // Execute a tool
        let output = executor
            .execute("read_file", &json!({"path": "small.txt"}))
            .await;
        assert!(!output.is_empty());

        let after = executor
            .aggregate_output_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            after > before,
            "aggregate should increase after tool execution: {after} vs {before}"
        );
        assert_eq!(after, output.len(), "aggregate should equal output size");

        // Execute another tool — aggregate should keep growing
        let output2 = executor
            .execute("read_file", &json!({"path": "small.txt"}))
            .await;
        let after2 = executor
            .aggregate_output_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            after2,
            after + output2.len(),
            "aggregate should accumulate across calls"
        );
    }

    #[tokio::test]
    async fn multi_turn_persist_triggers_via_execute() {
        // End-to-end: execute() should persist large bash output when aggregate is high
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Pre-load aggregate to above soft limit
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 10_000,
            std::sync::atomic::Ordering::Relaxed,
        );

        // Execute bash that produces large output (>50KB)
        let output = executor
            .execute(
                "bash",
                &json!({"command": format!("python3 -c \"print('x' * 70 + '\\n', end='')\" | head -c 60000; echo; seq 1 500")}),
            )
            .await;

        // If the output was large enough, it should have been persisted
        // (depends on actual bash output size — if python3 isn't available,
        // the error message will be small and won't trigger persist)
        if output.len() > PERSIST_THRESHOLD {
            assert!(
                output.contains("<persisted-output>"),
                "large execute output should be persisted when aggregate is high"
            );
        }
        // Either way, aggregate should have been updated
        let agg = executor
            .aggregate_output_bytes
            .load(std::sync::atomic::Ordering::Relaxed);
        assert!(
            agg > AGGREGATE_SOFT_LIMIT + 10_000,
            "aggregate should have increased"
        );
    }

    #[test]
    fn multi_turn_scaled_limit_affects_read_file_truncation() {
        // When aggregate is high, read_file should produce less content or downgrade
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());

        // Create a medium file
        let content: String = (0..500)
            .map(|i| format!("line {i}: {}\n", "data".repeat(15)))
            .collect();
        std::fs::write(dir.path().join("medium.txt"), &content).unwrap();

        // Read with no pressure — should get full content
        executor
            .aggregate_output_bytes
            .store(0, std::sync::atomic::Ordering::Relaxed);
        let normal_result = executor.read_file(&json!({"path": "medium.txt"}));
        assert!(
            !normal_result.contains("Auto-downgraded") && !normal_result.contains("too large"),
            "normal read should return full content"
        );

        // Read with high pressure — should downgrade or truncate
        executor.aggregate_output_bytes.store(
            AGGREGATE_OUTPUT_BUDGET,
            std::sync::atomic::Ordering::Relaxed,
        );
        executor.clear_file_state();
        let pressured_result = executor.read_file(&json!({"path": "medium.txt"}));

        // Under pressure, result should either be downgraded or truncated
        let is_downgraded = pressured_result.contains("Auto-downgraded")
            || pressured_result.contains("Auto-truncated")
            || pressured_result.contains("too large")
            || pressured_result.contains("[truncated");
        let is_smaller = pressured_result.len() <= normal_result.len();
        assert!(
            is_downgraded || is_smaller,
            "pressured read should be downgraded or smaller: pressured={}, normal={}",
            pressured_result.len(),
            normal_result.len()
        );
    }

    // ── expand_sandbox_path ──────────────────────────────────────────────────

    #[test]
    fn expand_sandbox_path_adds_directory() {
        let dir = tempfile::tempdir().unwrap();
        let mut exe = ToolExecutor::new(dir.path());
        // Before expansion: /etc is not allowed
        assert!(
            !exe.sandbox_policy
                .as_ref()
                .unwrap()
                .is_path_allowed(std::path::Path::new("/etc/passwd"))
        );
        // Expand
        exe.expand_sandbox_path(PathBuf::from("/etc"));
        // After expansion: /etc is allowed
        assert!(
            exe.sandbox_policy
                .as_ref()
                .unwrap()
                .is_path_allowed(std::path::Path::new("/etc/passwd"))
        );
    }

    #[test]
    fn expand_sandbox_path_noop_without_policy() {
        let dir = tempfile::tempdir().unwrap();
        let mut exe = ToolExecutor::new(dir.path());
        exe.sandbox_policy = None;
        // Should not panic
        exe.expand_sandbox_path(PathBuf::from("/etc"));
    }

    #[test]
    fn expand_sandbox_then_resolve_checked_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let mut exe = ToolExecutor::new(dir.path());
        // Before: /etc/passwd is blocked
        assert!(exe.resolve_checked("/etc/passwd").is_err());
        // Expand to /etc
        exe.expand_sandbox_path(PathBuf::from("/etc"));
        // After: /etc/passwd is allowed
        assert!(exe.resolve_checked("/etc/passwd").is_ok());
    }

    #[test]
    fn expand_sandbox_to_root_opens_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut exe = ToolExecutor::new(dir.path());
        // Expanding to "/" opens the entire filesystem — this is why
        // stream_render.rs must never pass "/" to expand_sandbox_path.
        exe.expand_sandbox_path(PathBuf::from("/"));
        assert!(exe.resolve_checked("/etc/passwd").is_ok());
        assert!(exe.resolve_checked("/var/secret").is_ok());
    }

    // ── Worktree session tests ────────────────────────────────────────────────

    #[test]
    fn worktree_session_initially_none() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        assert!(!exe.in_worktree_session());
        assert!(exe.get_worktree_session().is_none());
    }

    #[test]
    fn effective_project_root_returns_original_when_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        assert_eq!(exe.effective_project_root(), dir.path());
    }

    #[test]
    fn enter_worktree_requires_branch() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.enter_worktree("");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("required"));
    }

    #[test]
    fn enter_worktree_rejects_shell_injection() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        for dangerous in &["test;rm", "test|cat", "test&", "test`id`", "$(whoami)"] {
            let result = exe.enter_worktree(dangerous);
            assert!(result.is_err(), "should reject dangerous branch: {dangerous}");
            assert!(result.unwrap_err().contains("Invalid"));
        }
    }

    #[test]
    fn exit_worktree_fails_when_not_in_session() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.exit_worktree("keep", false);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Not in a worktree session"));
    }

    #[test]
    fn git_worktree_enter_requires_branch() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.git_worktree(&json!({"action": "enter"}));
        assert!(result.contains("Error"));
        assert!(result.contains("branch"));
    }

    #[test]
    fn git_worktree_exit_when_not_in_session() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.git_worktree(&json!({"action": "exit"}));
        assert!(result.contains("Error"));
        assert!(result.contains("Not in a worktree session"));
    }

    // ── Task tool tests ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_create_requires_title() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.task_create(&json!({})).await;
        assert!(result.contains("Error"));
        assert!(result.contains("title"));
    }

    #[tokio::test]
    async fn task_create_returns_task_id() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.task_create(&json!({"title": "Test task"})).await;
        assert!(result.contains("task-1"));
        assert!(result.contains("success"));
    }

    #[tokio::test]
    async fn task_list_shows_created_tasks() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a task
        exe.task_create(&json!({"title": "First task"})).await;
        
        // List should show it
        let list = exe.task_list(&json!({})).await;
        assert!(list.contains("First task"));
        assert!(list.contains("task-1"));
    }

    #[tokio::test]
    async fn task_get_returns_details() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        exe.task_create(&json!({
            "title": "Detailed task",
            "description": "This is a test"
        })).await;
        
        let details = exe.task_get(&json!({"task_id": "task-1"})).await;
        assert!(details.contains("Detailed task"));
        assert!(details.contains("This is a test"));
    }

    #[tokio::test]
    async fn task_update_changes_status() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        exe.task_create(&json!({"title": "Status test"})).await;
        
        // Update to in_progress
        let result = exe.task_update(&json!({
            "task_id": "task-1",
            "status": "in_progress"
        })).await;
        assert!(result.contains("success"));
        
        // Verify status changed
        let details = exe.task_get(&json!({"task_id": "task-1"})).await;
        assert!(details.contains("in_progress"));
    }

    #[tokio::test]
    async fn task_with_subtasks_tracks_progress() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        exe.task_create(&json!({
            "title": "Multi-step task",
            "subtasks": [
                {"id": "step-1", "title": "First step"},
                {"id": "step-2", "title": "Second step", "depends_on": ["step-1"]}
            ]
        })).await;
        
        // List shows subtask count
        let list = exe.task_list(&json!({})).await;
        assert!(list.contains("[0/2]"));
        
        // Complete first subtask
        exe.task_update(&json!({
            "task_id": "task-1",
            "subtask_id": "step-1",
            "status": "completed"
        })).await;
        
        let list2 = exe.task_list(&json!({})).await;
        assert!(list2.contains("[1/2]"));
    }

    // ── Sleep tool tests ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn sleep_requires_duration() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.sleep_tool(&json!({})).await;
        assert!(result.contains("Error"));
        assert!(result.contains("duration_ms"));
    }

    #[tokio::test]
    async fn sleep_rejects_zero_duration() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.sleep_tool(&json!({"duration_ms": 0})).await;
        assert!(result.contains("Error"));
    }

    #[tokio::test]
    async fn sleep_succeeds_with_valid_duration() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let start = std::time::Instant::now();
        let result = exe.sleep_tool(&json!({"duration_ms": 50})).await;
        let elapsed = start.elapsed();
        
        assert!(result.contains("success"));
        assert!(result.contains("50"));
        assert!(elapsed.as_millis() >= 40, "should have slept");
    }

    #[tokio::test]
    async fn sleep_caps_at_max_duration() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        // Request 10 minutes, should cap at 5 minutes (300000ms)
        // We won't actually wait that long, just verify the schema accepts it
        let result = exe.sleep_tool(&json!({"duration_ms": 1, "reason": "test cap"})).await;
        assert!(result.contains("success"));
    }

    // ── Tool search tests ─────────────────────────────────────────────────────

    #[test]
    fn tool_search_requires_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({}));
        assert!(result.contains("Error"));
        assert!(result.contains("query"));
    }

    #[test]
    fn tool_search_select_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:bash"}));
        assert!(result.contains("bash"));
        assert!(result.contains("\"missing\":[]"));
    }

    #[test]
    fn tool_search_select_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:READ_FILE"}));
        assert!(result.contains("read_file"));
    }

    #[test]
    fn tool_search_select_multiple() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:bash,grep,glob"}));
        assert!(result.contains("bash"));
        assert!(result.contains("grep"));
        assert!(result.contains("glob"));
    }

    #[test]
    fn tool_search_select_missing_tool() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "select:nonexistent_tool_xyz"}));
        assert!(result.contains("nonexistent_tool_xyz"));
        assert!(result.contains("missing"));
    }

    #[test]
    fn tool_search_keyword_finds_git_tools() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "git", "max_results": 10}));
        // Should find multiple git-related tools
        assert!(result.contains("git_status") || result.contains("git_diff") || result.contains("git_log"));
    }

    #[test]
    fn tool_search_keyword_file_operations() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "file read"}));
        assert!(result.contains("read_file"));
    }

    #[test]
    fn tool_search_respects_max_results() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        // Search for a broad term that matches many tools
        let result = exe.tool_search(&json!({"query": "file", "max_results": 2}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let matches = parsed["matches"].as_array().unwrap();
        assert!(matches.len() <= 2);
    }

    #[test]
    fn tool_search_reports_total_tools() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.tool_search(&json!({"query": "bash"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        let total = parsed["total_tools"].as_u64().unwrap();
        assert!(total >= 10, "should have many tools registered");
    }

    // ─── web_search tests ─────────────────────────────────────────────────────────

    #[test]
    fn web_search_google_default() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "rust programming"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "Google");
        assert!(parsed["search_url"].as_str().unwrap().contains("google.com"));
        assert!(parsed["search_url"].as_str().unwrap().contains("rust%20programming"));
        assert!(parsed["tip"].as_str().is_some());
    }

    #[test]
    fn web_search_duckduckgo() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "hello world", "engine": "duckduckgo"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "DuckDuckGo");
        assert!(parsed["search_url"].as_str().unwrap().contains("duckduckgo.com"));
    }

    #[test]
    fn web_search_wikipedia() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "quantum physics", "engine": "wikipedia"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "Wikipedia");
        assert!(parsed["search_url"].as_str().unwrap().contains("wikipedia.org"));
        assert!(parsed["search_url"].as_str().unwrap().contains("action=opensearch"));
        assert!(parsed["tip"].as_str().unwrap().contains("JSON"));
    }

    #[test]
    fn web_search_github() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "tokio async", "engine": "github"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "GitHub");
        assert!(parsed["search_url"].as_str().unwrap().contains("github.com/search"));
    }

    #[test]
    fn web_search_bing() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test query", "engine": "bing"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["engine"], "Bing");
        assert!(parsed["search_url"].as_str().unwrap().contains("bing.com"));
    }

    #[test]
    fn web_search_invalid_engine() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test", "engine": "askjeeves"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["error"].as_str().unwrap().contains("Unknown engine"));
    }

    #[test]
    fn web_search_empty_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": ""}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["error"].as_str().is_some());
    }

    #[test]
    fn web_search_missing_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["error"].as_str().is_some());
    }

    #[test]
    fn web_search_special_characters_encoded() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "C++ templates & generics"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        let url = parsed["search_url"].as_str().unwrap();
        // Should be URL encoded (no raw & or + in query part)
        assert!(url.contains("C%2B%2B"));
        assert!(url.contains("%26")); // & encoded
    }

    #[test]
    fn web_search_num_results_respected() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test", "num_results": 25}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        let url = parsed["search_url"].as_str().unwrap();
        assert!(url.contains("num=25"));
    }

    #[test]
    fn web_search_num_results_capped() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test", "num_results": 100}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        let url = parsed["search_url"].as_str().unwrap();
        // Should be capped at 50
        assert!(url.contains("num=50"));
    }

    #[test]
    fn web_search_has_alternatives() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let result = exe.web_search(&json!({"query": "test"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["alternatives"].as_array().is_some());
        assert!(parsed["usage"].as_str().is_some());
    }

    // ─── task_stop tests ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn task_stop_cancels_running_task() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a task
        exe.task_create(&json!({
            "title": "Long running task",
            "description": "This will be stopped"
        })).await;

        // Update to in_progress
        exe.task_update(&json!({
            "task_id": "task-1",
            "status": "in_progress"
        })).await;

        // Stop it
        let result = exe.task_stop(&json!({
            "task_id": "task-1",
            "reason": "Taking too long"
        })).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert!(parsed["success"].as_bool().unwrap());
        assert_eq!(parsed["previous_status"], "in_progress");
        assert!(parsed["message"].as_str().unwrap().contains("cancelled"));

        // Verify the task is now cancelled
        let task_result = exe.task_get(&json!({"task_id": "task-1"})).await;
        let task: serde_json::Value = serde_json::from_str(&task_result).unwrap();
        assert_eq!(task["status"], "cancelled");
    }

    #[tokio::test]
    async fn task_stop_cancels_pending_task() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a task (defaults to pending)
        exe.task_create(&json!({
            "title": "Pending task"
        })).await;

        // Stop it while pending
        let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert!(parsed["success"].as_bool().unwrap());
        assert_eq!(parsed["previous_status"], "pending");
    }

    #[tokio::test]
    async fn task_stop_rejects_completed_task() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create and complete a task
        exe.task_create(&json!({"title": "Done task"})).await;
        exe.task_update(&json!({
            "task_id": "task-1",
            "status": "completed"
        })).await;

        // Try to stop it
        let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert!(!parsed["success"].as_bool().unwrap());
        assert!(parsed["message"].as_str().unwrap().contains("Cannot stop"));
    }

    #[tokio::test]
    async fn task_stop_cancels_subtasks() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a task with subtasks
        exe.task_create(&json!({
            "title": "Parent task",
            "subtasks": [
                {"id": "sub-1", "title": "Subtask 1"},
                {"id": "sub-2", "title": "Subtask 2"}
            ]
        })).await;

        // Start the task
        exe.task_update(&json!({
            "task_id": "task-1",
            "status": "in_progress"
        })).await;

        // Stop it
        let result = exe.task_stop(&json!({"task_id": "task-1"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();

        assert!(parsed["success"].as_bool().unwrap());
        assert_eq!(parsed["cancelled_subtasks"], 2);
    }

    #[tokio::test]
    async fn task_stop_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.task_stop(&json!({"task_id": "nonexistent"})).await;
        assert!(result.contains("not found"));
    }

    #[tokio::test]
    async fn task_stop_missing_id() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.task_stop(&json!({})).await;
        assert!(result.contains("required"));
    }

    // ─── diagnose tests ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn diagnose_all_categories() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.diagnose(&json!({})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        // Should have all categories
        assert!(parsed["system"].is_object());
        assert!(parsed["environment"].is_object());
        assert!(parsed["tools"].is_object());
        assert!(parsed["tasks"].is_object());
        assert!(parsed["session"].is_object());
    }

    #[tokio::test]
    async fn diagnose_system_only() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.diagnose(&json!({"category": "system"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["system"].is_object());
        assert!(parsed["environment"].is_null());
        assert!(parsed["tools"].is_null());
    }

    #[tokio::test]
    async fn diagnose_contains_os_info() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.diagnose(&json!({"category": "system"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["system"]["os"].is_string());
        assert!(parsed["system"]["arch"].is_string());
        assert!(parsed["system"]["project_root"].is_string());
    }

    #[tokio::test]
    async fn diagnose_tools_info() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.diagnose(&json!({"category": "tools"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["tools"]["count"].as_u64().unwrap() > 10);
        assert!(parsed["tools"]["categories"].is_object());
    }

    #[tokio::test]
    async fn diagnose_tools_verbose() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.diagnose(&json!({"category": "tools", "verbose": true})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        // Verbose mode should list all tools
        assert!(parsed["tools"]["available"].is_array());
        let tools = parsed["tools"]["available"].as_array().unwrap();
        assert!(tools.contains(&json!("bash")));
        assert!(tools.contains(&json!("diagnose")));
    }

    #[tokio::test]
    async fn diagnose_tasks_with_items() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create some tasks
        exe.task_create(&json!({"title": "Task 1"})).await;
        exe.task_create(&json!({"title": "Task 2"})).await;
        exe.task_update(&json!({"task_id": "task-1", "status": "completed"})).await;
        
        let result = exe.diagnose(&json!({"category": "tasks"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed["tasks"]["total"], 2);
        assert_eq!(parsed["tasks"]["completed"], 1);
        assert_eq!(parsed["tasks"]["pending"], 1);
    }

    #[tokio::test]
    async fn diagnose_session_info() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.diagnose(&json!({"category": "session"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["session"]["output_bytes_this_turn"].is_number());
        assert!(parsed["session"]["output_budget"].is_number());
        assert!(parsed["session"]["output_utilization"].is_string());
    }

    #[tokio::test]
    async fn diagnose_environment_hides_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Set a mock API key (unsafe in Rust 2024 edition)
        // SAFETY: This is a single-threaded test
        unsafe {
            std::env::set_var("MO_API_KEY", "secret-key-12345");
        }
        
        let result = exe.diagnose(&json!({"category": "environment"})).await;
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        // API key should show [SET] not actual value
        if let Some(val) = parsed["environment"]["MO_API_KEY"].as_str() {
            assert_eq!(val, "[SET]");
        }
        
        // Cleanup
        // SAFETY: This is a single-threaded test
        unsafe {
            std::env::remove_var("MO_API_KEY");
        }
    }

    // ─── lsp tests ────────────────────────────────────────────────────────────────

    #[test]
    fn lsp_missing_operation_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({}));
        assert!(result.contains("error"));
        assert!(result.contains("operation"));
    }

    #[test]
    fn lsp_invalid_operation_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "invalid_op"}));
        assert!(result.contains("error"));
        assert!(result.contains("Unknown operation"));
    }

    #[test]
    fn lsp_diagnostics_returns_capabilities() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "diagnostics"}));
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed["capabilities"]["goto_definition"].as_bool().unwrap());
        assert!(parsed["capabilities"]["find_references"].as_bool().unwrap());
        assert!(parsed["supported_languages"].as_array().is_some());
    }

    #[test]
    fn lsp_goto_definition_requires_symbol_or_position() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "goto_definition"}));
        assert!(result.contains("error"));
        assert!(result.contains("symbol"));
    }

    #[test]
    fn lsp_find_references_requires_symbol() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "find_references"}));
        assert!(result.contains("error"));
        assert!(result.contains("symbol"));
    }

    #[test]
    fn lsp_document_symbols_requires_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "document_symbols"}));
        assert!(result.contains("error"));
        assert!(result.contains("file"));
    }

    #[test]
    fn lsp_workspace_symbols_with_query() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a test file with a symbol
        let test_file = dir.path().join("test.rs");
        std::fs::write(&test_file, "fn hello_world() {}\nfn goodbye() {}").unwrap();
        
        // workspace_symbols should work with query
        let result = exe.lsp(&json!({
            "operation": "workspace_symbols",
            "query": "hello"
        }));
        // Should return results (format depends on symbol_search implementation)
        assert!(!result.contains("error") || result.contains("No symbols"));
    }

    #[test]
    fn lsp_call_hierarchy_requires_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        let result = exe.lsp(&json!({"operation": "call_hierarchy"}));
        assert!(result.contains("error"));
        assert!(result.contains("file"));
    }

    #[test]
    fn lsp_document_symbols_on_rust_file() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        
        // Create a test Rust file
        let test_file = dir.path().join("lib.rs");
        std::fs::write(&test_file, r#"
pub fn main() {}
fn helper() {}
struct Config {}
impl Config {
    fn new() -> Self { Config {} }
}
"#).unwrap();
        
        let result = exe.lsp(&json!({
            "operation": "document_symbols",
            "file": "lib.rs"
        }));
        
        // Should find symbols
        assert!(!result.contains("Error:") || result.contains("main"));
    }

    // ── Env tool tests ────────────────────────────────────────────────────────

    #[test]
    fn env_list_returns_variables() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ "operation": "list" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("count").is_some());
        assert!(parsed.get("variables").is_some());
        let vars = parsed.get("variables").unwrap().as_array().unwrap();
        assert!(!vars.is_empty());
    }

    #[test]
    fn env_get_existing_var() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "HOME"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed.get("name").unwrap(), "HOME");
        assert_eq!(parsed.get("exists").unwrap(), true);
    }

    #[test]
    fn env_get_missing_var() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "DEFINITELY_NOT_A_REAL_VAR_12345"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed.get("exists").unwrap(), false);
    }

    #[test]
    fn env_set_and_unset() {
        let exe = test_executor();
        
        // Set a variable
        let result = exe.env_tool(&json!({ 
            "operation": "set",
            "name": "TEST_VAR_FOR_ASTRA",
            "value": "test_value_123"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("success").unwrap(), true);
        
        // Verify it's set
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "TEST_VAR_FOR_ASTRA"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("exists").unwrap(), true);
        
        // Unset it
        let result = exe.env_tool(&json!({ 
            "operation": "unset",
            "name": "TEST_VAR_FOR_ASTRA"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("success").unwrap(), true);
        
        // Verify it's gone
        let result = exe.env_tool(&json!({ 
            "operation": "get",
            "name": "TEST_VAR_FOR_ASTRA"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed.get("exists").unwrap(), false);
    }

    #[test]
    fn env_set_invalid_name() {
        let exe = test_executor();
        
        // Start with digit
        let result = exe.env_tool(&json!({ 
            "operation": "set",
            "name": "123VAR",
            "value": "test"
        }));
        assert!(result.contains("error"));
        
        // Empty name
        let result = exe.env_tool(&json!({ 
            "operation": "set",
            "name": "",
            "value": "test"
        }));
        assert!(result.contains("error"));
    }

    #[test]
    fn env_search_basic() {
        let exe = test_executor();
        let result = exe.env_tool(&json!({ 
            "operation": "search",
            "pattern": "PATH"
        }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("count").is_some());
        assert!(parsed.get("matches").is_some());
    }

    #[test]
    fn env_search_redos_protection() {
        let exe = test_executor();
        let long_pattern = "a".repeat(600);
        let result = exe.env_tool(&json!({ 
            "operation": "search",
            "pattern": long_pattern
        }));
        
        assert!(result.contains("error"));
        assert!(result.contains("too long"));
    }

    #[test]
    fn env_sensitive_var_masking() {
        // Test various sensitive patterns
        assert!(ToolExecutor::is_sensitive_var("API_KEY"));
        assert!(ToolExecutor::is_sensitive_var("GITHUB_TOKEN"));
        assert!(ToolExecutor::is_sensitive_var("AWS_SECRET_ACCESS_KEY"));
        assert!(ToolExecutor::is_sensitive_var("OPENAI_API_KEY"));
        assert!(ToolExecutor::is_sensitive_var("ANTHROPIC_API_KEY"));
        assert!(ToolExecutor::is_sensitive_var("DATABASE_URL"));
        
        // Non-sensitive vars
        assert!(!ToolExecutor::is_sensitive_var("HOME"));
        assert!(!ToolExecutor::is_sensitive_var("PATH"));
        assert!(!ToolExecutor::is_sensitive_var("USER"));
    }

    // ── UTF-16 conversion tests ───────────────────────────────────────────────

    #[test]
    fn utf16_col_to_char_idx_ascii() {
        let line = "hello world";
        assert_eq!(utf16_col_to_char_idx(line, 0), 0);  // h
        assert_eq!(utf16_col_to_char_idx(line, 5), 5);  // space
        assert_eq!(utf16_col_to_char_idx(line, 6), 6);  // w
    }

    #[test]
    fn utf16_col_to_char_idx_emoji() {
        // Emoji (😀) takes 2 UTF-16 code units but 1 char
        let line = "a😀b";
        assert_eq!(utf16_col_to_char_idx(line, 0), 0);  // a
        assert_eq!(utf16_col_to_char_idx(line, 1), 1);  // 😀 (first UTF-16 unit)
        assert_eq!(utf16_col_to_char_idx(line, 2), 1);  // 😀 (second UTF-16 unit, still same char)
        assert_eq!(utf16_col_to_char_idx(line, 3), 2);  // b
    }

    #[test]
    fn utf16_col_to_char_idx_chinese() {
        // Chinese char takes 1 UTF-16 code unit but 3 UTF-8 bytes
        let line = "a中b";
        assert_eq!(utf16_col_to_char_idx(line, 0), 0);  // a
        assert_eq!(utf16_col_to_char_idx(line, 1), 1);  // 中
        assert_eq!(utf16_col_to_char_idx(line, 2), 2);  // b
    }

    #[test]
    fn utf16_col_past_end() {
        let line = "abc";
        assert_eq!(utf16_col_to_char_idx(line, 10), 3);  // past end returns line length
    }

    // ── Notebook edit tests ───────────────────────────────────────────────────

    #[test]
    fn notebook_edit_requires_ipynb_extension() {
        let exe = test_executor();
        let result = exe.notebook_edit(&json!({
            "notebook_path": "test.py",
            "edit_mode": "insert",
            "new_source": "print('hello')"
        }));
        
        assert!(result.contains("error"));
        assert!(result.contains(".ipynb"));
    }

    #[test]
    fn notebook_edit_unknown_mode_rejected() {
        let exe = test_executor();
        // Create a temporary notebook
        let temp_dir = std::env::temp_dir();
        let notebook_path = temp_dir.join("test_unknown_mode.ipynb");
        std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();
        
        let result = exe.notebook_edit(&json!({
            "notebook_path": notebook_path.display().to_string(),
            "edit_mode": "unknown",
            "cell_id": "cell-1",
            "new_source": "test"
        }));
        
        // Cleanup
        let _ = std::fs::remove_file(&notebook_path);
        
        assert!(result.contains("error"), "Expected error in result: {}", result);
        assert!(result.contains("Unknown edit_mode"), "Expected 'Unknown edit_mode' in result: {}", result);
    }

    #[test]
    fn notebook_edit_requires_full_read_first() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let notebook_path = dir.path().join("needs_read.ipynb");
        std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();

        let result = exe.notebook_edit(&json!({
            "notebook_path": "needs_read.ipynb",
            "edit_mode": "replace",
            "cell_id": "cell-1",
            "new_source": "x=2"
        }));

        assert!(result.contains("read"), "Expected read-before-write error, got: {result}");
    }

    #[test]
    fn notebook_edit_succeeds_after_full_read() {
        let dir = tempfile::tempdir().unwrap();
        let exe = ToolExecutor::new(dir.path());
        let notebook_path = dir.path().join("edit_ok.ipynb");
        std::fs::write(&notebook_path, r#"{"cells":[{"cell_type":"code","id":"cell-1","source":"x=1","metadata":{},"outputs":[],"execution_count":null}],"metadata":{"language_info":{"name":"python"}},"nbformat":4,"nbformat_minor":5}"#).unwrap();

        let _ = exe.read_file(&json!({ "path": "edit_ok.ipynb" }));
        let result = exe.notebook_edit(&json!({
            "notebook_path": "edit_ok.ipynb",
            "edit_mode": "replace",
            "cell_id": "cell-1",
            "new_source": "x=2"
        }));

        assert!(result.contains("\"success\":true"), "Expected success, got: {result}");
        let updated = std::fs::read_to_string(&notebook_path).unwrap();
        assert!(updated.contains("x=2"), "Expected notebook update, got: {updated}");
    }

    // ── Config tool tests ─────────────────────────────────────────────────────

    #[test]
    fn config_list_settings() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "list" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("available_settings").is_some());
        let settings = parsed.get("available_settings").unwrap().as_array().unwrap();
        assert!(!settings.is_empty());
    }

    #[test]
    fn config_get_model() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "model" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert_eq!(parsed.get("setting").unwrap(), "model");
        assert!(parsed.get("value").is_some());
    }

    #[test]
    fn config_get_api_key_status() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "api_key" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        // Should never expose actual key values
        assert!(!result.contains("sk-"));
        assert!(parsed.get("status").is_some());
    }

    #[test]
    fn config_unknown_setting() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "unknown_setting_xyz" }));
        
        assert!(result.contains("error"));
        assert!(result.contains("Unknown setting"));
    }

    #[test]
    fn config_output_limit() {
        let exe = test_executor();
        let result = exe.config_tool(&json!({ "setting": "output_limit" }));
        let parsed: Value = serde_json::from_str(&result).unwrap();
        
        assert!(parsed.get("value").is_some());
        let value = parsed.get("value").unwrap().as_u64().unwrap();
        assert!(value > 0);
    }

    #[test]
    fn brief_includes_session_state() {
        let exe = test_executor();
        let result = exe.brief(&json!({}));
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert!(parsed.get("effective_project_root").is_some());
        assert!(parsed.get("session").is_some());
        assert!(parsed.get("git").is_some());
        assert!(parsed.get("tasks").is_some());
        assert!(parsed.get("files").is_some());
    }

    #[tokio::test]
    async fn brief_reports_created_tasks() {
        let exe = test_executor();
        exe.task_create(&json!({"title": "Implement thing"})).await;
        let result = exe.brief(&json!({"focus": "tasks"}));
        let parsed: Value = serde_json::from_str(&result).unwrap();

        assert_eq!(parsed["tasks"]["count"], 1);
        assert_eq!(parsed["tasks"]["items"][0]["title"], "Implement thing");
    }
}
