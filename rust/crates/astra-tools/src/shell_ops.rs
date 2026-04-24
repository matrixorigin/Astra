//! Shell operations: bash execution, grep, glob.

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;
use tracing::debug;
use uuid::Uuid;

use astra_sandbox::{CommandRisk, analyze_command_risks};

use crate::{ToolResult, per_tool_output_limit, truncate_output};

const GREP_TIMEOUT: Duration = Duration::from_secs(20);
const GLOB_TIMEOUT: Duration = Duration::from_secs(15);
const GREP_DEFAULT_HEAD_LIMIT: usize = 100;
const GLOB_DEFAULT_HEAD_LIMIT: usize = 100;
const RAW_GREP_OUTPUT_LIMIT: usize = 30_000;
const RAW_GLOB_OUTPUT_LIMIT: usize = 120_000;
const RAW_STDERR_OUTPUT_LIMIT: usize = 16_000;

const DEFAULT_SEARCH_EXCLUDE_DIRS: &[&str] = &[
    ".git",
    "target",
    "dist",
    "build",
    "coverage",
    "htmlcov",
    "node_modules",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".cache",
    "out",
];

static RIPGREP_AVAILABLE: OnceLock<bool> = OnceLock::new();

#[derive(Clone, Copy, PartialEq, Eq)]
enum SearchOutputMode {
    Content,
    FilesWithMatches,
    Count,
}

#[derive(Clone, Copy)]
enum SearchSortMode {
    Mtime,
    Path,
}

#[derive(Clone, Copy)]
enum StreamKind {
    Stdout,
    Stderr,
}

struct ReadOnlyCommandOutput {
    stdout: String,
    stderr: String,
    exit_code: i32,
    timed_out: bool,
    cancelled: bool,
    stdout_capped: bool,
    stderr_capped: bool,
}

struct EnumeratedSearchFiles {
    files: Vec<String>,
    timed_out: bool,
    cancelled: bool,
}

struct MultilineMatchBlock {
    start_line: usize,
    end_line: usize,
    match_ranges: Vec<(usize, usize)>,
}

struct SearchResultGroup {
    path: Option<String>,
    lines: Vec<String>,
}

#[derive(Clone)]
struct SearchIgnoreRule {
    pattern: String,
    negated: bool,
}

/// Hard validation for `execute_bash` (and server-side `bash` when routed through the same path).
///
/// Layering:
/// 1. Local substring/heuristic rules (destructive `rm`, pipe-to-shell, netcat, etc.).
/// 2. [`analyze_command_risks`] (tree-sitter + legacy): any reported risk **blocks** except
///    [`CommandRisk::PathTraversal`] and [`CommandRisk::NetworkAccess`], which are allowed here
///    so normal `cd ../..` and `curl`/`wget` workflows remain usable (network still subject to
///    sandbox/permissions elsewhere). All other sandbox risks (e.g. [`CommandRisk::Eval`],
///    [`CommandRisk::ProcessSubstitution`]) fail closed so we never return Ok when the sandbox
///    flags a higher-severity pattern only in AST.
pub fn validate_execute_bash_command(command: &str) -> Result<(), String> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Err("Error: empty bash command".into());
    }
    let lower = cmd.to_ascii_lowercase();
    let blocked_substrings = [
        "rm -rf",
        "rm -fr",
        "\nrm ",
        " rmdir ",
        "mkfs",
        "mkswap",
        " wipefs",
        " dd if=",
        " dd of=",
        "shutdown",
        "reboot",
        "poweroff",
        "halt",
        "telinit",
        "kill -9",
        "pkill",
        "killall",
        " :(){",
        ":(){ :",
        "fork bomb",
    ];
    for pat in blocked_substrings {
        if lower.contains(pat) {
            return Err(format!(
                "Error: bash command matches a blocked destructive pattern ({pat:?})"
            ));
        }
    }
    // Every `rm` form is blocked here (including `rm -r`, `rm -f`, flags-only variants): never
    // treat recursive remove as "safe" — use structured file/delete tools instead.
    if lower.starts_with("rm ")
        || lower.contains("; rm ")
        || lower.contains("&& rm ")
        || lower.contains("| rm ")
    {
        return Err("Error: `rm` is blocked in execute_bash — use structured file tools".into());
    }
    if lower.starts_with("rmdir")
        || lower.contains("; rmdir")
        || lower.contains("&& rmdir")
        || lower.contains("| rmdir")
    {
        return Err("Error: `rmdir` is blocked in execute_bash — use structured file tools".into());
    }
    if (lower.contains("curl ") || lower.contains("wget "))
        && (lower.contains("| bash")
            || lower.contains("| sh")
            || lower.contains("| zsh")
            || lower.contains("> bash")
            || lower.contains("> sh"))
    {
        return Err("Error: piping download output into a shell is blocked in execute_bash".into());
    }
    if lower.contains("nc ") || lower.contains("netcat") || lower.contains("ncat ") {
        return Err("Error: netcat-style networking in bash is blocked".into());
    }

    for risk in analyze_command_risks(command) {
        match &risk {
            // Allowed here only — still constrained by local rules + permission layer.
            CommandRisk::PathTraversal | CommandRisk::NetworkAccess => {}
            // Intentionally still allowed: benign redirects / `$(...)` are common in build scripts;
            // AST marks them aggressively; blocking would break typical `cargo`/`make` usage.
            CommandRisk::OutputRedirection | CommandRisk::CommandSubstitution => {}
            // Fail closed on every other sandbox-reported risk (eval, process substitution, etc.).
            CommandRisk::RemoteCodeExecution
            | CommandRisk::PrivilegeEscalation
            | CommandRisk::ProcessControl
            | CommandRisk::EnvManipulation
            | CommandRisk::ZshDangerous(_)
            | CommandRisk::SensitivePathAccess(_)
            | CommandRisk::Eval
            | CommandRisk::ProcessSubstitution => {
                return Err(format!("Error: bash command blocked ({risk})"));
            }
        }
    }
    Ok(())
}

struct GrepRequest<'a> {
    workspace_root: &'a Path,
    target: &'a str,
    pattern: &'a str,
    include_globs: Vec<String>,
    ignore_rules: Vec<SearchIgnoreRule>,
    case_sensitive: bool,
    fixed_strings: bool,
    word_match: bool,
    before_context_lines: Option<usize>,
    after_context_lines: Option<usize>,
    max_matches: Option<usize>,
    output_mode: SearchOutputMode,
    multiline: bool,
}

/// Execute a bash command with bounded partial-output capture.
pub async fn execute_bash(ctx: &crate::ToolContext, args: &Value) -> ToolResult {
    let workspace_root = ctx.workspace_root.as_path();
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::error("Error: Missing 'command' parameter".into()),
    };
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0)
        .clamp(0.1, 120.0);

    if let Err(reason) = validate_execute_bash_command(command) {
        return ToolResult::error(reason);
    }

    let timeout = Duration::from_secs_f64(timeout_secs);
    let mut cmd = Command::new("bash");
    cmd.arg("-c")
        .arg(command)
        .current_dir(workspace_root)
        .kill_on_drop(true);

    let output_limit = per_tool_output_limit("bash");
    let raw_stdout_limit = output_limit.saturating_mul(2).max(16_384);
    let raw_stderr_limit = output_limit.clamp(8_192, 32_768);
    let output = match run_readonly_command_with_partial(
        &mut cmd,
        timeout,
        raw_stdout_limit,
        raw_stderr_limit,
        ctx.cancel_token.as_deref(),
        "bash command",
    )
    .await
    {
        Ok(output) => output,
        Err(e) => return ToolResult::error(e),
    };

    let mut result = String::new();
    if !output.stdout.is_empty() {
        result.push_str(&output.stdout);
    }
    if !output.stderr.is_empty() {
        if !result.is_empty() && !result.ends_with('\n') {
            result.push('\n');
        }
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str("stderr:\n");
        result.push_str(&output.stderr);
    }

    let mut cap_notes = Vec::new();
    if output.stdout_capped {
        cap_notes.push("stdout capped");
    }
    if output.stderr_capped {
        cap_notes.push("stderr capped");
    }
    if !cap_notes.is_empty() {
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str(&format!(
            "[bash output capped before completion: {}]",
            cap_notes.join(", ")
        ));
    }

    if output.exit_code != 0 && !output.timed_out && !output.cancelled {
        if !result.is_empty() {
            result.push_str("\n\n");
        }
        result.push_str(&format!("[exit code: {}]", output.exit_code));
    }

    if output.timed_out {
        if !result.is_empty() {
            result.push_str("\n\n");
            result.push_str(&format!(
                "[bash timed out after {timeout_secs}s — showing partial output]"
            ));
        } else {
            result = format!("Error: bash timed out after {timeout_secs}s with no captured output");
        }
        return ToolResult::error(truncate_output(result, output_limit));
    }

    if output.cancelled {
        if !result.is_empty() {
            result.push_str("\n\n[bash cancelled — showing partial output]");
        } else {
            result = "Error: bash cancelled before any output was captured".into();
        }
        return ToolResult::error(truncate_output(result, output_limit));
    }

    if output.exit_code != 0 {
        return ToolResult::error(truncate_output(result, output_limit));
    }

    if result.is_empty() {
        ToolResult::text("(command completed with no output)".into())
    } else {
        ToolResult::text(truncate_output(result, output_limit))
    }
}

/// Search files with bounded, cancellable subprocess execution.
pub async fn grep(ctx: &crate::ToolContext, args: &Value) -> ToolResult {
    let workspace_root = ctx.workspace_root.as_path();
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'pattern' parameter".into()),
    };
    let requested_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let resolved = match resolve_existing_search_path(workspace_root, requested_path) {
        Ok(path) => path,
        Err(e) => return ToolResult::error(e),
    };
    let target = relative_search_target(workspace_root, &resolved);
    let ignore_rules = match load_search_ignore_rules(workspace_root) {
        Ok(rules) => rules,
        Err(e) => return ToolResult::error(e),
    };
    let mut include_globs = Vec::new();
    if let Some(value) = args.get("include") {
        match parse_search_globs(value, "include") {
            Ok(globs) => include_globs.extend(globs),
            Err(e) => return ToolResult::error(e),
        }
    }
    if let Some(value) = args.get("glob") {
        match parse_search_globs(value, "glob") {
            Ok(globs) => include_globs.extend(globs),
            Err(e) => return ToolResult::error(e),
        }
    }
    if let Some(value) = args.get("type") {
        match parse_search_type_globs(value) {
            Ok(globs) => include_globs.extend(globs),
            Err(e) => return ToolResult::error(e),
        }
    }
    dedup_preserve_order(&mut include_globs);
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let fixed_strings = args
        .get("fixed_strings")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let word_match = args
        .get("word_match")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .map(|value| value.min(10) as usize);
    let before_context_lines = args
        .get("before_context_lines")
        .and_then(|v| v.as_u64())
        .map(|value| value.min(10) as usize)
        .or(context_lines);
    let after_context_lines = args
        .get("after_context_lines")
        .and_then(|v| v.as_u64())
        .map(|value| value.min(10) as usize)
        .or(context_lines);
    let max_matches = args
        .get("max_matches")
        .and_then(|v| v.as_u64())
        .map(|value| value.max(1) as usize);
    let scope_context = args
        .get("scope_context")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let output_mode = match parse_search_output_mode(args) {
        Ok(mode) => mode,
        Err(e) => return ToolResult::error(e),
    };
    let sort_mode = match parse_search_sort_mode(args) {
        Ok(mode) => mode,
        Err(e) => return ToolResult::error(e),
    };
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let head_limit = args
        .get("head_limit")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize);
    let multiline = args
        .get("multiline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let request = GrepRequest {
        workspace_root,
        target: &target,
        pattern,
        include_globs,
        ignore_rules,
        case_sensitive,
        fixed_strings,
        word_match,
        before_context_lines,
        after_context_lines,
        max_matches,
        output_mode,
        multiline,
    };

    let command_output =
        run_grep_with_preferred_backend(&request, ctx.cancel_token.as_deref(), ripgrep_available())
            .await;

    let ReadOnlyCommandOutput {
        stdout,
        stderr,
        exit_code,
        timed_out,
        cancelled,
        stdout_capped,
        stderr_capped: _stderr_capped,
    } = match command_output {
        Ok(output) => output,
        Err(e) => return ToolResult::error(e),
    };

    if exit_code == 1 && stdout.trim().is_empty() {
        return if stderr.trim().is_empty() {
            ToolResult::text("No matches found".into())
        } else {
            ToolResult::text(format!("No matches found (warnings: {})", stderr.trim()))
        };
    }

    if stdout.trim().is_empty() && exit_code != 0 {
        if cancelled {
            return ToolResult::error("Error: grep was cancelled before returning results.".into());
        }
        if timed_out {
            return ToolResult::error(
                "Error: grep timed out after 20s with no results. Narrow the search with 'path', 'include'/'glob', 'type', or a more specific pattern.".into(),
            );
        }

        return if stderr.trim().is_empty() {
            ToolResult::error("Error: grep failed".into())
        } else {
            ToolResult::error(format!("Error: {}", stderr.trim()))
        };
    }

    let filtered = if output_mode == SearchOutputMode::Count {
        stdout
            .lines()
            .filter(|line| !line.ends_with(":0"))
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        stdout
    };

    let mut lines: Vec<String> = filtered
        .lines()
        .map(|line| normalize_grep_output_line(line, output_mode))
        .collect();
    let mut grep_paths = lines
        .iter()
        .filter_map(|line| extract_search_result_path(line, output_mode))
        .collect::<Vec<_>>();
    dedup_preserve_order(&mut grep_paths);
    let gitignored_paths = match load_gitignored_search_paths(workspace_root, &grep_paths).await {
        Ok(paths) => paths,
        Err(e) => return ToolResult::error(e),
    };
    sort_grep_result_lines(
        &mut lines,
        workspace_root,
        output_mode,
        sort_mode,
        &request.ignore_rules,
        &gitignored_paths,
    );
    if lines.is_empty() {
        return ToolResult::text(no_visible_results_message(
            "matches",
            timed_out,
            cancelled,
            stdout_capped,
            stderr.trim(),
        ));
    }
    let paged_lines = if offset > 0 {
        if offset >= lines.len() {
            return ToolResult::text(no_more_results_message(
                offset,
                lines.len(),
                "lines",
                timed_out,
                cancelled,
                stdout_capped,
            ));
        }
        &lines[offset..]
    } else {
        lines.as_slice()
    };

    let effective_limit = match head_limit {
        Some(0) => None,
        Some(value) => Some(value),
        None => Some(GREP_DEFAULT_HEAD_LIMIT),
    };
    let (visible_lines, was_truncated_by_limit) = if let Some(limit) = effective_limit {
        if paged_lines.len() > limit {
            (&paged_lines[..limit], true)
        } else {
            (paged_lines, false)
        }
    } else {
        (paged_lines, false)
    };

    let pre_truncate_text = visible_lines.join("\n");
    let was_truncated_by_output_limit = pre_truncate_text.len() > per_tool_output_limit("grep");
    let mut result_text = truncate_output(pre_truncate_text, per_tool_output_limit("grep"));

    if cancelled {
        if !result_text.is_empty() {
            result_text.push_str(
                "\n\n[grep cancelled — showing partial results captured before cancellation.]",
            );
        } else {
            result_text = "[grep cancelled before partial results were captured]".into();
        }
    } else if timed_out {
        if !result_text.is_empty() {
            result_text.push_str(
                "\n\n[grep timed out after 20s — showing partial results. Narrow the search with 'path', 'include'/'glob', or 'type'.]",
            );
        } else {
            result_text = "[grep timed out after 20s — no partial results captured]".into();
        }
    }

    if was_truncated_by_limit && let Some(limit) = effective_limit {
        result_text.push_str(&format!(
            "\n\n[Results limited to {limit} lines. Use 'offset' to paginate or 'head_limit: 0' for unlimited.]"
        ));
    }
    if stdout_capped {
        result_text.push_str(
            "\n\n[grep backend output capped before pagination. Narrow 'path', 'include'/'glob', 'type', or 'head_limit' for complete results.]",
        );
    }
    if was_truncated_by_output_limit {
        result_text.push_str(
            "\n\n[grep results truncated to the tool output limit. Narrow 'path' or lower 'head_limit' for complete output.]",
        );
    }
    if !stderr.trim().is_empty() {
        result_text.push_str(&format!(
            "\n\n[grep completed with warnings: {}]",
            format_search_warning(stderr.trim())
        ));
    }

    if scope_context && output_mode == SearchOutputMode::Content {
        result_text = annotate_grep_with_scope(&result_text, workspace_root);
    }

    ToolResult::text(result_text)
}

/// Find files matching a glob pattern without blocking the async executor.
pub async fn glob(ctx: &crate::ToolContext, args: &Value) -> ToolResult {
    let workspace_root = ctx.workspace_root.as_path();
    let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'pattern' parameter".into()),
    };
    if contains_path_traversal(pattern) {
        return ToolResult::error(
            "Error: glob pattern must not contain '..', start with '/', or contain '~/' (path traversal risk)"
                .into(),
        );
    }

    let requested_path = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let resolved = match resolve_existing_search_path(workspace_root, requested_path) {
        Ok(path) => path,
        Err(e) => return ToolResult::error(e),
    };
    let target = relative_search_target(workspace_root, &resolved);
    let sort_mode = match parse_search_sort_mode(args) {
        Ok(mode) => mode,
        Err(e) => return ToolResult::error(e),
    };
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let head_limit = args
        .get("head_limit")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize);
    let ignore_rules = match load_search_ignore_rules(workspace_root) {
        Ok(rules) => rules,
        Err(e) => return ToolResult::error(e),
    };

    if resolved.is_file() {
        return if glob_matches_path(pattern, &target)
            && !should_ignore_search_path(&target, &ignore_rules)
        {
            ToolResult::text(target)
        } else {
            ToolResult::text("No files found".into())
        };
    }

    let command_output = run_glob_with_preferred_backend(
        workspace_root,
        &target,
        pattern,
        ctx.cancel_token.as_deref(),
        ripgrep_available(),
    )
    .await;

    let ReadOnlyCommandOutput {
        stdout,
        stderr,
        exit_code,
        timed_out,
        cancelled,
        stdout_capped,
        stderr_capped: _stderr_capped,
    } = match command_output {
        Ok(output) => output,
        Err(e) => return ToolResult::error(e),
    };

    if stdout.trim().is_empty() && exit_code != 0 && !timed_out && !cancelled {
        return if stderr.trim().is_empty() {
            ToolResult::error("Error: glob failed".into())
        } else {
            ToolResult::error(format!("Error: {}", stderr.trim()))
        };
    }

    let mut files: Vec<String> = stdout
        .lines()
        .map(strip_current_dir_prefix)
        .filter(|line| !line.is_empty())
        .filter(|line| glob_matches_path(pattern, line))
        .filter(|line| !should_ignore_search_path(line, &ignore_rules))
        .collect();
    let gitignored_paths = match load_gitignored_search_paths(workspace_root, &files).await {
        Ok(paths) => paths,
        Err(e) => return ToolResult::error(e),
    };
    files.retain(|line| !gitignored_paths.contains(line));
    dedup_preserve_order(&mut files);
    sort_search_paths(&mut files, workspace_root, sort_mode);

    if files.is_empty() {
        return ToolResult::text(no_visible_results_message(
            "files",
            timed_out,
            cancelled,
            stdout_capped,
            stderr.trim(),
        ));
    }

    let total_files = files.len();
    let paged_files = if offset > 0 {
        if offset >= files.len() {
            return ToolResult::text(no_more_results_message(
                offset,
                files.len(),
                "files",
                timed_out,
                cancelled,
                stdout_capped,
            ));
        }
        &files[offset..]
    } else {
        files.as_slice()
    };
    let effective_limit = match head_limit {
        Some(0) => None,
        Some(value) => Some(value),
        None => Some(GLOB_DEFAULT_HEAD_LIMIT),
    };
    let (visible_files, was_truncated_by_limit) = if let Some(limit) = effective_limit {
        if paged_files.len() > limit {
            (&paged_files[..limit], true)
        } else {
            (paged_files, false)
        }
    } else {
        (paged_files, false)
    };

    let pre_truncate_text = visible_files.join("\n");
    let was_truncated_by_output_limit = pre_truncate_text.len() > per_tool_output_limit("glob");
    let mut result_text = truncate_output(pre_truncate_text, per_tool_output_limit("glob"));

    if cancelled {
        result_text.push_str(
            "\n\n[glob cancelled — showing partial results captured before cancellation.]",
        );
    } else if timed_out {
        result_text.push_str(
            "\n\n[glob timed out after 15s — showing partial results. Narrow the search with 'path' or a more specific pattern.]",
        );
    } else {
        result_text.push_str(&format!(
            "\n(showing {} of {} files)",
            visible_files.len(),
            total_files
        ));
    }
    if was_truncated_by_limit && let Some(limit) = effective_limit {
        result_text.push_str(&format!(
            "\n\n[Results limited to {limit} files. Use 'offset' to paginate or 'head_limit: 0' for unlimited.]"
        ));
    }
    if stdout_capped {
        result_text.push_str(
            "\n\n[glob backend output capped before pagination. Narrow 'path' or use a more specific pattern for complete results.]",
        );
    }
    if was_truncated_by_output_limit {
        result_text.push_str(
            "\n\n[glob results truncated to the tool output limit. Narrow 'path' or lower 'head_limit' for complete output.]",
        );
    }
    if !stderr.trim().is_empty() {
        result_text.push_str(&format!(
            "\n\n[glob completed with warnings: {}]",
            format_search_warning(stderr.trim())
        ));
    }

    ToolResult::text(result_text)
}

fn ripgrep_available() -> bool {
    *RIPGREP_AVAILABLE.get_or_init(|| probe_ripgrep_command("rg"))
}

fn probe_ripgrep_command(program: &str) -> bool {
    let probe_root = std::env::temp_dir().join(format!("astra-rg-probe-{}", Uuid::new_v4()));
    let probed = (|| {
        std::fs::create_dir_all(&probe_root).ok()?;
        std::fs::write(probe_root.join("probe.txt"), "needle\n").ok()?;

        if !run_ripgrep_probe(
            program,
            &probe_root,
            &["--files", "--hidden", "-g", "*.txt", "."],
            "probe.txt",
        ) {
            return Some(false);
        }

        Some(run_ripgrep_probe(
            program,
            &probe_root,
            &[
                "--line-number",
                "--with-filename",
                "--color",
                "never",
                "--max-columns",
                "500",
                "--max-columns-preview",
                "-e",
                "needle",
                "--",
                "probe.txt",
            ],
            "probe.txt:1:needle",
        ))
    })()
    .unwrap_or(false);

    let _ = std::fs::remove_dir_all(&probe_root);
    probed
}

fn run_ripgrep_probe(program: &str, cwd: &Path, args: &[&str], expected: &str) -> bool {
    StdCommand::new(program)
        .current_dir(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|output| {
            output.status.success() && String::from_utf8_lossy(&output.stdout).contains(expected)
        })
        .unwrap_or(false)
}

fn resolve_existing_search_path(
    workspace_root: &Path,
    requested_path: &str,
) -> Result<PathBuf, String> {
    let resolved = crate::fs_ops::resolve_path(workspace_root, requested_path)?;
    if !resolved.exists() {
        return Err(format!(
            "Error: path '{}' does not exist. Use list_dir to see available files/directories.",
            requested_path
        ));
    }
    Ok(resolved)
}

fn relative_search_target(workspace_root: &Path, resolved: &Path) -> String {
    match resolved.strip_prefix(workspace_root) {
        Ok(relative) if relative.as_os_str().is_empty() => ".".into(),
        Ok(relative) => relative.to_string_lossy().into_owned(),
        Err(_) => resolved.to_string_lossy().into_owned(),
    }
}

fn shell_safe_search_target(target: &str) -> String {
    if target.starts_with('-') {
        format!("./{target}")
    } else {
        target.to_string()
    }
}

fn parse_search_output_mode(args: &Value) -> Result<SearchOutputMode, String> {
    match args
        .get("output_mode")
        .and_then(|value| value.as_str())
        .unwrap_or("content")
    {
        "content" => Ok(SearchOutputMode::Content),
        "files_with_matches" => Ok(SearchOutputMode::FilesWithMatches),
        "count" => Ok(SearchOutputMode::Count),
        other => Err(format!(
            "Error: unsupported output_mode '{other}'. Use 'content', 'files_with_matches', or 'count'."
        )),
    }
}

fn parse_search_sort_mode(args: &Value) -> Result<SearchSortMode, String> {
    match args
        .get("sort_by")
        .and_then(|value| value.as_str())
        .unwrap_or("mtime")
    {
        "mtime" => Ok(SearchSortMode::Mtime),
        "path" => Ok(SearchSortMode::Path),
        other => Err(format!(
            "Error: unsupported sort_by '{other}'. Use 'mtime' or 'path'."
        )),
    }
}

fn parse_search_globs(value: &Value, arg_name: &str) -> Result<Vec<String>, String> {
    match value {
        Value::String(glob) => parse_single_search_glob(glob, arg_name).map(|glob| vec![glob]),
        Value::Array(items) => items
            .iter()
            .map(|item| match item.as_str() {
                Some(glob) => parse_single_search_glob(glob, arg_name),
                None => Err(format!(
                    "Error: '{arg_name}' entries must be strings when provided as an array."
                )),
            })
            .collect(),
        _ => Err(format!(
            "Error: '{arg_name}' must be a string or array of strings."
        )),
    }
}

fn parse_single_search_glob(glob: &str, arg_name: &str) -> Result<String, String> {
    let trimmed = glob.trim();
    if trimmed.is_empty() {
        return Err(format!("Error: '{arg_name}' must not be empty."));
    }
    Ok(trimmed.to_string())
}

fn parse_search_type_globs(value: &Value) -> Result<Vec<String>, String> {
    match value {
        Value::String(name) => Ok(search_type_globs(name)?
            .iter()
            .map(|glob| (*glob).to_string())
            .collect()),
        Value::Array(items) => {
            let mut globs = Vec::new();
            for item in items {
                let Some(name) = item.as_str() else {
                    return Err(
                        "Error: 'type' entries must be strings when provided as an array.".into(),
                    );
                };
                globs.extend(
                    search_type_globs(name)?
                        .iter()
                        .map(|glob| (*glob).to_string()),
                );
            }
            Ok(globs)
        }
        _ => Err("Error: 'type' must be a string or array of strings.".into()),
    }
}

fn search_type_globs(type_name: &str) -> Result<&'static [&'static str], String> {
    let normalized = type_name.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "rust" | "rs" => Ok(&["*.rs"]),
        "python" | "py" => Ok(&["*.py"]),
        "typescript" => Ok(&["*.ts", "*.tsx", "*.mts", "*.cts"]),
        "ts" => Ok(&["*.ts", "*.mts", "*.cts"]),
        "tsx" | "typescriptreact" => Ok(&["*.tsx"]),
        "javascript" => Ok(&["*.js", "*.jsx", "*.mjs", "*.cjs"]),
        "js" => Ok(&["*.js", "*.mjs", "*.cjs"]),
        "jsx" | "javascriptreact" => Ok(&["*.jsx"]),
        "go" => Ok(&["*.go"]),
        "java" => Ok(&["*.java"]),
        "c" => Ok(&["*.c", "*.h"]),
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hxx" | "hh" => {
            Ok(&["*.cpp", "*.cc", "*.cxx", "*.hpp", "*.hxx", "*.hh"])
        }
        "ruby" | "rb" => Ok(&["*.rb"]),
        "shell" | "sh" | "bash" => Ok(&["*.sh", "*.bash"]),
        "json" => Ok(&["*.json"]),
        "yaml" | "yml" => Ok(&["*.yaml", "*.yml"]),
        "toml" => Ok(&["*.toml"]),
        "markdown" | "md" => Ok(&["*.md"]),
        _ => Err(format!(
            "Error: unsupported grep type '{type_name}'. Use a common type like rust, python, typescript, tsx, javascript, jsx, go, java, c, cpp, ruby, shell, json, yaml, toml, or markdown."
        )),
    }
}

fn dedup_preserve_order(values: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn sort_search_paths(paths: &mut [String], workspace_root: &Path, sort_mode: SearchSortMode) {
    match sort_mode {
        SearchSortMode::Path => paths.sort(),
        SearchSortMode::Mtime => {
            let mut cache = std::collections::HashMap::new();
            paths.sort_by(|left, right| {
                search_path_mtime_ms(workspace_root, right, &mut cache)
                    .cmp(&search_path_mtime_ms(workspace_root, left, &mut cache))
                    .then(left.cmp(right))
            });
        }
    }
}

fn search_path_mtime_ms(
    workspace_root: &Path,
    path: &str,
    cache: &mut std::collections::HashMap<String, u128>,
) -> u128 {
    if let Some(value) = cache.get(path) {
        return *value;
    }

    let resolved = workspace_root.join(path);
    let value = resolved
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    cache.insert(path.to_string(), value);
    value
}

fn sort_grep_result_lines(
    lines: &mut Vec<String>,
    workspace_root: &Path,
    output_mode: SearchOutputMode,
    sort_mode: SearchSortMode,
    ignore_rules: &[SearchIgnoreRule],
    gitignored_paths: &std::collections::HashSet<String>,
) {
    if lines.len() < 2 {
        lines.retain(|line| {
            extract_search_result_path(line, output_mode).is_none_or(|path| {
                !should_ignore_search_path(&path, ignore_rules) && !gitignored_paths.contains(&path)
            })
        });
        return;
    }

    let mut groups = group_grep_result_lines(lines, output_mode);
    groups.retain(|group| {
        group.path.as_ref().is_none_or(|path| {
            !should_ignore_search_path(path, ignore_rules) && !gitignored_paths.contains(path)
        })
    });
    let mut cache = std::collections::HashMap::new();
    groups.sort_by(|left, right| match (&left.path, &right.path) {
        (Some(left_path), Some(right_path)) => match sort_mode {
            SearchSortMode::Path => left_path.cmp(right_path),
            SearchSortMode::Mtime => search_path_mtime_ms(workspace_root, right_path, &mut cache)
                .cmp(&search_path_mtime_ms(workspace_root, left_path, &mut cache))
                .then(left_path.cmp(right_path)),
        },
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    *lines = groups
        .into_iter()
        .flat_map(|group| group.lines)
        .collect::<Vec<_>>();
}

fn group_grep_result_lines(
    lines: &[String],
    output_mode: SearchOutputMode,
) -> Vec<SearchResultGroup> {
    let mut groups: Vec<SearchResultGroup> = Vec::new();
    let mut index_by_path = std::collections::HashMap::<String, usize>::new();
    let mut current_group: Option<usize> = None;

    for line in lines {
        if line == "--" {
            if let Some(index) = current_group {
                groups[index].lines.push(line.clone());
            }
            continue;
        }

        let Some(path) = extract_search_result_path(line, output_mode) else {
            groups.push(SearchResultGroup {
                path: None,
                lines: vec![line.clone()],
            });
            current_group = None;
            continue;
        };

        let index = if let Some(index) = index_by_path.get(&path).copied() {
            index
        } else {
            let index = groups.len();
            groups.push(SearchResultGroup {
                path: Some(path.clone()),
                lines: Vec::new(),
            });
            index_by_path.insert(path, index);
            index
        };
        groups[index].lines.push(line.clone());
        current_group = Some(index);
    }

    groups
}

fn extract_search_result_path(line: &str, output_mode: SearchOutputMode) -> Option<String> {
    static CONTENT_MATCH_RE: OnceLock<Regex> = OnceLock::new();
    static CONTENT_CONTEXT_RE: OnceLock<Regex> = OnceLock::new();
    static COUNT_RE: OnceLock<Regex> = OnceLock::new();

    match output_mode {
        SearchOutputMode::FilesWithMatches => Some(line.to_string()),
        SearchOutputMode::Count => COUNT_RE
            .get_or_init(|| Regex::new(r"^(.+?):\d+$").expect("valid count regex"))
            .captures(line)
            .and_then(|captures| captures.get(1))
            .map(|path| path.as_str().to_string()),
        SearchOutputMode::Content => CONTENT_MATCH_RE
            .get_or_init(|| Regex::new(r"^(.+?):\d+:").expect("valid content regex"))
            .captures(line)
            .and_then(|captures| captures.get(1))
            .or_else(|| {
                CONTENT_CONTEXT_RE
                    .get_or_init(|| Regex::new(r"^(.+?)-\d+-").expect("valid context regex"))
                    .captures(line)
                    .and_then(|captures| captures.get(1))
            })
            .map(|path| path.as_str().to_string()),
    }
}

fn normalize_grep_output_line(line: &str, output_mode: SearchOutputMode) -> String {
    if line == "--" {
        return line.to_string();
    }

    match output_mode {
        SearchOutputMode::FilesWithMatches => strip_current_dir_prefix(line),
        SearchOutputMode::Content | SearchOutputMode::Count => {
            line.strip_prefix("./").unwrap_or(line).to_string()
        }
    }
}

fn load_search_ignore_rules(workspace_root: &Path) -> Result<Vec<SearchIgnoreRule>, String> {
    let ignore_path = workspace_root.join(".astraignore");
    if !ignore_path.exists() {
        return Ok(Vec::new());
    }

    let contents = std::fs::read_to_string(&ignore_path)
        .map_err(|e| format!("Error: failed to read {}: {e}", ignore_path.display()))?;
    let mut rules = Vec::new();

    for (index, raw_line) in contents.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        let (negated, pattern) = if let Some(rest) = trimmed.strip_prefix('!') {
            (true, rest)
        } else {
            (false, trimmed)
        };
        let normalized = normalize_search_ignore_pattern(pattern).map_err(|e| {
            format!(
                "Error: invalid .astraignore pattern on line {}: {e}",
                index + 1
            )
        })?;
        rules.push(SearchIgnoreRule {
            pattern: normalized,
            negated,
        });
    }

    Ok(rules)
}

fn normalize_search_ignore_pattern(pattern: &str) -> Result<String, String> {
    let trimmed = pattern.trim().trim_start_matches('/');
    if trimmed.is_empty() {
        return Err("pattern must not be empty".into());
    }
    if contains_path_traversal(trimmed) {
        return Err("pattern must not escape the workspace".into());
    }

    if let Some(dir_pattern) = trimmed.strip_suffix('/') {
        if dir_pattern.is_empty() {
            return Err("pattern must not be empty".into());
        }
        return Ok(if dir_pattern.contains('/') {
            format!("{dir_pattern}/**")
        } else {
            format!("**/{dir_pattern}/**")
        });
    }

    Ok(trimmed.to_string())
}

fn should_ignore_search_path(path: &str, rules: &[SearchIgnoreRule]) -> bool {
    let mut ignored = false;
    for rule in rules {
        if glob_matches_path(&rule.pattern, path) {
            ignored = !rule.negated;
        }
    }
    ignored
}

async fn load_gitignored_search_paths(
    workspace_root: &Path,
    paths: &[String],
) -> Result<std::collections::HashSet<String>, String> {
    let mut candidates = paths
        .iter()
        .filter(|path| !path.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    dedup_preserve_order(&mut candidates);
    if candidates.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    let mut cmd = Command::new("git");
    cmd.current_dir(workspace_root)
        .kill_on_drop(true)
        .arg("check-ignore")
        .arg("--stdin")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(_) => return Ok(std::collections::HashSet::new()),
    };

    if let Some(mut stdin) = child.stdin.take() {
        let payload = format!("{}\n", candidates.join("\n"));
        if let Err(error) = stdin.write_all(payload.as_bytes()).await
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(format!("Error: failed to write gitignore query: {error}"));
        }
    }

    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .map_err(|_| "Error: git check-ignore timed out.".to_string())?
        .map_err(|e| format!("Error: git check-ignore failed: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code == 0 {
        return Ok(stdout.lines().map(strip_current_dir_prefix).collect());
    }
    if exit_code == 1 {
        return Ok(std::collections::HashSet::new());
    }
    if exit_code == 128
        || stderr.contains("not a git repository")
        || stderr.contains("outside repository")
    {
        debug!(
            "git check-ignore returned {exit_code}, assuming no ignore rules: {}",
            stderr.lines().next().unwrap_or("")
        );
        return Ok(std::collections::HashSet::new());
    }

    let detail = stderr.trim();
    Err(if detail.is_empty() {
        "Error: git check-ignore failed: unknown git error".into()
    } else {
        format!("Error: git check-ignore failed: {detail}")
    })
}

fn build_search_regex(request: &GrepRequest<'_>, multiline: bool) -> Result<regex::Regex, String> {
    let mut pattern = if request.fixed_strings {
        regex::escape(request.pattern)
    } else {
        request.pattern.to_string()
    };
    if request.word_match {
        pattern = format!(r"\b(?:{pattern})\b");
    }

    let mut builder = regex::RegexBuilder::new(&pattern);
    builder.case_insensitive(!request.case_sensitive);
    if multiline {
        builder.multi_line(true).dot_matches_new_line(true);
    }
    builder
        .build()
        .map_err(|e| format!("Error: invalid regex: {e}"))
}

async fn run_grep_with_rg_program(
    program: &str,
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let mut cmd = Command::new(program);
    cmd.current_dir(request.workspace_root)
        .kill_on_drop(true)
        .arg("--hidden")
        .arg("--color")
        .arg("never")
        .arg("--max-columns")
        .arg("500")
        .arg("--max-columns-preview");

    match request.output_mode {
        SearchOutputMode::Content => {
            cmd.arg("--line-number").arg("--with-filename");
        }
        SearchOutputMode::FilesWithMatches => {
            cmd.arg("--files-with-matches");
        }
        SearchOutputMode::Count => {
            cmd.arg("--count").arg("--with-filename");
        }
    }

    if !request.case_sensitive {
        cmd.arg("-i");
    }
    if request.fixed_strings {
        cmd.arg("-F");
    }
    if request.word_match {
        cmd.arg("-w");
    }
    append_context_flags(
        &mut cmd,
        request.before_context_lines,
        request.after_context_lines,
    );
    if let Some(max) = request.max_matches {
        cmd.arg("-m").arg(max.to_string());
    }
    for include in &request.include_globs {
        cmd.arg("-g").arg(include);
    }
    append_default_rg_excludes(&mut cmd);
    cmd.arg("-e")
        .arg(request.pattern)
        .arg("--")
        .arg(request.target);

    run_readonly_command_with_partial(
        &mut cmd,
        GREP_TIMEOUT,
        RAW_GREP_OUTPUT_LIMIT,
        RAW_STDERR_OUTPUT_LIMIT,
        cancel_token,
        "search command",
    )
    .await
}

async fn run_grep_with_preferred_backend(
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
    prefer_rg: bool,
) -> Result<ReadOnlyCommandOutput, String> {
    run_grep_with_preferred_backend_program("rg", request, cancel_token, prefer_rg).await
}

async fn run_grep_with_preferred_backend_program(
    program: &str,
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
    prefer_rg: bool,
) -> Result<ReadOnlyCommandOutput, String> {
    if request.multiline {
        return run_grep_multiline_locally(request, cancel_token).await;
    }
    if !prefer_rg {
        return run_grep_with_grep(request, cancel_token).await;
    }

    match run_grep_with_rg_program(program, request, cancel_token).await {
        Ok(output) if should_fallback_from_rg_grep_output(&output) => {
            run_grep_with_grep(request, cancel_token).await
        }
        Ok(output) => Ok(output),
        Err(_) => run_grep_with_grep(request, cancel_token).await,
    }
}

async fn run_grep_with_grep(
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let deadline = tokio::time::Instant::now() + GREP_TIMEOUT;
    let enumerated = enumerate_search_files(request, cancel_token, deadline).await?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut stdout_capped = false;
    let mut exit_code = if enumerated.files.is_empty() {
        if enumerated.timed_out || enumerated.cancelled {
            -1
        } else {
            1
        }
    } else {
        1
    };
    let mut timed_out = enumerated.timed_out;
    let mut cancelled = enumerated.cancelled;
    let mut saw_match_output = false;

    for chunk in enumerated.files.chunks(200) {
        if timed_out || cancelled || stdout_capped {
            break;
        }
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            break;
        }

        let mut cmd = Command::new("grep");
        cmd.current_dir(request.workspace_root).kill_on_drop(true);
        match request.output_mode {
            SearchOutputMode::Content => {
                cmd.arg(if request.fixed_strings { "-nH" } else { "-nHE" });
            }
            SearchOutputMode::FilesWithMatches => {
                cmd.arg(if request.fixed_strings { "-lH" } else { "-lHE" });
            }
            SearchOutputMode::Count => {
                cmd.arg(if request.fixed_strings { "-cH" } else { "-cHE" });
            }
        }
        if !request.case_sensitive {
            cmd.arg("-i");
        }
        if request.fixed_strings {
            cmd.arg("-F");
        }
        if request.word_match {
            cmd.arg("-w");
        }
        append_context_flags(
            &mut cmd,
            request.before_context_lines,
            request.after_context_lines,
        );
        if let Some(max) = request.max_matches {
            cmd.arg(format!("-m{max}"));
        }
        append_default_grep_excludes(&mut cmd);
        cmd.arg("-e").arg(request.pattern).arg("--");
        for file in chunk {
            cmd.arg(file);
        }

        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .unwrap_or_else(|| Duration::from_millis(1));
        let chunk_output = run_readonly_command_with_partial(
            &mut cmd,
            remaining,
            RAW_GREP_OUTPUT_LIMIT,
            RAW_STDERR_OUTPUT_LIMIT,
            cancel_token,
            "search command",
        )
        .await?;

        if !chunk_output.stdout.trim().is_empty() {
            saw_match_output = true;
            exit_code = 0;
            append_output_chunk(
                &mut stdout,
                &chunk_output.stdout,
                RAW_GREP_OUTPUT_LIMIT,
                &mut stdout_capped,
            );
        } else if exit_code == 1 && chunk_output.exit_code > 1 {
            exit_code = chunk_output.exit_code;
        }

        if !chunk_output.stderr.trim().is_empty() {
            if !stderr.is_empty() && !stderr.ends_with('\n') {
                stderr.push('\n');
            }
            stderr.push_str(chunk_output.stderr.trim_end());
        }

        timed_out |= chunk_output.timed_out;
        cancelled |= chunk_output.cancelled;
    }

    if !saw_match_output && (timed_out || cancelled) {
        exit_code = -1;
    }

    Ok(ReadOnlyCommandOutput {
        stdout,
        stderr,
        exit_code,
        timed_out,
        cancelled,
        stdout_capped,
        stderr_capped: false,
    })
}

async fn run_grep_multiline_locally(
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let regex = build_search_regex(request, true)?;

    let deadline = tokio::time::Instant::now() + GREP_TIMEOUT;
    let mut stdout = String::new();
    let mut stdout_capped = false;

    let enumerated = enumerate_search_files(request, cancel_token, deadline).await?;
    let mut timed_out = enumerated.timed_out;
    let mut cancelled = enumerated.cancelled;
    for relative_path in enumerated.files {
        if tokio::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        if cancel_token.is_some_and(CancellationToken::is_cancelled) {
            cancelled = true;
            break;
        }

        let absolute_path = request.workspace_root.join(&relative_path);
        let Ok(contents) = std::fs::read_to_string(&absolute_path) else {
            continue;
        };

        match request.output_mode {
            SearchOutputMode::FilesWithMatches => {
                if regex.is_match(&contents) {
                    append_output_line(
                        &mut stdout,
                        &relative_path,
                        RAW_GREP_OUTPUT_LIMIT,
                        &mut stdout_capped,
                    );
                }
            }
            SearchOutputMode::Count => {
                let count = count_regex_matches(&regex, &contents, request.max_matches);
                if count > 0 {
                    append_output_line(
                        &mut stdout,
                        &format!("{relative_path}:{count}"),
                        RAW_GREP_OUTPUT_LIMIT,
                        &mut stdout_capped,
                    );
                }
            }
            SearchOutputMode::Content => {
                for line in render_multiline_matches(&relative_path, &contents, &regex, request) {
                    append_output_line(
                        &mut stdout,
                        &line,
                        RAW_GREP_OUTPUT_LIMIT,
                        &mut stdout_capped,
                    );
                    if stdout_capped {
                        break;
                    }
                }
            }
        }

        if stdout_capped {
            break;
        }
        tokio::task::yield_now().await;
    }

    let exit_code = if stdout.trim().is_empty() {
        if timed_out || cancelled { -1 } else { 1 }
    } else {
        0
    };

    Ok(ReadOnlyCommandOutput {
        stdout,
        stderr: String::new(),
        exit_code,
        timed_out,
        cancelled,
        stdout_capped,
        stderr_capped: false,
    })
}

async fn enumerate_search_files(
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
    deadline: tokio::time::Instant,
) -> Result<EnumeratedSearchFiles, String> {
    let target_path = request.workspace_root.join(request.target);
    if target_path.is_file() {
        let relative = relative_search_target(request.workspace_root, &target_path);
        let gitignored =
            load_gitignored_search_paths(request.workspace_root, std::slice::from_ref(&relative))
                .await?;
        return Ok(EnumeratedSearchFiles {
            files: if matches_search_file_filters(&relative, &request.include_globs)
                && !gitignored.contains(&relative)
            {
                vec![relative]
            } else {
                Vec::new()
            },
            timed_out: false,
            cancelled: false,
        });
    }

    let listed = run_search_file_listing_with_find(
        request.workspace_root,
        request.target,
        cancel_token,
        GREP_TIMEOUT,
    )
    .await?;
    let mut files = listed
        .stdout
        .lines()
        .map(strip_current_dir_prefix)
        .filter(|line| !line.is_empty())
        .filter(|line| matches_search_file_filters(line, &request.include_globs))
        .filter(|line| !should_ignore_search_path(line, &request.ignore_rules))
        .collect::<Vec<_>>();
    let gitignored = load_gitignored_search_paths(request.workspace_root, &files).await?;
    files.retain(|line| !gitignored.contains(line));
    files.sort();
    files.dedup();

    Ok(EnumeratedSearchFiles {
        files,
        timed_out: tokio::time::Instant::now() >= deadline || listed.timed_out,
        cancelled: cancel_token.is_some_and(CancellationToken::is_cancelled) || listed.cancelled,
    })
}

fn matches_search_file_filters(path: &str, include_globs: &[String]) -> bool {
    include_globs.is_empty()
        || include_globs
            .iter()
            .any(|glob| glob_matches_path(glob, path))
}

fn count_regex_matches(regex: &regex::Regex, contents: &str, max_matches: Option<usize>) -> usize {
    match max_matches {
        Some(limit) => regex.find_iter(contents).take(limit).count(),
        None => regex.find_iter(contents).count(),
    }
}

fn render_multiline_matches(
    relative_path: &str,
    contents: &str,
    regex: &regex::Regex,
    request: &GrepRequest<'_>,
) -> Vec<String> {
    let lines: Vec<&str> = contents.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    let line_starts = build_line_starts(contents);
    let before = request.before_context_lines.unwrap_or(0);
    let after = request.after_context_lines.unwrap_or(0);
    let matches = match request.max_matches {
        Some(limit) => regex.find_iter(contents).take(limit).collect::<Vec<_>>(),
        None => regex.find_iter(contents).collect::<Vec<_>>(),
    };
    if matches.is_empty() {
        return Vec::new();
    }

    let mut blocks: Vec<MultilineMatchBlock> = Vec::new();
    for matched in matches {
        let match_start = line_number_for_offset(&line_starts, matched.start());
        let match_end = line_number_for_offset(&line_starts, matched.end().saturating_sub(1));
        let block_start = match_start.saturating_sub(before).max(1);
        let block_end = (match_end + after).min(lines.len());

        if let Some(block) = blocks.last_mut()
            && block_start <= block.end_line + 1
        {
            block.start_line = block.start_line.min(block_start);
            block.end_line = block.end_line.max(block_end);
            block.match_ranges.push((match_start, match_end));
            continue;
        }
        blocks.push(MultilineMatchBlock {
            start_line: block_start,
            end_line: block_end,
            match_ranges: vec![(match_start, match_end)],
        });
    }

    let mut rendered = Vec::new();
    for (index, block) in blocks.into_iter().enumerate() {
        if index > 0 {
            rendered.push("--".into());
        }
        for line_number in block.start_line..=block.end_line {
            let is_match = block
                .match_ranges
                .iter()
                .any(|(start, end)| line_number >= *start && line_number <= *end);
            let separator = if is_match { ':' } else { '-' };
            let line_text = lines.get(line_number - 1).copied().unwrap_or("");
            rendered.push(format!(
                "{relative_path}{separator}{line_number}{separator}{line_text}"
            ));
        }
    }
    rendered
}

fn build_line_starts(contents: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (index, byte) in contents.bytes().enumerate() {
        if byte == b'\n' && index + 1 < contents.len() {
            starts.push(index + 1);
        }
    }
    starts
}

fn line_number_for_offset(line_starts: &[usize], offset: usize) -> usize {
    match line_starts.binary_search(&offset) {
        Ok(index) => index + 1,
        Err(index) => index,
    }
}

fn append_output_line(output: &mut String, line: &str, max_bytes: usize, capped: &mut bool) {
    if *capped {
        return;
    }
    if !output.is_empty() {
        append_capped(output, "\n", max_bytes, capped);
    }
    append_capped(output, line, max_bytes, capped);
}

fn append_output_chunk(output: &mut String, chunk: &str, max_bytes: usize, capped: &mut bool) {
    if *capped || chunk.is_empty() {
        return;
    }
    if !output.is_empty() && !output.ends_with('\n') {
        append_capped(output, "\n", max_bytes, capped);
    }
    append_capped(output, chunk, max_bytes, capped);
}

async fn run_glob_with_rg_program(
    program: &str,
    workspace_root: &Path,
    target: &str,
    pattern: &str,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let shell_target = shell_safe_search_target(target);
    let mut cmd = Command::new(program);
    cmd.current_dir(workspace_root)
        .kill_on_drop(true)
        .arg("--files")
        .arg("--hidden");
    append_default_rg_excludes(&mut cmd);
    cmd.arg("-g").arg(pattern).arg(shell_target);

    run_readonly_command_with_partial(
        &mut cmd,
        GLOB_TIMEOUT,
        RAW_GLOB_OUTPUT_LIMIT,
        RAW_STDERR_OUTPUT_LIMIT,
        cancel_token,
        "search command",
    )
    .await
}

async fn run_glob_with_preferred_backend(
    workspace_root: &Path,
    target: &str,
    pattern: &str,
    cancel_token: Option<&CancellationToken>,
    prefer_rg: bool,
) -> Result<ReadOnlyCommandOutput, String> {
    run_glob_with_preferred_backend_program(
        "rg",
        workspace_root,
        target,
        pattern,
        cancel_token,
        prefer_rg,
    )
    .await
}

async fn run_glob_with_preferred_backend_program(
    program: &str,
    workspace_root: &Path,
    target: &str,
    pattern: &str,
    cancel_token: Option<&CancellationToken>,
    prefer_rg: bool,
) -> Result<ReadOnlyCommandOutput, String> {
    if !prefer_rg {
        return run_glob_with_find(workspace_root, target, cancel_token).await;
    }

    match run_glob_with_rg_program(program, workspace_root, target, pattern, cancel_token).await {
        Ok(output) if output.exit_code > 1 && !output.timed_out && !output.cancelled => {
            run_glob_with_find(workspace_root, target, cancel_token).await
        }
        Ok(output) => Ok(output),
        Err(_) => run_glob_with_find(workspace_root, target, cancel_token).await,
    }
}

async fn run_glob_with_find(
    workspace_root: &Path,
    target: &str,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    run_search_file_listing_with_find(workspace_root, target, cancel_token, GLOB_TIMEOUT).await
}

async fn run_search_file_listing_with_find(
    workspace_root: &Path,
    target: &str,
    cancel_token: Option<&CancellationToken>,
    timeout: Duration,
) -> Result<ReadOnlyCommandOutput, String> {
    let shell_target = shell_safe_search_target(target);
    let mut cmd = Command::new("find");
    cmd.current_dir(workspace_root).kill_on_drop(true);
    cmd.arg(shell_target)
        .arg("(")
        .arg("-type")
        .arg("d")
        .arg("(");
    for (index, dir) in DEFAULT_SEARCH_EXCLUDE_DIRS.iter().enumerate() {
        if index > 0 {
            cmd.arg("-o");
        }
        cmd.arg("-name").arg(dir);
    }
    cmd.arg(")")
        .arg("-prune")
        .arg(")")
        .arg("-o")
        .arg("-type")
        .arg("f")
        .arg("-print");

    run_readonly_command_with_partial(
        &mut cmd,
        timeout,
        RAW_GLOB_OUTPUT_LIMIT,
        RAW_STDERR_OUTPUT_LIMIT,
        cancel_token,
        "search command",
    )
    .await
}

fn append_default_rg_excludes(cmd: &mut Command) {
    for dir in DEFAULT_SEARCH_EXCLUDE_DIRS {
        cmd.arg("-g").arg(format!("!{dir}/**"));
    }
}

fn append_default_grep_excludes(cmd: &mut Command) {
    cmd.arg("--binary-files=without-match");
    cmd.arg("--devices=skip");
    for dir in DEFAULT_SEARCH_EXCLUDE_DIRS {
        cmd.arg("--exclude-dir").arg(dir);
    }
}

fn should_fallback_from_rg_grep_output(output: &ReadOnlyCommandOutput) -> bool {
    output.exit_code > 1
        && !output.timed_out
        && !output.cancelled
        && !looks_like_rg_regex_error(&output.stderr)
}

fn looks_like_rg_regex_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("regex parse error")
        || lower.contains("error parsing regex")
        || lower.contains("pcre2")
}

fn format_search_warning(stderr: &str) -> String {
    let compact = stderr
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    truncate_output(compact, 500)
}

fn no_more_results_message(
    offset: usize,
    captured: usize,
    unit: &str,
    timed_out: bool,
    cancelled: bool,
    stdout_capped: bool,
) -> String {
    let mut caveats = Vec::new();
    if cancelled {
        caveats.push("search was cancelled before completion");
    }
    if timed_out {
        caveats.push("search timed out before completion");
    }
    if stdout_capped {
        caveats.push("backend output was capped before pagination");
    }

    if caveats.is_empty() {
        format!("No more results (offset {offset} >= {captured} {unit})")
    } else {
        format!(
            "No more captured results (offset {offset} >= {captured} {unit}). Note: {} so additional results may exist.",
            caveats.join(", ")
        )
    }
}

fn no_visible_results_message(
    subject: &str,
    timed_out: bool,
    cancelled: bool,
    stdout_capped: bool,
    stderr: &str,
) -> String {
    let mut caveats = Vec::new();
    if cancelled {
        caveats.push("search was cancelled before completion");
    }
    if timed_out {
        caveats.push("search timed out before completion");
    }
    if stdout_capped {
        caveats.push("backend output was capped before post-filtering");
    }

    if caveats.is_empty() {
        if stderr.is_empty() {
            format!("No {subject} found")
        } else {
            format!(
                "No {subject} found (warnings: {})",
                format_search_warning(stderr)
            )
        }
    } else if stderr.is_empty() {
        format!(
            "No visible {subject} found in captured results. Note: {} so additional results may exist.",
            caveats.join(", ")
        )
    } else {
        format!(
            "No visible {subject} found in captured results. Note: {} so additional results may exist. Warnings: {}",
            caveats.join(", "),
            format_search_warning(stderr)
        )
    }
}

fn append_context_flags(cmd: &mut Command, before: Option<usize>, after: Option<usize>) {
    match (before, after) {
        (Some(b), Some(a)) if b == a && b > 0 => {
            cmd.arg("-C").arg(b.to_string());
        }
        (Some(b), Some(a)) => {
            if b > 0 {
                cmd.arg("-B").arg(b.to_string());
            }
            if a > 0 {
                cmd.arg("-A").arg(a.to_string());
            }
        }
        (Some(b), None) if b > 0 => {
            cmd.arg("-B").arg(b.to_string());
        }
        (None, Some(a)) if a > 0 => {
            cmd.arg("-A").arg(a.to_string());
        }
        _ => {}
    }
}

async fn run_readonly_command_with_partial(
    cmd: &mut Command,
    timeout: Duration,
    max_stdout_bytes: usize,
    max_stderr_bytes: usize,
    cancel_token: Option<&CancellationToken>,
    command_kind: &str,
) -> Result<ReadOnlyCommandOutput, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Error: failed to start {command_kind}: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("Error: failed to capture {command_kind} stdout"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("Error: failed to capture {command_kind} stderr"))?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(StreamKind, String)>();
    let stdout_task = tokio::spawn(read_stream(stdout, StreamKind::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_stream(stderr, StreamKind::Stderr, tx));

    let deadline = tokio::time::Instant::now() + timeout;
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut stdout_capped = false;
    let mut stderr_capped = false;
    let mut exit_code = None;
    let mut timed_out = false;
    let mut cancelled = false;

    loop {
        drain_command_chunks(
            &mut rx,
            &mut stdout_text,
            &mut stderr_text,
            max_stdout_bytes,
            &mut stdout_capped,
            max_stderr_bytes,
            &mut stderr_capped,
        );

        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = Some(status.code().unwrap_or(-1));
                break;
            }
            Ok(None) => {
                if tokio::time::Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    break;
                }
                if let Some(token) = cancel_token {
                    tokio::select! {
                        _ = token.cancelled() => {
                            cancelled = true;
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            break;
                        }
                        _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                    }
                } else {
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
            }
            Err(e) => return Err(format!("Error: {command_kind} failed: {e}")),
        }
    }

    let _ = stdout_task.await;
    let _ = stderr_task.await;
    drain_command_chunks(
        &mut rx,
        &mut stdout_text,
        &mut stderr_text,
        max_stdout_bytes,
        &mut stdout_capped,
        max_stderr_bytes,
        &mut stderr_capped,
    );

    if timed_out || cancelled {
        truncate_partial_line(&mut stdout_text);
        truncate_partial_line(&mut stderr_text);
    }

    Ok(ReadOnlyCommandOutput {
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code: exit_code.unwrap_or(-1),
        timed_out,
        cancelled,
        stdout_capped,
        stderr_capped,
    })
}

async fn read_stream<R>(
    mut stream: R,
    kind: StreamKind,
    tx: tokio::sync::mpsc::UnboundedSender<(StreamKind, String)>,
) where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut buffer = [0u8; 8192];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) => break,
            Ok(read) => {
                let text = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let _ = tx.send((kind, text));
            }
            Err(_) => break,
        }
    }
}

fn drain_command_chunks(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(StreamKind, String)>,
    stdout_text: &mut String,
    stderr_text: &mut String,
    max_stdout_bytes: usize,
    stdout_capped: &mut bool,
    max_stderr_bytes: usize,
    stderr_capped: &mut bool,
) {
    while let Ok((kind, chunk)) = rx.try_recv() {
        match kind {
            StreamKind::Stdout => {
                append_capped(stdout_text, &chunk, max_stdout_bytes, stdout_capped)
            }
            StreamKind::Stderr => {
                append_capped(stderr_text, &chunk, max_stderr_bytes, stderr_capped)
            }
        }
    }
}

fn truncate_partial_line(output: &mut String) {
    if let Some(last_newline) = output.rfind('\n') {
        output.truncate(last_newline);
    } else if !output.is_empty() {
        output.clear();
    }
}

fn append_capped(output: &mut String, chunk: &str, max_bytes: usize, capped: &mut bool) {
    if *capped {
        return;
    }
    if output.len() + chunk.len() > max_bytes {
        let remaining = max_bytes.saturating_sub(output.len());
        let safe = chunk.floor_char_boundary(remaining);
        output.push_str(&chunk[..safe]);
        *capped = true;
        return;
    }
    output.push_str(chunk);
}

fn annotate_grep_with_scope(grep_output: &str, workspace_root: &Path) -> String {
    use std::collections::HashMap;

    let mut file_cache: HashMap<String, Option<(String, crate::code_intel::Language)>> =
        HashMap::new();
    let mut result = String::with_capacity(grep_output.len() + grep_output.len() / 10);

    for line in grep_output.lines() {
        if let Some((file_part, rest)) = line.split_once(':')
            && let Some((line_number, _content)) = rest.split_once(':')
            && let Ok(line_number) = line_number.trim().parse::<usize>()
        {
            let file_path = if Path::new(file_part).is_absolute() {
                file_part.to_string()
            } else {
                workspace_root.join(file_part).to_string_lossy().to_string()
            };

            let cached = file_cache.entry(file_path.clone()).or_insert_with(|| {
                let path = Path::new(&file_path);
                let language = crate::code_intel::detect_language(path)?;
                let source = std::fs::read_to_string(path).ok()?;
                Some((source, language))
            });

            if let Some((source, language)) = cached {
                let scope = crate::code_intel::scope_at_line(source, *language, line_number);
                let scope_name = if scope.breadcrumbs.len() > 1 {
                    scope.breadcrumbs.join(" > ")
                } else if let Some(symbol) = scope.symbol {
                    symbol.name
                } else {
                    String::new()
                };
                if !scope_name.is_empty() {
                    result.push_str(line);
                    result.push_str("  // in ");
                    result.push_str(&scope_name);
                    result.push('\n');
                    continue;
                }
            }
        }

        result.push_str(line);
        result.push('\n');
    }

    if result.ends_with('\n') {
        result.pop();
    }

    result
}

fn contains_path_traversal(pattern: &str) -> bool {
    pattern.starts_with('/')
        || pattern.contains("~/")
        || pattern.split(['/', '\\']).any(|part| part == "..")
}

fn strip_current_dir_prefix(line: &str) -> String {
    line.strip_prefix("./").unwrap_or(line).to_string()
}

fn glob_matches_path(pattern: &str, path: &str) -> bool {
    let normalized_path = path.replace('\\', "/");
    let candidate = if pattern.contains('/') || pattern.contains('\\') {
        normalized_path
    } else {
        Path::new(&normalized_path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&normalized_path)
            .to_string()
    };

    match glob_pattern_regex(pattern) {
        Ok(regex) => regex.is_match(&candidate),
        Err(_) => false,
    }
}

fn glob_pattern_regex(pattern: &str) -> Result<Regex, String> {
    let body = glob_pattern_fragment(pattern)?;
    Regex::new(&format!("^{body}$")).map_err(|e| format!("Error: invalid glob pattern: {e}"))
}

fn glob_pattern_fragment(pattern: &str) -> Result<String, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut index = 0usize;
    let mut regex = String::new();

    while index < chars.len() {
        match chars[index] {
            '*' if chars.get(index + 1) == Some(&'*') => {
                if chars.get(index + 2) == Some(&'/') {
                    regex.push_str("(?:.*/)?");
                    index += 3;
                } else {
                    regex.push_str(".*");
                    index += 2;
                }
            }
            '*' => {
                regex.push_str("[^/]*");
                index += 1;
            }
            '?' => {
                regex.push_str("[^/]");
                index += 1;
            }
            '{' => {
                let mut depth = 1usize;
                let mut cursor = index + 1;
                let mut part = String::new();
                let mut parts = Vec::new();
                while cursor < chars.len() {
                    match chars[cursor] {
                        '{' => {
                            depth += 1;
                            part.push('{');
                        }
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                            part.push('}');
                        }
                        ',' if depth == 1 => {
                            parts.push(std::mem::take(&mut part));
                        }
                        other => part.push(other),
                    }
                    cursor += 1;
                }
                if depth != 0 {
                    return Err("Error: invalid glob pattern: unclosed '{'".into());
                }
                parts.push(part);
                regex.push('(');
                for (idx, part) in parts.iter().enumerate() {
                    if idx > 0 {
                        regex.push('|');
                    }
                    regex.push_str(&glob_pattern_fragment(part)?);
                }
                regex.push(')');
                index = cursor + 1;
            }
            other
                if matches!(
                    other,
                    '.' | '+' | '(' | ')' | '^' | '$' | '|' | '[' | ']' | '\\'
                ) =>
            {
                regex.push('\\');
                regex.push(other);
                index += 1;
            }
            other => {
                regex.push(other);
                index += 1;
            }
        }
    }

    Ok(regex)
}

#[cfg(test)]
mod tests {
    use serial_test::serial;
    use tempfile::tempdir;

    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;

    #[cfg(unix)]
    fn write_fake_rg_script(dir: &Path, body: &str) -> PathBuf {
        let script = dir.join("fake-rg");
        std::fs::write(&script, format!("#!/usr/bin/env bash\nset -eu\n{body}\n")).unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).unwrap();
        script
    }

    #[tokio::test]
    async fn grep_default_head_limit_applies() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        let content: String = (0..150).map(|i| format!("needle line {i}\n")).collect();
        std::fs::write(dir.path().join("big.txt"), content).unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": "."
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        let match_lines: Vec<&str> = result
            .output
            .lines()
            .filter(|line| line.contains("needle line"))
            .collect();
        assert_eq!(match_lines.len(), GREP_DEFAULT_HEAD_LIMIT);
        assert!(result.output.contains("Results limited to 100"));
    }

    #[tokio::test]
    async fn grep_count_mode_filters_zeroes_and_honors_head_limit() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a.txt"), "needle\nneedle\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "needle\n").unwrap();
        std::fs::write(dir.path().join("c.txt"), "nothing\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "count",
                "head_limit": 1
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        let count_lines: Vec<&str> = result
            .output
            .lines()
            .filter(|line| line.contains(".txt:"))
            .collect();
        assert_eq!(count_lines.len(), 1, "unexpected output: {}", result.output);
        assert!(!result.output.contains("c.txt:0"));
    }

    #[tokio::test]
    async fn grep_count_mode_all_zeroes_returns_no_matches_message() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
        std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "count"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert_eq!(result.output, "No matches found");
    }

    #[tokio::test]
    async fn grep_scope_context_adds_symbol_annotation() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(
            dir.path().join("sample.rs"),
            "fn alpha() {\n    let needle = 1;\n}\n",
        )
        .unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "scope_context": true
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(
            result.output.contains("// in alpha"),
            "expected scope annotation, got: {}",
            result.output
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn ripgrep_probe_accepts_supported_backend() {
        let bin_dir = tempdir().unwrap();
        let fake_rg = write_fake_rg_script(
            bin_dir.path(),
            r#"
for arg in "$@"; do
  if [ "$arg" = "--files" ]; then
    printf 'probe.txt\n'
    exit 0
  fi
done
printf 'probe.txt:1:needle\n'
"#,
        );

        assert!(
            probe_ripgrep_command(fake_rg.to_str().unwrap()),
            "expected fake rg backend probe to succeed"
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn ripgrep_probe_rejects_broken_backend() {
        let bin_dir = tempdir().unwrap();
        let fake_rg = write_fake_rg_script(
            bin_dir.path(),
            "echo 'simulated rg backend failure' >&2\nexit 2",
        );

        assert!(
            !probe_ripgrep_command(fake_rg.to_str().unwrap()),
            "expected fake rg backend probe to fail"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn grep_falls_back_when_rg_backend_errors() {
        let bin_dir = tempdir().unwrap();
        let fake_rg = write_fake_rg_script(
            bin_dir.path(),
            "echo 'simulated rg backend failure' >&2\nexit 2",
        );
        let workspace = tempdir().unwrap();
        std::fs::write(workspace.path().join("sample.txt"), "needle\n").unwrap();
        let request = GrepRequest {
            workspace_root: workspace.path(),
            target: ".",
            pattern: "needle",
            include_globs: Vec::new(),
            ignore_rules: Vec::new(),
            case_sensitive: false,
            fixed_strings: false,
            word_match: false,
            before_context_lines: None,
            after_context_lines: None,
            max_matches: None,
            output_mode: SearchOutputMode::Content,
            multiline: false,
        };

        let output = run_grep_with_preferred_backend_program(
            fake_rg.to_str().unwrap(),
            &request,
            None,
            true,
        )
        .await
        .expect("grep fallback should succeed");

        assert_eq!(
            output.exit_code, 0,
            "stdout={}, stderr={}",
            output.stdout, output.stderr
        );
        assert!(
            output.stdout.contains("sample.txt:1:needle"),
            "expected fallback grep output, got: {}",
            output.stdout
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn glob_falls_back_when_rg_backend_errors() {
        let bin_dir = tempdir().unwrap();
        let fake_rg = write_fake_rg_script(
            bin_dir.path(),
            "echo 'simulated rg backend failure' >&2\nexit 2",
        );
        let workspace = tempdir().unwrap();
        std::fs::write(workspace.path().join("sample.txt"), "").unwrap();

        let output = run_glob_with_preferred_backend_program(
            fake_rg.to_str().unwrap(),
            workspace.path(),
            ".",
            "*.txt",
            None,
            true,
        )
        .await
        .expect("glob fallback should succeed");

        assert_eq!(
            output.exit_code, 0,
            "stdout={}, stderr={}",
            output.stdout, output.stderr
        );
        assert!(
            output.stdout.contains("sample.txt"),
            "expected fallback glob output, got: {}",
            output.stdout
        );
    }

    #[tokio::test]
    async fn grep_glob_alias_filters_files() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("keep.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(dir.path().join("skip.py"), "needle = 1\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "glob": "*.rs"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(result.output.contains("keep.rs"), "got: {}", result.output);
        assert!(
            !result.output.contains("skip.py"),
            "glob alias should filter non-matching files: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_type_filter_limits_results() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("main.rs"), "let needle = 1;\n").unwrap();
        std::fs::write(dir.path().join("main.py"), "needle = 1\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "type": "python"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(result.output.contains("main.py"), "got: {}", result.output);
        assert!(
            !result.output.contains("main.rs"),
            "type filter should exclude other languages: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_fixed_strings_treats_metacharacters_literally() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("sample.txt"), "foo.bar\nfooXbar\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "foo.bar",
                "path": ".",
                "fixed_strings": true
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(
            result.output.contains("sample.txt:1:foo.bar"),
            "expected literal match, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("sample.txt:2:fooXbar"),
            "regex metacharacters should be treated literally: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_word_match_avoids_partial_identifier_hits() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("sample.txt"), "needle\nneedleish\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "word_match": true
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(
            result.output.contains("sample.txt:1:needle"),
            "expected whole-word match, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("sample.txt:2:needleish"),
            "whole-word search should skip partial identifiers: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_rejects_unknown_type_filter() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "type": "elixir"
            }),
        )
        .await;

        assert!(result.is_error, "unexpected success: {}", result.output);
        assert!(
            result.output.contains("unsupported grep type"),
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_multiline_matches_across_lines() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(
            dir.path().join("sample.txt"),
            "alpha\nneedle\nbridge\nomega\n",
        )
        .unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle\\nbridge",
                "path": ".",
                "multiline": true
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(
            result.output.contains("sample.txt:2:needle"),
            "expected first multiline line, got: {}",
            result.output
        );
        assert!(
            result.output.contains("sample.txt:3:bridge"),
            "expected second multiline line, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_before_and_after_context_lines_are_independent() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(
            dir.path().join("sample.txt"),
            "zero\none\nneedle\ntwo\nthree\nfour\n",
        )
        .unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "before_context_lines": 1,
                "after_context_lines": 2
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(
            result.output.contains("sample.txt-2-one"),
            "expected one line of leading context, got: {}",
            result.output
        );
        assert!(
            result.output.contains("sample.txt:3:needle"),
            "expected match line, got: {}",
            result.output
        );
        assert!(
            result.output.contains("sample.txt-4-two"),
            "expected first trailing context line, got: {}",
            result.output
        );
        assert!(
            result.output.contains("sample.txt-5-three"),
            "expected second trailing context line, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("sample.txt-1-zero"),
            "should not include extra leading context: {}",
            result.output
        );
        assert!(
            !result.output.contains("sample.txt-6-four"),
            "should not include extra trailing context: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_files_with_matches_defaults_to_newest_files_first() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a-old.rs"), "needle\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("z-new.rs"), "needle\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "files_with_matches"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(
            lines,
            vec!["z-new.rs", "a-old.rs"],
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_sort_by_path_overrides_newest_first() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a-old.rs"), "needle\n").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("z-new.rs"), "needle\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "files_with_matches",
                "sort_by": "path"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        let lines: Vec<&str> = result.output.lines().collect();
        assert_eq!(
            lines,
            vec!["a-old.rs", "z-new.rs"],
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_path_starting_with_dash_is_searched_safely() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::create_dir_all(dir.path().join("-src")).unwrap();
        std::fs::write(dir.path().join("-src").join("sample.txt"), "needle\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": "-src"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert!(
            result.output.contains("-src/sample.txt:1:needle"),
            "grep should handle directories beginning with '-': {}",
            result.output
        );
    }

    #[tokio::test]
    async fn grep_respects_astraignore_patterns() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join(".astraignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("shown.rs"), "needle\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "files_with_matches",
                "sort_by": "path"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert_eq!(result.output.lines().collect::<Vec<_>>(), vec!["shown.rs"]);
    }

    #[tokio::test]
    async fn grep_astraignore_negation_reincludes_matching_path() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join(".astraignore"), "*.rs\n!shown.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("shown.rs"), "needle\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "files_with_matches",
                "sort_by": "path"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert_eq!(result.output.lines().collect::<Vec<_>>(), vec!["shown.rs"]);
    }

    #[tokio::test]
    async fn grep_respects_gitignore_patterns() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        assert!(
            StdCommand::new("git")
                .current_dir(dir.path())
                .arg("init")
                .arg("-q")
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();
        std::fs::write(dir.path().join("ignored.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("shown.rs"), "needle\n").unwrap();

        let result = grep(
            &ctx,
            &serde_json::json!({
                "pattern": "needle",
                "path": ".",
                "output_mode": "files_with_matches",
                "sort_by": "path"
            }),
        )
        .await;

        assert!(!result.is_error, "grep should succeed: {}", result.output);
        assert_eq!(result.output.lines().collect::<Vec<_>>(), vec!["shown.rs"]);
    }

    #[tokio::test]
    async fn glob_skips_default_generated_directories() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::create_dir_all(dir.path().join("target")).unwrap();
        std::fs::write(dir.path().join("src").join("main.rs"), "").unwrap();
        std::fs::write(dir.path().join("target").join("cached.rs"), "").unwrap();

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.rs",
                "path": "."
            }),
        )
        .await;

        assert!(!result.is_error, "glob should succeed: {}", result.output);
        assert!(
            result.output.contains("src/main.rs"),
            "got: {}",
            result.output
        );
        assert!(
            !result.output.contains("target/cached.rs"),
            "default glob should skip generated dirs: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn glob_sort_by_path_overrides_newest_first() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a-old.txt"), "").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.path().join("z-new.txt"), "").unwrap();

        let default_result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.txt",
                "path": "."
            }),
        )
        .await;
        assert!(
            !default_result.is_error,
            "glob should succeed: {}",
            default_result.output
        );
        let default_lines: Vec<&str> = default_result
            .output
            .lines()
            .filter(|line| !line.starts_with('('))
            .collect();
        assert_eq!(
            default_lines,
            vec!["z-new.txt", "a-old.txt"],
            "got: {}",
            default_result.output
        );

        let path_result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.txt",
                "path": ".",
                "sort_by": "path"
            }),
        )
        .await;
        assert!(
            !path_result.is_error,
            "glob should succeed: {}",
            path_result.output
        );
        let path_lines: Vec<&str> = path_result
            .output
            .lines()
            .filter(|line| !line.starts_with('('))
            .collect();
        assert_eq!(
            path_lines,
            vec!["a-old.txt", "z-new.txt"],
            "got: {}",
            path_result.output
        );
    }

    #[tokio::test]
    async fn glob_supports_offset_and_head_limit_pagination() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("c.txt"), "").unwrap();

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.txt",
                "path": ".",
                "sort_by": "path",
                "offset": 1,
                "head_limit": 1
            }),
        )
        .await;

        assert!(!result.is_error, "glob should succeed: {}", result.output);
        let lines: Vec<&str> = result
            .output
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('(') && !line.starts_with('['))
            .collect();
        assert_eq!(lines, vec!["b.txt"], "got: {}", result.output);
        assert!(
            result.output.contains("(showing 1 of 3 files)"),
            "expected pagination summary, got: {}",
            result.output
        );
        assert!(
            result.output.contains("Results limited to 1 files"),
            "expected pagination limit note, got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn glob_offset_beyond_end_reports_no_more_results() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::write(dir.path().join("a.txt"), "").unwrap();

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.txt",
                "path": ".",
                "offset": 5
            }),
        )
        .await;

        assert!(!result.is_error, "glob should succeed: {}", result.output);
        assert_eq!(result.output, "No more results (offset 5 >= 1 files)");
    }

    #[tokio::test]
    async fn glob_path_starting_with_dash_is_listed_safely() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::create_dir_all(dir.path().join("-src")).unwrap();
        std::fs::write(dir.path().join("-src").join("sample.txt"), "needle\n").unwrap();

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.txt",
                "path": "-src"
            }),
        )
        .await;

        assert!(!result.is_error, "glob should succeed: {}", result.output);
        assert!(
            result.output.contains("-src/sample.txt"),
            "glob should handle directories beginning with '-': {}",
            result.output
        );
    }

    #[tokio::test]
    async fn glob_respects_astraignore_patterns() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        std::fs::create_dir_all(dir.path().join("dist")).unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join(".astraignore"), "dist/\n").unwrap();
        std::fs::write(dir.path().join("dist").join("bundle.js"), "").unwrap();
        std::fs::write(dir.path().join("src").join("app.js"), "").unwrap();

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.js",
                "path": ".",
                "sort_by": "path"
            }),
        )
        .await;

        assert!(!result.is_error, "glob should succeed: {}", result.output);
        assert!(
            result.output.contains("src/app.js"),
            "expected non-ignored file, got: {}",
            result.output
        );
        assert!(
            !result.output.contains("dist/bundle.js"),
            "ignored file should be filtered out: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn glob_respects_gitignore_patterns() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());
        assert!(
            StdCommand::new("git")
                .current_dir(dir.path())
                .arg("init")
                .arg("-q")
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(dir.path().join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(dir.path().join("ignored.txt"), "").unwrap();
        std::fs::write(dir.path().join("shown.txt"), "").unwrap();

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "*.txt",
                "path": ".",
                "sort_by": "path"
            }),
        )
        .await;

        assert!(!result.is_error, "glob should succeed: {}", result.output);
        assert!(
            result.output.contains("shown.txt"),
            "got: {}",
            result.output
        );
        assert!(
            !result.output.contains("ignored.txt"),
            "gitignored file should be filtered out: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn glob_rejects_path_traversal_patterns() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());

        let result = glob(
            &ctx,
            &serde_json::json!({
                "pattern": "../../etc/*"
            }),
        )
        .await;

        assert!(result.is_error);
        assert!(
            result.output.contains("path traversal"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn validate_execute_bash_rejects_empty_command() {
        assert!(validate_execute_bash_command("").is_err());
        assert!(validate_execute_bash_command("  \t").is_err());
    }

    #[test]
    fn validate_execute_bash_rejects_destructive_rm_rf() {
        assert!(validate_execute_bash_command("rm -rf ./build").is_err());
    }

    #[test]
    fn validate_execute_bash_rejects_rm_recursive_even_if_substring_order_differs() {
        assert!(validate_execute_bash_command("rm -r ./build").is_err());
        assert!(validate_execute_bash_command("rm ./build").is_err());
    }

    #[test]
    fn validate_execute_bash_rejects_rmdir() {
        assert!(validate_execute_bash_command("rmdir empty_dir").is_err());
        assert!(validate_execute_bash_command("true && rmdir x").is_err());
    }

    #[test]
    fn validate_execute_bash_rejects_eval_and_process_substitution_when_ast_flags_them() {
        assert!(validate_execute_bash_command("eval echo hi").is_err());
        // Process substitution is parsed as a distinct risk when the shell AST recognizes it.
        assert!(validate_execute_bash_command("cat <(echo 1)").is_err());
    }

    #[test]
    fn validate_execute_bash_rejects_curl_piped_to_shell() {
        assert!(validate_execute_bash_command("curl -s https://x | bash").is_err());
    }

    #[test]
    fn validate_execute_bash_allows_typical_build_commands() {
        assert!(validate_execute_bash_command("cargo test -p foo --quiet").is_ok());
        assert!(validate_execute_bash_command("echo hello && ls").is_ok());
    }

    #[tokio::test]
    async fn bash_non_zero_exit_is_reported_as_error() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());

        let result = execute_bash(
            &ctx,
            &serde_json::json!({
                "command": "echo nope >&2; exit 7"
            }),
        )
        .await;

        assert!(result.is_error, "non-zero bash should be error");
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
    async fn bash_timeout_keeps_partial_output() {
        let dir = tempdir().unwrap();
        let ctx = crate::ToolContext::test(dir.path());

        let result = execute_bash(
            &ctx,
            &serde_json::json!({
                "command": "echo start; sleep 1; echo done",
                "timeout": 0.2
            }),
        )
        .await;

        assert!(result.is_error, "timed out bash should be error");
        assert!(result.output.contains("start"), "got: {}", result.output);
        assert!(
            result.output.contains("timed out after 0.2s"),
            "got: {}",
            result.output
        );
        assert!(!result.output.contains("done"), "got: {}", result.output);
    }

    #[tokio::test]
    async fn bash_cancellation_keeps_partial_output() {
        let dir = tempdir().unwrap();
        let token = Arc::new(CancellationToken::new());
        let trigger = token.clone();

        // Use a sentinel file so we cancel only after output has started,
        // eliminating the race condition with a fixed timer.
        let sentinel = dir.path().join(".cancel_sentinel");
        let sentinel_path = sentinel.clone();
        tokio::spawn(async move {
            // Wait until the bash command has produced output and created the sentinel
            for _ in 0..200 {
                if sentinel_path.exists() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            trigger.cancel();
        });

        let mut ctx = crate::ToolContext::test(dir.path());
        ctx.cancel_token = Some(token);

        // The command creates a sentinel file after the first echo, then sleeps.
        let sentinel_str = sentinel.display();
        let result = execute_bash(
            &ctx,
            &serde_json::json!({
                "command": format!(
                    "echo line_1; touch {sentinel_str}; sleep 0.1; echo line_2; sleep 10"
                )
            }),
        )
        .await;

        assert!(result.is_error, "cancelled bash should be error");
        assert!(result.output.contains("line_1"), "got: {}", result.output);
        assert!(
            result.output.contains("bash cancelled"),
            "got: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn readonly_command_cancels_and_keeps_partial_output() {
        let token = CancellationToken::new();
        let trigger = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(250)).await;
            trigger.cancel();
        });

        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg("for i in $(seq 1 5); do echo line_$i; sleep 0.1; done; sleep 5");

        let output = run_readonly_command_with_partial(
            &mut cmd,
            Duration::from_secs(10),
            RAW_GREP_OUTPUT_LIMIT,
            RAW_STDERR_OUTPUT_LIMIT,
            Some(&token),
            "test command",
        )
        .await
        .expect("command should return partial output on cancellation");

        assert!(output.cancelled, "expected cancelled output");
        assert!(!output.timed_out, "cancellation should not report timeout");
        assert!(
            output.stdout.contains("line_1"),
            "expected partial stdout, got: {}",
            output.stdout
        );
    }

    #[tokio::test]
    async fn readonly_command_marks_stdout_cap() {
        let mut cmd = Command::new("bash");
        cmd.arg("-c")
            .arg("for i in $(seq 1 256); do printf 'xxxxxxxx'; done");

        let output = run_readonly_command_with_partial(
            &mut cmd,
            Duration::from_secs(5),
            128,
            64,
            None,
            "test command",
        )
        .await
        .expect("command should succeed");

        assert_eq!(output.exit_code, 0, "stderr: {}", output.stderr);
        assert!(output.stdout_capped, "expected stdout cap to be reported");
        assert!(output.stdout.len() <= 128, "stdout should be capped");
        assert!(!output.timed_out, "cap should not look like timeout");
        assert!(!output.cancelled, "cap should not look like cancellation");
    }

    #[test]
    fn no_more_results_message_stays_simple_when_search_completed() {
        assert_eq!(
            no_more_results_message(5, 1, "files", false, false, false),
            "No more results (offset 5 >= 1 files)"
        );
    }

    #[test]
    fn no_more_results_message_mentions_incomplete_capture() {
        let message = no_more_results_message(5, 1, "lines", true, false, true);
        assert!(
            message.contains("No more captured results"),
            "expected captured-results wording, got: {message}"
        );
        assert!(
            message.contains("search timed out before completion"),
            "expected timeout caveat, got: {message}"
        );
        assert!(
            message.contains("backend output was capped before pagination"),
            "expected cap caveat, got: {message}"
        );
        assert!(
            message.contains("additional results may exist"),
            "expected uncertainty note, got: {message}"
        );
    }

    #[test]
    fn no_visible_results_message_stays_simple_when_search_completed() {
        assert_eq!(
            no_visible_results_message("matches", false, false, false, ""),
            "No matches found"
        );
        assert_eq!(
            no_visible_results_message("files", false, false, false, "warn"),
            "No files found (warnings: warn)"
        );
    }

    #[test]
    fn no_visible_results_message_mentions_incomplete_capture() {
        let message = no_visible_results_message("matches", true, false, true, "disk warning");
        assert!(
            message.contains("No visible matches found in captured results"),
            "expected visible-results wording, got: {message}"
        );
        assert!(
            message.contains("search timed out before completion"),
            "expected timeout caveat, got: {message}"
        );
        assert!(
            message.contains("backend output was capped before post-filtering"),
            "expected cap caveat, got: {message}"
        );
        assert!(
            message.contains("Warnings: disk warning"),
            "expected warnings, got: {message}"
        );
    }

    #[test]
    fn glob_pattern_fragment_supports_braces_and_globstar() {
        assert!(glob_matches_path("**/*.{ts,tsx}", "src/app/main.ts"));
        assert!(glob_matches_path("**/*.{ts,tsx}", "src/app/main.tsx"));
        assert!(!glob_matches_path("**/*.{ts,tsx}", "src/app/main.js"));
    }
}
