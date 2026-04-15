//! # astra-tools
//!
//! Extracted tool execution library for astra-engine. This crate contains the
//! pure tool logic (file I/O, shell, git, code intelligence, etc.) decoupled
//! from CLI-specific concerns (terminal rendering, MCP dispatch, passive LSP).
//!
//! Both the CLI (`CliToolExecutor`) and the server (`ServerToolExecutor`) depend
//! on this crate and compose its `DefaultToolExecutor` with their own wrappers.

pub mod memoria;
pub mod schemas;
pub mod web_search;

pub mod build_test;
pub mod code_intel;
pub mod config_tool;
pub mod env_tools;
pub mod executor;
pub mod fs_ops;
pub mod fuzzy_replacer;
pub mod git_gix;
pub mod git_ops;
pub mod github;
pub mod passive_cargo_check;
pub mod passive_tsc_check;
pub mod shell_ops;
pub mod task_mgmt;
pub mod tool_search;

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

// ─── Core trait ─────────────────────────────────────────────────────────────

/// Result of a single tool execution.
#[derive(Debug, Clone, Default)]
pub struct ToolResult {
    /// Primary text output (shown to LLM and user).
    pub output: String,
    /// Optional structured metadata fields (e.g., for tool_call_end events).
    pub metadata: Option<serde_json::Map<String, Value>>,
    /// Whether this result represents an error condition.
    pub is_error: bool,
}

impl ToolResult {
    /// Convenience constructor for a plain text result.
    pub fn text(output: String) -> Self {
        Self {
            output,
            metadata: None,
            is_error: false,
        }
    }

    /// Convenience constructor for an error result.
    pub fn error(message: String) -> Self {
        Self {
            output: message,
            metadata: None,
            is_error: true,
        }
    }

    /// Convert a legacy `String` output into a `ToolResult`.
    ///
    /// Heuristic: strings starting with `"Error"` are treated as errors.
    pub fn from_string(output: String) -> Self {
        if output.starts_with("Error") {
            Self::error(output)
        } else {
            Self::text(output)
        }
    }
}

/// Trait for executing tools. Implementations provide the actual tool logic
/// while allowing different execution contexts (CLI, server, test).
#[async_trait]
pub trait ToolExecutor: Send + Sync {
    /// Execute a tool by name with the given JSON arguments.
    async fn execute(&self, name: &str, args: &Value) -> ToolResult;

    /// Return the JSON schemas for all available tools (OpenAI function-calling format).
    fn tool_schemas(&self) -> Vec<Value>;

    /// The root directory for file operations and path resolution.
    fn project_root(&self) -> &Path;

    /// Execute a tool and return extended metadata (e.g., rollback journal entries).
    /// Default implementation delegates to `execute()`.
    async fn execute_with_metadata(&self, name: &str, args: &Value) -> ToolResult {
        self.execute(name, args).await
    }
}

// ─── Sandbox configuration ──────────────────────────────────────────────────

/// Sandbox enforcement level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SandboxMode {
    /// No restrictions — backward compatible.
    Permissive,
    /// Path boundary enforcement + env filtering.
    Standard,
    /// Full isolation: Standard + restricted shell + optional namespace.
    Strict,
    /// Read-only access: no writes, no shell mutations.
    ReadOnly,
}

/// Sandbox configuration for tool execution.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// Primary project directory.
    pub project_root: PathBuf,
    /// Additional allowed path prefixes.
    pub allowed_paths: Vec<PathBuf>,
    /// Sandbox enforcement level.
    pub mode: SandboxMode,
    /// Maximum output size in bytes before truncation.
    pub max_output_bytes: usize,
    /// Maximum command execution time.
    pub command_timeout: Duration,
    /// Whether to allow network access from bash commands.
    pub network_allowed: bool,
}

impl SandboxConfig {
    /// Create a standard sandbox config for a project directory.
    pub fn standard(project_root: impl Into<PathBuf>) -> Self {
        let root = project_root.into();
        Self {
            project_root: root,
            allowed_paths: vec![
                PathBuf::from("/tmp"),
                dirs::home_dir()
                    .unwrap_or_else(|| PathBuf::from("/root"))
                    .join(".config"),
            ],
            mode: SandboxMode::Standard,
            max_output_bytes: 200_000,
            command_timeout: Duration::from_secs(120),
            network_allowed: true,
        }
    }

    /// Create a strict sandbox for server-side execution.
    pub fn strict(project_root: impl Into<PathBuf>) -> Self {
        let root = project_root.into();
        Self {
            project_root: root,
            allowed_paths: vec![PathBuf::from("/tmp")],
            mode: SandboxMode::Strict,
            max_output_bytes: 200_000,
            command_timeout: Duration::from_secs(120),
            network_allowed: false,
        }
    }
}

// ─── Tool context ───────────────────────────────────────────────────────────

/// Unified execution context passed to all tool modules.
///
/// Replaces ad-hoc parameter passing with a single struct that carries
/// everything a tool needs to run: paths, identity, sandbox rules, and logging.
#[derive(Clone)]
pub struct ToolContext {
    /// Primary project directory (git root).
    pub project_root: PathBuf,
    /// Workspace root for this session (may differ from project_root in server mode).
    pub workspace_root: PathBuf,
    /// Authenticated user running the tools.
    pub user_id: String,
    /// Session identifier for multi-tenant isolation.
    pub session_id: String,
    /// Sandbox enforcement rules.
    pub sandbox: SandboxConfig,
    /// Shared HTTP client for tools that make network requests (GitHub, web_search).
    pub http_client: Option<reqwest::Client>,
    /// Logger for tool-level diagnostics (replaces eprintln! in extracted modules).
    pub logger: std::sync::Arc<dyn ToolLogger>,
}

impl ToolContext {
    /// Create a minimal context for unit tests.
    pub fn test(project_root: impl Into<PathBuf>) -> Self {
        let root = project_root.into();
        Self {
            workspace_root: root.clone(),
            sandbox: SandboxConfig::standard(&root),
            project_root: root,
            user_id: "test-user".into(),
            session_id: "test-session".into(),
            http_client: None,
            logger: std::sync::Arc::new(TracingLogger),
        }
    }
}

impl std::fmt::Debug for ToolContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolContext")
            .field("project_root", &self.project_root)
            .field("workspace_root", &self.workspace_root)
            .field("user_id", &self.user_id)
            .field("session_id", &self.session_id)
            .field("sandbox_mode", &self.sandbox.mode)
            .finish()
    }
}

// ─── Tool logger ────────────────────────────────────────────────────────────

/// Trait-based logging for tool modules.
///
/// Extracted tool code must NOT use `eprintln!` (breaks server mode).
/// Instead, tools receive a `&dyn ToolLogger` via [`ToolContext`] and log
/// through it.  The CLI provides [`StderrLogger`], the server provides
/// [`TracingLogger`] (default).
pub trait ToolLogger: Send + Sync {
    fn debug(&self, msg: &str);
    fn info(&self, msg: &str);
    fn warn(&self, msg: &str);
    fn error(&self, msg: &str);
}

/// Logger that routes to the `tracing` crate (server default).
#[derive(Debug, Clone, Copy)]
pub struct TracingLogger;

impl ToolLogger for TracingLogger {
    fn debug(&self, msg: &str) {
        tracing::debug!("{}", msg);
    }
    fn info(&self, msg: &str) {
        tracing::info!("{}", msg);
    }
    fn warn(&self, msg: &str) {
        tracing::warn!("{}", msg);
    }
    fn error(&self, msg: &str) {
        tracing::error!("{}", msg);
    }
}

/// Logger that writes to stderr (CLI backward compat).
#[derive(Debug, Clone, Copy)]
pub struct StderrLogger;

impl ToolLogger for StderrLogger {
    fn debug(&self, msg: &str) {
        eprintln!("[DEBUG] {msg}");
    }
    fn info(&self, msg: &str) {
        eprintln!("[INFO] {msg}");
    }
    fn warn(&self, msg: &str) {
        eprintln!("[WARN] {msg}");
    }
    fn error(&self, msg: &str) {
        eprintln!("[ERROR] {msg}");
    }
}

// ─── Journal traits ─────────────────────────────────────────────────────────

/// Trait for recording file edit history (undo support).
pub trait FileEditJournal: Send + Sync {
    /// Record the before-state of a file about to be edited.
    fn record_before(&mut self, path: &Path, tool_call_id: &str, turn_index: u32);
    /// Record the after-state of a file that was just written.
    fn record_after(&mut self, path: &Path, tool_call_id: &str, content: &[u8]);
    /// Undo all edits to a specific file.
    fn undo_file(&mut self, path: &Path) -> Result<Vec<PathBuf>, String>;
    /// Undo all edits from a specific turn.
    fn undo_turn(&mut self, turn_index: u32) -> Result<Vec<PathBuf>, String>;
}

/// Trait for recording git operations (rollback support).
pub trait GitRollbackJournal: Send + Sync {
    /// Record a commit that was just created (for potential revert).
    fn record_commit(&mut self, commit_hash: &str, message: &str);
    /// Record a stash that was just created (for potential restore).
    fn record_stash(&mut self, stash_ref: &str, message: &str);
    /// Revert the most recent recorded commit.
    fn revert_last_commit(&mut self) -> Result<String, String>;
    /// Restore the most recent recorded stash.
    fn restore_last_stash(&mut self) -> Result<String, String>;
}

// ─── Tool approval gate ─────────────────────────────────────────────────────

/// Decision from the approval gate.
#[derive(Debug, Clone)]
pub enum ApprovalDecision {
    /// Tool execution approved.
    Approved,
    /// Tool execution denied with optional reason.
    Denied { reason: Option<String> },
    /// Approval timed out.
    Timeout,
}

/// Trait for gating tool execution behind user approval (e.g., over WebSocket).
///
/// Implementations send a request to the frontend and wait for the user's
/// decision. The gate is optional — when no gate is configured, all tools
/// execute immediately.
#[async_trait]
pub trait ToolApprovalGate: Send + Sync {
    /// Request approval for a tool invocation. The implementation should send
    /// the request to the UI and block until the user responds or timeout.
    async fn request_approval(
        &self,
        request_id: &str,
        tool_name: &str,
        args: &Value,
    ) -> ApprovalDecision;

    /// Returns `true` if this tool requires approval before execution.
    fn requires_approval(&self, tool_name: &str) -> bool;
}

// ─── Tool progress callback ─────────────────────────────────────────────────

/// Callback for streaming tool execution progress to the frontend.
///
/// Implementations forward events over WebSocket or SSE so the UI can show
/// real-time tool output (e.g., bash streaming, file diff preview).
#[async_trait]
pub trait ToolProgressCallback: Send + Sync {
    /// Tool execution is about to start.
    async fn tool_started(&self, call_id: &str, tool_name: &str, args: &Value);

    /// Incremental output from the tool (e.g., bash stdout line).
    async fn tool_output_delta(&self, call_id: &str, delta: &str);

    /// Tool execution completed.
    async fn tool_completed(&self, call_id: &str, result: &str, success: bool);
}

/// Tools that require user approval in server-side execution by default.
pub const APPROVAL_REQUIRED_TOOLS: &[&str] = &[
    "bash",
    "write_file",
    "str_replace",
    "delete_file",
    "git_commit",
];

// ─── Output management utilities ────────────────────────────────────────────

/// Global output size limit. Override with `MO_GLOBAL_OUTPUT_LIMIT` env var.
pub fn global_output_limit() -> usize {
    astra_core::RuntimeLimits::global().global_output_limit
}

/// Per-tool default output limit. Override with `MO_TOOL_OUTPUT_LIMIT` env var.
pub fn tool_output_limit() -> usize {
    astra_core::RuntimeLimits::global().tool_output_limit
}

/// Per-tool output size caps (bytes).
pub fn per_tool_output_limit(tool_name: &str) -> usize {
    let base = tool_output_limit();
    match tool_name {
        "grep" => base.min(10_000),
        "glob" => base.min(100_000),
        "find_definition" | "find_references" => base.min(15_000),
        _ => base,
    }
}

/// Per-turn aggregate output budget (bytes).
pub const AGGREGATE_OUTPUT_BUDGET: usize = 200_000;

/// Soft threshold for aggregate-aware gating.
pub const AGGREGATE_SOFT_LIMIT: usize = 120_000;

/// Per-tool persistence threshold (chars).
pub const PERSIST_THRESHOLD: usize = 50_000;

/// Preview size for persisted output references.
pub const PERSIST_PREVIEW_BYTES: usize = 2000;

/// Truncate tool output to `max_bytes`, cutting at a newline boundary when
/// possible to avoid mid-line cuts that confuse the LLM.
pub fn truncate_output(mut output: String, max_bytes: usize) -> String {
    if output.len() > max_bytes {
        let end = output.floor_char_boundary(max_bytes);
        let cut = output[..end]
            .rfind('\n')
            .filter(|&pos| pos > end / 2)
            .map(|pos| pos + 1)
            .unwrap_or(end);
        output.truncate(cut);
        output.push_str("\n[truncated]");
    }
    output
}

/// Normalize empty/whitespace-only tool output to a short marker.
pub fn normalize_empty_output(output: String, tool_name: &str) -> String {
    if output.trim().is_empty() {
        format!("({tool_name} completed with no output)")
    } else {
        output
    }
}

/// Directory for persisted tool results within the astra home.
pub fn tool_results_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("tool-results")
}

/// Persist large tool output to disk when aggregate budget is under pressure.
pub fn maybe_persist_large_output(
    output: String,
    aggregate_bytes: usize,
    _tool_name: &str,
) -> String {
    if output.len() < PERSIST_THRESHOLD {
        return output;
    }
    if aggregate_bytes <= AGGREGATE_SOFT_LIMIT {
        return output;
    }
    if output.starts_with("Error:") {
        return output;
    }

    let dir = tool_results_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return output;
    }

    let hash = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        output.hash(&mut h);
        format!("{:016x}", h.finish())
    };
    let filepath = dir.join(format!("{hash}.txt"));

    if !filepath.exists() && std::fs::write(&filepath, &output).is_err() {
        return output;
    }

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

// ─── Shared utility functions ───────────────────────────────────────────────

/// Convert UTF-16 column position to char index (for LSP protocol compatibility).
pub fn utf16_col_to_char_idx(line: &str, col_utf16: usize) -> usize {
    let mut utf16_offset = 0;
    for (char_idx, c) in line.chars().enumerate() {
        if utf16_offset + c.len_utf16() > col_utf16 {
            return char_idx;
        }
        utf16_offset += c.len_utf16();
    }
    line.chars().count()
}

/// Parse a grep output line to extract the file path and line number.
pub fn parse_grep_file_line(line: &str) -> Option<(&str, usize)> {
    let first_colon = line.find(':')?;
    let file = &line[..first_colon];
    let rest = &line[first_colon + 1..];
    let second_colon = rest.find(':')?;
    let line_str = &rest[..second_colon];
    let line_num: usize = line_str.parse().ok()?;
    Some((file, line_num))
}

/// Categorize a grep reference line as definition, import, call, or usage.
pub fn categorize_reference(line: &str, _symbol: &str) -> &'static str {
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

    if lower.starts_with("use ")
        || lower.starts_with("pub use ")
        || lower.starts_with("import ")
        || lower.starts_with("from ")
        || lower.contains("require(")
        || lower.starts_with("#include")
    {
        return "import";
    }

    if lower.starts_with("fn ")
        || lower.starts_with("pub fn ")
        || lower.starts_with("pub(crate) fn ")
        || lower.starts_with("async fn ")
        || lower.starts_with("pub async fn ")
        || lower.starts_with("def ")
        || lower.starts_with("class ")
        || lower.starts_with("struct ")
        || lower.starts_with("enum ")
        || lower.starts_with("trait ")
        || lower.starts_with("type ")
        || lower.starts_with("interface ")
        || lower.starts_with("const ")
        || lower.starts_with("pub const ")
        || lower.starts_with("static ")
        || lower.starts_with("pub static ")
        || lower.starts_with("let ")
        || lower.starts_with("pub let ")
        || lower.starts_with("var ")
        || lower.starts_with("function ")
        || lower.starts_with("export function ")
        || lower.starts_with("export default ")
        || lower.starts_with("export class ")
        || lower.starts_with("export const ")
    {
        return "definition";
    }

    "usage"
}

/// Git porcelain status codes for git status --porcelain output parsing.
pub mod git_status_codes {
    pub const MODIFIED: char = 'M';
    pub const ADDED: char = 'A';
    pub const DELETED: char = 'D';
    pub const RENAMED: char = 'R';
    pub const UNTRACKED_PREFIX: &str = "??";
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── ToolResult ─────────────────────────────────────────────────────

    #[test]
    fn tool_result_text() {
        let r = ToolResult::text("hello".into());
        assert_eq!(r.output, "hello");
        assert!(!r.is_error);
        assert!(r.metadata.is_none());
    }

    #[test]
    fn tool_result_error() {
        let r = ToolResult::error("boom".into());
        assert_eq!(r.output, "boom");
        assert!(r.is_error);
    }

    #[test]
    fn tool_result_default_is_empty() {
        let r = ToolResult::default();
        assert!(r.output.is_empty());
        assert!(!r.is_error);
        assert!(r.metadata.is_none());
    }

    // ── SandboxConfig ──────────────────────────────────────────────────

    #[test]
    fn sandbox_standard_defaults() {
        let cfg = SandboxConfig::standard("/projects/foo");
        assert_eq!(cfg.project_root, PathBuf::from("/projects/foo"));
        assert_eq!(cfg.mode, SandboxMode::Standard);
        assert!(cfg.network_allowed);
        assert_eq!(cfg.max_output_bytes, 200_000);
        assert_eq!(cfg.command_timeout, Duration::from_secs(120));
        assert!(cfg.allowed_paths.contains(&PathBuf::from("/tmp")));
    }

    #[test]
    fn sandbox_strict_defaults() {
        let cfg = SandboxConfig::strict("/srv/workspace");
        assert_eq!(cfg.mode, SandboxMode::Strict);
        assert!(!cfg.network_allowed);
        assert_eq!(cfg.allowed_paths, vec![PathBuf::from("/tmp")]);
    }

    #[test]
    fn sandbox_mode_ordering() {
        assert!(SandboxMode::Permissive < SandboxMode::Standard);
        assert!(SandboxMode::Standard < SandboxMode::Strict);
        assert!(SandboxMode::Strict < SandboxMode::ReadOnly);
    }

    // ── Output utilities ───────────────────────────────────────────────

    #[test]
    fn truncate_output_short_stays_intact() {
        let s = "short output".to_string();
        assert_eq!(truncate_output(s.clone(), 1000), s);
    }

    #[test]
    fn truncate_output_long_gets_cut() {
        let s = "a\n".repeat(5000); // 10000 chars
        let result = truncate_output(s, 100);
        assert!(result.len() < 200);
        assert!(result.ends_with("[truncated]"));
    }

    #[test]
    fn truncate_output_prefers_newline_boundary() {
        let s = "line one\nline two\nline three that is quite long\n".to_string();
        let result = truncate_output(s, 30);
        // Should cut at a newline
        assert!(result.ends_with("[truncated]"));
        assert!(!result.contains("line three"));
    }

    #[test]
    fn normalize_empty_output_replaces_blank() {
        let result = normalize_empty_output("".into(), "bash");
        assert_eq!(result, "(bash completed with no output)");
    }

    #[test]
    fn normalize_empty_output_replaces_whitespace() {
        let result = normalize_empty_output("   \n\t  ".into(), "grep");
        assert_eq!(result, "(grep completed with no output)");
    }

    #[test]
    fn normalize_empty_output_keeps_real_content() {
        let result = normalize_empty_output("hello".into(), "bash");
        assert_eq!(result, "hello");
    }

    #[test]
    fn per_tool_output_limit_grep_is_capped() {
        let base = tool_output_limit();
        let grep_limit = per_tool_output_limit("grep");
        assert!(grep_limit <= 10_000);
        assert!(grep_limit <= base);
    }

    #[test]
    fn per_tool_output_limit_bash_uses_base() {
        let base = tool_output_limit();
        assert_eq!(per_tool_output_limit("bash"), base);
    }

    // ── Utility functions ──────────────────────────────────────────────

    #[test]
    fn utf16_col_ascii() {
        assert_eq!(utf16_col_to_char_idx("hello world", 5), 5);
    }

    #[test]
    fn utf16_col_beyond_line_end() {
        let idx = utf16_col_to_char_idx("hi", 999);
        assert_eq!(idx, 2); // line.chars().count()
    }

    #[test]
    fn parse_grep_file_line_valid() {
        let (file, line) = parse_grep_file_line("src/main.rs:42:fn main()").unwrap();
        assert_eq!(file, "src/main.rs");
        assert_eq!(line, 42);
    }

    #[test]
    fn parse_grep_file_line_invalid() {
        assert!(parse_grep_file_line("no colons here").is_none());
        assert!(parse_grep_file_line("file:notanum:text").is_none());
    }

    #[test]
    fn categorize_reference_import() {
        assert_eq!(categorize_reference("f.rs:1:use std::io;", "io"), "import");
        assert_eq!(
            categorize_reference("f.rs:1:import React from 'react';", "React"),
            "import"
        );
        assert_eq!(
            categorize_reference("f.rs:1:from os import path", "path"),
            "import"
        );
    }

    #[test]
    fn categorize_reference_definition() {
        assert_eq!(
            categorize_reference("f.rs:1:fn my_func() {}", "my_func"),
            "definition"
        );
        assert_eq!(
            categorize_reference("f.rs:1:pub fn my_func() {}", "my_func"),
            "definition"
        );
        assert_eq!(
            categorize_reference("f.rs:1:struct Foo {}", "Foo"),
            "definition"
        );
        assert_eq!(
            categorize_reference("f.rs:1:class MyClass:", "MyClass"),
            "definition"
        );
    }

    #[test]
    fn categorize_reference_usage() {
        assert_eq!(
            categorize_reference("f.rs:1:    my_func();", "my_func"),
            "usage"
        );
        assert_eq!(
            categorize_reference("f.rs:1:    x = foo.bar();", "bar"),
            "usage"
        );
        // Note: "let x = ..." is categorized as "definition" by design (it starts with "let ")
        assert_eq!(
            categorize_reference("f.rs:1:let x = foo.bar();", "bar"),
            "definition"
        );
    }

    // ── APPROVAL_REQUIRED_TOOLS ────────────────────────────────────────

    #[test]
    fn approval_required_tools_includes_dangerous_ops() {
        assert!(APPROVAL_REQUIRED_TOOLS.contains(&"bash"));
        assert!(APPROVAL_REQUIRED_TOOLS.contains(&"write_file"));
        assert!(APPROVAL_REQUIRED_TOOLS.contains(&"delete_file"));
        assert!(APPROVAL_REQUIRED_TOOLS.contains(&"git_commit"));
        assert!(!APPROVAL_REQUIRED_TOOLS.contains(&"read_file"));
        assert!(!APPROVAL_REQUIRED_TOOLS.contains(&"grep"));
    }

    // ── ApprovalDecision ───────────────────────────────────────────────

    #[test]
    fn approval_decision_variants_debug() {
        let _ = format!("{:?}", ApprovalDecision::Approved);
        let _ = format!(
            "{:?}",
            ApprovalDecision::Denied {
                reason: Some("nope".into())
            }
        );
        let _ = format!("{:?}", ApprovalDecision::Timeout);
    }

    // ── Schemas ────────────────────────────────────────────────────────

    #[test]
    fn all_tool_schemas_has_expected_tools() {
        let schemas = schemas::all_tool_schemas();
        let names: Vec<String> = schemas
            .iter()
            .filter_map(|s| {
                s.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str())
                    .map(String::from)
            })
            .collect();
        // Core tools that must be present
        assert!(names.contains(&"bash".into()));
        assert!(names.contains(&"read_file".into()));
        assert!(names.contains(&"write_file".into()));
        assert!(names.contains(&"str_replace".into()));
        assert!(names.contains(&"grep".into()));
        assert!(names.contains(&"glob".into()));
        assert!(names.contains(&"memory_store".into()));
        assert!(names.contains(&"memory_search".into()));
        assert!(names.contains(&"memory_profile".into()));
    }

    #[test]
    fn all_tool_schemas_valid_structure() {
        let schemas = schemas::all_tool_schemas();
        assert!(!schemas.is_empty());
        for schema in &schemas {
            // Each schema must have type: "function" and function.name
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("function")
            );
            let func = schema.get("function").expect("missing 'function' key");
            assert!(func.get("name").and_then(|n| n.as_str()).is_some());
            assert!(func.get("description").and_then(|d| d.as_str()).is_some());
        }
    }

    // ── Persist large output ───────────────────────────────────────────

    #[test]
    fn maybe_persist_small_output_passes_through() {
        let out = "small output".to_string();
        let result = maybe_persist_large_output(out.clone(), 0, "bash");
        assert_eq!(result, out);
    }

    #[test]
    fn maybe_persist_under_soft_limit_passes_through() {
        let out = "x".repeat(PERSIST_THRESHOLD + 1);
        let result = maybe_persist_large_output(out.clone(), 0, "bash");
        // Under AGGREGATE_SOFT_LIMIT, even large output passes through
        assert_eq!(result, out);
    }

    #[test]
    fn maybe_persist_error_output_passes_through() {
        let out = format!("Error: {}", "x".repeat(PERSIST_THRESHOLD + 1));
        let result = maybe_persist_large_output(out.clone(), AGGREGATE_SOFT_LIMIT + 1, "bash");
        // Error output is never persisted
        assert_eq!(result, out);
    }

    // ── git_status_codes ───────────────────────────────────────────────

    #[test]
    fn git_status_codes_constants() {
        assert_eq!(git_status_codes::MODIFIED, 'M');
        assert_eq!(git_status_codes::ADDED, 'A');
        assert_eq!(git_status_codes::DELETED, 'D');
        assert_eq!(git_status_codes::RENAMED, 'R');
        assert_eq!(git_status_codes::UNTRACKED_PREFIX, "??");
    }
}
