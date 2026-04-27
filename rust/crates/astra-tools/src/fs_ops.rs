//! File operations: read, write, str_replace, delete, list_dir.
//!
//! All operations are sandboxed to a workspace root directory. Path traversal
//! via `..` is normalized before the boundary check to prevent escapes.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::Value;

use crate::code_intel;
use crate::fuzzy_replacer::{
    fuzzy_find_replacement, normalize_ws, preserve_quote_style, quote_normalized_match_count,
};
use crate::{ToolResult, per_tool_output_limit, truncate_output};

const READ_FILE_SIZE_LIMIT: usize = 80 * 1024;
/// Hard ceiling: files above this size are never read into memory for preview.
const READ_FILE_HARD_LIMIT: usize = 10 * 1024 * 1024;
const IMAGE_READ_SIZE_LIMIT: u64 = 1_500_000;
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "gif", "bmp", "webp"];
const BINARY_EXTS: &[&str] = &[
    "svg", "pdf", "zip", "gz", "tar", "bz2", "xz", "7z", "rar", "exe", "dll", "so", "dylib", "o",
    "a", "lib", "wasm", "class", "pyc", "pyo", "mp3", "mp4", "avi", "mov", "wav", "flac", "ogg",
    "ttf", "otf", "woff", "woff2", "eot", "sqlite", "db", "mdb", "ico",
];

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

/// Resolve a relative path against workspace_root with normalization.
///
/// Returns an error if the resolved path escapes the workspace boundary.
pub fn resolve_path(workspace_root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = if Path::new(relative).is_absolute() {
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
        normalized
    };

    if !is_within_workspace_root(&final_path, workspace_root) {
        return Err(format!(
            "SANDBOX_DENIED: Path '{}' is outside workspace root '{}'",
            relative,
            workspace_root.display()
        ));
    }
    Ok(final_path)
}

pub fn read_file(workspace_root: &Path, args: &Value) -> ToolResult {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let start_line = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize);
    let end_line = args
        .get("end_line")
        .and_then(|v| v.as_u64())
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
                    "Error: image too large ({} bytes). Use bash to resize first.",
                    metadata.len()
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
            return ToolResult::text(format!("data:{mime};base64,{encoded}"));
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
        ));
    }

    // For large files without explicit range, provide a helpful preview instead of error.
    // This auto-pagination helps the agent understand file structure without manual range specification.
    if !has_range && !outline && metadata.len() as usize > READ_FILE_SIZE_LIMIT {
        // Read head lines + count total via fast byte scan.
        // Phase 1: collect head lines (allocates Strings only for the first N lines).
        // Phase 2: scan the rest of the file in raw chunks counting '\n' bytes —
        // no per-line String allocation, no UTF-8 validation, ~10-100x faster than
        // read_line for the counting-only portion.
        const HEAD_LINES: usize = 50;
        const TAIL_LINES: usize = 20;

        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
        };
        let file_size = metadata.len();

        let mut reader = BufReader::new(file);
        let mut head_lines = Vec::with_capacity(HEAD_LINES);
        let mut line_buf = String::new();
        let mut total_lines = 0usize;

        // Phase 1: collect head lines only
        let mut io_error = false;
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    total_lines += 1;
                    if head_lines.len() < HEAD_LINES {
                        // Remove trailing newline for consistent formatting
                        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
                        head_lines.push(trimmed.to_string());
                    } else {
                        break; // Got enough head lines — stop allocating Strings
                    }
                }
                Err(e) => {
                    io_error = true;
                    astra_core::agent_warn!(
                        "fs",
                        "I/O error reading head lines of large file: {e}"
                    );
                    break;
                }
            }
        }

        // Phase 2: count remaining lines via raw byte scan (no String allocation)
        if !io_error {
            let mut chunk = [0u8; 8192];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        total_lines += chunk[..n].iter().filter(|&&b| b == b'\n').count();
                    }
                    Err(e) => {
                        io_error = true;
                        astra_core::agent_warn!(
                            "fs",
                            "I/O error counting lines in large file: {e}"
                        );
                        break;
                    }
                }
            }
        }

        // For tail, we need to re-read from near the end
        // Use a simple approach: seek backward and read last N lines
        let tail_lines = if total_lines > HEAD_LINES + TAIL_LINES {
            read_last_n_lines(&path, TAIL_LINES).unwrap_or_default()
        } else {
            Vec::new() // No tail needed if file fits in head
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
            file_size,
            total_lines,
            if io_error {
                " — partial read due to I/O error"
            } else {
                ""
            }
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
            && let Ok(content) = read_to_string_lossy(&path)
            && let Some(outline_str) = render_outline(&path, &content, total_lines)
        {
            preview.push_str(&outline_str);
        }

        // Truncate before appending tip so the tip is always intact when within limit.
        let limit = per_tool_output_limit("read_file");
        let tip = "\n**Tip**: Use `start_line`/`end_line` to read specific sections, or `outline=true` for definitions only.";
        if preview.len() + tip.len() > limit {
            preview = truncate_output(preview, limit.saturating_sub(tip.len()));
        }
        preview.push_str(tip);

        return ToolResult::text(preview);
    }

    let content = match read_to_string_lossy(&path) {
        Ok(content) => content,
        Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
    };
    let total_lines = content.lines().count();

    if outline {
        let rendered = render_outline(&path, &content, total_lines)
            .unwrap_or_else(|| no_definitions_outline_message(total_lines));
        return ToolResult::text(rendered);
    }

    if !has_range {
        let numbered = add_line_numbers(&content, 1);
        let limit = per_tool_output_limit("read_file");
        if numbered.len() > limit {
            let mut truncated = truncate_output(numbered, limit);
            truncated.push_str(&format!(
                "\n[file has {total_lines} lines — use start_line/end_line or outline=true]"
            ));
            return ToolResult::text(truncated);
        }
        return ToolResult::text(numbered);
    }

    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return ToolResult::text("(empty file)".into());
    }

    let start_line = start_line.unwrap_or(1);
    let end_line = end_line.unwrap_or(lines.len());
    let (start_line, end_line) = if start_line > end_line {
        (end_line, start_line)
    } else {
        (start_line, end_line)
    };
    let start = start_line.saturating_sub(1).min(lines.len());
    let end = end_line.min(lines.len());

    if start >= lines.len() {
        return ToolResult::error(format!(
            "Error: start_line {} exceeds file length {}",
            start + 1,
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

    let slice = lines[start..end].join("\n");
    let mut result = truncate_output(
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

fn read_to_string_lossy(path: &Path) -> std::io::Result<String> {
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
}

impl PreparedWriteFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn content_bytes(&self) -> &[u8] {
        self.content.as_bytes()
    }

    pub fn apply(&self) -> ToolResult {
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            return ToolResult::error(format!("Error: Cannot create directories: {e}"));
        }

        match std::fs::write(&self.path, &self.content) {
            Ok(()) => ToolResult::text(format!(
                "Successfully wrote {} bytes to {}",
                self.content.len(),
                self.path_str
            )),
            Err(e) => ToolResult::error(format!("Error: Cannot write file: {e}")),
        }
    }
}

pub fn prepare_write_file(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedWriteFile, ToolResult> {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Err(ToolResult::error("Error: Missing 'path' parameter".into())),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'content' parameter".into(),
            ));
        }
    };
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return Err(ToolResult::error(e)),
    };

    Ok(PreparedWriteFile {
        path,
        path_str: path_str.to_string(),
        content: content.to_string(),
    })
}

pub fn write_file(workspace_root: &Path, args: &Value) -> ToolResult {
    match prepare_write_file(workspace_root, args) {
        Ok(prepared) => prepared.apply(),
        Err(error) => error,
    }
}

#[derive(Debug)]
pub struct PreparedStrReplace {
    path: PathBuf,
    new_content: String,
    dry_run: bool,
    success_message: String,
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
        if self.dry_run {
            return ToolResult::text(self.success_message);
        }
        match std::fs::write(&self.path, &self.new_content) {
            Ok(()) => ToolResult::text(self.success_message),
            Err(e) => ToolResult::error(format!("Error: Cannot write file: {e}")),
        }
    }
}

pub fn prepare_str_replace(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedStrReplace, ToolResult> {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Err(ToolResult::error("Error: Missing 'path' parameter".into())),
    };
    let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'old_str' parameter".into(),
            ));
        }
    };
    let new_str = match args.get("new_str").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => {
            return Err(ToolResult::error(
                "Error: Missing 'new_str' parameter".into(),
            ));
        }
    };
    let replace_all = args
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return Err(ToolResult::error(e)),
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return Err(ToolResult::error(format!("Error: Cannot read file: {e}"))),
    };

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
            let new_content = if replace_all {
                content.replace(fuzzy_match.actual, &replacement)
            } else {
                content.replacen(fuzzy_match.actual, &replacement, 1)
            };
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
                success_message,
            });
        }

        if replace_all && normalized_quote_count > 1 {
            return Err(ToolResult::error(format!(
                "Error: old_str matches {normalized_quote_count} occurrences in {path_str} after normalizing curly quotes, but the file contains mixed curly quote forms. Cannot safely replace_all with inconsistent quoting styles."
            )));
        }

        return Err(ToolResult::error(str_replace_not_found_hint(
            path_str, &content, old_str,
        )));
    }
    if count > 1 && !replace_all {
        return Err(ToolResult::error(format!(
            "Error: old_str found {count} times in {path_str}. Make old_str more specific to match exactly once."
        )));
    }

    let new_content = if replace_all {
        content.replace(old_str, new_str)
    } else {
        content.replacen(old_str, new_str, 1)
    };
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
        success_message,
    })
}

pub fn str_replace(workspace_root: &Path, args: &Value) -> ToolResult {
    match prepare_str_replace(workspace_root, args) {
        Ok(prepared) => prepared.apply(),
        Err(error) => error,
    }
}

#[derive(Debug)]
pub struct PreparedMultiEdit {
    path: PathBuf,
    path_str: String,
    new_content: String,
    edit_count: usize,
    dry_run: bool,
}

impl PreparedMultiEdit {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn new_content_bytes(&self) -> &[u8] {
        self.new_content.as_bytes()
    }

    pub fn apply(&self) -> ToolResult {
        if self.dry_run {
            return ToolResult::text(format!(
                "Dry run: {} edit(s) would be applied to {}",
                self.edit_count, self.path_str
            ));
        }

        match std::fs::write(&self.path, &self.new_content) {
            Ok(()) => ToolResult::text(format!(
                "Successfully applied {} edit(s) to {}",
                self.edit_count, self.path_str
            )),
            Err(e) => ToolResult::error(format!("Error: Cannot write file: {e}")),
        }
    }
}

pub fn prepare_multi_edit(
    workspace_root: &Path,
    args: &Value,
) -> Result<PreparedMultiEdit, ToolResult> {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return Err(ToolResult::error("Error: Missing 'path' parameter".into())),
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

    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return Err(ToolResult::error(e)),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return Err(ToolResult::error(format!("Error: Cannot read file: {e}"))),
    };

    let mut working = content;
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
        if old_str == new_str {
            return Err(ToolResult::error(format!(
                "Error: edit[{i}] old_str and new_str are identical"
            )));
        }
        let count = working.matches(old_str).count();
        if count == 0 {
            return Err(ToolResult::error(format!(
                "Error: edit[{i}] old_str not found in {path_str}"
            )));
        }
        if count > 1 {
            return Err(ToolResult::error(format!(
                "Error: edit[{i}] old_str found {count} times in {path_str}. Must match exactly once."
            )));
        }
        working = working.replacen(old_str, new_str, 1);
    }

    Ok(PreparedMultiEdit {
        path,
        path_str: path_str.to_string(),
        new_content: working,
        edit_count: edits.len(),
        dry_run,
    })
}

#[derive(Debug)]
pub struct PreparedDeleteFile {
    path: PathBuf,
    path_str: String,
    before_content: Vec<u8>,
}

impl PreparedDeleteFile {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn before_content(&self) -> &[u8] {
        &self.before_content
    }

    pub fn apply(&self) -> ToolResult {
        match std::fs::remove_file(&self.path) {
            Ok(()) => ToolResult::text(format!("Successfully deleted {}", self.path_str)),
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
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return Err(ToolResult::error(e)),
    };

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

    Ok(PreparedDeleteFile {
        path,
        path_str: path_str.to_string(),
        before_content,
    })
}

pub fn delete_file(workspace_root: &Path, args: &Value) -> ToolResult {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };

    if !path.exists() {
        return ToolResult::error(format!("Error: File not found: {path_str}"));
    }

    match std::fs::remove_file(&path) {
        Ok(()) => ToolResult::text(format!("Successfully deleted {path_str}")),
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
    ToolResult::text(result.join("\n"))
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

fn str_replace_not_found_hint(path_str: &str, content: &str, old_str: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<&str> = old_str.lines().collect();
    let mut msg = format!("Error: old_str not found in {path_str}.\n");
    let mut has_specific_hint = false;

    let normalized_old = normalize_ws(old_str);
    let normalized_content = normalize_ws(content);
    if normalized_content.contains(&normalized_old) {
        msg.push_str(
            "Hint: A whitespace-normalized match exists. Check indentation/trailing spaces.\n",
        );
        if let Some(first_line) = old_lines.first() {
            let normalized_first = normalize_ws(first_line);
            for (idx, line) in lines.iter().enumerate() {
                if normalize_ws(line) == normalized_first {
                    msg.push_str(&format!("  Possible match at line {}\n", idx + 1));
                    let end = (idx + old_lines.len().min(5)).min(lines.len());
                    for (line_offset, line_content) in lines[idx..end].iter().enumerate() {
                        msg.push_str(&format!("  {}: {}\n", idx + line_offset + 1, line_content));
                    }
                    break;
                }
            }
        }
        return msg;
    }

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
                msg.push_str(&format!(
                    "Hint: First line of old_str ('{}') found at line(s): {:?}\n",
                    truncate_chars(needle, 60),
                    matches
                ));
                let line_idx = matches[0] - 1;
                let start = line_idx;
                let end = (line_idx + old_lines.len()).min(lines.len());
                msg.push_str("Actual file content:\n");
                for (line_offset, line_content) in lines[start..end].iter().enumerate() {
                    msg.push_str(&format!(
                        "  {}: {}\n",
                        start + line_offset + 1,
                        line_content
                    ));
                }
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
                "Hint: {matching_count}/{} lines from old_str exist individually in the file.\n",
                old_lines.len()
            ));
        }
    }

    if !has_specific_hint {
        msg.push_str(
            "Hint: Use read_file with start_line/end_line to verify the exact content before retrying.\n",
        );
    }
    msg
}

fn truncate_chars(s: &str, max_chars: usize) -> String {
    let mut chars = s.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}...")
    } else {
        truncated
    }
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
        // Should have tip about using start_line/end_line
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

    #[test]
    fn read_file_swaps_reversed_ranges() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("test.txt"), "a\nb\nc\nd").unwrap();

        let result = read_file(
            tmp.path(),
            &serde_json::json!({"path": "test.txt", "start_line": 4, "end_line": 2}),
        );

        assert!(!result.is_error);
        assert!(result.output.contains("2\tb"), "got: {}", result.output);
        assert!(result.output.contains("3\tc"), "got: {}", result.output);
        assert!(result.output.contains("4\td"), "got: {}", result.output);
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
    fn str_replace_basic() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), "foo bar baz").unwrap();
        let args = serde_json::json!({"path": "f.txt", "old_str": "bar", "new_str": "qux"});
        let result = str_replace(tmp.path(), &args);
        assert!(!result.is_error);
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "foo qux baz");
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
    fn str_replace_not_found_reports_whitespace_hint() {
        let msg = str_replace_not_found_hint(
            "f.txt",
            "  fn hello() {\n    println!(\"hi\");\n  }\n",
            "fn hello() {\n  println!(\"hi\");\n}",
        );
        assert!(msg.contains("whitespace-normalized"), "got: {msg}");
        assert!(msg.contains("line 1"), "got: {msg}");
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
        assert_eq!(content, "let x = \u{201C}world\u{201D};");
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

    // ─── Issue #3: not-found hint O(n*m) → use HashSet ──────────────────
    #[test]
    fn str_replace_not_found_hint_first_line_match_branch() {
        let msg = str_replace_not_found_hint(
            "f.txt",
            "fn foo() {\n    bar();\n    baz();\n}\n",
            "fn foo() {\n    bar();\n    qux();\n}",
        );
        assert!(msg.contains("First line"), "got: {msg}");
        assert!(msg.contains("fn foo()"), "got: {msg}");
        assert!(msg.contains("Actual file content"), "got: {msg}");
    }

    #[test]
    fn str_replace_not_found_hint_generic_fallback() {
        let msg = str_replace_not_found_hint(
            "f.txt",
            "totally different content\nno matches at all\n",
            "something completely unrelated",
        );
        assert!(msg.contains("read_file"), "got: {msg}");
    }

    #[test]
    fn str_replace_not_found_hint_individual_lines_branch() {
        let msg = str_replace_not_found_hint("f.txt", "aaa\nbbb\nccc\n", "aaa\nXXX\nccc");
        assert!(
            msg.contains("lines from old_str exist individually"),
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

    // ─── not-found hint: multiline with zero individual matches ─────────
    #[test]
    fn str_replace_not_found_hint_no_individual_line_matches() {
        let msg =
            str_replace_not_found_hint("f.txt", "real content here\n", "xxxxxxx\nyyyyyyy\nzzzzzzz");
        assert!(
            msg.contains("read_file"),
            "expected generic fallback, got: {msg}"
        );
        assert!(!msg.contains("lines from old_str exist"), "got: {msg}");
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

    // ─── Issue #3: first-line hint context should not show extra line ───
    #[test]
    fn str_replace_not_found_hint_first_line_shows_exact_span() {
        // File has 6 lines; old_str has 3 lines starting at line 2.
        // Hint should show exactly old_lines.len() lines of context, not +1.
        let content = "header\nfn foo() {\n    bar();\n    baz();\n}\nfooter\n";
        let old_str = "fn foo() {\n    bar();\n    qux();\n}";
        let msg = str_replace_not_found_hint("f.txt", content, old_str);
        // Should show lines 2..5 (4 lines = old_lines.len()), not line 6
        assert!(msg.contains("Actual file content"), "got: {msg}");
        assert!(
            !msg.contains("footer"),
            "should not show extra line beyond old_str span, got: {msg}"
        );
    }

    // ─── truncate_chars ─────────────────────────────────────────────────
    #[test]
    fn truncate_chars_short_string_unchanged() {
        assert_eq!(truncate_chars("hello", 10), "hello");
    }

    #[test]
    fn truncate_chars_exact_length_no_ellipsis() {
        assert_eq!(truncate_chars("hello", 5), "hello");
    }

    #[test]
    fn truncate_chars_truncates_with_ellipsis() {
        assert_eq!(truncate_chars("hello world", 5), "hello...");
    }

    #[test]
    fn truncate_chars_empty_string() {
        assert_eq!(truncate_chars("", 5), "");
    }

    // ─── Issue #3: DiffOp should be Copy ────────────────────────────────
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
        assert_eq!(content, "AAA bbb CCC");
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
        // Original file should be unchanged (atomic)
        let content = std::fs::read_to_string(tmp.path().join("f.txt")).unwrap();
        assert_eq!(content, "aaa bbb");
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
}
