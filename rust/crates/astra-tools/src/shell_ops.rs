//! Shell operations: bash execution, grep, glob.

use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

use regex::Regex;
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::{ToolResult, per_tool_output_limit, truncate_output};

const GREP_TIMEOUT: Duration = Duration::from_secs(20);
const GLOB_TIMEOUT: Duration = Duration::from_secs(15);
const GREP_DEFAULT_HEAD_LIMIT: usize = 100;
const RAW_GREP_OUTPUT_LIMIT: usize = 30_000;
const RAW_GLOB_OUTPUT_LIMIT: usize = 120_000;

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
}

struct GrepRequest<'a> {
    workspace_root: &'a Path,
    target: &'a str,
    pattern: &'a str,
    include: Option<&'a str>,
    case_sensitive: bool,
    context_lines: Option<usize>,
    max_matches: Option<usize>,
    output_mode: SearchOutputMode,
}

/// Execute a bash command with timeout in a workspace directory.
pub async fn execute_bash(workspace_root: &Path, args: &Value) -> ToolResult {
    let command = match args.get("command").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::error("Error: Missing 'command' parameter".into()),
    };
    let timeout_secs = args
        .get("timeout")
        .and_then(|v| v.as_f64())
        .unwrap_or(30.0)
        .min(120.0);

    let timeout = Duration::from_secs_f64(timeout_secs);
    let output = tokio::time::timeout(timeout, async {
        tokio::process::Command::new("bash")
            .arg("-c")
            .arg(command)
            .current_dir(workspace_root)
            .kill_on_drop(true)
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
            if result.is_empty() {
                ToolResult::text("(command completed with no output)".into())
            } else {
                ToolResult::text(result)
            }
        }
        Ok(Err(e)) => ToolResult::error(format!("Error: Failed to execute command: {e}")),
        Err(_) => ToolResult::error(format!("Error: Command timed out after {timeout_secs}s")),
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
    let include = args.get("include").and_then(|v| v.as_str());
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .map(|value| value.min(10) as usize);
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
    let offset = args.get("offset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
    let head_limit = args
        .get("head_limit")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize);

    let request = GrepRequest {
        workspace_root,
        target: &target,
        pattern,
        include,
        case_sensitive,
        context_lines,
        max_matches,
        output_mode,
    };

    let command_output = if ripgrep_available() {
        run_grep_with_rg(&request, ctx.cancel_token.as_deref()).await
    } else {
        run_grep_with_grep(&request, ctx.cancel_token.as_deref()).await
    };

    let ReadOnlyCommandOutput {
        stdout,
        stderr,
        exit_code,
        timed_out,
        cancelled,
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
                "Error: grep timed out after 20s with no results. Narrow the search with 'path', 'include', or a more specific pattern.".into(),
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

    let lines: Vec<&str> = filtered.lines().collect();
    let paged_lines = if offset > 0 {
        if offset >= lines.len() {
            return ToolResult::text(format!(
                "No more results (offset {} >= {} lines)",
                offset,
                lines.len()
            ));
        }
        &lines[offset..]
    } else {
        &lines[..]
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

    let mut result_text = visible_lines.join("\n");
    result_text = truncate_output(result_text, per_tool_output_limit("grep"));

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
                "\n\n[grep timed out after 20s — showing partial results. Narrow the search with 'path' or 'include'.]",
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

    if resolved.is_file() {
        return if glob_matches_path(pattern, &target) {
            ToolResult::text(target)
        } else {
            ToolResult::text("No files found".into())
        };
    }

    let command_output = if ripgrep_available() {
        run_glob_with_rg(
            workspace_root,
            &target,
            pattern,
            ctx.cancel_token.as_deref(),
        )
        .await
    } else {
        run_glob_with_find(workspace_root, &target, ctx.cancel_token.as_deref()).await
    };

    let ReadOnlyCommandOutput {
        stdout,
        stderr,
        exit_code,
        timed_out,
        cancelled,
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
        .collect();
    files.sort();
    files.dedup();

    if files.is_empty() {
        return if cancelled {
            ToolResult::text("No files found before glob was cancelled".into())
        } else if timed_out {
            ToolResult::text("No files found before glob timed out".into())
        } else {
            ToolResult::text("No files found".into())
        };
    }

    let total_files = files.len();
    let mut result_text = files.join("\n");
    result_text = truncate_output(result_text, per_tool_output_limit("glob"));

    if cancelled {
        result_text.push_str(
            "\n\n[glob cancelled — showing partial results captured before cancellation.]",
        );
    } else if timed_out {
        result_text.push_str(
            "\n\n[glob timed out after 15s — showing partial results. Narrow the search with 'path' or a more specific pattern.]",
        );
    } else {
        result_text.push_str(&format!("\n({total_files} files)"));
    }

    ToolResult::text(result_text)
}

fn ripgrep_available() -> bool {
    *RIPGREP_AVAILABLE.get_or_init(|| {
        StdCommand::new("rg")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
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

async fn run_grep_with_rg(
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let mut cmd = Command::new("rg");
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
    if let Some(context) = request.context_lines {
        cmd.arg("-C").arg(context.to_string());
    }
    if let Some(max) = request.max_matches {
        cmd.arg("-m").arg(max.to_string());
    }
    if let Some(include) = request.include {
        cmd.arg("-g").arg(include);
    }
    append_default_rg_excludes(&mut cmd);
    cmd.arg("-e")
        .arg(request.pattern)
        .arg("--")
        .arg(request.target);

    run_readonly_command_with_partial(&mut cmd, GREP_TIMEOUT, RAW_GREP_OUTPUT_LIMIT, cancel_token)
        .await
}

async fn run_grep_with_grep(
    request: &GrepRequest<'_>,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let mut cmd = Command::new("grep");
    cmd.current_dir(request.workspace_root).kill_on_drop(true);

    match request.output_mode {
        SearchOutputMode::Content => {
            cmd.arg("-rnHE");
        }
        SearchOutputMode::FilesWithMatches => {
            cmd.arg("-rlHE");
        }
        SearchOutputMode::Count => {
            cmd.arg("-rcHE");
        }
    }

    if !request.case_sensitive {
        cmd.arg("-i");
    }
    if let Some(context) = request.context_lines {
        cmd.arg(format!("-C{context}"));
    }
    if let Some(max) = request.max_matches {
        cmd.arg(format!("-m{max}"));
    }
    if let Some(include) = request.include {
        cmd.arg("--include").arg(include);
    }
    append_default_grep_excludes(&mut cmd);
    cmd.arg("-e")
        .arg(request.pattern)
        .arg("--")
        .arg(request.target);

    run_readonly_command_with_partial(&mut cmd, GREP_TIMEOUT, RAW_GREP_OUTPUT_LIMIT, cancel_token)
        .await
}

async fn run_glob_with_rg(
    workspace_root: &Path,
    target: &str,
    pattern: &str,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let mut cmd = Command::new("rg");
    cmd.current_dir(workspace_root)
        .kill_on_drop(true)
        .arg("--files")
        .arg("--hidden");
    append_default_rg_excludes(&mut cmd);
    cmd.arg("-g").arg(pattern).arg(target);

    run_readonly_command_with_partial(&mut cmd, GLOB_TIMEOUT, RAW_GLOB_OUTPUT_LIMIT, cancel_token)
        .await
}

async fn run_glob_with_find(
    workspace_root: &Path,
    target: &str,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    let mut cmd = Command::new("find");
    cmd.current_dir(workspace_root).kill_on_drop(true);
    cmd.arg(target).arg("(").arg("-type").arg("d").arg("(");
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

    run_readonly_command_with_partial(&mut cmd, GLOB_TIMEOUT, RAW_GLOB_OUTPUT_LIMIT, cancel_token)
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

async fn run_readonly_command_with_partial(
    cmd: &mut Command,
    timeout: Duration,
    max_stdout_bytes: usize,
    cancel_token: Option<&CancellationToken>,
) -> Result<ReadOnlyCommandOutput, String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Error: failed to start search command: {e}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Error: failed to capture stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Error: failed to capture stderr".to_string())?;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(StreamKind, String)>();
    let stdout_task = tokio::spawn(read_stream(stdout, StreamKind::Stdout, tx.clone()));
    let stderr_task = tokio::spawn(read_stream(stderr, StreamKind::Stderr, tx));

    let deadline = tokio::time::Instant::now() + timeout;
    let mut stdout_text = String::new();
    let mut stderr_text = String::new();
    let mut stdout_capped = false;
    let mut exit_code = None;
    let mut timed_out = false;
    let mut cancelled = false;

    loop {
        drain_search_chunks(
            &mut rx,
            &mut stdout_text,
            &mut stderr_text,
            max_stdout_bytes,
            &mut stdout_capped,
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
            Err(e) => return Err(format!("Error: search command failed: {e}")),
        }
    }

    let _ = stdout_task.await;
    let _ = stderr_task.await;
    drain_search_chunks(
        &mut rx,
        &mut stdout_text,
        &mut stderr_text,
        max_stdout_bytes,
        &mut stdout_capped,
    );

    if (timed_out || cancelled)
        && let Some(last_newline) = stdout_text.rfind('\n')
    {
        stdout_text.truncate(last_newline);
    }

    Ok(ReadOnlyCommandOutput {
        stdout: stdout_text,
        stderr: stderr_text,
        exit_code: exit_code.unwrap_or(-1),
        timed_out,
        cancelled,
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

fn drain_search_chunks(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<(StreamKind, String)>,
    stdout_text: &mut String,
    stderr_text: &mut String,
    max_stdout_bytes: usize,
    stdout_capped: &mut bool,
) {
    while let Ok((kind, chunk)) = rx.try_recv() {
        match kind {
            StreamKind::Stdout => {
                append_capped(stdout_text, &chunk, max_stdout_bytes, stdout_capped)
            }
            StreamKind::Stderr => stderr_text.push_str(&chunk),
        }
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
    use tempfile::tempdir;

    use super::*;

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
            Some(&token),
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

    #[test]
    fn glob_pattern_fragment_supports_braces_and_globstar() {
        assert!(glob_matches_path("**/*.{ts,tsx}", "src/app/main.ts"));
        assert!(glob_matches_path("**/*.{ts,tsx}", "src/app/main.tsx"));
        assert!(!glob_matches_path("**/*.{ts,tsx}", "src/app/main.js"));
    }
}
