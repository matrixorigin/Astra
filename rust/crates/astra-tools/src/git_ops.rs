//! Git operations: status, diff, log, show, blame, commit.

use std::path::Path;

use serde_json::Value;

use crate::ToolResult;

fn git_command(workspace_root: &Path, git_args: &[&str]) -> ToolResult {
    let output = std::process::Command::new("git")
        .args(git_args)
        .current_dir(workspace_root)
        .output();

    match output {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            if out.status.success() {
                ToolResult::text(stdout)
            } else {
                ToolResult::error(format!("Error: git {}: {}", git_args.join(" "), stderr))
            }
        }
        Err(e) => ToolResult::error(format!("Error: git command failed: {e}")),
    }
}

pub fn status(workspace_root: &Path) -> ToolResult {
    git_command(workspace_root, &["status", "--porcelain", "-b"])
}

pub fn diff(workspace_root: &Path, args: &Value) -> ToolResult {
    let mut git_args = vec!["diff"];
    if let Some(true) = args.get("staged").and_then(|v| v.as_bool()) {
        git_args.push("--cached");
    }
    let path_val;
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        git_args.push("--");
        path_val = path.to_string();
        git_args.push(&path_val);
    }
    git_command(workspace_root, &git_args)
}

pub fn log(workspace_root: &Path, args: &Value) -> ToolResult {
    let n = args
        .get("n")
        .and_then(|v| v.as_u64())
        .unwrap_or(10)
        .min(100);
    let n_str = format!("-{n}");
    let mut git_args = vec!["log", "--oneline", &n_str];
    let path_val;
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        git_args.push("--");
        path_val = path.to_string();
        git_args.push(&path_val);
    }
    git_command(workspace_root, &git_args)
}

pub fn show(workspace_root: &Path, args: &Value) -> ToolResult {
    let revision = args
        .get("revision")
        .and_then(|v| v.as_str())
        .unwrap_or("HEAD");
    git_command(workspace_root, &["show", "--stat", revision])
}

pub fn blame(workspace_root: &Path, args: &Value) -> ToolResult {
    let path = match args.get("path").and_then(|v| v.as_str()) {
        Some(p) => p,
        None => return ToolResult::error("Error: Missing 'path' parameter".into()),
    };
    git_command(workspace_root, &["blame", "--line-porcelain", path])
}

pub fn commit(workspace_root: &Path, args: &Value) -> ToolResult {
    let message = match args.get("message").and_then(|v| v.as_str()) {
        Some(m) => m,
        None => return ToolResult::error("Error: Missing 'message' parameter".into()),
    };

    // Stage all changes first.
    let stage_result = git_command(workspace_root, &["add", "-A"]);
    if stage_result.is_error {
        return stage_result;
    }

    git_command(workspace_root, &["commit", "-m", message])
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn status_in_non_git_dir() {
        let tmp = TempDir::new().unwrap();
        let result = status(tmp.path());
        assert!(result.is_error);
    }

    #[test]
    fn status_in_git_dir() {
        let tmp = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        let result = status(tmp.path());
        assert!(!result.is_error);
    }
}
