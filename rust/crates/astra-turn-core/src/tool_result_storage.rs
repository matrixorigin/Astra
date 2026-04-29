//! Disk persistence for large tool results.
//!
//! When a tool result exceeds [`PERSIST_THRESHOLD_CHARS`], the full output is
//! written to `~/.astra/sessions/<session_id>/tool-results/<tool_call_id>.txt`
//! and the in-memory content is replaced with a compact preview + file
//! reference.  This prevents oversized tool outputs from bloating the LLM
//! context window while still preserving the full output for later retrieval.

use std::path::{Path, PathBuf};

/// Maximum length of the human-readable portion of a sanitized filename.
/// Full filename has an 8-char hex hash suffix to prevent collisions when
/// different `tool_call_id`s sanitize to the same string.
const SAFE_ID_MAX_READABLE: usize = 64;

/// Sanitize a tool_call_id into a filesystem-safe filename stem.
///
/// Replaces every non-`[A-Za-z0-9_-]` character with `_`, truncates the
/// readable portion, and appends an 8-char hex hash of the original id to
/// prevent collisions (e.g. `a/b` and `a_b` would otherwise both map to `a_b`).
fn safe_filename_stem(tool_call_id: &str) -> String {
    let mut readable: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if readable.chars().count() > SAFE_ID_MAX_READABLE {
        readable = readable.chars().take(SAFE_ID_MAX_READABLE).collect();
    }
    // FNV-1a 64-bit: stable and deterministic across processes/Rust versions,
    // unlike std::collections::hash_map::DefaultHasher which has no stability guarantees.
    let suffix = fnv1a_64(tool_call_id.as_bytes());
    format!("{readable}-{suffix:016x}")
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Tool results larger than this (in chars) are persisted to disk.
/// Default 30 000 chars ≈ ~7 500 tokens.
pub const PERSIST_THRESHOLD_CHARS: usize = 30_000;

/// Number of chars to include as a preview in the replacement message.
const PREVIEW_CHARS: usize = 2_000;

/// XML-style tag that wraps the persisted-output reference.
const PERSISTED_TAG_OPEN: &str = "<persisted-output>";
const PERSISTED_TAG_CLOSE: &str = "</persisted-output>";

/// Subdirectory under the session folder for tool result files.
const TOOL_RESULTS_SUBDIR: &str = "tool-results";

/// FNV-1a 64-bit hash — stable and deterministic across processes and Rust versions.
fn fnv1a_64(data: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in data {
        hash ^= b as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// If `content` exceeds the persistence threshold, write it to disk and return
/// a compact replacement string with a preview and file path.
///
/// Returns `None` if the content is small enough to keep inline, or if disk
/// persistence fails (in which case the caller should use the original content).
///
/// `session_dir` is `~/.astra/sessions/<session_id>/`.
pub fn maybe_persist_tool_result(
    session_dir: &Path,
    tool_call_id: &str,
    tool_name: &str,
    content: &str,
) -> Option<String> {
    if content.chars().count() <= PERSIST_THRESHOLD_CHARS {
        return None;
    }

    let dir = session_dir.join(TOOL_RESULTS_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "[tool_result_storage] failed to create dir {}: {e}",
            dir.display()
        );
        return None;
    }

    // Sanitize tool_call_id for filesystem safety (with hash suffix to avoid collisions)
    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = dir.join(format!("{safe_id}.txt"));

    if let Err(e) = std::fs::write(&file_path, content) {
        eprintln!(
            "[tool_result_storage] failed to write {}: {e}",
            file_path.display()
        );
        return None;
    }

    Some(build_replacement(tool_name, content, &file_path))
}

/// Persist a tool result to disk unconditionally (no size threshold).
///
/// Used by compaction to save full content before clearing. Unlike
/// `maybe_persist_tool_result`, this always writes regardless of content size.
/// Returns `true` on success.
pub fn maybe_persist_tool_result_unconditional(
    session_dir: &Path,
    tool_call_id: &str,
    // tool_name is reserved for future metadata embedding in the persisted file header.
    // Currently unused because the file is identified solely by tool_call_id.
    _tool_name: &str,
    content: &str,
) -> bool {
    let dir = session_dir.join(TOOL_RESULTS_SUBDIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!(
            "[tool_result_storage] failed to create dir {}: {e}",
            dir.display()
        );
        return false;
    }

    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = dir.join(format!("{safe_id}.txt"));

    if let Err(e) = std::fs::write(&file_path, content) {
        eprintln!(
            "[tool_result_storage] failed to write {}: {e}",
            file_path.display()
        );
        return false;
    }
    true
}

/// Read a previously-persisted tool result back from disk.
///
/// Returns `None` if the file doesn't exist or can't be read.
pub fn read_persisted_result(session_dir: &Path, tool_call_id: &str) -> Option<String> {
    let safe_id = safe_filename_stem(tool_call_id);
    let file_path = session_dir
        .join(TOOL_RESULTS_SUBDIR)
        .join(format!("{safe_id}.txt"));
    std::fs::read_to_string(file_path).ok()
}

/// Return the storage directory for tool results under a session.
pub fn tool_results_dir(session_dir: &Path) -> PathBuf {
    session_dir.join(TOOL_RESULTS_SUBDIR)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn build_replacement(tool_name: &str, content: &str, file_path: &Path) -> String {
    let total_chars = content.chars().count();
    let preview: String = content.chars().take(PREVIEW_CHARS).collect();

    // Try to cut at a newline for cleaner preview
    let preview = if let Some(nl_pos) = preview.rfind('\n') {
        if nl_pos > PREVIEW_CHARS / 2 {
            &preview[..nl_pos]
        } else {
            &preview
        }
    } else {
        &preview
    };

    format!(
        "{PERSISTED_TAG_OPEN}\n\
         Tool `{tool_name}` produced {total_chars} chars of output (persisted to disk).\n\
         File: {path}\n\
         \n\
         Preview (first ~{prev_len} chars):\n\
         {preview}\n\
         ...[truncated — full output persisted at path above]\n\
         {PERSISTED_TAG_CLOSE}",
        path = file_path.display(),
        prev_len = preview.len(),
    )
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_result_returns_none() {
        let dir = std::env::temp_dir().join("trs_small");
        let _ = std::fs::create_dir_all(&dir);
        let content = "hello world";
        assert!(maybe_persist_tool_result(&dir, "call-1", "bash", content).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn large_result_persisted_and_replaced() {
        let dir = std::env::temp_dir().join("trs_large");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let content = "x".repeat(PERSIST_THRESHOLD_CHARS + 100);
        let replacement = maybe_persist_tool_result(&dir, "call-42", "bash", &content).unwrap();

        // Replacement contains the tag
        assert!(replacement.contains(PERSISTED_TAG_OPEN));
        assert!(replacement.contains(PERSISTED_TAG_CLOSE));
        assert!(replacement.contains("bash"));
        assert!(replacement.contains("persisted to disk"));

        // File was written (name is `<safe_id>-<hash>.txt` to avoid collisions)
        let results_dir = dir.join(TOOL_RESULTS_SUBDIR);
        let entries: Vec<_> = std::fs::read_dir(&results_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        let file_path = entries[0].path();
        let fname = file_path.file_name().unwrap().to_string_lossy();
        assert!(fname.starts_with("call-42-"));
        assert!(fname.ends_with(".txt"));
        let stored = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(stored.len(), content.len());

        // Roundtrip read via public API
        let recovered = read_persisted_result(&dir, "call-42").unwrap();
        assert_eq!(recovered, content);

        // Replacement is much smaller than original
        assert!(replacement.len() < content.len() / 5);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_persisted_result_roundtrip() {
        let dir = std::env::temp_dir().join("trs_read");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let content = "y".repeat(PERSIST_THRESHOLD_CHARS + 50);
        let _ = maybe_persist_tool_result(&dir, "call-99", "grep", &content);

        let recovered = read_persisted_result(&dir, "call-99").unwrap();
        assert_eq!(recovered, content);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_persisted_result_missing_returns_none() {
        let dir = std::env::temp_dir().join("trs_missing");
        let _ = std::fs::create_dir_all(&dir);
        assert!(read_persisted_result(&dir, "nonexistent").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sanitizes_tool_call_id_for_filesystem() {
        let dir = std::env::temp_dir().join("trs_sanitize");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        let content = "z".repeat(PERSIST_THRESHOLD_CHARS + 10);
        let replacement =
            maybe_persist_tool_result(&dir, "call/../../etc/passwd", "bash", &content);
        assert!(replacement.is_some());

        // Verify the file was created (with sanitized name)
        let results_dir = dir.join(TOOL_RESULTS_SUBDIR);
        assert!(results_dir.exists());
        let entries: Vec<_> = std::fs::read_dir(&results_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        // The filename should not contain path separators
        let filename = entries[0].file_name().to_string_lossy().to_string();
        assert!(!filename.contains('/'));
        assert!(!filename.contains(".."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_cuts_at_newline() {
        let dir = std::env::temp_dir().join("trs_preview_nl");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);

        // Build content with newlines
        let mut content = String::new();
        for i in 0..5000 {
            content.push_str(&format!("line {i}\n"));
        }

        let replacement = maybe_persist_tool_result(&dir, "call-nl", "bash", &content).unwrap();
        // Preview should end at a clean newline
        assert!(replacement.contains("Preview"));
        assert!(replacement.contains("line "));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fnv1a_64_known_vectors() {
        // empty string → offset basis
        assert_eq!(super::fnv1a_64(b""), 0xcbf29ce484222325);
        // "a" → well-known FNV-1a-64 value
        assert_eq!(super::fnv1a_64(b"a"), 0xaf63dc4c8601ec8c);
    }

    #[test]
    fn threshold_boundary_exact() {
        let dir = std::env::temp_dir().join("trs_boundary");
        let _ = std::fs::create_dir_all(&dir);

        // Exactly at threshold → not persisted
        let at_limit = "a".repeat(PERSIST_THRESHOLD_CHARS);
        assert!(maybe_persist_tool_result(&dir, "c1", "bash", &at_limit).is_none());

        // One over → persisted
        let over_limit = "a".repeat(PERSIST_THRESHOLD_CHARS + 1);
        assert!(maybe_persist_tool_result(&dir, "c2", "bash", &over_limit).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
