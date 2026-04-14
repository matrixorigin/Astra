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
}
