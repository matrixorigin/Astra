//! Edge tool definitions and execution for the astra CLI.
//!
//! Tools: bash, read_file (with outline mode), write_file, str_replace (with fuzzy matching),
//!        list_dir, grep (with context_lines/max_matches), glob,
//!        git(action=...), github(action=...), web_fetch,
//!        mo_query, mo_snapshot, mo_branch

use std::{
    collections::{HashMap, HashSet},
    env,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
    time::Duration,
};

use astra_runtime::tool_sandbox::{
    SandboxPolicy, sandbox_command, validate_path, wrap_command_with_limits,
};
use astra_turn_core::sync_utils::{rwlock_read_clone_or_default, rwlock_write_reset_on_poison};
use astra_turn_core::tool::deferred_activation::ToolSurfaceNames;

/// Prefix returned by tool execution when the sandbox blocks a path.
/// The agentic loop / permission manager can detect this to prompt the user
/// for authorization instead of letting the model silently fall back to bash.
pub const SANDBOX_DENIED_PREFIX: &str = "SANDBOX_DENIED: ";

/// Error returned by [`ToolExecutor::expand_sandbox_path`] when a path is
/// rejected by the validation gate.
///
/// Every variant maps to a concrete, auditable rejection reason so callers
/// (CLI flag parsing, TUI approval flow) can surface a precise message
/// instead of a generic "denied".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxExpansionError {
    /// The path was not absolute. Only absolute, concrete paths are expandable.
    #[error("sandbox expansion requires an absolute path")]
    NotAbsolute,
    /// The path is the filesystem root `/`, which would open the entire
    /// filesystem to the sandbox — always rejected.
    #[error("sandbox expansion cannot open the filesystem root")]
    RootPath,
    /// The path contains a `..` component that escapes the supplied directory.
    #[error("sandbox expansion rejects parent-dir traversal escapes")]
    TraversalEscape,
    /// The path is a system-sensitive directory or credential store whose
    /// children include secrets (e.g. `/etc`, `~/.ssh`, `/var/run/secrets`).
    #[error("sandbox expansion rejects system-sensitive path")]
    SystemSensitivePath,
    /// The sandbox policy lock is poisoned. The sandbox cannot be mutated
    /// safely — the request must be rejected rather than silently dropped.
    #[error("sandbox policy lock is poisoned; cannot expand safely")]
    PolicyLockPoisoned,
    /// No sandbox policy is installed. The caller asked to expand a path
    /// into a sandbox that doesn't exist (e.g. headless path before init).
    /// Returning `Ok` here would be a silent no-op: the user believes
    /// `--add-dir` took effect, but the sandbox was never updated.
    #[error("no sandbox policy installed; cannot expand path")]
    NoSandboxPolicy,
}

use crossterm::style::Stylize;
use reqwest::Client;
use serde_json::{Value, json};

#[path = "edge_tools/agent_messaging.rs"]
pub mod agent_messaging;
#[path = "edge_tools/agent_spawning.rs"]
pub mod agent_spawning;
use astra_tools::build_test;
pub use astra_tools::code_intel;
use astra_tools::truncate_output;
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
mod shell;
use astra_tools::env_tools;
use astra_tools::schemas::all_tool_schemas as full_tool_schemas;
use astra_turn_core::cloud_approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind_with_args};
pub use env_tools::apply_overlay as apply_env_overlay;
#[path = "edge_tools/code_analysis.rs"]
mod code_analysis;
#[path = "edge_tools/config_tool.rs"]
mod config_tool;
#[path = "edge_tools/context_tools.rs"]
mod context_tools;

pub fn all_tool_schemas() -> Vec<Value> {
    full_tool_schemas()
}

const CLI_LOCAL_EXECUTOR_TOOL_NAMES: &[&str] = &[
    "adjust_config",
    "agent",
    "agent_fanout",
    "ask_user",
    "brief",
    "call_graph",
    "compress_context",
    "config",
    "context_analysis",
    "dead_code",
    "delegate",
    "deprioritize_tool",
    "diagnose",
    "enter_plan_mode",
    "env",
    "exit_plan_mode",
    "extract_members",
    "find_definition",
    "find_references",
    "get_agent_info",
    "hover_info",
    "introspect",
    "lsp",
    "mo_branch",
    "mo_query",
    "mo_snapshot",
    "notebook_edit",
    "notify",
    "prioritize_tool",
    "query_context",
    "reflect",
    "rename_symbol",
    "rollback_database_snapshots",
    "rollback_session_state",
    "rollback_turn_actions",
    "run_build_test",
    "session",
    "share_context",
    "symbol_search",
    "task",
    "task_list",
    "task_output",
    "task_stop",
    "type_hierarchy",
    "web_search",
];

fn local_runtime_tool_schemas(raw_schemas: Vec<Value>) -> Vec<Value> {
    let registry = astra_runtime_env::ToolRegistry::builtins();
    let binding = astra_runtime_env::RunBinding::local_developer(".", &registry);
    astra_runtime_env::CapabilityResolver.filter_tool_schemas(
        &registry,
        raw_schemas,
        &binding.capabilities,
    )
}

/// Construct the CLI's session-wide `CapabilitySet`.
pub fn cli_default_capabilities(
    has_agent_spawner: bool,
    has_local_background_tasks: bool,
) -> astra_turn_core::capability::CapabilitySet {
    use astra_turn_core::capability::{Capability, CapabilitySet};
    CapabilitySet::empty()
        .with(Capability::MemoryService)
        .with(Capability::Database)
        .with(Capability::GitHubAuth)
        .with(Capability::LSPServer)
        .with(Capability::SkillsCatalog)
        .with(Capability::PlanLifecycle)
        .with_if(has_local_background_tasks, Capability::LocalBackgroundTasks)
        .with_if(has_agent_spawner, Capability::AgentSpawner)
}

struct CliCapabilityView {
    active_names: Vec<String>,
    inactive_names: Vec<String>,
    visible_names: Vec<String>,
    dropped_by_capability: Vec<Value>,
    dropped_by_surface: Vec<String>,
    mcp_pass_through: Vec<String>,
}

pub fn local_tool_schemas() -> Vec<Value> {
    local_runtime_tool_schemas(full_tool_schemas())
}

/// Plan-mode write guard tool list (CLI parity with
/// `server_tool_executor::is_plan_mode_blocked_tool`). While a plan is
/// in `phase=planning` these tools must be short-circuited: they all
/// mutate the world (filesystem, DB, git, GitHub), so allowing them
/// would let the model execute a plan it has not yet had approved.
/// Read-only tools (read_file, grep, glob, git(action=status/diff/log)) and
/// session-scoped authoring tools (`task`, memory_*) stay available so the
/// agent can keep authoring without mutating the external world.
pub(crate) fn is_plan_mode_blocked_tool(tool: &str, args: &Value) -> bool {
    // Legacy standalone tools are always blocked
    if tool == "task_stop" {
        return true;
    }

    // Consolidated `task` tool: block only destructive actions (stop)
    if tool == "task" {
        let action = args.get("action").and_then(|v| v.as_str()).unwrap_or("");
        return action == "stop";
    }

    if matches!(
        cloud_gated_tool_kind_with_args(tool, Some(args)),
        Some(CloudGatedToolKind::Write | CloudGatedToolKind::Execute)
    ) {
        return true;
    }

    matches!(
        tool,
        "bash" | "write_file" | "str_replace" | "mo" | "rollback_database_snapshots"
    )
}

fn git_stash_action_args(args: &Value) -> Value {
    let stash_action = args
        .get("sub_action")
        .or_else(|| args.get("stash_action"))
        .and_then(Value::as_str);
    let Some(stash_action) = stash_action else {
        return args.clone();
    };

    let mut map = args.as_object().cloned().unwrap_or_default();
    map.insert(
        "action".to_string(),
        Value::String(stash_action.to_string()),
    );
    Value::Object(map)
}

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
pub(crate) use file_state::{ReadCoverage, ReadDedupKey};

/// Shared file-state cache handle for cross-turn read-before-write tracking.
pub(crate) type SharedFileState = std::sync::Arc<std::sync::Mutex<HashMap<PathBuf, FileState>>>;

/// Per-session directory for persisted file-edit checkpoints.
/// Default location: `~/.astra/sessions/<session_id>/file_checkpoints/`.
///
/// Test-only override: `_ASTRA_FILE_CHECKPOINT_ROOT` (underscore-prefix
/// = internal; **not** a supported production configuration knob). When
/// set, checkpoints live at `$_ASTRA_FILE_CHECKPOINT_ROOT/<session_id>/
/// file_checkpoints/`. Used by `#[serial]` tests to redirect writes away
/// from the developer's real `~/.astra/`.
///
/// Returns `None` when neither override nor `HOME` is available, or the
/// session_id is empty / whitespace-only — in those cases persistence is
/// silently disabled and the journal runs in-memory only.
///
/// **Single-writer invariant**: two CLI instances against the same
/// `session_id` will share this directory and destructively prune each
/// other's entries (see `FileEditJournal::save_to_dir` docstring).
/// This is a known limitation. Callers with multiple concurrent
/// executors for the same session SHOULD share one `FileEditJournal`
/// via `with_shared_file_journal` — that's how the sse_loop path wires
/// it in production, and the in-process Mutex then serializes writes.
/// Cross-process (two separate CLI invocations) has no enforcement;
/// either avoid the setup or accept the data loss.
fn file_checkpoint_dir_for(session_id: &str) -> Option<PathBuf> {
    if session_id.trim().is_empty() {
        return None;
    }
    // Test-only override takes precedence so tests don't pollute the real HOME.
    if let Ok(root) = std::env::var("_ASTRA_FILE_CHECKPOINT_ROOT") {
        if !root.is_empty() {
            return Some(
                PathBuf::from(root)
                    .join(session_id)
                    .join("file_checkpoints"),
            );
        }
    }
    let store = astra_services::local_session_artifact_store();
    astra_services::SessionArtifactStore::session_path(&store, session_id, "file_checkpoints").ok()
}
#[path = "edge_tools/worktree.rs"]
mod worktree;
use crate::lock_recovery::LockRecovery;
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
pub(crate) use ask_user::parse_ask_user_prompt;
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
/// Tools that produce large output (read_file, git(action=show)) will check this
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

fn cli_tool_output_is_error(output: &str) -> bool {
    astra_turn_core::tool_result_semantics::classify_tool_result_status(output)
        == astra_turn_core::tool_result_semantics::ToolResultStatus::Failed
}

pub(crate) use astra_tools::git_gix::ToolExecutionOutcome;

fn sandbox_denied_outcome_from_output(output: &str) -> Option<ToolExecutionOutcome> {
    let message = crate::sandbox_retry::sandbox_denied_message(output)?.into_owned();
    Some(ToolExecutionOutcome {
        output: format!("Error: {message}"),
        tool_result_fields: Some(crate::sandbox_retry::sandbox_denied_tool_result_fields(
            &message,
        )),
        is_error: true,
    })
}

fn tool_execution_outcome_from_output(output: String) -> ToolExecutionOutcome {
    if let Some(outcome) = sandbox_denied_outcome_from_output(&output) {
        return outcome;
    }
    if cli_tool_output_is_error(&output) {
        ToolExecutionOutcome::error(output)
    } else {
        ToolExecutionOutcome::ok(output)
    }
}

struct EdgeToolRun {
    output: String,
    error_kind: Option<astra_core::ErrorKind>,
}

impl EdgeToolRun {
    fn ok(output: String) -> Self {
        Self {
            output,
            error_kind: None,
        }
    }

    fn error(output: String) -> Self {
        Self {
            output,
            error_kind: None,
        }
    }

    fn classified_error(output: String, kind: astra_core::ErrorKind) -> Self {
        Self {
            output,
            error_kind: Some(kind),
        }
    }

    fn into_outcome(self) -> ToolExecutionOutcome {
        if let Some(outcome) = sandbox_denied_outcome_from_output(&self.output) {
            return outcome;
        }

        let mut outcome = if self.error_kind.is_some() || cli_tool_output_is_error(&self.output) {
            ToolExecutionOutcome::error(self.output)
        } else {
            ToolExecutionOutcome::ok(self.output)
        };
        if let Some(kind) = self.error_kind {
            let metadata = outcome
                .tool_result_fields
                .get_or_insert_with(serde_json::Map::new);
            metadata.insert(
                "error_kind".to_string(),
                Value::String(kind.as_str().to_string()),
            );
        }
        outcome
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
/// One-line label for an extraction outcome. Matches the
/// [`ExtractionOutcome`] variants but renders terse strings suitable
/// for a bullet list in the introspect output.
fn render_extraction_outcome_label(
    outcome: &astra_runtime::session_memory::observatory::ExtractionOutcome,
) -> String {
    use astra_runtime::session_memory::observatory::ExtractionOutcome;
    match outcome {
        ExtractionOutcome::Persisted {
            source,
            bytes_written,
            store_attempt,
        } => format!("persisted({source:?},bytes={bytes_written},attempt={store_attempt})"),
        ExtractionOutcome::LlmFailedFallbackPersisted {
            reason,
            bytes_written,
            store_attempt,
        } => {
            format!("llm_failed_fallback({reason:?},bytes={bytes_written},attempt={store_attempt})")
        }
        ExtractionOutcome::PersistFailed { reason, llm_reason } => {
            if let Some(llm_reason) = llm_reason {
                format!("persist_failed({reason:?},llm={llm_reason:?})")
            } else {
                format!("persist_failed({reason:?})")
            }
        }
        ExtractionOutcome::Skipped { reason } => format!("skipped({reason})"),
    }
}

/// Compact staleness display: `-` when clean, else a slash-separated
/// list of flags that fired. Keeps the injection line short.
fn render_staleness(s: &astra_runtime::session_memory::observatory::StalenessSignals) -> String {
    let mut tags = Vec::new();
    if s.task_contradicted {
        tags.push("task_contradicted");
    }
    if s.missing_corrections {
        tags.push("missing_corrections");
    }
    if tags.is_empty() {
        "-".to_string()
    } else {
        tags.join("/")
    }
}

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

/// Commands queued by the tool executor for the TUI's BackgroundTaskRegistry.
#[derive(Debug, Clone)]
pub struct BgTaskOutputSnapshot {
    pub kind: String,
    pub title: Option<String>,
    pub output: String,
    pub end_offset: u64,
    pub total_bytes: u64,
    pub total_lines: u64,
    pub status: String,
    pub terminal: bool,
    pub output_ref: String,
}

pub enum BgTaskCommand {
    Kill {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    GetOutputSince {
        task_id: String,
        offset: u64,
        max_bytes: usize,
        reply: tokio::sync::oneshot::Sender<Result<BgTaskOutputSnapshot, String>>,
    },
    List {
        reply: tokio::sync::oneshot::Sender<String>,
    },
}

#[cfg(not(test))]
const BG_TASK_COMMAND_REPLY_TIMEOUT_MS: u64 = 1_000;
#[cfg(test)]
const BG_TASK_COMMAND_REPLY_TIMEOUT_MS: u64 = 25;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgTaskReplyError {
    Closed,
    TimedOut,
}

fn background_task_reply_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.clamp(1, BG_TASK_COMMAND_REPLY_TIMEOUT_MS))
}

async fn await_bg_task_command_reply<T>(
    rx: tokio::sync::oneshot::Receiver<T>,
    timeout: Duration,
) -> Result<T, BgTaskReplyError> {
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(reply)) => Ok(reply),
        Ok(Err(_)) => Err(BgTaskReplyError::Closed),
        Err(_) => Err(BgTaskReplyError::TimedOut),
    }
}

fn duration_ms_u64(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

fn format_background_task_error(task_id: &str, error: &str) -> String {
    if error.contains("no background shell with id") || error.contains("no background task with id")
    {
        format!("Background task not found: {task_id}")
    } else if let Some(detail) = error.strip_prefix("output artifact missing:") {
        format!(
            "Read shell output {task_id}\nOutput artifact missing ·{}",
            detail
        )
    } else {
        format!("Background task output unavailable: {error}")
    }
}

fn format_background_task_stop_error(task_id: &str, error: &str) -> String {
    if error.contains("no background shell with id") || error.contains("no background task with id")
    {
        format!("Background task not found: {task_id}")
    } else if error.contains("already terminated") || error.contains("already finished") {
        format!("Background task {task_id} already finished.")
    } else if error.contains("stale handle") {
        format!(
            "Background task {task_id} cannot be stopped because it was restored from a previous session and no live process handle is available."
        )
    } else {
        format!("Background task stop failed: {error}")
    }
}

fn format_background_task_output(
    task_id: &str,
    offset: u64,
    snapshot: &BgTaskOutputSnapshot,
) -> String {
    let kind = snapshot.kind.trim();
    let kind = if kind.is_empty() { "shell" } else { kind };
    let header = match kind {
        "shell" => format!("Read shell output {task_id}"),
        "local agent" => format!("Read local agent output {task_id}"),
        "cloud session" => format!("Read cloud session output {task_id}"),
        "main session" => format!("Read main session output {task_id}"),
        "monitor" => format!("Read monitor output {task_id}"),
        other => format!("Read {other} output {task_id}"),
    };
    let status_label = match snapshot.status.as_str() {
        "pending" => "pending",
        "running" => "still running",
        "waiting_for_input" => "needs input",
        "completed" => "completed",
        "failed" => "failed",
        "killed" => "killed",
        "unavailable" => "unavailable",
        other => other,
    };
    let mut metadata_parts = vec![
        format!("offset {offset} -> {}", snapshot.end_offset),
        format!("total {} bytes", snapshot.total_bytes),
        format!("{} total lines", snapshot.total_lines),
        format!(
            "terminal {}",
            if snapshot.terminal { "true" } else { "false" }
        ),
    ];
    if !snapshot.output_ref.trim().is_empty() {
        metadata_parts.push(format!("output_ref {}", snapshot.output_ref));
    }
    if let Some(title) = snapshot
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
    {
        metadata_parts.push(format!("title {}", title.trim()));
    }
    let metadata = metadata_parts.join(" · ");
    if snapshot.output.is_empty() {
        let state = match (kind, snapshot.status.as_str()) {
            ("local agent", "pending") => "Pending · local agent has not started",
            ("local agent", "running") => "No result yet · local agent still running",
            ("local agent", "waiting_for_input") => "Local agent waiting for input · no result yet",
            ("local agent", "completed") => "Local agent completed with no result",
            ("local agent", "failed") => "Local agent failed with no output",
            ("local agent", "killed") => "Local agent stopped with no result",
            ("local agent", "unavailable") => {
                "Local agent unavailable · stale handle or unsupported runner"
            }
            (_, "pending") => "Pending · no output yet",
            (_, "running") => "No output yet · still running",
            (_, "waiting_for_input") => "Waiting for input · no new output",
            (_, "completed") => "Completed with no output",
            (_, "failed") => "Failed with no output",
            (_, "killed") => "Stopped with no output",
            (_, "unavailable") => "Unavailable · stale handle or unsupported runner",
            _ => "No output yet",
        };
        return format!("{header}\n{state} · {metadata}");
    }

    let chunk = snapshot.output.trim_end();
    let line_count = chunk.lines().count();
    format!(
        "{header}\n{line_count} new {} · {metadata} · {status_label}\nOutput chunk:\n{chunk}",
        if line_count == 1 { "line" } else { "lines" }
    )
}

fn format_background_task_output_timeout(task_id: &str, timeout_ms: u64) -> String {
    format!("Read shell output {task_id}\nNo output yet · still running after {timeout_ms}ms")
}

fn format_background_task_output_registry_timeout(task_id: &str, timeout: Duration) -> String {
    format!(
        "Read shell output {task_id}\nBackground task registry did not respond within {}ms. Output polling is unavailable for this turn; the task may still be running.",
        duration_ms_u64(timeout)
    )
}

fn format_background_task_stop_registry_timeout(task_id: &str, timeout: Duration) -> String {
    format!(
        "Background task {task_id} stop status unknown\nBackground task registry did not respond within {}ms. The task may still be running; retry task_stop or task_list.",
        duration_ms_u64(timeout)
    )
}

fn format_background_task_list_registry_timeout(timeout: Duration) -> String {
    format!(
        "Background task registry unavailable\nTimed out after {}ms waiting for the interactive session to answer. Background tasks may still be running; retry task_list later.",
        duration_ms_u64(timeout)
    )
}

fn format_background_task_unavailable(cloud_session: bool) -> String {
    if cloud_session {
        "Background task unavailable\nno edge runner is attached to this cloud session".to_string()
    } else {
        "Background task unavailable\nlocal background tasks require an interactive CLI session"
            .to_string()
    }
}

fn background_task_status_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "unavailable")
}

fn background_task_status_should_return_immediately(status: &str) -> bool {
    background_task_status_is_terminal(status) || status == "waiting_for_input"
}

fn background_task_id_arg(args: &Value) -> Result<Option<String>, &'static str> {
    let Some(value) = args.get("task_id") else {
        return Ok(None);
    };
    let Some(raw) = value.as_str() else {
        return Err("task_id must be a non-empty string");
    };
    let id = raw.trim();
    if id.is_empty() {
        return Err("Task id is required");
    }
    Ok(Some(id.to_string()))
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
    /// variable-size output (git(action=diff), git(action=show)) to scale their limits.
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
    /// Optional progress sink for the *currently running* bash
    /// invocation. `stream_render::execute_tool` installs a fresh
    /// sink before dispatching bash and clears it on completion,
    /// so the sink pointer is alive only for the duration of a
    /// single in-flight call. Read by `shell_ops`/`edge_tools::shell`
    /// wait loops to record byte/line counters; polled by a
    /// `StreamEvent::ToolOutput` ticker.
    pub(crate) bash_progress_sink:
        std::sync::RwLock<Option<std::sync::Arc<crate::cli::chat_stream::ToolProgressSink>>>,
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
    /// Wrapped in Arc so the chat session can share the journal across turns.
    pub file_journal:
        std::sync::Arc<std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>>,
    /// MatrixOne snapshot journal — records captured pre-state snapshots so the
    /// executor can perform a bounded restore without reconstructing tool history.
    pub(crate) database_snapshot_journal:
        std::sync::Arc<std::sync::Mutex<mo_tools::DatabaseSnapshotRollbackJournal>>,
    /// Git stash rollback journal — records captured stash handles so bounded
    /// turn/batch rollback can re-apply shelved working tree state.
    pub git_stash_journal: std::sync::Arc<std::sync::Mutex<git_gix::GitStashRollbackJournal>>,
    /// Git commit rollback journal — records captured commit handles so bounded
    /// turn/batch rollback can revert recent committed history when it is still safe.
    pub git_commit_journal: std::sync::Arc<std::sync::Mutex<git_gix::GitCommitRollbackJournal>>,
    /// Git worktree rollback journal — records newly created worktrees so bounded
    /// turn/batch rollback can remove them again while they are still clean.
    pub(crate) git_worktree_journal:
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
    /// Broadcast sender that signals the TaskBoardObserver after a
    /// successful session task-board mutation. Payload is the session_id that
    /// changed. `None` when offline (in-memory store handles its own
    /// notifications via `InMemoryTaskStore::subscribe`).
    pub(crate) task_notify_tx: Option<tokio::sync::broadcast::Sender<String>>,
    /// Command queue for background task operations. Drained by the
    /// TUI event loop each tick. Allows the tool executor (which runs
    /// inside the agentic loop) to spawn/kill background tasks without
    /// owning the registry directly.
    ///
    /// `None` when no TUI/REPL is attached — in that case background
    /// task actions fail fast with a clear error rather than pushing
    /// to a queue nobody drains (which would hang the LLM turn forever).
    pub(crate) bg_task_commands: Option<std::sync::Arc<std::sync::Mutex<Vec<BgTaskCommand>>>>,
    /// Shared background task list cache.
    /// When the TUI event loop is active, it writes rendered
    /// task-list XML here every tick so [`Self::task_list_bg`] can
    /// bypass the BG command queue.
    pub(crate) bg_task_list_cache: Option<std::sync::Arc<tokio::sync::RwLock<String>>>,
    /// Detach slot for the bash tool. Renewed before each tool call
    /// by the TUI event loop so a fresh one-shot reply channel is
    /// available for every bash invocation. `None` outside the TUI
    /// (server mode, headless tests) — bash there remains a normal
    /// foreground command.
    pub(crate) bash_detach_slot: Option<astra_tools::detach::DetachShellSlot>,
    /// Optional agent spawning context for `agent(action='spawn'|'get_result')`.
    pub spawn_context: Option<agent_spawning::AgentActionContext>,
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
        std::sync::Arc<std::sync::RwLock<astra_runtime::observability::ObservabilitySession>>,
    >,
    /// Budget-adaptive introspection snapshot, updated each turn by the
    /// execution phase. The `introspect` tool reads this to return runtime
    /// state to the model.
    introspect_snapshot:
        std::sync::Arc<std::sync::RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>>,
    /// Session-memory observatory shared with
    /// [`MemoryExtractionService`] and the compaction path. `None` in
    /// tests and offline modes — `introspect subtopic=session_memory`
    /// then renders a short placeholder telling the model the
    /// observatory isn't wired rather than silently returning empty.
    session_memory_observatory:
        Option<std::sync::Arc<astra_runtime::session_memory::SessionMemoryObservatory>>,
    /// Session id for persisting self-modification state and serving `astra self`
    /// compatible diagnostics from inside the live agent loop.
    active_session_id: std::sync::Mutex<Option<String>>,
    /// Concrete model selected for the active turn/session. This is the
    /// source used by self-introspection tools; it is set by the CLI turn
    /// boundary and never inferred from a tool surface default.
    current_model: std::sync::RwLock<Option<String>>,
    /// Self-modification pinned tool preferences (manual override hints).
    self_mod_pinned_tools: std::sync::Mutex<Vec<String>>,
    /// Self-modification deprioritized tool preferences (manual override hints).
    self_mod_deprioritized_tools: std::sync::Mutex<Vec<String>>,
    /// P3.1 seam: cross-session lessons loaded at session bootstrap.
    /// Populated once via `set_session_lessons`, then passed through on
    /// every `build_self_model_snapshot` for the session's lifetime.
    session_lessons: std::sync::Mutex<Vec<astra_services::LessonHint>>,
    /// P3.3 seam: latest auto-invoked diagnostic skill output.
    /// `AutoInvokeHandler::maybe_fire` writes each successful parse here;
    /// the next `build_self_model_snapshot` injects it into the prompt and
    /// `set_latest_skill_diagnosis(None)` clears it once the triggering
    /// condition has resolved.
    latest_skill_diagnosis: std::sync::Mutex<Option<astra_skills::auto_invoke::SkillDiagnosis>>,
    /// Latest passive evaluator feedback from the previous turn. Kept separate
    /// from auto-invoked skill diagnoses so evaluator hints are not lost to
    /// diagnosis cooldown/clear behavior.
    latest_turn_quality_feedback:
        std::sync::Mutex<Option<astra_runtime::self_model::TurnQualityFeedback>>,
    /// Per-turn mutation accounting for adjust_config governor.
    /// (turn_number, mutations_applied_on_turn)
    self_mod_mutation_counter: std::sync::Mutex<(u32, u32)>,
    /// Shared tool executor for delegating unknown tools to astra-tools.
    default_executor: astra_tools::executor::DefaultToolExecutor,
    /// Plugin-registered tool schemas (e.g. MCP servers). Joined with the
    /// static catalog when `tool_search(select:X)` runs, so deferred
    /// activation can reach plugin tools. Populated by the TUI after
    /// `PluginRegistry::register` loads the user's skill manifests.
    plugin_schemas: std::sync::RwLock<Vec<Value>>,
    /// Atomic snapshot of the current visible/deferred execution surface.
    ///
    /// Visible and activatable names are read together by admission and
    /// activation paths. Keeping them behind one lock prevents impossible
    /// mixed snapshots such as "new activatable names with old visible names".
    current_tool_surface: std::sync::RwLock<ToolSurfaceNames>,
    /// Deferred tool names whose full schema has been fetched via
    /// `tool_search(query="select:NAME")`. Names remain pending until that
    /// tool is actually called once from a visible schema surface, or until a
    /// non-empty runtime surface proves the activation is stale.
    activated_deferred_tools: std::sync::RwLock<HashSet<String>>,
    /// Cached plan-mode authoring flag keyed by the session it was
    /// computed for. Mirrors the server-side write guard so a CLI run
    /// that talks to the same plan store cannot bypass plan mode by
    /// routing mutations through the local executor (session b4cef5bb
    /// regression). The session key is load-bearing: web-agent and
    /// scripted callers may reuse one executor across sessions, so a
    /// `Some(true)` probe for session A must never block writes in
    /// session B.
    plan_mode_authoring_cache: std::sync::Arc<tokio::sync::RwLock<Option<(String, bool)>>>,
    /// Per-turn ask-user channel — the host swaps a fresh sender in
    /// before each turn so the `ask_user` tool can reach the
    /// bottom-pane overlay.
    ask_user_request_tx: std::sync::Mutex<Option<crate::cli::chat_stream::AskUserRequestTx>>,
    /// Per-turn plan-review channel — installed by the host before
    /// each turn so `exit_plan_mode` can surface the dedicated
    /// plan-review overlay (scrollable plan body + 4-way radio).
    /// Separate from `ask_user_request_tx` because the plan overlay
    /// renders a markdown body, not the question/option layout
    /// `ask_user` needs.
    plan_review_request_tx: std::sync::Mutex<Option<crate::cli::chat_stream::PlanReviewRequestTx>>,
    /// Slot recording a permission-mode switch that the user
    /// confirmed inside a tool overlay (currently `exit_plan_mode`'s
    /// 4-option dialog). The host drains this slot at the start of
    /// the next turn — applying mid-turn would race the agentic
    /// loop's borrow of `perm_manager`.
    pending_permission_mode_change:
        std::sync::Mutex<Option<crate::cli::permission_manager::PermissionMode>>,
    /// One-shot schema boost for the next agentic round. Used by
    /// `exit_plan_mode` so the model immediately regains the core
    /// edit tools (`bash` / `read_file` / `write_file` /
    /// `str_replace`) after the user approves execution.
    pending_round_tool_boost: std::sync::Mutex<Option<Vec<String>>>,
}

impl ToolExecutor {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        let root: PathBuf = project_root.into();
        let preferred_repos = detect_git_remote_repos(&root);
        let sandbox = astra_runtime::tool_sandbox::SandboxPolicy::for_project(&root);
        let executor = Self {
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
            plugin_schemas: std::sync::RwLock::new(Vec::new()),
            current_tool_surface: std::sync::RwLock::new(ToolSurfaceNames::default()),
            activated_deferred_tools: std::sync::RwLock::new(HashSet::new()),
            sandbox_policy: std::sync::RwLock::new(Some(sandbox)),
            preferred_repos: std::sync::Mutex::new(preferred_repos),
            budget_pressure: std::sync::Mutex::new(0.0),
            build_test_tracker: std::sync::Mutex::new(build_test::BuildTestTracker::new()),
            memoria_fail_count: std::sync::atomic::AtomicU32::new(0),
            memoria_notified_down: std::sync::atomic::AtomicBool::new(false),
            file_state: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            aggregate_output_bytes: std::sync::atomic::AtomicUsize::new(0),
            bash_progress_sink: std::sync::RwLock::new(None),
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
            task_manager: std::sync::Arc::new(task_mgmt::TaskManager::in_memory()),
            task_notify_tx: None,
            bg_task_commands: None,
            bg_task_list_cache: None,
            bash_detach_slot: None,
            spawn_context: None,
            context_cache: None,
            agent_id: None,
            send_message_context: std::sync::Mutex::new(None),
            observability_session: None,
            introspect_snapshot: std::sync::Arc::new(std::sync::RwLock::new(None)),
            session_memory_observatory: None,
            active_session_id: std::sync::Mutex::new(None),
            current_model: std::sync::RwLock::new(None),
            self_mod_pinned_tools: std::sync::Mutex::new(Vec::new()),
            self_mod_deprioritized_tools: std::sync::Mutex::new(Vec::new()),
            session_lessons: std::sync::Mutex::new(Vec::new()),
            latest_skill_diagnosis: std::sync::Mutex::new(None),
            latest_turn_quality_feedback: std::sync::Mutex::new(None),
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
                    detach_shell_handle: None,
                },
            ),
            plan_mode_authoring_cache: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            ask_user_request_tx: std::sync::Mutex::new(None),
            plan_review_request_tx: std::sync::Mutex::new(None),
            pending_permission_mode_change: std::sync::Mutex::new(None),
            pending_round_tool_boost: std::sync::Mutex::new(None),
        };
        #[cfg(test)]
        {
            executor.install_default_test_visible_surface();
        }
        executor
    }

    #[cfg(test)]
    fn install_default_test_visible_surface(&self) {
        let mut schemas = all_tool_schemas();
        schemas.extend(CLI_LOCAL_EXECUTOR_TOOL_NAMES.iter().map(|name| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": "test-only local executor surface",
                    "parameters": {"type": "object", "properties": {}}
                }
            })
        }));
        let schemas = self.runtime_bound_tool_schemas(schemas);
        self.set_current_visible_tool_schemas(&schemas);
    }

    /// Install the per-turn `ask_user` channel so tools can surface a
    /// TUI overlay for confirmations.
    /// `None` clears the slot — passed at turn boundaries so a stale
    /// sender never leaks across turns.
    pub fn set_ask_user_request_tx(&self, tx: Option<crate::cli::chat_stream::AskUserRequestTx>) {
        let mut guard = self.ask_user_request_tx.lock().unwrap_or_else(|e| {
            tracing::error!(
                "ask_user_request_tx lock poisoned; recovering and overwriting stale sender"
            );
            e.into_inner()
        });
        *guard = tx;
    }

    /// Install the per-turn plan-review channel so `exit_plan_mode`
    /// can surface the dedicated plan-approval overlay. Cleared at
    /// turn boundaries to keep stale senders from leaking into
    /// background sub-runs that share the same `Arc<ToolExecutor>`.
    pub fn set_plan_review_request_tx(
        &self,
        tx: Option<crate::cli::chat_stream::PlanReviewRequestTx>,
    ) {
        let mut guard = self.plan_review_request_tx.lock().unwrap_or_else(|e| {
            tracing::error!(
                "plan_review_request_tx lock poisoned; recovering and overwriting stale sender"
            );
            e.into_inner()
        });
        *guard = tx;
    }

    /// Drain a permission-mode change recorded by a tool overlay (see
    /// `pending_permission_mode_change`). Returns the requested mode
    /// once and clears the slot. Called at turn start by the loop host.
    pub fn take_pending_permission_mode_change(
        &self,
    ) -> Option<crate::cli::permission_manager::PermissionMode> {
        self.pending_permission_mode_change
            .lock()
            .ok()
            .and_then(|mut g| g.take())
    }

    /// Drain a one-shot list of tool names that should be force-injected into
    /// the next agentic round's schema selection.
    pub fn take_pending_round_tool_boost(&self) -> Option<Vec<String>> {
        self.pending_round_tool_boost
            .lock()
            .ok()
            .and_then(|mut g| g.take())
    }

    /// Names of deferred tools currently queued for short-lived schema injection.
    /// Stale entries are pruned against the current visible/activatable
    /// surface so this side set cannot become a long-lived allowlist.
    pub fn activated_deferred_tool_names(&self) -> Vec<String> {
        let surface =
            self.current_tool_surface_snapshot("current_tool_surface_activation_retention");
        if matches!(surface, ToolSurfaceNames::Uninstalled) {
            return Vec::new();
        }

        let mut guard = rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools_prune",
        );
        let retained =
            astra_turn_core::tool::deferred_activation::retained_runtime_bound_activated_tool_names(
                &guard,
                &surface,
                |name| self.tool_has_runtime_binding(name),
            );
        // Use set-based comparison, not length comparison: same-count with
        // different names (e.g., {a,b} → {c,d}) must also trigger pruning.
        let retained_set: HashSet<&str> = retained.iter().map(String::as_str).collect();
        let before = guard.len();
        guard.retain(|name| retained_set.contains(name.as_str()));
        let after = guard.len();
        tracing::debug!(before, after, "pruned CLI activated_deferred_tools entries");
        retained
    }

    /// Return activated deferred tools for the next schema-selection round.
    ///
    /// Activation is consumed only when the tool is actually called from a
    /// visible schema surface. Merely including the schema in `tools[]` must
    /// not drop other selected tools from a long `select:a,b,c` chain.
    pub fn activated_deferred_tool_names_for_schema_injection(&self) -> Vec<String> {
        let surface = self.current_tool_surface_snapshot("current_tool_surface_activation_take");
        if matches!(surface, ToolSurfaceNames::Uninstalled) {
            return Vec::new();
        }

        let mut guard = rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools_take",
        );
        let before = guard.len();
        let retained =
            astra_turn_core::tool::deferred_activation::activated_tool_names_for_schema_injection(
                &mut guard,
                &surface,
                |name| self.tool_has_runtime_binding(name),
            );
        let after = guard.len();
        if before > 0 {
            tracing::debug!(
                before,
                after,
                returned = retained.len(),
                "resolved CLI activated_deferred_tools for schema injection"
            );
        }
        retained
    }

    /// Set the spawn context for agent spawning.
    pub fn with_spawn_context(mut self, ctx: agent_spawning::AgentActionContext) -> Self {
        self.spawn_context = Some(ctx);
        #[cfg(test)]
        {
            self.install_default_test_visible_surface();
        }
        self
    }

    /// Install plugin-registered schemas so `tool_search(select:NAME)`
    /// can resolve MCP / skill-backed tools. Called once at TUI start
    /// after `PluginRegistry::register` loads manifests.
    ///
    /// Poison handling: plugin schemas are a rebuildable cache. Reset cached
    /// state on poison instead of reusing possibly half-written inner data.
    pub fn set_plugin_schemas(&self, schemas: Vec<Value>) {
        let mut guard = rwlock_write_reset_on_poison(&self.plugin_schemas, "plugin_schemas");
        *guard = schemas;
    }

    /// Install the visible `tools[]` names for the current LLM request.
    pub fn set_current_visible_tool_schemas(&self, schemas: &[Value]) {
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(schemas);
        let mut guard =
            rwlock_write_reset_on_poison(&self.current_tool_surface, "current_tool_surface");
        let activatable = guard.activatable().cloned().unwrap_or_default();
        *guard = ToolSurfaceNames::installed(names, activatable);
    }

    /// Install the names that this turn's deferred manifest allows
    /// `tool_search(select:NAME)` to activate.
    pub fn set_current_activatable_tool_names(&self, names: HashSet<String>) {
        let names = self.runtime_bound_tool_names(names);
        let mut guard =
            rwlock_write_reset_on_poison(&self.current_tool_surface, "current_tool_surface");
        let visible = guard.visible().cloned().unwrap_or_default();
        *guard = ToolSurfaceNames::installed(visible, names);
    }

    /// Install the exact current surface in one write.
    ///
    /// Use this when both visible and activatable names are derived from the
    /// same payload assembly pass. It prevents readers from observing a
    /// half-updated surface between two setter calls.
    pub fn set_current_tool_surface(
        &self,
        visible_schemas: &[Value],
        activatable_names: HashSet<String>,
    ) {
        let visible = astra_turn_core::tool::schema::tool_names_from_schemas(visible_schemas);
        let activatable = self.runtime_bound_tool_names(activatable_names);
        let mut guard =
            rwlock_write_reset_on_poison(&self.current_tool_surface, "current_tool_surface");
        *guard = ToolSurfaceNames::installed(visible, activatable);
    }

    #[cfg(test)]
    pub(crate) fn clear_current_tool_surface_for_tests(&self) {
        *rwlock_write_reset_on_poison(
            &self.current_tool_surface,
            "current_tool_surface_test_clear",
        ) = ToolSurfaceNames::default();
        rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools_test_clear",
        )
        .clear();
    }

    /// Snapshot of the names that the model's `<deferred_tools>` manifest
    /// currently advertises. Used by the host to keep its validator-side
    /// `deferred_tool_names` set in lockstep with what the prompt rendered.
    pub fn current_activatable_tool_names_snapshot(&self) -> HashSet<String> {
        self.current_tool_surface_snapshot("current_tool_surface_snapshot")
            .activatable()
            .cloned()
            .unwrap_or_default()
    }

    fn current_searchable_tool_names(&self) -> Option<HashSet<String>> {
        let surface = self.current_tool_surface_snapshot("current_tool_surface_search_pool");

        astra_turn_core::tool::deferred_activation::searchable_runtime_bound_tool_names(
            &surface,
            |name| self.tool_has_runtime_binding(name),
        )
    }

    fn current_tool_surface_snapshot(&self, label: &str) -> ToolSurfaceNames {
        rwlock_read_clone_or_default(&self.current_tool_surface, label)
    }

    fn runtime_bound_tool_names(&self, names: HashSet<String>) -> HashSet<String> {
        astra_turn_core::tool::deferred_activation::runtime_bound_tool_names(names, |name| {
            self.tool_has_runtime_binding(name)
        })
    }

    pub(crate) fn runtime_bound_tool_schemas(&self, schemas: Vec<Value>) -> Vec<Value> {
        schemas
            .into_iter()
            .filter(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_some_and(|name| self.tool_has_runtime_binding(name))
            })
            .collect()
    }

    fn tool_has_runtime_binding(&self, name: &str) -> bool {
        if name.starts_with("mcp__") {
            return self.mcp_tool_has_runtime_binding(name);
        }
        let Some(meta) = astra_turn_core::tool::registry::meta::tool_meta(name) else {
            return self.cli_declared_local_tool_has_name(name)
                || self.plugin_schema_has_name(name);
        };
        meta.requires
            .iter()
            .all(|capability| self.capability_has_runtime_binding(*capability))
    }

    fn cli_declared_local_tool_has_name(&self, name: &str) -> bool {
        static STATIC_SCHEMA_NAMES: std::sync::OnceLock<std::collections::HashSet<String>> =
            std::sync::OnceLock::new();
        CLI_LOCAL_EXECUTOR_TOOL_NAMES.contains(&name)
            || STATIC_SCHEMA_NAMES
                .get_or_init(|| {
                    full_tool_schemas()
                        .iter()
                        .filter_map(astra_turn_core::tool::schema::tool_schema_name)
                        .map(str::to_string)
                        .collect()
                })
                .contains(name)
    }

    fn plugin_schema_has_name(&self, name: &str) -> bool {
        self.plugin_schemas_snapshot("plugin_schemas_runtime_binding")
            .iter()
            .any(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_some_and(|schema_name| schema_name == name)
            })
    }

    fn mcp_tool_has_runtime_binding(&self, name: &str) -> bool {
        let Some(manager) = &self.mcp_manager else {
            return false;
        };
        manager
            .try_read()
            .is_ok_and(|manager| manager.find_tool_by_mcp_name(name).is_some())
    }

    fn capability_has_runtime_binding(
        &self,
        capability: astra_turn_core::capability::Capability,
    ) -> bool {
        if !capability.is_executor_gated() {
            // Non-gated capabilities are always available — they are backed by
            // services or static features on this node.
            return true;
        }
        // Executor-gated capabilities require an active executor handle.
        self.executor_binding_is_active(capability)
    }

    /// Check whether this runtime node has an active executor for a gated capability.
    fn executor_binding_is_active(
        &self,
        capability: astra_turn_core::capability::Capability,
    ) -> bool {
        use astra_turn_core::capability::Capability;
        match capability {
            Capability::AgentSpawner => self.spawn_context.is_some(),
            // Fail-closed: unknown executor-gated capabilities are denied.
            // If a new executor-gated variant is added here, it MUST get an
            // explicit match arm — the wildcard is a safety net, not a policy.
            _ => false,
        }
    }

    fn tool_can_validate_without_runtime_binding(&self, name: &str, args: &Value) -> bool {
        let action = args.get("action").and_then(Value::as_str);
        astra_turn_core::tool::registry::meta::tool_allows_validation_without_runtime_binding(
            name, action,
        )
    }

    fn tool_binding_admission_denial(&self, name: &str, args: &Value) -> Option<EdgeToolRun> {
        if self.tool_has_runtime_binding(name) {
            return None;
        }
        if self.tool_can_validate_without_runtime_binding(name, args) {
            return None;
        }
        let message = astra_turn_core::tool::runtime_binding::runtime_binding_denial_message(
            name,
            args.get("action").and_then(Value::as_str),
        );
        Some(EdgeToolRun::classified_error(
            format!("Error: {message}"),
            astra_core::ErrorKind::ToolBinding,
        ))
    }

    fn tool_admission_denial(&self, name: &str, args: &Value) -> Option<EdgeToolRun> {
        // ── Phase 1: Structural binding (no locks) ───────────────────────
        //
        // First principles: "does this tool have an executor attached?" is
        // the most fundamental admission question. It runs before the visible
        // surface admission locks so lock recovery cannot widen the set of
        // tools admitted for this turn.
        //
        // Whether a tool needs binding is declared by its Capability list.
        // Each capability self-declares `is_executor_gated()`. The binding
        // check is structural (no tool-name special cases) and runs lock-free.
        if let Some(denial) = self.tool_binding_admission_denial(name, args) {
            return Some(denial);
        }

        // ── Phase 2: Admission-set membership (lock-protected) ──────────
        //
        // An executor is bound; now confirm the tool was actually
        // advertised / activated in this session.
        let surface = self.current_tool_surface_snapshot("current_tool_surface_admission");
        if surface.visible_contains(name) {
            return None;
        }

        // If no visible-set restriction is configured, deny (fail-closed).
        // First principles: if the visible tool set was never configured (e.g.
        // crash recovery before `set_current_visible_tool_schemas` ran), we
        // cannot confirm the tool was ever advertised in this session. Admitting
        // would widen the gate past what Phase 1 (binding) alone guarantees.
        if matches!(surface, ToolSurfaceNames::Uninstalled) {
            return Some(EdgeToolRun::classified_error(
                astra_turn_core::tool::deferred_activation::tool_not_admitted_message(name, false),
                astra_core::ErrorKind::ToolBinding,
            ));
        }

        let can_select = surface.activatable_contains(name);
        use astra_turn_core::tool::deferred_activation::{
            DirectDeferredCallAdmission, classify_direct_deferred_call,
            direct_deferred_call_activated_message, tool_not_admitted_message,
        };

        match classify_direct_deferred_call(name, can_select, |tool_name| {
            self.tool_has_runtime_binding(tool_name)
        }) {
            DirectDeferredCallAdmission::Activate {
                name: activated_name,
            } => {
                // Direct deferred call: the model called a tool advertised in
                // `<deferred_tools>` without first selecting it via
                // `tool_search(select:NAME)`. Treat as activation intent —
                // record the name so the next turn's `tools[]` includes the
                // full schema, then ask the model to retry. Do NOT execute:
                // the args are untrusted because the schema was not visible.
                let mut guard = rwlock_write_reset_on_poison(
                    &self.activated_deferred_tools,
                    "activated_deferred_tools_direct_call",
                );
                astra_turn_core::tool::deferred_activation::refresh_activated_tool_names(
                    &mut guard,
                    [activated_name.clone()],
                );
                return Some(EdgeToolRun::error(direct_deferred_call_activated_message(
                    &activated_name,
                )));
            }
            DirectDeferredCallAdmission::NotAdmitted => {
                return Some(EdgeToolRun::error(tool_not_admitted_message(name, true)));
            }
            DirectDeferredCallAdmission::Unknown => {}
        }
        Some(EdgeToolRun::error(tool_not_admitted_message(
            name, can_select,
        )))
    }

    fn record_tool_search_activation_output(&self, output: &str) {
        let surface = self.current_tool_surface_snapshot("current_tool_surface_activation");
        let names = astra_turn_core::tool::deferred_activation::recordable_activated_tool_names(
            output,
            &surface,
            |name| self.tool_has_runtime_binding(name),
        );
        if names.is_empty() {
            return;
        }

        let mut guard = rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools",
        );
        astra_turn_core::tool::deferred_activation::refresh_activated_tool_names(&mut guard, names);
    }

    fn consume_activated_deferred_tool_if_called(&self, name: &str) {
        let surface = self.current_tool_surface_snapshot("current_tool_surface_consume_call");
        if !surface.visible_contains(name) {
            return;
        }
        let mut guard = rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools_consume_call",
        );
        if astra_turn_core::tool::deferred_activation::consume_activated_tool_name(&mut guard, name)
        {
            tracing::debug!(
                tool = name,
                "consumed CLI deferred activation after visible tool call"
            );
        }
    }

    pub(crate) fn runtime_bound_plugin_schemas_excluding(
        &self,
        restricted_tools: &HashSet<String>,
    ) -> Vec<Value> {
        let plugin_schemas: Vec<Value> = self
            .plugin_schemas_snapshot("plugin_schemas_deferred_manifest")
            .into_iter()
            .filter(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_none_or(|name| !restricted_tools.contains(name))
            })
            .collect();
        self.runtime_bound_tool_schemas(plugin_schemas)
    }

    pub(super) fn plugin_schemas_snapshot(&self, label: &str) -> Vec<Value> {
        rwlock_read_clone_or_default(&self.plugin_schemas, label)
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
        let mut guard = self.send_message_context.lock().unwrap_or_else(|e| {
            tracing::error!(
                "send_message_context lock poisoned; recovering and overwriting stale slot"
            );
            e.into_inner()
        });
        *guard = ctx;
    }

    /// Set the observability session for context analysis tools.
    pub fn with_observability_session(
        mut self,
        session: std::sync::Arc<
            std::sync::RwLock<astra_runtime::observability::ObservabilitySession>,
        >,
    ) -> Self {
        self.observability_session = Some(session);
        self
    }

    /// Attach the shared session-memory observatory so
    /// `introspect subtopic=session_memory` can read the rings. Callers
    /// supply the same `Arc` they gave to `MemoryExtractionService`.
    pub fn with_session_memory_observatory(
        mut self,
        observatory: std::sync::Arc<astra_runtime::session_memory::SessionMemoryObservatory>,
    ) -> Self {
        self.session_memory_observatory = Some(observatory);
        self
    }

    pub fn with_active_session_id(self, session_id: impl Into<String>) -> Self {
        self.set_active_session_id(session_id);
        self
    }

    /// Install a progress sink for the next/currently-running bash
    /// invocation. Passing `None` clears the sink. Kept as a
    /// pointer rather than passed on every call so `shell.rs` can
    /// reach it from the sync wait loop without threading a new arg
    /// through half the shell module.
    pub fn set_bash_progress_sink(
        &self,
        sink: Option<std::sync::Arc<crate::cli::chat_stream::ToolProgressSink>>,
    ) {
        let mut slot = self.bash_progress_sink.write().unwrap_or_else(|e| {
            tracing::error!(
                "bash_progress_sink lock poisoned; recovering and overwriting stale sink"
            );
            e.into_inner()
        });
        *slot = sink;
    }

    /// Non-blocking read of the active bash progress sink. Used by
    /// `shell.rs`'s read-with-timeout loop to bump counters; returns
    /// `None` when no sink is installed (non-TUI callers, tests).
    pub(crate) fn current_bash_progress_sink(
        &self,
    ) -> Option<std::sync::Arc<crate::cli::chat_stream::ToolProgressSink>> {
        self.bash_progress_sink.read().ok().and_then(|g| g.clone())
    }

    pub fn set_active_session_id(&self, session_id: impl Into<String>) {
        let session_id = session_id.into();
        let session_changed = self.active_session_id().as_deref() != Some(session_id.as_str());
        let (pinned_tools, deprioritized_tools) =
            match astra_services::session_workspace::read_workspace_optional(&session_id) {
                Ok(Some(ws)) => (ws.pinned_tools, ws.deprioritized_tools),
                Ok(None) => (Vec::new(), Vec::new()),
                Err(error) => {
                    tracing::warn!(
                        "active session {} has unreadable workspace metadata; clearing self-mod tool preferences: {}",
                        session_id,
                        error
                    );
                    (Vec::new(), Vec::new())
                }
            };
        if let Ok(mut pinned) = self.self_mod_pinned_tools.lock() {
            *pinned = pinned_tools;
        }
        if let Ok(mut deprioritized) = self.self_mod_deprioritized_tools.lock() {
            *deprioritized = deprioritized_tools;
        }
        // File-edit checkpoint persistence: on session-id set, rebind the
        // journal to an auto-persist directory keyed by session.
        //
        // True merge (R9.1): load any prior-run entries from disk,
        // then merge them UNDER any pre-session in-memory entries via
        // `merge_older_entries`. Both sides survive, sequences are
        // re-issued contiguously, and `enable_persistence`'s initial
        // flush pushes the combined state back to disk.
        //
        // This handles all four quadrants of (memory empty/not) ×
        // (disk empty/not):
        // - (empty, empty): no-op merge, empty journal + persistence
        // - (empty, has): merge loads disk entries → memory reflects disk
        // - (has, empty): merge is no-op, flush writes memory to disk
        // - (has, has): both preserved, resequenced, flushed together
        if let Some(dir) = file_checkpoint_dir_for(&session_id)
            && let Ok(mut journal) = self.file_journal.lock()
        {
            // Three cases:
            //   (a) Already bound to the same dir → idempotent short-circuit.
            //       Skipping the merge prevents re-reading our own flushed
            //       entries and prepending them as older duplicates.
            //   (b) Bound to a DIFFERENT dir → rebinding sessions.
            //       Sessions are isolation boundaries: sid1's entries must
            //       NOT leak into sid2's dir. Reset in-memory state
            //       before loading sid2's own disk history.
            //   (c) Unbound → first binding. Merge disk entries under any
            //       pre-session in-memory entries (R9.1).
            match journal.persist_dir() {
                Some(existing) if existing == dir.as_path() => {
                    // Case (a): nothing to do.
                }
                Some(_) => {
                    // Case (b): different session. Clear memory first so
                    // sid1 entries don't flow into sid2's dir via the
                    // initial flush. Then load sid2's own disk state.
                    *journal = astra_turn_core::file_edit_journal::FileEditJournal::new(500);
                    if let Ok(disk_journal) =
                        astra_turn_core::file_edit_journal::FileEditJournal::load_from_dir(
                            &dir, 500,
                        )
                    {
                        journal.merge_older_entries(disk_journal);
                    }
                    journal.enable_persistence(dir);
                }
                None => {
                    // Case (c): first binding — R9.1 merge policy.
                    if let Ok(disk_journal) =
                        astra_turn_core::file_edit_journal::FileEditJournal::load_from_dir(
                            &dir, 500,
                        )
                    {
                        journal.merge_older_entries(disk_journal);
                    }
                    journal.enable_persistence(dir);
                }
            }
        }
        if session_changed {
            if let Ok(mut pending_mode) = self.pending_permission_mode_change.lock() {
                *pending_mode = None;
            }
            if let Ok(mut pending_boost) = self.pending_round_tool_boost.lock() {
                *pending_boost = None;
            }
            if let Ok(mut lessons) = self.session_lessons.lock() {
                lessons.clear();
            }
            if let Ok(mut diag) = self.latest_skill_diagnosis.lock() {
                *diag = None;
            }
            if let Ok(mut feedback) = self.latest_turn_quality_feedback.lock() {
                *feedback = None;
            }
            if let Ok(mut current_model) = self.current_model.write() {
                *current_model = None;
            }
        }
        if let Ok(mut guard) = self.active_session_id.lock() {
            *guard = Some(session_id);
        }
    }

    pub(crate) fn active_session_id(&self) -> Option<String> {
        self.active_session_id.lock().ok().and_then(|g| g.clone())
    }

    pub fn set_current_model(&self, model: impl Into<String>) {
        let raw = model.into();
        let model = astra_core::model_override::normalize_model_override(Some(raw.as_str()))
            .map(str::to_string);
        if let Ok(mut guard) = self.current_model.write() {
            *guard = model;
        }
    }

    fn current_model(&self) -> Option<String> {
        self.current_model.read().ok().and_then(|g| g.clone())
    }

    fn memory_args_with_context(&self, args: &Value) -> Value {
        let mut clean_args = args.clone();
        if let Some(obj) = clean_args.as_object_mut() {
            obj.remove("action");
            // Inject the active session id so focus hints and session-scoped
            // recalls work. CLI does not own a user_id — leave it to the
            // cloud proxy / Memoria server to fill in via the bearer token.
            if let Some(sid) = self.active_session_id().filter(|sid| !sid.is_empty()) {
                obj.insert("session_id".to_string(), serde_json::Value::String(sid));
            }
            let turn = self
                .journal_turn_index
                .load(std::sync::atomic::Ordering::Acquire);
            obj.insert(
                "turn".to_string(),
                serde_json::Value::Number(serde_json::Number::from(turn)),
            );
        }
        clean_args
    }

    /// P3.1 seam: stash cross-session lessons loaded at session bootstrap.
    /// Every subsequent `build_self_model_snapshot` will project them via
    /// [`astra_runtime::self_model::SelfModel::with_lessons`].
    pub fn set_session_lessons(&self, lessons: Vec<astra_services::LessonHint>) {
        if let Ok(mut g) = self.session_lessons.lock() {
            *g = lessons;
        }
    }

    /// Snapshot the currently-stashed session lessons. Used by the
    /// injection-freshness observer (`observe_bridge_injections`) so it can
    /// fingerprint the same slice the next SelfModel snapshot will
    /// project into the system prompt.
    pub fn session_lessons_snapshot(&self) -> Vec<astra_services::LessonHint> {
        self.session_lessons
            .lock()
            .map(|g| g.clone())
            .unwrap_or_default()
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

    pub fn set_latest_turn_quality_feedback(
        &self,
        feedback: Option<astra_runtime::self_model::TurnQualityFeedback>,
    ) {
        if let Ok(mut g) = self.latest_turn_quality_feedback.lock() {
            *g = feedback;
        }
    }

    /// Use a shared file edit journal (session-scoped) instead of the default.
    ///
    /// If an `active_session_id` is already set when this is called, the
    /// persistence binding is re-applied to the incoming journal so the
    /// `ToolExecutor::new(..).with_active_session_id(sid).with_shared_file_journal(arc)`
    /// order (as used by `sse_loop/mod.rs`) preserves crash-recovery
    /// semantics. Without this, the prior call to `with_active_session_id`
    /// would be silently discarded by the journal swap.
    ///
    /// **First-binding-wins policy**: if the incoming journal is already
    /// persistence-enabled (e.g. it was configured by an earlier
    /// ToolExecutor sharing the same Arc), this is a no-op. Rebinding
    /// mid-life would churn disk writes and is surprising — a shared
    /// journal carries its first binding. Set session_id on the first
    /// executor that sees the journal.
    ///
    /// **True merge policy**: if the incoming journal already holds
    /// in-memory entries AND the session's on-disk dir has prior-run
    /// entries, BOTH sides are preserved via
    /// [`FileEditJournal::merge_older_entries`] — disk entries go first
    /// (chronologically earlier), pre-session entries follow, all
    /// re-sequenced contiguously. The initial flush then writes the
    /// combined state to disk.
    pub fn with_shared_file_journal(
        mut self,
        journal: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        >,
    ) -> Self {
        self.file_journal = journal;
        // Re-apply persistence to the new journal if a session is already active.
        // Guarded by session_id presence so pure in-memory callers (no session
        // set) keep the original zero-side-effect behavior.
        if let Some(sid) = self.active_session_id()
            && let Some(dir) = file_checkpoint_dir_for(&sid)
            && let Ok(mut j) = self.file_journal.lock()
            && j.persist_dir().is_none()
        {
            // True merge (R9.1): load disk into a temp journal, then
            // merge_older_entries into self's in-memory state. See
            // set_active_session_id for the rationale matrix.
            if let Ok(disk_journal) =
                astra_turn_core::file_edit_journal::FileEditJournal::load_from_dir(&dir, 500)
            {
                j.merge_older_entries(disk_journal);
            }
            j.enable_persistence(dir);
        }
        self
    }

    /// Use a shared file-state cache (session-scoped) so read-before-write
    /// tracking survives across plan executor subtask turns.
    pub(crate) fn with_shared_file_state(mut self, state: SharedFileState) -> Self {
        self.file_state = state;
        self
    }

    /// Return a clone of the shared file-state Arc for cross-turn sharing.
    pub(crate) fn shared_file_state(&self) -> SharedFileState {
        self.file_state.clone()
    }

    /// Use a shared MatrixOne snapshot journal (session-scoped) instead of the default.
    pub(crate) fn with_shared_database_snapshot_journal(
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
    pub(crate) fn with_shared_git_worktree_journal(
        mut self,
        journal: std::sync::Arc<std::sync::Mutex<worktree::GitWorktreeRollbackJournal>>,
    ) -> Self {
        self.git_worktree_journal = journal;
        self
    }

    /// Use a shared session-state rollback journal (session-scoped) instead of the default.
    pub(crate) fn with_shared_session_state_journal(
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

    pub fn with_task_notify_tx(mut self, tx: tokio::sync::broadcast::Sender<String>) -> Self {
        self.task_notify_tx = Some(tx);
        self
    }

    pub(crate) fn with_bg_task_commands(
        mut self,
        commands: std::sync::Arc<std::sync::Mutex<Vec<BgTaskCommand>>>,
    ) -> Self {
        self.bg_task_commands = Some(commands);
        self
    }

    pub(crate) fn with_bg_task_list_cache(
        mut self,
        cache: std::sync::Arc<tokio::sync::RwLock<String>>,
    ) -> Self {
        self.bg_task_list_cache = Some(cache);
        self
    }

    /// Install the host-owned detach slot so the bash tool can
    /// observe Ctrl+B and transfer its child to the background
    /// registry. The slot is shared (`Arc`) — both the executor and
    /// the TUI hold the same instance so the TUI can refill it
    /// between tool calls without rebuilding the executor.
    pub fn with_bash_detach_slot(mut self, slot: astra_tools::detach::DetachShellSlot) -> Self {
        self.default_executor
            .set_detach_shell_slot(Some(slot.clone()));
        self.bash_detach_slot = Some(slot);
        self
    }

    /// Configure cloud proxy for memory tool calls.
    pub fn with_cloud(mut self, base: impl Into<String>, token: impl Into<String>) -> Self {
        self.cloud_base = Some(base.into());
        self.set_cloud_token(token);
        self
    }

    pub(crate) fn set_cloud_token(&self, token: impl Into<String>) {
        *astra_core::sync_poison::recover_rwlock_write(&self.cloud_token) = Some(token.into());
    }

    pub(crate) fn cloud_token(&self) -> Option<String> {
        astra_core::sync_poison::recover_rwlock_read(&self.cloud_token).clone()
    }

    // ─── Plan-mode write guard (parity with server_tool_executor) ───────────
    //
    // While a plan is in authoring (`phase=planning` or `phase=refining`),
    // world-mutating tools must be
    // short-circuited so the model cannot bypass authoring by routing
    // writes through the local executor. The check fails open when the
    // CLI is offline / unauthenticated — without a cloud binding there
    // is no plan store to consult, and a "fail closed" stance would
    // break every offline `astra` invocation.

    async fn plan_mode_authoring_active(&self) -> bool {
        let Some(session_id) = self.active_session_id().filter(|sid| !sid.is_empty()) else {
            return false;
        };
        if let Some((cached_session_id, cached)) =
            self.plan_mode_authoring_cache.read().await.as_ref()
            && cached_session_id == &session_id
        {
            return *cached;
        }
        let active = self
            .recompute_plan_mode_authoring_for_session(session_id.as_str())
            .await;
        if self.active_session_id().as_deref() == Some(session_id.as_str()) {
            *self.plan_mode_authoring_cache.write().await = Some((session_id, active));
        }
        active
    }

    fn cloud_plan_summary_status<'a>(&self, plan: &'a Value) -> Option<&'a str> {
        plan.get("status")
            // Older server payloads used `phase`; keep accepting that shape so
            // newer CLIs can talk to pre-migration servers during rolling upgrades.
            // TODO(#plans): remove this fallback once all servers ship `status` field (≥v2.1).
            .or_else(|| plan.get("phase"))
            .and_then(Value::as_str)
    }

    fn cloud_plan_summary_id(&self, plan: &Value) -> Option<String> {
        plan.get("plan_id")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn cloud_plan_is_authoring(&self, plan: &Value) -> bool {
        matches!(
            self.cloud_plan_summary_status(plan),
            Some("planning" | "refining")
        )
    }

    async fn lookup_active_cloud_plan_summary(&self, session_id: &str) -> Option<Value> {
        let token = self.cloud_token()?;
        let Ok(client) = self.remote_plan_client() else {
            return None;
        };
        let plans = match client
            .get_plans_query_json(
                &token,
                &[
                    ("session_id", session_id.to_string()),
                    ("active_session_only", "true".to_string()),
                    ("limit", "1".to_string()),
                ],
            )
            .await
        {
            Ok(value) => value,
            Err(_) => return None,
        };
        plans
            .get("plans")
            .and_then(Value::as_array)
            .and_then(|arr| arr.first())
            .cloned()
    }

    async fn recompute_plan_mode_authoring_for_session(&self, session_id: &str) -> bool {
        self.lookup_active_cloud_plan_summary(session_id)
            .await
            .is_some_and(|plan| self.cloud_plan_is_authoring(&plan))
    }

    pub(crate) async fn invalidate_plan_mode_cache(&self) {
        *self.plan_mode_authoring_cache.write().await = None;
    }

    async fn set_plan_mode_authoring_cache_for_active_session(&self, active: bool) {
        if let Some(session_id) = self.active_session_id().filter(|sid| !sid.is_empty()) {
            *self.plan_mode_authoring_cache.write().await = Some((session_id, active));
        } else {
            self.invalidate_plan_mode_cache().await;
        }
    }

    // ─── Task management methods (delegated to task_mgmt module) ────────────

    fn task_action_allowed_fields(action: &str) -> Option<&'static [&'static str]> {
        match action {
            "create" => Some(&[
                "action",
                "title",
                "description",
                "subtasks",
                "active_form",
                "owner",
                "metadata",
                "add_blocks",
                "add_blocked_by",
            ]),
            "list" => Some(&["action", "status_filter"]),
            "get" => Some(&["action", "task_id"]),
            "update" => Some(&[
                "action",
                "task_id",
                "new_status",
                "status",
                "title",
                "description",
                "subtask_id",
                "active_form",
                "owner",
                "metadata",
                "add_blocks",
                "add_blocked_by",
                "remove_blocks",
                "remove_blocked_by",
                "reason",
                "error_message",
            ]),
            "stop" => Some(&["action", "task_id", "reason"]),
            "list_user" => Some(&["action", "user_status"]),
            "adopt" => Some(&["action", "source_session_id", "task_id"]),
            "archive" => Some(&["action", "task_id", "older_than_days", "reason"]),
            _ => None,
        }
    }

    fn task_actions_allowing_field(field: &str, current_action: &str) -> Vec<&'static str> {
        const TASK_ACTIONS: &[&str] = &[
            "create",
            "list",
            "get",
            "update",
            "stop",
            "list_user",
            "adopt",
            "archive",
        ];
        TASK_ACTIONS
            .iter()
            .copied()
            .filter(|action| *action != current_action)
            .filter(|action| {
                Self::task_action_allowed_fields(action)
                    .is_some_and(|allowed| allowed.contains(&field))
            })
            .collect()
    }

    fn task_unknown_field_message(action: &str, key: &str, allowed: &[&str]) -> String {
        let other_actions = Self::task_actions_allowing_field(key, action);
        let action_hint = if other_actions.is_empty() {
            String::new()
        } else {
            format!(
                "; field is valid for: {}",
                other_actions
                    .iter()
                    .map(|action| format!("task.{action}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        format!(
            "unknown field '{key}' for task.{action} (valid: {}{})",
            allowed.join(", "),
            action_hint
        )
    }

    fn validate_task_tool_args_for_action(action: &str, args: &Value) -> Result<(), String> {
        let Some(allowed) = Self::task_action_allowed_fields(action) else {
            return Ok(());
        };
        let Some(obj) = args.as_object() else {
            return Err(format!("task.{action} arguments must be an object"));
        };
        for key in obj.keys() {
            if !allowed.contains(&key.as_str()) {
                return Err(Self::task_unknown_field_message(action, key, allowed));
            }
        }
        Ok(())
    }

    fn task_output_json(output: &str) -> Option<Value> {
        if output.starts_with("Error:") {
            return None;
        }
        // task_mgmt now prefixes success responses with a human-readable
        // summary line followed by the JSON body (see `prefix_summary`).
        // Strip up to the first `{` so JSON parsing still works for
        // the old-format (pure JSON) and new-format (summary + JSON)
        // responses. Pre-prefix responses still work — `find('{')` on
        // a pure-JSON string returns 0 and we parse the whole string.
        let json_body = match output.find('{') {
            Some(pos) => &output[pos..],
            None => return None,
        };
        serde_json::from_str::<Value>(json_body).ok()
    }

    fn task_output_success(output: &str) -> bool {
        Self::task_output_json(output)
            .and_then(|value| value.get("success").and_then(Value::as_bool))
            .unwrap_or(false)
    }

    fn task_action_mutates_board(action: &str) -> bool {
        matches!(action, "create" | "update" | "stop" | "adopt" | "archive")
    }

    fn task_lifecycle_summary(action: &str, payload: &Value) -> &'static str {
        if payload.get("subtask_id").and_then(Value::as_str).is_some() {
            return "subtask_updated";
        }
        match action {
            "create" => "task_created",
            "stop" => "task_cancelled",
            "archive" => "task_archived",
            "update" => match payload.get("status").and_then(Value::as_str) {
                Some("completed") => "task_completed",
                Some("failed") => "task_failed",
                Some("cancelled") => "task_cancelled",
                Some("deleted") => "task_deleted",
                _ => "task_updated",
            },
            _ => "task_updated",
        }
    }

    fn task_lifecycle_detail(action: &str, args: &Value, payload: &Value) -> Value {
        let mut detail = serde_json::Map::new();
        detail.insert("action".to_string(), json!(action));
        if let Some(value) = payload
            .get("task_id")
            .cloned()
            .or_else(|| args.get("task_id").cloned())
        {
            detail.insert("task_id".to_string(), value);
        }
        if let Some(value) = payload
            .get("subtask_id")
            .cloned()
            .or_else(|| args.get("subtask_id").cloned())
        {
            detail.insert("subtask_id".to_string(), value);
        }
        if let Some(value) = args.get("title").cloned() {
            detail.insert("title".to_string(), value);
        }
        if let Some(value) = payload.get("previous_status").cloned() {
            detail.insert("previous_status".to_string(), value);
        }
        let final_status = payload.get("status").cloned().or_else(|| match action {
            "create" => Some(json!("pending")),
            "stop" => Some(json!("cancelled")),
            _ => None,
        });
        if let Some(value) = final_status {
            detail.insert("status".to_string(), value);
        }
        if let Some(value) = payload
            .get("reason")
            .cloned()
            .or_else(|| args.get("reason").cloned())
        {
            detail.insert("reason".to_string(), value);
        }
        if let Some(value) = payload.get("cancelled_subtasks").cloned() {
            detail.insert("cancelled_subtasks".to_string(), value);
        }
        if let Some(value) = payload.get("archived").cloned() {
            detail.insert("archived".to_string(), value);
        }
        Value::Object(detail)
    }

    fn record_task_lifecycle_event(&self, action: &str, args: &Value, output: &str) {
        let Some(session_id) = self
            .active_session_id()
            .filter(|sid| !sid.trim().is_empty())
        else {
            return;
        };
        let Some(payload) = Self::task_output_json(output) else {
            return;
        };
        if payload.get("success").and_then(Value::as_bool) != Some(true) {
            return;
        }
        let turn = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Acquire);
        crate::cli::cli_config::cli_utils::append_session_journal_event_or_warn(
            &session_id,
            &astra_services::session_journal::JournalEvent::task_lifecycle(
                Some(&session_id),
                turn,
                Self::task_lifecycle_summary(action, &payload),
                Some(Self::task_lifecycle_detail(action, args, &payload)),
            ),
            "edge_tools:record_task_lifecycle_event",
        );
    }

    /// Route a `task` action either to the cloud (production) or the
    /// local in-memory TaskManager (offline/tests). Cloud is the
    /// preferred path: when `cloud_base` and `active_session_id` are
    /// both set, the call goes to `POST /sessions/{sid}/todos:execute`
    /// so the server is the single source of truth — CLI never
    /// touches MO directly. Falls back to the in-memory manager only
    /// when no cloud is wired (one-shot CLI, headless tests).
    async fn route_task_action(&self, action: &str, args: &Value) -> Option<String> {
        let cloud_base = self.cloud_base.clone()?;
        let session_id = self.active_session_id()?;
        if session_id.is_empty() {
            return None;
        }
        let token = self.cloud_token();
        match crate::cli::session::session_todo_client::execute_todo_action(
            &cloud_base,
            token.as_deref(),
            &session_id,
            action,
            args,
        )
        .await
        {
            Ok(output) => {
                if Self::task_action_mutates_board(action)
                    && Self::task_output_success(&output)
                    && let Some(tx) = &self.task_notify_tx
                {
                    let _ = tx.send(session_id);
                }
                Some(output)
            }
            Err(err) => Some(format!("Error: cloud todo {action} failed: {err}")),
        }
    }

    fn remote_plan_client(&self) -> Result<astra_thin_client::ThinClient, String> {
        let Some(cloud_base) = self.cloud_base.clone() else {
            return Err(
                "Error: plan lifecycle is unavailable in offline CLI mode; connect to cloud first."
                    .to_string(),
            );
        };
        astra_thin_client::ThinClient::new(&cloud_base, None)
            .map_err(|err| format!("Error: failed to initialize plan client: {err}"))
    }

    async fn enter_plan_mode_remote(&self, args: &Value) -> String {
        // Symmetric with `exit_plan_mode_remote`: there are two
        // structurally different paths and we pick by what the
        // environment supports, not by what the caller requests.
        //
        // 1. Cloud path — active session id + cloud token + reachable
        //    plan client all present → POST `/plans` to create a
        //    `phase=planning` row so the server-side write guard
        //    engages and `/plan` UI / multi-client coordination can
        //    see the authoring state. Used by the `/plan "goal"`
        //    slash command and any web-agent driven entry.
        //
        // 2. Local path — any prerequisite missing (no session id,
        //    no token, no client, network failure) → fall back to a
        //    purely local plan-mode pivot. Stages
        //    `PermissionMode::Plan` on the pending slot so the host
        //    flips `perm_manager` at the next turn boundary, exactly
        //    like Shift+Tab. No cloud row is created and no error
        //    bubbles up: a detached / unauthenticated CLI run still
        //    gets plan mode.
        //
        // Both branches always stage Plan on the pending slot —
        // single-source-of-truth invariant I6: whichever path runs,
        // `perm_manager.mode()` becomes `Plan` on the next turn.
        let goal = args
            .get("goal")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|goal| !goal.is_empty())
            .unwrap_or("(pending)");

        let cloud_outcome = self.try_enter_plan_mode_cloud_path(goal).await;
        match cloud_outcome {
            Ok(message) => {
                self.stage_pending_plan_mode();
                message
            }
            Err(_unavailable) => {
                self.stage_pending_plan_mode();
                // Local guard cache is informational — there is no
                // server guard to mirror in this branch. Set it to
                // `Some(true)` so any cached probe consult sees
                // "writes gated" while plan mode is active.
                self.set_plan_mode_authoring_cache_for_active_session(true)
                    .await;
                format!(
                    "Entered plan mode (local). goal=\"{goal}\". Write tools are now blocked — investigate read-only, then call exit_plan_mode(plan=\"<markdown>\") when ready."
                )
            }
        }
    }

    /// Attempt the cloud `enter_plan_mode` flow. Returns `Err(())`
    /// (with no message) when any prerequisite is missing so the
    /// caller falls back to the local path silently. Returns
    /// `Err(message)` semantics are *not* used here — a real cloud
    /// failure (e.g. server returned 5xx) is also swallowed into the
    /// local fallback because the user's intent ("enter plan mode")
    /// must succeed end-to-end.
    async fn try_enter_plan_mode_cloud_path(&self, goal: &str) -> Result<String, ()> {
        let session_id = self
            .active_session_id()
            .filter(|sid| !sid.is_empty())
            .ok_or(())?;
        let token = self.cloud_token().ok_or(())?;
        let client = self.remote_plan_client().map_err(|_| ())?;

        let response = client
            .post_plans_json(
                &token,
                &json!({
                    "goal": goal,
                    "session_id": session_id,
                }),
            )
            .await
            .map_err(|_| ())?;

        let plan_id = response
            .get("plan_id")
            .and_then(Value::as_str)
            .ok_or(())?
            .to_string();
        self.set_plan_mode_authoring_cache_for_active_session(true)
            .await;
        Ok(format!(
            "Entered plan mode. plan_id={plan_id} goal=\"{goal}\". Write tools are now blocked — author the plan, then call exit_plan_mode when it's ready for execution."
        ))
    }

    /// Stage `PermissionMode::Plan` on the pending slot so the host
    /// applies it on the next turn boundary. Idempotent — repeated
    /// calls within the same turn collapse to a single switch.
    fn stage_pending_plan_mode(&self) {
        if let Ok(mut slot) = self.pending_permission_mode_change.lock() {
            *slot = Some(crate::cli::permission_manager::PermissionMode::Plan);
        }
    }

    fn stage_pending_permission_mode_change(
        &self,
        mode: crate::cli::permission_manager::PermissionMode,
    ) {
        if let Ok(mut slot) = self.pending_permission_mode_change.lock() {
            *slot = Some(mode);
        }
    }

    fn stage_pending_round_tool_boost(&self, tools: &[&str]) {
        if let Ok(mut slot) = self.pending_round_tool_boost.lock() {
            *slot = Some(tools.iter().map(|name| (*name).to_string()).collect());
        }
    }

    #[cfg(test)]
    pub(crate) fn debug_stage_pending_round_tool_boost_for_test(&self, tools: &[&str]) {
        self.stage_pending_round_tool_boost(tools);
    }

    #[cfg(test)]
    pub(crate) fn debug_stage_pending_permission_mode_change_for_test(
        &self,
        mode: crate::cli::permission_manager::PermissionMode,
    ) {
        self.stage_pending_permission_mode_change(mode);
    }

    async fn exit_plan_mode_remote(&self, args: &Value) -> String {
        // `exit_plan_mode` has two structurally different sources of
        // truth depending on how plan mode was entered:
        //
        // 1. Cloud workflow (`/plan "goal"` or the `enter_plan_mode`
        //    tool): a `plans` row with `phase=planning` exists; the
        //    server-side write guard depends on it; approving the
        //    plan must POST `/plans/{id}/exit-plan-mode` so the row
        //    flips to `refining` and the guard releases.
        // 2. Shift+Tab / `/allow plan`: only flips the local
        //    `perm_manager` to `Plan`. There is no cloud row, no
        //    server-side guard, and the user expects exiting to be
        //    purely local — zero network calls.
        //
        // Conflating both broke session d9b5119f: the user pressed
        // Shift+Tab, the model produced a plan, called exit_plan_mode,
        // and the cloud lookup returned "no active planning plan
        // found" because none was ever created.
        //
        // The fix: probe the cloud row only when the prerequisites
        // are present (active session + cloud token + reachable plan
        // client + a planning row actually exists). If any of those
        // are missing, fall through to the local path which uses the
        // overlay + `pending_permission_mode_change` slot exactly the
        // same way the cloud path does — it just skips the network
        // round-trips and the `phase=planning` row update.
        let plan_markdown = args
            .get("plan")
            .and_then(Value::as_str)
            .or_else(|| args.get("plan_markdown").and_then(Value::as_str))
            .or_else(|| args.get("plan_md").and_then(Value::as_str))
            .map(str::trim)
            .filter(|plan| !plan.is_empty())
            .map(str::to_string);
        let cloud_plan_id = self.lookup_active_authoring_cloud_plan_id().await;
        match cloud_plan_id {
            Some(plan_id) => {
                self.exit_plan_mode_cloud_path(plan_id, plan_markdown.as_deref())
                    .await
            }
            None => {
                self.exit_plan_mode_local_path(plan_markdown.as_deref())
                    .await
            }
        }
    }

    /// Best-effort lookup for an active authoring cloud plan
    /// for the current session. Returns `None` whenever any of the
    /// prerequisites for the cloud workflow are absent (no session,
    /// no token, no client, no row, network failure). The caller
    /// uses `None` as the signal to fall back to the purely local
    /// Shift+Tab plan-mode flow.
    async fn lookup_active_authoring_cloud_plan_id(&self) -> Option<String> {
        let session_id = self.active_session_id().filter(|sid| !sid.is_empty())?;
        let plan = self
            .lookup_active_cloud_plan_summary(session_id.as_str())
            .await?;
        if self.cloud_plan_is_authoring(&plan) {
            self.cloud_plan_summary_id(&plan)
        } else {
            None
        }
    }

    /// Cloud workflow exit path — there is a `phase=planning` row
    /// in the `plans` table; flipping it to `refining` is the
    /// authoritative signal that releases the server-side write
    /// guard. The 4-option overlay still runs locally; only the
    /// follow-up state mutation goes through the cloud API.
    async fn exit_plan_mode_cloud_path(
        &self,
        plan_id: String,
        plan_markdown: Option<&str>,
    ) -> String {
        let Some(token) = self.cloud_token() else {
            return "Error: exit_plan_mode lost the cloud token mid-flight.".to_string();
        };
        let client = match self.remote_plan_client() {
            Ok(client) => client,
            Err(err) => return err,
        };

        let (approved, follow_up_mode) =
            match self.resolve_exit_plan_mode_via_overlay(plan_markdown).await {
                Ok(decision) => decision,
                Err(message) => return message,
            };

        let mut body = json!({ "approved": approved });
        if let Some(plan_markdown) = plan_markdown {
            body["plan_md"] = Value::String(plan_markdown.to_string());
        }

        let response = match client
            .post_plan_exit_mode_json(&token, &plan_id, &body)
            .await
        {
            Ok(value) => value,
            Err(err) => {
                return format!(
                    "Error: failed to exit plan mode: {}",
                    crate::cli::cli_config::cli_utils::map_thin_err(err)
                );
            }
        };
        let resolved_plan_id = response
            .get("plan_id")
            .and_then(Value::as_str)
            .unwrap_or(&plan_id)
            .to_string();

        if approved {
            let next_mode =
                follow_up_mode.unwrap_or(crate::cli::permission_manager::PermissionMode::Auto);
            self.set_plan_mode_authoring_cache_for_active_session(false)
                .await;
            self.stage_pending_permission_mode_change(next_mode);
            self.stage_pending_round_tool_boost(&[
                "bash",
                "read_file",
                "write_file",
                "str_replace",
            ]);
            let mode_suffix = format!(" Next turn will run in {next_mode} mode.");
            format!(
                "Exited plan mode. plan_id={resolved_plan_id} is approved; write tools unlocked.{mode_suffix}"
            )
        } else {
            self.set_plan_mode_authoring_cache_for_active_session(true)
                .await;
            format!(
                "Plan {resolved_plan_id} left open for another authoring pass. Write tools remain blocked. Address the user's feedback and call exit_plan_mode again when ready."
            )
        }
    }

    /// Local-only exit path (Shift+Tab / `/allow plan` entry). No
    /// cloud row, no server-side guard, no network calls — purely
    /// the overlay + `pending_permission_mode_change` slot. This is
    /// `exit_plan_mode` here is a permission-state pivot driven by
    /// user choice — no cloud row is required for it to succeed.
    async fn exit_plan_mode_local_path(&self, plan_markdown: Option<&str>) -> String {
        let (approved, follow_up_mode) =
            match self.resolve_exit_plan_mode_via_overlay(plan_markdown).await {
                Ok(decision) => decision,
                Err(message) => return message,
            };

        if approved {
            let next_mode =
                follow_up_mode.unwrap_or(crate::cli::permission_manager::PermissionMode::Auto);
            // Local guard cache is informational only here — there
            // is no cloud guard to mirror. Setting it to `Some(false)`
            // keeps the cached flag consistent with "writes are
            // unlocked" so any tool that consults it reads the same
            // outcome the cloud path would have set.
            self.set_plan_mode_authoring_cache_for_active_session(false)
                .await;
            self.stage_pending_permission_mode_change(next_mode);
            self.stage_pending_round_tool_boost(&[
                "bash",
                "read_file",
                "write_file",
                "str_replace",
            ]);
            let plan_suffix = match plan_markdown {
                Some(plan) if !plan.is_empty() => {
                    format!(" Plan recorded:\n{plan}")
                }
                _ => String::new(),
            };
            let mode_suffix = format!(" Next turn will run in {next_mode} mode.");
            format!("Exited plan mode; user approved.{mode_suffix}{plan_suffix}")
        } else {
            // Keep planning: the local perm_manager mode is unchanged
            // (the host never staged a switch). The cache flag
            // mirrors "writes still gated" for downstream consultation.
            self.set_plan_mode_authoring_cache_for_active_session(true)
                .await;
            "Plan left open for another authoring pass. Address the user's feedback and call exit_plan_mode again when ready.".to_string()
        }
    }

    /// Surface the Approve / Keep-planning overlay through the
    /// per-turn `ask_user` channel. Returns `(approved, next_mode)`
    /// when the user submits an answer; `Err(message)` when the
    /// channel is missing (headless context) or the prompt is
    /// cancelled — in that case the model sees the message as the
    /// tool result and stays in plan mode.
    async fn resolve_exit_plan_mode_via_overlay(
        &self,
        plan_markdown: Option<&str>,
    ) -> Result<(bool, Option<crate::cli::permission_manager::PermissionMode>), String> {
        use crate::cli::chat_stream::{PlanReviewDecision, PlanReviewRequest};

        let tx = self
            .plan_review_request_tx
            .lock()
            .ok()
            .and_then(|guard| guard.clone());
        let Some(tx) = tx else {
            return Err(
                "Error: exit_plan_mode requires a trusted interactive plan-review overlay; model-supplied approval arguments are ignored."
                    .to_string(),
            );
        };

        let plan_body = plan_markdown
            .unwrap_or("(plan body was empty — pass a `plan` argument so the user can review it)")
            .to_string();

        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        if tx
            .send(PlanReviewRequest {
                plan_markdown: plan_body,
                response_tx,
            })
            .is_err()
        {
            return Err("Error: exit_plan_mode overlay sink is closed.".to_string());
        }

        match response_rx.await.unwrap_or(PlanReviewDecision::Cancelled) {
            PlanReviewDecision::Approve { mode } => Ok((true, Some(mode))),
            PlanReviewDecision::KeepPlanning | PlanReviewDecision::Cancelled => Ok((false, None)),
        }
    }

    async fn task_action_create(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("create", args).await {
            self.record_task_lifecycle_event("create", args, &output);
            return output;
        }
        let mut snapshot = match self.task_manager.try_snapshot_state().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return format!("Error: failed to capture task rollback snapshot: {error}");
            }
        };
        let output = self.task_manager.create(args).await;
        if Self::task_output_success(&output) {
            if let Err(error) = self
                .task_manager
                .seal_snapshot_for_restore(&mut snapshot)
                .await
            {
                return format!("Error: failed to seal task rollback snapshot: {error}");
            }
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task:create:{}",
                    args.get("title").and_then(Value::as_str).unwrap_or("task")
                ),
            );
            self.record_task_lifecycle_event("create", args, &output);
        }
        output
    }
    async fn task_list(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("list", args).await {
            return output;
        }
        self.task_manager.list(args).await
    }
    async fn task_get(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("get", args).await {
            return output;
        }
        self.task_manager.get(args).await
    }
    async fn task_action_update(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("update", args).await {
            self.record_task_lifecycle_event("update", args, &output);
            return output;
        }
        let mut snapshot = match self.task_manager.try_snapshot_state().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return format!("Error: failed to capture task rollback snapshot: {error}");
            }
        };
        let output = self.task_manager.update(args).await;
        if Self::task_output_success(&output) {
            if let Err(error) = self
                .task_manager
                .seal_snapshot_for_restore(&mut snapshot)
                .await
            {
                return format!("Error: failed to seal task rollback snapshot: {error}");
            }
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task:update:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
            self.record_task_lifecycle_event("update", args, &output);
        }
        output
    }
    async fn task_action_stop(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("stop", args).await {
            self.record_task_lifecycle_event("stop", args, &output);
            return output;
        }
        let mut snapshot = match self.task_manager.try_snapshot_state().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return format!("Error: failed to capture task rollback snapshot: {error}");
            }
        };
        let output = self.task_manager.stop(args).await;
        if Self::task_output_success(&output) {
            if let Err(error) = self
                .task_manager
                .seal_snapshot_for_restore(&mut snapshot)
                .await
            {
                return format!("Error: failed to seal task rollback snapshot: {error}");
            }
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task:stop:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
            self.record_task_lifecycle_event("stop", args, &output);
        }
        output
    }

    /// `task(action='list_user')` — cross-session active list. Cloud
    /// only: in-memory mode by definition has only one session, so
    /// the cross-session question is meaningless without a backing
    /// store that aggregates across users.
    async fn task_list_user(&self, args: &Value) -> String {
        let status = match Self::normalize_task_user_status(args) {
            Ok(status) => status,
            Err(err) => return err,
        };
        let Some(cloud_base) = self.cloud_base.clone() else {
            return "Error: task(action='list_user') requires a cloud connection. \
                    The cross-session view is server-side only — set ASTRA_API_URL \
                    or sign in with `astra login` to enable it."
                .to_string();
        };
        let token = self.cloud_token();
        match crate::cli::session::session_todo_client::list_user_todos(
            &cloud_base,
            token.as_deref(),
            status,
        )
        .await
        {
            Ok(output) => output,
            Err(err) => format!("Error: list_user todos failed: {err}"),
        }
    }

    fn normalize_task_user_status(args: &Value) -> Result<&str, String> {
        let Some(raw) = args.get("user_status") else {
            return Ok("active");
        };
        let Some(status) = raw.as_str() else {
            return Err("Error: field 'user_status' must be a string".to_string());
        };
        if astra_tools::task_mgmt::VALID_LIST_STATUS_FILTERS.contains(&status) {
            Ok(status)
        } else {
            Err(format!(
                "Error: invalid user_status '{}' (valid: {})",
                status,
                astra_tools::task_mgmt::VALID_LIST_STATUS_FILTERS.join("|")
            ))
        }
    }

    /// `task(action='adopt', source_session_id, task_id)` — bring a
    /// task from another of the user's sessions into the current
    /// session. Server-side it copies the row's title/description/
    /// metadata into a fresh todo here and marks the source migrated
    /// so the user doesn't see it twice. Cloud-only.
    async fn task_adopt(&self, args: &Value) -> String {
        if self.cloud_base.is_none() {
            return "Error: task(action='adopt') requires a cloud connection.".to_string();
        }
        // Adopt is a write — route through the same execute endpoint.
        // Server-side dispatch will reject if source isn't owned by
        // the same user (auth check via SessionService).
        match self.route_task_action("adopt", args).await {
            Some(output) => output,
            None => "Error: cannot adopt task without an active session id".to_string(),
        }
    }

    /// `task(action='archive', task_id?)` — either archive one
    /// current-session task immediately, or bulk-archive stale
    /// completed history in the current session.
    async fn task_archive(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("archive", args).await {
            self.record_task_lifecycle_event("archive", args, &output);
            return output;
        }
        let mut snapshot = match self.task_manager.try_snapshot_state().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                return format!("Error: failed to capture task rollback snapshot: {error}");
            }
        };
        let output = self.task_manager.archive(args).await;
        if Self::task_output_success(&output) {
            if let Err(error) = self
                .task_manager
                .seal_snapshot_for_restore(&mut snapshot)
                .await
            {
                return format!("Error: failed to seal task rollback snapshot: {error}");
            }
            self.record_task_state_rollback(
                snapshot,
                format!(
                    "task:archive:{}",
                    args.get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("bulk")
                ),
            );
            self.record_task_lifecycle_event("archive", args, &output);
        }
        output
    }

    async fn task_list_bg(&self) -> String {
        // Fast path: read the latest snapshot directly from the shared
        // cache. The TUI event loop refreshes this every tick, so we
        // completely bypass the BG command queue and event-loop tick
        // latency.
        if let Some(ref cache) = self.bg_task_list_cache {
            let cached = cache.read().await;
            if !cached.is_empty() {
                return cached.clone();
            }
            // Cache not yet populated (first call before the event
            // loop has rendered). Fall through to the queue path so
            // we still return a valid response.
        }
        // Fallback: queue path for when no cache is available
        let Some(ref bg_commands) = self.bg_task_commands else {
            return format_background_task_unavailable(self.cloud_base.is_some());
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut cmds = bg_commands.lock_recover();
            cmds.push(BgTaskCommand::List { reply: tx });
        }
        let reply_timeout = background_task_reply_timeout(BG_TASK_COMMAND_REPLY_TIMEOUT_MS);
        match await_bg_task_command_reply(rx, reply_timeout).await {
            Ok(output) => output,
            Err(BgTaskReplyError::Closed) => {
                "Error: background task registry not available".to_string()
            }
            Err(BgTaskReplyError::TimedOut) => {
                format_background_task_list_registry_timeout(reply_timeout)
            }
        }
    }

    async fn task_output(&self, args: &Value) -> String {
        let task_id = match background_task_id_arg(args) {
            Ok(Some(id)) => id,
            Err(error) => return error.to_string(),
            Ok(None) => return "Task id is required".to_string(),
        };
        let block = args.get("block").and_then(Value::as_bool).unwrap_or(false);
        let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(8_192)
            .min(65_536) as usize;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(1, 300_000);

        if let Some(output) = self
            .fanout_group_task_output_response(&task_id, offset, max_bytes, None)
            .await
        {
            return output;
        }

        let Some(ref bg_commands) = self.bg_task_commands else {
            return format_background_task_unavailable(self.cloud_base.is_some());
        };

        if block {
            let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    return format_background_task_output_timeout(&task_id, timeout_ms);
                }
                let reply_timeout = background_task_reply_timeout(timeout_ms).min(remaining);
                let (tx, rx) = tokio::sync::oneshot::channel();
                {
                    let mut cmds = bg_commands.lock_recover();
                    cmds.push(BgTaskCommand::GetOutputSince {
                        task_id: task_id.clone(),
                        offset,
                        max_bytes,
                        reply: tx,
                    });
                }
                match await_bg_task_command_reply(rx, reply_timeout).await {
                    Ok(Ok(snapshot)) => {
                        if snapshot.kind != "shell"
                            || background_task_status_should_return_immediately(&snapshot.status)
                            || !snapshot.output.is_empty()
                            || tokio::time::Instant::now() >= deadline
                        {
                            return format_background_task_output(&task_id, offset, &snapshot);
                        }
                    }
                    Ok(Err(e)) => return format_background_task_error(&task_id, &e),
                    Err(BgTaskReplyError::Closed) => {
                        return "Error: background task registry not available".to_string();
                    }
                    Err(BgTaskReplyError::TimedOut) => {
                        return format_background_task_output_registry_timeout(
                            &task_id,
                            reply_timeout,
                        );
                    }
                }
                if tokio::time::Instant::now() >= deadline {
                    return format_background_task_output_timeout(&task_id, timeout_ms);
                }
                let sleep_for = Duration::from_millis(500)
                    .min(deadline.saturating_duration_since(tokio::time::Instant::now()));
                if sleep_for.is_zero() {
                    return format_background_task_output_timeout(&task_id, timeout_ms);
                }
                tokio::time::sleep(sleep_for).await;
            }
        } else {
            let reply_timeout = background_task_reply_timeout(timeout_ms);
            let (tx, rx) = tokio::sync::oneshot::channel();
            {
                let mut cmds = bg_commands.lock_recover();
                cmds.push(BgTaskCommand::GetOutputSince {
                    task_id: task_id.clone(),
                    offset,
                    max_bytes,
                    reply: tx,
                });
            }
            match await_bg_task_command_reply(rx, reply_timeout).await {
                Ok(Ok(snapshot)) => format_background_task_output(&task_id, offset, &snapshot),
                Ok(Err(e)) => format_background_task_error(&task_id, &e),
                Err(BgTaskReplyError::Closed) => {
                    "Error: background task registry not available".to_string()
                }
                Err(BgTaskReplyError::TimedOut) => {
                    format_background_task_output_registry_timeout(&task_id, reply_timeout)
                }
            }
        }
    }

    async fn fanout_group_task_output_response(
        &self,
        task_id: &str,
        offset: u64,
        max_bytes: usize,
        miss_reason: Option<&str>,
    ) -> Option<String> {
        match self
            .fanout_group_task_output_snapshot(task_id, offset, max_bytes)
            .await
        {
            Some(snapshot) => Some(format_background_task_output(task_id, offset, &snapshot)),
            None => {
                if let Some(reason) = miss_reason {
                    tracing::debug!(
                        task_id,
                        reason,
                        "fanout group snapshot fallback did not match task_output id"
                    );
                }
                None
            }
        }
    }

    async fn fanout_group_task_output_snapshot(
        &self,
        task_id: &str,
        offset: u64,
        max_bytes: usize,
    ) -> Option<BgTaskOutputSnapshot> {
        let ctx = self.spawn_context.as_ref()?;
        let group = ctx
            .spawner
            .list_fanout_groups()
            .await
            .into_iter()
            .find(|group| group.group_id == task_id)?;
        let summary = group.summary();
        let terminal = group.is_terminal();
        let status = if terminal {
            if summary.failed > 0 || summary.timed_out > 0 || summary.spawn_rejected > 0 {
                "failed"
            } else {
                "completed"
            }
        } else if summary.active > 0 {
            "running"
        } else {
            "pending"
        };
        // Estimate ~150 bytes per slot JSON entry. Cap serialized slots to avoid
        // allocating hundreds of KB for large fanout groups when the caller only
        // needs a small window.
        const BYTES_PER_SLOT: usize = 150;
        let overhead = 512; // fixed JSON overhead (keys, summary, hint, etc.)
        let max_slots = if max_bytes > overhead {
            ((max_bytes - overhead) / BYTES_PER_SLOT).max(1)
        } else {
            1
        };
        let total_slots = group.slots.len();
        let truncated = total_slots > max_slots;
        let slots_json: Vec<_> = group
            .slots
            .iter()
            .take(max_slots)
            .map(|slot| {
                json!({
                    "slot_index": slot.slot_index,
                    "id": &slot.slot_id,
                    "agent_id": &slot.agent_id,
                    "status": slot.status.as_str(),
                    "result_collected": slot.result_collected,
                    "terminal_reason": &slot.terminal_reason,
                })
            })
            .collect();
        let output = json!({
            "type": "agent_fanout_group",
            "group_id": &group.group_id,
            "title": &group.title,
            "status": status,
            "summary": group.summary_sentence(),
            "target_count": summary.target_count,
            "accepted": summary.accepted,
            "active": summary.active,
            "terminal": summary.terminal,
            "completed": summary.completed,
            "failed": summary.failed,
            "cancelled_by_user": summary.cancelled_by_user,
            "cancelled_by_parent_budget": summary.cancelled_by_parent_budget,
            "timed_out": summary.timed_out,
            "spawn_rejected": summary.spawn_rejected,
            "collected": summary.collected,
            "uncollected": summary.uncollected,
            "slots": slots_json,
            "slots_truncated": if truncated { Some(total_slots) } else { None },
            "hint": "This id belongs to an agent_fanout group, not a shell background task. Use agent_fanout(action='get_results', group_id=...) for full slot results.",
        })
        .to_string();
        let start = output.floor_char_boundary((offset as usize).min(output.len()));
        let end = output.floor_char_boundary((start + max_bytes).min(output.len()));
        let chunk = output[start..end].to_string();
        Some(BgTaskOutputSnapshot {
            kind: "agent fanout".to_string(),
            title: Some(group.title),
            output: chunk,
            end_offset: end as u64,
            total_bytes: output.len() as u64,
            total_lines: output.lines().count() as u64,
            status: status.to_string(),
            terminal,
            output_ref: format!("agent_fanout:{}", group.group_id),
        })
    }

    async fn task_kill_bg(&self, args: &Value) -> String {
        let Some(ref bg_commands) = self.bg_task_commands else {
            return format_background_task_unavailable(self.cloud_base.is_some());
        };
        let task_id = match background_task_id_arg(args) {
            Ok(Some(id)) => id,
            Err(error) => return error.to_string(),
            Ok(None) => return "Task id is required".to_string(),
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut cmds = bg_commands.lock_recover();
            cmds.push(BgTaskCommand::Kill {
                task_id: task_id.clone(),
                reply: tx,
            });
        }
        let reply_timeout = background_task_reply_timeout(BG_TASK_COMMAND_REPLY_TIMEOUT_MS);
        match await_bg_task_command_reply(rx, reply_timeout).await {
            Ok(Ok(())) => format!("Background task {task_id} stopped."),
            Ok(Err(e)) => format_background_task_stop_error(&task_id, &e),
            Err(BgTaskReplyError::Closed) => {
                "Error: background task registry not available".to_string()
            }
            Err(BgTaskReplyError::TimedOut) => {
                format_background_task_stop_registry_timeout(&task_id, reply_timeout)
            }
        }
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

    // ── Timeline tool: unified multi-agent trace ───────────────────────────────

    fn render_session_timeline(&self, args: &Value) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session. Timeline requires a session.".to_string(),
        };
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(50) as usize;
        let agent_filter = args.get("agent_id").and_then(Value::as_str);

        let journal_path = astra_services::session_journal::journal_file_path(&session_id);

        if !journal_path.exists() {
            return format!("Error: journal not found at {}", journal_path.display());
        }

        let events: Vec<serde_json::Value> = match std::fs::read_to_string(&journal_path) {
            Ok(content) => content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(e) => return format!("Error reading journal: {e}"),
        };

        let mut timeline = astra_turn_core::unified_timeline::build_timeline(&events);

        if let Some(filter) = agent_filter {
            timeline.entries.retain(|e| {
                e.agent_id.as_deref() == Some(filter)
                    || matches!(&e.kind,
                        astra_turn_core::unified_timeline::TimelineEntryKind::AgentSpawned { child_agent_id, .. }
                        | astra_turn_core::unified_timeline::TimelineEntryKind::AgentCompleted { child_agent_id, .. }
                        | astra_turn_core::unified_timeline::TimelineEntryKind::AgentFailed { child_agent_id, .. }
                        if child_agent_id.contains(filter)
                    )
                    || e.agent_id.is_none() // always show parent rounds for context
            });
        }

        astra_turn_core::unified_timeline::render_timeline(&timeline, limit)
    }

    // ── Session summary: structured overview of current session ────────────────

    async fn render_session_summary(&self) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        let journal_path = astra_services::session_journal::journal_file_path(&session_id);

        let events: Vec<serde_json::Value> = match std::fs::read_to_string(&journal_path) {
            Ok(content) => content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(_) => return "Error: journal not found.".to_string(),
        };

        let mut turns = 0u32;
        let mut total_tokens_in = 0u64;
        let mut total_tokens_out = 0u64;
        let mut total_rounds = 0u32;
        let mut errors = 0u32;
        let mut agents_spawned = 0u32;
        let mut agents_completed = 0u32;

        for evt in &events {
            let etype = evt.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match etype {
                "turn" => {
                    turns += 1;
                    if let Some(tin) = evt.get("tokens_in").and_then(|v| v.as_u64()) {
                        total_tokens_in += tin;
                    }
                    if let Some(tout) = evt.get("tokens_out").and_then(|v| v.as_u64()) {
                        total_tokens_out += tout;
                    }
                }
                "llm_round" => total_rounds += 1,
                "turn_error" => errors += 1,
                "agent_spawned" => agents_spawned += 1,
                "AgentTerminated" => agents_completed += 1,
                _ => {}
            }
        }

        let mut out = String::from("## Session Summary\n");
        out.push_str(&format!("Session: {session_id}\n"));
        out.push_str(&format!(
            "Turns: {turns} | LLM rounds: {total_rounds} | Errors: {errors}\n"
        ));
        out.push_str(&format!(
            "Tokens: {} in + {} out = {} total\n",
            total_tokens_in,
            total_tokens_out,
            total_tokens_in + total_tokens_out
        ));
        if agents_spawned > 0 {
            out.push_str(&format!(
                "Agents: {agents_spawned} spawned, {agents_completed} completed\n"
            ));
        }
        // Task status nudge: if there is open work, remind the
        // agent to update them (reference-agent parity: proactive nudge).
        match self.task_manager.load_tasks().await {
            Ok(tasks) => {
                let open_tasks: Vec<_> = tasks.iter().filter(|t| t.status.is_open_work()).collect();
                if !open_tasks.is_empty() {
                    out.push_str(&format!("\nOpen tasks: {}\n", open_tasks.len()));
                    for t in open_tasks.iter().take(5) {
                        let status_icon = if t.status.is_in_progress() {
                            "▶"
                        } else if t.status == astra_tools::task_mgmt::SessionTaskStatusKind::Paused
                        {
                            "⏸"
                        } else {
                            "○"
                        };
                        let blocked = if !t.blocked_by.is_empty() {
                            format!(" [blocked by: {}]", t.blocked_by.join(","))
                        } else {
                            String::new()
                        };
                        out.push_str(&format!(
                            "  {status_icon} {} — {}{}\n",
                            t.id, t.title, blocked
                        ));
                    }
                    out.push_str(
                        "Hint: update task status with `task(action=\"update\", task_id=\"...\", new_status=\"...\")` as you make progress.\n",
                    );
                }
            }
            Err(error) => {
                out.push_str(&format!(
                    "\nTask board unavailable: {error}\n\
                     Do not assume there are no open tasks; retry `task(action=\"list\")` before creating duplicate work.\n"
                ));
            }
        }
        out
    }

    // ── Session history: recall past conversation turns ──────────────────────

    fn render_session_history(&self, args: &Value) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
        let query = args.get("query").and_then(Value::as_str).unwrap_or("");

        let journal_path = astra_services::session_journal::journal_file_path(&session_id);

        let events: Vec<serde_json::Value> = match std::fs::read_to_string(&journal_path) {
            Ok(content) => content
                .lines()
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect(),
            Err(_) => return "Error: journal not found.".to_string(),
        };

        // Extract turn events with user_input + assistant_output
        let mut turns: Vec<(u32, &str, &str)> = Vec::new();
        for evt in &events {
            if evt.get("type").and_then(|v| v.as_str()) != Some("turn") {
                continue;
            }
            let turn_num = evt.get("turn").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let user = evt.get("user_input").and_then(|v| v.as_str()).unwrap_or("");
            let assistant = evt
                .get("assistant_output")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !user.is_empty() || !assistant.is_empty() {
                turns.push((turn_num, user, assistant));
            }
        }

        // Filter by query if provided
        let filtered: Vec<&(u32, &str, &str)> = if query.is_empty() {
            turns.iter().collect()
        } else {
            let q_lower = query.to_lowercase();
            turns
                .iter()
                .filter(|(_, u, a)| {
                    u.to_lowercase().contains(&q_lower) || a.to_lowercase().contains(&q_lower)
                })
                .collect()
        };

        if filtered.is_empty() {
            return if query.is_empty() {
                "No conversation history in this session.".to_string()
            } else {
                format!("No turns matching '{query}' found.")
            };
        }

        // Take last N
        let shown: &[&(u32, &str, &str)] = if filtered.len() > limit {
            &filtered[filtered.len() - limit..]
        } else {
            &filtered
        };

        let mut out = format!(
            "## Conversation History ({} turns{})\n",
            filtered.len(),
            if !query.is_empty() {
                format!(" matching '{query}'")
            } else {
                String::new()
            }
        );
        for (turn_num, user, assistant) in shown {
            let user_preview: String = user.chars().take(120).collect();
            let assist_preview: String = assistant.chars().take(200).collect();
            out.push_str(&format!("\n**T{turn_num} User**: {user_preview}"));
            if user.len() > 120 {
                out.push('…');
            }
            out.push_str(&format!("\n**T{turn_num} Assistant**: {assist_preview}"));
            if assistant.len() > 200 {
                out.push('…');
            }
            out.push('\n');
        }
        out
    }

    // ── Memory suppress ────────────────────────────────────────────────────────

    /// Suppress a Memoria `memory_id` for the active session.
    ///
    /// `memory_id` is the exact id shown by memory recall/search results.
    /// The optional `reason` is written to the session journal. Suppression
    /// only affects prompt injection for this session; it does not delete the
    /// memory from Memoria.
    fn suppress_memory(&self, args: &Value) -> String {
        let memory_id = args.get("memory_id").and_then(Value::as_str).unwrap_or("");
        if memory_id.is_empty() {
            return "Error: missing required parameter `memory_id`.".to_string();
        }
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        let reason = args.get("reason").and_then(Value::as_str);
        astra_tools::memoria::MemoriaClient::suppress_memory(&session_id, memory_id);
        let turn = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        crate::cli::cli_config::cli_utils::append_session_journal_event_or_warn(
            &session_id,
            &astra_services::session_journal::JournalEvent::memory_suppressed(
                Some(&session_id),
                turn,
                memory_id,
                reason,
            ),
            "edge_tools:suppress_memory",
        );
        format!(
            "Memory `{mid}` suppressed for this session. It will not be injected in future turns.",
            mid = memory_id
        )
    }

    /// Remove a Memoria `memory_id` from the active session suppress list.
    fn unsuppress_memory(&self, args: &Value) -> String {
        let memory_id = args.get("memory_id").and_then(Value::as_str).unwrap_or("");
        if memory_id.is_empty() {
            return "Error: missing required parameter `memory_id`.".to_string();
        }
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        astra_tools::memoria::MemoriaClient::unsuppress_memory(&session_id, memory_id);
        format!("Memory `{mid}` unsuppressed.", mid = memory_id)
    }

    /// List Memoria `memory_id` values suppressed in the active session.
    fn list_suppressed_memories(&self) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        let suppressed = astra_tools::memoria::MemoriaClient::suppressed_snapshot(&session_id);
        if suppressed.is_empty() {
            return "No memories are suppressed in this session.".to_string();
        }
        let mut out = format!("## Suppressed memories ({} total)\n", suppressed.len());
        for id in &suppressed {
            out.push_str(&format!("- {id}\n"));
        }
        out
    }

    // ── Context release ──────────────────────────────────────────────────────────

    /// Release one or more tool results from future LLM context.
    ///
    /// `tool_call_id` accepts a string or array of strings copied from tool
    /// result metadata. Released results remain in the journal, but their
    /// content is replaced with a short stub before the next LLM request.
    fn release_context(&self, args: &Value) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        // Accept single ID or array of IDs. `tool_call_ids` (plural) is
        // accepted as an alias so a typo in the agent prompt doesn't silently
        // fall through to the missing-parameter error.
        let raw = args
            .get("tool_call_id")
            .or_else(|| args.get("tool_call_ids"));
        let ids: Vec<String> = if let Some(arr) = raw.and_then(Value::as_array) {
            arr.iter()
                .filter_map(Value::as_str)
                .map(String::from)
                .collect()
        } else if let Some(id) = raw.and_then(Value::as_str) {
            vec![id.to_string()]
        } else {
            return "Error: missing required parameter `tool_call_id` (string or array)."
                .to_string();
        };
        if ids.is_empty() {
            return "Error: `tool_call_id` must not be empty.".to_string();
        }
        for id in &ids {
            astra_tools::memoria::MemoriaClient::release_context(&session_id, id);
        }
        let turn = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        let id_refs: Vec<&str> = ids.iter().map(String::as_str).collect();
        crate::cli::cli_config::cli_utils::append_session_journal_event_or_warn(
            &session_id,
            &astra_services::session_journal::JournalEvent::context_released(
                Some(&session_id),
                turn,
                &id_refs,
            ),
            "edge_tools:release_context",
        );
        format!(
            "Released {} tool result(s). They will be stubbed on the next LLM call.",
            ids.len()
        )
    }

    /// List tool_call_id values marked for context release in this session.
    fn list_released_context(&self) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        let released = astra_tools::memoria::MemoriaClient::released_snapshot(&session_id);
        if released.is_empty() {
            return "No tool results are released in this session.".to_string();
        }
        let mut out = format!("## Released context ({} tool_call_ids)\n", released.len());
        for id in &released {
            out.push_str(&format!("- {id}\n"));
        }
        out
    }

    // ── Env tool: environment variable management ─────────────────────────────

    /// Environment variable management tool — delegated to `env_tools` module.
    fn env_tool(&self, args: &Value) -> String {
        env_tools::env_tool(args)
    }

    fn handle_introspect(&self, args: &Value) -> String {
        if args.get("dimension").and_then(Value::as_str) == Some("capability") {
            return self.capability_info_json().to_string();
        }

        // `subtopic` routes to a specialized diagnostic. Default behavior
        // (session health: token pressure, tool health, alerts) remains
        // unchanged when `subtopic` is missing, empty, or "session".
        let subtopic = args
            .get("subtopic")
            .and_then(Value::as_str)
            .map(str::trim)
            .unwrap_or("")
            .to_ascii_lowercase();
        if subtopic == "cache" {
            return self.handle_introspect_cache();
        }

        let detail_arg = args
            .get("detail")
            .and_then(Value::as_str)
            .unwrap_or("summary");
        let detail = astra_turn_core::introspect::IntrospectDetail::from_arg(detail_arg);

        let snapshot = self
            .introspect_snapshot
            .read()
            .unwrap_or_else(|poisoned| {
                astra_core::agent_warn!("introspect", "recovering from poisoned RwLock");
                poisoned.into_inner()
            })
            .clone();

        // Fall back to a zero-state snapshot on the first turn (before the
        // host has had a chance to populate one) so the model always gets
        // structured output instead of an opaque "first turn" string.
        let mut snap = snapshot.unwrap_or_default();
        if snap.current_model.is_none() {
            snap.current_model = self.current_model();
        }

        // Overlay session-scoped injection freshness. The per-turn
        // snapshot lives on `AgenticLoopState` (not session) so the
        // runtime leaves this empty; we fill it here from the
        // session-scoped history maintained via `observe_bridge_injections`.
        // Subtopic-agnostic: `render_all` and `noise` both need it.
        if let Some(obs) = self.observability_session.as_ref()
            && let Ok(s) = obs.read()
        {
            snap.injection_freshness = astra_turn_core::injection_tracking::freshness_report(
                &s.injection_history,
                s.turn_number,
            );
            snap.current_round = s.turn_number;
        }

        // Task #46: three new subtopics for fine-grained runtime
        // self-awareness. All read from `IntrospectSnapshot`, which the
        // runtime populates every turn — no disk I/O required.
        match subtopic.as_str() {
            "recent" | "recent_rounds" | "rounds" => {
                return astra_turn_core::introspect::render_recent_rounds(&snap);
            }
            "volatile" | "volatile_pending" | "pending" => {
                return astra_turn_core::introspect::render_volatile_pending(&snap);
            }
            "stall" | "stall_state" | "loop_guard" => {
                return astra_turn_core::introspect::render_stall_state(&snap);
            }
            "noise" | "injection" | "injections" | "freshness" => {
                return astra_turn_core::introspect::render_injection_freshness(&snap);
            }
            "session_memory" | "session-memory" | "memory" | "extraction" | "extractions" => {
                return self.render_session_memory_introspect();
            }
            "errors" | "tool_errors" | "failures" => {
                return astra_turn_core::introspect::render_errors(&snap);
            }
            "all" => {
                return astra_turn_core::introspect::render_all(&snap);
            }
            _ => {}
        }

        astra_turn_core::introspect::render_introspect(&snap, detail)
    }

    /// Render `introspect subtopic=session_memory`. Answers the
    /// recurring question "what did astra extract this session, and
    /// what did the last compaction inject?" without dumping enough
    /// content to pressure context.
    ///
    /// Output cap: the last 8 extractions + last 4 injections are
    /// included verbatim; older rings are summarised as counts. Each
    /// record renders in one line plus an optional short preview,
    /// keeping the total under ~400 tokens even when both rings are
    /// full.
    fn render_session_memory_introspect(&self) -> String {
        use std::fmt::Write as _;

        let surface_status = self
            .active_session_id()
            .filter(|sid| !sid.is_empty())
            .map(|sid| {
                let record = crate::cli::slash::slash_memory::load_local_session_memory(&sid);
                crate::cli::slash::slash_memory::session_memory_surface_status(
                    &sid,
                    record.as_ref(),
                )
            });
        let surface_block = surface_status
            .as_ref()
            .map(crate::cli::slash::slash_memory::render_session_memory_surface_status)
            .filter(|block| !block.trim().is_empty());
        let (journal_fallback, journal_pipeline, journal_notice) =
            match self.load_active_session_memory_journal() {
                Ok(Some((session_id, events))) => (
                    Self::render_session_memory_journal_fallback(&session_id, &events),
                    Self::render_session_memory_pipeline_traces(&events),
                    None,
                ),
                Ok(None) => (None, None, None),
                Err(error) => (None, None, Some(format!("journal unavailable: {error}"))),
            };
        let Some(obs) = self.session_memory_observatory.as_ref() else {
            let body = journal_fallback.unwrap_or_else(|| {
                "# session-memory observatory\n\n\
                     No observatory attached to this runtime. This is expected \
                     for offline CLI or legacy test modes; production servers \
                     attach one so extractions + injections are traceable here."
                    .to_string()
            });
            let body = journal_notice
                .as_deref()
                .map(|notice| Self::inject_session_memory_notice(&body, notice))
                .unwrap_or(body);
            return Self::prepend_session_memory_surface_status(surface_block.as_deref(), &body);
        };

        let ext = obs.extractions_snapshot();
        let inj = obs.injections_snapshot();
        if ext.is_empty()
            && inj.is_empty()
            && let Some(fallback) = journal_fallback
        {
            return Self::prepend_session_memory_surface_status(
                surface_block.as_deref(),
                &fallback,
            );
        }
        let mut out = String::from("# session-memory observatory\n\n");
        if let Some(block) = surface_block.as_deref() {
            writeln!(out, "{block}\n").ok();
        }
        if let Some(notice) = journal_notice.as_deref() {
            writeln!(out, "{notice}\n").ok();
        }

        writeln!(
            out,
            "extractions_ring: {} / {}    injections_ring: {} / {}\n",
            ext.len(),
            astra_runtime::session_memory::observatory::EXTRACTION_RING_CAPACITY,
            inj.len(),
            astra_runtime::session_memory::observatory::INJECTION_RING_CAPACITY,
        )
        .ok();

        // ── Extractions: last 8, newest-last ────────────────────────
        out.push_str("## extractions (newest last)\n");
        if ext.is_empty() {
            out.push_str("(none recorded this session)\n\n");
        } else {
            let tail = ext.iter().rev().take(8).collect::<Vec<_>>();
            for rec in tail.iter().rev() {
                writeln!(
                    out,
                    "- t{turn} {trigger:?} {outcome} model={model} sections={sections:?}",
                    turn = rec.turn,
                    trigger = rec.trigger,
                    outcome = render_extraction_outcome_label(&rec.outcome),
                    model = rec.selector_model.as_deref().unwrap_or("-"),
                    sections = rec.narrative_sections,
                )
                .ok();
                if !rec.content_preview.is_empty() {
                    // Preview is already capped to PREVIEW_CHAR_CAP by
                    // the observatory; further truncate for display.
                    let line = rec.content_preview.replace('\n', " ⏎ ");
                    let short: String = line.chars().take(120).collect();
                    writeln!(out, "    preview: {short}").ok();
                }
            }
            if ext.len() > 8 {
                writeln!(out, "… ({} older records elided)", ext.len() - 8).ok();
            }
            out.push('\n');
        }

        // ── Injections: last 4, newest-last ─────────────────────────
        out.push_str("## compaction injections (newest last)\n");
        if inj.is_empty() {
            out.push_str("(none recorded this session)\n");
        } else {
            let tail = inj.iter().rev().take(4).collect::<Vec<_>>();
            for rec in tail.iter().rev() {
                writeln!(
                    out,
                    "- t{turn} level={level:?} pressure={pressure:.2} chars={chars} plan={cmp}/{tot} files={files} errs={errs} staleness={stale}",
                    turn = rec.turn,
                    level = rec.level,
                    pressure = rec.pressure,
                    chars = rec.injected_chars,
                    cmp = rec.facts_summary.plan_completed,
                    tot = rec.facts_summary.plan_total,
                    files = rec.facts_summary.active_files_count,
                    errs = rec.facts_summary.error_count,
                    stale = render_staleness(&rec.staleness),
                )
                .ok();
                if !rec.narrative_sections_kept.is_empty() {
                    writeln!(
                        out,
                        "    narrative_sections: {:?}",
                        rec.narrative_sections_kept
                    )
                    .ok();
                }
                if !rec.retrieved_memories.is_empty() {
                    let mems = rec
                        .retrieved_memories
                        .iter()
                        .map(|m| {
                            let id_short: String = m.memory_id.chars().take(8).collect();
                            let score_s = m.score.map(|s| format!("={s:.2}")).unwrap_or_default();
                            let content_s = m
                                .content_preview
                                .as_deref()
                                .map(|c| {
                                    let short: String =
                                        c.replace('\n', " ").chars().take(60).collect();
                                    format!(" \"{short}\"")
                                })
                                .unwrap_or_default();
                            format!("{}[{}]{}{}", m.memory_type, id_short, score_s, content_s,)
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    writeln!(out, "    retrieved: {mems}").ok();
                }
            }
            if inj.len() > 4 {
                writeln!(out, "… ({} older records elided)", inj.len() - 4).ok();
            }
        }

        if let Some(pipeline) = journal_pipeline {
            writeln!(out, "\n{pipeline}").ok();
        }

        out
    }

    fn load_active_session_memory_journal(
        &self,
    ) -> Result<Option<(String, Vec<astra_services::session_journal::JournalEvent>)>, String> {
        let Some(session_id) = self.active_session_id().filter(|sid| !sid.is_empty()) else {
            return Ok(None);
        };
        let events = astra_services::session_journal::read_journal(&session_id)
            .map_err(|error| format!("failed to read session journal for {session_id}: {error}"))?;
        Ok(Some((session_id, events)))
    }

    fn render_session_memory_journal_fallback(
        session_id: &str,
        events: &[astra_services::session_journal::JournalEvent],
    ) -> Option<String> {
        use std::fmt::Write as _;

        if events.is_empty() {
            return None;
        }

        let extractions: Vec<_> = events
            .iter()
            .filter(|event| {
                event.event_type
                    == astra_services::session_journal::JournalEventType::SessionMemoryExtraction
            })
            .collect();
        let session_end = events.iter().any(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::SessionEnd
        });
        let last_turn_error = events.iter().rev().find(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::TurnError
        });
        let last_cache_break = events.iter().rev().find(|event| {
            event.event_type == astra_services::session_journal::JournalEventType::PipelineAlert
                && event
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("alert_rule"))
                    .and_then(serde_json::Value::as_str)
                    == Some("prompt_cache_break")
        });

        let mut out = String::from("# session-memory observatory\n\n");
        writeln!(out, "source: local_journal").ok();
        writeln!(out, "session_id: {session_id}").ok();
        writeln!(
            out,
            "session_end: {}",
            if session_end { "present" } else { "missing" }
        )
        .ok();

        if extractions.is_empty() {
            out.push_str("\n## extraction journal\n(none recorded in local journal)\n");
        } else {
            let extracted = extractions
                .iter()
                .filter(|event| {
                    event
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("outcome"))
                        .and_then(serde_json::Value::as_str)
                        == Some("extracted")
                })
                .count();
            let skipped = extractions
                .iter()
                .filter(|event| {
                    event
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("outcome"))
                        .and_then(serde_json::Value::as_str)
                        == Some("skipped")
                })
                .count();
            let errored = extractions
                .iter()
                .filter(|event| {
                    event
                        .metadata
                        .as_ref()
                        .and_then(|m| m.get("outcome"))
                        .and_then(serde_json::Value::as_str)
                        == Some("errored")
                })
                .count();
            writeln!(
                out,
                "\nextractions_journal: {}  extracted={} skipped={} errored={}",
                extractions.len(),
                extracted,
                skipped,
                errored
            )
            .ok();
            out.push_str("\n## extraction journal (newest last)\n");
            for event in extractions
                .iter()
                .rev()
                .take(8)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                let meta = event.metadata.as_ref();
                let outcome = meta
                    .and_then(|m| m.get("outcome"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?");
                let reason = meta
                    .and_then(|m| m.get("reason"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-");
                let source = meta
                    .and_then(|m| m.get("source"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-");
                let model = meta
                    .and_then(|m| m.get("selector_model"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("-");
                let llm_reason = meta
                    .and_then(|m| m.get("llm_reason"))
                    .and_then(serde_json::Value::as_str);
                let llm_detail = meta
                    .and_then(|m| m.get("llm_detail"))
                    .and_then(serde_json::Value::as_str);
                let messages = meta
                    .and_then(|m| m.get("messages_count"))
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                let mut line = format!("- t{} {}", event.turn.unwrap_or(0), outcome,);
                if source != "-" {
                    line.push_str(&format!(" source={source}"));
                }
                if reason != "-" {
                    line.push_str(&format!(" reason={reason}"));
                }
                if let Some(llm_reason) = llm_reason {
                    line.push_str(&format!(" llm_reason={llm_reason}"));
                }
                if let Some(llm_detail) = llm_detail {
                    line.push_str(&format!(" llm_detail={llm_detail}"));
                }
                line.push_str(&format!(" model={model} messages={messages}"));
                writeln!(out, "{line}").ok();
            }
            if extractions.len() > 8 {
                writeln!(out, "… ({} older records elided)", extractions.len() - 8).ok();
            }
        }

        if let Some(event) = last_turn_error {
            let error = event.error.as_deref().unwrap_or("-");
            writeln!(
                out,
                "\nlast_turn_error: t{} {}",
                event.turn.unwrap_or(0),
                error
            )
            .ok();
        }
        if let Some(event) = last_cache_break {
            let msg = event
                .metadata
                .as_ref()
                .and_then(|m| m.get("alert_message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("prompt cache break");
            writeln!(out, "last_cache_alert: {msg}").ok();
        }
        if !session_end {
            out.push_str(
                "\nnote: session_end is missing, so any shutdown-time session-memory flush did not run.\n",
            );
        }
        if let Some(pipeline) = Self::render_session_memory_pipeline_traces(&events) {
            writeln!(out, "\n{pipeline}").ok();
        }
        Some(out)
    }

    fn render_session_memory_pipeline_traces(
        events: &[astra_services::session_journal::JournalEvent],
    ) -> Option<String> {
        use std::fmt::Write as _;

        let traces: Vec<_> = events
            .iter()
            .filter(|event| {
                event.event_type
                    == astra_services::session_journal::JournalEventType::ContextAssemblyRecorded
            })
            .filter_map(Self::render_session_memory_pipeline_trace_line)
            .collect();
        if traces.is_empty() {
            return None;
        }

        let mut out = String::from("## turn-pipeline session memory (newest last)\n");
        let tail = traces.iter().rev().take(6).collect::<Vec<_>>();
        for line in tail.into_iter().rev() {
            writeln!(out, "{line}").ok();
        }
        if traces.len() > 6 {
            writeln!(out, "… ({} older records elided)", traces.len() - 6).ok();
        }
        Some(out)
    }

    fn render_session_memory_pipeline_trace_line(
        event: &astra_services::session_journal::JournalEvent,
    ) -> Option<String> {
        let trace = event.context_assembly_trace.as_ref()?;
        let system_prompt = trace.get("system_prompt")?;
        let session_memory = system_prompt.get("session_memory_injected");
        let selected = trace
            .get("memory")
            .and_then(|memory| memory.get("memories_selected"))
            .and_then(serde_json::Value::as_array)
            .map(|entries| entries.len())
            .unwrap_or(0);
        let turn = event.turn.unwrap_or(0);

        let Some(session_memory) = session_memory.filter(|value| !value.is_null()) else {
            return Some(format!(
                "- t{turn} session_memory=absent retrieved_memories={selected}"
            ));
        };

        let source = session_memory
            .get("memory_type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("-");
        let tokens = session_memory
            .get("tokens")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let preview = session_memory
            .get("content_preview")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .replace('\n', " ⏎ ");
        let preview: String = preview.chars().take(80).collect();

        Some(format!(
            "- t{turn} session_memory=present source={source} tokens={tokens} retrieved_memories={selected} preview=\"{preview}\""
        ))
    }

    fn prepend_session_memory_surface_status(surface_block: Option<&str>, body: &str) -> String {
        match surface_block.filter(|block| !block.trim().is_empty()) {
            Some(block) => format!("{block}\n\n{body}"),
            None => body.to_string(),
        }
    }

    fn inject_session_memory_notice(body: &str, notice: &str) -> String {
        const HEADER: &str = "# session-memory observatory\n\n";
        if let Some(rest) = body.strip_prefix(HEADER) {
            format!("{HEADER}{notice}\n\n{rest}")
        } else {
            format!("{notice}\n\n{body}")
        }
    }

    /// `introspect(subtopic="cache")` — scan recent `llm_capture_*.json`
    /// files for the current session and run the four cache-diagnosis
    /// rules over them. Returns a markdown report.
    ///
    /// Requires `full_llm_capture=true` in session metadata; otherwise
    /// the renderer explains why no data is available so the LLM knows
    /// how to enable it. A future task (#17) adds an in-memory per-turn
    /// ring so diagnosis also works without full capture.
    fn handle_introspect_cache(&self) -> String {
        use astra_turn_core::introspect::cache_diagnosis;
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                return cache_diagnosis::render_findings_markdown(&[], &[]);
            }
        };
        let session_dir = std::path::PathBuf::from(
            std::env::var("HOME")
                .ok()
                .or_else(|| dirs::home_dir().map(|p| p.to_string_lossy().into_owned()))
                .unwrap_or_else(|| ".".to_string()),
        )
        .join(".astra")
        .join("sessions")
        .join(&session_id);
        let rounds = match cache_diagnosis::load_session_captures(&session_dir) {
            Ok(rs) => rs,
            Err(e) => {
                astra_core::agent_warn!(
                    "introspect",
                    "cache diagnosis: failed to read session dir {}: {e}",
                    session_dir.display(),
                );
                Vec::new()
            }
        };
        let findings = cache_diagnosis::evaluate_all(&rounds);
        cache_diagnosis::render_findings_markdown(&rounds, &findings)
    }

    pub fn update_introspect_snapshot(
        &self,
        snapshot: astra_turn_core::introspect::IntrospectSnapshot,
    ) {
        if let Ok(mut guard) = self.introspect_snapshot.write() {
            *guard = Some(snapshot);
        }
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
    ///
    /// Validates the path before mutating the policy. Rejects:
    /// - non-absolute paths ([`SandboxExpansionError::NotAbsolute`])
    /// - the filesystem root `/` ([`SandboxExpansionError::RootPath`])
    /// - parent-dir (`..`) traversal escapes ([`SandboxExpansionError::TraversalEscape`])
    /// - system-sensitive directories and credential paths
    ///   ([`SandboxExpansionError::SystemSensitivePath`])
    ///
    /// This is defense-in-depth: most call paths pre-validate via
    /// `sandbox_retry::checked_expand_path`, but this gate ensures
    /// user-supplied inputs (e.g. `--add-dir`, `ASTRA_CLI_ADD_DIRS`)
    /// cannot open `/` or `/etc`.
    pub fn expand_sandbox_path(&self, dir: PathBuf) -> Result<PathBuf, SandboxExpansionError> {
        if !dir.is_absolute() {
            return Err(SandboxExpansionError::NotAbsolute);
        }
        if dir == Path::new("/") {
            return Err(SandboxExpansionError::RootPath);
        }
        if dir
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(SandboxExpansionError::TraversalEscape);
        }
        if astra_sandbox::is_sensitive_system_dir(&dir)
            || astra_sandbox::is_never_readable_path(&dir)
        {
            return Err(SandboxExpansionError::SystemSensitivePath);
        }
        if let Ok(mut guard) = self.sandbox_policy.write() {
            match *guard {
                Some(ref mut policy) => {
                    policy.allowed_paths.push(dir.clone());
                }
                None => return Err(SandboxExpansionError::NoSandboxPolicy),
            }
        } else {
            return Err(SandboxExpansionError::PolicyLockPoisoned);
        }
        Ok(dir)
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
        let persisted = self.maybe_persist_large_output(output, name);
        truncate_output(persisted, global_output_limit())
    }

    pub async fn execute_with_metadata_cancelable(
        &self,
        name: &str,
        args: &Value,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> ToolExecutionOutcome {
        // Admission gate (fail-closed). Every public execution entry point
        // must pass through `tool_admission_denial` before any tool runs —
        // including the cancel-aware shell path and metadata-tagged paths,
        // which otherwise bypass `execute_run`'s gate.
        if let Some(denied) = self.tool_admission_denial(name, args) {
            return denied.into_outcome();
        }
        if let Some(outcome) = self.execute_blocking_shell_tool(name, args, cancel_token) {
            return outcome;
        }
        self.execute_with_metadata(name, args).await
    }

    /// Synchronous core for `bash` / `powershell` execution. Returns
    /// `Some(outcome)` when `name` matches a cancel-aware shell tool, allowing
    /// the caller to invoke this directly inside `tokio::task::spawn_blocking`
    /// without re-entering the runtime via `Handle::block_on`. Returns `None`
    /// for any other tool, signaling that the caller should fall back to the
    /// generic async path.
    pub fn execute_blocking_shell_tool(
        &self,
        name: &str,
        args: &Value,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
    ) -> Option<ToolExecutionOutcome> {
        // Deferred activation must be consumed exactly once per tool call.
        // The other public entry points (execute_with_metadata, execute) also
        // call consume, but they do NOT call this function — so this is the
        // only consume site for shell-tool paths.
        self.consume_activated_deferred_tool_if_called(name);
        if name == "bash" {
            let output = self.finalize_tool_output(self.bash_with_cancel(args, cancel_token), name);
            self.record_output_size(output.len());
            return Some(tool_execution_outcome_from_output(output));
        }
        #[cfg(windows)]
        if name == "powershell" {
            let output =
                self.finalize_tool_output(self.powershell_with_cancel(args, cancel_token), name);
            self.record_output_size(output.len());
            return Some(tool_execution_outcome_from_output(output));
        }
        #[cfg(not(windows))]
        let _ = cancel_token; // powershell branch is the only other cancel-aware tool
        None
    }

    pub async fn execute_with_metadata(&self, name: &str, args: &Value) -> ToolExecutionOutcome {
        // Admission gate (fail-closed). This is a public entry point called
        // directly by the server executor; without this gate, `mo_query`, `mo`,
        // and `git` metadata-tagged paths would bypass `execute_run`'s gate.
        if let Some(denied) = self.tool_admission_denial(name, args) {
            return denied.into_outcome();
        }
        self.consume_activated_deferred_tool_if_called(name);
        if name == "mo_query" {
            let mut outcome = self.mo_query_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        // Consolidated `mo` tool (action=query|snapshot|branch).
        // Only `query` has a metadata path — snapshot/branch fall
        // through to `execute()` which dispatches without metadata.
        if name == "mo" && args.get("action").and_then(Value::as_str) == Some("query") {
            let mut outcome = self.mo_query_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        if name == "git" {
            let action = args
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or("status");
            match action {
                "commit" => {
                    let mut outcome = self.git_commit_with_metadata(args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                "revert_commit" => {
                    let mut outcome = self.git_revert_commit_with_metadata(args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                "stash" => {
                    let stash_args = git_stash_action_args(args);
                    let mut outcome = self.git_stash_with_metadata(&stash_args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                "worktree" => {
                    let mut outcome = self.git_worktree_with_metadata(args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                _ => {} // Other git actions handled in execute() below
            }
        }
        self.execute_run(name, args).await.into_outcome()
    }

    pub async fn execute(&self, name: &str, args: &Value) -> String {
        self.consume_activated_deferred_tool_if_called(name);
        self.execute_run(name, args).await.output
    }

    async fn execute_run(&self, name: &str, args: &Value) -> EdgeToolRun {
        if let Some(error) = self.tool_admission_denial(name, args) {
            return error;
        }
        let output = self.execute_raw(name, args).await;
        // Structural error propagation: `execute_raw` returns a plain String,
        // discarding any structured error kind at the source. Recover it here
        // so downstream `tool_work_surface_events` can route on `error_kind`
        // metadata instead of re-deriving it from fragile string matching.
        if cli_tool_output_is_error(&output) {
            let kind = astra_core::classify_tool_output(&output);
            EdgeToolRun::classified_error(output, kind)
        } else {
            EdgeToolRun::ok(output)
        }
    }

    async fn execute_raw(&self, name: &str, args: &Value) -> String {
        let output = if let Err(error) =
            crate::tool_safety_guard::ToolSafetyGuard::check_dispatch(name, args)
        {
            error
        } else if is_plan_mode_blocked_tool(name, args) && self.plan_mode_authoring_active().await {
            format!(
                "Error: Tool '{name}' is blocked while plan mode is active. \
                 The agent must call `exit_plan_mode` with an approved plan \
                 before any write operation. This mirrors the reference agent's plan \
                 mode: the plan is authored with read-only tools, approved by \
                 the user, then execution proceeds with writes unlocked."
            )
        } else {
            match name {
                "bash" => self.bash_async(args).await,
                #[cfg(windows)]
                "powershell" => self.powershell(args),
                // Activation primitive for the deferred tool layer.
                // Uses the local CLI catalog plus plugin-installed schemas,
                // so `select:NAME` matches the tools this surface actually
                // exposes while still resolving MCP/skill-backed tools.
                "tool_search" => {
                    let output = self.tool_search(args);
                    self.record_tool_search_activation_output(&output);
                    output
                }
                "read_file" => self.read_file(args),
                "write_file" => {
                    // delete=true routes to delete_file handler
                    if args.get("delete").and_then(Value::as_bool).unwrap_or(false) {
                        self.delete_file(args)
                    } else {
                        self.write_file(args)
                    }
                }
                "rollback_database_snapshots" => self.rollback_database_snapshots(args),
                "rollback_session_state" => self.rollback_session_state(args).await,
                "rollback_turn_actions" => self.rollback_turn_actions(args).await,
                "str_replace" => {
                    let args = match astra_tools::fs_ops::normalize_str_replace_args(args) {
                        Ok(args) => args,
                        Err(error) => return error,
                    };
                    // edits array routes through the str_replace batch
                    // wrapper so both same-file and per-edit path batches
                    // share one contract.
                    if args.get("edits").and_then(Value::as_array).is_some() {
                        self.str_replace_batch(&args)
                    } else {
                        self.str_replace(&args)
                    }
                }
                "list_dir" => self.list_dir(args),
                "grep" => self.grep(args),
                "glob" => self.glob(args),
                "git" => {
                    let action = args
                        .get("action")
                        .and_then(Value::as_str)
                        .unwrap_or("status");
                    match action {
                        "status" => git_gix::git_status(&self.project_root, args),
                        "diff" => git_gix::git_diff(
                            &self.project_root,
                            args,
                            self.get_budget_pressure(),
                            self.aggregate_output_bytes
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        "log" => git_gix::git_log(&self.project_root, args),
                        "show" => git_gix::git_show(
                            &self.project_root,
                            args,
                            self.get_budget_pressure(),
                            self.aggregate_output_bytes
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        "blame" => git_gix::git_blame(&self.project_root, args),
                        "file_history" => git_gix::git_file_history(&self.project_root, args),
                        "log_search" => git_gix::git_log_search(&self.project_root, args),
                        "contributors" => git_gix::git_contributors(&self.project_root, args),
                        "commit" => self.git_commit(args),
                        "revert_commit" => self.git_revert_commit(args),
                        "stash" => {
                            let stash_args = git_stash_action_args(args);
                            self.git_stash(&stash_args)
                        }
                        "checkout_file" => self.git_checkout_file(args),
                        "worktree" => self.git_worktree(args),
                        "push" => git_gix::git_push(&self.project_root, args),
                        _ => format!(
                            "Error: unknown git action '{action}'. Use one of: status, diff, log, show, blame, file_history, log_search, contributors, commit, revert_commit, stash, checkout_file, worktree, push"
                        ),
                    }
                }
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
                // Consolidated `mo` tool (matches the schema in
                // astra-tools/schemas.rs). Routes by `action` to the
                // existing legacy handlers. Without this arm, calls
                // to `mo` fall to DefaultToolExecutor which doesn't
                // know about it either — the tool was effectively
                // dead-wired. Per-action required fields (sql for
                // query, sub_action for snapshot/branch) are
                // enforced by the schema's `allOf` block.
                "mo" => {
                    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                    match action {
                        "query" => self.mo_query(args),
                        "snapshot" => self.mo_snapshot(args),
                        "branch" => self.mo_branch(args),
                        "" => "Error: missing required parameter 'action'. Use one of: query, snapshot, branch".to_string(),
                        other => format!(
                            "Error: unknown mo action '{other}'. Use one of: query, snapshot, branch"
                        ),
                    }
                }
                "github" => {
                    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                    match action {
                        "list_prs" => self.github_list_prs(args).await,
                        "get_pr" => self.github_get_pr(args).await,
                        "ci_status" => self.github_ci_status(args).await,
                        "list_issues" => self.github_list_issues(args).await,
                        "get_issue" => self.github_get_issue(args).await,
                        "repo_stats" => self.github_repo_stats(args).await,
                        "create_issue" => self.github_create_issue(args).await,
                        "" => "Error: missing required parameter 'action'. Use one of: list_prs, get_pr, ci_status, list_issues, get_issue, repo_stats, create_issue".to_string(),
                        other => format!(
                            "Error: unknown github action '{other}'. Use one of: list_prs, get_pr, ci_status, list_issues, get_issue, repo_stats, create_issue"
                        ),
                    }
                }
                "web_fetch" => {
                    let cache_scope = self
                        .active_session_id
                        .lock()
                        .ok()
                        .and_then(|guard| guard.clone())
                        .unwrap_or_else(|| self.project_root.to_string_lossy().to_string());
                    astra_tools::web_fetch::fetch_with_cache_scope(None, args, &cache_scope).await
                }
                "memory" => {
                    let op = match args.get("action").and_then(|v| v.as_str()) {
                        Some(a) => a,
                        None => return "Error: missing required parameter `action`. \
                             Use one of: remember, recall, expand, forget, update, focus, reflect, profile, feedback".to_string(),
                    };
                    let clean_args = self.memory_args_with_context(args);
                    self.memoria_call(op, &clean_args).await
                }
                "enter_plan_mode" => self.enter_plan_mode_remote(args).await,
                "exit_plan_mode" => self.exit_plan_mode_remote(args).await,
                "adjust_config" => self.adjust_config(args),
                "prioritize_tool" => self.prioritize_tool(args),
                "deprioritize_tool" => self.deprioritize_tool(args),
                "compress_context" => self.compress_context(args),
                "get_agent_info" => self.get_agent_info(args).await,
                "reflect" => {
                    let focus = args.get("focus").and_then(|v| v.as_str()).unwrap_or("auto");
                    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    let last_n = args.get("last_n").and_then(|v| v.as_i64()).unwrap_or(20);
                    if let Some(session_id) = self.active_session_id().filter(|id| !id.is_empty()) {
                        let limit = usize::try_from(last_n.max(1)).unwrap_or(20);
                        match crate::cli::self_command::render_reflect_surface_for_session(
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
                // ── Consolidated agent tool ──────────────────────────────────
                "agent" => {
                    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                    match action {
                        // `delegate` was a placeholder action that returned a
                        // fake-success acknowledgement string while spawning
                        // nothing — the CLI never wired a delegation engine,
                        // and `agentic_delegate_interception` only matches on
                        // tool NAME == "delegate", so `agent(action='delegate')`
                        // was never intercepted and never spawned a sub-agent.
                        // Models trusted the "Delegation request acknowledged"
                        // string and reported success to the user (observed in
                        // session f3c4b457: 5 fake delegations in 0 ms each).
                        //
                        // Defense in depth: the schema enum no longer advertises
                        // "delegate", so this branch is unreachable from the
                        // model. Kept for two reasons: (1) old session journals
                        // may replay tool_calls with the legacy action; (2) a
                        // sharp Error: result is far better than the silent
                        // success the placeholder produced. The error names
                        // the agent spawn action so the model has a working alternative
                        // — agent_fanout is the correct fan-out shape.
                        "delegate" => {
                            "Error: agent.delegate has been removed because it had no execution \
                             backend in CLI mode and silently no-op'd. Use agent(action='spawn', \
                             description='...', prompt='...') instead. \
                             To run N sub-agents in parallel, use agent_fanout(action='start', \
                             target_count=N, slots=[...])."
                                .to_string()
                        }
                        "run_chain" => {
                            match serde_json::from_value::<astra_runtime::tool_registry::ToolChain>(
                                args.clone(),
                            ) {
                                Ok(chain) => {
                                    let known: Vec<&str> =
                                        astra_runtime::tool_registry::TOOL_CATALOG
                                            .iter()
                                            .map(|t| t.name)
                                            .collect();
                                    if let Err(errors) = chain.validate(&known) {
                                        return format!(
                                            "Error: Invalid chain: {}",
                                            errors.join("; ")
                                        );
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
                        "spawn" => {
                            agent_spawning::handle_agent_spawn_action(
                                args,
                                self.spawn_context.as_ref(),
                            )
                            .await
                        }
                        "get_result" => {
                            agent_spawning::handle_agent_get_result_action(
                                args,
                                self.spawn_context.as_ref(),
                            )
                            .await
                        }
                        "send_message" => {
                            let ctx = self
                                .send_message_context
                                .lock()
                                .ok()
                                .and_then(|g| g.clone());
                            agent_messaging::handle_send_message_tool(args, ctx.as_ref()).await
                        }
                        _ if action.is_empty() && args.get("spawn").is_some() => {
                            "Error: invalid agent call shape. Use the top-level \
                             `action='spawn'` field, not a `spawn` wrapper key. \
                             Example: agent(action='spawn', description='...', \
                             prompt='...'). For parallel \
                             fan-out, use agent_fanout(action='start', target_count=N, \
                             slots=[...]); do not pass `agents:[...]`."
                                .to_string()
                        }
                        _ if action.is_empty() && args.get("agents").is_some() => {
                            "Error: unsupported `agents` batch payload for `agent`. \
                             Each `agent(action='spawn', ...)` call launches exactly \
                             one child. Use `agent_fanout(action='start', target_count=N, \
                             slots=[...])` for atomic parallel fan-out."
                                .to_string()
                        }
                        _ => format!(
                            "Error: unknown agent action '{action}'. Use one of: spawn, get_result, run_chain, send_message"
                        ),
                    }
                }
                "agent_fanout" => {
                    agent_spawning::handle_agent_fanout_tool(args, self.spawn_context.as_ref())
                        .await
                }
                // ── Consolidated session tool ──────────────────────────────
                "session" => {
                    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
                    match action {
                        "config" => self.adjust_config(args),
                        "prioritize" => self.prioritize_tool(args),
                        "deprioritize" => self.deprioritize_tool(args),
                        "compact" => self.compress_context(args),
                        "rollback_edits" => self.rollback_file_edits(args),
                        "sleep" => self.sleep_tool(args).await,
                        "timeline" => self.render_session_timeline(args),
                        "summary" => self.render_session_summary().await,
                        "history" => self.render_session_history(args),
                        "suppress_memory" => self.suppress_memory(args),
                        "unsuppress_memory" => self.unsuppress_memory(args),
                        "list_suppressed" => self.list_suppressed_memories(),
                        "release_context" => self.release_context(args),
                        "list_released" => self.list_released_context(),
                        "" => "Missing required parameter: action. Use: config, prioritize, deprioritize, compact, rollback_edits, sleep, timeline, summary, history, suppress_memory(memory_id, reason?), unsuppress_memory(memory_id), list_suppressed, release_context(tool_call_id|string[]), list_released. Use the first-class `ask_user` tool for user questions. For plan mode use the dedicated `enter_plan_mode` / `exit_plan_mode` tools.".to_string(),
                        other => format!("Error: unknown `session` action '{other}'. Valid: config, prioritize, deprioritize, compact, rollback_edits, sleep, timeline, summary, history, suppress_memory, unsuppress_memory, list_suppressed, release_context, list_released. Use the first-class `ask_user` tool for user questions. For plan mode use the dedicated `enter_plan_mode` / `exit_plan_mode` tools."),
                    }
                }
                // Task management (unified tool with action param)
                "task" => {
                    let action_value = args.get("action");
                    let action = match action_value {
                        Some(Value::String(action)) => action.as_str(),
                        Some(_) => return "Error: field 'action' must be a string".to_string(),
                        None => "",
                    };
                    match action {
                        "create" => match Self::validate_task_tool_args_for_action("create", args)
                        {
                            Ok(()) => self.task_action_create(args).await,
                            Err(error) => format!("Error: {error}"),
                        },
                        "list" => match Self::validate_task_tool_args_for_action("list", args) {
                            Ok(()) => self.task_list(args).await,
                            Err(error) => format!("Error: {error}"),
                        },
                        "get" => match Self::validate_task_tool_args_for_action("get", args) {
                            Ok(()) => self.task_get(args).await,
                            Err(error) => format!("Error: {error}"),
                        },
                        "update" => match Self::validate_task_tool_args_for_action("update", args)
                        {
                            Ok(()) => self.task_action_update(args).await,
                            Err(error) => format!("Error: {error}"),
                        },
                        "stop" => match Self::validate_task_tool_args_for_action("stop", args) {
                            Ok(()) => self.task_action_stop(args).await,
                            Err(error) => format!("Error: {error}"),
                        },
                        // Cross-session views (Phase 7): user_id-indexed
                        // queries served by the server. Cloud-only;
                        // offline mode returns an error since there's
                        // nothing to aggregate across sessions.
                        "list_user" => {
                            match Self::validate_task_tool_args_for_action("list_user", args) {
                                Ok(()) => self.task_list_user(args).await,
                                Err(error) => format!("Error: {error}"),
                            }
                        }
                        "adopt" => match Self::validate_task_tool_args_for_action("adopt", args) {
                            Ok(()) => self.task_adopt(args).await,
                            Err(error) => format!("Error: {error}"),
                        },
                        "archive" => {
                            match Self::validate_task_tool_args_for_action("archive", args) {
                                Ok(()) => self.task_archive(args).await,
                                Err(error) => format!("Error: {error}"),
                            }
                        }
                        "" => "Error: missing required parameter `action` for `task`. Use one of: create, update, list, get, stop, list_user, adopt, archive. For typed background tasks use `task_output`, `task_list`, or `task_stop`.".to_string(),
                        other => match Self::validate_task_tool_args_for_action(other, args) {
                            Ok(()) => format!("Error: unknown `task` action '{other}'. Valid: create, update, list, get, stop, list_user, adopt, archive. For typed background tasks use `task_output`, `task_list`, or `task_stop`. For parallel sub-agents use `agent_fanout(action='start', target_count=N, slots=[...])`; it returns results by default. Backgrounding is user-controlled with Ctrl+B while the live tool is running."),
                            Err(error) => format!("Error: {error}"),
                        },
                    }
                }
                "task_output" => self.task_output(args).await,
                "task_stop" => self.task_kill_bg(args).await,
                "task_list" => self.task_list_bg().await,
                "web_search" => self.web_search(args),
                "ask_user" => "Error: ask_user requires an interactive TUI prompt sink".to_string(),
                "notify" => {
                    const MAX_NOTIFY_MSG: usize = 4096;
                    let message = args.get("message").and_then(Value::as_str).unwrap_or("");
                    let raw_type = args
                        .get("notification_type")
                        .and_then(Value::as_str)
                        .unwrap_or("normal");
                    // Enforce the schema enum — reject anything outside the
                    // declared set rather than silently passing through.
                    let notification_type = match raw_type {
                        "normal" | "proactive" => raw_type,
                        other => {
                            return format!(
                                "Error: 'notification_type' must be 'normal' or 'proactive' (got '{}')",
                                other
                            );
                        }
                    };
                    if message.is_empty() {
                        "Error: 'message' is required".to_string()
                    } else if message.len() > MAX_NOTIFY_MSG {
                        format!(
                            "Error: 'message' exceeds {} bytes ({}). Notifications should be short.",
                            MAX_NOTIFY_MSG,
                            message.len()
                        )
                    } else {
                        serde_json::json!({
                            "delivered": true,
                            "notification_type": notification_type,
                            "message": message,
                        })
                        .to_string()
                    }
                }
                "share_context" => self.share_context(args),
                "query_context" => self.query_context(args),
                astra_runtime::turn::agentic_loop::host::DELEGATE_TOOL_NAME => {
                    "Delegation request acknowledged. The delegation engine will execute \
                this request and provide results in the next round."
                        .to_string()
                }
                "introspect" => self.handle_introspect(args),
                "diagnose" => self.diagnose(args).await,
                "lsp" => self.lsp(args),
                "env" => self.env_tool(args),
                "notebook_edit" => self.notebook_edit(args),
                "config" => self.config_tool(args),
                "brief" => self.brief(args).await,
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
        if name != "memory"
            && !cli_tool_output_is_error(&output)
            && let Some(session_id) = self.active_session_id().filter(|sid| !sid.is_empty())
        {
            let client = astra_tools::memoria::MemoriaClient::new(
                self.cloud_base.clone(),
                self.cloud_token(),
            );
            let ctx = format!("cli-tool:{name}");
            tokio::spawn(async move {
                let report = client
                    .feedback_pending_recalls(&session_id, "useful", &ctx)
                    .await;
                if report.attempted > 0 {
                    tracing::debug!(
                        session_id = %session_id,
                        context = %ctx,
                        attempted = report.attempted,
                        succeeded = report.succeeded,
                        failed = report.failed,
                        "closed recall feedback after successful cli tool"
                    );
                }
            });
        }
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
        let agg = agg.saturating_add(output.len());
        if agg <= AGGREGATE_SOFT_LIMIT {
            return output;
        }
        // Never persist error outputs (they're small and actionable)
        if cli_tool_output_is_error(&output) {
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
        let preview_end = output.floor_char_boundary(preview_end);
        let preview_end = output[..preview_end]
            .rfind('\n')
            .filter(|&pos| pos > preview_end / 2)
            .map(|pos| pos + 1)
            .unwrap_or(preview_end);
        let preview = &output[..preview_end];
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
                let is_err = cli_tool_output_is_error(&output);

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
                                let rollback_output = self
                                    .rollback_turn_actions(&serde_json::json!({
                                        "scope": "turn",
                                        "turn_index": rollback_turn_index,
                                        "file_after_sequence": file_checkpoint,
                                        "database_after_sequence": database_checkpoint,
                                        "stash_after_sequence": stash_checkpoint,
                                        "commit_after_sequence": commit_checkpoint,
                                        "worktree_after_sequence": worktree_checkpoint,
                                        "session_state_after_sequence": session_state_checkpoint,
                                    }))
                                    .await;
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
            && let Some(surface) = crate::cli::self_command::agent_info_surface_alias(dimension)
        {
            return crate::cli::self_command::render_surface_for_session(&session_id, surface, 20)
                .await
                .unwrap_or_else(|error| serde_json::json!({ "error": error }).to_string());
        }

        let self_model = self.build_self_model_snapshot();
        let current_model = self.current_model();
        match dimension {
            "capability" => self.capability_info_json().to_string(),
            "state" => {
                if let Some(ref model) = self_model {
                    serde_json::json!({
                        "current_model": current_model,
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
                        "current_model": current_model,
                        "note": "No observability session available."
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
                "current_model": current_model,
            })
            .to_string(),
            _ => {
                if let Some(ref model) = self_model {
                    let mut text = model.to_detailed_text();
                    if let Some(current_model) = current_model {
                        text.push_str("\nCurrent model: ");
                        text.push_str(&current_model);
                    }
                    text
                } else {
                    serde_json::json!({
                        "current_model": current_model,
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

    fn capability_info_json(&self) -> Value {
        let caps = self.cli_capability_view();
        let current_model = self.current_model();
        if let Some(ref model) = self.build_self_model_snapshot() {
            json!({
                "surface": "CliLocal",
                "current_model": current_model,
                "capabilities_active": caps.active_names,
                "capabilities_inactive": caps.inactive_names,
                "tools_visible": model.capabilities.tool_names,
                "tools_dropped_by_capability": caps.dropped_by_capability,
                "tools_dropped_by_surface": caps.dropped_by_surface,
                "tools_pass_through_mcp": caps.mcp_pass_through,
                "tool_count": model.capabilities.total_tools,
                "deprioritized_tools": model.capabilities.deprioritized_tools,
                "skills": model.capabilities.skills,
                "pinned_tools": model.capabilities.pinned_tools,
                "tool_health": model.capabilities.tool_health.iter().map(|t| {
                    json!({
                        "name": t.name,
                        "total_calls": t.total_calls,
                        "success_rate": t.success_rate,
                        "deprioritized": t.deprioritized,
                    })
                }).collect::<Vec<_>>(),
            })
        } else {
            let tool_count = caps.visible_names.len();
            json!({
                "surface": "CliLocal",
                "current_model": current_model,
                "capabilities_active": caps.active_names,
                "capabilities_inactive": caps.inactive_names,
                "tools_visible": caps.visible_names,
                "tools_dropped_by_capability": caps.dropped_by_capability,
                "tools_dropped_by_surface": caps.dropped_by_surface,
                "tools_pass_through_mcp": caps.mcp_pass_through,
                "tool_count": tool_count,
            })
        }
    }

    fn cli_capability_view(&self) -> CliCapabilityView {
        use astra_turn_core::capability::Capability;

        let caps = cli_default_capabilities(
            self.spawn_context.is_some(),
            self.bg_task_commands.is_some(),
        );
        let mut active_names = Vec::new();
        let mut inactive_names = Vec::new();
        for capability in [
            Capability::AgentSpawner,
            Capability::MemoryService,
            Capability::Database,
            Capability::SkillsCatalog,
            Capability::GitHubAuth,
            Capability::LSPServer,
            Capability::PlanLifecycle,
            Capability::LocalBackgroundTasks,
        ] {
            if caps.has(capability) {
                active_names.push(format!("{capability:?}"));
            } else {
                inactive_names.push(format!("{capability:?}"));
            }
        }

        let mut pool = full_tool_schemas();
        pool.extend(self.plugin_schemas_snapshot("plugin_schemas_capability_view"));

        let outcome = astra_turn_core::tool_surface::resolve_with_diagnostics(
            astra_turn_core::tool_surface::Surface::CliLocal,
            &caps,
            &pool,
        );
        let mut visible_names_vec: Vec<String> = outcome
            .schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        visible_names_vec.sort();
        let visible_names: std::collections::HashSet<String> = outcome
            .schemas
            .iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect();

        let mut dropped_by_capability = Vec::new();
        for meta in astra_turn_core::tool_registry_meta::TOOL_CATALOG {
            if visible_names.contains(meta.name) {
                continue;
            }
            let missing: Vec<String> = meta
                .requires
                .iter()
                .filter(|capability| !caps.has(**capability))
                .map(|capability| format!("{capability:?}"))
                .collect();
            if !missing.is_empty() {
                dropped_by_capability.push(json!({
                    "name": meta.name,
                    "missing": missing,
                }));
            }
        }

        let mcp_pass_through = visible_names
            .into_iter()
            .filter(|name| {
                !astra_turn_core::tool_registry_meta::TOOL_CATALOG
                    .iter()
                    .any(|meta| meta.name == name.as_str())
            })
            .filter(|name| name.starts_with("mcp__"))
            .collect();
        let dropped_by_surface = outcome
            .dropped_by_surface
            .into_iter()
            .map(str::to_string)
            .collect();

        CliCapabilityView {
            active_names,
            inactive_names,
            visible_names: visible_names_vec,
            dropped_by_capability,
            dropped_by_surface,
            mcp_pass_through,
        }
    }

    /// Build a SelfModel snapshot from available observability session data.
    pub fn build_self_model_snapshot(&self) -> Option<astra_runtime::self_model::SelfModel> {
        let obs_session = self.observability_session.as_ref()?;
        let session = obs_session.read().ok()?;

        let visible_tools: Vec<String> = session
            .context_traces
            .last()
            .map(|trace| {
                trace
                    .tools
                    .visible_tools
                    .iter()
                    .map(|tool| tool.tool_name.clone())
                    .collect()
            })
            .unwrap_or_default();
        let tool_name_strs = if visible_tools.is_empty() {
            self.tool_names()
        } else {
            visible_tools
        };
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
            None,
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

        if let Ok(feedback_guard) = self.latest_turn_quality_feedback.lock()
            && let Some(ref feedback) = *feedback_guard
        {
            snapshot = snapshot.with_turn_quality_feedback(Some(feedback.clone()));
        }

        Some(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BgTaskCommand, BgTaskOutputSnapshot, ToolExecutor, all_tool_schemas,
        detect_git_remote_repos, extract_github_owner_repo, file_checkpoint_dir_for,
        format_background_task_error, format_background_task_output,
        format_background_task_output_timeout, format_background_task_stop_error, memoria,
        parse_memory_search_contents, utf16_col_to_char_idx,
    };
    use crate::lock_recovery::LockRecovery;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    struct ImmediateSpawnExecutor;

    struct NoopDelegationLookup;

    #[async_trait::async_trait]
    impl astra_messaging::DelegationLookup for NoopDelegationLookup {
        async fn get_parent(&self, _run_id: &str) -> Option<String> {
            None
        }

        async fn get_agent_id(&self, _run_id: &str) -> Option<String> {
            None
        }

        async fn get_depth(&self, _run_id: &str) -> Option<u32> {
            None
        }

        async fn record_sub_run(&self, _info: astra_messaging::SubRunInfo) {}
    }

    #[async_trait::async_trait]
    impl astra_runtime::orchestration::SpawnAgentExecutor for ImmediateSpawnExecutor {
        async fn execute(
            &self,
            config: astra_runtime::orchestration::SpawnRunConfig,
        ) -> Result<astra_runtime::orchestration::SpawnRunResult, String> {
            Ok(astra_runtime::orchestration::SpawnRunResult {
                agent_id: config.agent_id,
                run_id: config.run_id,
                status: "completed".into(),
                finish_reason: "normal".into(),
                cancelled_by_user: None,
                output: Some("child result".into()),
                error: None,
                prompt_tokens: 0,
                completion_tokens: 0,
                tool_calls: 0,
                permission_summary: None,
                permission_requests: 0,
                permission_requests_approved: 0,
                tools_blocked: 0,
            })
        }
    }

    fn fanout_test_context(
        spawner: Arc<astra_runtime::orchestration::DynamicAgentSpawner>,
    ) -> astra_runtime::orchestration::AgentToolContext {
        astra_runtime::orchestration::AgentToolContext {
            run_id: "run-parent".into(),
            agent_id: "root-agent".into(),
            delegation_chain: Vec::new(),
            current_model: None,
            recursion_depth: 0,
            is_fork_child: false,
            working_dir: PathBuf::from("."),
            spawner,
            inherited_permissions: astra_runtime::orchestration::InheritedPermissions::auto_approve(
            ),
            active_skills: Vec::new(),
            live_event_sink: None,
            trace_context: None,
            execution_metadata: None,
        }
    }

    fn test_spawner() -> Arc<astra_runtime::orchestration::DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(NoopDelegationLookup);
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(router)
                .with_executor(Arc::new(ImmediateSpawnExecutor)),
        )
    }

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
    fn runtime_bound_tool_schemas_fail_closed_for_malformed_and_unbound_tools() {
        let executor = test_executor();
        let plugin_schema =
            serde_json::json!({"type": "function", "function": {"name": "plugin_registered"}});
        let candidate_schemas = || {
            vec![
                serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
                serde_json::json!({"type": "function", "function": {"name": "ask_user"}}),
                serde_json::json!({"type": "function", "function": {"name": "agent_fanout"}}),
                serde_json::json!({"type": "function", "function": {"name": "not_registered"}}),
                plugin_schema.clone(),
                serde_json::json!({"function": {"name": "missing_type"}}),
                serde_json::json!({"type": "custom", "function": {"name": "custom_shape"}}),
                serde_json::json!({"bad": "schema"}),
            ]
        };

        let filtered = executor.runtime_bound_tool_schemas(candidate_schemas());

        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&filtered);
        assert_eq!(
            names,
            HashSet::from(["tool_search".to_string(), "ask_user".to_string()])
        );

        executor.set_plugin_schemas(vec![plugin_schema.clone()]);
        let filtered = executor.runtime_bound_tool_schemas(candidate_schemas());
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&filtered);
        assert_eq!(
            names,
            HashSet::from([
                "tool_search".to_string(),
                "ask_user".to_string(),
                "plugin_registered".to_string()
            ])
        );
    }

    fn parse_tool_search_output(output: &str) -> serde_json::Value {
        serde_json::from_str(output)
            .unwrap_or_else(|error| panic!("tool_search must return JSON, got {error}: {output}"))
    }

    fn tool_search_match_names(parsed: &serde_json::Value) -> Vec<String> {
        parsed["matches"]
            .as_array()
            .unwrap_or_else(|| panic!("matches must be an array in {parsed}"))
            .iter()
            .map(|entry| {
                entry["name"]
                    .as_str()
                    .unwrap_or_else(|| panic!("match entry must have a string name in {entry}"))
                    .to_string()
            })
            .collect()
    }

    fn tool_search_string_array(parsed: &serde_json::Value, field: &str) -> Vec<String> {
        parsed[field]
            .as_array()
            .unwrap_or_else(|| panic!("{field} must be an array in {parsed}"))
            .iter()
            .map(|entry| {
                entry
                    .as_str()
                    .unwrap_or_else(|| panic!("{field} entries must be strings in {parsed}"))
                    .to_string()
            })
            .collect()
    }

    #[tokio::test]
    async fn enter_plan_mode_without_goal_uses_default_label() {
        let (_dir, executor) = temp_executor();
        let output = executor
            .execute("enter_plan_mode", &serde_json::json!({}))
            .await;
        assert!(
            !output.contains("missing required parameter"),
            "goal is optional in the public schema and must not fail empty-args calls: {output}"
        );
        assert!(
            output.contains("Entered plan mode") && output.contains("(pending)"),
            "missing goal should enter local plan mode with the default label: {output}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bash_execute_does_not_block_runtime_worker() {
        let (_dir, executor) = temp_executor();
        let start = std::time::Instant::now();

        let args = serde_json::json!({
            "command": "sleep 0.2; echo done",
            "timeout": 1.0
        });
        let bash = executor.execute("bash", &args);
        let spinner_tick = async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            std::time::Instant::now()
        };

        let (output, ticked_at) = tokio::join!(bash, spinner_tick);
        let tick_latency = ticked_at.duration_since(start);
        assert!(
            tick_latency < std::time::Duration::from_millis(100),
            "bash execution blocked the runtime worker for {tick_latency:?}"
        );
        assert!(output.contains("done"), "unexpected bash output: {output}");
    }

    // ── tool_search dispatch: CLI must route to its local catalog ──────
    //
    // Before this fix: `execute()` had no `"tool_search"` arm, so
    // `default_executor.execute("tool_search")` was called. That uses
    // the default executor's built-in dispatch instead of the CLI's
    // local-tool catalog + plugin schemas. That broke deferred activation
    // for plugin tools on CLI.

    #[tokio::test]
    async fn task_kill_bg_fails_fast_when_unwired() {
        let executor = test_executor();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_kill_bg(&serde_json::json!({"task_id": "bg-shell-1"})),
        )
        .await
        .expect("should fail fast, not hang");
        assert_eq!(
            result,
            "Background task unavailable\nlocal background tasks require an interactive CLI session"
        );
    }

    #[tokio::test]
    async fn task_output_fails_fast_when_unwired() {
        let executor = test_executor();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_output(&serde_json::json!({
                "task_id": "bg-shell-1",
                "block": false
            })),
        )
        .await
        .expect("should fail fast, not hang");
        assert_eq!(
            result,
            "Background task unavailable\nlocal background tasks require an interactive CLI session"
        );
    }

    #[tokio::test]
    async fn task_output_cloud_without_edge_runner_names_missing_runner() {
        let executor = test_executor().with_cloud("https://cloud.example", "token");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_output(&serde_json::json!({
                "task_id": "bg-shell-1",
                "block": false
            })),
        )
        .await
        .expect("should fail fast, not hang");
        assert_eq!(
            result,
            "Background task unavailable\nno edge runner is attached to this cloud session"
        );
    }

    #[tokio::test]
    async fn task_output_nonblocking_times_out_when_registry_does_not_answer() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_output(&serde_json::json!({
                "task_id": "bg-shell-1",
                "block": false,
                "timeout_ms": 5
            })),
        )
        .await
        .expect("registry reply timeout should bound task_output");

        assert!(
            result.contains("Background task registry did not respond within 5ms"),
            "{result}"
        );
        assert!(result.contains("task may still be running"), "{result}");
    }

    #[tokio::test]
    async fn task_output_projects_fanout_group_when_registry_does_not_answer() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let spawner = test_spawner();
        let ctx = fanout_test_context(spawner);
        let executor = test_executor()
            .with_spawn_context(ctx)
            .with_bg_task_commands(commands);

        let started = executor
            .execute(
                "agent_fanout",
                &serde_json::json!({
                    "action": "start",
                    "group_id": "review-fanout",
                    "target_count": 1,
                    "slots": [{
                        "id": "review",
                        "description": "Review one area",
                        "prompt": "Return a short result."
                    }]
                }),
            )
            .await;
        let started_value: serde_json::Value = serde_json::from_str(&started).unwrap();
        assert_eq!(started_value["status"], "completed", "{started}");

        let result = executor
            .task_output(&serde_json::json!({
                "task_id": "review-fanout",
                "timeout_ms": 5
            }))
            .await;

        assert!(
            result.contains("Read agent fanout output review-fanout"),
            "{result}"
        );
        assert!(result.contains("agent_fanout_group"), "{result}");
        assert!(
            result.contains("agent_fanout(action='get_results'"),
            "{result}"
        );
        assert!(
            !result.contains("Background task registry did not respond"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn task_output_blocking_times_out_when_registry_does_not_answer() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_output(&serde_json::json!({
                "task_id": "bg-shell-1",
                "block": true,
                "timeout_ms": 5
            })),
        )
        .await
        .expect("registry reply timeout should bound blocking task_output");

        assert!(
            result.contains("Background task registry did not respond within"),
            "{result}"
        );
        assert!(result.contains("task may still be running"), "{result}");
    }

    #[tokio::test]
    async fn task_output_defaults_to_nonblocking_snapshot() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({
            "task_id": "bg-shell-1",
            "timeout_ms": 10_000
        });
        let output = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let output_fut = executor.task_output(&args);
            tokio::pin!(output_fut);

            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        let command = commands.lock_recover().pop();
                        if let Some(BgTaskCommand::GetOutputSince {
                            task_id,
                            offset,
                            reply,
                            ..
                        }) = command
                        {
                            assert_eq!(task_id, "bg-shell-1");
                            assert_eq!(offset, 0);
                            let _ = reply.send(Ok(bg_snapshot(0, 0, 0, "running", "")));
                        }
                    }
                }
            }
        })
        .await
        .expect("default task_output should return the first snapshot without waiting");

        assert!(output.contains("Read shell output bg-shell-1"), "{output}");
        assert!(output.contains("No output yet"), "{output}");
    }

    #[tokio::test]
    async fn task_stop_cloud_without_edge_runner_names_missing_runner() {
        let executor = test_executor().with_cloud("https://cloud.example", "token");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_kill_bg(&serde_json::json!({"task_id": "bg-shell-1"})),
        )
        .await
        .expect("should fail fast, not hang");
        assert_eq!(
            result,
            "Background task unavailable\nno edge runner is attached to this cloud session"
        );
    }

    #[tokio::test]
    async fn task_stop_times_out_when_registry_does_not_answer() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_kill_bg(&serde_json::json!({"task_id": "bg-shell-1"})),
        )
        .await
        .expect("registry reply timeout should bound task_stop");

        assert!(
            result.contains("Background task bg-shell-1 stop status unknown"),
            "{result}"
        );
        assert!(
            result.contains("Background task registry did not respond within"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn task_list_cloud_without_edge_runner_names_missing_runner() {
        let executor = test_executor().with_cloud("https://cloud.example", "token");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_list_bg(),
        )
        .await
        .expect("should fail fast, not hang");
        assert_eq!(
            result,
            "Background task unavailable\nno edge runner is attached to this cloud session"
        );
    }

    #[tokio::test]
    async fn task_list_times_out_when_registry_does_not_answer() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_list_bg(),
        )
        .await
        .expect("registry reply timeout should bound task_list");

        assert!(
            result.contains("Background task registry unavailable"),
            "{result}"
        );
        assert!(
            result.contains("Timed out after") && result.contains("retry task_list"),
            "{result}"
        );
    }

    #[tokio::test]
    async fn task_list_bg_fails_fast_when_unwired() {
        let executor = test_executor();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            executor.task_list_bg(),
        )
        .await
        .expect("should fail fast, not hang");
        assert_eq!(
            result,
            "Background task unavailable\nlocal background tasks require an interactive CLI session"
        );
    }

    #[tokio::test]
    async fn task_output_empty_id_names_required_task_id() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_output(&serde_json::json!({"task_id": "   ", "block": false}))
            .await;
        assert_eq!(output, "Task id is required");
    }

    #[tokio::test]
    async fn task_stop_empty_id_names_required_task_id() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_kill_bg(&serde_json::json!({"task_id": "   "}))
            .await;
        assert_eq!(output, "Task id is required");
    }

    #[tokio::test]
    async fn task_output_non_string_id_names_expected_shape() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_output(&serde_json::json!({"task_id": 123, "block": false}))
            .await;
        assert_eq!(output, "task_id must be a non-empty string");
    }

    #[tokio::test]
    async fn task_stop_non_string_id_names_expected_shape() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_kill_bg(&serde_json::json!({"task_id": 123}))
            .await;
        assert_eq!(output, "task_id must be a non-empty string");
    }

    #[tokio::test]
    async fn unknown_task_action_points_parallel_agents_to_agent_fanout() {
        let executor = test_executor();
        let output = executor
            .execute("task", &serde_json::json!({"action": "spawn_agents"}))
            .await;

        assert!(output.contains("agent_fanout(action='start'"), "{output}");
        assert!(output.contains("returns results by default"), "{output}");
        assert!(output.contains("Ctrl+B"), "{output}");
        assert!(
            !output.contains("agent_fanout(action='get_results'"),
            "{output}"
        );
        assert!(!output.contains("run_in_background: true"), "{output}");
        assert!(!output.contains("run_in_background=true"), "{output}");
        assert!(!output.contains("agent(action='get_result'"), "{output}");
    }

    fn bg_snapshot(
        end_offset: u64,
        total_bytes: u64,
        total_lines: u64,
        status: &str,
        output: &str,
    ) -> BgTaskOutputSnapshot {
        BgTaskOutputSnapshot {
            kind: "shell".to_string(),
            title: Some("cargo test".to_string()),
            output: output.to_string(),
            end_offset,
            total_bytes,
            total_lines,
            status: status.to_string(),
            terminal: matches!(status, "completed" | "failed" | "killed" | "unavailable"),
            output_ref: "stdout: /tmp/bg-shell-1.stdout · stderr: /tmp/bg-shell-1.stderr"
                .to_string(),
        }
    }

    #[test]
    fn background_task_output_projection_names_empty_running_state() {
        let output =
            format_background_task_output("bg-shell-1", 0, &bg_snapshot(0, 0, 0, "running", ""));
        assert!(output.contains("Read shell output bg-shell-1"), "{output}");
        assert!(output.contains("No output yet · still running"), "{output}");
        assert!(output.contains("0 total lines"), "{output}");
        assert!(output.contains("terminal false"), "{output}");
        assert!(output.contains("output_ref stdout:"), "{output}");
        assert!(!output.contains("<job_output"), "{output}");
        assert!(!output.contains("Job still running"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_terminal_empty_state() {
        let output =
            format_background_task_output("bg-shell-1", 0, &bg_snapshot(0, 0, 0, "completed", ""));
        assert!(output.contains("Completed with no output"), "{output}");
        assert!(output.contains("terminal true"), "{output}");
        assert!(!output.contains("No details returned"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_local_agent_state() {
        let snapshot = BgTaskOutputSnapshot {
            kind: "local agent".to_string(),
            title: Some("review auth flow".to_string()),
            output: "reviewing auth middleware".to_string(),
            end_offset: 25,
            total_bytes: 25,
            total_lines: 1,
            status: "running".to_string(),
            terminal: false,
            output_ref: "agent_state: agent-1".to_string(),
        };

        let output = format_background_task_output("agent-1", 0, &snapshot);

        assert!(
            output.contains("Read local agent output agent-1"),
            "{output}"
        );
        assert!(output.contains("reviewing auth middleware"), "{output}");
        assert!(output.contains("title review auth flow"), "{output}");
        assert!(!output.contains("Read shell output"), "{output}");
        assert!(!output.contains("Job"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_waiting_for_input_state() {
        let output = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(0, 0, 0, "waiting_for_input", ""),
        );
        assert!(
            output.contains("Waiting for input · no new output"),
            "{output}"
        );
        assert!(output.contains("terminal false"), "{output}");
        assert!(!output.contains("still running after"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_pending_and_unavailable_states() {
        let pending =
            format_background_task_output("bg-shell-1", 0, &bg_snapshot(0, 0, 0, "pending", ""));
        let unavailable = format_background_task_output(
            "bg-shell-2",
            0,
            &bg_snapshot(0, 0, 0, "unavailable", ""),
        );

        assert!(pending.contains("Pending · no output yet"), "{pending}");
        assert!(
            unavailable.contains("Unavailable · stale handle or unsupported runner"),
            "{unavailable}"
        );
        assert!(!pending.contains("No details returned"), "{pending}");
        assert!(
            !unavailable.contains("No details returned"),
            "{unavailable}"
        );
    }

    #[test]
    fn background_task_output_projection_hides_internal_waiting_status_with_new_output() {
        let output = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(14, 14, 1, "waiting_for_input", "Continue? (y/n)\n"),
        );
        assert!(output.contains("needs input"), "{output}");
        assert!(output.contains("Continue? (y/n)"), "{output}");
        assert!(!output.contains("waiting_for_input"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_failed_and_killed_empty_states() {
        let failed =
            format_background_task_output("bg-shell-1", 0, &bg_snapshot(0, 0, 0, "failed", ""));
        let killed =
            format_background_task_output("bg-shell-2", 0, &bg_snapshot(0, 0, 0, "killed", ""));
        assert!(failed.contains("Failed with no output"), "{failed}");
        assert!(killed.contains("Stopped with no output"), "{killed}");
        assert!(!failed.contains("Completed with no output"), "{failed}");
        assert!(!killed.contains("Completed with no output"), "{killed}");
    }

    #[test]
    fn background_task_output_projection_reports_total_lines() {
        let output = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(12, 12, 2, "running", "hello\nworld\n"),
        );
        assert!(output.contains("2 new lines"), "{output}");
        assert!(output.contains("2 total lines"), "{output}");
        assert!(output.contains("Output chunk:"), "{output}");
        assert!(output.contains("hello"), "{output}");
        assert!(output.contains("world"), "{output}");
        assert!(!output.contains("\n└ world"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_timeout_without_job_vocabulary() {
        let output = format_background_task_output_timeout("bg-shell-1", 250);
        assert_eq!(
            output,
            "Read shell output bg-shell-1\nNo output yet · still running after 250ms"
        );
        assert!(!output.contains("Job"), "{output}");
    }

    #[test]
    fn background_task_error_projection_names_unknown_id() {
        let output = format_background_task_error(
            "bg-shell-missing",
            "no background shell with id 'bg-shell-missing'",
        );
        assert_eq!(output, "Background task not found: bg-shell-missing");
    }

    #[test]
    fn background_task_error_projection_names_missing_output_artifact() {
        let output = format_background_task_error(
            "bg-shell-1",
            "output artifact missing: /tmp/astra/bg-shell-1.stdout",
        );

        assert!(output.contains("Read shell output bg-shell-1"), "{output}");
        assert!(output.contains("Output artifact missing"), "{output}");
        assert!(output.contains("/tmp/astra/bg-shell-1.stdout"), "{output}");
        assert!(!output.contains("Background shell error"), "{output}");
        assert!(!output.contains("No details returned"), "{output}");
    }

    #[test]
    fn background_task_error_projection_uses_task_vocabulary_for_generic_errors() {
        let output = format_background_task_error("bg-shell-1", "permission denied");

        assert_eq!(
            output,
            "Background task output unavailable: permission denied"
        );
        assert!(!output.contains("Background shell error"), "{output}");
        assert!(!output.contains("No details returned"), "{output}");
    }

    #[test]
    fn background_task_stop_error_projection_names_unknown_id() {
        let output = format_background_task_stop_error(
            "bg-shell-missing",
            "no background shell with id 'bg-shell-missing'",
        );
        assert_eq!(output, "Background task not found: bg-shell-missing");
    }

    #[test]
    fn background_task_stop_error_projection_names_terminal_race() {
        let output = format_background_task_stop_error(
            "bg-shell-1",
            "background shell 'bg-shell-1' already terminated",
        );
        assert_eq!(output, "Background task bg-shell-1 already finished.");
    }

    #[test]
    fn background_task_stop_error_projection_names_stale_handle() {
        let output = format_background_task_stop_error(
            "bg-shell-1",
            "background shell 'bg-shell-1' has a stale handle",
        );

        assert!(
            output.contains("Background task bg-shell-1 cannot be stopped"),
            "{output}"
        );
        assert!(output.contains("no live process handle"), "{output}");
        assert!(!output.contains("Background task stop failed"), "{output}");
    }

    #[tokio::test]
    async fn tool_search_select_github_resolves_on_cli_path() {
        let executor = test_executor();
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:github"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(parsed["mode"].as_str(), Some("select"));
        assert_eq!(
            tool_search_string_array(&parsed, "requested"),
            vec!["github".to_string()]
        );
        assert!(tool_search_string_array(&parsed, "missing").is_empty());
        assert_eq!(tool_search_match_names(&parsed), vec!["github".to_string()]);
        assert!(parsed["matches"][0].get("parameters").is_some());
    }

    #[tokio::test]
    async fn tool_search_select_memory_resolves_on_cli_path() {
        let executor = test_executor();
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:memory"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(parsed["mode"].as_str(), Some("select"));
        assert_eq!(
            tool_search_string_array(&parsed, "requested"),
            vec!["memory".to_string()]
        );
        assert!(tool_search_string_array(&parsed, "missing").is_empty());
        assert_eq!(tool_search_match_names(&parsed), vec!["memory".to_string()]);
        assert!(parsed["matches"][0].get("parameters").is_some());
    }

    #[tokio::test]
    async fn direct_deferred_tool_call_activates_without_executing_on_cli_path() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));

        let before = executor
            .execute(
                "memory",
                &serde_json::json!({"action": "remember", "content": "do not write"}),
            )
            .await;
        assert!(
            before.contains("called directly")
                && before.contains("select:memory")
                && before.contains("not executed"),
            "direct deferred call must become a non-executing activation hint; got: {before}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "direct deferred call must record activation for the next schema-selection round"
        );

        let search = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:memory"}),
            )
            .await;
        let parsed = parse_tool_search_output(&search);
        assert_eq!(tool_search_match_names(&parsed), vec!["memory".to_string()]);
        assert!(tool_search_string_array(&parsed, "missing").is_empty());
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()]
        );

        let after = executor.execute("memory", &serde_json::json!({})).await;
        assert!(
            after.contains("called directly")
                && after.contains("select:memory")
                && after.contains("not executed"),
            "activation state alone must not bypass current tools[] visibility; got: {after}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names_for_schema_injection(),
            vec!["memory".to_string()],
            "schema assembly should surface the selected deferred tool"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "schema assembly must not consume activation before the tool is called"
        );
        assert_eq!(
            executor.activated_deferred_tool_names_for_schema_injection(),
            vec!["memory".to_string()],
            "repeated schema assembly must keep the selected tool available"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "activation must remain pending until the tool is actually called"
        );

        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "memory"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::new());
        let injected = executor.execute("memory", &serde_json::json!({})).await;
        assert!(
            injected.contains("missing required"),
            "visible schema must allow the real executor path; got: {injected}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "accepted visible tool calls consume the matching deferred activation"
        );
    }

    #[tokio::test]
    async fn stale_direct_deferred_activation_cannot_execute_after_manifest_disappears() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::from(["memory".to_string()]));

        let activation = executor
            .execute(
                "memory",
                &serde_json::json!({"action": "remember", "content": "stale"}),
            )
            .await;
        assert!(
            activation.contains("not executed"),
            "direct call must only activate, got: {activation}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()]
        );

        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::new());

        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "stale activation must be pruned once the tool is neither visible nor activatable"
        );
        let denied = executor.execute("memory", &serde_json::json!({})).await;
        assert!(
            denied.contains("not available in this turn")
                && denied.contains("visible in this turn's `tools[]`"),
            "stale activation must not bypass the current surface; got: {denied}"
        );
    }

    #[tokio::test]
    async fn unbound_agent_fanout_cannot_be_deferred_activated() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::from(["agent_fanout".to_string()]));

        let fanout_args = serde_json::json!({
            "action": "start",
            "target_count": 1,
            "slots": [{
                "id": "review",
                "description": "Review",
                "prompt": "Review the current change."
            }]
        });
        let direct_outcome = executor
            .execute_with_metadata("agent_fanout", &fanout_args)
            .await;
        let direct = &direct_outcome.output;
        assert!(
            direct.contains("multi-agent runtime is not connected"),
            "unbound agent_fanout must report binding failure, got: {direct}"
        );
        assert_eq!(
            direct_outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );

        let search = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:agent_fanout"}),
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&search).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "tool_search must not activate an unbound agent_fanout; got: {search}"
        );

        let after_outcome = executor
            .execute_with_metadata("agent_fanout", &fanout_args)
            .await;
        let after = &after_outcome.output;
        assert!(
            after.contains("multi-agent runtime is not connected"),
            "tool_search must not silently mark unbound agent_fanout activated; got: {after}"
        );
        assert_eq!(
            after_outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }

    #[tokio::test]
    async fn unbound_future_agent_action_fails_closed_on_runtime_binding() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);

        let outcome = executor
            .execute_with_metadata(
                "agent",
                &serde_json::json!({"action": "stop", "agent_id": "a1"}),
            )
            .await;
        let output = &outcome.output;

        assert!(
            output.contains("multi-agent runtime is not connected"),
            "future executor action must fail closed on missing binding; got: {output}"
        );
        assert_eq!(
            outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }

    #[tokio::test]
    async fn tool_search_select_cannot_activate_policy_hidden_tool() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::from(["web_fetch".to_string()]));

        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:ask_user"}),
            )
            .await;
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert!(
            parsed["matches"].as_array().unwrap().is_empty(),
            "hidden ask_user must not resolve through tool_search; got: {out}"
        );
        assert!(
            parsed["missing"]
                .as_array()
                .unwrap()
                .iter()
                .any(|value| value.as_str() == Some("ask_user")),
            "hidden ask_user must be reported missing from the current search pool; got: {out}"
        );
    }

    #[tokio::test]
    async fn nonvisible_ask_user_is_denied_before_prompt_sink_error() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);

        let out = executor
            .execute(
                "ask_user",
                &serde_json::json!({
                    "questions": [{
                        "id": "decision",
                        "header": "Decision",
                        "question": "Continue?",
                        "options": [
                            {"label": "Yes", "description": "Continue."},
                            {"label": "No", "description": "Stop."}
                        ]
                    }]
                }),
            )
            .await;

        assert!(out.contains("not available in this turn"), "{out}");
        assert!(!out.contains("select:ask_user"), "{out}");
        assert!(
            !out.contains("requires an interactive TUI prompt sink"),
            "visibility guard must run before the ask_user backend; got: {out}"
        );
    }

    #[tokio::test]
    async fn nonvisible_internal_legacy_tool_is_denied_before_handler() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
        ]);

        let out = executor
            .execute(
                "run_build_test",
                &serde_json::json!({"command": "echo should-not-run"}),
            )
            .await;

        assert!(out.contains("not available in this turn"), "{out}");
        assert!(!out.contains("select:run_build_test"), "{out}");
        assert!(
            !out.contains("should-not-run"),
            "internal handler must not execute when the tool was not visible or activated; got: {out}"
        );
    }

    #[test]
    fn task_update_unknown_field_names_action_that_accepts_field() {
        let err = ToolExecutor::validate_task_tool_args_for_action(
            "update",
            &serde_json::json!({
                "action": "update",
                "task_id": "task-1",
                "subtasks": []
            }),
        )
        .expect_err("task.update must reject create-only subtasks");

        assert!(err.contains("unknown field 'subtasks' for task.update"));
        assert!(err.contains("field is valid for: task.create"), "{err}");
    }

    #[tokio::test]
    async fn introspect_capability_reports_inactive_agent_spawner() {
        let executor = test_executor();
        let out = executor
            .execute(
                "introspect",
                &serde_json::json!({"dimension": "capability"}),
            )
            .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("introspect must return JSON");

        let inactive: Vec<&str> = parsed["capabilities_inactive"]
            .as_array()
            .map(|values| values.iter().filter_map(|value| value.as_str()).collect())
            .unwrap_or_default();
        assert!(
            inactive.contains(&"AgentSpawner"),
            "bare executor must report AgentSpawner inactive; got {out}"
        );
        assert!(
            inactive.contains(&"LocalBackgroundTasks"),
            "bare executor must report LocalBackgroundTasks inactive; got {out}"
        );
        assert!(
            !inactive.contains(&"PlanLifecycle"),
            "local CLI exposes client-backed plan lifecycle wrappers, so PlanLifecycle must stay active; got {out}"
        );

        let dropped = parsed["tools_dropped_by_capability"]
            .as_array()
            .expect("tools_dropped_by_capability");
        let agent_drop = dropped.iter().find(|entry| entry["name"] == "agent");
        assert!(
            agent_drop.is_some(),
            "agent must be reported as capability-dropped; got {out}"
        );
        let missing: Vec<&str> = agent_drop.unwrap()["missing"]
            .as_array()
            .map(|values| values.iter().filter_map(|value| value.as_str()).collect())
            .unwrap_or_default();
        assert!(missing.contains(&"AgentSpawner"));

        let task_output_drop = dropped.iter().find(|entry| entry["name"] == "task_output");
        assert!(
            task_output_drop.is_some(),
            "task_output must be capability-dropped without a background registry; got {out}"
        );
        let missing: Vec<&str> = task_output_drop.unwrap()["missing"]
            .as_array()
            .map(|values| values.iter().filter_map(|value| value.as_str()).collect())
            .unwrap_or_default();
        assert!(missing.contains(&"LocalBackgroundTasks"));
    }

    #[tokio::test]
    async fn introspect_capability_reports_background_tasks_active_when_wired() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let out = executor
            .execute(
                "introspect",
                &serde_json::json!({"dimension": "capability"}),
            )
            .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("introspect must return JSON");

        let active: Vec<&str> = parsed["capabilities_active"]
            .as_array()
            .map(|values| values.iter().filter_map(|value| value.as_str()).collect())
            .unwrap_or_default();
        assert!(
            active.contains(&"LocalBackgroundTasks"),
            "wired executor must report LocalBackgroundTasks active; got {out}"
        );

        let dropped = parsed["tools_dropped_by_capability"]
            .as_array()
            .expect("tools_dropped_by_capability");
        assert!(
            !dropped.iter().any(|entry| entry["name"] == "task_output"),
            "task_output must be visible when LocalBackgroundTasks is active; got {out}"
        );
    }

    #[tokio::test]
    async fn tool_search_rejects_mcp_plugin_schema_without_runtime_binding() {
        // MCP schemas are only callable when the MCP manager currently owns
        // the public tool name. A cached schema alone must not make
        // `tool_search(select:mcp__X)` more optimistic than execution.
        let executor = test_executor();
        let plugin = serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__weather",
                "description": "Get weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        });
        executor.set_plugin_schemas(vec![plugin]);
        executor.set_current_activatable_tool_names(HashSet::from(["mcp__weather".to_string()]));
        assert!(
            executor
                .current_activatable_tool_names_snapshot()
                .is_empty(),
            "MCP activatable names must be dropped when no MCP manager owns the tool"
        );

        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:mcp__weather"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(tool_search_match_names(&parsed), Vec::<String>::new());
        assert_eq!(
            tool_search_string_array(&parsed, "missing"),
            vec!["mcp__weather".to_string()]
        );
    }

    #[tokio::test]
    async fn tool_search_rejects_stale_mcp_plugin_schema_not_owned_by_manager() {
        let executor = test_executor().with_mcp_manager(std::sync::Arc::new(
            tokio::sync::RwLock::new(crate::mcp_client::McpClientManager::new()),
        ));
        executor.set_plugin_schemas(vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__weather",
                "description": "Get weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })]);
        executor.set_current_activatable_tool_names(HashSet::from(["mcp__weather".to_string()]));

        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:mcp__weather"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(tool_search_match_names(&parsed), Vec::<String>::new());
        assert_eq!(
            tool_search_string_array(&parsed, "missing"),
            vec!["mcp__weather".to_string()]
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "stale MCP schemas must not create deferred activation state"
        );
    }

    #[test]
    fn runtime_bound_plugin_schemas_excluding_filters_restricted_plugins() {
        let executor = test_executor();
        executor.set_plugin_schemas(vec![serde_json::json!({
            "type": "function",
            "function": {
                "name": "custom_weather",
                "description": "Get weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })]);

        let unrestricted = executor.runtime_bound_plugin_schemas_excluding(&HashSet::new());
        assert_eq!(
            astra_turn_core::tool::schema::tool_names_from_schemas(&unrestricted),
            HashSet::from(["custom_weather".to_string()])
        );

        let restricted = executor
            .runtime_bound_plugin_schemas_excluding(&HashSet::from(["custom_weather".to_string()]));
        assert!(
            restricted.is_empty(),
            "restricted dynamic plugin schemas must not be advertised as deferred"
        );
    }

    #[tokio::test]
    async fn direct_mcp_call_without_runtime_binding_names_recovery_path() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__weather",
                "description": "Get weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })]);

        let outcome = executor
            .execute_with_metadata("mcp__weather", &serde_json::json!({"city": "Shanghai"}))
            .await;

        assert!(outcome.is_error, "{outcome:?}");
        assert!(
            outcome.output.contains("no connected MCP server")
                && outcome.output.contains("connect or enable the MCP server"),
            "direct MCP call should explain the real recovery path, got: {}",
            outcome.output
        );
        assert_eq!(
            outcome
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("error_kind"))
                .and_then(serde_json::Value::as_str),
            Some(astra_core::ErrorKind::ToolBinding.as_str())
        );
    }

    #[test]
    fn activated_deferred_tool_is_pruned_when_runtime_binding_disappears() {
        let executor = test_executor();
        executor.set_current_visible_tool_schemas(&[serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__weather",
                "description": "Get weather for a city.",
                "parameters": {
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }
            }
        })]);
        executor
            .activated_deferred_tools
            .write()
            .unwrap()
            .insert("mcp__weather".to_string());

        assert_eq!(
            executor.activated_deferred_tool_names(),
            Vec::<String>::new(),
            "stale visible MCP schemas must not retain activation after runtime binding disappears"
        );
    }

    /// Poison recovery: plugin schemas are a cache. If a prior panic poisoned
    /// the RwLock, reset to a known empty state rather than reading possibly
    /// half-written inner data; a later `set_plugin_schemas` repopulates it.
    #[tokio::test]
    async fn tool_search_resets_poisoned_plugin_schemas_lock() {
        let executor = test_executor();
        let plugin = serde_json::json!({
            "type": "function",
            "function": {
                "name": "mcp__calc",
                "description": "Evaluate expressions.",
                "parameters": {"type": "object", "properties": {}}
            }
        });
        executor.set_plugin_schemas(vec![plugin.clone()]);
        executor.set_current_activatable_tool_names(HashSet::from(["mcp__calc".to_string()]));

        // Simulate a prior panic-poisoned write lock.
        let arc = std::sync::Arc::new(&executor.plugin_schemas);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = arc.write().unwrap();
            panic!("simulated panic under write lock");
        }));
        assert!(
            executor.plugin_schemas.read().is_err() || executor.plugin_schemas.write().is_err(),
            "lock should be poisoned for the test to be meaningful"
        );

        // The first select after poison must be stable and clear the poisoned
        // cache, not read inner state from the panicking writer.
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:mcp__calc"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(tool_search_match_names(&parsed), Vec::<String>::new());
        assert_eq!(
            tool_search_string_array(&parsed, "missing"),
            vec!["mcp__calc".to_string()]
        );

        executor.set_plugin_schemas(vec![plugin]);
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:mcp__calc"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(
            tool_search_match_names(&parsed),
            Vec::<String>::new(),
            "repopulating the cache should not bypass the missing MCP runtime binding"
        );
        assert_eq!(
            tool_search_string_array(&parsed, "missing"),
            vec!["mcp__calc".to_string()]
        );
    }

    #[tokio::test]
    async fn tool_search_select_web_fetch_returns_schema_on_cli_path() {
        // web_fetch is deferred by default. The whole point
        // of this test is to exercise the deferred activation flow.
        let executor = test_executor();
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:web_fetch"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert!(tool_search_string_array(&parsed, "missing").is_empty());
        assert_eq!(
            tool_search_match_names(&parsed),
            vec!["web_fetch".to_string()]
        );
        assert!(parsed["matches"][0].get("parameters").is_some());
    }

    // ── introspect tool: first-turn behavior (regression guard) ────────
    //
    // Regression: `handle_introspect` used to return the string
    // "No introspection data available yet (first turn)." whenever the
    // snapshot had not been populated. In the CLI edge path the snapshot
    // is only updated *after* `turn_result?` unwraps, so the model calling
    // `introspect` during turn 1 (or mid-turn in any later turn, before
    // that write lands) always saw the opaque string. The fix: on `None`
    // render a zero-state snapshot so output is always structured.
    #[test]
    fn introspect_returns_structured_output_on_first_turn() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"detail": "summary"}));
        assert!(
            out.contains("Session Health"),
            "expected structured output, got: {out}"
        );
        assert!(
            !out.contains("first turn"),
            "must not return opaque first-turn placeholder, got: {out}"
        );
    }

    #[test]
    fn introspect_minimal_first_turn_has_metrics_not_placeholder() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"detail": "minimal"}));
        assert!(
            out.contains("pressure=") && out.contains("turns="),
            "expected minimal metrics line, got: {out}"
        );
        assert!(!out.contains("first turn"));
    }

    #[test]
    fn introspect_first_turn_reports_current_model_from_executor() {
        let executor = test_executor();
        executor.set_current_model("deepseek-v4-pro-official(thinking:high)");

        let out = executor.handle_introspect(&serde_json::json!({"detail": "minimal"}));

        assert!(
            out.contains("model=deepseek-v4-pro-official(thinking:high)"),
            "expected first-turn introspect to expose current model, got: {out}"
        );
    }

    #[test]
    fn introspect_reflects_updated_snapshot() {
        let executor = test_executor();
        // Populate a non-trivial snapshot.
        executor.update_introspect_snapshot(astra_turn_core::introspect::IntrospectSnapshot {
            turns_completed: 5,
            turns_remaining: 10,
            total_input_tokens: 12345,
            total_output_tokens: 678,
            compaction_tier: "None".to_string(),
            lifecycle_summary: "resume pending: [plan-resume] goal=\"Fix auth\"".to_string(),
            ..Default::default()
        });
        let out = executor.handle_introspect(&serde_json::json!({"detail": "summary"}));
        assert!(out.contains("Turns: 5/15"), "got: {out}");
        assert!(out.contains("12345in"), "got: {out}");
        assert!(out.contains("resume pending"), "got: {out}");
    }

    #[tokio::test]
    async fn get_agent_info_reports_current_model_without_observability_session() {
        let executor = test_executor();
        executor.set_current_model("deepseek-v4-pro-official(thinking:high)");

        let capability = executor
            .get_agent_info(&serde_json::json!({"dimension": "capability"}))
            .await;
        let capability_json: serde_json::Value = serde_json::from_str(&capability).unwrap();
        assert_eq!(
            capability_json["current_model"],
            "deepseek-v4-pro-official(thinking:high)"
        );

        let identity = executor
            .get_agent_info(&serde_json::json!({"dimension": "identity"}))
            .await;
        let identity_json: serde_json::Value = serde_json::from_str(&identity).unwrap();
        assert_eq!(
            identity_json["current_model"],
            "deepseek-v4-pro-official(thinking:high)"
        );
    }

    #[test]
    fn introspect_subtopic_cache_routes_to_cache_diagnosis() {
        let executor = test_executor();
        // No session set → renderer explains the "no data" path.
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "cache"}));
        assert!(
            out.contains("Cache Diagnosis"),
            "subtopic=cache must produce the cache section, got: {out}",
        );
        assert!(
            out.contains("No per-round cache snapshots"),
            "without a session / captures, the renderer must explain why: {out}",
        );
    }

    #[test]
    fn introspect_subtopic_session_is_default_behavior() {
        // Without subtopic the tool still shows Session Health unchanged.
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"detail": "summary"}));
        assert!(
            out.contains("Session Health"),
            "default subtopic must preserve legacy output, got: {out}",
        );
    }

    #[test]
    fn introspect_subtopic_cache_is_case_insensitive() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "Cache"}));
        assert!(out.contains("Cache Diagnosis"), "got: {out}");
    }

    #[test]
    fn cloud_token_updates_after_cloud_configuration() {
        let executor = test_executor().with_cloud("https://cloud.example", "old-token");
        assert_eq!(executor.cloud_token().as_deref(), Some("old-token"));

        executor.set_cloud_token("new-token");
        assert_eq!(executor.cloud_token().as_deref(), Some("new-token"));
    }

    #[test]
    fn memory_args_include_session_and_current_turn() {
        let executor = test_executor().with_active_session_id("mem-session");
        executor
            .journal_turn_index
            .store(9, std::sync::atomic::Ordering::Release);

        let args = executor.memory_args_with_context(&serde_json::json!({
            "action": "recall",
            "query": "memory loop",
        }));

        assert!(args.get("action").is_none());
        assert_eq!(args["session_id"].as_str(), Some("mem-session"));
        assert_eq!(args["turn"].as_u64(), Some(9));
    }

    // ── File-journal persistence wiring (regression guard) ──────────────

    /// RAII guard that scrubs `_ASTRA_FILE_CHECKPOINT_ROOT` on drop so the
    /// test's override doesn't bleed into other tests running in the same
    /// process. Also restores any prior value so running under a hostile
    /// parent env stays idempotent.
    struct CheckpointRootGuard {
        prior: Option<String>,
    }
    impl CheckpointRootGuard {
        fn set(dir: &std::path::Path) -> Self {
            let prior = std::env::var("_ASTRA_FILE_CHECKPOINT_ROOT").ok();
            // SAFETY: test-only; callers are `#[serial]` so no parallel
            // reads race this write.
            unsafe { std::env::set_var("_ASTRA_FILE_CHECKPOINT_ROOT", dir) };
            Self { prior }
        }
    }
    impl Drop for CheckpointRootGuard {
        fn drop(&mut self) {
            // SAFETY: see set().
            match &self.prior {
                Some(v) => unsafe { std::env::set_var("_ASTRA_FILE_CHECKPOINT_ROOT", v) },
                None => unsafe { std::env::remove_var("_ASTRA_FILE_CHECKPOINT_ROOT") },
            }
        }
    }

    /// Baseline: setting the active session-id binds persistence on the
    /// executor's journal. Without the shared-journal override, this is the
    /// simple path that already worked before the regression fix.
    #[serial_test::serial]
    #[test]
    fn active_session_id_enables_persistence_on_default_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());

        let executor = test_executor().with_active_session_id("session-a");

        let journal = executor.file_journal.lock_recover();
        assert!(
            journal.persist_dir().is_some(),
            "persistence must be enabled after session-id is set"
        );
        let expected = tmp.path().join("session-a").join("file_checkpoints");
        assert_eq!(journal.persist_dir(), Some(expected.as_path()));
    }

    /// REGRESSION: the production wiring order is
    /// `ToolExecutor::new(..).with_active_session_id(sid).with_shared_file_journal(arc)`
    /// (see sse_loop/mod.rs:192-201). Before this fix, `with_shared_file_journal`
    /// replaced the executor's journal wholesale, discarding the
    /// persistence binding that `with_active_session_id` had just installed.
    ///
    /// Invariant: after both builder calls, the FINAL journal on the
    /// executor must have `persist_dir == Some(dir for sid)`.
    #[serial_test::serial]
    #[test]
    fn shared_journal_inherits_persistence_from_active_session() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());

        // Simulate SessionState: a shared in-memory journal with no persistence.
        let shared: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        > = std::sync::Arc::new(std::sync::Mutex::new(
            astra_turn_core::file_edit_journal::FileEditJournal::new(500),
        ));
        assert!(
            shared.lock_recover().persist_dir().is_none(),
            "fresh shared journal starts in-memory"
        );

        // Production wiring order from sse_loop/mod.rs.
        let executor = test_executor()
            .with_active_session_id("session-b")
            .with_shared_file_journal(shared.clone());

        let expected = tmp.path().join("session-b").join("file_checkpoints");

        // The executor must see the shared journal as its own (Arc ptr eq).
        assert!(
            std::sync::Arc::ptr_eq(&executor.file_journal, &shared),
            "executor should hold the shared journal Arc"
        );

        // AND that shared journal must now carry the persistence binding.
        let journal = shared.lock_recover();
        assert_eq!(
            journal.persist_dir(),
            Some(expected.as_path()),
            "with_shared_file_journal after with_active_session_id must re-apply persistence"
        );
    }

    /// Reverse order: `with_shared_file_journal(arc).with_active_session_id(sid)`
    /// — legacy builder call sequence. Persistence must still end up on
    /// the shared journal (because set_active_session_id operates on
    /// whatever journal is currently bound) AND any entries recorded
    /// before the session-id was set must survive.
    #[serial_test::serial]
    #[test]
    fn shared_journal_gets_persistence_when_session_id_set_last() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());

        let shared: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        > = std::sync::Arc::new(std::sync::Mutex::new(
            astra_turn_core::file_edit_journal::FileEditJournal::new(500),
        ));

        // Record something into the shared journal BEFORE either builder
        // call. This validates R8.7: the reverse order must preserve
        // pre-session entries, not just bind persistence.
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("pre.txt");
        std::fs::write(&file, b"v0").unwrap();
        {
            let mut j = shared.lock_recover();
            j.record_before(&file, "pre", 0);
            j.record_after(&file, "pre", b"v1");
        }

        let executor = test_executor()
            .with_shared_file_journal(shared.clone())
            .with_active_session_id("session-c");

        let expected = tmp.path().join("session-c").join("file_checkpoints");
        let j = shared.lock_recover();
        assert_eq!(j.persist_dir(), Some(expected.as_path()));
        assert!(std::sync::Arc::ptr_eq(&executor.file_journal, &shared));
        assert_eq!(
            j.len(),
            1,
            "pre-session entry must survive late session binding"
        );
        // Entry was also flushed to disk via enable_persistence's initial save.
        let on_disk = std::fs::read_dir(&expected)
            .map(|r| r.flatten().count())
            .unwrap_or(0);
        assert!(
            on_disk >= 1,
            "pre-session entry should have been flushed to disk"
        );
    }

    /// R8.2 regression: if the shared journal already has in-memory entries
    /// (because a prior call recorded something before session-id was set),
    /// calling `set_active_session_id` must NOT silently drop them.
    /// Before the fix, `*journal = loaded` blindly replaced the journal
    /// with whatever `load_from_dir` returned — typically empty on a fresh
    /// checkpoint dir — so the prior entry vanished.
    #[serial_test::serial]
    #[test]
    fn set_active_session_id_preserves_existing_in_memory_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());

        // Create a shared journal and record an entry into it BEFORE any
        // session binding.
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("pre-session.txt");
        std::fs::write(&file, b"before").unwrap();

        let shared: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        > = std::sync::Arc::new(std::sync::Mutex::new(
            astra_turn_core::file_edit_journal::FileEditJournal::new(500),
        ));
        {
            let mut j = shared.lock_recover();
            j.record_before(&file, "early-call", 0);
            j.record_after(&file, "early-call", b"after");
        }
        assert_eq!(shared.lock_recover().len(), 1);

        // Now wire into an executor and set the session id.
        let _executor = test_executor()
            .with_shared_file_journal(shared.clone())
            .with_active_session_id("session-d");

        // The pre-session entry MUST survive the binding.
        let j = shared.lock_recover();
        assert_eq!(j.len(), 1, "pre-session in-memory entry must not be lost");
        let entries: Vec<_> = j.entries().collect();
        assert_eq!(entries[0].path, file);
        assert_eq!(entries[0].after_content, b"after");
    }

    /// Corollary: when the shared journal is empty at session-binding
    /// time AND disk has entries from a prior run, those disk entries
    /// should load into memory (the crash-recovery happy path).
    #[serial_test::serial]
    #[test]
    fn set_active_session_id_loads_disk_entries_when_memory_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());

        // Pre-seed the disk dir with an entry from a "prior run".
        let dir = tmp.path().join("session-e").join("file_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        let prior = astra_turn_core::file_edit_journal::FileEditEntry {
            sequence: 0,
            path: PathBuf::from("/tmp/prior.txt"),
            turn_index: 5,
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            before_content: Some(b"p".to_vec()),
            after_content: b"q".to_vec(),
            tool_call_id: "prior".into(),
            edit_type: astra_turn_core::file_edit_journal::EditType::Overwrite,
        };
        std::fs::write(dir.join("000000.json"), serde_json::to_vec(&prior).unwrap()).unwrap();

        // Empty shared journal + set session-id → should load the disk entry.
        let shared: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        > = std::sync::Arc::new(std::sync::Mutex::new(
            astra_turn_core::file_edit_journal::FileEditJournal::new(500),
        ));

        let _executor = test_executor()
            .with_shared_file_journal(shared.clone())
            .with_active_session_id("session-e");

        let j = shared.lock_recover();
        assert_eq!(j.len(), 1, "prior-run entry must load from disk");
        let entries: Vec<_> = j.entries().collect();
        assert_eq!(entries[0].tool_call_id, "prior");
        assert_eq!(entries[0].after_content, b"q");
    }

    /// T61 / R9.1: when the shared journal has pre-session entries AND
    /// the on-disk dir holds prior-run entries, BOTH sides must survive
    /// the session binding. Before the merge fix, `save_to_dir`'s
    /// destructive pruning would delete disk entries whose sequences
    /// were not in memory.
    #[serial_test::serial]
    #[test]
    fn set_active_session_id_merges_pre_session_entries_with_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());

        // Seed disk with 2 prior-run entries.
        let dir = tmp.path().join("session-f").join("file_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();
        for i in 0..2u64 {
            let entry = astra_turn_core::file_edit_journal::FileEditEntry {
                sequence: i,
                path: PathBuf::from(format!("/tmp/prior-{i}.txt")),
                turn_index: 0,
                timestamp: std::time::SystemTime::UNIX_EPOCH,
                before_content: None,
                after_content: b"p".to_vec(),
                tool_call_id: format!("prior-{i}"),
                edit_type: astra_turn_core::file_edit_journal::EditType::Create,
            };
            std::fs::write(
                dir.join(format!("{i:06}.json")),
                serde_json::to_vec(&entry).unwrap(),
            )
            .unwrap();
        }

        // Pre-seed shared journal with 1 pre-session entry.
        let work = tempfile::tempdir().unwrap();
        let pre_file = work.path().join("pre.txt");
        std::fs::write(&pre_file, b"v0").unwrap();
        let shared: std::sync::Arc<
            std::sync::Mutex<astra_turn_core::file_edit_journal::FileEditJournal>,
        > = std::sync::Arc::new(std::sync::Mutex::new(
            astra_turn_core::file_edit_journal::FileEditJournal::new(500),
        ));
        {
            let mut j = shared.lock_recover();
            j.record_before(&pre_file, "pre-session", 0);
            j.record_after(&pre_file, "pre-session", b"v1");
        }

        // Bind session — triggers the merge path.
        let _executor = test_executor()
            .with_shared_file_journal(shared.clone())
            .with_active_session_id("session-f");

        // Must have ALL 3 entries (2 disk + 1 pre-session).
        let j = shared.lock_recover();
        assert_eq!(
            j.len(),
            3,
            "both disk (2) and pre-session (1) entries must survive"
        );
        let tags: Vec<String> = j.entries().map(|e| e.tool_call_id.clone()).collect();
        assert_eq!(tags, vec!["prior-0", "prior-1", "pre-session"]);

        // All 3 entries should now be on disk.
        let on_disk_count = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path().extension().and_then(|x| x.to_str()) == Some("json")
                    && !e.file_name().to_string_lossy().starts_with('.')
            })
            .count();
        assert_eq!(
            on_disk_count, 3,
            "all 3 merged entries must be persisted to disk"
        );
    }

    /// T62: calling `set_active_session_id` twice with the same sid
    /// must NOT double-merge the disk entries. After the first bind, the
    /// journal is persistence-enabled and disk matches memory. A second
    /// bind with the same sid should be a no-op on content (possibly a
    /// no-op on disk state too, though the flush is idempotent).
    #[serial_test::serial]
    #[test]
    fn set_active_session_id_idempotent_for_same_sid() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, b"v0").unwrap();

        let executor = test_executor().with_active_session_id("session-g");
        {
            let mut j = executor.file_journal.lock_recover();
            j.record_before(&file, "call", 0);
            j.record_after(&file, "call", b"v1");
        }

        let before_len = executor.file_journal.lock_recover().len();
        assert_eq!(before_len, 1);

        // Second call with same sid.
        executor.set_active_session_id("session-g");

        let after_len = executor.file_journal.lock_recover().len();
        assert_eq!(
            after_len, before_len,
            "re-binding same sid must not duplicate entries"
        );
    }

    /// T63: binding a DIFFERENT sid after an initial bind redirects
    /// persistence to the new dir but leaves the old dir's files alone
    /// (no destructive cross-session cleanup).
    #[serial_test::serial]
    #[test]
    fn rebind_different_sid_leaves_old_dir_files_intact() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, b"v0").unwrap();

        let executor = test_executor().with_active_session_id("session-h1");
        {
            let mut j = executor.file_journal.lock_recover();
            j.record_before(&file, "call-h1", 0);
            j.record_after(&file, "call-h1", b"v1");
        }
        let h1_dir = tmp.path().join("session-h1").join("file_checkpoints");
        let h1_count_before = std::fs::read_dir(&h1_dir).unwrap().count();
        assert!(h1_count_before >= 1, "h1 dir should have entries");

        executor.set_active_session_id("session-h2");
        let h2_dir = tmp.path().join("session-h2").join("file_checkpoints");
        assert_eq!(
            executor.file_journal.lock_recover().persist_dir(),
            Some(h2_dir.as_path()),
            "persist_dir must redirect to the new session"
        );

        // h1's dir must NOT have been pruned by the rebind.
        let h1_count_after = std::fs::read_dir(&h1_dir).unwrap().count();
        assert_eq!(
            h1_count_before, h1_count_after,
            "rebinding must not touch the old session's dir"
        );
    }

    /// R10.1: rebinding to a different session starts that new session
    /// with a **clean slate** — the old session's in-memory entries
    /// do NOT carry forward into the new session's disk dir.
    ///
    /// Rationale: sessions are isolation boundaries. An entry recorded
    /// under sid1 is logically part of sid1's history. If a user
    /// rebinds to sid2 (e.g. switching working contexts mid-process),
    /// sid2 should not inherit ghost entries that were never part of
    /// its own edit history.
    ///
    /// Before this policy, rebind carried memory forward and
    /// `enable_persistence`'s initial flush wrote those carryover
    /// entries into sid2's dir.
    #[serial_test::serial]
    #[test]
    fn rebind_different_sid_does_not_leak_entries_into_new_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = CheckpointRootGuard::set(tmp.path());
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("f.txt");
        std::fs::write(&file, b"v0").unwrap();

        let executor = test_executor().with_active_session_id("session-i1");
        {
            let mut j = executor.file_journal.lock_recover();
            j.record_before(&file, "call-i1", 0);
            j.record_after(&file, "call-i1", b"v1");
        }
        // Confirm sid1 has an entry in memory.
        assert_eq!(executor.file_journal.lock_recover().len(), 1);

        // Rebind to a fresh session.
        executor.set_active_session_id("session-i2");
        let i2_dir = tmp.path().join("session-i2").join("file_checkpoints");

        // In-memory journal starts clean under the new session.
        assert_eq!(
            executor.file_journal.lock_recover().len(),
            0,
            "memory must be cleared on rebind-to-different-sid"
        );
        // sid2's dir must NOT contain sid1's entry.
        let i2_count = std::fs::read_dir(&i2_dir)
            .map(|r| {
                r.flatten()
                    .filter(|e| {
                        e.path().extension().and_then(|x| x.to_str()) == Some("json")
                            && !e.file_name().to_string_lossy().starts_with('.')
                    })
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(
            i2_count, 0,
            "sid2 dir must be empty — sid1 entries must not leak"
        );
    }

    /// T64: `file_checkpoint_dir_for` rejects empty / whitespace session
    /// ids — persistence stays off. We pin this at the function level
    /// rather than through `set_active_session_id`, because an upstream
    /// validator in `session_workspace::workspace_dir` panics on
    /// invalid ids before our code runs (pre-existing defensive panic,
    /// out of scope to redesign here).
    #[test]
    fn file_checkpoint_dir_for_rejects_empty_and_whitespace() {
        assert!(file_checkpoint_dir_for("").is_none());
        assert!(file_checkpoint_dir_for("   ").is_none());
        assert!(file_checkpoint_dir_for("\t\n").is_none());
    }

    /// T65: with the test-only override set, `file_checkpoint_dir_for`
    /// honors it regardless of `HOME` state. We pin this at the function
    /// level rather than through the executor; the executor path goes
    /// through `read_workspace` and other HOME-touching code we can't
    /// isolate cleanly.
    ///
    /// The "HOME fully unset → None" case is not portable: `dirs::home_dir()`
    /// has platform-specific passwd-file fallbacks. Document the
    /// degradation contract by exercising the override path instead.
    #[serial_test::serial]
    #[test]
    fn file_checkpoint_dir_for_override_wins_regardless_of_home() {
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: test-only; #[serial]; env is serialised.
        let prior = std::env::var("_ASTRA_FILE_CHECKPOINT_ROOT").ok();
        unsafe { std::env::set_var("_ASTRA_FILE_CHECKPOINT_ROOT", tmp.path()) };

        let dir = file_checkpoint_dir_for("test-session");
        let expected = tmp.path().join("test-session").join("file_checkpoints");
        assert_eq!(dir, Some(expected));

        // Restore.
        unsafe {
            match prior {
                Some(v) => std::env::set_var("_ASTRA_FILE_CHECKPOINT_ROOT", v),
                None => std::env::remove_var("_ASTRA_FILE_CHECKPOINT_ROOT"),
            }
        }
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

    mod lsp_tests;
    mod memoria_tests;
    mod notebook_tests;
    mod sandbox_tests;
    mod schema_tests;
    mod self_mod_tests;
    mod sleep_tests;
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

    // ── introspect subtopic=session_memory (unhappy first) ────────────

    fn attach_empty_observatory(
        executor: ToolExecutor,
    ) -> (
        ToolExecutor,
        std::sync::Arc<astra_runtime::session_memory::SessionMemoryObservatory>,
    ) {
        let obs =
            std::sync::Arc::new(astra_runtime::session_memory::SessionMemoryObservatory::new());
        (
            executor.with_session_memory_observatory(std::sync::Arc::clone(&obs)),
            obs,
        )
    }

    #[test]
    fn introspect_session_memory_missing_observatory_returns_placeholder() {
        // Offline executor never wires the observatory — output must be
        // a short, honest placeholder rather than silently empty so the
        // model knows the tool works but isn't wired.
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(
            out.contains("No observatory attached"),
            "expected placeholder, got: {out}"
        );
    }

    #[test]
    fn introspect_session_memory_missing_observatory_falls_back_to_journal() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "sess-introspect-memory";
        let writer = astra_services::session_journal::JournalWriter::new(session_id).unwrap();
        let breadcrumbs = astra_services::session_journal::SessionMemoryExtractionBreadcrumbs {
            messages_count: Some(13),
            selector_model: Some("haiku".to_string()),
            attempt: Some(1),
            llm_reason: None,
            llm_detail: None,
            persist_detail: None,
        };
        writer
            .append(&astra_services::session_journal::JournalEvent::session_memory_extraction(
                Some(session_id),
                2,
                120,
                astra_services::session_journal::SessionMemoryExtractionOutcome::Errored {
                    reason: astra_services::session_journal::SessionMemoryExtractionErrorReason::LlmError,
                },
                &breadcrumbs,
            ))
            .unwrap();
        let turn_error = astra_services::session_journal::JournalEvent::turn_error(
            Some(session_id),
            2,
            Some("deepseek-v4-pro-anthropic"),
            "continue",
            "[cancelled] user_interrupted",
            0,
        );
        writer.append(&turn_error).unwrap();

        let executor = test_executor().with_active_session_id(session_id);
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(out.contains("source: local_journal"), "{out}");
        assert!(out.contains("session_end: missing"), "{out}");
        assert!(out.contains("errored reason=llm_error"), "{out}");
        assert!(
            out.contains("last_turn_error: t2 [cancelled] user_interrupted"),
            "{out}"
        );
    }

    #[test]
    fn introspect_session_memory_journal_shows_fallback_recovery_chain() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "sess-introspect-fallback";
        let writer = astra_services::session_journal::JournalWriter::new(session_id).unwrap();
        let breadcrumbs = astra_services::session_journal::SessionMemoryExtractionBreadcrumbs {
            messages_count: Some(7),
            selector_model: Some("haiku".to_string()),
            attempt: Some(1),
            llm_reason: Some(
                astra_services::session_journal::SessionMemoryExtractionErrorReason::LlmError,
            ),
            llm_detail: Some("http 502: upstream model gateway timed out".to_string()),
            persist_detail: None,
        };
        writer
            .append(&astra_services::session_journal::JournalEvent::session_memory_extraction(
                Some(session_id),
                3,
                180,
                astra_services::session_journal::SessionMemoryExtractionOutcome::Extracted {
                    source: astra_services::session_journal::SessionMemoryExtractionSource::RuleFallback,
                    bytes_written: 42,
                },
                &breadcrumbs,
            ))
            .unwrap();

        let executor = test_executor().with_active_session_id(session_id);
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(out.contains("extracted source=rule_fallback"), "{out}");
        assert!(out.contains("llm_reason=llm_error"), "{out}");
        assert!(
            out.contains("llm_detail=http 502: upstream model gateway timed out"),
            "{out}"
        );
        assert!(out.contains("model=haiku"), "{out}");
    }

    #[test]
    fn introspect_session_memory_journal_surfaces_turn_pipeline_injection() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "sess-introspect-pipeline";
        let writer = astra_services::session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(
                &astra_services::session_journal::JournalEvent::context_assembly_recorded(
                    Some(session_id),
                    4,
                    serde_json::json!({
                        "system_prompt": {
                            "session_memory_injected": {
                                "memory_id": "session-memory",
                                "memory_type": "session_memory_llm",
                                "tokens": 27,
                                "relevance_score": 1.0,
                                "content_preview": "Session memory injected into current turn"
                            }
                        },
                        "memory": {
                            "memories_selected": [
                                {"memory_id": "old-1"},
                                {"memory_id": "old-2"}
                            ]
                        }
                    }),
                ),
            )
            .unwrap();

        let executor = test_executor().with_active_session_id(session_id);
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(out.contains("## turn-pipeline session memory"), "{out}");
        assert!(
            out.contains(
                "t4 session_memory=present source=session_memory_llm tokens=27 retrieved_memories=2"
            ),
            "{out}"
        );
    }

    #[test]
    fn introspect_session_memory_unreadable_journal_surfaces_error() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "sess-introspect-unreadable";
        std::fs::create_dir_all(astra_services::session_journal::journal_file_path(
            session_id,
        ))
        .unwrap();

        let executor = test_executor().with_active_session_id(session_id);
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(out.contains("journal unavailable:"), "{out}");
        assert!(
            out.contains("failed to read session journal for sess-introspect-unreadable"),
            "{out}"
        );
        assert!(out.contains("No observatory attached"), "{out}");
    }

    #[test]
    fn introspect_session_memory_empty_rings_renders_cleanly() {
        let (executor, _obs) = attach_empty_observatory(test_executor());
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(out.contains("extractions_ring: 0"));
        assert!(out.contains("injections_ring: 0"));
        assert!(out.contains("(none recorded this session)"));
    }

    #[test]
    fn introspect_session_memory_renders_extraction_and_injection() {
        use astra_runtime::session_memory::observatory::{
            ExtractionOutcome, ExtractionRecord, ExtractionSource, ExtractionTrigger, FactsSummary,
            InjectionLevel, InjectionRecord, RetrievedMemoryRef, StalenessSignals,
        };
        use std::time::{Duration, SystemTime};

        let (executor, obs) = attach_empty_observatory(test_executor());
        obs.record_extraction(ExtractionRecord {
            session_id: "s1".into(),
            turn: 3,
            at: SystemTime::UNIX_EPOCH,
            trigger: ExtractionTrigger::GrowthGate,
            selector_model: Some("mini-judge".into()),
            outcome: ExtractionOutcome::Persisted {
                source: ExtractionSource::Llm,
                bytes_written: 1234,
                store_attempt: 2,
            },
            narrative_sections: vec!["Task Specification".into(), "Learnings".into()],
            content_preview: "[session-memory:v1] full\ndetails".into(),
            latency: Duration::from_millis(120),
        });
        obs.record_injection(InjectionRecord {
            session_id: "s1".into(),
            turn: 4,
            at: SystemTime::UNIX_EPOCH,
            pressure: 0.82,
            level: InjectionLevel::L1Minimal,
            injected_chars: 140,
            facts_summary: FactsSummary {
                turn: 4,
                plan_completed: 2,
                plan_total: 5,
                active_files_count: 3,
                error_count: 1,
                last_error_preview: Some("compile error".into()),
                ..Default::default()
            },
            staleness: StalenessSignals {
                task_contradicted: false,
                missing_corrections: true,
            },
            retrieved_memories: vec![RetrievedMemoryRef {
                memory_id: "abcdef12-3456".into(),
                memory_type: "working".into(),
                score: Some(0.71),
                content_preview: Some("Uses PostgreSQL for primary storage".into()),
            }],
            narrative_sections_kept: vec!["Task Specification".into()],
        });

        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        // Extraction line
        assert!(
            out.contains("t3 GrowthGate persisted(Llm,bytes=1234,attempt=2)"),
            "extraction line missing; got:\n{out}"
        );
        assert!(
            out.contains("preview:"),
            "preview line missing; got:\n{out}"
        );
        // Newline in preview must be collapsed so one record stays one
        // visual line (the arrow-return marker keeps it debuggable).
        assert!(out.contains(" ⏎ "), "expected newline marker; got:\n{out}");
        // Injection line
        assert!(
            out.contains("level=L1Minimal pressure=0.82"),
            "injection line missing; got:\n{out}"
        );
        assert!(
            out.contains("plan=2/5 files=3 errs=1 staleness=missing_corrections"),
            "injection summary missing; got:\n{out}"
        );
        // Retrieved memories show short id + score
        assert!(
            out.contains("retrieved: working[abcdef12]=0.71"),
            "retrieved line missing; got:\n{out}"
        );
    }

    #[test]
    fn introspect_session_memory_output_size_is_bounded_under_full_ring() {
        // Under capacity, output must stay under 4KB (a comfortable
        // upper bound well below any reasonable context concern).
        // Guards against accidental regressions that would dump the
        // full injection content.
        use astra_runtime::session_memory::observatory::{
            ExtractionOutcome, ExtractionRecord, ExtractionSource, ExtractionTrigger, FactsSummary,
            InjectionLevel, InjectionRecord, StalenessSignals,
        };
        use std::time::{Duration, SystemTime};

        let (executor, obs) = attach_empty_observatory(test_executor());
        let big = "x".repeat(1000); // > PREVIEW_CHAR_CAP on purpose
        for i in 0..64u32 {
            obs.record_extraction(ExtractionRecord {
                session_id: format!("s{i}"),
                turn: i,
                at: SystemTime::UNIX_EPOCH,
                trigger: ExtractionTrigger::GrowthGate,
                selector_model: Some("m".into()),
                outcome: ExtractionOutcome::Persisted {
                    source: ExtractionSource::Llm,
                    bytes_written: 42,
                    store_attempt: 1,
                },
                narrative_sections: vec!["Task Specification".into()],
                content_preview: big.clone(), // test overrides; real path clips
                latency: Duration::from_millis(10),
            });
        }
        for i in 0..32u32 {
            obs.record_injection(InjectionRecord {
                session_id: format!("s{i}"),
                turn: i,
                at: SystemTime::UNIX_EPOCH,
                pressure: 0.6,
                level: InjectionLevel::L1Full,
                injected_chars: 500,
                facts_summary: FactsSummary::default(),
                staleness: StalenessSignals::default(),
                retrieved_memories: vec![],
                narrative_sections_kept: vec![],
            });
        }
        let out = executor.handle_introspect(&serde_json::json!({"subtopic": "session_memory"}));
        assert!(
            out.len() < 8_000,
            "introspect output must stay bounded; got {} bytes",
            out.len()
        );
        // Last 8 extractions visible — the oldest must be hidden.
        assert!(out.contains("t63"), "newest extraction missing");
        assert!(!out.contains("t0 GrowthGate"), "oldest must be elided");
        // Elision breadcrumb present.
        assert!(
            out.contains("older records elided"),
            "must indicate elision; got:\n{out}"
        );
    }
}
