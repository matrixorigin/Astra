//! File operations: read, write, str_replace, delete, list_dir.
//!
//! All operations are sandboxed to a workspace root directory. Path traversal
//! via `..` is normalized before the boundary check to prevent escapes.

use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::Value;

use crate::code_intel;
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
    path_str: String,
    new_content: String,
}

impl PreparedStrReplace {
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn new_content_bytes(&self) -> &[u8] {
        self.new_content.as_bytes()
    }

    pub fn apply(&self) -> ToolResult {
        match std::fs::write(&self.path, &self.new_content) {
            Ok(()) => ToolResult::text(format!("Successfully replaced text in {}", self.path_str)),
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
        return Err(ToolResult::error(format!(
            "Error: old_str not found in {path_str}"
        )));
    }
    if count > 1 {
        return Err(ToolResult::error(format!(
            "Error: old_str found {count} times in {path_str}. Make old_str more specific to match exactly once."
        )));
    }

    Ok(PreparedStrReplace {
        path,
        path_str: path_str.to_string(),
        new_content: content.replacen(old_str, new_str, 1),
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
