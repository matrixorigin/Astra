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
    sync::atomic::AtomicBool,
    time::Duration,
};

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

#[path = "edge_tools/agent_messaging.rs"]
pub mod agent_messaging;
#[path = "edge_tools/agent_spawning.rs"]
pub mod agent_spawning;
use astra_tools::build_test;
pub use astra_tools::code_intel;
#[path = "edge_tools/context_sharing.rs"]
pub mod context_sharing;
#[path = "edge_tools/fs.rs"]
mod fs_tools;
pub(crate) use astra_tools::fuzzy_replacer;
#[path = "edge_tools/git_gix.rs"]
mod git_gix;
#[path = "edge_tools/github.rs"]
mod github;
#[path = "edge_tools/lsp_stdio_session.rs"]
mod lsp_stdio_session;
#[path = "edge_tools/mo_tools.rs"]
mod mo_tools;
use astra_tools::passive_cargo_check;
pub(crate) use git_gix::GitCommitRollbackJournal;
pub(crate) use git_gix::GitStashRollbackJournal;
pub(crate) use mo_tools::DatabaseSnapshotRollbackJournal;
pub(crate) use session_state::SessionStateRollbackJournal;
#[path = "edge_tools/passive_lsp.rs"]
mod passive_lsp;
use astra_tools::passive_tsc_check;
#[path = "edge_tools/shell.rs"]
#[allow(clippy::needless_range_loop)]
mod shell;
use astra_tools::env_tools;
pub use astra_tools::schemas::all_tool_schemas;
pub use env_tools::apply_overlay as apply_env_overlay;
#[path = "edge_tools/code_analysis.rs"]
mod code_analysis;
#[path = "edge_tools/config_tool.rs"]
mod config_tool;
#[path = "edge_tools/context_tools.rs"]
mod context_tools;
#[path = "edge_tools/diagnose.rs"]
mod diagnose;
#[path = "edge_tools/file_state.rs"]
mod file_state;
#[path = "edge_tools/lsp_tools.rs"]
mod lsp_tools;
#[path = "edge_tools/notebook_edit.rs"]
mod notebook_edit;
#[path = "edge_tools/self_mod_tools.rs"]
mod self_mod_tools;
#[path = "edge_tools/session_state.rs"]
mod session_state;
use astra_tools::task_mgmt;
pub(crate) use task_mgmt::TaskManager;
#[path = "edge_tools/web_search.rs"]
mod web_search;
use file_state::FileState;
pub(crate) use file_state::ReadDedupKey;

/// Shared file-state cache handle for cross-turn read-before-write tracking.
pub(crate) type SharedFileState = std::sync::Arc<std::sync::Mutex<HashMap<PathBuf, FileState>>>;
#[path = "edge_tools/worktree.rs"]
mod worktree;
pub(crate) use worktree::GitWorktreeRollbackJournal;
pub use worktree::WorktreeSession;
use worktree::detect_git_remote_repos;
#[cfg(test)]
use worktree::extract_github_owner_repo;
#[path = "edge_tools/memoria.rs"]
pub(crate) mod memoria;
#[cfg(test)]
use astra_tools::memoria::parse_memory_search_contents;
#[path = "edge_tools/ask_user.rs"]
mod ask_user;
#[path = "edge_tools/context_analysis.rs"]
mod context_analysis;
#[path = "edge_tools/mcp_dispatch.rs"]
mod mcp_dispatch;
#[path = "edge_tools/tool_search.rs"]
mod tool_search;

// ─── Tool schema ─────────────────────────────────────────────────────────────

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

// ─── Tool execution ───────────────────────────────────────────────────────────

/// Global output size limit. Individual tools may have tighter limits.
/// Override with `ASTRA_GLOBAL_OUTPUT_LIMIT` env var.
fn global_output_limit() -> usize {
    astra_core::RuntimeLimits::global().global_output_limit
}
/// Per-tool default output limit for tools without explicit truncation.
/// Override with `ASTRA_TOOL_OUTPUT_LIMIT` env var.
fn tool_output_limit() -> usize {
    astra_core::RuntimeLimits::global().tool_output_limit
}

/// Per-tool output size caps (bytes).
///
/// Grep results are content-heavy (line-by-line matches), so a 10KB cap
/// prevents a single broad grep from consuming most of the aggregate budget.
/// Glob results are filename-only, so 100KB is fine. Bash has its own
/// streaming cap at 30KB. Everything else uses the global default.
///
/// Per-tool output size overrides (grep: 10K, glob: 100K, default: 50K).
pub(crate) fn per_tool_output_limit(tool_name: &str) -> usize {
    let base = tool_output_limit();
    match tool_name {
        "grep" => base.min(10_000),
        "glob" => base.min(100_000),
        "find_definition" | "find_references" => base.min(15_000),
        _ => base,
    }
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
/// Global default for large-output persistence threshold (50K).
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
/// Prevents model confusion from truly empty tool results.
fn normalize_empty_output(output: String, tool_name: &str) -> String {
    if output.trim().is_empty() {
        format!("({tool_name} completed with no output)")
    } else {
        output
    }
}

pub(crate) use astra_tools::git_gix::ToolExecutionOutcome;

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

pub struct ToolExecutor {
    pub project_root: PathBuf,
    /// Cloud API base URL — used to proxy memory tool calls through the server
    /// so the server can add user_id for proper multi-user isolation.
    pub cloud_base: Option<String>,
    /// Auth token for cloud proxy calls.
    cloud_token: std::sync::Arc<std::sync::RwLock<Option<String>>>,
    /// Optional GitHub token for authenticated GitHub API requests.
    pub github_token: Option<String>,
    /// Shared async GitHub client for edge tools.
    pub github_client: Client,
    /// Security sandbox policy for tool execution (None = Permissive/legacy).
    ///
    /// Wrapped in `RwLock` so the policy can be swapped per-turn (e.g. skill
    /// sandbox activation) while the executor is shared via `Arc<ToolExecutor>`.
    pub sandbox_policy: std::sync::RwLock<Option<SandboxPolicy>>,
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
    /// Whether the user has been notified about Memoria being down.
    /// Prevents spamming the same warning every turn.
    memoria_notified_down: std::sync::atomic::AtomicBool,
    /// File state tracker: records mtime after each read/write/edit.
    /// Used for staleness detection (prevent overwriting user edits)
    /// and dedup (skip re-reading unchanged files).
    file_state: std::sync::Arc<std::sync::Mutex<HashMap<PathBuf, FileState>>>,
    /// Per-turn aggregate tool output size (bytes). When this exceeds
    /// `AGGREGATE_OUTPUT_BUDGET`, subsequent tool outputs are truncated
    /// more aggressively.
    /// Per-turn aggregate budget is 200K.
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
    /// Wrapped in Arc so the REPL session can share the journal across turns.
    pub file_journal:
        std::sync::Arc<std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>>,
    /// MatrixOne snapshot journal — records captured pre-state snapshots so the
    /// executor can perform a bounded restore without reconstructing tool history.
    pub database_snapshot_journal:
        std::sync::Arc<std::sync::Mutex<mo_tools::DatabaseSnapshotRollbackJournal>>,
    /// Git stash rollback journal — records captured stash handles so bounded
    /// turn/batch rollback can re-apply shelved working tree state.
    pub git_stash_journal: std::sync::Arc<std::sync::Mutex<git_gix::GitStashRollbackJournal>>,
    /// Git commit rollback journal — records captured commit handles so bounded
    /// turn/batch rollback can revert recent committed history when it is still safe.
    pub git_commit_journal: std::sync::Arc<std::sync::Mutex<git_gix::GitCommitRollbackJournal>>,
    /// Git worktree rollback journal — records newly created worktrees so bounded
    /// turn/batch rollback can remove them again while they are still clean.
    pub git_worktree_journal:
        std::sync::Arc<std::sync::Mutex<worktree::GitWorktreeRollbackJournal>>,
    /// Session-state rollback journal — records bounded self-mod/task mutations so
    /// same-turn rollback can restore prior in-memory session state.
    session_state_journal:
        std::sync::Arc<std::sync::Mutex<session_state::SessionStateRollbackJournal>>,
    /// Current turn index for file journal entries. Set externally per-turn.
    pub journal_turn_index: std::sync::atomic::AtomicU32,
    /// Active worktree session state. When set, `effective_project_root()` returns
    /// the worktree path instead of the original `project_root`.
    worktree_session: std::sync::Mutex<Option<WorktreeSession>>,
    /// In-memory task manager for the current session.
    task_manager: std::sync::Arc<task_mgmt::TaskManager>,
    /// Optional agent spawning context for the `spawn_agent` tool.
    pub spawn_context: Option<agent_spawning::SpawnAgentContext>,
    /// Optional shared context cache for cross-agent knowledge sharing.
    /// Used by share_context and query_context tools.
    pub context_cache: Option<std::sync::Arc<astra_runtime::orchestration::SharedContextCache>>,
    /// Agent ID for context sharing attribution.
    pub agent_id: Option<String>,
    /// Optional messaging context for the `send_message` tool.
    pub send_message_context: std::sync::Mutex<Option<agent_messaging::SendMessageRuntimeContext>>,
    /// Optional observability session for context analysis tools.
    /// Provides access to per-turn context assembly traces, timing data,
    /// drift detection, and decision explanations.
    pub observability_session: Option<
        std::sync::Arc<
            std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
        >,
    >,
    /// Session id for persisting self-modification state and serving `astra self`
    /// compatible diagnostics from inside the live agent loop.
    active_session_id: std::sync::Mutex<Option<String>>,
    /// Self-modification pinned tool preferences (manual override hints).
    self_mod_pinned_tools: std::sync::Mutex<Vec<String>>,
    /// Self-modification deprioritized tool preferences (manual override hints).
    self_mod_deprioritized_tools: std::sync::Mutex<Vec<String>>,
    /// P3.1 seam: cross-session lessons loaded at session bootstrap.
    /// Populated once via `set_session_lessons`, then passed through on
    /// every `build_self_model_snapshot` for the session's lifetime.
    session_lessons: std::sync::Mutex<Vec<astra_runtime::self_model::LessonHint>>,
    /// P3.3 seam: latest auto-invoked diagnostic skill output.
    /// `AutoInvokeHandler::maybe_fire` writes each successful parse here;
    /// the next `build_self_model_snapshot` injects it into the prompt and
    /// `set_latest_skill_diagnosis(None)` clears it once the triggering
    /// condition has resolved.
    latest_skill_diagnosis: std::sync::Mutex<Option<astra_skills::auto_invoke::SkillDiagnosis>>,
    /// Per-turn mutation accounting for adjust_config governor.
    /// (turn_number, mutations_applied_on_turn)
    self_mod_mutation_counter: std::sync::Mutex<(u32, u32)>,
    /// Shared tool executor for delegating unknown tools to astra-tools.
    default_executor: astra_tools::executor::DefaultToolExecutor,
}

impl ToolExecutor {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = project_root.into();
        let preferred_repos = detect_git_remote_repos(&root);
        let sandbox = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
        Self {
            project_root: root.clone(),
            cloud_base: None,
            cloud_token: std::sync::Arc::new(std::sync::RwLock::new(None)),
            // TODO: Consider using a zeroize-capable wrapper for tokens to prevent
            // memory-resident secrets from lingering after drop.
            github_token: astra_tools::github::resolve_github_token(),
            // GitHub API is external traffic (api.github.com), so it honours
            // HTTPS_PROXY/ALL_PROXY via the authoritative helper in astra_core::net.
            // See core/src/net.rs for the workspace proxy policy (3e3d6fa8).
            github_client: astra_core::net::apply_env_proxy(
                Client::builder()
                    .timeout(Duration::from_secs(15))
                    .user_agent(format!("astra/{}", env!("CARGO_PKG_VERSION"))),
            )
            .build()
            .unwrap_or_else(|_| Client::new()),
            sandbox_policy: std::sync::RwLock::new(Some(sandbox)),
            preferred_repos: std::sync::Mutex::new(preferred_repos),
            budget_pressure: std::sync::Mutex::new(0.0),
            build_test_tracker: std::sync::Mutex::new(build_test::BuildTestTracker::new()),
            memoria_fail_count: std::sync::atomic::AtomicU32::new(0),
            memoria_notified_down: std::sync::atomic::AtomicBool::new(false),
            file_state: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            aggregate_output_bytes: std::sync::atomic::AtomicUsize::new(0),
            url_cache: std::sync::Mutex::new(HashMap::new()),
            passive_cargo_pending: AtomicBool::new(false),
            passive_tsc_pending: AtomicBool::new(false),
            passive_lsp: passive_lsp::PassiveLspManager::new(),
            mcp_manager: None,
            file_journal: std::sync::Arc::new(std::sync::Mutex::new(
                astra_turn_core::file_edit_journal::FileEditJournal::default(),
            )),
            database_snapshot_journal: std::sync::Arc::new(std::sync::Mutex::new(
                mo_tools::DatabaseSnapshotRollbackJournal::default(),
            )),
            git_stash_journal: std::sync::Arc::new(std::sync::Mutex::new(
                git_gix::GitStashRollbackJournal::default(),
            )),
            git_commit_journal: std::sync::Arc::new(std::sync::Mutex::new(
                git_gix::GitCommitRollbackJournal::default(),
            )),
            git_worktree_journal: std::sync::Arc::new(std::sync::Mutex::new(
                worktree::GitWorktreeRollbackJournal::default(),
            )),
            session_state_journal: std::sync::Arc::new(std::sync::Mutex::new(
                session_state::SessionStateRollbackJournal::default(),
            )),
            journal_turn_index: std::sync::atomic::AtomicU32::new(0),
            worktree_session: std::sync::Mutex::new(None),
            task_manager: std::sync::Arc::new(task_mgmt::TaskManager::new()),
            spawn_context: None,
            context_cache: None,
            agent_id: None,
            send_message_context: std::sync::Mutex::new(None),
            observability_session: None,
            active_session_id: std::sync::Mutex::new(None),
            self_mod_pinned_tools: std::sync::Mutex::new(Vec::new()),
            self_mod_deprioritized_tools: std::sync::Mutex::new(Vec::new()),
            session_lessons: std::sync::Mutex::new(Vec::new()),
            latest_skill_diagnosis: std::sync::Mutex::new(None),
            self_mod_mutation_counter: std::sync::Mutex::new((0, 0)),
            default_executor: astra_tools::executor::DefaultToolExecutor::new(
                astra_tools::ToolContext {
                    project_root: root.clone(),
                    workspace_root: root.clone(),
                    user_id: String::new(),
                    session_id: String::new(),
                    sandbox: astra_tools::SandboxConfig::standard(&root),
                    http_client: None,
                    logger: std::sync::Arc::new(astra_tools::TracingLogger),
                    cancel_token: None,
                },
            ),
        }
    }

    /// Set the spawn context for agent spawning.
    pub fn with_spawn_context(mut self, ctx: agent_spawning::SpawnAgentContext) -> Self {
        self.spawn_context = Some(ctx);
        self
    }

    /// Set the shared context cache for cross-agent knowledge sharing.
    pub fn with_context_cache(
        mut self,
        cache: std::sync::Arc<astra_runtime::orchestration::SharedContextCache>,
        agent_id: impl Into<String>,
    ) -> Self {
        self.context_cache = Some(cache);
        self.agent_id = Some(agent_id.into());
        self
    }

    /// Set or clear the messaging context for inter-agent communication.
    pub fn set_send_message_context(
        &self,
        ctx: Option<agent_messaging::SendMessageRuntimeContext>,
    ) {
        if let Ok(mut guard) = self.send_message_context.lock() {
            *guard = ctx;
        }
    }

    /// Set the observability session for context analysis tools.
    pub fn with_observability_session(
        mut self,
        session: std::sync::Arc<
            std::sync::RwLock<astra_runtime::observability_integration::ObservabilitySession>,
        >,
    ) -> Self {
        self.observability_session = Some(session);
        self
    }

    pub fn with_active_session_id(self, session_id: impl Into<String>) -> Self {
        self.set_active_session_id(session_id);
        self
    }

    pub fn set_active_session_id(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        if let Ok(ws) = astra_services::session_workspace::read_workspace(&session_id) {
            if let Ok(mut pinned) = self.self_mod_pinned_tools.lock() {
                *pinned = ws.pinned_tools.clone();
            }
            if let Ok(mut deprioritized) = self.self_mod_deprioritized_tools.lock() {
                *deprioritized = ws.deprioritized_tools.clone();
            }
        }
        if let Ok(mut guard) = self.active_session_id.lock() {
            *guard = Some(session_id);
        }
    }

    pub(crate) fn active_session_id(&self) -> Option<String> {
        self.active_session_id.lock().ok().and_then(|g| g.clone())
    }

    /// P3.1 seam: stash cross-session lessons loaded at session bootstrap.
    /// Every subsequent `build_self_model_snapshot` will project them via
    /// [`astra_runtime::self_model::SelfModel::with_lessons`].
    pub fn set_session_lessons(&self, lessons: Vec<astra_runtime::self_model::LessonHint>) {
        if let Ok(mut g) = self.session_lessons.lock() {
            *g = lessons;
        }
    }

    /// P3.3 seam: stash the latest auto-invoke diagnosis. Pass `None` to
    /// clear a stale diagnosis once the triggering condition resolves.
    /// The next `build_self_model_snapshot` picks it up.
    pub fn set_latest_skill_diagnosis(
        &self,
        diag: Option<astra_skills::auto_invoke::SkillDiagnosis>,
    ) {
        if let Ok(mut g) = self.latest_skill_diagnosis.lock() {
            *g = diag;
        }
    }

    /// Use a shared file edit journal (session-scoped) instead of the default.
    pub fn with_shared_file_journal(
        mut self,
        journal: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        >,
    ) -> Self {
        self.file_journal = journal;
        self
    }

    /// Use a shared file-state cache (session-scoped) so read-before-write
    /// tracking survives across plan executor subtask turns.
    pub fn with_shared_file_state(mut self, state: SharedFileState) -> Self {
        self.file_state = state;
        self
    }

    /// Return a clone of the shared file-state Arc for cross-turn sharing.
    pub fn shared_file_state(&self) -> SharedFileState {
        self.file_state.clone()
    }

    /// Use a shared MatrixOne snapshot journal (session-scoped) instead of the default.
    pub fn with_shared_database_snapshot_journal(
        mut self,
        journal: std::sync::Arc<std::sync::Mutex<mo_tools::DatabaseSnapshotRollbackJournal>>,
    ) -> Self {
        self.database_snapshot_journal = journal;
        self
    }

    /// Use a shared git stash rollback journal (session-scoped) instead of the default.
    pub fn with_shared_git_stash_journal(
        mut self,
        journal: std::sync::Arc<std::sync::Mutex<git_gix::GitStashRollbackJournal>>,
    ) -> Self {
        self.git_stash_journal = journal;
        self
    }

    /// Use a shared git commit rollback journal (session-scoped) instead of the default.
    pub fn with_shared_git_commit_journal(
        mut self,
        journal: std::sync::Arc<std::sync::Mutex<git_gix::GitCommitRollbackJournal>>,
    ) -> Self {
        self.git_commit_journal = journal;
        self
    }

    /// Use a shared git worktree rollback journal (session-scoped) instead of the default.
    pub fn with_shared_git_worktree_journal(
        mut self,
        journal: std::sync::Arc<std::sync::Mutex<worktree::GitWorktreeRollbackJournal>>,
    ) -> Self {
        self.git_worktree_journal = journal;
        self
    }

    /// Use a shared session-state rollback journal (session-scoped) instead of the default.
    pub fn with_shared_session_state_journal(
        mut self,
        journal: std::sync::Arc<std::sync::Mutex<session_state::SessionStateRollbackJournal>>,
    ) -> Self {
        self.session_state_journal = journal;
        self
    }

    /// Use a shared task manager (session-scoped) instead of the default.
    pub fn with_shared_task_manager(
        mut self,
        task_manager: std::sync::Arc<task_mgmt::TaskManager>,
    ) -> Self {
        self.task_manager = task_manager;
        self
    }

    /// Configure cloud proxy for memory tool calls.
    pub fn with_cloud(mut self, base: impl Into<String>, token: impl Into<String>) -> Self {
        self.cloud_base = Some(base.into());
        self.set_cloud_token(token);
        self
    }

    pub(crate) fn set_cloud_token(&self, token: impl Into<String>) {
        *self.cloud_token.write().unwrap_or_else(|e| e.into_inner()) = Some(token.into());
    }

    pub(crate) fn cloud_token(&self) -> Option<String> {
        self.cloud_token
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // ─── Task management methods (delegated to task_mgmt module) ────────────

    async fn task_create(&self, args: &Value) -> String {
        let snapshot = self.task_manager.snapshot_state();
        let output = self.task_manager.create(args).await;
        if !output.starts_with("Error:") {
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task_create:{}",
                    args.get("title").and_then(Value::as_str).unwrap_or("task")
                ),
            );
        }
        output
    }
    async fn task_list(&self, args: &Value) -> String {
        self.task_manager.list(args).await
    }
    async fn task_get(&self, args: &Value) -> String {
        self.task_manager.get(args).await
    }
    async fn task_update(&self, args: &Value) -> String {
        let snapshot = self.task_manager.snapshot_state();
        let output = self.task_manager.update(args).await;
        if !output.starts_with("Error:")
            && serde_json::from_str::<Value>(&output)
                .ok()
                .and_then(|value| value.get("success").and_then(Value::as_bool))
                .unwrap_or(false)
        {
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task_update:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
        }
        output
    }
    async fn task_stop(&self, args: &Value) -> String {
        let snapshot = self.task_manager.snapshot_state();
        let output = self.task_manager.stop(args).await;
        if !output.starts_with("Error:")
            && serde_json::from_str::<Value>(&output)
                .ok()
                .and_then(|value| value.get("success").and_then(Value::as_bool))
                .unwrap_or(false)
        {
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task_stop:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
        }
        output
    }

    /// Sleep for a specified duration without holding a shell process.
    async fn sleep_tool(&self, args: &Value) -> String {
        const MAX_SLEEP_MS: u64 = 300_000; // 5 minutes max

        let duration_ms = match args.get("duration_ms").and_then(Value::as_u64) {
            Some(ms) if ms > 0 => ms.min(MAX_SLEEP_MS),
            Some(_) => return "Error: duration_ms must be positive".to_string(),
            None => return "Error: 'duration_ms' is required".to_string(),
        };

        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("waiting");

        eprintln!(
            "  {}",
            format!("💤 Sleeping for {}ms ({})", duration_ms, reason).dim()
        );

        tokio::time::sleep(std::time::Duration::from_millis(duration_ms)).await;

        serde_json::json!({
            "success": true,
            "slept_ms": duration_ms,
            "reason": reason
        })
        .to_string()
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
    pub fn expand_sandbox_path(&self, dir: PathBuf) {
        if let Ok(mut guard) = self.sandbox_policy.write() {
            if let Some(ref mut policy) = *guard {
                policy.allowed_paths.push(dir);
            }
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

    /// Output limit scaled by budget pressure and aggregate output.
    ///
    /// Two independent pressures are combined:
    /// 1. **Token pressure** (from context window fill ratio): 0.0→full, 0.9→25%.
    /// 2. **Aggregate output pressure** (from cumulative tool output this turn):
    ///    smooth curve that progressively tightens as output accumulates,
    ///    reaching 25% of base at 2× the aggregate budget.
    fn scaled_output_limit(&self) -> usize {
        self.scaled_output_limit_for("")
    }

    /// Per-tool variant: applies per-tool cap as the base *before* pressure
    /// scaling. This makes per-tool caps absolute upper bounds — even at zero
    /// pressure, grep can never exceed 10KB. Previous inline `.min(N)` calls
    /// applied after scaling, so caps only kicked in when `scaled > N`.
    fn scaled_output_limit_for(&self, tool_name: &str) -> usize {
        let base = per_tool_output_limit(tool_name);
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
    fn finalize_tool_output(&self, output: String, name: &str) -> String {
        let output = normalize_empty_output(output, name);
        let output = truncate_output(output, global_output_limit());
        self.maybe_persist_large_output(output, name)
    }

    pub async fn execute_with_metadata(&self, name: &str, args: &Value) -> ToolExecutionOutcome {
        if name == "mo_query" {
            let mut outcome = self.mo_query_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        if name == "git_stash" {
            let mut outcome = self.git_stash_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        if name == "git_commit" {
            let mut outcome = self.git_commit_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        if name == "git_revert_commit" {
            let mut outcome = self.git_revert_commit_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        if name == "git_worktree" {
            let mut outcome = self.git_worktree_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }

        ToolExecutionOutcome::text(self.execute(name, args).await)
    }

    pub async fn execute(&self, name: &str, args: &Value) -> String {
        let output = if let Err(error) =
            crate::tool_safety_guard::ToolSafetyGuard::check_dispatch(name, args)
        {
            error
        } else {
            match name {
                "bash" => self.bash(args),
                "powershell" => self.powershell(args),
                "read_file" => self.read_file(args),
                "write_file" => self.write_file(args),
                "rollback_file_edits" => self.rollback_file_edits(args),
                "rollback_database_snapshots" => self.rollback_database_snapshots(args),
                "rollback_session_state" => self.rollback_session_state(args),
                "rollback_turn_actions" => self.rollback_turn_actions(args),
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
                "git_commit" => self.git_commit(args),
                "git_revert_commit" => self.git_revert_commit(args),
                "git_stash" => self.git_stash(args),
                "git_checkout_file" => self.git_checkout_file(args),
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
                "adjust_config" => self.adjust_config(args),
                "prioritize_tool" => self.prioritize_tool(args),
                "deprioritize_tool" => self.deprioritize_tool(args),
                "set_goal" => self.set_goal(args),
                "compress_context" => self.compress_context(args),
                "get_agent_info" => self.get_agent_info(args).await,
                "reflect" => {
                    let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
                    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
                    if let Some(session_id) = self.active_session_id().filter(|id| !id.is_empty()) {
                        let limit = usize::try_from(last_n.max(1)).unwrap_or(20);
                        match crate::self_command::render_reflect_surface_for_session(
                            &session_id,
                            limit,
                            Some(focus),
                            Some(question),
                        )
                        .await
                        {
                            Ok(surface) => surface,
                            Err(error) => serde_json::json!({
                                "status": "reflect_unavailable",
                                "focus": focus,
                                "question": question,
                                "last_n": last_n,
                                "error": error,
                            })
                            .to_string(),
                        }
                    } else {
                        serde_json::json!({
                        "status": "reflect_requires_session",
                        "focus": focus,
                        "question": question,
                        "last_n": last_n,
                        "note": "Reflect data comes from the server API. Use /reflect command for direct access."
                    }).to_string()
                    }
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
                "send_message" => {
                    let ctx = self
                        .send_message_context
                        .lock()
                        .ok()
                        .and_then(|g| g.clone());
                    agent_messaging::handle_send_message_tool(args, ctx.as_ref()).await
                }
                "spawn_agent" => {
                    agent_spawning::handle_spawn_agent_tool(args, self.spawn_context.as_ref()).await
                }
                "get_agent_result" => {
                    agent_spawning::handle_get_agent_result_tool(args, self.spawn_context.as_ref())
                        .await
                }
                "share_context" => self.share_context(args),
                "query_context" => self.query_context(args),
                astra_runtime::turn::agentic_loop_host::DELEGATE_TOOL_NAME => {
                    "Delegation request acknowledged. The delegation engine will execute \
                this request and provide results in the next round."
                        .to_string()
                }
                "diagnose" => self.diagnose(args).await,
                "lsp" => self.lsp(args),
                "env" => self.env_tool(args),
                "notebook_edit" => self.notebook_edit(args),
                "config" => self.config_tool(args),
                "brief" => self.brief(args),
                "context_analysis" => self.context_analysis(args),
                _ if name.starts_with("mcp_") => self.execute_mcp_tool(name, args).await,
                _ => {
                    // Delegate unknown tools to the shared DefaultToolExecutor.
                    use astra_tools::ToolExecutor as _;
                    self.default_executor.execute(name, args).await.output
                }
            }
        };
        // Normalize empty output, then apply global safety net
        let output = self.finalize_tool_output(output, name);
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
    /// Large results are persisted to a temp file and replaced with a
    /// ~2KB preview + file path.
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

    /// Execute a multi-step ToolChain, forwarding each step to self.execute().
    ///
    /// Returns a JSON summary with per-step outputs and the final result.
    /// Execution stops on the first error unless the step has a skip condition.
    pub fn execute_chain(
        &self,
        chain: &astra_runtime::tool_registry::ToolChain,
        input: Value,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + '_>> {
        use astra_turn_core::tool_registry_chain::{ChainContext, resolve_args};

        if let Err(error) = crate::tool_safety_guard::ToolSafetyGuard::check_chain(chain) {
            let chain_name = chain.name.clone();
            let steps_total = chain.steps.len();
            return Box::pin(async move {
                serde_json::json!({
                    "chain": chain_name,
                    "steps_executed": 0,
                    "steps_total": steps_total,
                    "final_output": error,
                    "steps": [],
                })
                .to_string()
            });
        }

        let chain_name = chain.name.clone();
        let rollback_on_failure = chain.rollback_on_failure;
        let rollback_turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Acquire);
        let file_checkpoint = rollback_on_failure.then(|| self.file_journal_checkpoint());
        let database_checkpoint =
            rollback_on_failure.then(|| self.database_snapshot_journal_checkpoint());
        let stash_checkpoint = rollback_on_failure.then(|| self.git_stash_journal_checkpoint());
        let commit_checkpoint = rollback_on_failure.then(|| self.git_commit_journal_checkpoint());
        let worktree_checkpoint =
            rollback_on_failure.then(|| self.git_worktree_journal_checkpoint());
        let session_state_checkpoint =
            rollback_on_failure.then(|| self.session_state_journal_checkpoint());
        let steps = chain.steps.clone();

        Box::pin(async move {
            let mut ctx = ChainContext::new(input);
            let mut step_results = Vec::new();
            let mut rollback = None;

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
                    if rollback_on_failure {
                        if let (
                            Some(file_checkpoint),
                            Some(database_checkpoint),
                            Some(stash_checkpoint),
                            Some(commit_checkpoint),
                            Some(worktree_checkpoint),
                            Some(session_state_checkpoint),
                        ) = (
                            file_checkpoint,
                            database_checkpoint,
                            stash_checkpoint,
                            commit_checkpoint,
                            worktree_checkpoint,
                            session_state_checkpoint,
                        ) {
                            let file_entries_added = self
                                .file_journal_checkpoint()
                                .saturating_sub(file_checkpoint);
                            let database_entries_added = self
                                .database_snapshot_journal_checkpoint()
                                .saturating_sub(database_checkpoint);
                            let stash_entries_added = self
                                .git_stash_journal_checkpoint()
                                .saturating_sub(stash_checkpoint);
                            let commit_entries_added = self
                                .git_commit_journal_checkpoint()
                                .saturating_sub(commit_checkpoint);
                            let worktree_entries_added = self
                                .git_worktree_journal_checkpoint()
                                .saturating_sub(worktree_checkpoint);
                            let session_state_entries_added = self
                                .session_state_journal_checkpoint()
                                .saturating_sub(session_state_checkpoint);
                            if file_entries_added > 0
                                || database_entries_added > 0
                                || stash_entries_added > 0
                                || commit_entries_added > 0
                                || worktree_entries_added > 0
                                || session_state_entries_added > 0
                            {
                                let rollback_output =
                                    self.rollback_turn_actions(&serde_json::json!({
                                        "scope": "turn",
                                        "turn_index": rollback_turn_index,
                                        "file_after_sequence": file_checkpoint,
                                        "database_after_sequence": database_checkpoint,
                                        "stash_after_sequence": stash_checkpoint,
                                        "commit_after_sequence": commit_checkpoint,
                                        "worktree_after_sequence": worktree_checkpoint,
                                        "session_state_after_sequence": session_state_checkpoint,
                                    }));
                                rollback = Some(
                                    serde_json::from_str(&rollback_output).unwrap_or_else(
                                        |error| {
                                            serde_json::json!({
                                                "success": false,
                                                "error": format!("invalid rollback_turn_actions output: {error}"),
                                                "raw_output": rollback_output,
                                            })
                                        },
                                    ),
                                );
                            }
                        }
                    }
                    break;
                }
            }

            let mut result = serde_json::Map::from_iter([
                ("chain".to_string(), serde_json::json!(chain_name)),
                (
                    "steps_executed".to_string(),
                    serde_json::json!(step_results.len()),
                ),
                ("steps_total".to_string(), serde_json::json!(steps.len())),
                (
                    "final_output".to_string(),
                    serde_json::json!(truncate_output(ctx.prev_output, 8192)),
                ),
                ("steps".to_string(), serde_json::json!(step_results)),
            ]);
            if let Some(rollback) = rollback {
                result.insert("rollback".to_string(), rollback);
            }
            Value::Object(result).to_string()
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

    async fn get_agent_info(&self, args: &Value) -> String {
        let dimension = args
            .get("dimension")
            .and_then(|v| v.as_str())
            .unwrap_or("all");

        if let Some(session_id) = self.active_session_id()
            && let Some(surface) = crate::self_command::agent_info_surface_alias(dimension)
        {
            return crate::self_command::render_surface_for_session(&session_id, surface, 20)
                .await
                .unwrap_or_else(|error| serde_json::json!({ "error": error }).to_string());
        }

        let self_model = self.build_self_model_snapshot();
        match dimension {
            "capability" => {
                if let Some(ref model) = self_model {
                    serde_json::json!({
                        "tools": model.capabilities.tool_names,
                        "tool_count": model.capabilities.total_tools,
                        "deprioritized_tools": model.capabilities.deprioritized_tools,
                        "skills": model.capabilities.skills,
                        "pinned_tools": model.capabilities.pinned_tools,
                        "tool_health": model.capabilities.tool_health.iter().map(|t| {
                            serde_json::json!({
                                "name": t.name,
                                "total_calls": t.total_calls,
                                "success_rate": t.success_rate,
                                "deprioritized": t.deprioritized,
                            })
                        }).collect::<Vec<_>>(),
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "tools": self.tool_names(),
                        "tool_count": self.tool_count(),
                    })
                    .to_string()
                }
            }
            "state" => {
                if let Some(ref model) = self_model {
                    serde_json::json!({
                        "turn": model.state.turn_number,
                        "token_budget": model.state.token_budget,
                        "scenario": model.state.scenario,
                        "active_experiment": model.state.active_experiment,
                        "session_elapsed_secs": model.state.session_elapsed_secs,
                        "correction_count": model.state.correction_count,
                        "compression_count": model.state.compression_count,
                        "recent_signals": model.recent_signals,
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "note": "No observability session available."
                    })
                    .to_string()
                }
            }
            "goals" => {
                if let Some(ref model) = self_model {
                    serde_json::json!({
                        "goal": model.goals.goal,
                        "session_goal": model.goals.session_goal,
                        "plan_goal": model.goals.plan_goal,
                        "tracked_goal": model.goals.tracked_goal,
                        "goal_source": model.goals.goal_source,
                        "tracking_status": model.goals.tracking_status,
                        "progress": model.goals.progress,
                        "recent_milestones": model.goals.recent_milestones,
                        "milestone_count": model.goals.milestone_count,
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "note": "No goal tracker available."
                    })
                    .to_string()
                }
            }
            "context_snapshot" | "context_trend" => {
                if let Some(ref model) = self_model {
                    serde_json::json!({
                        "token_budget": model.state.token_budget,
                        "compression_count": model.state.compression_count,
                    })
                    .to_string()
                } else {
                    serde_json::json!({
                        "note": "Context window data not available. Use /explain for token breakdown."
                    })
                    .to_string()
                }
            }
            "identity" => serde_json::json!({
                "name": "astra",
                "version": env!("CARGO_PKG_VERSION"),
                "runtime": "Rust edge CLI",
            })
            .to_string(),
            _ => {
                if let Some(ref model) = self_model {
                    model.to_detailed_text()
                } else {
                    serde_json::json!({
                        "tools_available": self.tool_names(),
                        "tool_count": self.tool_count(),
                        "runtime": "astra Rust CLI",
                        "version": env!("CARGO_PKG_VERSION"),
                    })
                    .to_string()
                }
            }
        }
    }

    /// Build a SelfModel snapshot from available observability session data.
    pub fn build_self_model_snapshot(&self) -> Option<astra_runtime::self_model::SelfModel> {
        let obs_session = self.observability_session.as_ref()?;
        let session = obs_session.read().ok()?;

        let tool_name_strs = self.tool_names();
        let tool_name_refs: Vec<&str> = tool_name_strs.iter().map(|s| s.as_str()).collect();
        let pinned_tools = self
            .self_mod_pinned_tools
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();
        let deprioritized_tools = self
            .self_mod_deprioritized_tools
            .lock()
            .map(|v| v.clone())
            .unwrap_or_default();

        let elapsed = session.started_at.elapsed().as_secs();

        // Latest token budget from context traces.
        let latest_budget = session.context_traces.last().map(|ct| &ct.token_budget);

        // Goal information.
        let goal_text = session.goal_tracker.as_ref().map(|gt| gt.goal());
        let goal_progress = session.goal_tracker.as_ref().map(|gt| gt.progress());
        let milestones: Option<Vec<_>> = session
            .goal_tracker
            .as_ref()
            .map(|gt| gt.milestones().to_vec());
        let milestone_slice = milestones.as_deref();

        let skills_slice: &[String] = &session.cached_skill_names;
        let tool_health_tracker = if session.last_tool_health_export.is_empty() {
            None
        } else {
            Some(
                astra_turn_core::tool_health::ToolHealthTracker::from_entries(
                    &session.last_tool_health_export,
                ),
            )
        };
        let scenario_opt = session.active_scenario.as_ref();
        let signals_slice: &[_] = &session.last_feedback_signals;

        let mut snapshot = astra_runtime::self_model::SelfModel::snapshot_with_strategy(
            &tool_name_refs,
            &pinned_tools,
            &deprioritized_tools,
            skills_slice,
            tool_health_tracker.as_ref(),
            session.turn_number,
            latest_budget,
            scenario_opt,
            None,
            elapsed,
            session.user_corrections.len(),
            session.compressed_turns.len(),
            goal_text,
            None,
            goal_text,
            goal_progress.as_ref(),
            milestone_slice,
            signals_slice,
            &session.config,
            session.last_strategy_application.as_ref(),
        );
        if let Some(g) = session.last_guardrail_view.clone() {
            snapshot = snapshot.with_guardrail(g);
        }
        if let Some(dp) = session.last_denial_pressure {
            snapshot = snapshot.with_denial_pressure(dp);
        }
        if !session.recent_failing_tests.is_empty() {
            snapshot = snapshot.with_recent_failing_tests(session.recent_failing_tests.clone());
        }
        if !session.recent_rejections.is_empty() {
            snapshot = snapshot.with_recent_rejections(session.recent_rejections.clone());
        }
        if !session.recent_correction_excerpts.is_empty() {
            snapshot = snapshot
                .with_recent_correction_excerpts(session.recent_correction_excerpts.clone());
        }
        if !session.outcome_bias.is_empty() {
            snapshot = snapshot.with_outcome_bias(session.outcome_bias.clone());
        }
        if !session.low_confidence_tools.is_empty() {
            snapshot = snapshot.with_low_confidence_tools(
                session
                    .low_confidence_tools
                    .iter()
                    .cloned()
                    .map(|(name, fail_rate, samples)| {
                        astra_runtime::self_model::LowConfidenceTool {
                            name,
                            fail_rate,
                            samples,
                        }
                    })
                    .collect(),
            );
        }

        // P3.1 seam: attach cross-session lessons if bootstrap cached any.
        if let Ok(lessons) = self.session_lessons.lock()
            && !lessons.is_empty()
        {
            snapshot = snapshot.with_lessons(lessons.clone());
        }

        // P3.3 seam: attach the latest auto-invoke diagnosis (if any).
        // `with_skill_diagnosis(None)` is a no-op — we only call it when
        // something is stashed.
        if let Ok(diag_guard) = self.latest_skill_diagnosis.lock()
            && let Some(ref diag) = *diag_guard
        {
            snapshot = snapshot.with_skill_diagnosis(Some(diag.clone()));
        }

        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    pub(super) fn test_executor() -> ToolExecutor {
        ToolExecutor::new(std::env::temp_dir())
    }

    /// Create a ToolExecutor rooted in a fresh temp directory.
    /// Returns both the TempDir (to keep it alive) and the executor.
    pub(super) fn temp_executor() -> (tempfile::TempDir, ToolExecutor) {
        let dir = tempfile::tempdir().unwrap();
        let executor = ToolExecutor::new(dir.path());
        (dir, executor)
    }

    #[test]
    fn cloud_token_updates_after_cloud_configuration() {
        let executor = test_executor().with_cloud("https://cloud.example", "old-token");
        assert_eq!(executor.cloud_token().as_deref(), Some("old-token"));

        executor.set_cloud_token("new-token");
        assert_eq!(executor.cloud_token().as_deref(), Some("new-token"));
    }

    mod aggregate_tests;
    mod build_test_tests;
    mod chain_tests;
    mod code_intel_api_tests;
    mod code_intel_enhancement_tests;
    mod code_intel_integration_tests;
    mod code_intel_tests;
    mod config_tests;
    mod context_analysis_tests;
    mod cross_file_caller_tests;
    mod diagnose_tests;
    mod env_tests;
    mod executor_core_tests;
    mod fs_tests;
    mod lsp_tests;
    mod memoria_tests;
    mod notebook_tests;
    mod sandbox_tests;
    mod schema_tests;
    mod self_mod_tests;
    mod task_tests;
    mod tool_search_tests;
    mod utf16_tests;
    mod web_search_tests;
    mod worktree_tests;

    /// Regression test for 3e3d6fa8 proxy policy:
    /// `github_client` targets api.github.com (external traffic), so it must
    /// honour HTTPS_PROXY/ALL_PROXY via `astra_core::net::apply_env_proxy`.
    /// Before the fix, this builder silently inherited reqwest's default env
    /// handling without NO_PROXY / socks5 / tracing parity with the LLM client.
    ///
    /// We can't introspect reqwest's internal proxy config, so we assert the
    /// observable contract: (a) the builder constructs successfully under a
    /// variety of proxy envs (NO_PROXY, malformed, socks5 via ALL_PROXY), and
    /// (b) `ToolExecutor::new` never panics when those envs are set — which
    /// was the actual risk if a caller forgot to call `apply_env_proxy` and
    /// reqwest rejected a malformed env URL at build time.
    #[test]
    fn github_client_honours_proxy_env_without_panicking() {
        let dir = tempfile::tempdir().unwrap();

        // valid https proxy
        temp_env::with_var("HTTPS_PROXY", Some("http://proxy.example:8080"), || {
            let _ = ToolExecutor::new(dir.path());
        });
        // socks5 via ALL_PROXY (parity with LLM client regression)
        temp_env::with_var("ALL_PROXY", Some("socks5://127.0.0.1:1080"), || {
            let _ = ToolExecutor::new(dir.path());
        });
        // malformed must not panic (apply_env_proxy swallows parse errors)
        temp_env::with_var("HTTPS_PROXY", Some("not a url"), || {
            let _ = ToolExecutor::new(dir.path());
        });
        // NO_PROXY honoured
        temp_env::with_vars(
            [
                ("HTTPS_PROXY", Some("http://proxy.example:8080")),
                ("NO_PROXY", Some("api.github.com")),
            ],
            || {
                let _ = ToolExecutor::new(dir.path());
            },
        );
    }
}
