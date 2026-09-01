//! File operations: read, write, str_replace, delete, list_dir.
//!
//! All operations are sandboxed to a workspace root directory. Path traversal
//! via `..` is normalized before the boundary check to prevent escapes.

use std::io::Read;
#[cfg(test)]
use std::io::{Seek, SeekFrom};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicIsize, Ordering as AtomicOrdering};

use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::code_intel;
use crate::fuzzy_replacer::{
    fuzzy_find_replacement, normalize_ws, preserve_quote_style, quote_normalized_match_count,
};
use crate::{ToolResult, per_tool_output_limit};

const READ_FILE_SIZE_LIMIT: usize = 80 * 1024;
/// Hard ceiling: files above this size are never read into memory for preview.
const READ_FILE_HARD_LIMIT: usize = 10 * 1024 * 1024;

#[cfg(test)]
static MULTI_PATH_RENAME_FAILURE_INDEX: AtomicIsize = AtomicIsize::new(-1);
/// Format file size in MB with one decimal place, avoiding integer division truncation.
fn format_file_size_mb(size_bytes: u64) -> String {
    format!("{:.1} MB", size_bytes as f64 / (1024.0 * 1024.0))
}
const IMAGE_READ_SIZE_LIMIT: u64 = 10 * 1024 * 1024;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
const BINARY_EXTS: &[&str] = &[
    "svg", "pdf", "zip", "gz", "tar", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "o",
    "a", "lib", "wasm", "class", "pyc", "pyo", "mp3", "mp4", "avi", "mov", "wav", "flac", "ogg",
    "ttf", "otf", "woff", "woff2", "eot", "sqlite", "db", "mdb", "ico",
];

const READ_FILE_ALLOWED_FIELDS: &[&str] = &[
    "path",
    "start_line",
    "end_line",
    "outline",
    // System-level transaction/rollback fields injected by the rollback engine.
    "transaction_id",
    "rollback_on_failure",
    "rollback_boundary",
    "rollback_state",
    "rollback",
    // System-level field injected by headless tool pipeline for idempotency
    // and work-surface deduplication.
    "_tool_call_id",
];
const READ_FILE_VISIBLE_FIELDS: &[&str] = &["path", "start_line", "end_line", "outline"];

pub fn validate_read_file_args(args: &Value) -> Result<(), String> {
    let Some(object) = args.as_object() else {
        return Err(
            "Error: read_file arguments must be a JSON object with required field `path`."
                .to_string(),
        );
    };
    for key in object.keys() {
        if !READ_FILE_ALLOWED_FIELDS.contains(&key.as_str()) {
            let hint = read_file_unknown_field_hint(key);
            return Err(format!(
                "Error: unknown field `{key}` for read_file. Valid fields: {}. Required: path.{}",
                READ_FILE_VISIBLE_FIELDS.join(", "),
                hint
            ));
        }
    }
    match object.get("path") {
        Some(Value::String(path)) if !path.trim().is_empty() => Ok(()),
        Some(_) => Err(
            "Error: field `path` for read_file must be a non-empty string. Valid fields: path, start_line, end_line, outline."
                .to_string(),
        ),
        None => Err(
            "Error: missing required field `path` for read_file. Valid fields: path, start_line, end_line, outline."
                .to_string(),
        ),
    }?;
    validate_read_file_line_arg(object, "start_line")?;
    validate_read_file_line_arg(object, "end_line")?;
    if let (Some(start), Some(end)) = (
        object.get("start_line").and_then(Value::as_u64),
        object.get("end_line").and_then(Value::as_u64),
    ) && start > end
    {
        return Err(
            "Error: `start_line` must not exceed `end_line` for read_file. Use an inclusive 1-based range, or omit `end_line` to read through the end of the file."
                .to_string(),
        );
    }
    if let Some(value) = object.get("outline")
        && !value.is_boolean()
    {
        return Err(
            "Error: field `outline` for read_file must be a boolean. Valid fields: path, start_line, end_line, outline."
                .to_string(),
        );
    }
    Ok(())
}

fn read_file_unknown_field_hint(key: &str) -> &'static str {
    match key {
        "file" | "filename" => " Use `path` for the file path.",
        "offset" | "limit" | "length" | "count" => {
            " `read_file` uses an inclusive `start_line` + `end_line` range; omit `end_line` to read from `start_line` through the end of the file."
        }
        _ => "",
    }
}

fn validate_read_file_line_arg(
    object: &serde_json::Map<String, Value>,
    field: &str,
) -> Result<(), String> {
    let Some(value) = object.get(field) else {
        return Ok(());
    };
    match value.as_u64() {
        Some(value) if value > 0 => Ok(()),
        _ => Err(format!(
            "Error: field `{field}` for read_file must be a positive integer. Valid fields: path, start_line, end_line, outline. Omit `end_line` to read through the end of the file."
        )),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadLineRange {
    pub start_line: usize,
    pub end_line: usize,
}

pub fn normalize_read_file_line_range(
    start_line: Option<usize>,
    end_line: Option<usize>,
    total_lines: usize,
) -> ReadLineRange {
    let start = start_line.unwrap_or(1);
    let end = end_line.unwrap_or(total_lines);
    ReadLineRange {
        start_line: start,
        end_line: end,
    }
}

/// Build a structured str_replace failure message.
///
/// All six `str_replace` failure paths in this module share this banner so
/// downstream matchers (hallucination tripwire, step recorder, tool result
/// semantics) can rely on a single sentinel:
/// `❌ STR_REPLACE FAILED — FILE NOT MODIFIED`.
///
/// The "old_str not found" path in `str_replace_not_found_hint` calls this
/// helper and then appends diagnostic hints (whitespace near-misses,
/// line-by-line context).
pub fn str_replace_fail(what: &str, why: &str, next: &str) -> String {
    format!("❌ STR_REPLACE FAILED — FILE NOT MODIFIED\n\nWHAT: {what}\nWHY:  {why}\nNEXT: {next}")
}

/// Anchor (`old_str`) shorter than this is considered "too short to locate a
/// large insertion uniquely without re-failing on minor whitespace drift".
const MIN_ANCHOR_BYTES_FOR_LARGE_REPLACEMENT: usize = 32;

/// Replacement (`new_str`) larger than this should go through `write_file`
/// rather than `str_replace`. Threshold chosen to absorb typical
/// section-level edits (~30-50 lines) while rejecting full-file pastes that
/// belong in `write_file`.
const MAX_REPLACEMENT_BYTES_FOR_SHORT_ANCHOR: usize = 4096;

/// Guard against the "short old_str + huge new_str" anti-pattern that
/// dominates failed edit retries: the model uses a section header (e.g.
/// `## Anti-Patterns`) as anchor and pastes a multi-KB block, so any
/// whitespace drift causes a retry that re-emits the entire payload.
///
/// Returns `None` when the edit is acceptable, otherwise a structured error
/// pointing the model toward `write_file` or a longer anchor.
pub fn check_anchor_vs_replacement_size(
    edit_label: &str,
    old_str: &str,
    new_str: &str,
    replace_all: bool,
) -> Option<String> {
    if replace_all {
        return None;
    }
    if old_str.len() >= MIN_ANCHOR_BYTES_FOR_LARGE_REPLACEMENT
        || new_str.len() <= MAX_REPLACEMENT_BYTES_FOR_SHORT_ANCHOR
    {
        return None;
    }
    Some(str_replace_fail(
        &format!(
            "{edit_label} rejected: anchor too short for replacement size (old_str {} bytes, new_str {} bytes).",
            old_str.len(),
            new_str.len(),
        ),
        "Short anchors with multi-KB replacements fail repeatedly on minor whitespace drift, and each retry re-emits the entire new_str — a pattern that wastes context and rarely succeeds.",
        &format!(
            "Either (a) extend old_str to ≥{MIN_ANCHOR_BYTES_FOR_LARGE_REPLACEMENT} bytes (typically 3+ adjacent lines including surrounding context) so the anchor uniquely locates the target, or (b) call write_file with the complete file content for full-section rewrites."
        ),
    ))
}

fn validate_str_replace_anchor(edit_label: &str, old_str: &str) -> Result<(), String> {
    if old_str.trim().is_empty() {
        return Err(str_replace_fail(
            &format!("{edit_label} rejected: old_str is empty or whitespace-only."),
            "An empty anchor matches every byte boundary in the file, so the target location is undefined.",
            "Re-read the target file and provide the exact current bytes around the intended edit as old_str.",
        ));
    }
    Ok(())
}

fn normalize_path(path: &Path) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut acc, component| {
            match component {
                std::path::Component::ParentDir => {
                    acc.pop();
                }
                std::path::Component::CurDir => {}
                other => acc.push(other),
            }
            acc
        })
}

fn unique_path_variants(path: &Path) -> Vec<PathBuf> {
    let mut variants = vec![normalize_path(path)];
    if let Ok(canonical) = path.canonicalize()
        && !variants.iter().any(|existing| existing == &canonical)
    {
        variants.push(canonical);
    } else if let Ok(canonical_parent) = astra_sandbox::canonicalize_parent_and_append(path)
        && !variants
            .iter()
            .any(|existing| existing == &canonical_parent)
    {
        variants.push(canonical_parent);
    }
    variants
}

fn is_within_workspace_root(path: &Path, workspace_root: &Path) -> bool {
    let path_variants = unique_path_variants(path);
    let root_variants = unique_path_variants(workspace_root);

    path_variants.iter().any(|candidate| {
        root_variants
            .iter()
            .any(|root| candidate == root || candidate.starts_with(root))
    })
}

pub(crate) fn relative_to_workspace_root(workspace_root: &Path, path: &Path) -> Option<PathBuf> {
    let path_variants = unique_path_variants(path);
    let root_variants = unique_path_variants(workspace_root);

    path_variants.iter().find_map(|candidate| {
        root_variants.iter().find_map(|root| {
            candidate
                .strip_prefix(root)
                .ok()
                .map(std::path::Path::to_path_buf)
        })
    })
}

/// Default allowed paths that ALL sandbox modes include.
/// Both `SandboxConfig::standard` and `SandboxConfig::strict` allow `/tmp`
/// and the platform's resolved temp directory (e.g. macOS `/var/folders/.../T`).
fn default_allowed_paths() -> Vec<PathBuf> {
    let mut allowed = vec![PathBuf::from("/tmp")];
    let temp_dir = std::env::temp_dir();
    if !allowed.iter().any(|existing| existing == &temp_dir) {
        allowed.push(temp_dir);
    }
    allowed
}

/// Resolve a relative path against workspace_root with normalization.
///
/// Allows workspace root AND default allowed paths (`/tmp`).
/// For custom allowed paths, use [`resolve_path_sandboxed`].
pub fn resolve_path(workspace_root: &Path, relative: &str) -> Result<PathBuf, String> {
    resolve_path_sandboxed(workspace_root, relative, &default_allowed_paths())
}

/// Resolve a path with full sandbox awareness: allows workspace root
/// AND any path under `allowed_paths` (e.g., `/tmp`).
///
/// This closes the inconsistency where `bash("cat /tmp/x")` works but
/// `read_file("/tmp/x")` gets SANDBOX_DENIED. The `allowed_paths` list
/// comes from [`SandboxConfig`] and typically includes `/tmp`.
pub fn resolve_path_sandboxed(
    workspace_root: &Path,
    relative: &str,
    allowed_paths: &[PathBuf],
) -> Result<PathBuf, String> {
    let input_is_absolute = Path::new(relative).is_absolute();
    let path = if input_is_absolute {
        PathBuf::from(relative)
    } else {
        workspace_root.join(relative)
    };

    let normalized = normalize_path(&path);

    let final_path = if normalized.exists() {
        normalized
            .canonicalize()
            .map_err(|e| format!("Cannot resolve path: {e}"))?
    } else {
        // Canonicalize parent directory to resolve symlinks in the path
        // prefix even when the leaf doesn't exist yet. Without this, a
        // symlink in the workspace pointing outside (e.g. → /etc) would
        // bypass the sandbox check for not-yet-created files.
        match normalized.parent() {
            Some(parent) if parent.exists() => {
                let canonical_parent = parent
                    .canonicalize()
                    .map_err(|e| format!("Cannot resolve parent path: {e}"))?;
                match normalized.file_name() {
                    Some(name) => canonical_parent.join(name),
                    None => normalized,
                }
            }
            _ => normalized,
        }
    };

    // Check workspace root first.
    if is_within_workspace_root(&final_path, workspace_root) {
        return Ok(final_path);
    }

    // The allowlist (e.g. `/tmp`) only applies to inputs the caller
    // explicitly typed as absolute. A relative input that escapes the
    // workspace via `..` and happens to land inside an allowed prefix is
    // still a sandbox escape — the caller did not opt into the allowed
    // root, the path traversal did. Without this guard, a workspace
    // located under $TMPDIR can be escaped via `read_file("../foo")`.
    if input_is_absolute {
        for allowed in allowed_paths {
            if is_within_workspace_root(&final_path, allowed) {
                return Ok(final_path);
            }
        }
    }

    Err(format!(
        "SANDBOX_DENIED: Path '{}' is outside workspace root '{}'",
        relative,
        workspace_root.display()
    ))
}

fn path_resolution_failed(
    tool_name: &str,
    path_str: &str,
    what: &str,
    candidates: Vec<String>,
) -> ToolResult {
    let mut message = format!(
        "PATH_RESOLUTION_FAILED: {tool_name} target {what}: `{path_str}`.\nNo file operation was performed."
    );
    if !candidates.is_empty() {
        message.push_str("\ncanonical_candidates:");
        for candidate in candidates {
            message.push_str("\n- ");
            message.push_str(&candidate);
        }
    }
    message.push_str(
        "\nNEXT: Choose one canonical candidate explicitly, or inspect the workspace with glob/list_dir. Do not retry the stale path.",
    );
    ToolResult::error(message)
}

fn resolve_existing_path_for_tool(
    workspace_root: &Path,
    path_str: &str,
    tool_name: &str,
) -> Result<PathBuf, ToolResult> {
    let path = resolve_path(workspace_root, path_str).map_err(ToolResult::error)?;
    if path.exists() {
        return Ok(path);
    }
    Err(path_resolution_failed(
        tool_name,
        path_str,
        "does not exist",
        workspace_file_identity_candidates(workspace_root, path_str),
    ))
}

fn resolve_write_target_path(
    workspace_root: &Path,
    path_str: &str,
    tool_name: &str,
) -> Result<PathBuf, ToolResult> {
    let path = resolve_path(workspace_root, path_str).map_err(ToolResult::error)?;
    let Some(parent) = path.parent() else {
        return Ok(path);
    };
    if parent.exists() {
        return Ok(path);
    }
    let candidates = workspace_file_identity_candidates(workspace_root, path_str);
    if candidates.is_empty() {
        return Ok(path);
    }
    Err(path_resolution_failed(
        tool_name,
        path_str,
        "has a missing parent directory",
        candidates,
    ))
}

fn workspace_file_identity_candidates(workspace_root: &Path, requested: &str) -> Vec<String> {
    let requested_path = Path::new(requested);
    if requested_path.is_absolute() {
        return Vec::new();
    }
    let requested_components = path_identity_components(requested_path);
    if requested_components.is_empty() {
        return Vec::new();
    }

    let mut best_suffix_len = 0usize;
    let mut candidates = Vec::new();
    for repo_path in collect_workspace_file_paths(workspace_root, 20_000) {
        let components = path_identity_components(&repo_path);
        let max_len = requested_components.len().min(components.len());
        let Some(matched_len) = (1..=max_len)
            .rev()
            .find(|len| path_components_suffix_eq(&components, &requested_components, *len))
        else {
            continue;
        };
        if matched_len < best_suffix_len {
            continue;
        }
        if matched_len > best_suffix_len {
            best_suffix_len = matched_len;
            candidates.clear();
        }
        candidates.push(repo_path.to_string_lossy().to_string());
    }

    candidates.sort();
    candidates.dedup();
    candidates.truncate(8);
    candidates
}

fn path_identity_components(path: &Path) -> Vec<String> {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(value) => {
                value.to_str().map(std::string::ToString::to_string)
            }
            _ => None,
        })
        .collect()
}

fn path_components_suffix_eq(path_components: &[String], requested: &[String], len: usize) -> bool {
    if len == 0 || len > path_components.len() || len > requested.len() {
        return false;
    }
    path_components[path_components.len() - len..] == requested[requested.len() - len..]
}

fn collect_workspace_file_paths(workspace_root: &Path, limit: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![workspace_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if should_skip_path_identity_dir(&path) {
                    continue;
                }
                stack.push(path);
            } else if file_type.is_file()
                && let Ok(relative) = path.strip_prefix(workspace_root)
            {
                out.push(relative.to_path_buf());
                if out.len() >= limit {
                    return out;
                }
            }
        }
    }
    out
}

fn should_skip_path_identity_dir(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(|name| name.to_str()),
        Some(
            ".git" | ".astra" | ".direnv" | ".next" | "node_modules" | "target" | "dist" | "build"
        )
    )
}

pub fn read_file(workspace_root: &Path, args: &Value) -> ToolResult {
    if let Err(error) = validate_read_file_args(args) {
        return ToolResult::error(error);
    }
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return ToolResult::error(
                "Error: missing required field `path` for read_file. Valid fields: path, start_line, end_line, outline."
                    .into(),
            );
        }
    };
    if let Some(error) =
        crate::internal_artifacts::internal_tool_result_artifact_access_error("read_file", path_str)
    {
        return ToolResult::error(error);
    }
    let path = match resolve_existing_path_for_tool(workspace_root, path_str, "read_file") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let start_line = args
        .get("start_line")
        .and_then(Value::as_u64)
        .map(|l| l as usize);
    let end_line = args
        .get("end_line")
        .and_then(Value::as_u64)
        .map(|l| l as usize);
    let outline = args
        .get("outline")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let has_range = start_line.is_some() || end_line.is_some();

    let metadata = match std::fs::metadata(&path) {
        Ok(meta) => meta,
        Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
    };
    if metadata.is_dir() {
        return ToolResult::error(format!(
            "Error: '{}' is a directory. Use list_dir instead.",
            path_str
        ));
    }
    if !metadata.is_file() {
        return ToolResult::error(format!(
            "Error: refusing to read special file '{}'. Use bash with an appropriate tool instead.",
            path_str
        ));
    }

    if let Some(ext) = path.extension().and_then(|ext| ext.to_str()) {
        let ext_lower = ext.to_ascii_lowercase();
        if IMAGE_EXTS.contains(&ext_lower.as_str()) {
            if metadata.len() > IMAGE_READ_SIZE_LIMIT {
                return ToolResult::error(format!(
                    "Error: image file too large ({}). Maximum supported: 10MB.",
                    format_file_size_mb(metadata.len())
                ));
            }
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(e) => return ToolResult::error(format!("Error reading image: {e}")),
            };
            let mime = match ext_lower.as_str() {
                "png" => "image/png",
                "jpg" | "jpeg" => "image/jpeg",
                "gif" => "image/gif",
                "bmp" => "image/bmp",
                "webp" => "image/webp",
                _ => "application/octet-stream",
            };
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let data_uri = format!("data:{mime};base64,{encoded}");
            return ToolResult::text(crate::truncate_output(
                data_uri,
                per_tool_output_limit("read_file"),
            ));
        }

        if BINARY_EXTS.contains(&ext_lower.as_str()) {
            return ToolResult::error(format!(
                "Error: refusing to read binary file (.{ext}). Use bash with appropriate tools (e.g. file, xxd, strings) for binary analysis."
            ));
        }
    }

    // Hard ceiling: refuse to load extremely large files into memory for full/preview reads.
    // Range reads (start_line/end_line) and outline are still allowed — they read the file
    // but produce bounded output, and 10MB is still manageable for a single read_to_string.
    if !has_range && !outline && metadata.len() as usize > READ_FILE_HARD_LIMIT {
        return ToolResult::error(format!(
                "Error: file is too large ({} bytes). Use start_line/end_line to read a specific range, or outline=true.",
            metadata.len()
        ))
        .with_failure_evidence(astra_core::ToolFailureEvidence::new(
            astra_core::ErrorKind::ToolInvalidArgs,
            astra_core::ToolFailureCause::InputTooLarge,
            false,
            vec![
                astra_core::ToolRecoveryAction::ReadTargetedRange,
                astra_core::ToolRecoveryAction::SearchBeforeRead,
                astra_core::ToolRecoveryAction::NarrowScope,
            ],
        ));
    }

    // For large files without explicit range, provide a helpful preview instead of error.
    // This auto-pagination helps the agent understand file structure without manual range specification.
    if !has_range && !outline && metadata.len() as usize > READ_FILE_SIZE_LIMIT {
        // The hard ceiling above bounds this full source read at 10 MiB.  A
        // range-only head/tail scan is tempting, but it cannot tell whether a
        // selected body line belongs to a PEM block whose BEGIN line was
        // outside the window.  Build a line-preserving safe view first, then
        // page that view.  This keeps the security boundary independent of
        // pagination while retaining the same bounded preview contract.
        const HEAD_LINES: usize = 50;
        const TAIL_LINES: usize = 20;
        let file_size = metadata.len();
        let raw_preview = match read_to_string_lossy(&path) {
            Ok(content) => content,
            Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
        };
        let total_lines = raw_preview.lines().count();
        let safe_preview =
            crate::credential_redaction::redact_line_window(&raw_preview, 1, total_lines);
        let safe_lines = safe_preview.lines().collect::<Vec<_>>();
        let head_lines = safe_lines
            .iter()
            .take(HEAD_LINES)
            .map(|line| (*line).to_string())
            .collect::<Vec<_>>();
        let tail_lines = if total_lines > HEAD_LINES + TAIL_LINES {
            safe_lines
                .iter()
                .skip(total_lines.saturating_sub(TAIL_LINES))
                .map(|line| (*line).to_string())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        let head_count = head_lines.len();
        let tail_count = tail_lines.len();
        let tail_start = if total_lines > HEAD_LINES + TAIL_LINES {
            total_lines - tail_count
        } else {
            head_count
        };

        let mut preview = String::new();
        preview.push_str(&format!(
            "# Large file preview ({} bytes, {} lines){}\n\n",
            file_size, total_lines, ""
        ));

        // Head section
        preview.push_str("## First lines (1-");
        preview.push_str(&head_count.to_string());
        preview.push_str(")\n```\n");
        for (i, line) in head_lines.iter().enumerate() {
            preview.push_str(&format!("{:4}. {}\n", i + 1, line));
        }
        preview.push_str("```\n\n");

        // Gap indicator if there's a gap
        if tail_start > head_count {
            preview.push_str(&format!(
                "... ({} lines omitted, use start_line/end_line to read specific ranges) ...\n\n",
                tail_start - head_count
            ));
        }

        // Tail section (only if there's content after head)
        if !tail_lines.is_empty() && tail_start > head_count {
            preview.push_str("## Last lines (");
            preview.push_str(&(tail_start + 1).to_string());
            preview.push('-');
            preview.push_str(&total_lines.to_string());
            preview.push_str(")\n```\n");
            for (i, line) in tail_lines.iter().enumerate() {
                preview.push_str(&format!("{:4}. {}\n", tail_start + i + 1, line));
            }
            preview.push_str("```\n\n");
        }

        // For outline, we need the full file content (only for code files)
        // Since outline uses tree-sitter, we need to read the full file anyway
        // But only do this for reasonably sized code files (< 1MB)
        if file_size < 1_000_000
            && let Some(outline_str) = render_outline(&path, &raw_preview, total_lines)
        {
            preview.push_str(
                &crate::credential_redaction::redact_credentials_for_display(&outline_str).0,
            );
        }

        // Truncate before appending tip so the tip is always intact when within limit.
        let limit = per_tool_output_limit("read_file");
        let tip = "\n**Tip**: Use `start_line`/`end_line` to read specific sections, or `outline=true` for definitions only.";
        if preview.len() + tip.len() > limit {
            preview = crate::credential_redaction::truncate_redacted_output(
                preview,
                limit.saturating_sub(tip.len()),
            );
        }
        preview.push_str(tip);

        return ToolResult::text(preview);
    }

    let raw_content = match read_to_string_lossy(&path) {
        Ok(content) => content,
        Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
    };
    let total_lines = raw_content.lines().count();

    if outline {
        let rendered = render_outline(&path, &raw_content, total_lines)
            .unwrap_or_else(|| no_definitions_outline_message(total_lines));
        return ToolResult::text(
            crate::credential_redaction::redact_credentials_for_display(&rendered).0,
        );
    }

    if !has_range {
        let content = crate::credential_redaction::redact_credentials_in_text(&raw_content).0;
        let numbered = add_line_numbers(&content, 1);
        let limit = per_tool_output_limit("read_file");
        if numbered.len() > limit {
            let mut truncated =
                crate::credential_redaction::truncate_redacted_output(numbered, limit);
            truncated.push_str(&format!(
                "\n[file has {total_lines} lines — use start_line/end_line or outline=true]"
            ));
            return ToolResult::text(truncated);
        }
        return ToolResult::text(numbered);
    }

    let lines: Vec<&str> = raw_content.lines().collect();
    if lines.is_empty() {
        return ToolResult::text("(empty file)".to_string());
    }

    let range = normalize_read_file_line_range(start_line, end_line, lines.len());
    let start = range.start_line.saturating_sub(1).min(lines.len());
    let end = range.end_line.min(lines.len());

    if start >= lines.len() {
        return ToolResult::error(format!(
            "Error: start_line {} exceeds file length {}",
            range.start_line,
            lines.len()
        ));
    }
    if start >= end {
        return ToolResult::text(format!(
            "(empty range: start_line {} >= end_line {} or file has only {} lines)",
            start + 1,
            end,
            lines.len()
        ));
    }

    let slice = crate::credential_redaction::redact_line_window(&raw_content, start + 1, end);
    let mut result = crate::credential_redaction::truncate_redacted_output(
        add_line_numbers(&slice, start + 1),
        per_tool_output_limit("read_file"),
    );
    if end < lines.len() {
        result.push_str(&format!(
            "\n[showing lines {}-{} of {}]",
            start + 1,
            end,
            lines.len()
        ));
    }
    ToolResult::text(result)
}

/// Read the last N lines of a file efficiently.
///
/// Uses reverse reading from the end of the file to avoid loading
/// the entire file into memory for large files.
#[cfg(test)]
fn read_last_n_lines(path: &Path, n: usize) -> std::io::Result<Vec<String>> {
    if n == 0 {
        return Ok(Vec::new());
    }

    let mut file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len() as usize;

    if file_size == 0 {
        return Ok(Vec::new());
    }

    // Read raw bytes from the end and only decode once we've aligned to a newline
    // boundary. That avoids starting in the middle of a UTF-8 codepoint.
    let mut buffer_size = 8192usize.min(file_size);
    let mut suffix = Vec::new();
    let mut newline_count = 0usize;
    let mut position = file_size;

    loop {
        let seek_pos = position.saturating_sub(buffer_size);
        file.seek(SeekFrom::Start(seek_pos as u64))?;

        let to_read = position - seek_pos;
        let mut buffer = vec![0u8; to_read];
        file.read_exact(&mut buffer)?;
        newline_count += buffer.iter().filter(|&&byte| byte == b'\n').count();

        if suffix.is_empty() {
            suffix = buffer;
        } else {
            buffer.extend_from_slice(&suffix);
            suffix = buffer;
        }

        if newline_count >= n || seek_pos == 0 {
            break;
        }

        position = seek_pos;
        buffer_size = (buffer_size * 2).min(65536); // Double buffer size, cap at 64KB
    }

    let scan_end = suffix
        .len()
        .saturating_sub(usize::from(matches!(suffix.last(), Some(b'\n'))));
    let mut start = 0usize;
    let mut seen_newlines = 0usize;
    for idx in (0..scan_end).rev() {
        if suffix[idx] == b'\n' {
            seen_newlines += 1;
            if seen_newlines == n {
                start = idx + 1;
                break;
            }
        }
    }

    // Ensure we start at a valid UTF-8 character boundary. If `start` landed
    // inside a multi-byte sequence (e.g. a CJK character), scan forward to the
    // next lead byte (0x00–0x7F or 0xC0–0xFF) so the first decoded line isn't
    // garbled with a `` replacement. This matters both when start > 0 (common
    // case: landed mid-character after a newline) and when start == 0 (rare
    // case: file has fewer lines than n and seek_pos landed mid-character).
    let mut safe_start = start;
    while safe_start < suffix.len() && suffix[safe_start] & 0xC0 == 0x80 {
        safe_start += 1;
    }

    let text = String::from_utf8_lossy(&suffix[safe_start..]);
    Ok(text.lines().map(|line| line.to_string()).collect())
}

pub fn read_to_string_lossy(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    match String::from_utf8(bytes) {
        Ok(content) => Ok(content),
        Err(error) => Ok(String::from_utf8_lossy(&error.into_bytes()).into_owned()),
    }
}

fn add_line_numbers(content: &str, start_line: usize) -> String {
    let lines: Vec<&str> = content.split('\n').collect();
    let max_num = start_line + lines.len().saturating_sub(1);
    let width = max_num.to_string().len().max(1);

    lines
        .into_iter()
        .enumerate()
        .map(|(idx, line)| format!("{:width$}\t{}", start_line + idx, line, width = width))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_outline(path: &Path, content: &str, total_lines: usize) -> Option<String> {
    if let Some(lang) = code_intel::detect_language(path) {
        let outline = code_intel::generate_outline(content, lang);
        if !outline.is_empty() {
            let symbol_count = outline.lines().count();
            return Some(format!(
                "# Outline ({total_lines} lines, {symbol_count} symbols)\n{outline}"
            ));
        }
    }

    let fallback = fallback_outline(content);
    if fallback.is_empty() {
        None
    } else {
        Some(format!(
            "# Outline ({total_lines} lines total, {} definitions)\n{}",
            fallback.len(),
            fallback
                .into_iter()
                .map(|(line_no, sig)| format!("L{line_no}: {sig}"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
    }
}

fn no_definitions_outline_message(total_lines: usize) -> String {
    format!("(no definitions found in {total_lines}-line file)")
}

fn fallback_outline(content: &str) -> Vec<(usize, String)> {
    content
        .lines()
        .enumerate()
        .filter_map(|(idx, line)| {
            let trimmed = line.trim_start();
            let looks_like_definition = trimmed.starts_with("fn ")
                || trimmed.starts_with("pub fn ")
                || trimmed.starts_with("async fn ")
                || trimmed.starts_with("pub async fn ")
                || trimmed.starts_with("def ")
                || trimmed.starts_with("class ")
                || trimmed.starts_with("struct ")
                || trimmed.starts_with("pub struct ")
                || trimmed.starts_with("trait ")
                || trimmed.starts_with("pub trait ")
                || trimmed.starts_with("enum ")
                || trimmed.starts_with("pub enum ")
                || trimmed.starts_with("interface ")
                || trimmed.starts_with("type ")
                || trimmed.starts_with("func ");
            looks_like_definition.then(|| (idx + 1, trimmed.to_string()))
        })
        .collect()
}

#[derive(Debug)]
pub struct PreparedWriteFile {
    path: PathBuf,
    path_str: String,
    content: String,
    /// SHA-256 hex digest of the file content as it was read (None for new files).
    /// Verified before commit to detect concurrent modifications.
    original_content_hash: Option<String>,
    /// Exact complete-state no-op established before any staging file,
    /// journal entry, cache invalidation, or workspace generation change.
    already_desired: bool,
    requested_content_state: crate::workspace_observation::WorkspaceFileStateIdentity,
}

impl PreparedWriteFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn content_bytes(&self) -> &[u8] {
        self.content.as_bytes()
    }

    /// Owner-derived outcome from the prepared full-state comparison. This is
    /// available before apply so journal owners can avoid recording an undo
    /// entry for an exact no-op; apply still revalidates the preimage hash.
    pub fn is_already_desired(&self) -> bool {
        self.already_desired
    }

    pub fn apply(&self) -> ToolResult {
        self.apply_with_formatting(true)
    }

    fn apply_with_formatting(&self, format_staging: bool) -> ToolResult {
        if self.already_desired {
            if let Err(error) =
                verify_expected_original_hash(&self.path, self.original_content_hash.as_deref())
            {
                return ToolResult::error(error);
            }
            return ToolResult::text(format!(
                "{} already contains the requested {} bytes; no bytes were written",
                self.path_str,
                self.content.len()
            ))
            .with_workspace_desired_state_converged(
                self.requested_content_state.clone(),
                crate::workspace_observation::workspace_file_state_identity(
                    self.content.as_bytes(),
                ),
            );
        }
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolResult::error(format!("Error: Cannot create directories: {e}"));
        }

        match write_file_atomic_with_format(
            &self.path,
            self.content.as_bytes(),
            false,
            self.original_content_hash.as_deref(),
            format_staging,
        ) {
            Ok(warning) => {
                let mut message = format!(
                    "Successfully wrote {} bytes to {}",
                    self.content.len(),
                    self.path_str
                );
                if let Some(warning) = warning {
                    message.push_str(&format!("\nWarning: {warning}"));
                }
                ToolResult::text(message).with_workspace_mutation_applied()
            }
            Err(e) => ToolResult::error(e),
        }
    }
}

pub fn prepare_write_file(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedWriteFile, ToolResult> {
    let Some(input) = args.as_object() else {
        return Err(ToolResult::error(
            "Error: write_file arguments must be an object".into(),
        ));
    };
    reject_unknown_fields(
        input,
        "write_file",
        &["content", "delete", "path", "_tool_call_id"],
    )
    .map_err(ToolResult::error)?;

    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'path' parameter. Retry write_file with both path and content. Do not switch to bash or python just to write this file.".into(),
            ));
        }
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'content' parameter. Retry write_file with both path and content. Do not switch to bash or python just to write this file.".into(),
            ));
        }
    };
    let path = resolve_write_target_path(workspace_root, path_str, "write_file")?;
    let requested_content_state =
        crate::workspace_observation::workspace_file_state_identity(content.as_bytes());
    let content = normalize_content_before_write(&path, content);

    let existing_bytes = std::fs::read(&path).ok();
    let already_desired = existing_bytes.as_deref() == Some(content.as_bytes());
    let original_content_hash = existing_bytes
        .as_deref()
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)));

    Ok(PreparedWriteFile {
        path,
        path_str: path_str.to_string(),
        content,
        original_content_hash,
        already_desired,
        requested_content_state,
    })
}

pub fn write_file(workspace_root: &Path, args: &Value) -> ToolResult {
    match prepare_write_file(workspace_root, args) {
        Ok(prepared) => prepared.apply(),
        Err(error) => error,
    }
}

pub(crate) fn write_file_without_formatter(workspace_root: &Path, args: &Value) -> ToolResult {
    match prepare_write_file(workspace_root, args) {
        Ok(prepared) => prepared.apply_with_formatting(false),
        Err(error) => error,
    }
}

#[derive(Debug)]
pub struct PreparedStrReplace {
    path: PathBuf,
    new_content: String,
    dry_run: bool,
    allow_structural_change: bool,
    success_message: String,
    /// SHA-256 hex digest of the file content as it was read.
    /// Verified before commit to detect concurrent modifications.
    original_content_hash: Option<String>,
}

impl PreparedStrReplace {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn new_content_bytes(&self) -> &[u8] {
        self.new_content.as_bytes()
    }

    pub fn is_dry_run(&self) -> bool {
        self.dry_run
    }

    pub fn apply(self) -> ToolResult {
        self.apply_with_formatting(true)
    }

    fn apply_with_formatting(self, format_staging: bool) -> ToolResult {
        if self.dry_run {
            return ToolResult::text(self.success_message);
        }
        match write_file_atomic_with_format(
            &self.path,
            self.new_content.as_bytes(),
            self.allow_structural_change,
            self.original_content_hash.as_deref(),
            format_staging,
        ) {
            Ok(warning) => {
                let mut message = self.success_message;
                if let Some(warning) = warning {
                    message.push_str(&format!("\nWarning: {warning}"));
                }
                ToolResult::text(message).with_workspace_mutation_applied()
            }
            Err(e) => ToolResult::error(e),
        }
    }
}

pub fn prepare_str_replace(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedStrReplace, ToolResult> {
    let args = normalize_str_replace_args(args).map_err(ToolResult::error)?;
    let args = &args;
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'path' parameter. Retry str_replace with path plus either old_str/new_str or edits.".into(),
            ));
        }
    };
    let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'old_str' parameter. Retry str_replace single-edit mode with path, old_str, and new_str; or use edits for batch mode.".into(),
            ));
        }
    };
    let new_str = match args.get("new_str").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'new_str' parameter. Retry str_replace single-edit mode with path, old_str, and new_str; or use edits for batch mode.".into(),
            ));
        }
    };
    crate::credential_redaction::reject_redaction_markers_in_replacement(new_str)
        .map_err(ToolResult::error)?;
    validate_str_replace_anchor("str_replace", old_str).map_err(ToolResult::error)?;
    if old_str == new_str {
        return Err(ToolResult::error(str_replace_fail(
            "old_str and new_str are identical — no change needed.",
            "The replacement is a no-op; the file would be unchanged.",
            "Provide a new_str that actually differs from old_str, or skip the edit.",
        )));
    }
    let replace_all = args
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_structural_change = args
        .get("allow_structural_change")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(err) =
        check_anchor_vs_replacement_size("str_replace", old_str, new_str, replace_all)
    {
        return Err(ToolResult::error(err));
    }

    let path = resolve_existing_path_for_tool(workspace_root, path_str, "str_replace")?;

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return Err(ToolResult::error(format!("Error: Cannot read file: {e}"))),
    };

    // Credential values are intentionally unavailable in model context.  A
    // complete non-secret redaction marker is a safe edit reference: resolve
    // it against the raw file at execution time, and fail closed on forged or
    // ambiguous references instead of asking the model to repeat the secret.
    let redaction_reference =
        crate::credential_redaction::resolve_redacted_anchor(&content, old_str, replace_all)
            .map_err(ToolResult::error)?;
    let old_str = redaction_reference.as_deref().unwrap_or(old_str);
    // Resolve opaque anchors before deciding whether this is a no-op.  A
    // marker can differ from the source value while still resolving to the
    // exact new_str (for example when a credential-shaped placeholder was
    // already applied).  Such a request must not write, journal, advance the
    // workspace generation, or emit a mutation-success sentinel.
    if old_str == new_str {
        return Err(ToolResult::error(str_replace_fail(
            "the resolved old_str already equals new_str — no change needed.",
            "The anchor resolved successfully, but the file already contains the requested replacement.",
            "Choose a different new_str or skip this edit; no bytes were changed.",
        )));
    }

    // Capture content hash BEFORE any mutation so the commit phase
    // can detect concurrent modifications (another process or a
    // parallel agent writing the same file between our read and
    // the final rename).
    let original_hash = content_hash(&content);

    let count = content.matches(old_str).count();
    if count == 0 {
        let normalized_quote_count = quote_normalized_match_count(&content, old_str);
        if normalized_quote_count > 1 && !replace_all {
            return Err(ToolResult::error(format!(
                "Error: old_str found {normalized_quote_count} times in {path_str} after normalizing curly quotes. Make old_str more specific to match exactly once."
            )));
        }

        if let Some(fuzzy_match) = fuzzy_find_replacement(&content, old_str, replace_all) {
            let replacement = if fuzzy_match.is_quote_normalized() {
                preserve_quote_style(old_str, fuzzy_match.actual, new_str)
            } else {
                new_str.to_string()
            };
            let mut new_content = if replace_all {
                content.replace(fuzzy_match.actual, &replacement)
            } else {
                content.replacen(fuzzy_match.actual, &replacement, 1)
            };
            if new_content == content {
                return Err(ToolResult::error(str_replace_fail(
                    "the resolved replacement would not change the file.",
                    "The anchor matched, but the resulting file bytes are identical to the current content.",
                    "Choose a different new_str or skip this edit; no bytes were changed.",
                )));
            }
            if !allow_structural_change {
                validate_structural_edit(
                    &path,
                    &content,
                    &new_content,
                    fuzzy_match.actual,
                    new_str,
                )
                .map_err(ToolResult::error)?;
            }
            new_content = normalize_content_before_write(&path, &new_content);
            if new_content == content {
                return Err(ToolResult::error(str_replace_fail(
                    "the normalized replacement would not change the file.",
                    "The fuzzy anchor matched, but deterministic newline/format normalization returns the exact original bytes.",
                    "Choose a replacement that changes the normalized file, or skip this edit; no bytes were changed.",
                )));
            }
            let success_message = if dry_run {
                unified_diff(&content, &new_content, path_str)
            } else {
                format!(
                    "Successfully replaced text in {} (matched via {})",
                    path_str, fuzzy_match.strategy
                )
            };
            return Ok(PreparedStrReplace {
                path,
                new_content,
                dry_run,
                allow_structural_change,
                success_message,
                original_content_hash: Some(original_hash),
            });
        }

        if replace_all && normalized_quote_count > 1 {
            return Err(ToolResult::error(str_replace_fail(
                &format!("Cannot replace_all in {path_str}."),
                &format!(
                    "old_str matches {normalized_quote_count} occurrences after normalizing curly quotes, but the file mixes straight and curly quote forms."
                ),
                "Either (a) split into multiple targeted str_replace calls with surrounding context to disambiguate, or (b) normalize the file's quote style first, then retry.",
            )));
        }

        return Err(ToolResult::error(str_replace_not_found_hint(
            path_str, &content, old_str,
        )));
    }
    if count > 1 && !replace_all {
        return Err(ToolResult::error(str_replace_fail(
            &format!("old_str is ambiguous in {path_str}."),
            &format!(
                "old_str matched {count} times; without replace_all=true the target location is undefined."
            ),
            "Add more surrounding context lines to old_str so it matches exactly once, OR pass replace_all=true if you intend to replace every occurrence.",
        )));
    }

    let mut new_content = if replace_all {
        content.replace(old_str, new_str)
    } else {
        content.replacen(old_str, new_str, 1)
    };
    if new_content == content {
        return Err(ToolResult::error(str_replace_fail(
            "the resolved replacement would not change the file.",
            "The anchor matched, but the resulting file bytes are identical to the current content.",
            "Choose a different new_str or skip this edit; no bytes were changed.",
        )));
    }
    if !allow_structural_change {
        validate_structural_edit(&path, &content, &new_content, old_str, new_str)
            .map_err(ToolResult::error)?;
    }
    new_content = normalize_content_before_write(&path, &new_content);
    if new_content == content {
        return Err(ToolResult::error(str_replace_fail(
            "the normalized replacement would not change the file.",
            "The anchor matched, but deterministic newline/format normalization returns the exact original bytes.",
            "Choose a replacement that changes the normalized file, or skip this edit; no bytes were changed.",
        )));
    }
    let success_message = if dry_run {
        unified_diff(&content, &new_content, path_str)
    } else if replace_all {
        format!(
            "Successfully replaced text in {} ({count} occurrences)",
            path_str
        )
    } else {
        format!("Successfully replaced text in {}", path_str)
    };
    Ok(PreparedStrReplace {
        path,
        new_content,
        dry_run,
        allow_structural_change,
        success_message,
        original_content_hash: Some(original_hash),
    })
}

pub fn str_replace(workspace_root: &Path, args: &Value) -> ToolResult {
    str_replace_with_formatting(workspace_root, args, true)
}

pub(crate) fn str_replace_without_formatter(workspace_root: &Path, args: &Value) -> ToolResult {
    str_replace_with_formatting(workspace_root, args, false)
}

fn str_replace_with_formatting(
    workspace_root: &Path,
    args: &Value,
    format_staging: bool,
) -> ToolResult {
    let args = match normalize_str_replace_args(args) {
        Ok(args) => args,
        Err(error) => return ToolResult::error(error),
    };
    if args.get("edits").and_then(Value::as_array).is_some() {
        return multi_path_edit(workspace_root, &args, format_staging);
    }
    match prepare_str_replace(workspace_root, &args) {
        Ok(prepared) => prepared.apply_with_formatting(format_staging),
        Err(error) => error,
    }
}

#[derive(Debug, Clone)]
pub struct PreparedMultiEdit {
    path: PathBuf,
    path_str: String,
    new_content: String,
    edit_count: usize,
    dry_run: bool,
    allow_structural_change: bool,
    /// Formatter warning captured during the staging phase (if any).
    /// Carried through to the commit message.
    warning: Option<String>,
    /// SHA-256 hex digest of the file content as it was read.
    /// Verified before commit to detect concurrent modifications.
    original_content_hash: Option<String>,
}

impl PreparedMultiEdit {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn new_content_bytes(&self) -> &[u8] {
        self.new_content.as_bytes()
    }

    pub fn apply(&self) -> ToolResult {
        self.apply_with_formatting(true)
    }

    fn apply_with_formatting(&self, format_staging: bool) -> ToolResult {
        if self.dry_run {
            return ToolResult::text(format!(
                "Dry run: {} edit(s) would be applied to {}",
                self.edit_count, self.path_str
            ));
        }

        match write_file_atomic_with_format(
            &self.path,
            self.new_content.as_bytes(),
            self.allow_structural_change,
            self.original_content_hash.as_deref(),
            format_staging,
        ) {
            Ok(warning) => {
                let mut message = format!(
                    "Successfully applied {} edit(s) to {}",
                    self.edit_count, self.path_str
                );
                if let Some(warning) = warning {
                    message.push_str(&format!("\nWarning: {warning}"));
                }
                ToolResult::text(message).with_workspace_mutation_applied()
            }
            Err(e) => ToolResult::error(e),
        }
    }
}

pub fn prepare_multi_edit(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedMultiEdit, ToolResult> {
    let args = normalize_str_replace_args(args).map_err(ToolResult::error)?;
    let args = &args;
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'path' parameter. Retry str_replace batch mode with path and edits.".into(),
            ));
        }
    };
    let edits = match args.get("edits").and_then(|v| v.as_array()) {
        Some(e) => e,
        None => return Err(ToolResult::error("Error: Missing 'edits' array".into())),
    };
    if edits.is_empty() {
        return Err(ToolResult::error("Error: 'edits' array is empty".into()));
    }
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let allow_structural_change = args
        .get("allow_structural_change")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let path = resolve_existing_path_for_tool(workspace_root, path_str, "multi_edit")?;
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return Err(ToolResult::error(format!("Error: Cannot read file: {e}"))),
    };

    let original_content = content;
    let mut working = original_content.clone();
    for (i, edit) in edits.iter().enumerate() {
        let old_str = match edit.get("old_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return Err(ToolResult::error(format!(
                    "Error: edit[{i}] missing 'old_str'"
                )));
            }
        };
        let new_str = match edit.get("new_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => {
                return Err(ToolResult::error(format!(
                    "Error: edit[{i}] missing 'new_str'"
                )));
            }
        };
        crate::credential_redaction::reject_redaction_markers_in_replacement(new_str)
            .map_err(ToolResult::error)?;
        validate_str_replace_anchor(&format!("edit[{i}]"), old_str).map_err(ToolResult::error)?;
        let redaction_reference =
            crate::credential_redaction::resolve_redacted_anchor(&working, old_str, false)
                .map_err(ToolResult::error)?;
        let old_str = redaction_reference.as_deref().unwrap_or(old_str);
        if old_str == new_str {
            return Err(ToolResult::error(str_replace_fail(
                &format!("edit[{i}] is a no-op."),
                "old_str and new_str are byte-for-byte identical.",
                "Remove this edit, or fix new_str to reflect the intended change.",
            )));
        }
        if let Some(err) =
            check_anchor_vs_replacement_size(&format!("edit[{i}]"), old_str, new_str, false)
        {
            return Err(ToolResult::error(err));
        }
        let count = working.matches(old_str).count();
        if count == 0 {
            return Err(ToolResult::error(str_replace_not_found_hint_for_edit(
                path_str, &working, old_str, i,
            )));
        }
        if count > 1 {
            return Err(ToolResult::error(str_replace_fail(
                &format!("edit[{i}] old_str is ambiguous in {path_str}."),
                &format!(
                    "old_str matched {count} times; batch edits require exactly one match per edit."
                ),
                "Extend old_str with more surrounding context lines so it matches exactly once.",
            )));
        }
        let next = working.replacen(old_str, new_str, 1);
        if !allow_structural_change {
            validate_structural_edit(&path, &working, &next, old_str, new_str)
                .map_err(ToolResult::error)?;
        }
        working = next;
    }
    working = normalize_content_before_write(&path, &working);
    if working == original_content {
        return Err(ToolResult::error(str_replace_fail(
            "the normalized batch replacement would not change the file.",
            "The edits cancel out after deterministic newline normalization, so the final bytes equal the original.",
            "Change or remove the no-op edit; no bytes were changed.",
        )));
    }

    let original_content_hash = sha256_digest_of_existing_file(&path);

    Ok(PreparedMultiEdit {
        path,
        path_str: path_str.to_string(),
        new_content: working,
        edit_count: edits.len(),
        dry_run,
        allow_structural_change,
        warning: None,
        original_content_hash,
    })
}

#[derive(Debug)]
pub struct PreparedDeleteFile {
    path: PathBuf,
    path_str: String,
    before_content: Vec<u8>,
    /// SHA-256 hex digest of the file content captured at prepare time.
    /// Verified before commit to detect concurrent modifications: a delete
    /// that silently destroys newer content than the model read is unsafe.
    before_content_hash: Option<String>,
}

impl PreparedDeleteFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn before_content(&self) -> &[u8] {
        &self.before_content
    }

    pub fn apply(&self) -> ToolResult {
        // First principles: deleting a file that changed since it was read is
        // a silent data-loss hazard. Re-verify the hash before removing, so a
        // concurrent write between prepare→apply aborts instead of destroying
        // new content. This mirrors write_file / str_replace pre-commit checks.
        if let Err(e) =
            verify_expected_original_hash(&self.path, self.before_content_hash.as_deref())
        {
            return ToolResult::error(e);
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => ToolResult::text(format!("Successfully deleted {}", self.path_str))
                .with_workspace_mutation_applied(),
            Err(e) => ToolResult::error(format!("Error: Cannot delete file: {e}")),
        }
    }

    pub fn into_before_content(self) -> Vec<u8> {
        self.before_content
    }
}

pub fn prepare_delete_file(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedDeleteFile, ToolResult> {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Err(ToolResult::error("Error: Missing 'path' parameter".into())),
    };
    let path = resolve_existing_path_for_tool(workspace_root, path_str, "delete_file")?;

    if !path.exists() {
        return Err(ToolResult::error(format!(
            "Error: File not found: {path_str}"
        )));
    }

    let before_content = match std::fs::read(&path) {
        Ok(content) => content,
        Err(e) => {
            return Err(ToolResult::error(format!(
                "Error: Cannot read file before delete: {e}"
            )));
        }
    };

    let before_content_hash = sha256_digest_of_existing_file(&path);

    Ok(PreparedDeleteFile {
        path,
        path_str: path_str.to_string(),
        before_content,
        before_content_hash,
    })
}

pub fn delete_file(workspace_root: &Path, args: &Value) -> ToolResult {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    let path = match resolve_existing_path_for_tool(workspace_root, path_str, "delete_file") {
        Ok(p) => p,
        Err(e) => return e,
    };

    if !path.exists() {
        return ToolResult::error(format!("Error: File not found: {path_str}"));
    }

    match std::fs::remove_file(&path) {
        Ok(()) => ToolResult::text(format!("Successfully deleted {path_str}"))
            .with_workspace_mutation_applied(),
        Err(e) => ToolResult::error(format!("Error: Cannot delete file: {e}")),
    }
}

pub fn list_dir(workspace_root: &Path, args: &Value) -> ToolResult {
    let path_str = args.get("path").and_then(|v| v.as_str()).unwrap_or(".");
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };

    let entries = match std::fs::read_dir(&path) {
        Ok(entries) => entries,
        Err(e) => return ToolResult::error(format!("Error: Cannot list directory: {e}")),
    };

    let mut result = Vec::new();
    let mut regular_names = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            result.push(format!("{name}/"));
        } else {
            regular_names.push(name.clone());
            result.push(name);
        }
    }
    result.sort();

    // A directory listing is often the first observation in a recovery or
    // forensic task.  Surface a cheap, generic signal when one filename is a
    // delimiter-bounded companion of another (for example `source` and
    // `source-journal`), because opening the primary with a stateful parser
    // can implicitly consume or rewrite the companion.  This is advisory:
    // it never hides files, denies bash, or guesses a format-specific name.
    let mut companion_groups = Vec::new();
    for (index, primary) in regular_names.iter().enumerate() {
        let has_companion = regular_names
            .iter()
            .enumerate()
            .any(|(other_index, other)| {
                index != other_index
                    && other
                        .strip_prefix(primary)
                        .is_some_and(|suffix| !suffix.is_empty() && is_companion_suffix(suffix))
            });
        if has_companion {
            companion_groups.push(primary.as_str());
        }
    }
    companion_groups.sort_unstable();
    companion_groups.dedup();
    if !companion_groups.is_empty() {
        result.push(format!(
            "Advisory: related source/companion artifacts detected ({}) — for recovery, forensics, migration, or other stateful inspection, copy and checksum the source and companions before opening them with a parser or CLI; the tool may checkpoint, normalize, truncate, lock, or delete them implicitly.",
            companion_groups.join(", ")
        ));
    }
    ToolResult::text(result.join("\n"))
}

fn is_companion_suffix(suffix: &str) -> bool {
    suffix
        .chars()
        .next()
        .is_some_and(|first| matches!(first, '.' | '-' | '_' | '~'))
}

/// Apply multiple edits to a single file atomically (all-or-nothing).
///
/// Each edit must have `old_str` and `new_str`. All edits are validated
/// first (no partial application). `old_str` must match exactly once.
pub fn multi_edit(workspace_root: &Path, args: &Value) -> ToolResult {
    match prepare_multi_edit(workspace_root, args) {
        Ok(prepared) => prepared.apply(),
        Err(error) => error,
    }
}

pub(crate) fn multi_edit_without_formatter(workspace_root: &Path, args: &Value) -> ToolResult {
    match prepare_multi_edit(workspace_root, args) {
        Ok(prepared) => prepared.apply_with_formatting(false),
        Err(error) => error,
    }
}

#[derive(Debug)]
pub struct PreparedMultiPathEdit {
    prepared: Vec<PreparedMultiEdit>,
}

impl PreparedMultiPathEdit {
    pub fn prepared_edits(&self) -> &[PreparedMultiEdit] {
        &self.prepared
    }

    pub fn apply(&self) -> ToolResult {
        self.apply_with_formatting(true)
    }

    fn apply_with_formatting(&self, format_staging: bool) -> ToolResult {
        if self.prepared.iter().all(|prepared| prepared.dry_run) {
            let messages: Vec<String> = self
                .prepared
                .iter()
                .map(|prepared| {
                    format!(
                        "Dry run: {} edit(s) would be applied to {}",
                        prepared.edit_count, prepared.path_str
                    )
                })
                .collect();
            return ToolResult::text(if messages.len() == 1 {
                messages.into_iter().next().unwrap_or_default()
            } else {
                format!(
                    "Dry run: edits would be applied to {} file(s)\n{}",
                    self.prepared.len(),
                    messages.join("\n")
                )
            });
        }

        for prepared in &self.prepared {
            if let Err(error) = verify_expected_original_hash(
                &prepared.path,
                prepared.original_content_hash.as_deref(),
            ) {
                return ToolResult::error(error);
            }
        }

        // ── Two-phase atomic commit ────────────────────────────────────────
        // Phase 1: Write every file to a staging path next to the target.
        //          If any stage fails, no target file is touched.
        // Phase 2: If all stages succeeded, atomically rename every staging
        //          file to its target.  POSIX rename() is atomic on the same
        //          filesystem, so each file transitions from old → new
        //          without a window of partial content.
        //
        // This eliminates the dual journal+preimage rollback path entirely:
        // there is nothing to roll back because no target is modified until
        // every staging write has succeeded.
        let mut staging_entries: Vec<(PathBuf, PathBuf, PreparedMultiEdit)> =
            Vec::with_capacity(self.prepared.len());

        // Phase 1: Stage all files.
        for prepared in &self.prepared {
            if prepared.dry_run {
                // Dry-run batches are handled before reaching this point.
                continue;
            }
            let staging_path = staging_tmp_path(&prepared.path);
            // Best-effort cleanup of a stale staging file from a prior crash.
            let _ = std::fs::remove_file(&staging_path);

            if let Err(e) = std::fs::write(&staging_path, &prepared.new_content) {
                // Clean up any already-staged files before returning.
                for (_, staging, _) in &staging_entries {
                    let _ = std::fs::remove_file(staging);
                }
                let _ = std::fs::remove_file(&staging_path);
                return ToolResult::error(format!(
                    "Error: Cannot stage write for {}: {e}",
                    prepared.path_str
                ));
            }

            // Format the staging file (best-effort, same as single-file path).
            let formatter_outcome = if format_staging {
                format_file_in_place_best_effort(&staging_path)
            } else {
                FormatterOutcome::NotFound
            };
            let warning = match formatter_outcome {
                FormatterOutcome::Success | FormatterOutcome::NotFound => None,
                FormatterOutcome::Warning(w) => Some(w),
                FormatterOutcome::SyntaxError(error) => {
                    if prepared.allow_structural_change {
                        Some(error)
                    } else {
                        // Clean up all staged files.
                        for (_, staging, _) in &staging_entries {
                            let _ = std::fs::remove_file(staging);
                        }
                        let _ = std::fs::remove_file(&staging_path);
                        return ToolResult::error(error);
                    }
                }
            };

            staging_entries.push((
                prepared.path.clone(),
                staging_path,
                PreparedMultiEdit {
                    warning: warning.clone(),
                    ..prepared.clone()
                },
            ));
        }

        // Re-check all targets immediately before the first rename. This
        // catches edits made while staging/formatting without leaving a partial
        // multi-file commit behind.
        for (_, _, prepared) in &staging_entries {
            if let Err(error) = verify_expected_original_hash(
                &prepared.path,
                prepared.original_content_hash.as_deref(),
            ) {
                for (_, staging, _) in &staging_entries {
                    let _ = std::fs::remove_file(staging);
                }
                return ToolResult::error(error);
            }
        }

        // Phase 2: Commit all staged files via atomic rename.
        let mut messages = Vec::with_capacity(staging_entries.len());
        let mut committed_paths = Vec::new();
        for (target, staging, prepared) in &staging_entries {
            #[cfg(test)]
            if MULTI_PATH_RENAME_FAILURE_INDEX.load(AtomicOrdering::SeqCst)
                == committed_paths.len() as isize
            {
                let committed_paths = committed_paths.clone();
                let error = ToolResult::error(format!(
                    "Error: injected multi-path commit failure for {}",
                    prepared.path_str
                ));
                return if committed_paths.is_empty() {
                    error
                } else {
                    error.with_workspace_mutation_partial(committed_paths)
                };
            }
            if let Err(e) = std::fs::rename(staging, target) {
                // Rename failed — files already renamed before this point
                // are committed (same-fs rename is atomic per-file).  Files
                // not yet renamed have their staging artifacts still on disk;
                // attempt cleanup but don't fail the overall result — the
                // model already has error context.
                for (_, remaining_staging, _) in
                    staging_entries.iter().skip_while(|(t, _, _)| t != target)
                {
                    let _ = std::fs::remove_file(remaining_staging);
                }
                let error = ToolResult::error(format!(
                    "Error: Cannot commit write for {} (rename failed): {e}",
                    prepared.path_str
                ));
                return if committed_paths.is_empty() {
                    error
                } else {
                    error.with_workspace_mutation_partial(committed_paths)
                };
            }
            committed_paths.push(target.display().to_string());
            let mut message = format!(
                "Successfully applied {} edit(s) to {}",
                prepared.edit_count, prepared.path_str
            );
            if let Some(ref warning) = prepared.warning {
                message.push_str(&format!("\nWarning: {warning}"));
            }
            messages.push(message);
        }

        ToolResult::text(if self.prepared.len() == 1 {
            messages.into_iter().next().unwrap_or_default()
        } else {
            format!(
                "Successfully applied edits to {} file(s)\n{}",
                self.prepared.len(),
                messages.join("\n")
            )
        })
        .with_workspace_mutation_applied()
    }
}

fn multi_path_edit(workspace_root: &Path, args: &Value, format_staging: bool) -> ToolResult {
    match prepare_multi_path_edit(workspace_root, args) {
        Ok(prepared) => prepared.apply_with_formatting(format_staging),
        Err(error) => error,
    }
}

/// Partition a multi-file `edits` array into per-file groups.
///
/// Each edit entry may carry an optional `path`; entries without `path`
/// fall back to `top_path`. Returns `(path, scoped_edits)` pairs,
/// where each scoped edit is a map with only `old_str`/`new_str` keys.
pub fn partition_edits_by_path(
    edits: &[Value],
    top_path: Option<&str>,
) -> Result<Vec<(String, Vec<Value>)>, String> {
    let mut groups: Vec<(String, Vec<Value>)> = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let edit_path = edit.get("path").and_then(Value::as_str).or(top_path);
        let Some(path) = edit_path.filter(|p| !p.trim().is_empty()) else {
            return Err(format!(
                "Error: str_replace edit[{index}] is missing 'path'. Provide top-level path for a same-file batch, or path inside every edit for multi-file batch mode."
            ));
        };
        let Some(edit_obj) = edit.as_object() else {
            return Err(format!(
                "Error: str_replace edit[{index}] must be an object"
            ));
        };
        let mut scoped_edit = serde_json::Map::new();
        for key in ["old_str", "new_str"] {
            if let Some(value) = edit_obj.get(key) {
                scoped_edit.insert(key.to_string(), value.clone());
            }
        }
        if let Some((_, existing)) = groups
            .iter_mut()
            .find(|(existing_path, _)| existing_path == path)
        {
            existing.push(Value::Object(scoped_edit));
        } else {
            groups.push((path.to_string(), vec![Value::Object(scoped_edit)]));
        }
    }
    Ok(groups)
}

pub fn prepare_multi_path_edit(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedMultiPathEdit, ToolResult> {
    let args = normalize_str_replace_args(args).map_err(ToolResult::error)?;
    let top_path = args.get("path").and_then(Value::as_str);
    let edits = match args.get("edits").and_then(Value::as_array) {
        Some(edits) => edits,
        None => return Err(ToolResult::error("Error: Missing 'edits' array".into())),
    };
    if edits.is_empty() {
        return Err(ToolResult::error("Error: 'edits' array is empty".into()));
    }

    let dry_run = args
        .get("dry_run")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let allow_structural_change = args
        .get("allow_structural_change")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let groups = partition_edits_by_path(edits, top_path).map_err(ToolResult::error)?;

    let mut prepared = Vec::with_capacity(groups.len());
    for (path, edits) in groups {
        let mut scoped = serde_json::Map::new();
        scoped.insert("path".to_string(), Value::String(path));
        scoped.insert("edits".to_string(), Value::Array(edits));
        if dry_run {
            scoped.insert("dry_run".to_string(), Value::Bool(true));
        }
        if allow_structural_change {
            scoped.insert("allow_structural_change".to_string(), Value::Bool(true));
        }
        prepared.push(prepare_multi_edit(workspace_root, &Value::Object(scoped))?);
    }

    Ok(PreparedMultiPathEdit { prepared })
}

pub fn normalize_str_replace_args(args: &Value) -> Result<Value, String> {
    let Some(input) = args.as_object() else {
        return Err("Error: str_replace arguments must be an object".to_string());
    };
    let mut out = input.clone();

    reject_unknown_fields(
        input,
        "str_replace",
        &[
            "allow_structural_change",
            "dry_run",
            "edits",
            "new_str",
            "old_str",
            "path",
            "replace_all",
        ],
    )?;

    if let Some(edits) = out.get("edits").cloned() {
        if out.contains_key("old_str") || out.contains_key("new_str") {
            return Err(
                "Error: str_replace edits mode is mutually exclusive with top-level old_str/new_str"
                    .to_string(),
            );
        }
        out.insert("edits".to_string(), normalize_edit_array(&edits)?);
    }

    Ok(Value::Object(out))
}

fn reject_unknown_fields(
    input: &serde_json::Map<String, Value>,
    context: &str,
    allowed: &[&str],
) -> Result<(), String> {
    for key in input.keys() {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "Error: unknown field '{key}' for {context} (valid: {})",
                allowed.join(", ")
            ));
        }
    }
    Ok(())
}

fn normalize_edit_array(value: &Value) -> Result<Value, String> {
    let Some(items) = value.as_array() else {
        return Err("Error: str_replace 'edits' must be an array".to_string());
    };
    let mut normalized = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let Some(edit) = item.as_object() else {
            return Err(format!(
                "Error: str_replace edit[{index}] must be an object"
            ));
        };
        reject_unknown_fields(
            edit,
            &format!("str_replace.edits[{index}]"),
            &["new_str", "old_str", "path"],
        )?;
        normalized.push(Value::Object(edit.clone()));
    }
    Ok(Value::Array(normalized))
}

/// Fast SHA-256 hex digest for concurrent-modification detection.
/// Not security-critical — a content change within the same turn is
/// overwhelmingly a real race, not a crafted collision.
fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    format!("{:x}", hasher.finalize())
}

const TEXT_TRAILING_NEWLINE_EXTS: &[&str] = &[
    "bash", "c", "cc", "cfg", "conf", "cpp", "css", "csv", "go", "h", "hpp", "html", "java", "js",
    "jsx", "json", "jsonl", "kt", "lua", "md", "py", "rb", "rs", "sh", "sql", "toml", "ts", "tsx",
    "txt", "xml", "yaml", "yml", "zsh",
];

const BINARY_WRITE_EXTS: &[&str] = &[
    "bin", "dat", "pdf", "zip", "gz", "tar", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib",
    "o", "a", "lib", "wasm", "class", "pyc", "pyo", "mp3", "mp4", "avi", "mov", "wav", "flac",
    "ogg", "png", "jpg", "jpeg", "gif", "bmp", "webp", "ttf", "otf", "woff", "woff2", "eot",
    "sqlite", "db", "mdb", "ico",
];

/// Common extensionless text files that still want POSIX-style
/// trailing newlines. Matched case-insensitively on the file
/// basename. Without this, files like `Makefile` and `Dockerfile`
/// silently fell off the normalization path because
/// `path.extension()` returns `None` for them.
const TEXT_NEWLINE_BASENAMES: &[&str] = &[
    "makefile",
    "dockerfile",
    "rakefile",
    "gemfile",
    ".gitignore",
    ".gitattributes",
    ".dockerignore",
    ".editorconfig",
    ".env",
    ".bashrc",
    ".zshrc",
    ".profile",
    "license",
    "readme",
    "authors",
    "contributors",
    "changelog",
];

fn extension_lower(path: &Path) -> Option<String> {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
}

fn basename_lower(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
}

fn should_ensure_trailing_newline(path: &Path) -> bool {
    // Extension-based classification is authoritative first: it
    // distinguishes binary (jpg, wasm) from text even when the
    // basename alone would be ambiguous.
    if let Some(ext) = extension_lower(path) {
        if BINARY_WRITE_EXTS.contains(&ext.as_str()) {
            return false;
        }
        if TEXT_TRAILING_NEWLINE_EXTS.contains(&ext.as_str()) {
            return true;
        }
    }
    // Fall back to the basename whitelist for common extensionless
    // text files. This is conservative — we don't infer from content
    // sniffing, so unknown extensions we haven't listed stay
    // unaffected.
    if let Some(base) = basename_lower(path) {
        // Match the full basename (`Makefile`) or the "leading dot"
        // form (`.gitignore`). A file path like `src/Makefile` matches
        // the basename `makefile`.
        if TEXT_NEWLINE_BASENAMES.contains(&base.as_str()) {
            return true;
        }
    }
    false
}

pub fn normalize_content_before_write(path: &Path, content: &str) -> String {
    // Normalize line endings before the trailing-newline check. Files
    // with mixed `\r\n` / `\n` are a cross-platform data-corruption
    // source: the trailing-newline check was only string-matching
    // `\n`, so a `\r\n`-terminated line "fake-passed" and the file
    // went to disk with mixed endings. Downstream diff tooling then
    // shows a spurious "everything changed" view.
    //
    // Rule: on text files we're ensuring a trailing newline for,
    // unify to LF. Binary / unknown files are passed through
    // untouched so we don't mangle content we haven't classified.
    let enforce_newline = should_ensure_trailing_newline(path);
    let mut out = if enforce_newline {
        normalize_line_endings_to_lf(content)
    } else {
        content.to_string()
    };
    if enforce_newline && !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn write_file_desired_state_identity(
    workspace_root: &Path,
    args: &Value,
) -> Option<crate::workspace_observation::WorkspaceFileStateIdentity> {
    let path = resolve_write_target_path(workspace_root, args.get("path")?.as_str()?, "write_file")
        .ok()?;
    let content = normalize_content_before_write(&path, args.get("content")?.as_str()?);
    Some(crate::workspace_observation::workspace_file_state_identity(
        content.as_bytes(),
    ))
}

/// Convert `\r\n` pairs to `\n`. Standalone `\r` (old Mac endings)
/// is also folded to `\n`. Idempotent — `\n\n` stays `\n\n`.
fn normalize_line_endings_to_lf(s: &str) -> String {
    // Fast path: ASCII `\n`-only content is returned without
    // allocation when no `\r` appears.
    if !s.contains('\r') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            // `\r\n` → `\n`. Lone `\r` → `\n`.
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    out
}

enum FormatterOutcome {
    Success,
    NotFound,
    Warning(String),
    SyntaxError(String),
}

fn format_file_in_place_best_effort(path: &Path) -> FormatterOutcome {
    match extension_lower(path).as_deref() {
        Some("rs") => run_formatter(path, "rustfmt", &["--emit=files"]),
        Some("py") => run_formatter(path, "ruff", &["format", "--quiet"]),
        Some("ts" | "tsx" | "js" | "jsx" | "json" | "md" | "yaml" | "yml") => {
            run_formatter(path, "prettier", &["--write", "--log-level=warn"])
        }
        _ => FormatterOutcome::NotFound,
    }
}

/// Atomic write pipeline for edits + formatter: stage content in a
/// sibling tmp file, run the best-effort formatter on the tmp, then
/// rename tmp over the real path. The real path never observes a
/// half-written or half-formatted state.
///
/// Returns a formatter warning string when the formatter reported a
/// non-fatal error. Syntax-level formatter failures abort the write:
/// returning success while writing syntactically broken code gives the
/// caller a false signal that the edit is valid.
///
/// Pre-commit hooks / editors watching the target via inotify see
/// **one** `MODIFY` event (the rename), not `CREATE` + partial
/// writes during formatting.
fn sha256_digest_of_existing_file(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let meta = file.metadata().ok()?;
    if !meta.is_file() {
        return None;
    }
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return None,
        }
    }
    Some(format!("{:x}", hasher.finalize()))
}

fn verify_expected_original_hash(
    path: &Path,
    expected_original_hash: Option<&str>,
) -> Result<(), String> {
    let Some(expected) = expected_original_hash else {
        return Ok(());
    };
    let Some(current) = sha256_digest_of_existing_file(path) else {
        return Err(format!(
            "Error: Cannot verify that file {path} is unchanged before commit. Re-read the file content and retry.",
            path = path.display()
        ));
    };
    if current != expected {
        return Err(format!(
            "Error: File {path} was modified since it was read (hash mismatch). \
             Expected {expected}, found {current}. Re-read the file content and retry.",
            path = path.display()
        ));
    }
    Ok(())
}

fn write_file_atomic_with_format(
    path: &Path,
    content: &[u8],
    allow_formatter_syntax_error: bool,
    expected_original_hash: Option<&str>,
    format_staging: bool,
) -> Result<Option<String>, String> {
    // Verify the file hasn't been modified since we read it.
    verify_expected_original_hash(path, expected_original_hash)?;

    // Staging file lives next to the target — POSIX rename() is
    // only atomic within the same filesystem. Using a /tmp staging
    // file would break across mount points.
    let tmp = staging_tmp_path(path);

    // Best-effort cleanup of a stale tmp from a prior crashed write.
    let _ = std::fs::remove_file(&tmp);

    if let Err(e) = std::fs::write(&tmp, content) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Error: Cannot stage write: {e}"));
    }

    let formatter_outcome = if format_staging {
        format_file_in_place_best_effort(&tmp)
    } else {
        FormatterOutcome::NotFound
    };
    let warning = match formatter_outcome {
        FormatterOutcome::Success | FormatterOutcome::NotFound => None,
        FormatterOutcome::Warning(warning) => Some(warning),
        FormatterOutcome::SyntaxError(error) => {
            if allow_formatter_syntax_error {
                Some(error)
            } else {
                let _ = std::fs::remove_file(&tmp);
                return Err(error);
            }
        }
    };

    // Formatters are part of the write contract. They may normalize the
    // staged candidate back to the exact bytes already on disk; do not rename
    // such a candidate or report a mutation merely because the pre-format
    // string differed.
    match (std::fs::read(&tmp), std::fs::read(path)) {
        (Ok(staged), Ok(current)) if staged == current => {
            let _ = std::fs::remove_file(&tmp);
            return Err(
                "Error: the final formatted content is unchanged; no bytes were written."
                    .to_string(),
            );
        }
        (Err(error), _) => {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("Error: Cannot verify staged write: {error}"));
        }
        (_, Err(error)) if error.kind() != std::io::ErrorKind::NotFound => {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!(
                "Error: Cannot verify existing content before commit: {error}"
            ));
        }
        _ => {}
    }

    // Atomic rename commits the final state. Only here does the
    // target path change.
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("Error: Cannot commit write (rename failed): {e}"));
    }

    Ok(warning)
}

fn staging_tmp_path(target: &Path) -> PathBuf {
    let pid = std::process::id();
    // Timestamp via nanos reduces the chance of collision when
    // multiple writers race on the same path in the same pid (e.g.
    // tests).
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut tmp = target.to_path_buf();
    let base = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("astra_write");
    // Preserve the original EXTENSION at the tail so external
    // formatters (rustfmt, prettier, ruff) recognize the staged file
    // as the right language. Pattern: `.astra-tmp.<pid>.<nanos>.<basename>`.
    // The leading dot keeps it hidden; the original extension at
    // tail drives formatter detection.
    tmp.set_file_name(format!(".astra-tmp.{pid}.{nanos}.{base}"));
    tmp
}

fn run_formatter(path: &Path, program: &str, args: &[&str]) -> FormatterOutcome {
    let mut command = std::process::Command::new(program);
    command.args(args).arg(path);
    match command.output() {
        Ok(output) if output.status.success() => FormatterOutcome::Success,
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            let diagnostic = format!(
                "{program} failed for {}: {}{}",
                path.display(),
                stdout.trim(),
                stderr.trim()
            );
            if formatter_failure_is_syntax_error(&diagnostic) {
                FormatterOutcome::SyntaxError(format!("Error: SYNTAX ERROR: {diagnostic}"))
            } else {
                FormatterOutcome::Warning(diagnostic)
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FormatterOutcome::NotFound,
        Err(e) => {
            FormatterOutcome::Warning(format!("{program} failed for {}: {e}", path.display()))
        }
    }
}

fn formatter_failure_is_syntax_error(diagnostic: &str) -> bool {
    let lower = diagnostic.to_ascii_lowercase();
    [
        "syntax",
        "parse error",
        "failed to parse",
        "unterminated",
        "unclosed",
        "mismatched",
        "unexpected",
        "expected",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn validate_structural_edit(
    path: &Path,
    original: &str,
    updated: &str,
    old_str: &str,
    new_str: &str,
) -> Result<(), String> {
    let Some(lang) = crate::code_intel::detect_language(path) else {
        return Ok(());
    };

    if comment_line_count(new_str) < comment_line_count(old_str) {
        return Err(format!(
            "Error: structural validation rejected edit to {}: replacement removes comment/doc-comment lines. If this is intentional, retry with allow_structural_change=true.",
            path.display()
        ));
    }

    let Some(original_has_error) = tree_sitter_has_error(original, lang) else {
        return Ok(());
    };
    if original_has_error {
        return Ok(());
    }
    if tree_sitter_has_error(updated, lang).unwrap_or(false) {
        return Err(format!(
            "Error: structural validation rejected edit to {}: updated file has syntax errors. If this is intentional, retry with allow_structural_change=true.",
            path.display()
        ));
    }
    Ok(())
}

/// Count lines that look like **documentation** (not ordinary code
/// comments). Intentionally conservative — the caller uses this to
/// flag edits that strip out documentation the author likely wants
/// to keep. The old implementation also matched any `#`-prefixed
/// line, which mistakenly fired on Python `#!/usr/bin/env python`
/// shebangs, `# coding: utf-8` pragmas, Python commented-out code,
/// shell scripts, TOML section headers (`[deps]` is obviously not a
/// comment but `# comment` in toml looks like one), etc. — too broad
/// to be signal, so edits that intentionally refactored out dead `#`
/// lines got rejected.
///
/// New rule: only count doc-comments and multi-line
/// doc blocks — the stuff an IDE hover or rustdoc/jsdoc picks up.
fn comment_line_count(s: &str) -> usize {
    s.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            // Rust doc comments.
            trimmed.starts_with("///")
                || trimmed.starts_with("//!")
                // JSDoc / Doxygen / Rust module-level block doc.
                || trimmed.starts_with("/**")
                || trimmed.starts_with("/*!")
                // Continuation lines of a block doc comment ("* …").
                // Scoped enough to not collide with plain prose or
                // markdown bullets (those are typically `-` / `+`
                // inside code here, not ` * `).
                || trimmed.starts_with("* ")
                || trimmed == "*/"
                // Python / TypeScript / Markdown triple-quoted
                // docstring delimiters. One-sided match is OK for a
                // loss count; the caller just compares old vs new.
                || trimmed.starts_with("\"\"\"")
                || trimmed.starts_with("'''")
        })
        .count()
}

fn tree_sitter_has_error(source: &str, lang: crate::code_intel::Language) -> Option<bool> {
    let mut parser = tree_sitter::Parser::new();
    let language = match lang {
        crate::code_intel::Language::Rust => tree_sitter_rust::LANGUAGE.into(),
        crate::code_intel::Language::Python => tree_sitter_python::LANGUAGE.into(),
        crate::code_intel::Language::TypeScript | crate::code_intel::Language::JavaScript => {
            tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
        }
        crate::code_intel::Language::Go => tree_sitter_go::LANGUAGE.into(),
        crate::code_intel::Language::Java => tree_sitter_java::LANGUAGE.into(),
        crate::code_intel::Language::C | crate::code_intel::Language::Cpp => {
            tree_sitter_cpp::LANGUAGE.into()
        }
        crate::code_intel::Language::Ruby => tree_sitter_ruby::LANGUAGE.into(),
    };
    parser.set_language(&language).ok()?;
    parser
        .parse(source, None)
        .map(|tree| tree.root_node().has_error())
}

/// Build a structured error message for failed str_replace lookups.
///
/// Output is intentionally compact: WHAT/WHY/NEXT banner plus structured
/// boolean signals (whitespace_normalized_match, first_line_at_lines,
/// individual_line_match_ratio). It does NOT echo file content — the prior
/// `read_file` tool_result is still in the prompt and is the source of truth.
/// Echoing nearby lines wastes tokens, breaks prompt-cache prefix matching
/// (the echoed window depends on per-call old_str), and encourages the model
/// to retry by re-emitting the full new_str instead of fixing the anchor.
fn str_replace_not_found_hint(path_str: &str, content: &str, old_str: &str) -> String {
    str_replace_not_found_hint_with_what(
        format!("old_str not found in {path_str}."),
        content,
        old_str,
    )
}

fn str_replace_not_found_hint_for_edit(
    path_str: &str,
    content: &str,
    old_str: &str,
    edit_index: usize,
) -> String {
    str_replace_not_found_hint_with_what(
        format!("edit[{edit_index}] old_str not found in {path_str}."),
        content,
        old_str,
    )
}

fn str_replace_not_found_hint_with_what(what: String, content: &str, old_str: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_str.lines().collect();
    let mut msg = str_replace_fail(
        &what,
        "The exact byte sequence does not appear in the current file content (whitespace, indentation, or quote style may differ; or the file changed since you last read it).",
        "Do NOT blindly retry with the same old_str. Re-read the file first with read_file (targeted range if the file is large), then copy the exact current bytes into old_str and retry str_replace.",
    );
    msg.push('\n');

    let normalized_old = normalize_ws(old_str);
    let normalized_content = normalize_ws(content);
    if normalized_content.contains(&normalized_old) {
        msg.push_str("whitespace_normalized_match: true (check indentation/trailing whitespace)\n");
        if let Some(first_line) = old_lines.first() {
            let normalized_first = normalize_ws(first_line);
            for (idx, line) in lines.iter().enumerate() {
                if normalize_ws(line) == normalized_first {
                    msg.push_str(&format!("first_line_at: L{}\n", idx + 1));
                    break;
                }
            }
        }
        return msg;
    }

    let mut has_specific_hint = false;
    if let Some(first_line) = old_lines.first() {
        let needle = first_line.trim();
        if !needle.is_empty() {
            let mut matches = Vec::new();
            for (idx, line) in lines.iter().enumerate() {
                if line.trim() == needle || line.contains(needle) {
                    matches.push(idx + 1);
                    if matches.len() >= 5 {
                        break;
                    }
                }
            }
            if !matches.is_empty() {
                has_specific_hint = true;
                msg.push_str(&format!("first_line_at: {matches:?}\n"));
            }
        }
    }

    if old_lines.len() > 1 {
        let file_line_set: std::collections::HashSet<&str> =
            lines.iter().map(|l| l.trim()).collect();
        let matching_count = old_lines
            .iter()
            .filter(|old_line| {
                let trimmed = old_line.trim();
                !trimmed.is_empty() && file_line_set.contains(trimmed)
            })
            .count();
        if matching_count > 0 {
            has_specific_hint = true;
            msg.push_str(&format!(
                "individual_line_match_ratio: {matching_count}/{}\n",
                old_lines.len()
            ));
        }
    }

    if !has_specific_hint {
        msg.push_str("no_partial_match: true (old_str doesn't appear under any normalization)\n");
    }
    msg
}

const LCS_LINE_LIMIT: usize = 4000;

fn unified_diff(old_content: &str, new_content: &str, path_str: &str) -> String {
    let filename = Path::new(path_str)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();

    if old_lines.len().max(new_lines.len()) > LCS_LINE_LIMIT {
        return unified_diff_simple(old_content, new_content, &filename);
    }

    let ops = lcs_diff(&old_lines, &new_lines);
    if ops.is_empty() || ops.iter().all(|op| matches!(op, DiffOp::Equal(..))) {
        return format!(
            "[DRY RUN] Preview of changes (not applied):\n--- a/{filename}\n+++ b/{filename}\n(no changes)\n"
        );
    }

    let hunks = group_into_hunks(&ops, 3);
    let mut diff = format!("--- a/{filename}\n+++ b/{filename}\n");
    for hunk in &hunks {
        let mut old_start = usize::MAX;
        let mut old_count = 0;
        let mut new_start = usize::MAX;
        let mut new_count = 0;
        for op in hunk {
            match op {
                DiffOp::Equal(o, n, _) => {
                    old_start = old_start.min(*o);
                    new_start = new_start.min(*n);
                    old_count += 1;
                    new_count += 1;
                }
                DiffOp::Delete(o, _) => {
                    old_start = old_start.min(*o);
                    old_count += 1;
                }
                DiffOp::Insert(n, _) => {
                    new_start = new_start.min(*n);
                    new_count += 1;
                }
            }
        }
        if old_start == usize::MAX {
            old_start = 0;
        }
        if new_start == usize::MAX {
            new_start = 0;
        }
        diff.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            old_start + 1,
            old_count,
            new_start + 1,
            new_count,
        ));
        for op in hunk {
            match op {
                DiffOp::Equal(_, _, line) => diff.push_str(&format!(" {line}\n")),
                DiffOp::Delete(_, line) => diff.push_str(&format!("-{line}\n")),
                DiffOp::Insert(_, line) => diff.push_str(&format!("+{line}\n")),
            }
        }
    }
    format!("[DRY RUN] Preview of changes (not applied):\n{diff}")
}

#[derive(Debug, Clone, Copy)]
enum DiffOp<'a> {
    Equal(usize, usize, &'a str),
    Delete(usize, &'a str),
    Insert(usize, &'a str),
}

fn lcs_diff<'a>(old: &[&'a str], new: &[&'a str]) -> Vec<DiffOp<'a>> {
    let m = old.len();
    let n = new.len();
    // LCS table
    let mut table = vec![vec![0u32; n + 1]; m + 1];
    for i in (0..m).rev() {
        for j in (0..n).rev() {
            table[i][j] = if old[i] == new[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut raw = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < m || j < n {
        if i < m && j < n && old[i] == new[j] {
            raw.push(DiffOp::Equal(i, j, old[i]));
            i += 1;
            j += 1;
        } else if i < m && (j >= n || table[i + 1][j] >= table[i][j + 1]) {
            raw.push(DiffOp::Delete(i, old[i]));
            i += 1;
        } else {
            raw.push(DiffOp::Insert(j, new[j]));
            j += 1;
        }
    }
    // Reorder runs of non-equal ops: deletes before inserts (standard diff convention)
    let mut ops = Vec::with_capacity(raw.len());
    let mut idx = 0;
    while idx < raw.len() {
        if matches!(raw[idx], DiffOp::Equal(..)) {
            ops.push(raw[idx]);
            idx += 1;
        } else {
            let run_start = idx;
            while idx < raw.len() && !matches!(raw[idx], DiffOp::Equal(..)) {
                idx += 1;
            }
            for op in &raw[run_start..idx] {
                if matches!(op, DiffOp::Delete(..)) {
                    ops.push(*op);
                }
            }
            for op in &raw[run_start..idx] {
                if matches!(op, DiffOp::Insert(..)) {
                    ops.push(*op);
                }
            }
        }
    }
    ops
}

fn group_into_hunks<'a>(ops: &[DiffOp<'a>], context: usize) -> Vec<Vec<DiffOp<'a>>> {
    let mut hunks: Vec<Vec<DiffOp<'a>>> = Vec::new();
    let mut change_indices: Vec<usize> = Vec::new();
    for (idx, op) in ops.iter().enumerate() {
        if !matches!(op, DiffOp::Equal(..)) {
            change_indices.push(idx);
        }
    }
    if change_indices.is_empty() {
        return hunks;
    }
    let mut hunk_start = change_indices[0].saturating_sub(context);
    let mut hunk_end = (change_indices[0] + context + 1).min(ops.len());
    for &ci in &change_indices[1..] {
        let cs = ci.saturating_sub(context);
        let ce = (ci + context + 1).min(ops.len());
        if cs <= hunk_end {
            hunk_end = ce;
        } else {
            hunks.push(ops[hunk_start..hunk_end].to_vec());
            hunk_start = cs;
            hunk_end = ce;
        }
    }
    hunks.push(ops[hunk_start..hunk_end].to_vec());
    hunks
}

fn unified_diff_simple(old_content: &str, new_content: &str, filename: &str) -> String {
    let old_lines: Vec<&str> = old_content.lines().collect();
    let new_lines: Vec<&str> = new_content.lines().collect();
    let max_len = old_lines.len().max(new_lines.len());
    let mut first_diff = max_len;
    let mut last_diff = 0;
    for idx in 0..max_len {
        let o = old_lines.get(idx).copied().unwrap_or("");
        let n = new_lines.get(idx).copied().unwrap_or("");
        if o != n {
            first_diff = first_diff.min(idx);
            last_diff = idx;
        }
    }
    let mut diff = format!("--- a/{filename}\n+++ b/{filename}\n");
    if first_diff > last_diff {
        return format!("[DRY RUN] Preview of changes (not applied):\n{diff}(no changes)\n");
    }
    let context = 3;
    let start = first_diff.saturating_sub(context);
    let end = (last_diff + context + 1).min(max_len);
    diff.push_str(&format!(
        "@@ -{},{} +{},{} @@\n",
        start + 1,
        end.min(old_lines.len()).saturating_sub(start),
        start + 1,
        end.min(new_lines.len()).saturating_sub(start),
    ));
    let mut idx = start;
    while idx < end {
        let o = old_lines.get(idx).copied();
        let n = new_lines.get(idx).copied();
        match (o, n) {
            (Some(a), Some(b)) if a == b => {
                diff.push_str(&format!(" {a}\n"));
                idx += 1;
            }
            _ => {
                let run_start = idx;
                while idx < end {
                    let a = old_lines.get(idx).copied();
                    let b = new_lines.get(idx).copied();
                    if matches!((a, b), (Some(x), Some(y)) if x == y) {
                        break;
                    }
                    idx += 1;
                }
                for i in run_start..idx {
                    if let Some(line) = old_lines.get(i) {
                        diff.push_str(&format!("-{line}\n"));
                    }
                }
                for i in run_start..idx {
                    if let Some(line) = new_lines.get(i) {
                        diff.push_str(&format!("+{line}\n"));
                    }
                }
            }
        }
    }
    format!("[DRY RUN] Preview of changes (not applied):\n{diff}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn format_file_size_mb_shows_fractional_for_small_files() {
        // 500 KB → should show "0.5 MB", NOT "0 MB" (integer division bug)
        assert_eq!(format_file_size_mb(500 * 1024), "0.5 MB");
        // 1 byte over 10MB → should show "10.0 MB", NOT "10 MB"
        assert_eq!(format_file_size_mb(10 * 1024 * 1024 + 1), "10.0 MB");
        // exactly 10MB
        assert_eq!(format_file_size_mb(10 * 1024 * 1024), "10.0 MB");
        // 0 bytes
        assert_eq!(format_file_size_mb(0), "0.0 MB");
        // 15.3 MB
        assert_eq!(format_file_size_mb(15 * 1024 * 1024 + 307_200), "15.3 MB");
    }

    #[test]
    fn read_file_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "line1\nline2\nline3").unwrap();
        let args = serde_json::json!({"path": "test.txt"});
        let result = read_file(tmp.path(), &args);
        assert!(!result.is_error);
        assert!(result.output.contains("1\tline1"));
        assert!(result.output.contains("3\tline3"));
    }

    #[test]
    fn read_file_rejects_internal_tool_result_artifacts() {
        let tmp = TempDir::new().unwrap();
        let artifact = tmp
            .path()
            .join(".astra/sessions/session-1/tool-results/call_abc.txt");
        std::fs::create_dir_all(artifact.parent().unwrap()).unwrap();
        std::fs::write(&artifact, "child output").unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": ".astra/sessions/session-1/tool-results/call_abc.txt"}),
        );

        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("runtime-owned tool-result artifacts"),
            "{}",
            result.output
        );
        assert!(result.output.contains("agent_fanout(action='get_results'"));
    }

    #[test]
    fn read_file_rejects_unknown_fields_before_missing_path() {
        let tmp = TempDir::new().unwrap();
        let result = read_file(
            tmp.path(),
            &serde_json::json!({
                "file": "test.txt",
                "start_line": 1,
                "end_line": 300
            }),
        );

        assert!(result.is_error);
        assert!(
            result.output.contains("unknown field `file`"),
            "{:?}",
            result.output
        );
        assert!(
            result.output.contains("Valid fields: path"),
            "{:?}",
            result.output
        );
        assert!(
            result.output.contains("Use `path` for the file path"),
            "{:?}",
            result.output
        );
        assert!(
            !result.output.contains("Missing 'path'"),
            "unknown-field contract should fire before legacy missing-path text: {}",
            result.output
        );
    }

    #[test]
    fn read_file_missing_path_returns_structured_canonical_candidates() {
        let tmp = TempDir::new().unwrap();
        let current = tmp.path().join("crates/runtime/src/server");
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(current.join("header_utils.rs"), "pub fn header() {}\n").unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "old/workspace/src/server/header_utils.rs"}),
        );

        assert!(result.is_error);
        assert!(
            result.output.contains("PATH_RESOLUTION_FAILED"),
            "{}",
            result.output
        );
        assert!(result.output.contains("No file operation was performed"));
        assert!(
            result
                .output
                .contains("crates/runtime/src/server/header_utils.rs"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("Cannot read file"),
            "missing path should fail before OS read: {}",
            result.output
        );
    }

    #[test]
    fn read_file_with_range() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\nd").unwrap();
        let args = serde_json::json!({"path": "test.txt", "start_line": 2, "end_line": 3});
        let result = read_file(tmp.path(), &args);
        assert!(result.output.contains("2\tb"));
        assert!(result.output.contains("3\tc"));
        assert!(!result.output.contains("1\ta"));
    }

    #[test]
    fn read_file_outline_returns_signatures() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("lib.rs"),
            "pub struct User;\n\npub fn parse() {}\nfn helper() {}\n",
        )
        .unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "lib.rs", "outline": true}),
        );

        assert!(!result.is_error);
        assert!(
            result.output.contains("# Outline"),
            "got: {}",
            result.output
        );
        assert!(result.output.contains("parse"), "got: {}", result.output);
        assert!(result.output.contains("User"), "got: {}", result.output);
    }

    #[test]
    fn read_file_outline_without_definitions_returns_fallback_message() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("notes.txt"),
            "plain text\nstill plain text\n",
        )
        .unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "notes.txt", "outline": true}),
        );

        assert!(!result.is_error);
        assert!(
            result.output.contains("no definitions found"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_auto_paginates_large_files() {
        let tmp = TempDir::new().unwrap();
        // Create file larger than READ_FILE_SIZE_LIMIT (80KB) with 3000 lines
        let mut large = String::new();
        for i in 1..=3000 {
            large.push_str(&format!(
                "Line {}: Some content here to make the file larger\n",
                i
            ));
        }
        std::fs::write(tmp.path().join("big.txt"), &large).unwrap();

        let result = read_file(tmp.path(), &serde_json::json!({"path": "big.txt"}));

        // Should NOT be an error anymore - returns preview instead
        assert!(!result.is_error, "got error: {}", result.output);
        // Should contain preview header
        assert!(
            result.output.contains("Large file preview"),
            "got: {}",
            result.output
        );
        // Should have first lines section
        assert!(
            result.output.contains("First lines"),
            "got: {}",
            result.output
        );
        // Should have line 1 content
        assert!(result.output.contains("Line 1:"), "got: {}", result.output);
        // Should mention omitted lines
        assert!(
            result.output.contains("lines omitted"),
            "got: {}",
            result.output
        );
        // Should have last lines section
        assert!(
            result.output.contains("Last lines"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("start_line"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_rejects_extremely_large_files() {
        let tmp = TempDir::new().unwrap();
        let big_path = tmp.path().join("huge.txt");
        // Create a file just over the 10MB hard limit using sparse write.
        let f = std::fs::File::create(&big_path).unwrap();
        f.set_len((10 * 1024 * 1024 + 1) as u64).unwrap();

        let result = read_file(tmp.path(), &serde_json::json!({"path": "huge.txt"}));
        assert!(result.is_error, "expected error, got: {}", result.output);
        assert!(
            result.output.contains("too large"),
            "got: {}",
            result.output
        );
        let evidence = result
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.get("recovery_evidence"))
            .cloned()
            .and_then(|value| serde_json::from_value::<astra_core::ToolFailureEvidence>(value).ok())
            .expect("large-file rejection must preserve structured recovery evidence");
        assert_eq!(evidence.cause, astra_core::ToolFailureCause::InputTooLarge);
        assert_eq!(
            evidence.recovery_actions,
            vec![
                astra_core::ToolRecoveryAction::ReadTargetedRange,
                astra_core::ToolRecoveryAction::SearchBeforeRead,
                astra_core::ToolRecoveryAction::NarrowScope,
            ]
        );
    }

    #[test]
    fn read_last_n_lines_preserves_multibyte_characters() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("utf8-tail.txt");

        let prefix = "头\n";
        let body = "你".repeat(4000);
        let mut pad = String::new();
        let suffix = loop {
            let candidate = format!("\npad{pad}\n倒数第二行\n最终行");
            let total_len = prefix.len() + body.len() + candidate.len();
            let seek_pos = total_len.saturating_sub(8192);
            let body_start = prefix.len();
            let body_end = body_start + body.len();
            if total_len > 8192
                && seek_pos > body_start
                && seek_pos < body_end
                && !(seek_pos - body_start).is_multiple_of(3)
            {
                break candidate;
            }
            pad.push('x');
        };

        std::fs::write(&path, format!("{prefix}{body}{suffix}")).unwrap();

        let lines = read_last_n_lines(&path, 2).unwrap();
        assert_eq!(lines, vec!["倒数第二行".to_string(), "最终行".to_string()]);
    }

    #[test]
    fn read_last_n_lines_few_lines_avoids_mid_utf8_start() {
        // Edge case: file has fewer lines than n, so start == 0 but seek_pos
        // may land mid-UTF-8. Verify the safe_start scan produces valid output.
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("few-cjk.txt");
        // Two lines of CJK text — requesting 10 last lines (more than exist).
        std::fs::write(&path, "第一行内容\n第二行内容").unwrap();
        let lines = read_last_n_lines(&path, 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "第一行内容");
        assert_eq!(lines[1], "第二行内容");
        // No `` replacement characters from mid-UTF-8 boundary corruption.
        assert!(!lines.iter().any(|l| l.contains('\u{FFFD}')));
    }

    #[test]
    fn render_outline_returns_none_when_no_definitions() {
        assert_eq!(
            render_outline(Path::new("notes.txt"), "plain text\n", 1),
            None
        );
    }

    #[test]
    fn read_file_rejects_binary_extensions() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("data.db"), b"\0\0\0sqlite").unwrap();

        let result = read_file(tmp.path(), &serde_json::json!({"path": "data.db"}));

        assert!(result.is_error);
        assert!(
            result.output.contains("binary file"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_returns_image_data_uri() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("tiny.png"),
            [0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'],
        )
        .unwrap();

        let result = read_file(tmp.path(), &serde_json::json!({"path": "tiny.png"}));

        assert!(!result.is_error);
        assert!(
            result.output.starts_with("data:image/png;base64,"),
            "got: {}",
            result.output
        );
    }

    // --- Bug #9: large image base64 must be capped by output limit ---
    #[test]
    fn read_file_image_base64_is_capped() {
        let tmp = TempDir::new().unwrap();
        // Create a ~1MB fake PNG (just PNG header + random data)
        let mut data = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
        data.extend(vec![0xAB; 1_000_000]); // ~1MB body
        std::fs::write(tmp.path().join("big.png"), &data).unwrap();

        let result = read_file(tmp.path(), &serde_json::json!({"path": "big.png"}));

        assert!(!result.is_error);
        assert!(result.output.starts_with("data:image/png;base64,"));
        // The output must be capped — raw base64 of 1MB is ~1.33MB chars.
        // per_tool_output_limit() caps it (typically 80-200KB).
        let limit = per_tool_output_limit("read_file");
        assert!(
            result.output.len() <= limit + 200, // small tolerance for prefix
            "Image base64 output {} should be capped at ~{limit}",
            result.output.len()
        );
    }

    // Supplementary: truncated image output contains truncation marker
    #[test]
    fn read_file_image_truncated_has_marker() {
        let tmp = TempDir::new().unwrap();
        let mut data = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1A, b'\n'];
        data.extend(vec![0xAB; 1_000_000]);
        std::fs::write(tmp.path().join("big.png"), &data).unwrap();

        let result = read_file(tmp.path(), &serde_json::json!({"path": "big.png"}));

        assert!(!result.is_error);
        let limit = per_tool_output_limit("read_file");
        // If the image is larger than limit, it must contain a truncation indicator
        if result.output.len() < 1_300_000 {
            // Was truncated — verify marker or that it's shorter than raw base64
            assert!(
                result.output.contains("truncated")
                    || result.output.contains("…")
                    || result.output.len() <= limit + 200,
                "Truncated image should have marker or be within limit, len={}",
                result.output.len()
            );
        }
    }

    #[test]
    fn read_file_inclusive_range_reads_correct_lines() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\nd").unwrap();

        // start_line=2, end_line=3 → lines 2-3
        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "test.txt", "start_line": 2, "end_line": 3}),
        );

        assert!(
            !result.is_error,
            "expected success, got error: {}",
            result.output
        );
        assert!(
            result.output.contains("b") && result.output.contains("c"),
            "expected lines 2-3, got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_large_inclusive_range() {
        let tmp = TempDir::new().unwrap();
        let mut content = String::new();
        for line in 1..=3_200 {
            content.push_str(&format!("line {line}\n"));
        }
        std::fs::write(tmp.path().join("tool-result.txt"), content).unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "tool-result.txt", "start_line": 300, "end_line": 799}),
        );

        assert!(
            !result.is_error,
            "expected success, got error: {}",
            result.output
        );
        assert!(
            result.output.contains("line 300"),
            "expected first requested line, got: {}",
            result.output
        );
        assert!(
            result.output.contains("line 799"),
            "expected final requested line, got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_start_line_beyond_file_gives_error() {
        let tmp = TempDir::new().unwrap();
        let json = serde_json::json!({
            "status": "completed",
            "results": [
                {"slot_index": 0, "result": {"summary": "first review"}},
                {"slot_index": 1, "result": {"summary": "second review"}}
            ]
        })
        .to_string();
        std::fs::write(tmp.path().join("fanout-result.txt"), json).unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({
                "path": "fanout-result.txt",
                "start_line": 300,
                "end_line": 309
            }),
        );

        // line 300 on a single-line file exceeds the file length.
        assert!(result.is_error, "got: {}", result.output);
        assert!(
            result.output.contains("exceeds"),
            "expected range-exceeds-file error, got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_rejects_invalid_optional_arg_types() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\nd").unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "test.txt", "start_line": "2"}),
        );
        assert!(result.is_error);
        assert!(
            result.output.contains("`start_line`") && result.output.contains("positive integer"),
            "got: {}",
            result.output
        );

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "test.txt", "outline": 1}),
        );
        assert!(result.is_error);
        assert!(
            result.output.contains("`outline`") && result.output.contains("boolean"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn read_file_zero_end_line_explains_how_to_read_to_end() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\n").unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "test.txt", "start_line": 2, "end_line": 0}),
        );

        assert!(result.is_error);
        assert!(result.output.contains("`end_line`"), "{}", result.output);
        assert!(
            result.output.contains("Omit `end_line`"),
            "{}",
            result.output
        );
    }

    #[test]
    fn write_and_read() {
        let tmp = TempDir::new().unwrap();
        let w_args = serde_json::json!({"path": "new.txt", "content": "hello world"});
        let w_result = write_file(tmp.path(), &w_args);
        assert!(!w_result.is_error);

        let r_args = serde_json::json!({"path": "new.txt"});
        let r_result = read_file(tmp.path(), &r_args);
        assert!(r_result.output.contains("hello world"));
    }

    #[test]
    fn write_file_text_extensions_get_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let args = serde_json::json!({"path": "permissions.json", "content": "{\"allow\":[]}"});

        let result = write_file(tmp.path(), &args);

        assert!(!result.is_error, "got error: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("permissions.json")).unwrap();
        assert_eq!(content, "{\"allow\":[]}\n");
    }

    #[test]
    fn write_file_binary_extensions_do_not_get_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        let args = serde_json::json!({"path": "payload.bin", "content": "abc"});

        let result = write_file(tmp.path(), &args);

        assert!(!result.is_error, "got error: {}", result.output);
        let content = std::fs::read(tmp.path().join("payload.bin")).unwrap();
        assert_eq!(content, b"abc");
    }

    #[test]
    fn write_file_rust_runs_rustfmt_best_effort() {
        let tmp = TempDir::new().unwrap();
        let args =
            serde_json::json!({"path": "main.rs", "content": "fn main(){println!(\"hi\");}"});

        let result = write_file(tmp.path(), &args);

        assert!(!result.is_error, "got error: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("main.rs")).unwrap();
        assert_eq!(content, "fn main() {\n    println!(\"hi\");\n}\n");
    }

    #[test]
    fn write_file_rust_syntax_formatter_failure_is_error_and_preserves_target() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("main.rs");
        std::fs::write(&target, "fn main() {}\n").unwrap();
        let args = serde_json::json!({
            "path": "main.rs",
            "content": "fn main(){ println!(\"hi); }"
        });

        let result = write_file(tmp.path(), &args);

        assert!(
            result.is_error,
            "syntax failure must be an error: {}",
            result.output
        );
        assert!(
            result.output.contains("SYNTAX ERROR"),
            "syntax failure must be prominent: {}",
            result.output
        );
        let content = std::fs::read_to_string(target).unwrap();
        assert_eq!(content, "fn main() {}\n");
    }

    #[test]
    fn prepare_write_file_missing_path_guides_retry_same_tool() {
        let tmp = TempDir::new().unwrap();
        let err = prepare_write_file(tmp.path(), &serde_json::json!({"content": "hello"}))
            .expect_err("missing path should fail");
        assert!(err.is_error);
        assert!(err.output.contains("Missing 'path' parameter"));
        assert!(err.output.contains("Retry write_file"));
        assert!(err.output.contains("path and content"));
        assert!(
            !err.output.contains("delete=true"),
            "path-missing error must not suggest delete=true (write-only path)"
        );
        assert!(err.output.contains("Do not switch to bash"));
    }

    #[test]
    fn prepare_write_file_missing_content_guides_retry_same_tool() {
        let tmp = TempDir::new().unwrap();
        let err = prepare_write_file(tmp.path(), &serde_json::json!({"path": "note.txt"}))
            .expect_err("missing content should fail");
        assert!(err.is_error);
        assert!(err.output.contains("Missing 'content' parameter"));
        assert!(err.output.contains("Retry write_file"));
        assert!(err.output.contains("path and content"));
        // The delete=true hint is intentionally absent: if delete=true is present,
        // the executor routes to delete_file before prepare_write_file is called,
        // so this error only fires when delete is definitely NOT true.
        assert!(
            !err.output.contains("set delete=true"),
            "content-missing error must not suggest delete=true (unreachable path)"
        );
        assert!(err.output.contains("Do not switch to bash"));
    }

    #[test]
    fn prepare_write_file_rejects_unknown_fields() {
        let tmp = TempDir::new().unwrap();
        let err = prepare_write_file(
            tmp.path(),
            &serde_json::json!({
                "path": "note.txt",
                "content": "hello",
                "mode": "append"
            }),
        )
        .expect_err("unknown fields should fail");

        assert!(err.is_error);
        assert!(err.output.contains("unknown field 'mode'"));
        assert!(err.output.contains("write_file"));
        assert!(
            !tmp.path().join("note.txt").exists(),
            "unknown fields must fail before writing"
        );
    }

    #[test]
    fn write_file_rejects_missing_parent_instead_of_creating_stale_path_tree() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("crates/runtime/src/server")).unwrap();
        std::fs::write(
            tmp.path().join("crates/runtime/src/server/header_utils.rs"),
            "current",
        )
        .unwrap();

        let result = write_file(
            tmp.path(),
            &serde_json::json!({
                "path": "old/workspace/src/server/header_utils.rs",
                "content": "stale"
            }),
        );

        assert!(result.is_error);
        assert!(
            result.output.contains("PATH_RESOLUTION_FAILED"),
            "{}",
            result.output
        );
        assert!(
            result
                .output
                .contains("crates/runtime/src/server/header_utils.rs"),
            "{}",
            result.output
        );
        assert!(
            !tmp.path().join("old").exists(),
            "write_file must not create a missing parent tree for stale paths"
        );
    }

    #[test]
    fn str_replace_missing_path_reports_own_tool_name() {
        let tmp = TempDir::new().unwrap();
        let result = str_replace(
            tmp.path(),
            &serde_json::json!({
                "path": "missing/file.txt",
                "old_str": "alpha",
                "new_str": "beta"
            }),
        );

        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("PATH_RESOLUTION_FAILED: str_replace target"),
            "{}",
            result.output
        );
    }

    #[test]
    fn multi_edit_missing_path_reports_own_tool_name() {
        let tmp = TempDir::new().unwrap();
        let result = multi_edit(
            tmp.path(),
            &serde_json::json!({
                "path": "missing/file.txt",
                "edits": [
                    {"old_str": "alpha", "new_str": "beta"}
                ]
            }),
        );

        assert!(result.is_error);
        assert!(
            result
                .output
                .contains("PATH_RESOLUTION_FAILED: multi_edit target"),
            "{}",
            result.output
        );
    }

    #[test]
    fn str_replace_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "foo bar baz").unwrap();
        let args = serde_json::json!({"path": "f.txt", "old_str": "bar", "new_str": "qux"});
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "foo qux baz\n");
    }

    #[test]
    fn str_replace_rejects_noop_after_line_ending_normalization() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let before = std::fs::metadata(&path).unwrap();
        let result = str_replace(
            tmp.path(),
            &serde_json::json!({
                "path": "f.txt",
                "old_str": "a\n",
                "new_str": "a\r\n"
            }),
        );
        assert!(result.is_error, "normalized no-op must not report success");
        assert!(
            result.output.contains("normalized replacement"),
            "{}",
            result.output
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"a\n");
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before.len());
    }

    #[test]
    fn multi_edit_rejects_edits_that_cancel_after_normalization() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let result = str_replace(
            tmp.path(),
            &serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_str": "a\n", "new_str": "a\r\n"},
                    {"old_str": "a\r\n", "new_str": "a\n"}
                ]
            }),
        );
        assert!(result.is_error, "cancelled normalized batch must fail");
        assert!(
            result.output.contains("normalized batch"),
            "{}",
            result.output
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"a\n");
    }

    #[test]
    fn str_replace_resolves_non_secret_credential_reference() {
        let tmp = TempDir::new().unwrap();
        let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n";
        let path = tmp.path().join("config.env");
        std::fs::write(&path, raw).unwrap();
        let (redacted, count) = crate::credential_redaction::redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        assert!(!redacted.contains("AKIAIOSFODNN7EXAMPLE"));

        let result = str_replace(
            tmp.path(),
            &serde_json::json!({
                "path": "config.env",
                "old_str": "[REDACTED:AWS_ACCESS_KEY:invalid]",
                "new_str": "[configured-access-key]"
            }),
        );
        assert!(result.is_error, "a forged digest must fail closed");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), raw);

        let marker = redacted
            .split_once('=')
            .and_then(|(_, value)| value.lines().next())
            .expect("marker should be present");
        let result = str_replace(
            tmp.path(),
            &serde_json::json!({
                "path": "config.env",
                "old_str": marker,
                "new_str": "[configured-access-key]"
            }),
        );
        assert!(
            !result.is_error,
            "valid redaction reference: {}",
            result.output
        );
        let updated = std::fs::read_to_string(path).unwrap();
        assert!(updated.contains("[configured-access-key]"));
        assert!(!updated.contains("AKIAIOSFODNN7EXAMPLE"));
    }

    #[test]
    fn str_replace_resolved_marker_equal_to_new_text_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let raw = "AWS_ACCESS_KEY_ID=AKIAIOSFODNN7EXAMPLE\n";
        let path = tmp.path().join("config.env");
        std::fs::write(&path, raw).unwrap();
        let (redacted, count) = crate::credential_redaction::redact_credentials_in_text(raw);
        assert_eq!(count, 1);
        let marker = redacted
            .split_once('=')
            .and_then(|(_, value)| value.lines().next())
            .expect("marker should be present");

        let result = str_replace(
            tmp.path(),
            &serde_json::json!({
                "path": "config.env",
                "old_str": marker,
                "new_str": "AKIAIOSFODNN7EXAMPLE"
            }),
        );
        assert!(
            result.is_error,
            "resolved no-op must not be success: {}",
            result.output
        );
        assert!(
            result.output.contains("no change needed"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("ASTRA_TOOL_OK"),
            "{}",
            result.output
        );
        assert_eq!(std::fs::read_to_string(path).unwrap(), raw);
    }

    #[test]
    fn str_replace_routes_schema_edits_array_to_atomic_multi_edit() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha beta gamma").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_str": "alpha", "new_str": "ALPHA"},
                {"old_str": "gamma", "new_str": "GAMMA"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(!result.is_error, "got error: {}", result.output);
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "a committed multi-path edit must carry the owner applied fact"
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "ALPHA beta GAMMA\n");
    }

    #[test]
    fn str_replace_accepts_per_edit_paths_for_multi_file_batch() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha beta").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "gamma delta").unwrap();
        let args = serde_json::json!({
            "edits": [
                {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                {"path": "b.txt", "old_str": "delta", "new_str": "DELTA"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(!result.is_error, "got error: {}", result.output);
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_applied"))
                .and_then(serde_json::Value::as_bool),
            Some(true),
            "a committed multi-file edit must carry the owner applied fact"
        );
        assert!(
            result.output.contains("2 file(s)"),
            "multi-file summary should name the file count: {}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "ALPHA beta\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(),
            "gamma DELTA\n"
        );
    }

    #[test]
    fn str_replace_per_edit_path_batch_prevalidates_all_files_before_writing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha beta").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "gamma delta").unwrap();
        let args = serde_json::json!({
            "edits": [
                {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                {"path": "b.txt", "old_str": "missing", "new_str": "MISSING"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "missing old_str should fail");
        assert!(
            result.output.contains("old_str not found"),
            "got: {}",
            result.output
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "alpha beta"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(),
            "gamma delta"
        );
    }

    #[test]
    fn str_replace_multi_path_rejects_stale_target_before_any_commit() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, "alpha beta").unwrap();
        std::fs::write(&b, "gamma delta").unwrap();
        let args = serde_json::json!({
            "edits": [
                {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                {"path": "b.txt", "old_str": "gamma", "new_str": "GAMMA"}
            ]
        });

        let prepared = prepare_multi_path_edit(tmp.path(), &args).expect("prepared");
        std::fs::write(&b, "external change").unwrap();

        let result = prepared.apply_with_formatting(true);

        assert!(result.is_error, "stale target must fail: {}", result.output);
        assert!(
            result.output.contains("modified since it was read"),
            "expected stale-file diagnosis: {}",
            result.output
        );
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "alpha beta");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "external change");
    }

    #[test]
    fn str_replace_multi_path_reports_partial_commit_as_quarantine_fact() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("a.txt");
        let b = tmp.path().join("b.txt");
        std::fs::write(&a, "alpha beta").unwrap();
        std::fs::write(&b, "gamma delta").unwrap();
        let args = serde_json::json!({
            "edits": [
                {"path": "a.txt", "old_str": "alpha", "new_str": "ALPHA"},
                {"path": "b.txt", "old_str": "gamma", "new_str": "GAMMA"}
            ]
        });
        let prepared = prepare_multi_path_edit(tmp.path(), &args).expect("prepared");
        MULTI_PATH_RENAME_FAILURE_INDEX.store(1, AtomicOrdering::SeqCst);
        let result = prepared.apply();
        MULTI_PATH_RENAME_FAILURE_INDEX.store(-1, AtomicOrdering::SeqCst);

        assert!(result.is_error, "injected rename must fail");
        assert_eq!(
            result
                .metadata
                .as_ref()
                .and_then(|fields| fields.get("workspace_mutation_partial"))
                .and_then(Value::as_bool),
            Some(true)
        );
        let paths = result
            .metadata
            .as_ref()
            .and_then(|fields| fields.get("workspace_mutation_partial_paths"))
            .and_then(Value::as_array)
            .expect("partial paths");
        assert_eq!(paths.len(), 1);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "ALPHA beta\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "gamma delta");
    }

    #[test]
    fn str_replace_batch_requires_some_path_source() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "alpha beta").unwrap();
        let args = serde_json::json!({
            "edits": [
                {"old_str": "alpha", "new_str": "ALPHA"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error);
        assert!(result.output.contains("edit[0] is missing 'path'"));
    }

    #[test]
    fn str_replace_rejects_replacements_alias() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "one two three").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "replacements": [
                {"original_text": "one", "new_text": "ONE"},
                {"original_text": "three", "new_text": "THREE"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "alias must be rejected");
        assert!(result.output.contains("unknown field 'replacements'"));
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "one two three");
    }

    #[test]
    fn str_replace_rejects_string_edits_payload() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "red green blue").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": r#"[{"old_str":"red","new_str":"RED"},{"old_str":"blue","new_str":"BLUE"}]"#
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "string edits payload must be rejected");
        assert!(result.output.contains("'edits' must be an array"));
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "red green blue");
    }

    #[test]
    fn str_replace_rejects_top_level_single_fields_with_edits() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "red green blue").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "red",
            "new_str": "RED",
            "edits": [
                {"old_str": "blue", "new_str": "BLUE"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "mixed edit modes must be rejected");
        assert!(result.output.contains("mutually exclusive"));
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "red green blue");
    }

    #[test]
    fn str_replace_rejects_unknown_edit_fields() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "red green blue").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_str": "red", "new_str": "RED", "comment": "legacy note"}
            ]
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "unknown edit fields must be rejected");
        assert!(result.output.contains("unknown field 'comment'"));
        assert!(result.output.contains("str_replace.edits[0]"));
        assert!(!result.output.contains("unknown field 'path'"));
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "red green blue");
    }

    #[test]
    fn str_replace_text_extensions_get_trailing_newline() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lib.rs"), "fn main() {}").unwrap();
        let args = serde_json::json!({"path": "lib.rs", "old_str": "main", "new_str": "run"});

        let result = str_replace(tmp.path(), &args);

        assert!(!result.is_error, "got error: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert!(
            content.ends_with('\n'),
            "missing trailing newline: {content:?}"
        );
    }

    #[test]
    fn str_replace_rejects_new_rust_parse_errors_by_default() {
        let tmp = TempDir::new().unwrap();
        let original = "fn main() {\n    println!(\"ok\");\n}\n";
        std::fs::write(tmp.path().join("main.rs"), original).unwrap();
        let args = serde_json::json!({
            "path": "main.rs",
            "old_str": "println!(\"ok\");",
            "new_str": "println!(\"ok);"
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "invalid Rust edit should be rejected");
        assert!(
            result.output.contains("structural validation"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("main.rs")).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn str_replace_rejects_comment_line_loss_by_default() {
        let tmp = TempDir::new().unwrap();
        let original = "/// One\n/// Two\n/// Three\npub fn thing() {}\n";
        std::fs::write(tmp.path().join("lib.rs"), original).unwrap();
        let args = serde_json::json!({
            "path": "lib.rs",
            "old_str": original,
            "new_str": "/// One\n/// Three\npub fn thing() {}\n"
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error, "doc-comment loss should be rejected");
        assert!(
            result.output.contains("comment/doc-comment"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("lib.rs")).unwrap();
        assert_eq!(content, original);
    }

    #[test]
    fn str_replace_allows_structural_change_when_explicitly_requested() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("main.rs"),
            "fn main() {\n    println!(\"ok\");\n}\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": "main.rs",
            "old_str": "println!(\"ok\");",
            "new_str": "println!(\"ok);",
            "allow_structural_change": true
        });

        let result = str_replace(tmp.path(), &args);

        assert!(
            !result.is_error,
            "explicit bypass should allow edit: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("main.rs")).unwrap();
        assert!(content.contains("println!(\"ok);"));
    }

    #[test]
    fn str_replace_multiple_matches() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "aaa").unwrap();
        let args = serde_json::json!({"path": "f.txt", "old_str": "a", "new_str": "b"});
        let result = str_replace(tmp.path(), &args);
        assert!(result.is_error);
        assert!(result.output.contains("3 times"));
    }

    #[test]
    fn str_replace_falls_back_to_whitespace_normalized_match() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("f.txt"),
            "  fn hello() {\n    println!(\"hi\");\n  }\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "fn hello() {\n  println!(\"hi\");\n}",
            "new_str": "fn hello() {\n  println!(\"bye\");\n}"
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("line-trimmed"),
            "expected line-trimmed strategy, got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert!(content.contains("println!(\"bye\");"), "got: {content}");
    }

    #[test]
    fn str_replace_not_found_hint_whitespace_branch_emits_signal_only() {
        let content =
            "  fn big() {\n    a();\n    b();\n    c();\n    d();\n    e();\n    f();\n  }\n";
        let old_str = "fn big() {\n  a();\n  b();\n  c();\n  d();\n  e();\n  f();\n}";
        let msg = str_replace_not_found_hint("f.txt", content, old_str);
        assert!(
            msg.contains("whitespace_normalized_match: true"),
            "got: {msg}"
        );
        // Hint must NOT echo file content — model already has it from prior read_file
        assert!(
            !msg.contains("Actual file content"),
            "hint should not echo file content, got: {msg}"
        );
        assert!(
            !msg.contains("f();"),
            "hint should not echo file lines, got: {msg}"
        );
    }

    #[test]
    fn str_replace_not_found_reports_whitespace_hint() {
        let msg = str_replace_not_found_hint(
            "f.txt",
            "  fn hello() {\n    println!(\"hi\");\n  }\n",
            "fn hello() {\n  println!(\"hi\");\n}",
        );
        assert!(
            msg.contains("whitespace_normalized_match: true"),
            "got: {msg}"
        );
        assert!(msg.contains("first_line_at: L1"), "got: {msg}");
    }

    #[test]
    fn str_replace_quote_normalized_match_preserves_file_quote_style() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "let x = \u{201C}hello\u{201D};").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "let x = \"hello\";",
            "new_str": "let x = \"world\";"
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "let x = \u{201C}world\u{201D};\n");
    }

    #[test]
    fn str_replace_dry_run_shows_diff_without_writing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "line1\nline2\nline3\n").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "line2",
            "new_str": "REPLACED",
            "dry_run": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("[DRY RUN]"),
            "got: {}",
            result.output
        );
        assert!(result.output.contains("-line2"), "got: {}", result.output);
        assert!(
            result.output.contains("+REPLACED"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "line1\nline2\nline3\n");
    }

    #[test]
    fn str_replace_rejects_identical_single_edit() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "same\n").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "same",
            "new_str": "same"
        });
        let result = str_replace(tmp.path(), &args);
        assert!(result.is_error);
        assert!(
            result.output.contains("STR_REPLACE FAILED"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("no change needed"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "same\n");
    }

    #[test]
    fn str_replace_rejects_empty_anchor_before_scanning_file() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha beta\n").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "",
            "new_str": "replacement"
        });

        let result = str_replace(tmp.path(), &args);

        assert!(result.is_error);
        assert!(result.output.contains("STR_REPLACE FAILED"));
        assert!(
            result.output.contains("old_str is empty"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("found ") && !result.output.contains("times"),
            "empty anchor should be rejected before match counting: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "alpha beta\n");
    }

    #[test]
    fn multi_edit_rejects_empty_anchor_before_noop_check() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "alpha beta\n").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_str": "", "new_str": ""}
            ]
        });

        let result = multi_edit(tmp.path(), &args);

        assert!(result.is_error);
        assert!(
            result.output.contains("old_str is empty"),
            "{}",
            result.output
        );
        assert!(
            !result.output.contains("no-op"),
            "empty anchor should report an invalid anchor, not a no-op: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "alpha beta\n");
    }

    #[test]
    fn str_replace_replace_all_fuzzy_ambiguous_does_not_partially_apply() {
        let tmp = TempDir::new().unwrap();
        let original = "  fn hi() {\n    a();\n  }\n\n\tfn hi() {\n\t  a();\n\t}\n";
        std::fs::write(tmp.path().join("f.txt"), original).unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "fn hi() {\n  a();\n}",
            "new_str": "fn bye() {\n  b();\n}",
            "replace_all": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(result.is_error, "got success: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, original);
    }

    // ─── Issue #1: replace_all + quote-normalized should replace all ────
    #[test]
    fn str_replace_replace_all_quote_normalized_replaces_all_occurrences() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("f.txt"),
            "let a = \u{201C}hello\u{201D};\nlet b = \u{201C}hello\u{201D};\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "\"hello\"",
            "new_str": "\"world\"",
            "replace_all": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(
            content,
            "let a = \u{201C}world\u{201D};\nlet b = \u{201C}world\u{201D};\n"
        );
    }

    #[test]
    fn str_replace_not_found_hint_first_line_match_branch() {
        let msg = str_replace_not_found_hint(
            "f.txt",
            "fn foo() {\n    bar();\n    baz();\n}\n",
            "fn foo() {\n    bar();\n    qux();\n}",
        );
        assert!(msg.contains("first_line_at:"), "got: {msg}");
        assert!(
            !msg.contains("Actual file content"),
            "hint should not echo file content, got: {msg}"
        );
    }

    #[test]
    fn str_replace_not_found_hint_generic_fallback() {
        let msg = str_replace_not_found_hint(
            "f.txt",
            "totally different content\nno matches at all\n",
            "something completely unrelated",
        );
        assert!(msg.contains("no_partial_match: true"), "got: {msg}");
        assert!(msg.contains("Do NOT blindly retry"), "got: {msg}");
    }

    #[test]
    fn str_replace_not_found_hint_individual_lines_branch() {
        let msg = str_replace_not_found_hint("f.txt", "aaa\nbbb\nccc\n", "aaa\nXXX\nccc");
        assert!(
            msg.contains("individual_line_match_ratio: 2/3"),
            "got: {msg}"
        );
    }

    // ─── Missing coverage: dry_run + fuzzy match ────────────────────────
    #[test]
    fn str_replace_dry_run_fuzzy_match_does_not_write() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("f.txt"),
            "  fn hello() {\n    println!(\"hi\");\n  }\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "fn hello() {\n  println!(\"hi\");\n}",
            "new_str": "fn hello() {\n  println!(\"bye\");\n}",
            "dry_run": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("[DRY RUN]"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(
            content, "  fn hello() {\n    println!(\"hi\");\n  }\n",
            "file should not be modified"
        );
    }

    // ─── Missing coverage: replace_all + dry_run ────────────────────────
    #[test]
    fn str_replace_replace_all_dry_run_shows_all_changes() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "aaa\nbbb\naaa\nbbb\n").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "aaa",
            "new_str": "ZZZ",
            "replace_all": true,
            "dry_run": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("[DRY RUN]"),
            "got: {}",
            result.output
        );
        assert!(result.output.contains("-aaa"), "got: {}", result.output);
        assert!(result.output.contains("+ZZZ"), "got: {}", result.output);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(
            content, "aaa\nbbb\naaa\nbbb\n",
            "file should not be modified"
        );
    }

    // ─── replace_all exact match (non-fuzzy) ──────────────────────────
    #[test]
    fn str_replace_replace_all_exact_match() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "aaa\nbbb\naaa\n").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "aaa",
            "new_str": "ZZZ",
            "replace_all": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("2 occurrences"),
            "expected occurrence count, got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "ZZZ\nbbb\nZZZ\n");
    }

    // ─── dry_run + quote-normalized fuzzy match ─────────────────────────
    #[test]
    fn str_replace_dry_run_quote_normalized_does_not_write() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "let x = \u{201C}hello\u{201D};").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "let x = \"hello\";",
            "new_str": "let x = \"world\";",
            "dry_run": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error, "got error: {}", result.output);
        assert!(
            result.output.contains("[DRY RUN]"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "let x = \u{201C}hello\u{201D};");
    }

    // ─── not-found hint: empty first line of old_str ────────────────────
    #[test]
    fn str_replace_not_found_hint_empty_first_line() {
        let msg = str_replace_not_found_hint("f.txt", "some content\n", "  \nactual code");
        assert!(msg.contains("old_str not found"), "got: {msg}");
    }

    #[test]
    fn str_replace_not_found_hint_no_individual_line_matches() {
        let msg =
            str_replace_not_found_hint("f.txt", "real content here\n", "xxxxxxx\nyyyyyyy\nzzzzzzz");
        assert!(
            msg.contains("no_partial_match: true"),
            "expected generic fallback, got: {msg}"
        );
        assert!(!msg.contains("individual_line_match_ratio"), "got: {msg}");
    }

    // ─── unified_diff edge cases ────────────────────────────────────────
    #[test]
    fn unified_diff_groups_removed_then_added_lines() {
        let old = "ctx\nold1\nold2\nctx\n";
        let new = "ctx\nnew1\nnew2\nctx\n";
        let diff = unified_diff(old, new, "test.txt");
        let minus_pos = diff.find("-old1").unwrap();
        let minus2_pos = diff.find("-old2").unwrap();
        let plus_pos = diff.find("+new1").unwrap();
        assert!(
            minus2_pos < plus_pos,
            "expected grouped -/+ lines, got:\n{diff}"
        );
        assert!(
            minus2_pos > minus_pos,
            "minus lines out of order in:\n{diff}"
        );
    }

    #[test]
    fn unified_diff_no_changes_shows_no_changes() {
        let content = "line1\nline2\n";
        let diff = unified_diff(content, content, "same.txt");
        assert!(diff.contains("(no changes)"), "got:\n{diff}");
    }

    #[test]
    fn unified_diff_added_lines() {
        let old = "a\nb\n";
        let new = "a\nb\nc\n";
        let diff = unified_diff(old, new, "add.txt");
        assert!(diff.contains("+c"), "got:\n{diff}");
    }

    #[test]
    fn unified_diff_removed_lines() {
        let old = "a\nb\nc\n";
        let new = "a\nb\n";
        let diff = unified_diff(old, new, "rm.txt");
        assert!(diff.contains("-c"), "got:\n{diff}");
    }

    #[test]
    fn unified_diff_includes_context_lines() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\n";
        let new = "a\nb\nc\nd\nX\nf\ng\nh\n";
        let diff = unified_diff(old, new, "ctx.txt");
        assert!(diff.contains(" b"), "expected context before, got:\n{diff}");
        assert!(diff.contains(" f"), "expected context after, got:\n{diff}");
        assert!(diff.contains("-e"), "got:\n{diff}");
        assert!(diff.contains("+X"), "got:\n{diff}");
    }

    // ─── Issue #1: unified_diff insertion should not shift subsequent lines ──
    #[test]
    fn unified_diff_insertion_does_not_shift_subsequent_lines() {
        let old = "a\nb\nc\nd\n";
        let new = "a\nb\nINSERTED\nc\nd\n";
        let diff = unified_diff(old, new, "ins.txt");
        // Only the inserted line should appear as +, c and d should be context
        assert!(diff.contains("+INSERTED"), "got:\n{diff}");
        // c and d must NOT appear as changed lines
        assert!(
            !diff.contains("-c"),
            "c should not be removed, got:\n{diff}"
        );
        assert!(
            !diff.contains("-d"),
            "d should not be removed, got:\n{diff}"
        );
    }

    #[test]
    fn unified_diff_deletion_does_not_shift_subsequent_lines() {
        let old = "a\nb\nDELETED\nc\nd\n";
        let new = "a\nb\nc\nd\n";
        let diff = unified_diff(old, new, "del.txt");
        assert!(diff.contains("-DELETED"), "got:\n{diff}");
        assert!(
            !diff.contains("-c"),
            "c should not be removed, got:\n{diff}"
        );
        assert!(!diff.contains("+c"), "c should not be added, got:\n{diff}");
    }

    #[test]
    fn unified_diff_hunk_header_counts_match_actual_lines() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nX\nY\nd\ne\n";
        let diff = unified_diff(old, new, "hdr.txt");
        // Parse the @@ line and verify counts match actual - and + lines (+ context)
        for line in diff.lines() {
            if line.starts_with("@@") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let old_spec = parts[1]; // e.g. "-1,5"
                let new_spec = parts[2]; // e.g. "+1,6"
                let old_count: usize = old_spec.split(',').nth(1).unwrap().parse().unwrap();
                let new_count: usize = new_spec.split(',').nth(1).unwrap().parse().unwrap();
                // Count actual lines in the hunk
                let hunk_lines: Vec<&str> = diff
                    .lines()
                    .skip_while(|l| !l.starts_with("@@"))
                    .skip(1)
                    .collect();
                let actual_old = hunk_lines
                    .iter()
                    .filter(|l| l.starts_with(' ') || l.starts_with('-'))
                    .count();
                let actual_new = hunk_lines
                    .iter()
                    .filter(|l| l.starts_with(' ') || l.starts_with('+'))
                    .count();
                assert_eq!(
                    old_count, actual_old,
                    "old count in header ({old_count}) != actual old lines ({actual_old}), diff:\n{diff}"
                );
                assert_eq!(
                    new_count, actual_new,
                    "new count in header ({new_count}) != actual new lines ({actual_new}), diff:\n{diff}"
                );
                break;
            }
        }
    }

    // ─── Issue #2: LCS line-count guard for large files ──────────────────
    #[test]
    fn unified_diff_large_file_uses_simple_fallback() {
        // File exceeds LCS_LINE_LIMIT → falls back to index-aligned diff.
        let line_count = LCS_LINE_LIMIT + 100;
        let old_lines: Vec<String> = (0..line_count).map(|i| format!("line {i}")).collect();
        let mut new_lines = old_lines.clone();
        new_lines[line_count / 2] = "CHANGED".to_string();
        let old = old_lines.join("\n");
        let new = new_lines.join("\n");
        let start = std::time::Instant::now();
        let diff = unified_diff(&old, &new, "big.txt");
        let elapsed = start.elapsed();
        assert!(
            diff.contains("[DRY RUN]"),
            "got:\n{}",
            &diff[..200.min(diff.len())]
        );
        assert!(
            diff.contains("+CHANGED"),
            "got:\n{}",
            &diff[..500.min(diff.len())]
        );
        assert!(
            elapsed.as_millis() < 200,
            "fallback should be fast, took {}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn unified_diff_within_lcs_limit_uses_lcs() {
        // File within limit still gets proper LCS-based diff
        let old = "a\nb\nc\nd\n";
        let new = "a\nb\nINSERTED\nc\nd\n";
        let diff = unified_diff(old, new, "small.txt");
        assert!(diff.contains("+INSERTED"), "got:\n{diff}");
        // LCS correctly identifies insertion — c should not appear as changed
        assert!(
            !diff.contains("-c"),
            "LCS should handle insertion correctly, got:\n{diff}"
        );
    }

    // ─── Issue #3: pure-insert hunk should reference correct old line ───
    #[test]
    fn unified_diff_pure_insert_hunk_references_adjacent_old_line() {
        // Use a file long enough that context(3) doesn't cover the start
        let old = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";
        let new = "1\n2\n3\n4\n5\n6\n7\nINSERTED\n8\n9\n10\n";
        let diff = unified_diff(old, new, "ins.txt");
        assert!(diff.contains("+INSERTED"), "got:\n{diff}");
        for line in diff.lines() {
            if line.starts_with("@@") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let old_spec = parts[1]; // e.g. "-5,6"
                let old_start: usize = old_spec[1..].split(',').next().unwrap().parse().unwrap();
                // Insertion is between old line 7 and 8. With context=3, hunk should start
                // around line 5 (7-3+1), not line 1.
                assert!(
                    old_start >= 4,
                    "old_start should reference nearby context, not line 1, got: {line}"
                );
                break;
            }
        }
    }

    #[test]
    fn str_replace_not_found_hint_first_line_does_not_echo_file_content() {
        let content = "header\nfn foo() {\n    bar();\n    baz();\n}\nfooter\n";
        let old_str = "fn foo() {\n    bar();\n    qux();\n}";
        let msg = str_replace_not_found_hint("f.txt", content, old_str);

        assert!(msg.contains("first_line_at:"), "got: {msg}");
        assert!(
            !msg.contains("Actual file content"),
            "hint must not echo file content, got: {msg}"
        );
        assert!(
            !msg.contains("header"),
            "hint must not echo file lines, got: {msg}"
        );
        assert!(
            !msg.contains("footer"),
            "hint must not echo file lines, got: {msg}"
        );
    }

    #[test]
    fn diff_op_is_copy() {
        let op = DiffOp::Equal(0, 0, "line");
        let copy = op;
        // If DiffOp is not Copy, using `op` after the move would fail to compile.
        assert!(matches!(op, DiffOp::Equal(0, 0, "line")));
        assert!(matches!(copy, DiffOp::Equal(0, 0, "line")));
    }

    // ─── Issue #2: replace_all + mixed curly-quote forms → specific error ──
    #[test]
    fn str_replace_replace_all_mixed_curly_quotes_gives_specific_error() {
        let tmp = TempDir::new().unwrap();
        // Two occurrences with different curly-quote forms: \u{201C}a\u{201D} vs \u{201C}a\u{201C}
        std::fs::write(
            tmp.path().join("f.txt"),
            "say \u{201C}a\u{201D} and \u{201C}a\u{201C} done",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "old_str": "\"a\"",
            "new_str": "\"b\"",
            "replace_all": true
        });
        let result = str_replace(tmp.path(), &args);
        assert!(
            result.is_error,
            "should error on mixed curly-quote forms with replace_all, got: {}",
            result.output
        );
        assert!(
            result.output.contains("curly quote"),
            "error should mention curly quotes, got: {}",
            result.output
        );
    }

    #[test]
    fn delete_file_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("del.txt"), "x").unwrap();
        let args = serde_json::json!({"path": "del.txt"});
        let result = delete_file(tmp.path(), &args);
        assert!(!result.is_error);
        assert!(!tmp.path().join("del.txt").exists());
    }

    #[test]
    fn list_dir_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "").unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let args = serde_json::json!({"path": "."});
        let result = list_dir(tmp.path(), &args);
        assert!(result.output.contains("a.txt"));
        assert!(result.output.contains("sub/"));
    }

    #[test]
    fn list_dir_surfaces_generic_companion_artifact_advisory() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("source.bin"), [0_u8, 1, 2]).unwrap();
        std::fs::write(tmp.path().join("source.bin-journal"), [3_u8, 4, 5]).unwrap();
        let result = list_dir(tmp.path(), &serde_json::json!({"path": "."}));
        assert!(!result.is_error);
        assert!(result.output.contains("source.bin"));
        assert!(result.output.contains("source.bin-journal"));
        assert!(result.output.contains("related source/companion artifacts"));
        assert!(result.output.contains("copy and checksum"));
    }

    #[test]
    fn list_dir_does_not_warn_for_unrelated_names() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("source.bin"), [0_u8, 1, 2]).unwrap();
        std::fs::write(tmp.path().join("source.txt"), [3_u8, 4, 5]).unwrap();
        let result = list_dir(tmp.path(), &serde_json::json!({"path": "."}));
        assert!(!result.output.contains("related source/companion artifacts"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_path_allows_existing_path_under_workspace_alias() {
        let real_root = TempDir::new().unwrap();
        let alias_parent = TempDir::new().unwrap();
        let alias_root = alias_parent.path().join("workspace-alias");
        std::os::unix::fs::symlink(real_root.path(), &alias_root).unwrap();

        let file = real_root.path().join("nested.txt");
        std::fs::write(&file, "hello").unwrap();

        let resolved = resolve_path(&alias_root, "nested.txt").unwrap();
        assert_eq!(resolved, file.canonicalize().unwrap());
    }

    #[test]
    fn path_traversal_blocked() {
        let tmp = TempDir::new().unwrap();
        let args = serde_json::json!({"path": "../../../etc/passwd"});
        let result = read_file(tmp.path(), &args);
        assert!(result.is_error);
        assert!(result.output.contains("SANDBOX_DENIED"));
    }

    #[test]
    fn multi_edit_applies_all() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "aaa bbb ccc").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_str": "aaa", "new_str": "AAA"},
                {"old_str": "ccc", "new_str": "CCC"}
            ]
        });
        let result = multi_edit(tmp.path(), &args);
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "AAA bbb CCC\n");
    }

    #[test]
    fn multi_edit_aborts_on_missing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "aaa bbb").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_str": "aaa", "new_str": "AAA"},
                {"old_str": "zzz", "new_str": "ZZZ"}
            ]
        });
        let result = multi_edit(tmp.path(), &args);
        assert!(result.is_error);
        assert!(
            result.output.contains("STR_REPLACE FAILED"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("edit[1] old_str not found"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("no_partial_match: true"),
            "got: {}",
            result.output
        );
        // Original file should be unchanged (atomic)
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "aaa bbb");
    }

    #[test]
    fn multi_edit_missing_anchor_reports_structured_near_match_hint() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(
            tmp.path().join("f.txt"),
            "  fn hello() {\n    println!(\"hi\");\n  }\n",
        )
        .unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [{
                "old_str": "fn hello() {\n  println!(\"hi\");\n}",
                "new_str": "fn hello() {}"
            }]
        });

        let result = multi_edit(tmp.path(), &args);
        assert!(result.is_error);
        assert!(
            result.output.contains("edit[0] old_str not found"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("whitespace_normalized_match: true"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("first_line_at: L1"),
            "got: {}",
            result.output
        );
    }

    #[test]
    fn multi_edit_dry_run() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "foo bar").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [{"old_str": "foo", "new_str": "baz"}],
            "dry_run": true
        });
        let result = multi_edit(tmp.path(), &args);
        assert!(!result.is_error);
        assert!(result.output.contains("Dry run"));
        // File unchanged
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "foo bar");
    }

    #[test]
    fn multi_edit_rejects_ambiguous() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "aaa aaa").unwrap();
        let args = serde_json::json!({
            "path": "f.txt",
            "edits": [{"old_str": "aaa", "new_str": "bbb"}]
        });
        let result = multi_edit(tmp.path(), &args);
        assert!(result.is_error);
        assert!(result.output.contains("2 times"));
    }

    // ── Sandbox allowed_paths tests ──

    #[test]
    fn resolve_path_allows_tmp_by_default() {
        let workspace = tempfile::tempdir().unwrap();
        let result = resolve_path(workspace.path(), "/tmp/test_file.txt");
        assert!(
            result.is_ok(),
            "resolve_path must allow /tmp (default allowed path): {:?}",
            result
        );
    }

    #[test]
    fn resolve_path_allows_platform_temp_dir_by_default() {
        let workspace = tempfile::tempdir().unwrap();
        let temp_file = std::env::temp_dir().join("astra-default-allowed-temp.txt");
        let result = resolve_path(workspace.path(), &temp_file.to_string_lossy());
        assert!(
            result.is_ok(),
            "resolve_path must allow the platform temp dir by default: {:?}",
            result
        );
    }

    #[test]
    fn resolve_path_sandboxed_denies_tmp_with_empty_allowed() {
        let workspace = tempfile::tempdir().unwrap();
        let result = resolve_path_sandboxed(workspace.path(), "/tmp/test_file.txt", &[]);
        assert!(result.is_err(), "empty allowed_paths must deny /tmp");
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[test]
    fn resolve_path_sandboxed_allows_tmp_when_configured() {
        let workspace = tempfile::tempdir().unwrap();
        let allowed = vec![PathBuf::from("/tmp")];
        let result = resolve_path_sandboxed(workspace.path(), "/tmp/test_file.txt", &allowed);
        assert!(
            result.is_ok(),
            "resolve_path_sandboxed must allow /tmp when in allowed_paths: {:?}",
            result
        );
        let resolved = result.unwrap();
        assert!(
            is_within_workspace_root(&resolved, Path::new("/tmp")),
            "resolved temp path must stay within the configured temp root: {:?}",
            resolved
        );
    }

    /// Regression: a workspace under $TMPDIR (e.g. /tmp/.../tmpXXX) plus a
    /// relative input that climbs out via `..` MUST NOT be rescued by the
    /// allowlist. Relative paths express intent "stay inside workspace";
    /// the /tmp allowlist is for absolute paths the user explicitly named.
    /// Conflating the two lets `read_file("../../foo")` from a workspace
    /// inside /tmp escape the sandbox.
    #[test]
    fn resolve_path_relative_dot_dot_escape_is_denied_even_when_landing_in_tmp() {
        let tmp_root = std::env::temp_dir();
        let workspace = tempfile::tempdir_in(&tmp_root).unwrap();
        // Climb above the workspace, land in /tmp (which is in default
        // allowed_paths). Must still be denied because the request was
        // a relative path that resolved out of workspace_root.
        let result = resolve_path(workspace.path(), "../escape.txt");
        assert!(
            result.is_err(),
            "relative ../ that escapes workspace must be denied even when result is under /tmp: {:?}",
            result
        );
        assert!(result.unwrap_err().contains("SANDBOX_DENIED"));
    }

    #[test]
    fn resolve_path_sandboxed_still_denies_etc() {
        let workspace = tempfile::tempdir().unwrap();
        let allowed = vec![PathBuf::from("/tmp")];
        let result = resolve_path_sandboxed(workspace.path(), "/etc/passwd", &allowed);
        assert!(
            result.is_err(),
            "/etc must still be denied even with /tmp allowed"
        );
    }

    #[test]
    fn resolve_path_sandboxed_allows_workspace_paths() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::write(workspace.path().join("hello.txt"), "hi").unwrap();
        let allowed = vec![PathBuf::from("/tmp")];
        let result = resolve_path_sandboxed(workspace.path(), "hello.txt", &allowed);
        assert!(result.is_ok(), "workspace-relative paths must still work");
    }

    // ── normalize_line_endings_to_lf ─────────────────────────────

    #[test]
    fn normalize_crlf_to_lf_preserves_line_count() {
        let s = "a\r\nb\r\nc";
        assert_eq!(normalize_line_endings_to_lf(s), "a\nb\nc");
    }

    #[test]
    fn normalize_lone_cr_becomes_lf() {
        // Classic Mac line endings — rare but the conversion must
        // still produce valid Unix text, not silent corruption.
        let s = "a\rb\rc";
        assert_eq!(normalize_line_endings_to_lf(s), "a\nb\nc");
    }

    #[test]
    fn normalize_lf_only_is_idempotent_and_allocation_free() {
        // Fast path: no `\r` anywhere → function returns identical
        // content. The caller's fast path relies on this.
        let s = "a\nb\nc";
        assert_eq!(normalize_line_endings_to_lf(s), "a\nb\nc");
    }

    #[test]
    fn normalize_mixed_crlf_and_lf_is_uniform() {
        let s = "a\r\nb\nc\r\nd";
        assert_eq!(normalize_line_endings_to_lf(s), "a\nb\nc\nd");
    }

    // ── normalize_content_before_write: CRLF + trailing newline ──

    #[test]
    fn write_file_crlf_input_gets_normalized_to_lf_before_newline_check() {
        // Regression guard: previously a file with `\r\n` endings
        // would pass `ends_with('\n')` (it literally did) and the
        // "already has newline" branch kicked in, leaving the file
        // on disk with mixed endings.
        let tmp = TempDir::new().unwrap();
        let content_crlf = "one\r\ntwo\r\nthree";
        let args = serde_json::json!({
            "path": "notes.txt",
            "content": content_crlf,
        });
        let result = write_file(tmp.path(), &args);
        assert!(!result.is_error, "write should succeed: {}", result.output);
        let written = std::fs::read_to_string(tmp.path().join("notes.txt")).unwrap();
        assert!(
            !written.contains('\r'),
            "file must not contain \\r: {written:?}"
        );
        assert!(written.ends_with('\n'), "file must end with LF");
    }

    // ── should_ensure_trailing_newline: basename fallback ────────

    #[test]
    fn extensionless_text_basenames_enforce_newline() {
        for name in ["Makefile", "Dockerfile", "Rakefile", "README", "LICENSE"] {
            let tmp = TempDir::new().unwrap();
            let args = serde_json::json!({
                "path": name,
                "content": "body-without-newline",
            });
            let result = write_file(tmp.path(), &args);
            assert!(
                !result.is_error,
                "write of {name} failed: {}",
                result.output
            );
            let written = std::fs::read_to_string(tmp.path().join(name)).unwrap();
            assert!(
                written.ends_with('\n'),
                "{name} must gain trailing newline (extensionless text); got {written:?}"
            );
        }
    }

    #[test]
    fn dotfile_text_basenames_enforce_newline() {
        for name in [".gitignore", ".editorconfig", ".bashrc"] {
            let tmp = TempDir::new().unwrap();
            let args = serde_json::json!({
                "path": name,
                "content": "*.log",
            });
            let result = write_file(tmp.path(), &args);
            assert!(!result.is_error, "{name}: {}", result.output);
            let written = std::fs::read_to_string(tmp.path().join(name)).unwrap();
            assert!(written.ends_with('\n'), "{name} must gain trailing newline");
        }
    }

    #[test]
    fn unknown_extension_does_not_force_newline() {
        // Conservative: unknown extension → leave content alone. We
        // don't want to mangle binary files we haven't classified.
        let tmp = TempDir::new().unwrap();
        let args = serde_json::json!({
            "path": "data.unknownext",
            "content": "raw-bytes",
        });
        let result = write_file(tmp.path(), &args);
        assert!(!result.is_error);
        let written = std::fs::read_to_string(tmp.path().join("data.unknownext")).unwrap();
        assert!(
            !written.ends_with('\n'),
            "unknown extension must not gain a synthetic newline; got {written:?}"
        );
    }

    // ── comment_line_count: narrowed to doc-comments ─────────────

    #[test]
    fn comment_line_count_narrowed_ignores_shebang() {
        // Regression: the old `#`-prefix rule treated the shebang
        // as a comment, so any edit that dropped the shebang was
        // rejected as "comment loss" even when intentional (e.g.
        // converting a script into a module).
        let with_shebang = "#!/usr/bin/env python\n# coding: utf-8\nx = 1\n";
        let without_shebang = "x = 1\n";
        assert_eq!(
            comment_line_count(with_shebang),
            comment_line_count(without_shebang),
            "shebang + coding pragma must not inflate the doc-comment count"
        );
    }

    #[test]
    fn comment_line_count_counts_rust_doc_comments() {
        let s = "/// Doc1\n/// Doc2\n//! Module doc\nfn plain_code() {}\n// regular comment\n";
        // Only the 3 rust doc lines count; the `//` is not a doc comment.
        assert_eq!(comment_line_count(s), 3);
    }

    #[test]
    fn comment_line_count_counts_jsdoc_opener() {
        let s = "/**\n * Hello\n */\nfunction f() {}\n";
        assert_eq!(comment_line_count(s), 3);
    }

    #[test]
    fn str_replace_rejects_jsdoc_continuation_line_loss_by_default() {
        let tmp = TempDir::new().unwrap();
        let original = "/**\n * Important details\n */\nfunction thing() {}\n";
        std::fs::write(tmp.path().join("doc.js"), original).unwrap();
        let args = serde_json::json!({
            "path": "doc.js",
            "old_str": original,
            "new_str": "/**\n */\nfunction thing() {}\n"
        });

        let result = str_replace(tmp.path(), &args);

        assert!(
            result.is_error,
            "JSDoc continuation line loss should be rejected: {}",
            result.output
        );
        assert!(
            result.output.contains("comment/doc-comment"),
            "got: {}",
            result.output
        );
        let content = std::fs::read_to_string(tmp.path().join("doc.js")).unwrap();
        assert_eq!(content, original);
    }

    // ── Atomic write pipeline ────────────────────────────────────

    #[test]
    fn atomic_write_staging_path_is_sibling_not_target() {
        // Rename is only atomic within the same filesystem, so the
        // staging tmp MUST sit in the target's parent directory.
        let target = PathBuf::from("/workspace/project/src/lib.rs");
        let tmp = staging_tmp_path(&target);
        assert_eq!(
            tmp.parent(),
            target.parent(),
            "staging tmp must live beside the target, not in /tmp"
        );
        // And must not equal the target.
        assert_ne!(tmp.file_name(), target.file_name());
    }

    #[test]
    fn atomic_write_preserves_extension_for_formatters() {
        // rustfmt / prettier / ruff detect the file's language by
        // extension. The staging path MUST end in the original
        // extension so the formatter treats it as the right kind.
        let target = PathBuf::from("/workspace/proj/src/lib.rs");
        let tmp = staging_tmp_path(&target);
        let tmp_name = tmp.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            tmp_name.ends_with(".rs"),
            "staging path must end in .rs so rustfmt recognizes it; got {tmp_name}"
        );
    }

    #[test]
    fn atomic_write_completes_cleanly_with_no_stale_tmp() {
        // After a successful atomic write, the staging tmp must be
        // renamed (not left behind) and the target must have the
        // final content.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("lib.rs");
        std::fs::write(&target, "original\n").unwrap();

        let _warning =
            write_file_atomic_with_format(&target, b"pub fn new_body() {}\n", false, None, true)
                .unwrap();
        assert_eq!(
            std::fs::read_to_string(&target).unwrap(),
            "pub fn new_body() {}\n",
            "target must have the new content after atomic write"
        );
        // Directory must contain exactly `lib.rs` — no lingering
        // `.astra-tmp.*` staging file after the rename.
        let leftover: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".astra-tmp."))
            .collect();
        assert!(
            leftover.is_empty(),
            "no staging tmp files should remain; got {leftover:?}"
        );
    }

    #[test]
    fn atomic_write_target_contains_new_content_after_success() {
        // End-to-end: write, check content lands.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("hello.txt");
        let result = write_file_atomic_with_format(&target, b"hello world\n", false, None, true);
        assert!(result.is_ok(), "atomic write must succeed: {result:?}");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world\n");
    }

    #[test]
    fn atomic_write_target_preserved_on_staging_failure() {
        // If the staging write fails (e.g. the parent dir is not
        // writable), the existing target must NOT be touched. Guard
        // against the old behaviour where a partial `fs::write`
        // could truncate the target before erroring.
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("existing.txt");
        std::fs::write(&target, "ORIGINAL\n").unwrap();

        // Force staging failure by making the parent directory
        // read-only on Unix.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let orig_perm = std::fs::metadata(tmp.path()).unwrap().permissions();
            let mut ro = orig_perm.clone();
            ro.set_mode(0o500); // r-x only, no write for owner
            std::fs::set_permissions(tmp.path(), ro).unwrap();

            let result = write_file_atomic_with_format(&target, b"NEW\n", false, None, true);

            // Restore perms before asserting so tempdir drop works.
            std::fs::set_permissions(tmp.path(), orig_perm).unwrap();

            assert!(result.is_err(), "staging write should have failed");
            assert_eq!(
                std::fs::read_to_string(&target).unwrap(),
                "ORIGINAL\n",
                "target must remain untouched when staging fails"
            );
        }
    }

    #[test]
    fn str_replace_allows_shebang_removal() {
        // Concrete end-to-end regression: before the narrowing,
        // dropping a shebang raised "removes comment/doc-comment
        // lines". After the fix, the edit should proceed — shebangs
        // aren't doc comments.
        let tmp = TempDir::new().unwrap();
        let original = "#!/usr/bin/env python3\nprint('hi')\n";
        std::fs::write(tmp.path().join("run.py"), original).unwrap();
        let args = serde_json::json!({
            "path": "run.py",
            "old_str": original,
            "new_str": "print('hi')\n",
        });
        let result = str_replace(tmp.path(), &args);
        assert!(
            !result.is_error,
            "shebang removal should succeed (not a doc comment): {}",
            result.output
        );
    }
}
