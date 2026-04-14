//! File operations: read, write, str_replace, delete, list_dir.
//!
//! All operations are sandboxed to a workspace root directory. Path traversal
//! via `..` is normalized before the boundary check to prevent escapes.

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::ToolResult;

/// Resolve a relative path against workspace_root with normalization.
///
/// Returns an error if the resolved path escapes the workspace boundary.
pub fn resolve_path(workspace_root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = if Path::new(relative).is_absolute() {
        PathBuf::from(relative)
    } else {
        workspace_root.join(relative)
    };

    let normalized = path.components().fold(PathBuf::new(), |mut acc, c| {
        match c {
            std::path::Component::ParentDir => {
                acc.pop();
            }
            std::path::Component::CurDir => {}
            other => acc.push(other),
        }
        acc
    });

    let final_path = if normalized.exists() {
        normalized
            .canonicalize()
            .map_err(|e| format!("Cannot resolve path: {e}"))?
    } else {
        normalized
    };

    if !final_path.starts_with(workspace_root) {
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
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
    };

    let start_line = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize);
    let end_line = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|l| l as usize);

    let lines: Vec<&str> = content.lines().collect();
    let start = start_line.unwrap_or(1).saturating_sub(1);
    let end = end_line.unwrap_or(lines.len()).min(lines.len());

    if start >= lines.len() {
        return ToolResult::error(format!(
            "Error: start_line {} exceeds file length {}",
            start + 1,
            lines.len()
        ));
    }

    let mut result = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        result.push_str(&format!("{}\t{}\n", start + i + 1, line));
    }
    ToolResult::text(result)
}

pub fn write_file(workspace_root: &Path, args: &Value) -> ToolResult {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    let content = match args.get("content").and_then(|v| v.as_str()) {
        Some(c) => c,
        None => return ToolResult::error("Error: Missing 'content' parameter".into()),
    };
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };

    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        return ToolResult::error(format!("Error: Cannot create directories: {e}"));
    }

    match std::fs::write(&path, content) {
        Ok(()) => ToolResult::text(format!(
            "Successfully wrote {} bytes to {}",
            content.len(),
            path_str
        )),
        Err(e) => ToolResult::error(format!("Error: Cannot write file: {e}")),
    }
}

pub fn str_replace(workspace_root: &Path, args: &Value) -> ToolResult {
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    let old_str = match args.get("old_str").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolResult::error("Error: Missing 'old_str' parameter".into()),
    };
    let new_str = match args.get("new_str").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return ToolResult::error("Error: Missing 'new_str' parameter".into()),
    };
    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
    };

    let count = content.matches(old_str).count();
    if count == 0 {
        return ToolResult::error(format!("Error: old_str not found in {path_str}"));
    }
    if count > 1 {
        return ToolResult::error(format!(
            "Error: old_str found {count} times in {path_str}. Make old_str more specific to match exactly once."
        ));
    }

    let new_content = content.replacen(old_str, new_str, 1);
    match std::fs::write(&path, &new_content) {
        Ok(()) => ToolResult::text(format!("Successfully replaced text in {path_str}")),
        Err(e) => ToolResult::error(format!("Error: Cannot write file: {e}")),
    }
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
    let path_str = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    let edits = match args.get("edits").and_then(|v| v.as_array()) {
        Some(e) => e,
        None => return ToolResult::error("Error: Missing 'edits' array".into()),
    };
    if edits.is_empty() {
        return ToolResult::error("Error: 'edits' array is empty".into());
    }
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let path = match resolve_path(workspace_root, path_str) {
        Ok(p) => p,
        Err(e) => return ToolResult::error(e),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolResult::error(format!("Error: Cannot read file: {e}")),
    };

    // Validate all edits first (atomic: all or nothing)
    let mut working = content.clone();
    for (i, edit) in edits.iter().enumerate() {
        let old_str = match edit.get("old_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::error(format!("Error: edit[{i}] missing 'old_str'")),
        };
        let new_str = match edit.get("new_str").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return ToolResult::error(format!("Error: edit[{i}] missing 'new_str'")),
        };
        if old_str == new_str {
            return ToolResult::error(format!(
                "Error: edit[{i}] old_str and new_str are identical"
            ));
        }
        let count = working.matches(old_str).count();
        if count == 0 {
            return ToolResult::error(format!("Error: edit[{i}] old_str not found in {path_str}"));
        }
        if count > 1 {
            return ToolResult::error(format!(
                "Error: edit[{i}] old_str found {count} times in {path_str}. Must match exactly once."
            ));
        }
        working = working.replacen(old_str, new_str, 1);
    }

    if dry_run {
        return ToolResult::text(format!(
            "Dry run: {} edit(s) would be applied to {path_str}",
            edits.len()
        ));
    }

    match std::fs::write(&path, &working) {
        Ok(()) => ToolResult::text(format!(
            "Successfully applied {} edit(s) to {path_str}",
            edits.len()
        )),
        Err(e) => ToolResult::error(format!("Error: Cannot write file: {e}")),
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
