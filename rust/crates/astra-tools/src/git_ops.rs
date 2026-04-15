//! Git operations: status, diff, log, show, blame, commit, revert.

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

fn resolve_commit_ref(workspace_root: &Path, commit_ref: &str) -> Option<String> {
    std::process::Command::new("git")
        .args(["rev-parse", "--verify", commit_ref])
        .current_dir(workspace_root)
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn short_commit_sha(commit_sha: &str) -> String {
    commit_sha[..7.min(commit_sha.len())].to_string()
}

fn abort_git_revert(workspace_root: &Path) -> Result<bool, String> {
    let output = std::process::Command::new("git")
        .args(["revert", "--abort"])
        .current_dir(workspace_root)
        .output()
        .map_err(|error| format!("Error: git revert --abort failed: {error}"))?;
    if output.status.success() {
        Ok(true)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("no cherry-pick or revert in progress") {
            Ok(false)
        } else {
            Err(format!(
                "Error: git revert --abort failed: {}",
                stderr.trim()
            ))
        }
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

    let output = std::process::Command::new("git")
        .args(["commit", "-m", message])
        .current_dir(workspace_root)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let commit_sha = resolve_commit_ref(workspace_root, "HEAD");
            let short_hash = commit_sha
                .as_deref()
                .map(short_commit_sha)
                .unwrap_or_else(|| "???".to_string());
            let metadata = commit_sha.map(|commit_sha| {
                serde_json::Map::from_iter([
                    ("commit_sha".to_string(), Value::String(commit_sha.clone())),
                    (
                        "commit_short_sha".to_string(),
                        Value::String(short_commit_sha(&commit_sha)),
                    ),
                ])
            });
            ToolResult {
                output: format!("✓ Committed: {short_hash} {message}"),
                metadata,
                is_error: false,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("nothing to commit") {
                ToolResult::text("Nothing to commit — working tree clean".to_string())
            } else {
                ToolResult::error(format!("Error: git commit failed: {}", stderr.trim()))
            }
        }
        Err(error) => ToolResult::error(format!("Error: git commit failed: {error}")),
    }
}

pub fn revert_commit(workspace_root: &Path, args: &Value) -> ToolResult {
    let commit_ref = match args.get("commit_sha").and_then(|v| v.as_str()) {
        Some(commit_ref) if !commit_ref.trim().is_empty() => commit_ref.trim().to_string(),
        _ => return ToolResult::error("Error: Missing 'commit_sha' parameter".into()),
    };
    let target_commit_sha = match resolve_commit_ref(workspace_root, &commit_ref) {
        Some(commit_sha) => commit_sha,
        None => return ToolResult::error(format!("Error: unknown commit '{commit_ref}'")),
    };

    match std::process::Command::new("git")
        .args(["revert", "--no-edit", target_commit_sha.as_str()])
        .current_dir(workspace_root)
        .output()
    {
        Ok(out) if out.status.success() => {
            let revert_commit_sha = resolve_commit_ref(workspace_root, "HEAD");
            let revert_short_sha = revert_commit_sha
                .as_deref()
                .map(short_commit_sha)
                .unwrap_or_else(|| "???".to_string());
            let metadata = revert_commit_sha.map(|revert_commit_sha| {
                serde_json::Map::from_iter([
                    (
                        "reverted_commit_sha".to_string(),
                        Value::String(target_commit_sha.clone()),
                    ),
                    (
                        "reverted_commit_short_sha".to_string(),
                        Value::String(short_commit_sha(&target_commit_sha)),
                    ),
                    (
                        "revert_commit_sha".to_string(),
                        Value::String(revert_commit_sha.clone()),
                    ),
                    (
                        "revert_commit_short_sha".to_string(),
                        Value::String(short_commit_sha(&revert_commit_sha)),
                    ),
                ])
            });
            ToolResult {
                output: format!(
                    "✓ Reverted commit: {} via {}",
                    short_commit_sha(&target_commit_sha),
                    revert_short_sha
                ),
                metadata,
                is_error: false,
            }
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let mut message = format!("Error: git revert failed: {}", stderr.trim());
            match abort_git_revert(workspace_root) {
                Ok(true) => message.push_str(" (aborted in-progress revert)"),
                Ok(false) => {}
                Err(error) => message.push_str(&format!(" ({error})")),
            }
            ToolResult::error(message)
        }
        Err(error) => ToolResult::error(format!("Error: git revert failed: {error}")),
    }
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

    #[test]
    fn commit_returns_metadata_and_revert_restores_state() {
        let tmp = TempDir::new().unwrap();
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(tmp.path())
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(tmp.path())
            .output()
            .unwrap();

        let tracked = tmp.path().join("tracked.txt");
        std::fs::write(&tracked, "original\n").unwrap();
        let initial = commit(tmp.path(), &serde_json::json!({"message": "initial"}));
        assert!(!initial.is_error, "got: {}", initial.output);

        std::fs::write(&tracked, "changed\n").unwrap();
        let committed = commit(
            tmp.path(),
            &serde_json::json!({"message": "change tracked"}),
        );
        assert!(!committed.is_error, "got: {}", committed.output);
        let commit_sha = committed
            .metadata
            .as_ref()
            .and_then(|fields| fields.get("commit_sha"))
            .and_then(Value::as_str)
            .expect("commit_sha metadata");

        let reverted = revert_commit(tmp.path(), &serde_json::json!({"commit_sha": commit_sha}));
        assert!(!reverted.is_error, "got: {}", reverted.output);
        assert_eq!(std::fs::read_to_string(&tracked).unwrap(), "original\n");
        let revert_fields = reverted.metadata.as_ref().expect("revert metadata");
        assert_eq!(
            revert_fields["reverted_commit_sha"].as_str(),
            Some(commit_sha)
        );
        assert!(revert_fields["revert_commit_sha"].as_str().is_some());
    }
}
