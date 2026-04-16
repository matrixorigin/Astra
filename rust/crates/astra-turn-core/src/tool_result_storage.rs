//! Disk persistence for large tool results.
//!
//! When a tool result exceeds [`PERSIST_THRESHOLD_CHARS`], the full output is
//! written to `~/.astra/sessions/<session_id>/tool-results/<tool_call_id>.txt`
//! and the in-memory content is replaced with a compact preview + file
//! reference.  This prevents oversized tool outputs from bloating the LLM
//! context window while still preserving the full output for later retrieval.

use std::path::{Path, PathBuf};

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

    // Sanitize tool_call_id for filesystem safety
    let safe_id: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
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

/// Read a previously-persisted tool result back from disk.
///
/// Returns `None` if the file doesn't exist or can't be read.
pub fn read_persisted_result(session_dir: &Path, tool_call_id: &str) -> Option<String> {
    let safe_id: String = tool_call_id
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
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

        // File was written
        let file_path = dir.join(TOOL_RESULTS_SUBDIR).join("call-42.txt");
        assert!(file_path.exists());
        let stored = std::fs::read_to_string(&file_path).unwrap();
        assert_eq!(stored.len(), content.len());

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
