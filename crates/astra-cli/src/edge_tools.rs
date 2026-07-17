//! Edge tool definitions and execution for the astra CLI.
//!
//! Tools: bash, read_file (with outline mode), write_file, str_replace (with fuzzy matching),
//!        list_dir, grep (with context_lines/max_matches), glob,
//!        git(action=...), github(action=...), web_fetch, mo_query.

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

use crate::background_task_error::BackgroundTaskError;

/// Prefix returned by tool execution when the sandbox blocks a path.
/// The agentic loop / permission manager can detect this to prompt the user
/// for authorization instead of letting the model silently fall back to bash.
pub const SANDBOX_DENIED_PREFIX: &str = "SANDBOX_DENIED: ";

#[derive(Clone, Default)]
pub(super) struct EdgeMcpRuntimeSnapshot {
    manager: Option<std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>>,
    schemas: Vec<Value>,
}

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
    "mo_query",
    "notebook_edit",
    "notify",
    "query_context",
    "reflect",
    "rename_symbol",
    "rollback_database_snapshots",
    "rollback_file_edits",
    "rollback_session_state",
    "run_build_test",
    "session",
    "share_context",
    "symbol_search",
    "task_board",
    "task_list",
    "task_output",
    "task_stop",
    "type_hierarchy",
    "web_search",
];

fn runtime_env_builtin_registry() -> &'static astra_runtime_env::ToolRegistry {
    static REGISTRY: std::sync::OnceLock<astra_runtime_env::ToolRegistry> =
        std::sync::OnceLock::new();
    REGISTRY.get_or_init(astra_runtime_env::ToolRegistry::builtins)
}

fn local_runtime_tool_schemas(raw_schemas: Vec<Value>) -> Vec<Value> {
    let registry = runtime_env_builtin_registry();
    let binding = astra_runtime_env::RunBinding::local_developer(".", registry);
    astra_runtime_env::CapabilityResolver.filter_tool_schemas_for_binding(
        registry,
        raw_schemas,
        &binding,
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
    capacity_provider_coverage: Vec<astra_turn_core::introspect::CapacityProviderCoverageEntry>,
}

pub fn local_tool_schemas() -> Vec<Value> {
    local_runtime_tool_schemas(full_tool_schemas())
}

/// Plan-mode write guard tool list (CLI parity with
/// `runtime_tool_executor::is_plan_mode_blocked_tool`). While a plan is
/// in `phase=planning` these tools must be short-circuited: they all
/// mutate the world (filesystem, DB, git, GitHub), so allowing them
/// would let the model execute a plan it has not yet had approved.
/// Read-only tools (read_file, grep, glob, git(action=status/diff/log)) and
/// session-scoped authoring tools (`task`, memory_*) stay available so the
/// agent can keep authoring without mutating the external world.
pub(crate) fn is_plan_mode_blocked_tool(tool: &str, args: &Value) -> bool {
    astra_turn_core::plan_mode_policy::is_plan_mode_blocked_tool(tool, args)
}

fn git_stash_sub_action_args(args: &Value) -> Value {
    let sub_action = args.get("sub_action").and_then(Value::as_str);
    let Some(sub_action) = sub_action else {
        return args.clone();
    };

    let mut map = args.as_object().cloned().unwrap_or_default();
    map.insert("action".to_string(), Value::String(sub_action.to_string()));
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
use file_state::FileState;
pub(crate) use file_state::{ReadCoverage, ReadDedupKey};
pub(crate) use task_mgmt::TaskManager;

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
mod porcelain_status {
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
/// as an internal artifact and replaced with a preview. The artifact path is
/// not part of the workspace filesystem contract.
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

pub(crate) fn nonexecuted_tool_result_fields(
    disposition: astra_services::session_journal::ToolCallDisposition,
) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([
        (
            "result_class".to_string(),
            Value::String(astra_services::session_journal::NOOP_OR_CACHED_RESULT_CLASS.to_string()),
        ),
        (
            "disposition".to_string(),
            serde_json::to_value(disposition).expect("tool disposition must serialize"),
        ),
    ])
}

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

fn tool_result_from_cli_outcome(outcome: ToolExecutionOutcome) -> astra_tools::ToolResult {
    astra_tools::ToolResult {
        output: outcome.output,
        metadata: outcome.tool_result_fields,
        is_error: outcome.is_error,
        exit_semantics: None,
    }
}

struct EdgeToolRun {
    output: String,
    error_kind: Option<astra_core::ErrorKind>,
    tool_result_fields: Option<serde_json::Map<String, Value>>,
}

impl EdgeToolRun {
    fn ok(output: String) -> Self {
        Self {
            output,
            error_kind: None,
            tool_result_fields: None,
        }
    }

    fn error(output: String) -> Self {
        Self {
            output,
            error_kind: None,
            tool_result_fields: None,
        }
    }

    fn classified_error(output: String, kind: astra_core::ErrorKind) -> Self {
        Self {
            output,
            error_kind: Some(kind),
            tool_result_fields: None,
        }
    }

    fn failure_evidence(output: String, evidence: astra_core::ToolFailureEvidence) -> Self {
        let mut fields = serde_json::Map::new();
        fields.insert(
            "disposition".to_string(),
            serde_json::Value::String("rejected".to_string()),
        );
        if let Ok(value) = serde_json::to_value(&evidence) {
            fields.insert("recovery_evidence".to_string(), value);
        }
        Self {
            output,
            error_kind: Some(evidence.kind),
            tool_result_fields: Some(fields),
        }
    }

    fn with_tool_result_fields(mut self, fields: Option<serde_json::Map<String, Value>>) -> Self {
        self.tool_result_fields = fields;
        self
    }

    fn into_outcome(self) -> ToolExecutionOutcome {
        if let Some(outcome) = sandbox_denied_outcome_from_output(&self.output) {
            return outcome;
        }

        let EdgeToolRun {
            output,
            error_kind,
            tool_result_fields,
        } = self;

        let mut outcome = if error_kind.is_some() || cli_tool_output_is_error(&output) {
            ToolExecutionOutcome::error(output)
        } else {
            ToolExecutionOutcome::ok(output)
        };
        if let Some(fields) = tool_result_fields {
            let metadata = outcome
                .tool_result_fields
                .get_or_insert_with(serde_json::Map::new);
            metadata.extend(fields);
        }
        if let Some(kind) = error_kind {
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

#[derive(Debug, Clone)]
pub struct BgTaskOutputSearchSnapshot {
    pub kind: String,
    pub title: Option<String>,
    pub output: String,
    pub matching_lines: u64,
    pub truncated: bool,
    pub status: String,
    pub terminal: bool,
    pub output_ref: String,
}

pub(crate) enum BgTaskCommand {
    Kill {
        task_id: String,
        reply: tokio::sync::oneshot::Sender<Result<(), BackgroundTaskError>>,
    },
    GetOutputSince {
        task_id: String,
        offset: u64,
        max_bytes: usize,
        reply: tokio::sync::oneshot::Sender<Result<BgTaskOutputSnapshot, BackgroundTaskError>>,
    },
    SearchOutput {
        task_id: String,
        pattern: String,
        context_lines: usize,
        max_bytes: usize,
        reply:
            tokio::sync::oneshot::Sender<Result<BgTaskOutputSearchSnapshot, BackgroundTaskError>>,
    },
    List {
        reply: tokio::sync::oneshot::Sender<String>,
    },
}

#[cfg(not(test))]
const BG_TASK_COMMAND_REPLY_TIMEOUT_MS: u64 = 1_000;
#[cfg(test)]
const BG_TASK_COMMAND_REPLY_TIMEOUT_MS: u64 = 25;
#[cfg(not(test))]
const BG_TASK_WAIT_POLL_INTERVAL_MS: u64 = 500;
#[cfg(test)]
const BG_TASK_WAIT_POLL_INTERVAL_MS: u64 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgTaskReplyError {
    Closed,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BgTaskOutputReadMode {
    Current,
    Historical,
}

fn background_task_reply_timeout(timeout_ms: u64) -> Duration {
    Duration::from_millis(timeout_ms.clamp(1, BG_TASK_COMMAND_REPLY_TIMEOUT_MS))
}

async fn request_background_task_output_snapshot(
    bg_commands: &std::sync::Arc<std::sync::Mutex<Vec<BgTaskCommand>>>,
    task_id: &str,
    offset: u64,
    max_bytes: usize,
    reply_timeout: Duration,
) -> Result<BgTaskOutputSnapshot, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut commands = bg_commands.lock_recover();
        commands.push(BgTaskCommand::GetOutputSince {
            task_id: task_id.to_string(),
            offset,
            max_bytes,
            reply: tx,
        });
    }
    match await_bg_task_command_reply(rx, reply_timeout).await {
        Ok(Ok(snapshot)) => Ok(snapshot),
        Ok(Err(error)) => Err(format_background_task_error(&error)),
        Err(BgTaskReplyError::Closed) => Err(format_background_task_registry_closed(Some(task_id))),
        Err(BgTaskReplyError::TimedOut) => Err(format_background_task_output_registry_timeout(
            task_id,
            reply_timeout,
        )),
    }
}

async fn request_background_task_output_search(
    bg_commands: &std::sync::Arc<std::sync::Mutex<Vec<BgTaskCommand>>>,
    task_id: &str,
    pattern: &str,
    context_lines: usize,
    max_bytes: usize,
    reply_timeout: Duration,
) -> Result<BgTaskOutputSearchSnapshot, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    {
        let mut commands = bg_commands.lock_recover();
        commands.push(BgTaskCommand::SearchOutput {
            task_id: task_id.to_string(),
            pattern: pattern.to_string(),
            context_lines,
            max_bytes,
            reply: tx,
        });
    }
    match await_bg_task_command_reply(rx, reply_timeout).await {
        Ok(Ok(snapshot)) => Ok(snapshot),
        Ok(Err(error)) => Err(format_background_task_error(&error)),
        Err(BgTaskReplyError::Closed) => Err(format_background_task_registry_closed(Some(task_id))),
        Err(BgTaskReplyError::TimedOut) => Err(format_background_task_output_registry_timeout(
            task_id,
            reply_timeout,
        )),
    }
}

async fn background_task_output_view(
    bg_commands: &std::sync::Arc<std::sync::Mutex<Vec<BgTaskCommand>>>,
    task_id: &str,
    requested_offset: Option<u64>,
    max_bytes: usize,
    reply_timeout: Duration,
) -> Result<(u64, BgTaskOutputSnapshot), String> {
    let first_offset = requested_offset.unwrap_or(0);
    let first = request_background_task_output_snapshot(
        bg_commands,
        task_id,
        first_offset,
        max_bytes,
        reply_timeout,
    )
    .await?;
    if requested_offset.is_some() || first.kind != "shell" || first.total_bytes <= max_bytes as u64
    {
        return Ok((first_offset, first));
    }

    // An offset-free shell read is a current-status observation. Anchor it at
    // the latest bounded tail instead of replaying byte zero, which made models
    // paginate megabytes of historical test output just to answer "still running?".
    let tail_offset = first.total_bytes.saturating_sub(max_bytes as u64);
    let tail = request_background_task_output_snapshot(
        bg_commands,
        task_id,
        tail_offset,
        max_bytes,
        reply_timeout,
    )
    .await?;
    if tail.end_offset >= tail.total_bytes {
        return Ok((tail_offset, tail));
    }

    // A very noisy process may advance between the probe and tail read. Make
    // one bounded correction to stay current without turning the tool itself
    // into an unbounded follower.
    let latest_offset = tail.total_bytes.saturating_sub(max_bytes as u64);
    let latest = request_background_task_output_snapshot(
        bg_commands,
        task_id,
        latest_offset,
        max_bytes,
        reply_timeout,
    )
    .await?;
    Ok((latest_offset, latest))
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

fn format_background_task_error(error: &BackgroundTaskError) -> String {
    let (task_id, status, detail) = match error {
        BackgroundTaskError::NotFound { task_id } => (
            task_id.as_str(),
            "not_found",
            format!("Background task not found: {task_id}"),
        ),
        BackgroundTaskError::OutputArtifactMissing { task_id, path } => (
            task_id.as_str(),
            "output_missing",
            format!("Output artifact missing: {}", path.display()),
        ),
        BackgroundTaskError::OutputUnavailable { task_id, detail } => {
            (task_id.as_str(), "output_unavailable", detail.clone())
        }
        BackgroundTaskError::AlreadyTerminated { task_id } => {
            (task_id.as_str(), "already_terminal", error.to_string())
        }
        BackgroundTaskError::StaleHandle { task_id } => {
            (task_id.as_str(), "stale_handle", error.to_string())
        }
        BackgroundTaskError::CannotStop { task_id } => {
            (task_id.as_str(), "cannot_read", error.to_string())
        }
    };
    serde_json::json!({
        "ok": false,
        "kind": "background_task",
        "task_id": task_id,
        "status": status,
        "error": detail,
    })
    .to_string()
}

fn format_background_task_stop_error(error: &BackgroundTaskError) -> String {
    let (task_id, status, ok, terminal, detail) = match error {
        BackgroundTaskError::NotFound { task_id } => (
            task_id.as_str(),
            "not_found",
            false,
            false,
            format!("Background task not found: {task_id}"),
        ),
        BackgroundTaskError::AlreadyTerminated { task_id } => (
            task_id.as_str(),
            "already_terminal",
            true,
            true,
            format!("Background task {task_id} already finished."),
        ),
        BackgroundTaskError::StaleHandle { task_id } => (
            task_id.as_str(),
            "stale_handle",
            false,
            false,
            format!(
                "Background task {task_id} cannot be stopped because no live process handle is available."
            ),
        ),
        BackgroundTaskError::CannotStop { task_id } => (
            task_id.as_str(),
            "cannot_stop",
            false,
            false,
            format!("Background task {task_id} cannot be stopped in its current state."),
        ),
        BackgroundTaskError::OutputArtifactMissing { task_id, .. }
        | BackgroundTaskError::OutputUnavailable { task_id, .. } => (
            task_id.as_str(),
            "stop_failed",
            false,
            false,
            format!("Background task stop failed: {error}"),
        ),
    };
    let mut response = serde_json::json!({
        "ok": ok,
        "kind": "background_task",
        "task_id": task_id,
        "status": status,
        "terminal": terminal,
    });
    if ok {
        response["message"] = serde_json::Value::String(detail);
    } else {
        response["error"] = serde_json::Value::String(detail);
    }
    response.to_string()
}

fn format_background_task_argument_error(error: &str) -> String {
    serde_json::json!({
        "ok": false,
        "kind": "background_task",
        "status": "invalid_argument",
        "error": error,
    })
    .to_string()
}

fn format_background_task_registry_closed(task_id: Option<&str>) -> String {
    let mut response = serde_json::json!({
        "ok": false,
        "kind": if task_id.is_some() { "background_task" } else { "background_task_list" },
        "status": "registry_unavailable",
        "error": "background task registry is not available",
    });
    if let Some(task_id) = task_id {
        response["task_id"] = serde_json::Value::String(task_id.to_string());
    }
    response.to_string()
}

fn format_background_task_output(
    task_id: &str,
    offset: u64,
    snapshot: &BgTaskOutputSnapshot,
    read_mode: BgTaskOutputReadMode,
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
        format!("next_offset {}", snapshot.end_offset),
        format!("total {} bytes", snapshot.total_bytes),
        format!("{} total lines", snapshot.total_lines),
        format!(
            "terminal {}",
            if snapshot.terminal { "true" } else { "false" }
        ),
    ];
    if (read_mode == BgTaskOutputReadMode::Historical || kind != "shell")
        && snapshot.end_offset < snapshot.total_bytes
    {
        metadata_parts.push(format!(
            "next_call task_output(task_id='{task_id}', offset={}, block=false)",
            snapshot.end_offset
        ));
    } else if !snapshot.terminal && snapshot.status != "waiting_for_input" {
        metadata_parts.push(format!(
            "later_call task_output(task_id='{task_id}', block=false)"
        ));
        metadata_parts.push("do_not_poll_again_this_turn".to_string());
    } else if snapshot.status == "waiting_for_input" {
        metadata_parts.push("requires_input true".to_string());
    }
    if !snapshot.output_ref.trim().is_empty() {
        metadata_parts.push(format!("output_ref {}", snapshot.output_ref));
    }
    if kind == "shell" && snapshot.status == "failed" {
        metadata_parts.push("failure_cause unverified".to_string());
        metadata_parts.push("diagnostic_search_available true".to_string());
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
        let diagnosis = (kind == "shell" && snapshot.status == "failed").then_some(
            "\nFailure diagnosis is still open. Search this task's captured output with task_output(pattern=...) before assigning a cause; a failed status alone is not evidence that a test is flaky.",
        );
        return format!("{header}\n{state} · {metadata}{}", diagnosis.unwrap_or(""));
    }

    let chunk = snapshot.output.trim_end();
    let line_count = chunk.lines().count();
    let diagnosis = (kind == "shell" && snapshot.status == "failed").then_some(
        "\nFailure diagnosis is still open. Search this task's captured output with task_output(pattern=...) before assigning a cause; a failed status alone is not evidence that a test is flaky.",
    );
    format!(
        "{header}\n{line_count} new {} · {metadata} · {status_label}\nOutput chunk:\n{chunk}",
        if line_count == 1 { "line" } else { "lines" }
    ) + diagnosis.unwrap_or("")
}

fn format_background_task_output_search(
    task_id: &str,
    pattern: &str,
    context_lines: usize,
    snapshot: &BgTaskOutputSearchSnapshot,
) -> String {
    let kind = snapshot.kind.trim();
    let kind = if kind.is_empty() { "shell" } else { kind };
    let status_label = match snapshot.status.as_str() {
        "pending" => "pending",
        "running" => "still running",
        "waiting_for_input" => "needs input",
        "completed" => "completed",
        "failed" => "failed",
        "killed" => "killed",
        other => other,
    };
    let title = snapshot
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .map(|title| format!(" · title {}", title.trim()))
        .unwrap_or_default();
    let output_ref = if snapshot.output_ref.trim().is_empty() {
        String::new()
    } else {
        format!(" · output_ref {}", snapshot.output_ref)
    };
    let truncation = if snapshot.truncated {
        " · result_truncated true"
    } else {
        ""
    };
    let header = format!(
        "Search {kind} output {task_id}\npattern {pattern:?} · {} matching {} · context_lines {context_lines} · terminal {} · {status_label}{truncation}{title}{output_ref}",
        snapshot.matching_lines,
        if snapshot.matching_lines == 1 {
            "line"
        } else {
            "lines"
        },
        snapshot.terminal,
    );
    let evidence_contract = if snapshot.status == "failed" {
        "\nDiagnostic search returns evidence, not a classification. Call a test flaky only after a controlled rerun succeeds or equivalent repeatability evidence exists."
    } else {
        ""
    };
    if snapshot.output.trim().is_empty() {
        return format!("{header}\nNo matching output lines.{evidence_contract}");
    }
    format!(
        "{header}\nMatched output:\n{}{evidence_contract}",
        snapshot.output.trim_end()
    )
}

fn format_background_task_output_wait_timeout(
    task_id: &str,
    offset: u64,
    timeout_ms: u64,
    snapshot: &BgTaskOutputSnapshot,
) -> String {
    format!(
        "{}\nWait timed out after {timeout_ms}ms; the task is still non-terminal. Do not poll again in this turn; completion will be delivered automatically.",
        format_background_task_output(task_id, offset, snapshot, BgTaskOutputReadMode::Current)
    )
}

fn background_task_output_result_fields(
    task_id: &str,
    snapshot: &BgTaskOutputSnapshot,
    read_mode: BgTaskOutputReadMode,
    block: bool,
) -> serde_json::Map<String, Value> {
    let observation_mode = if block {
        "wait"
    } else {
        match read_mode {
            BgTaskOutputReadMode::Current => "current",
            BgTaskOutputReadMode::Historical => "historical",
        }
    };
    serde_json::Map::from_iter([(
        "background_task_observation".to_string(),
        serde_json::json!({
            "task_id": task_id,
            "task_kind": snapshot.kind,
            "status": snapshot.status,
            "terminal": snapshot.terminal,
            "mode": observation_mode,
        }),
    )])
}

fn background_task_output_search_result_fields(
    task_id: &str,
    pattern: &str,
    snapshot: &BgTaskOutputSearchSnapshot,
) -> serde_json::Map<String, Value> {
    serde_json::Map::from_iter([(
        "background_task_observation".to_string(),
        serde_json::json!({
            "task_id": task_id,
            "task_kind": snapshot.kind,
            "status": snapshot.status,
            "terminal": snapshot.terminal,
            "mode": "diagnostic",
            "pattern": pattern,
            "matching_lines": snapshot.matching_lines,
            "truncated": snapshot.truncated,
        }),
    )])
}

fn format_background_task_output_registry_timeout(task_id: &str, timeout: Duration) -> String {
    serde_json::json!({
        "ok": false,
        "kind": "background_task",
        "task_id": task_id,
        "status": "registry_timeout",
        "error": format!(
            "Background task registry did not respond within {}ms. The task may still be running.",
            duration_ms_u64(timeout)
        ),
    })
    .to_string()
}

fn format_background_task_stop_registry_timeout(task_id: &str, timeout: Duration) -> String {
    serde_json::json!({
        "ok": false,
        "kind": "background_task",
        "task_id": task_id,
        "status": "stop_status_unknown",
        "error": format!(
            "Background task registry did not respond within {}ms. The task may still be running; retry task_stop or task_list.",
            duration_ms_u64(timeout)
        ),
    })
    .to_string()
}

fn format_background_task_list_registry_timeout(timeout: Duration) -> String {
    serde_json::json!({
        "ok": false,
        "kind": "background_task_list",
        "status": "registry_timeout",
        "error": format!(
            "Timed out after {}ms waiting for the interactive session to answer. Background tasks may still be running; retry task_list later.",
            duration_ms_u64(timeout)
        ),
    })
    .to_string()
}

fn format_background_task_unavailable(cloud_session: bool, task_id: Option<&str>) -> String {
    let error = if cloud_session {
        "no edge runner is attached to this cloud session"
    } else {
        "local background tasks require an interactive CLI session"
    };
    let mut response = serde_json::json!({
        "ok": false,
        "kind": if task_id.is_some() { "background_task" } else { "background_task_list" },
        "status": "unavailable",
        "error": error,
    });
    if let Some(task_id) = task_id {
        response["task_id"] = serde_json::Value::String(task_id.to_string());
    }
    response.to_string()
}

fn background_task_status_is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "killed" | "unavailable")
}

fn background_task_status_should_wake_waiter(status: &str) -> bool {
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
    /// Shared-state circuit breaker for process-lived Memoria availability.
    memoria_circuit: astra_tools::memoria::MemoryCircuitBreaker,
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
    /// MCP routing snapshot. Manager and MCP schemas are installed together so
    /// `tool_search(select:mcp__*)` sees the same discovery snapshot as
    /// execution.
    mcp_runtime: std::sync::RwLock<EdgeMcpRuntimeSnapshot>,
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
    /// Session id for persisting self-modification state and serving `astra self`
    /// compatible diagnostics from inside the live agent loop.
    active_session_id: std::sync::Mutex<Option<String>>,
    /// Concrete model selected for the active turn/session. This is the
    /// source used by self-introspection tools; it is set by the CLI turn
    /// boundary and never inferred from a tool surface default.
    current_model: std::sync::RwLock<Option<String>>,
    /// Effective per-turn input budget for the active model/config. This is
    /// populated at the CLI turn boundary so first-round introspection does not
    /// need to infer context capacity from pressure percentages.
    current_effective_input_budget_tokens: std::sync::RwLock<Option<u64>>,
    /// Full provider context window from the server model registry for the
    /// active model. Kept separate from effective input budget so introspection
    /// can answer both questions without guessing.
    current_context_window_tokens: std::sync::RwLock<Option<u64>>,
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
    /// Shared implementation host for explicitly delegated tools.
    default_executor: astra_tools::executor::DefaultToolExecutor,
    /// Schemas declared by CLI-side providers except CLI MCP
    /// (server service, control plane, and CLI local executor). MCP schemas live
    /// in `mcp_runtime` so routing ownership and discovery data stay atomic.
    cli_local_provider_schemas: std::sync::RwLock<Vec<Value>>,
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
            cli_local_provider_schemas: std::sync::RwLock::new(Vec::new()),
            current_tool_surface: std::sync::RwLock::new(ToolSurfaceNames::default()),
            activated_deferred_tools: std::sync::RwLock::new(HashSet::new()),
            sandbox_policy: std::sync::RwLock::new(Some(sandbox)),
            preferred_repos: std::sync::Mutex::new(preferred_repos),
            budget_pressure: std::sync::Mutex::new(0.0),
            build_test_tracker: std::sync::Mutex::new(build_test::BuildTestTracker::new()),
            memoria_circuit: astra_tools::memoria::MemoryCircuitBreaker::default(),
            memoria_notified_down: std::sync::atomic::AtomicBool::new(false),
            file_state: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
            aggregate_output_bytes: std::sync::atomic::AtomicUsize::new(0),
            bash_progress_sink: std::sync::RwLock::new(None),
            passive_cargo_pending: AtomicBool::new(false),
            passive_tsc_pending: AtomicBool::new(false),
            passive_lsp: passive_lsp::PassiveLspManager::new(),
            mcp_runtime: std::sync::RwLock::new(EdgeMcpRuntimeSnapshot::default()),
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
            active_session_id: std::sync::Mutex::new(None),
            current_model: std::sync::RwLock::new(None),
            current_effective_input_budget_tokens: std::sync::RwLock::new(None),
            current_context_window_tokens: std::sync::RwLock::new(None),
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
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&schemas);
        let mut guard =
            rwlock_write_reset_on_poison(&self.current_tool_surface, "current_tool_surface");
        *guard = ToolSurfaceNames::installed(names, HashSet::new());
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

    /// Install schemas declared by CLI-side providers so
    /// `tool_search(select:NAME)` can resolve provider-owned dynamic tools. MCP
    /// schemas are rejected here: they require an MCP runtime binding and must
    /// be installed through `install_mcp_bundle`.
    ///
    /// Poison handling: provider schemas are a rebuildable cache. Reset cached
    /// state on poison instead of reusing possibly half-written inner data.
    pub fn set_cli_local_provider_schemas(&self, schemas: Vec<Value>) {
        let mut dropped_mcp_schema_names = Vec::new();
        let schemas = schemas
            .into_iter()
            .filter(|schema| {
                let Some(name) = astra_turn_core::tool::schema::tool_schema_name(schema) else {
                    return true;
                };
                if astra_runtime_env::is_mcp_namespaced_tool_name(name) {
                    dropped_mcp_schema_names.push(name.to_string());
                    return false;
                }
                true
            })
            .collect();
        if !dropped_mcp_schema_names.is_empty() {
            tracing::warn!(
                target: "astra.cli.capacity_provider",
                dropped = ?dropped_mcp_schema_names,
                "dropped MCP schemas from CLI local provider; install them through the MCP runtime provider"
            );
        }

        let mut guard = rwlock_write_reset_on_poison(
            &self.cli_local_provider_schemas,
            "cli_local_provider_schemas",
        );
        *guard = schemas;
    }

    /// Install MCP routing and schemas from one discovery snapshot.
    pub fn install_mcp_bundle(
        &mut self,
        manager: std::sync::Arc<tokio::sync::RwLock<crate::mcp_client::McpClientManager>>,
        schemas: Vec<Value>,
    ) {
        let mut guard = rwlock_write_reset_on_poison(&self.mcp_runtime, "mcp_runtime");
        *guard = EdgeMcpRuntimeSnapshot {
            manager: Some(manager),
            schemas,
        };
    }

    /// Install the visible `tools[]` names for the current LLM request.
    pub fn set_current_visible_tool_schemas(&self, schemas: &[Value]) {
        let visible_schemas = self.runtime_bound_tool_schemas(schemas.to_vec());
        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&visible_schemas);
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
        let visible_schemas = self.runtime_bound_tool_schemas(visible_schemas.to_vec());
        let visible = astra_turn_core::tool::schema::tool_names_from_schemas(&visible_schemas);
        let activatable = self.runtime_bound_tool_names(activatable_names);
        let mut guard =
            rwlock_write_reset_on_poison(&self.current_tool_surface, "current_tool_surface");
        *guard = ToolSurfaceNames::installed(visible, activatable);
    }

    pub(crate) fn restore_activated_deferred_tool_names_for_session(&self, names: &[String]) {
        let restored: HashSet<String> = names
            .iter()
            .map(|name| name.trim())
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .collect();

        {
            let mut surface = rwlock_write_reset_on_poison(
                &self.current_tool_surface,
                "current_tool_surface_restore_deferred_activation",
            );
            if !restored.is_empty() && matches!(*surface, ToolSurfaceNames::Uninstalled) {
                *surface = ToolSurfaceNames::installed(HashSet::new(), HashSet::new());
            }
        }

        *rwlock_write_reset_on_poison(
            &self.activated_deferred_tools,
            "activated_deferred_tools_restore",
        ) = restored;
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

    /// Snapshot of the names that the model's `<deferred-tools>` manifest
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
            |name| self.tool_has_public_schema_runtime_binding(name),
        )
    }

    fn current_tool_surface_snapshot(&self, label: &str) -> ToolSurfaceNames {
        rwlock_read_clone_or_default(&self.current_tool_surface, label)
    }

    fn runtime_bound_tool_names(&self, names: HashSet<String>) -> HashSet<String> {
        astra_turn_core::tool::deferred_activation::runtime_bound_tool_names(names, |name| {
            self.tool_has_public_schema_runtime_binding(name)
        })
    }

    pub(crate) fn runtime_bound_tool_schemas(&self, schemas: Vec<Value>) -> Vec<Value> {
        schemas
            .into_iter()
            .filter(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_some_and(|name| self.tool_has_public_schema_runtime_binding(name))
            })
            .collect()
    }

    fn runtime_available_tool_schemas(&self) -> Vec<Value> {
        let mut schemas = local_tool_schemas();
        schemas.extend(self.provider_owned_schemas_snapshot("shared_tool_executor_surface"));
        let mut seen = HashSet::new();
        let mut schemas: Vec<Value> = self
            .runtime_bound_tool_schemas(schemas)
            .into_iter()
            .filter(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_some_and(|name| seen.insert(name.to_string()))
            })
            .collect();
        astra_core::tool_schema::sort_tool_schemas_by_name(&mut schemas);
        schemas
    }

    fn tool_has_public_schema_runtime_binding(&self, name: &str) -> bool {
        if astra_runtime_env::is_mcp_namespaced_tool_name(name) {
            return self.mcp_tool_has_runtime_binding(name);
        }

        let registry = runtime_env_builtin_registry();
        if let Some(spec) = registry.get(name) {
            if !spec.load_policy.is_public_schema_policy() {
                return false;
            }
            return self.tool_has_runtime_binding(name);
        }

        self.cli_local_provider_schema_has_name(name) && self.tool_has_runtime_binding(name)
    }

    fn tool_has_runtime_binding(&self, name: &str) -> bool {
        self.tool_has_runtime_binding_for_call(name, &Value::Null)
    }

    fn tool_has_runtime_binding_for_call(&self, name: &str, args: &Value) -> bool {
        if self.runtime_environment_tool_denial(name, args).is_some() {
            return false;
        }

        let Some(meta) = astra_turn_core::tool::registry::meta::tool_meta(name) else {
            return self.cli_declared_local_tool_has_name(name)
                || self.cli_local_provider_schema_has_name(name);
        };
        meta.requires
            .iter()
            .all(|capability| self.capability_has_runtime_binding(*capability))
    }

    fn runtime_environment_tool_denial(
        &self,
        name: &str,
        args: &Value,
    ) -> Option<astra_runtime_env::ToolUnavailableReason> {
        let registry = runtime_env_builtin_registry();
        if registry.get(name).is_none() && !astra_runtime_env::is_mcp_namespaced_tool_name(name) {
            return None;
        }
        let binding = self.runtime_environment_binding_for_tool(name, registry);
        astra_runtime_env::CapabilityResolver
            .check_tool_call_for_surface(
                registry,
                name,
                args,
                &binding.capabilities,
                &binding.tool_surface,
            )
            .err()
    }

    fn runtime_environment_binding_for_tool(
        &self,
        name: &str,
        registry: &astra_runtime_env::ToolRegistry,
    ) -> astra_runtime_env::RunBinding {
        if astra_runtime_env::is_mcp_namespaced_tool_name(name)
            && self.mcp_tool_has_runtime_binding(name)
        {
            let providers = vec![astra_runtime_env::mcp_provider(
                "cli-mcp",
                [name.to_string()],
            )];
            return astra_runtime_env::RunBinding::resolve_with_provider_declarations(
                astra_runtime_env::WorkspaceBinding::none(),
                astra_runtime_env::ExecutorBinding {
                    kind: astra_runtime_env::ExecutorBindingKind::Mcp,
                    executor_id: "cli-mcp".to_string(),
                    display_name: "CLI MCP server".to_string(),
                    transport: astra_runtime_env::ToolTransportKind::McpHttp,
                    status: astra_runtime_env::ExecutorStatus::Online,
                },
                astra_runtime_env::RuntimeBinding::none(),
                astra_runtime_env::PolicyIntent::cloud_control_plane(),
                registry,
                &providers,
            );
        }

        astra_runtime_env::RunBinding::local_developer(
            self.project_root.display().to_string(),
            registry,
        )
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

    fn cli_local_provider_schema_has_name(&self, name: &str) -> bool {
        self.cli_local_provider_schemas_snapshot("cli_local_provider_schemas_runtime_binding")
            .iter()
            .any(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_some_and(|schema_name| schema_name == name)
            })
    }

    fn mcp_tool_has_runtime_binding(&self, name: &str) -> bool {
        let runtime = self.mcp_runtime_snapshot("mcp_runtime_binding");
        let schema_declared = runtime.schemas.iter().any(|schema| {
            astra_turn_core::tool::schema::tool_schema_name(schema)
                .is_some_and(|schema_name| schema_name == name)
        });
        if !schema_declared {
            return false;
        }
        let Some(manager) = &runtime.manager else {
            return false;
        };
        match manager.try_read() {
            Ok(manager) => manager.find_tool_by_mcp_name(name).is_some(),
            Err(_) => {
                tracing::debug!(
                    target: "astra_cli::tool_binding",
                    tool = %name,
                    "MCP registry busy while checking runtime binding; preserving schema-declared availability"
                );
                true
            }
        }
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
        if self.tool_has_runtime_binding_for_call(name, args) {
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
            direct_deferred_call_activation_message, tool_not_admitted_message,
        };

        match classify_direct_deferred_call(name, can_select, |tool_name| {
            self.tool_has_runtime_binding(tool_name)
        }) {
            DirectDeferredCallAdmission::Activate {
                name: activated_name,
            } => {
                // Direct deferred call: the model called a tool advertised in
                // `<deferred-tools>` without first selecting it via
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
                return Some(EdgeToolRun::error(direct_deferred_call_activation_message(
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

    pub(crate) fn runtime_bound_provider_owned_schemas_excluding(
        &self,
        restricted_tools: &HashSet<String>,
    ) -> Vec<Value> {
        let provider_schemas: Vec<Value> = self
            .provider_owned_schemas_snapshot("provider_owned_schemas_deferred_manifest")
            .into_iter()
            .filter(|schema| {
                astra_turn_core::tool::schema::tool_schema_name(schema)
                    .is_none_or(|name| !restricted_tools.contains(name))
            })
            .collect();
        self.runtime_bound_tool_schemas(provider_schemas)
    }

    pub(super) fn provider_owned_schemas_snapshot(&self, label: &str) -> Vec<Value> {
        let mut schemas = self.cli_local_provider_schemas_snapshot(label);
        schemas.extend(
            self.mcp_runtime_snapshot("mcp_runtime_schema_snapshot")
                .schemas,
        );
        schemas
    }

    fn cli_local_provider_schemas_snapshot(&self, label: &str) -> Vec<Value> {
        rwlock_read_clone_or_default(&self.cli_local_provider_schemas, label)
    }

    pub(super) fn mcp_runtime_snapshot(&self, label: &str) -> EdgeMcpRuntimeSnapshot {
        rwlock_read_clone_or_default(&self.mcp_runtime, label)
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
            if let Ok(mut budget) = self.current_effective_input_budget_tokens.write() {
                *budget = None;
            }
            if let Ok(mut window) = self.current_context_window_tokens.write() {
                *window = None;
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

    pub fn set_current_effective_input_budget_tokens(&self, tokens: u64) {
        if let Ok(mut guard) = self.current_effective_input_budget_tokens.write() {
            *guard = (tokens > 0).then_some(tokens);
        }
    }

    pub fn set_current_context_window_tokens(&self, tokens: u64) {
        if let Ok(mut guard) = self.current_context_window_tokens.write() {
            *guard = (tokens > 0).then_some(tokens);
        }
    }

    fn current_model(&self) -> Option<String> {
        self.current_model.read().ok().and_then(|g| g.clone())
    }

    fn current_effective_input_budget_tokens(&self) -> Option<u64> {
        self.current_effective_input_budget_tokens
            .read()
            .ok()
            .and_then(|g| *g)
    }

    fn current_context_window_tokens(&self) -> Option<u64> {
        self.current_context_window_tokens
            .read()
            .ok()
            .and_then(|g| *g)
    }

    fn memory_args_with_context(&self, args: &Value) -> Value {
        let mut clean_args = args.clone();
        if let Some(obj) = clean_args.as_object_mut() {
            obj.remove("action");
            // Inject the active session id so memory targets and session-scoped
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

    /// Build a task context hint by delegating to the data-layer TaskManager.
    /// The result flows through the standard `plan_resume_hint` →
    /// `ExternalSources.plan_context` pipeline instead of polluting
    /// `append_system_prompt`.
    pub async fn build_task_context_hint(&self) -> Option<String> {
        self.task_manager.build_active_task_context().await
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

    // ─── Plan-mode write guard (parity with runtime_tool_executor) ───────────
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
        plan.get("status").and_then(Value::as_str)
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

    fn validate_task_tool_args_for_action(action: &str, args: &Value) -> Result<(), String> {
        astra_tools::task_tool_contract::validate_runtime_task_tool_args_for_action(action, args)
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
        let json_body = {
            let pos = output.find('{')?;
            &output[pos..]
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
        //    like an explicit local permission choice. No cloud row is created and no error
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
        // 2. `/allow plan`: only flips the local
        //    `perm_manager` to `Plan`. There is no cloud row, no
        //    server-side guard, and the user expects exiting to be
        //    purely local — zero network calls.
        //
        // Conflating both broke session d9b5119f: the user pressed
        // `/allow plan`, the model produced a plan, called exit_plan_mode,
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
    /// `/allow plan` flow.
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

    /// `exit_plan_mode` here is a permission-state pivot driven by
    /// the plan-review overlay. When the user approves, writes unlock
    /// and the host advances to the chosen execution mode.
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
            let plan_suffix = plan_markdown
                .filter(|plan| !plan.is_empty())
                .map(|plan| format!(" Plan recorded:\n{plan}"))
                .unwrap_or_default();
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
        if let Err(error) = crate::cli::chat_stream::enqueue_interactive_request(
            &tx,
            PlanReviewRequest {
                plan_markdown: plan_body,
                response_tx,
            },
        ) {
            return Err(format!(
                "Error: exit_plan_mode cannot open its review because {error}."
            ));
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
        let public_args = astra_tools::task_tool_contract::strip_runtime_private_task_fields(args);
        let output = self.task_manager.create(&public_args).await;
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
                    "task_board:create:{}",
                    public_args
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
            self.record_task_lifecycle_event("create", &public_args, &output);
        }
        output
    }

    async fn execute_task_tool_args(&self, args: &Value) -> String {
        let action = match astra_tools::task_tool_contract::task_action_from_args(args) {
            Ok(action) => action,
            Err(error) => return format!("Error: {error}"),
        };
        match action {
            "create" => match Self::validate_task_tool_args_for_action("create", args) {
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
            "update" => match Self::validate_task_tool_args_for_action("update", args) {
                Ok(()) => self.task_action_update(args).await,
                Err(error) => format!("Error: {error}"),
            },
            "stop" => match Self::validate_task_tool_args_for_action("stop", args) {
                Ok(()) => self.task_action_stop(args).await,
                Err(error) => format!("Error: {error}"),
            },
            "list_user" => match Self::validate_task_tool_args_for_action("list_user", args) {
                Ok(()) => self.task_list_user(args).await,
                Err(error) => format!("Error: {error}"),
            },
            "adopt" => match Self::validate_task_tool_args_for_action("adopt", args) {
                Ok(()) => self.task_adopt(args).await,
                Err(error) => format!("Error: {error}"),
            },
            "archive" => match Self::validate_task_tool_args_for_action("archive", args) {
                Ok(()) => self.task_action_archive(args).await,
                Err(error) => format!("Error: {error}"),
            },
            other => match Self::validate_task_tool_args_for_action(other, args) {
                Ok(()) => format!(
                    "Error: {}",
                    astra_tools::task_tool_contract::task_unknown_action_message(other)
                ),
                Err(error) => format!("Error: {error}"),
            },
        }
    }

    async fn task_list(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("list", args).await {
            return output;
        }
        let public_args = astra_tools::task_tool_contract::strip_runtime_private_task_fields(args);
        self.task_manager.list(&public_args).await
    }
    async fn task_get(&self, args: &Value) -> String {
        if let Some(output) = self.route_task_action("get", args).await {
            return output;
        }
        let public_args = astra_tools::task_tool_contract::strip_runtime_private_task_fields(args);
        self.task_manager.get(&public_args).await
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
        let public_args = astra_tools::task_tool_contract::strip_runtime_private_task_fields(args);
        let output = self.task_manager.update(&public_args).await;
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
                    "task_board:update:{}",
                    public_args
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
            self.record_task_lifecycle_event("update", &public_args, &output);
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
        let public_args = astra_tools::task_tool_contract::strip_runtime_private_task_fields(args);
        let output = self.task_manager.stop(&public_args).await;
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
                    "task_board:stop:{}",
                    public_args
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("task")
                ),
            );
            self.record_task_lifecycle_event("stop", &public_args, &output);
        }
        output
    }

    /// `task_board(action='list_user')` — cross-session active list. Cloud
    /// only: in-memory mode by definition has only one session, so
    /// the cross-session question is meaningless without a backing
    /// store that aggregates across users.
    async fn task_list_user(&self, args: &Value) -> String {
        let status = match Self::normalize_task_user_status(args) {
            Ok(status) => status,
            Err(err) => return err,
        };
        let Some(cloud_base) = self.cloud_base.clone() else {
            return "Error: task_board(action='list_user') requires a cloud connection. \
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

    /// `task_board(action='adopt', source_session_id, task_id)` — bring a
    /// task from another of the user's sessions into the current
    /// session. Server-side it copies the row's title/description/
    /// metadata into a fresh todo here and marks the source migrated
    /// so the user doesn't see it twice. Cloud-only.
    async fn task_adopt(&self, args: &Value) -> String {
        if self.cloud_base.is_none() {
            return "Error: task_board(action='adopt') requires a cloud connection.".to_string();
        }
        // Adopt is a write — route through the same execute endpoint.
        // Server-side dispatch will reject if source isn't owned by
        // the same user (auth check via SessionService).
        match self.route_task_action("adopt", args).await {
            Some(output) => output,
            None => "Error: cannot adopt task without an active session id".to_string(),
        }
    }

    /// `task_board(action='archive', task_id?)` — either archive one
    /// current-session task immediately, or bulk-archive stale
    /// completed history in the current session.
    async fn task_action_archive(&self, args: &Value) -> String {
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
        let public_args = astra_tools::task_tool_contract::strip_runtime_private_task_fields(args);
        let output = self.task_manager.archive(&public_args).await;
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
                    "task_board:archive:{}",
                    public_args
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("bulk")
                ),
            );
            self.record_task_lifecycle_event("archive", &public_args, &output);
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
                return self
                    .attach_recoverable_fanouts_to_task_list(cached.clone())
                    .await;
            }
            // Cache not yet populated (first call before the event
            // loop has rendered). Fall through to the queue path so
            // we still return a valid response.
        }
        // Fallback: queue path for when no cache is available
        let Some(ref bg_commands) = self.bg_task_commands else {
            if let Some(addendum) = self.recoverable_fanout_task_list_addendum().await {
                return Self::empty_background_task_list_with_fanouts(&addendum);
            }
            return format_background_task_unavailable(self.cloud_base.is_some(), None);
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut cmds = bg_commands.lock_recover();
            cmds.push(BgTaskCommand::List { reply: tx });
        }
        let reply_timeout = background_task_reply_timeout(BG_TASK_COMMAND_REPLY_TIMEOUT_MS);
        match await_bg_task_command_reply(rx, reply_timeout).await {
            Ok(output) => self.attach_recoverable_fanouts_to_task_list(output).await,
            Err(BgTaskReplyError::Closed) => format_background_task_registry_closed(None),
            Err(BgTaskReplyError::TimedOut) => {
                format_background_task_list_registry_timeout(reply_timeout)
            }
        }
    }

    async fn attach_recoverable_fanouts_to_task_list(&self, output: String) -> String {
        let Some(addendum) = self.recoverable_fanout_task_list_addendum().await else {
            return output;
        };
        if output.trim() == "<background_tasks count=\"0\" />" {
            return Self::empty_background_task_list_with_fanouts(&addendum);
        }
        if let Some(close_tag) = output.rfind("</background_tasks>") {
            let mut merged = String::with_capacity(output.len() + addendum.len() + 2);
            merged.push_str(&output[..close_tag]);
            merged.push('\n');
            merged.push_str(&addendum);
            merged.push('\n');
            merged.push_str(&output[close_tag..]);
            return merged;
        }
        format!("{output}\n{addendum}")
    }

    fn empty_background_task_list_with_fanouts(addendum: &str) -> String {
        format!(
            "<background_tasks count=\"0\" active_task_semantics=\"no active shell background tasks; recoverable terminal agent_fanout results may still exist\">\n{addendum}\n</background_tasks>"
        )
    }

    async fn recoverable_fanout_task_list_addendum(&self) -> Option<String> {
        let ctx = self.spawn_context.as_ref()?;
        let groups: Vec<_> = ctx
            .spawner
            .list_fanout_groups()
            .await
            .into_iter()
            .filter(|group| {
                let summary = group.summary();
                group.is_terminal() || summary.active > 0
            })
            .collect();
        if groups.is_empty() {
            return None;
        }

        const MAX_GROUPS_IN_TASK_LIST: usize = 8;
        let visible_count = groups.len().min(MAX_GROUPS_IN_TASK_LIST);
        let mut xml = format!(
            "<agent_fanouts count=\"{}\" visible=\"{}\" semantics=\"recoverable terminal fanout results are independent from active background shell tasks\">",
            groups.len(),
            visible_count,
        );
        for group in groups.iter().take(MAX_GROUPS_IN_TASK_LIST) {
            let summary = group.summary();
            let status = if group.is_terminal() {
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
            let get_results_call = format!(
                "agent_fanout(action='get_results', group_id='{}')",
                group.group_id
            );
            let task_output_call = format!("task_output(task_id='{}')", group.group_id);
            let instruction = format!(
                "Recover existing results with {get_results_call} or {task_output_call}. Do not rerun solely because background_tasks count is zero."
            );
            xml.push_str(&format!(
                "<agent_fanout id=\"{}\" title=\"{}\" status=\"{}\" terminal=\"{}\" active=\"{}\" completed=\"{}\" failed=\"{}\" uncollected=\"{}\" result_ref=\"{}\" get_results_call=\"{}\" task_output_call=\"{}\" instruction=\"{}\" />",
                Self::xml_attr(&group.group_id),
                Self::xml_attr(&group.title),
                status,
                group.is_terminal(),
                summary.active,
                summary.completed,
                summary.failed,
                summary.uncollected,
                Self::xml_attr(&format!("agent_fanout:{}", group.group_id)),
                Self::xml_attr(&get_results_call),
                Self::xml_attr(&task_output_call),
                Self::xml_attr(&instruction),
            ));
        }
        if groups.len() > MAX_GROUPS_IN_TASK_LIST {
            xml.push_str(&format!(
                "<truncated hidden=\"{}\" />",
                groups.len() - MAX_GROUPS_IN_TASK_LIST
            ));
        }
        xml.push_str("</agent_fanouts>");
        Some(xml)
    }

    fn xml_attr(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('"', "&quot;")
            .replace('\'', "&apos;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }

    #[cfg(test)]
    async fn task_output(&self, args: &Value) -> String {
        let mut fields = None;
        self.task_output_with_fields(args, &mut fields).await
    }

    async fn task_output_with_fields(
        &self,
        args: &Value,
        tool_result_fields: &mut Option<serde_json::Map<String, Value>>,
    ) -> String {
        let task_id = match background_task_id_arg(args) {
            Ok(Some(id)) => id,
            Err(error) => return format_background_task_argument_error(error),
            Ok(None) => return format_background_task_argument_error("task_id is required"),
        };
        let block = args.get("block").and_then(Value::as_bool).unwrap_or(false);
        let requested_offset = args.get("offset").and_then(Value::as_u64);
        let pattern = match args.get("pattern") {
            None => None,
            Some(Value::String(pattern)) => {
                let pattern = pattern.trim();
                if pattern.is_empty() {
                    return format_background_task_argument_error("pattern must not be empty");
                }
                if pattern.chars().count() > 512 {
                    return format_background_task_argument_error(
                        "pattern must be at most 512 characters",
                    );
                }
                if pattern.contains(['\r', '\n']) {
                    return format_background_task_argument_error("pattern must be a single line");
                }
                Some(pattern.to_string())
            }
            Some(_) => {
                return format_background_task_argument_error("pattern must be a string");
            }
        };
        if pattern.is_some() && (block || requested_offset.is_some()) {
            return format_background_task_argument_error(
                "pattern cannot be combined with block or offset",
            );
        }
        let read_mode = if requested_offset.is_some() {
            BgTaskOutputReadMode::Historical
        } else {
            BgTaskOutputReadMode::Current
        };
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(8_192)
            .clamp(1, 65_536) as usize;
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(30_000)
            .clamp(1, 300_000);
        let context_lines = args
            .get("context_lines")
            .and_then(Value::as_u64)
            .unwrap_or(3)
            .min(20) as usize;

        if pattern.is_none() {
            if let Some(output) = self
                .fanout_group_task_output_response(
                    &task_id,
                    requested_offset.unwrap_or(0),
                    max_bytes,
                    read_mode,
                    None,
                )
                .await
            {
                return output;
            }
        }

        let Some(ref bg_commands) = self.bg_task_commands else {
            return format_background_task_unavailable(self.cloud_base.is_some(), Some(&task_id));
        };

        let reply_timeout = background_task_reply_timeout(timeout_ms);
        if let Some(pattern) = pattern.as_deref() {
            return match request_background_task_output_search(
                bg_commands,
                &task_id,
                pattern,
                context_lines,
                max_bytes,
                reply_timeout,
            )
            .await
            {
                Ok(snapshot) => {
                    *tool_result_fields = Some(background_task_output_search_result_fields(
                        &task_id, pattern, &snapshot,
                    ));
                    format_background_task_output_search(
                        &task_id,
                        pattern,
                        context_lines,
                        &snapshot,
                    )
                }
                Err(error) => error,
            };
        }
        if !block {
            return match background_task_output_view(
                bg_commands,
                &task_id,
                requested_offset,
                max_bytes,
                reply_timeout,
            )
            .await
            {
                Ok((offset, snapshot)) => {
                    *tool_result_fields = Some(background_task_output_result_fields(
                        &task_id, &snapshot, read_mode, false,
                    ));
                    format_background_task_output(&task_id, offset, &snapshot, read_mode)
                }
                Err(error) => error,
            };
        }

        // Blocking is owned by the runtime, not by repeated model calls. Take
        // one baseline, then wait for terminal state (or the bounded timeout),
        // ignoring ordinary output growth while the TUI continues to project it.
        let probe_offset = requested_offset.unwrap_or(0);
        let mut latest = match request_background_task_output_snapshot(
            bg_commands,
            &task_id,
            probe_offset,
            max_bytes,
            reply_timeout,
        )
        .await
        {
            Ok(snapshot) => snapshot,
            Err(error) => return error,
        };
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            if background_task_status_should_wake_waiter(&latest.status) {
                return match background_task_output_view(
                    bg_commands,
                    &task_id,
                    requested_offset,
                    max_bytes,
                    reply_timeout,
                )
                .await
                {
                    Ok((offset, snapshot)) => {
                        *tool_result_fields = Some(background_task_output_result_fields(
                            &task_id, &snapshot, read_mode, true,
                        ));
                        format_background_task_output(&task_id, offset, &snapshot, read_mode)
                    }
                    Err(error) => error,
                };
            }

            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return match background_task_output_view(
                    bg_commands,
                    &task_id,
                    requested_offset,
                    max_bytes,
                    reply_timeout,
                )
                .await
                {
                    Ok((offset, snapshot)) => {
                        *tool_result_fields = Some(background_task_output_result_fields(
                            &task_id, &snapshot, read_mode, true,
                        ));
                        if background_task_status_should_wake_waiter(&snapshot.status) {
                            format_background_task_output(&task_id, offset, &snapshot, read_mode)
                        } else {
                            format_background_task_output_wait_timeout(
                                &task_id, offset, timeout_ms, &snapshot,
                            )
                        }
                    }
                    Err(error) => error,
                };
            }

            let sleep_for = Duration::from_millis(BG_TASK_WAIT_POLL_INTERVAL_MS).min(remaining);
            tokio::time::sleep(sleep_for).await;
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining <= Duration::from_millis(BG_TASK_WAIT_POLL_INTERVAL_MS) {
                if !remaining.is_zero() {
                    tokio::time::sleep(remaining).await;
                }
                continue;
            }
            let next_probe_offset = latest.total_bytes;
            latest = match request_background_task_output_snapshot(
                bg_commands,
                &task_id,
                next_probe_offset,
                max_bytes,
                reply_timeout.min(remaining),
            )
            .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => return error,
            };
        }
    }

    async fn fanout_group_task_output_response(
        &self,
        task_id: &str,
        offset: u64,
        max_bytes: usize,
        read_mode: BgTaskOutputReadMode,
        miss_reason: Option<&str>,
    ) -> Option<String> {
        match self
            .fanout_group_task_output_snapshot(task_id, offset, max_bytes)
            .await
        {
            Some(snapshot) => Some(format_background_task_output(
                task_id, offset, &snapshot, read_mode,
            )),
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
            "recovery": {
                "result_ref": format!("agent_fanout:{}", group.group_id),
                "task_output_id": &group.group_id,
                "get_results_call": format!("agent_fanout(action='get_results', group_id='{}')", group.group_id),
                "task_output_call": format!("task_output(task_id='{}')", group.group_id),
                "active_task_list_empty_does_not_mean_results_missing": true,
                "do_not_rerun_when_user_asks_for_results": true,
            },
            "hint": "This id belongs to a recoverable agent_fanout group, not a shell background task. Use agent_fanout(action='get_results', group_id=...) for full slot results.",
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
        let task_id = match background_task_id_arg(args) {
            Ok(Some(id)) => id,
            Err(error) => return format_background_task_argument_error(error),
            Ok(None) => return format_background_task_argument_error("task_id is required"),
        };
        let Some(ref bg_commands) = self.bg_task_commands else {
            return format_background_task_unavailable(self.cloud_base.is_some(), Some(&task_id));
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
            Ok(Ok(())) => serde_json::json!({
                "ok": true,
                "kind": "background_task",
                "task_id": task_id,
                "status": "stop_requested",
                "terminal": false,
                "message": "Cancellation was accepted; terminal status will be surfaced automatically.",
            })
            .to_string(),
            Ok(Err(e)) => format_background_task_stop_error(&e),
            Err(BgTaskReplyError::Closed) => {
                format_background_task_registry_closed(Some(&task_id))
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

    // ── Session history: recall past conversation turns ──────────────────────

    fn render_session_history(&self, args: &Value) -> String {
        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => return "Error: no active session.".to_string(),
        };
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(10) as usize;
        let query = args
            .get("pattern")
            .or_else(|| args.get("query"))
            .and_then(Value::as_str)
            .unwrap_or("");

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

    fn render_session_history_around(&self, args: &Value) -> String {
        if args.get("item_seq").and_then(Value::as_u64).is_none() {
            return "Error: history_around requires item_seq.".to_string();
        }
        "Error: session history_around requires server-backed transcript item_seq support in this runtime. Use history_page or history_search in local CLI mode.".to_string()
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

        let request = astra_turn_core::introspect::IntrospectRequest::from_args(args);

        if !request.format.is_json() && request.source_policy.allows_edge_local_artifacts() {
            match request.facet {
                astra_core::ObservationFacet::Cache => return self.handle_introspect_cache(),
                astra_core::ObservationFacet::SessionMemory => {
                    return self.render_session_memory_introspect();
                }
                _ => {}
            }
        }

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
        let mut snap = match snapshot {
            Some(mut snapshot) => {
                astra_turn_core::introspect::mark_snapshot_age(
                    &mut snapshot,
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Acquire),
                );
                snapshot
            }
            None => astra_turn_core::introspect::IntrospectSnapshot::default(),
        };
        if snap.current_model.is_none() {
            snap.current_model = self.current_model();
        }
        if snap.effective_input_budget_tokens == 0 {
            snap.effective_input_budget_tokens = self
                .current_effective_input_budget_tokens()
                .unwrap_or_default();
        }
        if snap.context_window_tokens == 0 {
            snap.context_window_tokens = self.current_context_window_tokens().unwrap_or_default();
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
            for advisory in astra_turn_core::injection_tracking::stale_channel_advisories(
                &snap.injection_freshness,
            ) {
                if !snap.alerts.iter().any(|existing| existing == &advisory) {
                    snap.alerts.push(advisory);
                }
            }
        }

        astra_turn_core::introspect::render_introspect_request(&snap, &request)
    }

    /// Render `introspect facet=session_memory`. Answers the
    /// recurring question "what did astra extract this session, and
    /// what did the last compaction inject?" without dumping enough
    /// content to pressure context.
    ///
    /// Extraction events and prompt-placement traces are reconstructed from
    /// the durable local journal. This keeps introspection aligned with the
    /// cross-process source of truth instead of a second in-memory event copy.
    fn render_session_memory_introspect(&self) -> String {
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
        let body = match self.load_active_session_memory_journal() {
            Ok(Some((session_id, events))) => {
                Self::render_session_memory_journal_fallback(&session_id, &events).unwrap_or_else(
                    || {
                        "# session-memory diagnostics\n\n\
                         source: local_journal\n\
                         No session-memory events or context-assembly traces recorded yet."
                            .to_string()
                    },
                )
            }
            Ok(None) => "# session-memory diagnostics\n\nNo active session journal.".to_string(),
            Err(error) => format!("# session-memory diagnostics\n\njournal unavailable: {error}"),
        };
        Self::prepend_session_memory_surface_status(surface_block.as_deref(), &body)
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

        let mut out = String::from("# session-memory diagnostics\n\n");
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

    /// `introspect(facet="cache")` — prefer recent `llm_capture_*.json`
    /// files for full cache-diagnosis rules, then fall back to the live
    /// in-memory recent-round ring for per-round token/cache trend.
    ///
    /// Full cache-control marker diagnosis requires `full_llm_capture=true`.
    /// The lightweight fallback is still useful for identifying prompt/cache
    /// growth, cache-hit collapse, and cache-creation churn without disk I/O.
    fn handle_introspect_cache(&self) -> String {
        use astra_turn_core::introspect::cache_diagnosis;
        let live_snapshot = self
            .introspect_snapshot
            .read()
            .unwrap_or_else(|poisoned| {
                astra_core::agent_warn!("introspect", "recovering from poisoned RwLock");
                poisoned.into_inner()
            })
            .clone();

        let session_id = match self.active_session_id() {
            Some(s) if !s.trim().is_empty() => s,
            _ => {
                if let Some(snapshot) = live_snapshot.as_ref()
                    && !snapshot.recent_rounds.is_empty()
                {
                    return cache_diagnosis::render_recent_round_history_markdown(
                        &snapshot.recent_rounds,
                    );
                }
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
        if rounds.is_empty()
            && let Some(snapshot) = live_snapshot.as_ref()
            && !snapshot.recent_rounds.is_empty()
        {
            return cache_diagnosis::render_recent_round_history_markdown(&snapshot.recent_rounds);
        }
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
            let mut outcome = self.bash_outcome_with_cancel(args, cancel_token);
            outcome.output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(outcome.output.len());
            return Some(outcome);
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
        // directly by the server executor; without this gate, `mo_query`
        // and `git` metadata-tagged paths would bypass `execute_run`'s gate.
        if let Some(denied) = self.tool_admission_denial(name, args) {
            return denied.into_outcome();
        }
        self.consume_activated_deferred_tool_if_called(name);
        if name == "bash" {
            let mut outcome = self.bash_outcome_with_cancel(args, None);
            outcome.output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(outcome.output.len());
            return outcome;
        }
        if name == "mo_query" {
            let mut outcome = self.mo_query_with_metadata(args);
            let output = self.finalize_tool_output(outcome.output, name);
            self.record_output_size(output.len());
            outcome.output = output;
            return outcome;
        }
        if name == "git" {
            let action = match astra_tools::git_tool_contract::git_action_from_args(args) {
                Ok(action) => action,
                Err(error) => return ToolExecutionOutcome::error(format!("Error: {error}")),
            };
            match action {
                astra_tools::git_tool_contract::GitAction::Commit => {
                    let mut outcome = self.commit_with_metadata(args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                astra_tools::git_tool_contract::GitAction::RevertCommit => {
                    let mut outcome = self.revert_commit_with_metadata(args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                astra_tools::git_tool_contract::GitAction::Stash => {
                    let stash_args = git_stash_sub_action_args(args);
                    let mut outcome = self.stash_with_metadata(&stash_args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                astra_tools::git_tool_contract::GitAction::Worktree => {
                    let mut outcome = self.worktree_with_metadata(args);
                    outcome.output = self.finalize_tool_output(outcome.output, name);
                    self.record_output_size(outcome.output.len());
                    return outcome;
                }
                astra_tools::git_tool_contract::GitAction::Status
                | astra_tools::git_tool_contract::GitAction::Diff
                | astra_tools::git_tool_contract::GitAction::Log
                | astra_tools::git_tool_contract::GitAction::Show
                | astra_tools::git_tool_contract::GitAction::Blame
                | astra_tools::git_tool_contract::GitAction::FileHistory
                | astra_tools::git_tool_contract::GitAction::LogSearch
                | astra_tools::git_tool_contract::GitAction::Contributors
                | astra_tools::git_tool_contract::GitAction::CheckoutFile
                | astra_tools::git_tool_contract::GitAction::Push => {
                    // Other git actions are handled by execute_run below.
                }
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
        if name == "git"
            && let Err(error) = astra_tools::git_gix::validate_git_request(&self.project_root, args)
        {
            return EdgeToolRun::failure_evidence(error.message, error.evidence);
        }
        let mut tool_result_fields = None;
        let output = self.execute_raw(name, args, &mut tool_result_fields).await;
        // Structural error propagation: `execute_raw` returns a plain String,
        // discarding any structured error kind at the source. Recover it here
        // so downstream `tool_work_surface_events` can route on `error_kind`
        // metadata instead of re-deriving it from fragile string matching.
        let is_error = cli_tool_output_is_error(&output);
        let tool_result_fields = if is_error { None } else { tool_result_fields };
        if is_error {
            let kind = astra_core::classify_tool_output(&output);
            EdgeToolRun::classified_error(output, kind)
        } else {
            EdgeToolRun::ok(output)
        }
        .with_tool_result_fields(tool_result_fields)
    }

    async fn execute_raw(
        &self,
        name: &str,
        args: &Value,
        tool_result_fields: &mut Option<serde_json::Map<String, Value>>,
    ) -> String {
        let output = if let Err(error) =
            crate::tool_safety_guard::ToolSafetyGuard::check_dispatch(name, args)
        {
            error
        } else if is_plan_mode_blocked_tool(name, args) && self.plan_mode_authoring_active().await {
            format!(
                "Error: Tool '{name}' is blocked while plan mode is active. \
                 Use read-only tools to finish the plan, then call \
                 `exit_plan_mode(plan='...')` to submit it through the trusted \
                 plan-review overlay before write tools can run."
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
                "read_file" => {
                    let (output, fields) = self.read_file_with_metadata(args);
                    *tool_result_fields = fields;
                    output
                }
                "write_file" => {
                    // delete=true routes to delete_file handler
                    if args.get("delete").and_then(Value::as_bool).unwrap_or(false) {
                        self.delete_file(args)
                    } else {
                        self.write_file(args)
                    }
                }
                "rollback_database_snapshots" => self.rollback_database_snapshots(args),
                "rollback_file_edits" => self.rollback_file_edits(args),
                "rollback_session_state" => self.rollback_session_state(args).await,
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
                    let action = match astra_tools::git_tool_contract::git_action_from_args(args) {
                        Ok(action) => action,
                        Err(error) => return format!("Error: {error}"),
                    };
                    match action {
                        astra_tools::git_tool_contract::GitAction::Status => {
                            git_gix::status(&self.project_root, args)
                        }
                        astra_tools::git_tool_contract::GitAction::Diff => git_gix::diff(
                            &self.project_root,
                            args,
                            self.get_budget_pressure(),
                            self.aggregate_output_bytes
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        astra_tools::git_tool_contract::GitAction::Log => {
                            git_gix::log(&self.project_root, args)
                        }
                        astra_tools::git_tool_contract::GitAction::Show => git_gix::show(
                            &self.project_root,
                            args,
                            self.get_budget_pressure(),
                            self.aggregate_output_bytes
                                .load(std::sync::atomic::Ordering::Relaxed),
                        ),
                        astra_tools::git_tool_contract::GitAction::Blame => {
                            git_gix::blame(&self.project_root, args)
                        }
                        astra_tools::git_tool_contract::GitAction::FileHistory => {
                            git_gix::file_history(&self.project_root, args)
                        }
                        astra_tools::git_tool_contract::GitAction::LogSearch => {
                            git_gix::log_search(&self.project_root, args)
                        }
                        astra_tools::git_tool_contract::GitAction::Contributors => {
                            git_gix::contributors(&self.project_root, args)
                        }
                        astra_tools::git_tool_contract::GitAction::Commit => self.commit(args),
                        astra_tools::git_tool_contract::GitAction::RevertCommit => {
                            self.revert_commit(args)
                        }
                        astra_tools::git_tool_contract::GitAction::Stash => {
                            let stash_args = git_stash_sub_action_args(args);
                            self.stash(&stash_args)
                        }
                        astra_tools::git_tool_contract::GitAction::CheckoutFile => {
                            self.checkout_file(args)
                        }
                        astra_tools::git_tool_contract::GitAction::Worktree => self.worktree(args),
                        astra_tools::git_tool_contract::GitAction::Push => {
                            git_gix::push(&self.project_root, args)
                        }
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
                "github" => {
                    let action =
                        match astra_tools::github_tool_contract::github_action_from_args(args) {
                            Ok(action) => action,
                            Err(error) => return format!("Error: {error}"),
                        };
                    match action {
                        astra_tools::github_tool_contract::GithubAction::ListPrs => {
                            self.list_prs(args).await
                        }
                        astra_tools::github_tool_contract::GithubAction::GetPr => {
                            self.get_pr(args).await
                        }
                        astra_tools::github_tool_contract::GithubAction::CiStatus => {
                            self.ci_status(args).await
                        }
                        astra_tools::github_tool_contract::GithubAction::RepoStats => {
                            self.repo_stats(args).await
                        }
                        astra_tools::github_tool_contract::GithubAction::ListIssues => {
                            self.list_issues(args).await
                        }
                        astra_tools::github_tool_contract::GithubAction::GetIssue => {
                            self.get_issue(args).await
                        }
                        astra_tools::github_tool_contract::GithubAction::CreateIssue => {
                            self.github_create_issue(args).await
                        }
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
                "display_sixel" => {
                    astra_tools::ToolExecutor::execute(&self.default_executor, name, args)
                        .await
                        .output
                }
                "run_script" => {
                    #[cfg(unix)]
                    {
                        let config = astra_tools::run_script::RunScriptConfig::default();
                        astra_tools::run_script::handle_run_script(args, self, config)
                            .await
                            .output
                    }
                    #[cfg(not(unix))]
                    {
                        "Error: run_script is not available on this platform (requires Unix domain sockets)"
                            .to_string()
                    }
                }
                "memory" => {
                    let action =
                        match astra_tools::memory_tool_contract::memory_action_from_args(args) {
                            Ok(action) => action,
                            Err(error) => return format!("Error: {error}"),
                        };
                    if action == astra_tools::memory_tool_contract::MemoryAction::Inventory {
                        let Some(session_id) = self.active_session_id().filter(|id| !id.is_empty())
                        else {
                            return "Error: memory inventory requires an active session_id"
                                .to_string();
                        };
                        let inventory = match astra_services::session_memory_inventory::load_local_session_memory_inventory(
                            &session_id,
                        ) {
                            Ok(inventory) => inventory,
                            Err(error) => {
                                return format!(
                                    "Error: session memory inventory failed: {error}"
                                );
                            }
                        };
                        return serde_json::to_string(&inventory).unwrap_or_else(|error| {
                            format!("Error: serialize session memory inventory: {error}")
                        });
                    }
                    let clean_args = self.memory_args_with_context(args);
                    self.memoria_call(action.as_str(), &clean_args).await
                }
                "enter_plan_mode" => self.enter_plan_mode_remote(args).await,
                "exit_plan_mode" => self.exit_plan_mode_remote(args).await,
                "adjust_config" => self.adjust_config(args),
                "compress_context" => self.compress_context(args),
                "get_agent_info" => self.get_agent_info(args).await,
                "reflect" => {
                    let topic = args
                        .get("topic")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let facet = args
                        .get("facet")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let depth = args
                        .get("depth")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let horizon = args
                        .get("horizon")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let source_policy = args
                        .get("source_policy")
                        .and_then(|v| v.as_str())
                        .map(str::trim)
                        .filter(|value| !value.is_empty());
                    let include_context = args
                        .get("include_context")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let question = args.get("question").and_then(|v| v.as_str()).unwrap_or("");
                    let last_n = args
                        .get("last_n")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(20)
                        .clamp(1, 100) as i32;
                    let request =
                        astra_services::reflect::ReflectRequest::from_observation_params_with_source(
                            topic,
                            facet,
                            depth,
                            horizon,
                            source_policy,
                            include_context,
                            last_n,
                            question,
                        );
                    if let Some(session_id) = self.active_session_id().filter(|id| !id.is_empty()) {
                        let limit = usize::try_from(request.last_n).unwrap_or(20);
                        match crate::cli::self_command::render_reflect_surface_for_session_with_profile(
                            &session_id,
                            limit,
                            request.clone(),
                            None,
                        )
                        .await
                        {
                            Ok(surface) => surface,
                            Err(error) => serde_json::json!({
                                "status": "reflect_unavailable",
                                "topic": request.topic,
                                "facet": request.facet,
                                "depth": request.depth,
                                "horizon": request.horizon,
                                "source_policy": request.source_policy,
                                "include_context": request.include_context,
                                "question": request.question,
                                "last_n": request.last_n,
                                "error": error,
                            })
                            .to_string(),
                        }
                    } else {
                        serde_json::json!({
                        "status": "reflect_requires_session",
                        "topic": request.topic,
                        "facet": request.facet,
                        "depth": request.depth,
                        "horizon": request.horizon,
                        "source_policy": request.source_policy,
                        "include_context": request.include_context,
                        "question": request.question,
                        "last_n": request.last_n,
                        "note": "Reflect data comes from the server API. Use /reflect command for direct access."
                    }).to_string()
                    }
                }
                // ── Consolidated agent tool ──────────────────────────────────
                "agent" => {
                    let action = match astra_tools::agent_tool_contract::agent_action_from_args(
                        args,
                    ) {
                        Ok(action) => action,
                        Err(error) => {
                            return astra_turn_core::orchestration::agent_result_wire::render_agent_tool_error_with_kind(
                                    None,
                                    &error,
                                    Some(astra_core::ErrorKind::ToolInvalidArgs),
                                );
                        }
                    };
                    match action {
                        astra_tools::agent_tool_contract::AgentAction::RunChain => {
                            match serde_json::from_value::<
                                astra_turn_core::tool_registry_chain::ToolChain,
                            >(args.clone())
                            {
                                Ok(chain) => {
                                    let known: Vec<&str> =
                                        astra_turn_core::tool_registry_meta::TOOL_CATALOG
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
                        astra_tools::agent_tool_contract::AgentAction::Spawn => {
                            agent_spawning::handle_agent_spawn_action(
                                args,
                                self.spawn_context.as_ref(),
                            )
                            .await
                        }
                        astra_tools::agent_tool_contract::AgentAction::GetResult => {
                            agent_spawning::handle_agent_get_result_action(
                                args,
                                self.spawn_context.as_ref(),
                            )
                            .await
                        }
                        astra_tools::agent_tool_contract::AgentAction::SendMessage => {
                            let mailbox_ctx = self
                                .send_message_context
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone();
                            agent_spawning::handle_agent_send_message_action(
                                args,
                                self.spawn_context.as_ref(),
                                mailbox_ctx.as_ref(),
                            )
                            .await
                        }
                    }
                }
                "agent_fanout" => {
                    agent_spawning::handle_agent_fanout_tool(args, self.spawn_context.as_ref())
                        .await
                }
                // ── Consolidated session tool ──────────────────────────────
                "session" => {
                    let action =
                        match astra_tools::session_tool_contract::session_action_from_args(args) {
                            Ok(action) => action,
                            Err(error) => return format!("Error: {error}"),
                        };
                    match action {
                        astra_tools::session_tool_contract::SessionAction::Config => {
                            self.adjust_config(args)
                        }
                        astra_tools::session_tool_contract::SessionAction::Sleep => {
                            self.sleep_tool(args).await
                        }
                        astra_tools::session_tool_contract::SessionAction::HistoryPage
                        | astra_tools::session_tool_contract::SessionAction::HistorySearch => {
                            self.render_session_history(args)
                        }
                        astra_tools::session_tool_contract::SessionAction::HistoryAround => {
                            self.render_session_history_around(args)
                        }
                    }
                }
                "task_board" => self.execute_task_tool_args(args).await,
                "task_output" => self.task_output_with_fields(args, tool_result_fields).await,
                "task_stop" => self.task_kill_bg(args).await,
                "task_list" => self.task_list_bg().await,
                "web_search" => {
                    let cache_scope = self
                        .active_session_id
                        .lock()
                        .ok()
                        .and_then(|guard| guard.clone())
                        .unwrap_or_else(|| self.project_root.to_string_lossy().to_string());
                    astra_tools::web_search::perform_web_search(None, args, &cache_scope)
                        .await
                        .output
                }
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
                "introspect" => self.handle_introspect(args),
                "diagnose" => self.diagnose(args).await,
                "lsp" => self.lsp(args),
                "env" => self.env_tool(args),
                "notebook_edit" => self.notebook_edit(args),
                "config" => self.config_tool(args),
                "brief" => self.brief(args).await,
                "context_analysis" => self.context_analysis(args),
                _ if astra_runtime_env::is_mcp_namespaced_tool_name(name) => {
                    self.execute_mcp_tool(name, args).await
                }
                _ => format!("Error: Tool '{name}' is not implemented by the CLI executor"),
            }
        };
        // Normalize empty output, then apply global safety net
        let output = self.finalize_tool_output(output, name);
        if name != "memory"
            && !cli_tool_output_is_error(&output)
            && let Some(session_id) = self.active_session_id().filter(|sid| !sid.is_empty())
        {
            let client = astra_tools::memoria::MemoriaToolGateway::new(
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
    /// written to an internal artifact and replaced with a ~2KB preview. The
    /// artifact path is intentionally not exposed to the model because it is
    /// outside the workspace filesystem contract.
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
             Full output was persisted as an internal tool-result artifact outside the workspace.\n\n\
             Preview (first ~{} bytes):\n\
             {}\n...\n\
             </persisted-output>",
            output.len(),
            total_lines,
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
        chain: &astra_turn_core::tool_registry_chain::ToolChain,
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
                                    .rollback_recorded_turn_mutations(&serde_json::json!({
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
                                                "error": format!("invalid recorded turn rollback output: {error}"),
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

    fn provider_visible_tool_schemas(&self) -> Vec<Value> {
        let mut tools = self.runtime_bound_provider_owned_schemas_excluding(&HashSet::new());
        if tools.is_empty() {
            tools = self.runtime_bound_tool_schemas(local_tool_schemas());
        }
        tools
    }

    fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .provider_visible_tool_schemas()
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        names.sort();
        names
    }

    fn tool_count(&self) -> usize {
        self.tool_names().len()
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
                "capacity_provider_coverage": caps.capacity_provider_coverage,
                "tool_count": model.capabilities.total_tools,
                "retry_cautioned_tools": model.capabilities.retry_cautioned_tools,
                "skills": model.capabilities.skills,
                "tool_health": model.capabilities.tool_health.iter().map(|t| {
                    json!({
                        "name": t.name,
                        "total_calls": t.total_calls,
                        "success_rate": t.success_rate,
                        "retry_cautioned": t.retry_cautioned,
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
                "capacity_provider_coverage": caps.capacity_provider_coverage,
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
        pool.extend(self.provider_owned_schemas_snapshot("provider_owned_schemas_capability_view"));

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
            .filter(|name| astra_runtime_env::is_mcp_namespaced_tool_name(name))
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
            capacity_provider_coverage: self.cli_capacity_provider_coverage(),
        }
    }

    fn cli_capacity_provider_coverage(
        &self,
    ) -> Vec<astra_turn_core::introspect::CapacityProviderCoverageEntry> {
        vec![
            self.cli_local_provider_coverage(),
            self.cli_control_plane_provider_coverage(),
            self.cli_mcp_provider_coverage(),
        ]
    }

    fn cli_local_provider_coverage(
        &self,
    ) -> astra_turn_core::introspect::CapacityProviderCoverageEntry {
        astra_runtime_env::CapacityProviderCoverageEntry::ready(
            astra_runtime_env::CapacityProviderType::CliLocal,
            "local-cli",
            astra_runtime_env::read_write_workspace_capabilities(),
        )
    }

    fn cli_control_plane_provider_coverage(
        &self,
    ) -> astra_turn_core::introspect::CapacityProviderCoverageEntry {
        let mut extra = vec![
            astra_runtime_env::CAP_INTROSPECT,
            astra_runtime_env::CAP_REFLECT,
        ];
        if self.spawn_context.is_some() {
            extra.push(astra_runtime_env::CAP_MULTI_AGENT);
        }
        if self.bg_task_commands.is_some() {
            extra.push(astra_runtime_env::CAP_LOCAL_BACKGROUND_TASKS);
        }
        astra_runtime_env::CapacityProviderCoverageEntry::ready(
            astra_runtime_env::CapacityProviderType::ControlPlane,
            "cli-control-plane",
            astra_runtime_env::control_plane_capabilities(extra),
        )
    }

    fn cli_mcp_provider_coverage(
        &self,
    ) -> astra_turn_core::introspect::CapacityProviderCoverageEntry {
        let schemas = self.mcp_runtime_snapshot("cli_mcp_coverage").schemas;
        let mut ready_names = schemas
            .iter()
            .filter_map(astra_turn_core::tool::schema::tool_schema_name)
            .filter(|name| self.mcp_tool_has_runtime_binding(name))
            .map(str::to_string)
            .collect::<Vec<_>>();
        ready_names.sort();
        ready_names.dedup();
        if !ready_names.is_empty() {
            return astra_runtime_env::CapacityProviderCoverageEntry::ready(
                astra_runtime_env::CapacityProviderType::McpProvider,
                "cli-mcp",
                ready_names,
            );
        }

        let reason = if schemas.is_empty() {
            "no_cli_mcp_provider_bound"
        } else {
            "no_cli_mcp_runtime_binding"
        };
        astra_runtime_env::CapacityProviderCoverageEntry::unavailable(
            astra_runtime_env::CapacityProviderType::McpProvider,
            "cli-mcp",
            astra_runtime_env::CapacityProviderStatus::Unbound,
            reason,
        )
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
        let injection_freshness = astra_turn_core::injection_tracking::freshness_report(
            &session.injection_history,
            session.turn_number,
        );
        let stale_runtime_signals =
            astra_turn_core::injection_tracking::stale_channel_advisories(&injection_freshness);

        let mut snapshot = astra_runtime::self_model::SelfModel::snapshot_with_strategy(
            &tool_name_refs,
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
        if !stale_runtime_signals.is_empty() {
            snapshot = snapshot.with_stale_runtime_signals(stale_runtime_signals);
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

#[async_trait::async_trait]
impl astra_tools::ToolExecutor for ToolExecutor {
    async fn execute(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        tool_result_from_cli_outcome(ToolExecutor::execute_with_metadata(self, name, args).await)
    }

    fn tool_schemas(&self) -> Vec<Value> {
        self.runtime_available_tool_schemas()
    }

    fn project_root(&self) -> &Path {
        &self.project_root
    }

    async fn execute_with_metadata(&self, name: &str, args: &Value) -> astra_tools::ToolResult {
        tool_result_from_cli_outcome(ToolExecutor::execute_with_metadata(self, name, args).await)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AGGREGATE_SOFT_LIMIT, BgTaskCommand, BgTaskOutputReadMode, BgTaskOutputSearchSnapshot,
        BgTaskOutputSnapshot, PERSIST_THRESHOLD, ToolExecutor, all_tool_schemas,
        cli_tool_output_is_error, detect_git_remote_repos, extract_github_owner_repo,
        file_checkpoint_dir_for, format_background_task_error, format_background_task_output,
        format_background_task_output_wait_timeout, format_background_task_stop_error,
        git_stash_sub_action_args, memoria, parse_memory_search_contents, utf16_col_to_char_idx,
    };
    use crate::background_task_error::BackgroundTaskError;
    use crate::lock_recovery::LockRecovery;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;

    fn parse_control_result(output: &str) -> serde_json::Value {
        serde_json::from_str(output)
            .unwrap_or_else(|error| panic!("expected structured control result: {error}: {output}"))
    }

    #[test]
    fn git_stash_bridge_remaps_canonical_sub_action() {
        let canonical =
            git_stash_sub_action_args(&serde_json::json!({"action":"stash","sub_action":"push"}));
        assert_eq!(canonical["action"], "push");
    }

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
                turns_completed: 0,
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
            client_tool_delivery_tx: None,
            trace_context: None,
            execution_metadata: None,
            transcript_location:
                astra_runtime::orchestration::AgentTranscriptLocation::LocalJournal,
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

    fn function_schema(name: &str) -> serde_json::Value {
        serde_json::json!({
            "type": "function",
            "function": {
                "name": name,
                "description": format!("{name} test schema"),
                "parameters": {"type": "object", "properties": {}}
            }
        })
    }

    #[tokio::test]
    async fn cli_executor_implements_shared_tool_executor_contract() {
        let (_dir, executor) = temp_executor();
        let result = astra_tools::ToolExecutor::execute_with_metadata(
            &executor,
            "list_dir",
            &serde_json::json!({"path": "."}),
        )
        .await;

        assert!(
            !result.is_error,
            "shared trait execution failed: {result:?}"
        );
        assert!(
            result.output.contains("Directory")
                || result.output.contains("No entries")
                || result.output.contains("empty"),
            "shared trait must return the CLI tool output, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn cli_git_invalid_path_preserves_typed_source_evidence() {
        let (_dir, executor) = temp_executor();

        let result = astra_tools::ToolExecutor::execute_with_metadata(
            &executor,
            "git",
            &serde_json::json!({"action": "diff", "path": "missing.rs"}),
        )
        .await;

        assert!(result.is_error, "{result:?}");
        let metadata = result.metadata.expect("typed validation metadata");
        assert_eq!(metadata["error_kind"], "tool_invalid_args");
        assert_eq!(metadata["recovery_evidence"]["cause"], "resource_missing");
        assert_eq!(metadata["recovery_evidence"]["retryable"], false);
    }

    #[test]
    fn cli_shared_tool_schemas_are_runtime_bound_stable_and_deduped() {
        let executor = test_executor();
        let duplicate_schema = function_schema("read_file");
        executor.set_cli_local_provider_schemas(vec![duplicate_schema]);

        let first = astra_tools::ToolExecutor::tool_schemas(&executor);
        let second = astra_tools::ToolExecutor::tool_schemas(&executor);
        assert_eq!(first, second, "shared schema surface must be byte-stable");

        let names = astra_turn_core::tool::schema::tool_names_from_schemas(&first);
        assert!(
            names.contains("read_file"),
            "runtime-bound CLI surface must include core local tools"
        );
        assert!(
            names.contains("task_board"),
            "runtime-bound CLI surface must include control-plane local tools"
        );
        assert_eq!(
            names
                .iter()
                .filter(|name| name.as_str() == "read_file")
                .count(),
            1,
            "duplicate provider schemas must not create duplicate prompt tools"
        );
    }

    #[tokio::test]
    async fn cli_executor_rejects_provider_tool_without_explicit_handler() {
        let executor = test_executor();
        let schema = function_schema("custom_provider_probe");
        executor.set_cli_local_provider_schemas(vec![schema.clone()]);
        executor.set_current_visible_tool_schemas(&[schema]);

        let output = executor
            .execute("custom_provider_probe", &serde_json::json!({}))
            .await;

        assert!(
            output.contains("not implemented by the CLI executor"),
            "provider-owned tools without a CLI handler must fail closed: {output}"
        );
        assert!(
            !output.contains("DefaultToolExecutor"),
            "unknown CLI tools must not leak through the shared default executor: {output}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn cli_run_script_is_explicit_shared_tool_delegate() {
        let executor = test_executor();
        let output = executor.execute("run_script", &serde_json::json!({})).await;

        assert!(
            output.contains("run_script requires a non-empty top-level `script` string"),
            "run_script must be handled by its shared contract, got: {output}"
        );
    }

    #[test]
    fn edge_large_output_reference_does_not_expose_unreadable_filesystem_path() {
        let executor = test_executor();
        executor.aggregate_output_bytes.store(
            AGGREGATE_SOFT_LIMIT + 1,
            std::sync::atomic::Ordering::Relaxed,
        );
        let output = "x".repeat(PERSIST_THRESHOLD + 100);

        let rendered = executor.maybe_persist_large_output(output, "bash");

        assert!(rendered.contains("<persisted-output>"));
        assert!(rendered.contains("internal tool-result artifact"));
        assert!(!rendered.contains("Full output saved to:"));
        assert!(!rendered.contains("Use read_file"));
        assert!(!rendered.contains(".astra/tool-results"));
    }

    #[test]
    fn cloud_plan_summary_requires_canonical_status_field() {
        let executor = test_executor();

        let authoring = serde_json::json!({"plan_id": "p1", "status": "planning"});
        assert_eq!(
            executor.cloud_plan_summary_status(&authoring),
            Some("planning")
        );
        assert!(executor.cloud_plan_is_authoring(&authoring));

        let refining = serde_json::json!({"plan_id": "p2", "status": "refining"});
        assert!(executor.cloud_plan_is_authoring(&refining));

        let old_phase_only = serde_json::json!({"plan_id": "p3", "phase": "planning"});
        assert_eq!(executor.cloud_plan_summary_status(&old_phase_only), None);
        assert!(
            !executor.cloud_plan_is_authoring(&old_phase_only),
            "phase-only plan summaries must not keep the cloud authoring guard active"
        );
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

        executor.set_cli_local_provider_schemas(vec![plugin_schema.clone()]);
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

    #[test]
    fn runtime_bound_tool_schemas_hide_internal_builtin_helpers() {
        let executor = test_executor();
        let filtered = executor.runtime_bound_tool_schemas(vec![
            function_schema("read_file"),
            function_schema("delete_file"),
            function_schema("multi_edit"),
            function_schema("background_shell"),
            function_schema("git_clone"),
            function_schema("find_definition"),
            function_schema("find_references"),
        ]);

        assert_eq!(
            astra_turn_core::tool::schema::tool_names_from_schemas(&filtered),
            HashSet::from(["read_file".to_string()]),
            "internal helper schemas must not be prompt-visible even when the local runtime can execute their underlying capability"
        );
    }

    #[test]
    fn current_tool_surface_filters_internal_helpers_before_admission() {
        let executor = test_executor();

        executor.set_current_activatable_tool_names(HashSet::from([
            "read_file".to_string(),
            "delete_file".to_string(),
            "multi_edit".to_string(),
        ]));
        assert_eq!(
            executor.current_activatable_tool_names_snapshot(),
            HashSet::from(["read_file".to_string()]),
            "deferred activation must mirror public provider-owned schemas, not internal helper executability"
        );

        executor.set_current_tool_surface(
            &[
                function_schema("read_file"),
                function_schema("delete_file"),
                function_schema("multi_edit"),
            ],
            HashSet::from(["background_shell".to_string()]),
        );
        let surface = executor.current_tool_surface_snapshot("test_visible_surface");
        assert_eq!(
            surface.visible().cloned().unwrap_or_default(),
            HashSet::from(["read_file".to_string()]),
            "manual visible-surface installation must not admit internal helper schemas"
        );
        assert!(
            surface
                .activatable()
                .cloned()
                .unwrap_or_default()
                .is_empty(),
            "manual activatable surface must not retain internal helper names"
        );
    }

    #[test]
    fn cli_runtime_env_admission_allows_local_workspace_tools() {
        let (_dir, executor) = temp_executor();

        for (tool, args) in [
            ("read_file", serde_json::json!({"path": "README.md"})),
            ("bash", serde_json::json!({"command": "pwd"})),
            (
                "git",
                serde_json::json!({"action": "commit", "message": "local change"}),
            ),
        ] {
            assert!(
                executor
                    .runtime_environment_tool_denial(tool, &args)
                    .is_none(),
                "{tool} should be allowed by the CLI local provider runtime-env binding"
            );
        }
    }

    #[test]
    fn cli_runtime_env_admission_requires_mcp_executor() {
        let executor = test_executor();

        let denial = executor
            .runtime_environment_tool_denial("mcp__weather", &serde_json::json!({"city": "NYC"}))
            .expect("MCP tool without runtime binding must be denied");

        assert_eq!(
            denial,
            astra_runtime_env::ToolUnavailableReason::ExecutorUnavailable(
                "mcp_executor_required".to_string()
            )
        );
        assert!(
            !executor.tool_has_runtime_binding("mcp__weather"),
            "MCP schema must stay invisible without MCP provider ownership"
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
        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["kind"], "background_task", "{result}");
        assert_eq!(parsed["task_id"], "bg-shell-1", "{result}");
        assert_eq!(parsed["status"], "unavailable", "{result}");
        assert!(cli_tool_output_is_error(&result), "{result}");
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
        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["kind"], "background_task", "{result}");
        assert_eq!(parsed["task_id"], "bg-shell-1", "{result}");
        assert_eq!(parsed["status"], "unavailable", "{result}");
        assert!(cli_tool_output_is_error(&result), "{result}");
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
        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["kind"], "background_task", "{result}");
        assert_eq!(parsed["task_id"], "bg-shell-1", "{result}");
        assert_eq!(parsed["status"], "unavailable", "{result}");
        assert_eq!(
            parsed["error"],
            "no edge runner is attached to this cloud session"
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
        assert_eq!(started_value["status"], "started", "{started}");
        assert_eq!(started_value["fanout"]["status"], "running", "{started}");

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
    async fn bound_agent_spawn_executes_through_runtime_context() {
        let spawner = test_spawner();
        let ctx = fanout_test_context(spawner);
        let executor = test_executor().with_spawn_context(ctx);

        let result = executor
            .execute(
                "agent",
                &serde_json::json!({
                    "action": "spawn",
                    "description": "Review one area",
                    "prompt": "Return a short result.",
                    "agent_type": "general-purpose"
                }),
            )
            .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&result).expect("agent spawn result must be structured JSON");

        assert_eq!(parsed["status"], "launched", "{result}");
        assert_eq!(parsed["lifecycle"], "running", "{result}");
        assert_eq!(parsed["delivery"], "asynchronous", "{result}");
        assert!(
            parsed["agent_id"]
                .as_str()
                .is_some_and(|agent_id| !agent_id.is_empty()),
            "{result}"
        );
        assert!(
            !result.contains("multi-agent runtime is not connected"),
            "bound spawn context must not be treated as missing: {result}"
        );
    }

    #[tokio::test]
    async fn task_list_bg_surfaces_recoverable_fanout_results_without_background_runner() {
        let spawner = test_spawner();
        let ctx = fanout_test_context(spawner);
        let executor = test_executor().with_spawn_context(ctx);

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
        assert_eq!(started_value["status"], "started", "{started}");
        assert_eq!(started_value["fanout"]["status"], "running", "{started}");

        let result = executor.task_list_bg().await;

        assert!(result.contains("<background_tasks count=\"0\""), "{result}");
        assert!(result.contains("<agent_fanouts count=\"1\""), "{result}");
        assert!(result.contains("id=\"review-fanout\""), "{result}");
        assert!(
            result.contains(
                "agent_fanout(action=&apos;get_results&apos;, group_id=&apos;review-fanout&apos;)"
            ),
            "{result}"
        );
        assert!(
            result.contains("task_output(task_id=&apos;review-fanout&apos;)"),
            "{result}"
        );
        assert!(
            result.contains("Do not rerun solely because background_tasks count is zero."),
            "{result}"
        );
        assert!(
            !result.contains("Background task unavailable"),
            "recoverable fanout results are a valid task_list surface even without a shell background runner: {result}"
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
    async fn task_output_without_offset_returns_latest_bounded_shell_tail() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({
            "task_id": "bg-shell-1",
            "max_bytes": 100,
            "timeout_ms": 10_000
        });
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count = requests.clone();
        let mut result_fields = None;
        let output = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            let output_fut = executor.task_output_with_fields(&args, &mut result_fields);
            tokio::pin!(output_fut);
            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::GetOutputSince { offset, reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            let request = request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            match request {
                                0 => {
                                    assert_eq!(offset, 0);
                                    let _ = reply.send(Ok(bg_snapshot(100, 1_000, 200, "running", "old prefix")));
                                }
                                1 => {
                                    assert_eq!(offset, 900);
                                    let _ = reply.send(Ok(bg_snapshot(1_000, 1_000, 200, "running", "latest progress")));
                                }
                                _ => panic!("unexpected background output request {request}"),
                            }
                        }
                    }
                }
            }
        })
        .await
        .expect("offset-free snapshot should return a bounded tail promptly");

        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(output.contains("offset 900 -> 1000"), "{output}");
        assert!(output.contains("latest progress"), "{output}");
        assert!(!output.contains("old prefix"), "{output}");
        assert!(output.contains("do_not_poll_again_this_turn"), "{output}");
        let observation = result_fields
            .as_ref()
            .and_then(|fields| fields.get("background_task_observation"))
            .expect("current shell observation must be structured");
        assert_eq!(observation["task_id"], "bg-shell-1");
        assert_eq!(observation["task_kind"], "shell");
        assert_eq!(observation["mode"], "current");
    }

    #[tokio::test]
    async fn task_output_pattern_uses_typed_bounded_diagnostic_search() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({
            "task_id": "bg-shell-1",
            "pattern": "failing_test_name",
            "context_lines": 5,
            "max_bytes": 4096,
            "timeout_ms": 10_000
        });
        let mut result_fields = None;
        let output = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let output_fut = executor.task_output_with_fields(&args, &mut result_fields);
            tokio::pin!(output_fut);
            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::SearchOutput {
                            task_id,
                            pattern,
                            context_lines,
                            max_bytes,
                            reply,
                        }) = commands.lock_recover().pop()
                        {
                            assert_eq!(task_id, "bg-shell-1");
                            assert_eq!(pattern, "failing_test_name");
                            assert_eq!(context_lines, 5);
                            assert_eq!(max_bytes, 4096);
                            let _ = reply.send(Ok(BgTaskOutputSearchSnapshot {
                                kind: "shell".to_string(),
                                title: Some("cargo test".to_string()),
                                output: "[stdout lines 40-42]\n41: panic detail\n".to_string(),
                                matching_lines: 1,
                                truncated: false,
                                status: "failed".to_string(),
                                terminal: true,
                                output_ref: "stdout: /tmp/bg-shell-1.stdout".to_string(),
                            }));
                        }
                    }
                }
            }
        })
        .await
        .expect("diagnostic search should return promptly");

        assert!(
            output.contains("Search shell output bg-shell-1"),
            "{output}"
        );
        assert!(output.contains("pattern \"failing_test_name\""), "{output}");
        assert!(output.contains("panic detail"), "{output}");
        assert!(output.contains("not a classification"), "{output}");
        let observation = result_fields
            .as_ref()
            .and_then(|fields| fields.get("background_task_observation"))
            .expect("diagnostic search observation must be structured");
        assert_eq!(observation["mode"], "diagnostic");
        assert_eq!(observation["terminal"], true);
        assert_eq!(observation["matching_lines"], 1);
    }

    #[tokio::test]
    async fn task_output_pattern_rejects_cursor_or_wait_combinations() {
        let executor = test_executor();
        for args in [
            serde_json::json!({
                "task_id": "bg-shell-1",
                "pattern": "failure",
                "offset": 0
            }),
            serde_json::json!({
                "task_id": "bg-shell-1",
                "pattern": "failure",
                "block": true
            }),
        ] {
            let output = executor.task_output(&args).await;
            let result = parse_control_result(&output);
            assert_eq!(result["status"], "invalid_argument", "{output}");
            assert_eq!(
                result["error"], "pattern cannot be combined with block or offset",
                "{output}"
            );
        }
    }

    #[tokio::test]
    async fn task_output_pattern_rejects_multiline_literal() {
        let executor = test_executor();
        let output = executor
            .task_output(&serde_json::json!({
                "task_id": "bg-shell-1",
                "pattern": "failure\npanic"
            }))
            .await;
        let result = parse_control_result(&output);
        assert_eq!(result["status"], "invalid_argument", "{output}");
        assert_eq!(result["error"], "pattern must be a single line", "{output}");
    }

    #[tokio::test]
    async fn task_output_blocking_waits_for_terminal_not_ordinary_output_growth() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({
            "task_id": "bg-shell-1",
            "block": true,
            "max_bytes": 100,
            "timeout_ms": 500
        });
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count = requests.clone();
        let output = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            let output_fut = executor.task_output(&args);
            tokio::pin!(output_fut);
            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::GetOutputSince { offset, reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            let request = request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            match request {
                                0 => {
                                    assert_eq!(offset, 0);
                                    let _ = reply.send(Ok(bg_snapshot(100, 100, 10, "running", "initial")));
                                }
                                1 => {
                                    assert_eq!(offset, 100);
                                    let _ = reply.send(Ok(bg_snapshot(120, 120, 12, "running", "ordinary progress")));
                                }
                                2 => {
                                    assert_eq!(offset, 120);
                                    let _ = reply.send(Ok(bg_snapshot(130, 130, 13, "completed", "done")));
                                }
                                3 => {
                                    assert_eq!(offset, 0);
                                    let _ = reply.send(Ok(bg_snapshot(100, 130, 13, "completed", "old final prefix")));
                                }
                                4 => {
                                    assert_eq!(offset, 30);
                                    let _ = reply.send(Ok(bg_snapshot(130, 130, 13, "completed", "latest final tail")));
                                }
                                _ => panic!("unexpected background output request {request}"),
                            }
                        }
                    }
                }
            }
        })
        .await
        .expect("one blocking call should own the wait through terminal state");

        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 5);
        assert!(output.contains("terminal true"), "{output}");
        assert!(output.contains("completed"), "{output}");
        assert!(output.contains("latest final tail"), "{output}");
        assert!(!output.contains("ordinary progress"), "{output}");
    }

    #[tokio::test]
    async fn task_output_blocking_wakes_when_local_agent_needs_input() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({
            "task_id": "agent-1",
            "block": true,
            "timeout_ms": 500
        });
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count = requests.clone();
        let output = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let output_fut = executor.task_output(&args);
            tokio::pin!(output_fut);
            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::GetOutputSince { offset, reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            assert_eq!(offset, 0);
                            request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let mut snapshot = bg_snapshot(
                                28,
                                28,
                                1,
                                "waiting_for_input",
                                "Agent is waiting for input.",
                            );
                            snapshot.kind = "local agent".to_string();
                            let _ = reply.send(Ok(snapshot));
                        }
                    }
                }
            }
        })
        .await
        .expect("required input must wake one blocking observation promptly");

        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert!(output.contains("needs input"), "{output}");
        assert!(output.contains("Agent is waiting for input"), "{output}");
        assert!(!output.contains("Wait timed out"), "{output}");
    }

    #[tokio::test]
    async fn task_output_blocking_owns_wait_until_timeout_despite_output_growth() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({
            "task_id": "bg-shell-1",
            "block": true,
            "timeout_ms": 25
        });
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let request_count = requests.clone();
        let started = std::time::Instant::now();
        let output = tokio::time::timeout(std::time::Duration::from_millis(300), async {
            let output_fut = executor.task_output(&args);
            tokio::pin!(output_fut);
            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::GetOutputSince { offset, reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            let request = request_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            let total = (request as u64 + 1) * 10;
                            let _ = reply.send(Ok(bg_snapshot(
                                total.max(offset),
                                total.max(offset),
                                request as u64 + 1,
                                "running",
                                "ordinary progress",
                            )));
                        }
                    }
                }
            }
        })
        .await
        .expect("runtime-owned wait must remain bounded");

        assert!(started.elapsed() >= std::time::Duration::from_millis(20));
        assert!(
            requests.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "wait should observe growth internally before timing out"
        );
        assert!(output.contains("Wait timed out after 25ms"), "{output}");
        assert!(output.contains("ordinary progress"), "{output}");
        assert!(
            output.contains("Do not poll again in this turn"),
            "{output}"
        );
    }

    #[tokio::test]
    async fn task_output_maps_typed_not_found_reply_without_text_classification() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({"task_id": "bg-shell-missing"});

        let output = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let output_fut = executor.task_output(&args);
            tokio::pin!(output_fut);
            loop {
                tokio::select! {
                    output = &mut output_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::GetOutputSince { reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            let _ = reply.send(Err(BackgroundTaskError::NotFound {
                                task_id: "bg-shell-missing".into(),
                            }));
                        }
                    }
                }
            }
        })
        .await
        .expect("typed registry error should return promptly");

        let result = parse_control_result(&output);
        assert_eq!(result["ok"], false, "{output}");
        assert_eq!(result["status"], "not_found", "{output}");
        assert_eq!(result["task_id"], "bg-shell-missing", "{output}");
        assert!(cli_tool_output_is_error(&output), "{output}");
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
        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["kind"], "background_task", "{result}");
        assert_eq!(parsed["task_id"], "bg-shell-1", "{result}");
        assert_eq!(parsed["status"], "unavailable", "{result}");
        assert_eq!(
            parsed["error"],
            "no edge runner is attached to this cloud session"
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

        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["status"], "stop_status_unknown", "{result}");
        assert_eq!(parsed["task_id"], "bg-shell-1", "{result}");
        assert!(cli_tool_output_is_error(&result), "{result}");
    }

    #[tokio::test]
    async fn task_stop_maps_typed_terminal_reply_without_text_classification() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({"task_id": "bg-shell-1"});

        let output = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let stop_fut = executor.task_kill_bg(&args);
            tokio::pin!(stop_fut);
            loop {
                tokio::select! {
                    output = &mut stop_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::Kill { reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            let _ = reply.send(Err(BackgroundTaskError::AlreadyTerminated {
                                task_id: "bg-shell-1".into(),
                            }));
                        }
                    }
                }
            }
        })
        .await
        .expect("typed registry error should return promptly");

        let result = parse_control_result(&output);
        assert_eq!(result["ok"], true, "{output}");
        assert_eq!(result["status"], "already_terminal", "{output}");
        assert_eq!(result["terminal"], true, "{output}");
        assert!(!cli_tool_output_is_error(&output), "{output}");
    }

    #[tokio::test]
    async fn task_stop_reports_accepted_request_without_claiming_terminal_state() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands.clone());
        let args = serde_json::json!({"task_id": "bg-shell-1"});

        let output = tokio::time::timeout(std::time::Duration::from_millis(200), async {
            let stop_fut = executor.task_kill_bg(&args);
            tokio::pin!(stop_fut);
            loop {
                tokio::select! {
                    output = &mut stop_fut => break output,
                    _ = tokio::time::sleep(std::time::Duration::from_millis(1)) => {
                        if let Some(BgTaskCommand::Kill { reply, .. }) =
                            commands.lock_recover().pop()
                        {
                            let _ = reply.send(Ok(()));
                        }
                    }
                }
            }
        })
        .await
        .expect("typed stop receipt should return promptly");

        let result = parse_control_result(&output);
        assert_eq!(result["ok"], true, "{output}");
        assert_eq!(result["status"], "stop_requested", "{output}");
        assert_eq!(result["terminal"], false, "{output}");
        assert!(!cli_tool_output_is_error(&output), "{output}");
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
        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["kind"], "background_task_list", "{result}");
        assert_eq!(parsed["status"], "unavailable", "{result}");
        assert_eq!(
            parsed["error"],
            "no edge runner is attached to this cloud session"
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

        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["status"], "registry_timeout", "{result}");
        assert!(
            parsed["error"]
                .as_str()
                .is_some_and(|error| error.contains("retry task_list")),
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
        let parsed = parse_control_result(&result);
        assert_eq!(parsed["ok"], false, "{result}");
        assert_eq!(parsed["status"], "unavailable", "{result}");
        assert_eq!(
            parsed["error"],
            "local background tasks require an interactive CLI session"
        );
    }

    #[tokio::test]
    async fn task_output_empty_id_names_required_task_id() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_output(&serde_json::json!({"task_id": "   ", "block": false}))
            .await;
        let result = parse_control_result(&output);
        assert_eq!(result["status"], "invalid_argument", "{output}");
        assert_eq!(result["error"], "Task id is required", "{output}");
        assert!(cli_tool_output_is_error(&output), "{output}");
    }

    #[tokio::test]
    async fn task_stop_empty_id_names_required_task_id() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_kill_bg(&serde_json::json!({"task_id": "   "}))
            .await;
        let result = parse_control_result(&output);
        assert_eq!(result["status"], "invalid_argument", "{output}");
        assert_eq!(result["error"], "Task id is required", "{output}");
        assert!(cli_tool_output_is_error(&output), "{output}");
    }

    #[tokio::test]
    async fn task_output_non_string_id_names_expected_shape() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_output(&serde_json::json!({"task_id": 123, "block": false}))
            .await;
        let result = parse_control_result(&output);
        assert_eq!(result["status"], "invalid_argument", "{output}");
        assert_eq!(result["error"], "task_id must be a non-empty string");
    }

    #[tokio::test]
    async fn task_stop_non_string_id_names_expected_shape() {
        let commands = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let executor = test_executor().with_bg_task_commands(commands);
        let output = executor
            .task_kill_bg(&serde_json::json!({"task_id": 123}))
            .await;
        let result = parse_control_result(&output);
        assert_eq!(result["status"], "invalid_argument", "{output}");
        assert_eq!(result["error"], "task_id must be a non-empty string");
    }

    #[tokio::test]
    async fn unknown_task_action_stays_inside_task_contract() {
        let executor = test_executor();
        let output = executor
            .execute("task_board", &serde_json::json!({"action": "spawn_agents"}))
            .await;

        assert!(
            output.contains("unknown `task_board` action 'spawn_agents'"),
            "{output}"
        );
        assert!(output.contains("create, update, list"), "{output}");
        assert!(
            !output.contains("agent_fanout"),
            "task must not route unknown actions to agent orchestration: {output}"
        );
        assert!(!output.contains("run_in_background: true"), "{output}");
        assert!(!output.contains("run_in_background=true"), "{output}");
        assert!(!output.contains("agent(action="), "{output}");
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
        let output = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(0, 0, 0, "running", ""),
            BgTaskOutputReadMode::Current,
        );
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
        let output = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(0, 0, 0, "completed", ""),
            BgTaskOutputReadMode::Current,
        );
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

        let output =
            format_background_task_output("agent-1", 0, &snapshot, BgTaskOutputReadMode::Current);

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
            BgTaskOutputReadMode::Current,
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
        let pending = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(0, 0, 0, "pending", ""),
            BgTaskOutputReadMode::Current,
        );
        let unavailable = format_background_task_output(
            "bg-shell-2",
            0,
            &bg_snapshot(0, 0, 0, "unavailable", ""),
            BgTaskOutputReadMode::Current,
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
            BgTaskOutputReadMode::Current,
        );
        assert!(output.contains("needs input"), "{output}");
        assert!(output.contains("Continue? (y/n)"), "{output}");
        assert!(!output.contains("waiting_for_input"), "{output}");
    }

    #[test]
    fn background_task_output_projection_names_failed_and_killed_empty_states() {
        let failed = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(0, 0, 0, "failed", ""),
            BgTaskOutputReadMode::Current,
        );
        let killed = format_background_task_output(
            "bg-shell-2",
            0,
            &bg_snapshot(0, 0, 0, "killed", ""),
            BgTaskOutputReadMode::Current,
        );
        assert!(failed.contains("Failed with no output"), "{failed}");
        assert!(killed.contains("Stopped with no output"), "{killed}");
        assert!(!failed.contains("Completed with no output"), "{failed}");
        assert!(!killed.contains("Completed with no output"), "{killed}");
        assert!(failed.contains("failure_cause unverified"), "{failed}");
        assert!(failed.contains("task_output(pattern=...)"), "{failed}");
        assert!(
            failed.contains("not evidence that a test is flaky"),
            "{failed}"
        );
    }

    #[test]
    fn background_task_output_projection_reports_total_lines() {
        let output = format_background_task_output(
            "bg-shell-1",
            0,
            &bg_snapshot(12, 12, 2, "running", "hello\nworld\n"),
            BgTaskOutputReadMode::Current,
        );
        assert!(output.contains("2 new lines"), "{output}");
        assert!(output.contains("2 total lines"), "{output}");
        assert!(output.contains("Output chunk:"), "{output}");
        assert!(output.contains("hello"), "{output}");
        assert!(output.contains("world"), "{output}");
        assert!(output.contains("next_offset 12"), "{output}");
        assert!(
            output.contains("later_call task_output(task_id='bg-shell-1', block=false)"),
            "{output}"
        );
        assert!(output.contains("do_not_poll_again_this_turn"), "{output}");
        assert!(!output.contains("\n└ world"), "{output}");
    }

    #[test]
    fn current_snapshot_never_invites_live_log_pagination() {
        let snapshot = bg_snapshot(900, 1_000, 20, "running", "latest bounded chunk");
        let current = format_background_task_output(
            "bg-shell-1",
            800,
            &snapshot,
            BgTaskOutputReadMode::Current,
        );
        let historical = format_background_task_output(
            "bg-shell-1",
            800,
            &snapshot,
            BgTaskOutputReadMode::Historical,
        );

        assert!(current.contains("do_not_poll_again_this_turn"), "{current}");
        assert!(!current.contains("offset=900"), "{current}");
        assert!(historical.contains("offset=900"), "{historical}");
    }

    #[test]
    fn background_task_output_projection_names_wait_timeout_without_job_vocabulary() {
        let output = format_background_task_output_wait_timeout(
            "bg-shell-1",
            42,
            250,
            &bg_snapshot(42, 42, 2, "running", ""),
        );
        assert!(output.contains("Read shell output bg-shell-1"), "{output}");
        assert!(output.contains("Wait timed out after 250ms"), "{output}");
        assert!(output.contains("still non-terminal"), "{output}");
        assert!(
            output.contains("Do not poll again in this turn"),
            "{output}"
        );
        assert!(!output.contains("Job"), "{output}");
    }

    #[test]
    fn background_task_error_projection_names_unknown_id() {
        let output = format_background_task_error(&BackgroundTaskError::NotFound {
            task_id: "bg-shell-missing".into(),
        });
        let result = parse_control_result(&output);
        assert_eq!(result["ok"], false, "{output}");
        assert_eq!(result["status"], "not_found", "{output}");
        assert_eq!(result["task_id"], "bg-shell-missing", "{output}");
        assert!(cli_tool_output_is_error(&output), "{output}");
    }

    #[test]
    fn background_task_error_projection_names_missing_output_artifact() {
        let output = format_background_task_error(&BackgroundTaskError::OutputArtifactMissing {
            task_id: "bg-shell-1".into(),
            path: PathBuf::from("/tmp/astra/bg-shell-1.stdout"),
        });

        let result = parse_control_result(&output);
        assert_eq!(result["ok"], false, "{output}");
        assert_eq!(result["status"], "output_missing", "{output}");
        assert_eq!(result["task_id"], "bg-shell-1", "{output}");
        assert!(
            result["error"]
                .as_str()
                .is_some_and(|error| error.contains("/tmp/astra/bg-shell-1.stdout")),
            "{output}"
        );
    }

    #[test]
    fn background_task_error_projection_uses_task_vocabulary_for_generic_errors() {
        let output = format_background_task_error(&BackgroundTaskError::OutputUnavailable {
            task_id: "bg-shell-1".into(),
            detail: "permission denied".into(),
        });

        let result = parse_control_result(&output);
        assert_eq!(result["ok"], false, "{output}");
        assert_eq!(result["status"], "output_unavailable", "{output}");
        assert_eq!(result["error"], "permission denied", "{output}");
    }

    #[test]
    fn background_task_stop_error_projection_names_unknown_id() {
        let output = format_background_task_stop_error(&BackgroundTaskError::NotFound {
            task_id: "bg-shell-missing".into(),
        });
        let result = parse_control_result(&output);
        assert_eq!(result["ok"], false, "{output}");
        assert_eq!(result["status"], "not_found", "{output}");
        assert!(cli_tool_output_is_error(&output), "{output}");
    }

    #[test]
    fn background_task_stop_error_projection_names_terminal_race() {
        let output = format_background_task_stop_error(&BackgroundTaskError::AlreadyTerminated {
            task_id: "bg-shell-1".into(),
        });
        let result = parse_control_result(&output);
        assert_eq!(result["ok"], true, "{output}");
        assert_eq!(result["status"], "already_terminal", "{output}");
        assert_eq!(result["terminal"], true, "{output}");
        assert!(!cli_tool_output_is_error(&output), "{output}");
    }

    #[test]
    fn background_task_stop_error_projection_names_stale_handle() {
        let output = format_background_task_stop_error(&BackgroundTaskError::StaleHandle {
            task_id: "bg-shell-1".into(),
        });

        let result = parse_control_result(&output);
        assert_eq!(result["ok"], false, "{output}");
        assert_eq!(result["status"], "stale_handle", "{output}");
        assert!(cli_tool_output_is_error(&output), "{output}");
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
        executor.set_current_activatable_tool_names(HashSet::from(["session".to_string()]));

        let before = executor
            .execute(
                "session",
                &serde_json::json!({"action": "sleep", "seconds": 1}),
            )
            .await;
        assert!(
            before.contains("tool_search")
                && before.contains("select:session")
                && before.contains("not executed"),
            "direct deferred call must become a non-executing activation hint; got: {before}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["session".to_string()],
            "direct deferred call must record activation for the next schema-selection round"
        );

        let search = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:session"}),
            )
            .await;
        let parsed = parse_tool_search_output(&search);
        assert_eq!(
            tool_search_match_names(&parsed),
            vec!["session".to_string()]
        );
        assert!(tool_search_string_array(&parsed, "missing").is_empty());
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["session".to_string()]
        );

        let after = executor.execute("session", &serde_json::json!({})).await;
        assert!(
            after.contains("tool_search")
                && after.contains("select:session")
                && after.contains("not executed"),
            "activation state alone must not bypass current tools[] visibility; got: {after}"
        );
        assert_eq!(
            executor.activated_deferred_tool_names_for_schema_injection(),
            vec!["session".to_string()],
            "schema assembly should surface the selected deferred tool"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["session".to_string()],
            "schema assembly must not consume activation before the tool is called"
        );
        assert_eq!(
            executor.activated_deferred_tool_names_for_schema_injection(),
            vec!["session".to_string()],
            "repeated schema assembly must keep the selected tool available"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["session".to_string()],
            "activation must remain pending until the tool is actually called"
        );

        executor.set_current_visible_tool_schemas(&[
            serde_json::json!({"type": "function", "function": {"name": "bash"}}),
            serde_json::json!({"type": "function", "function": {"name": "tool_search"}}),
            serde_json::json!({"type": "function", "function": {"name": "session"}}),
        ]);
        executor.set_current_activatable_tool_names(HashSet::new());
        let injected = executor.execute("session", &serde_json::json!({})).await;
        assert!(
            injected.contains("missing required parameter `action` for `session`"),
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
        .expect_err("task_board.update must reject create-only subtasks");

        assert!(err.contains("unknown field 'subtasks' for task_board.update"));
        assert!(
            err.contains("field is valid for: task_board.create"),
            "{err}"
        );
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

        let providers = parsed["capacity_provider_coverage"]
            .as_array()
            .expect("capacity_provider_coverage");
        let cli_local = providers
            .iter()
            .find(|provider| provider["provider_type"] == "cli_local")
            .expect("CLI introspect must expose cli_local provider coverage");
        assert_eq!(cli_local["status"].as_str(), Some("ready"));
        assert!(
            cli_local["capabilities"]
                .as_array()
                .is_some_and(|capabilities| capabilities
                    .iter()
                    .any(|capability| capability.as_str() == Some("shell"))),
            "cli_local provider should report shell capacity; got {out}"
        );
        let mcp = providers
            .iter()
            .find(|provider| provider["provider_type"] == "mcp_provider")
            .expect("CLI introspect must expose MCP provider coverage");
        assert_eq!(mcp["status"].as_str(), Some("unbound"));
        assert_eq!(
            mcp["unavailable_reason"].as_str(),
            Some("no_cli_mcp_provider_bound")
        );
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

        let providers = parsed["capacity_provider_coverage"]
            .as_array()
            .expect("capacity_provider_coverage");
        let control_plane = providers
            .iter()
            .find(|provider| provider["provider_type"] == "control_plane")
            .expect("CLI introspect must expose control-plane provider coverage");
        assert!(
            control_plane["capabilities"]
                .as_array()
                .is_some_and(|capabilities| capabilities
                    .iter()
                    .any(|capability| capability.as_str() == Some("local_background_tasks"))),
            "control-plane provider should report local background task capacity when wired; got {out}"
        );
    }

    #[tokio::test]
    async fn introspect_capability_reports_stale_mcp_provider_unbound() {
        let mut executor = test_executor();
        executor.install_mcp_bundle(
            std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::mcp_client::McpClientManager::new(),
            )),
            vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "mcp__weather",
                    "description": "Get weather for a city.",
                    "parameters": {"type": "object", "properties": {}}
                }
            })],
        );

        let out = executor
            .execute(
                "introspect",
                &serde_json::json!({"dimension": "capability"}),
            )
            .await;
        let parsed: serde_json::Value =
            serde_json::from_str(&out).expect("introspect must return JSON");
        let providers = parsed["capacity_provider_coverage"]
            .as_array()
            .expect("capacity_provider_coverage");
        let mcp = providers
            .iter()
            .find(|provider| provider["provider_type"] == "mcp_provider")
            .expect("CLI MCP provider coverage");

        assert_eq!(mcp["status"].as_str(), Some("unbound"));
        assert_eq!(
            mcp["unavailable_reason"].as_str(),
            Some("no_cli_mcp_runtime_binding")
        );
        assert!(mcp["capabilities"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn tool_search_rejects_mcp_schema_installed_on_cli_local_provider() {
        // MCP schemas are only callable when the MCP provider currently owns
        // the public tool name. Installing an MCP-shaped schema on the CLI
        // local provider must not make it visible or activatable.
        let executor = test_executor();
        let schema = serde_json::json!({
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
        executor.set_cli_local_provider_schemas(vec![schema]);
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
    async fn tool_search_rejects_stale_mcp_schema_not_owned_by_manager() {
        let mut executor = test_executor();
        executor.install_mcp_bundle(
            std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::mcp_client::McpClientManager::new(),
            )),
            vec![serde_json::json!({
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
            })],
        );
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
    fn runtime_bound_provider_owned_schemas_excluding_filters_restricted_cli_provider_tools() {
        let executor = test_executor();
        executor.set_cli_local_provider_schemas(vec![serde_json::json!({
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

        let unrestricted = executor.runtime_bound_provider_owned_schemas_excluding(&HashSet::new());
        assert_eq!(
            astra_turn_core::tool::schema::tool_names_from_schemas(&unrestricted),
            HashSet::from(["custom_weather".to_string()])
        );

        let restricted = executor.runtime_bound_provider_owned_schemas_excluding(&HashSet::from([
            "custom_weather".to_string(),
        ]));
        assert!(
            restricted.is_empty(),
            "restricted dynamic CLI provider schemas must not be advertised as deferred"
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
    fn restored_activated_deferred_tool_survives_first_schema_injection() {
        let executor = test_executor();
        executor.restore_activated_deferred_tool_names_for_session(&[
            "memory".to_string(),
            " ".to_string(),
        ]);

        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "session restore should seed valid pending activation"
        );
        assert_eq!(
            executor.activated_deferred_tool_names_for_schema_injection(),
            vec!["memory".to_string()],
            "restored activation must survive until the first schema-injection opportunity"
        );
        assert_eq!(
            executor.activated_deferred_tool_names(),
            vec!["memory".to_string()],
            "schema injection does not consume deferred activation"
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

    /// Poison recovery: CLI provider schemas are a cache. If a prior panic poisoned
    /// the RwLock, reset to a known empty state rather than reading possibly
    /// half-written inner data; a later `set_cli_local_provider_schemas` repopulates it.
    #[tokio::test]
    async fn tool_search_resets_poisoned_cli_local_provider_schemas_lock() {
        let executor = test_executor();
        let schema = serde_json::json!({
            "type": "function",
            "function": {
                "name": "custom_calc",
                "description": "Evaluate expressions.",
                "parameters": {"type": "object", "properties": {}}
            }
        });
        executor.set_cli_local_provider_schemas(vec![schema.clone()]);
        executor.set_current_activatable_tool_names(HashSet::from(["custom_calc".to_string()]));

        // Simulate a prior panic-poisoned write lock.
        let arc = std::sync::Arc::new(&executor.cli_local_provider_schemas);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = arc.write().unwrap();
            panic!("simulated panic under write lock");
        }));
        assert!(
            executor.cli_local_provider_schemas.read().is_err()
                || executor.cli_local_provider_schemas.write().is_err(),
            "lock should be poisoned for the test to be meaningful"
        );

        // The first select after poison must be stable and clear the poisoned
        // cache, not read inner state from the panicking writer.
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:custom_calc"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(tool_search_match_names(&parsed), Vec::<String>::new());
        assert_eq!(
            tool_search_string_array(&parsed, "missing"),
            vec!["custom_calc".to_string()]
        );

        executor.set_cli_local_provider_schemas(vec![schema]);
        let out = executor
            .execute(
                "tool_search",
                &serde_json::json!({"query": "select:custom_calc"}),
            )
            .await;
        let parsed = parse_tool_search_output(&out);
        assert_eq!(
            tool_search_match_names(&parsed),
            vec!["custom_calc".to_string()],
            "repopulating the CLI provider cache should restore provider-owned tools"
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
        let out = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));
        assert!(
            out.contains("Current Runtime Snapshot"),
            "expected structured output, got: {out}"
        );
        assert!(
            !out.contains("first turn"),
            "must not return opaque first-turn placeholder, got: {out}"
        );
    }

    #[test]
    fn introspect_hint_first_turn_has_metrics_not_placeholder() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"depth": "hint"}));
        assert!(
            out.contains("pressure=") && out.contains("turns="),
            "expected hint metrics line, got: {out}"
        );
        assert!(!out.contains("first turn"));
    }

    #[test]
    fn introspect_json_session_memory_reports_structured_data_coverage() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({
            "facet": "session_memory",
            "format": "json"
        }));
        let parsed: serde_json::Value = serde_json::from_str(&out).unwrap_or_else(|error| {
            panic!("expected structured json introspect output: {error}; {out}")
        });

        assert_eq!(parsed["schema_version"], 1);
        assert_eq!(parsed["tool"], "introspect");
        assert_eq!(parsed["facet"], "session_memory");
        assert_eq!(parsed["data_coverage"], parsed["view"]["data_coverage"]);
        assert_eq!(parsed["view"]["facet"], "session_memory");
        assert_eq!(
            parsed["view"]["data_coverage"]["source"],
            "edge_local_artifacts_unavailable"
        );
        assert_eq!(parsed["view"]["data_coverage"]["events"], 0);
        assert!(
            parsed["observations"]
                .as_array()
                .is_some_and(|observations| observations.iter().any(|observation| {
                    observation["kind"].as_str() == Some("data_surface_unavailable")
                })),
            "expected data-surface observation, got: {parsed}"
        );
        assert!(
            parsed
                .get("evidence")
                .and_then(serde_json::Value::as_array)
                .is_none_or(|evidence| evidence.is_empty()),
            "unavailable edge-only JSON must not expose unrelated runtime evidence: {parsed}"
        );
        assert!(
            !out.contains("No observatory attached"),
            "json mode must not return legacy session_memory text: {out}"
        );
    }

    #[test]
    fn introspect_first_turn_reports_current_model_from_executor() {
        let executor = test_executor();
        executor.set_current_model("deepseek-v4-pro-official(thinking:high)");

        let out = executor.handle_introspect(&serde_json::json!({"depth": "hint"}));

        assert!(
            out.contains("model=deepseek-v4-pro-official(thinking:high)"),
            "expected first-turn introspect to expose current model, got: {out}"
        );
    }

    #[test]
    fn introspect_first_turn_reports_effective_input_budget_from_executor() {
        let executor = test_executor();
        executor.set_current_effective_input_budget_tokens(800_000);

        let out = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));

        assert!(
            out.contains("Effective input budget: 800000 tokens"),
            "expected first-turn introspect to expose effective budget, got: {out}"
        );
        assert!(
            !out.contains("262144"),
            "introspect must not expose guessed context-window values, got: {out}"
        );
    }

    #[test]
    fn introspect_first_turn_reports_provider_context_window_from_executor() {
        let executor = test_executor();
        executor.set_current_context_window_tokens(1_000_000);
        executor.set_current_effective_input_budget_tokens(800_000);

        let out = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));

        assert!(
            out.contains("Provider context window: 1000000 tokens"),
            "expected first-turn introspect to expose provider context window, got: {out}"
        );
        assert!(
            out.contains("Effective input budget: 800000 tokens"),
            "expected first-turn introspect to keep effective budget distinct, got: {out}"
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
        let out = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));
        assert!(out.contains("Turns: 5/15"), "got: {out}");
        assert!(out.contains("input_total=12345"), "got: {out}");
        assert!(out.contains("resume pending"), "got: {out}");
    }

    #[test]
    fn introspect_marks_stale_snapshot_from_current_turn() {
        let executor = test_executor();
        executor
            .journal_turn_index
            .store(9, std::sync::atomic::Ordering::Release);
        executor.update_introspect_snapshot(astra_turn_core::introspect::IntrospectSnapshot {
            turns_completed: 7,
            turns_remaining: 0,
            turn_budget_unlimited: true,
            ..Default::default()
        });

        let out = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));

        assert!(out.contains("Turns: 7/∞"), "got: {out}");
        assert!(out.contains("Snapshot age: 2 turn(s)"), "got: {out}");
    }

    #[test]
    fn stale_outcome_bias_surfaces_in_summary_and_self_model() {
        let mut session = astra_runtime::observability::ObservabilitySession::new_simple("s1");
        session.turn_number = 17;
        session.outcome_bias.insert(
            "bash".to_string(),
            astra_turn_core::tool_health::OutcomeBiasEntry {
                score: -0.1,
                last_failure_tag: Some("timeout".to_string()),
            },
        );
        let fingerprint = astra_turn_core::injection_tracking::InjectionFingerprint::from_content(
            "bash=-0.100:timeout",
        );
        for round in 0..=17 {
            session.injection_history.observe(
                round,
                astra_turn_core::injection_tracking::InjectionChannel::OutcomeBias,
                fingerprint.clone(),
            );
        }

        let executor =
            test_executor().with_observability_session(Arc::new(std::sync::RwLock::new(session)));
        let summary = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));
        assert!(
            summary.contains("stale_injection: outcome_bias unchanged for 17 rounds"),
            "summary must surface stale outcome_bias alert: {summary}",
        );

        let self_model = executor
            .build_self_model_snapshot()
            .expect("observability session should produce self model");
        let rendered = self_model.to_system_prompt_section();
        assert!(
            rendered.contains("Stale runtime signals: stale_injection: outcome_bias"),
            "SelfModel must demote stale outcome_bias in prompt context: {rendered}",
        );
        assert!(rendered.contains("weak prior"), "got: {rendered}");
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
    fn introspect_cache_facet_routes_to_cache_diagnosis() {
        let executor = test_executor();
        // No session set → renderer explains the "no data" path.
        let out = executor.handle_introspect(&serde_json::json!({"facet": "cache"}));
        assert!(
            out.contains("Cache Diagnosis"),
            "facet=cache must produce the cache section, got: {out}",
        );
        assert!(
            out.contains("No per-round cache snapshots"),
            "without a session / captures, the renderer must explain why: {out}",
        );
    }

    #[test]
    fn introspect_cache_facet_falls_back_to_live_recent_rounds() {
        let executor = test_executor();
        executor.update_introspect_snapshot(astra_turn_core::introspect::IntrospectSnapshot {
            recent_rounds: vec![astra_turn_core::introspect::RoundSnapshotEntry {
                turn: 3,
                round: 1,
                provider: "deepseek".to_string(),
                model: "deepseek-chat".to_string(),
                prompt_tokens: 2_000,
                cache_read_tokens: 1_200,
                cache_creation_tokens: 300,
                completion_tokens: 150,
                tool_calls_returned: 2,
                tool_call_names: vec!["rg".to_string(), "read_file".to_string()],
                duration_ms: 77,
                finish_reason: Some("tool_calls".to_string()),
            }],
            ..Default::default()
        });

        let out = executor.handle_introspect(&serde_json::json!({"facet": "cache"}));

        assert!(out.contains("live `recent_rounds` ring"), "got: {out}");
        assert!(out.contains("| t3_r1 |"), "live round missing: {out}");
        assert!(
            out.contains("cache_create=300") || out.contains("| 300 |"),
            "got: {out}"
        );
        assert!(
            out.contains("Cache-control marker placement diagnosis")
                || out.contains("cache-control marker placement diagnosis"),
            "fallback must state full-capture limitation: {out}",
        );
    }

    #[test]
    fn introspect_cache_cloud_only_does_not_read_edge_artifacts() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({
            "facet": "cache",
            "source_policy": "cloud_only",
        }));

        assert!(
            !out.contains("Cache Diagnosis"),
            "cloud_only must not route to local cache diagnosis: {out}"
        );
        assert!(
            out.contains("Introspect Unavailable")
                && out.contains("source_policy=cloud_only")
                && out.contains("requested source_policy does not allow CLI/Edge-local artifacts"),
            "cloud_only cache request should report unavailable local coverage: {out}"
        );
    }

    #[test]
    fn introspect_depth_summary_session_is_default_behavior() {
        // Without facet the tool still shows the current runtime snapshot.
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"depth": "summary"}));
        assert!(
            out.contains("Current Runtime Snapshot"),
            "default facet must preserve session output, got: {out}",
        );
    }

    #[test]
    fn introspect_cache_facet_is_case_insensitive() {
        let executor = test_executor();
        let out = executor.handle_introspect(&serde_json::json!({"facet": "Cache"}));
        assert!(out.contains("Cache Diagnosis"), "got: {out}");
    }

    #[test]
    fn introspect_diagnostic_depth_includes_step_latency() {
        let executor = test_executor();
        executor.update_introspect_snapshot(astra_turn_core::introspect::IntrospectSnapshot {
            step_latency: vec![astra_turn_core::introspect::StepLatencySnapshotEntry {
                step_id: "turn-1-step-3".into(),
                total_ms: Some(8_978),
                pre_tool_wait_ms: Some(8_000),
                first_tool_name: Some("bash".into()),
                tool_execution_ms: 8,
                max_tool_execution_ms: 8,
                tool_call_count: 1,
                dominant_phase: "model_wait".into(),
                terminal_event_kind: Some("StepIncomplete".into()),
                ..Default::default()
            }],
            ..Default::default()
        });

        let out = executor.handle_introspect(&serde_json::json!({"depth": "diagnostic"}));

        assert!(out.contains("## Step Latency"), "got: {out}");
        assert!(out.contains("model_wait"), "got: {out}");
        assert!(out.contains("8000"), "got: {out}");
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

    #[tokio::test]
    async fn memory_inventory_reads_authoritative_local_journal_without_memoria_recall() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "cli-memory-inventory";
        let writer = astra_services::session_journal::JournalWriter::new(session_id).unwrap();
        writer
            .append(&astra_services::session_journal::JournalEvent::session_memory_extraction(
                Some(session_id),
                5,
                20,
                astra_services::session_journal::SessionMemoryExtractionOutcome::Extracted {
                    source:
                        astra_services::session_journal::SessionMemoryExtractionSource::RuleFallback,
                    bytes_written: 80,
                },
                &astra_services::session_journal::SessionMemoryExtractionBreadcrumbs::default(),
            ))
            .unwrap();
        let executor = test_executor().with_active_session_id(session_id);

        let output = executor
            .execute("memory", &serde_json::json!({"action": "inventory"}))
            .await;
        let inventory: astra_services::session_memory_inventory::SessionMemoryInventory =
            serde_json::from_str(&output).unwrap();

        assert_eq!(inventory.session_id, session_id);
        assert_eq!(inventory.successful_extraction_versions, 1);
        assert_eq!(inventory.rule_fallback_versions, 1);
        assert_eq!(inventory.logical_current_snapshot_count, Some(0));
        assert_eq!(inventory.inventory_source, "local_journal");
    }

    #[tokio::test]
    async fn memory_inventory_surfaces_corrupt_journal_instead_of_reporting_zero() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "cli-memory-inventory-corrupt";
        let path = astra_services::session_journal::journal_file_path(session_id);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{not-json}\n").unwrap();
        let executor = test_executor().with_active_session_id(session_id);

        let output = executor
            .execute("memory", &serde_json::json!({"action": "inventory"}))
            .await;

        assert!(output.starts_with("Error: session memory inventory failed:"));
        assert!(output.contains("cannot be exact"));
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

    // ── introspect facet=session_memory (unhappy first) ───────────────

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
        let out = executor.handle_introspect(&serde_json::json!({"facet": "session_memory"}));
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
        let out = executor.handle_introspect(&serde_json::json!({"facet": "session_memory"}));
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
        let out = executor.handle_introspect(&serde_json::json!({"facet": "session_memory"}));
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
        let out = executor.handle_introspect(&serde_json::json!({"facet": "session_memory"}));
        assert!(out.contains("journal unavailable:"), "{out}");
        assert!(
            out.contains("failed to read session journal for sess-introspect-unreadable"),
            "{out}"
        );
        assert!(out.contains("# session-memory diagnostics"), "{out}");
    }
}
