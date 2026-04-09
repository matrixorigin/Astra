//! Git worktree session management for isolated branch work.
//!
//! Provides enter/exit workflow for git worktrees with session tracking,
//! change counting, and safe cleanup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use super::ToolExecutor;

/// State for an active worktree session created by `git_worktree enter`.
/// Tracks the worktree path, branch, and original directory for restoration.
#[derive(Debug, Clone)]
pub struct WorktreeSession {
    /// Path to the worktree directory (new working root).
    pub worktree_path: PathBuf,
    /// Branch name of the worktree.
    pub branch_name: String,
    /// Original project root to restore on `exit`.
    pub original_root: PathBuf,
    /// Git commit SHA at the time the worktree was created.
    pub original_head_commit: Option<String>,
}

/// Extract owner/repo from git remote URLs in the given directory.
/// Returns lowercased "owner/repo" strings for all GitHub remotes.
pub(super) fn detect_git_remote_repos(project_root: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["remote", "-v"])
        .current_dir(project_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut repos = Vec::new();
    for line in stdout.lines() {
        if let Some(repo) = extract_github_owner_repo(line) {
            let lower = repo.to_lowercase();
            if !repos.contains(&lower) {
                repos.push(lower);
            }
        }
    }
    repos
}

/// Parse owner/repo from a GitHub remote URL (SSH or HTTPS).
pub(super) fn extract_github_owner_repo(remote_line: &str) -> Option<String> {
    // SSH:   git@github.com:MatrixOrigin/Memoria.git (fetch)
    // HTTPS: https://github.com/MatrixOrigin/Memoria.git (fetch)
    let parts: Vec<&str> = remote_line.split_whitespace().collect();
    let url = parts.get(1)?;
    let path = if let Some(rest) = url.strip_prefix("git@github.com:") {
        rest
    } else if url.contains("://github.com/") {
        url.rsplit_once("github.com/")?.1
    } else {
        return None;
    };
    let path = path.strip_suffix(".git").unwrap_or(path);
    if path.contains('/') && !path.contains(' ') {
        Some(path.to_string())
    } else {
        None
    }
}

/// Count uncommitted changes and new commits in a worktree since a baseline commit.
/// Returns (changed_files, commits). Used by `exit_worktree` to warn before discarding work.
fn count_worktree_changes(worktree_path: &Path, original_head: Option<&str>) -> (usize, usize) {
    // Count uncommitted files
    let status = Command::new("git")
        .args([
            "-C",
            &worktree_path.display().to_string(),
            "status",
            "--porcelain",
        ])
        .output();
    let changed_files = status
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .count()
        })
        .unwrap_or(0);

    // Count commits since baseline
    let commits = original_head
        .and_then(|base| {
            Command::new("git")
                .args([
                    "-C",
                    &worktree_path.display().to_string(),
                    "rev-list",
                    "--count",
                    &format!("{base}..HEAD"),
                ])
                .output()
                .ok()
                .filter(|o| o.status.success())
                .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
        })
        .unwrap_or(0);

    (changed_files, commits)
}

impl ToolExecutor {
    /// Return the effective project root, considering any active worktree session.
    /// When inside a worktree session, returns the worktree path; otherwise returns
    /// the original `project_root`.
    pub fn effective_project_root(&self) -> PathBuf {
        if let Ok(guard) = self.worktree_session.lock() {
            if let Some(ref session) = *guard {
                return session.worktree_path.clone();
            }
        }
        self.project_root.clone()
    }

    /// Check if there is an active worktree session.
    pub fn in_worktree_session(&self) -> bool {
        self.worktree_session
            .lock()
            .map(|guard| guard.is_some())
            .unwrap_or(false)
    }

    /// Get a clone of the current worktree session state, if any.
    pub fn get_worktree_session(&self) -> Option<WorktreeSession> {
        self.worktree_session.lock().ok()?.clone()
    }

    /// Enter a worktree session. Creates the worktree and updates internal state.
    /// Returns the new WorktreeSession on success.
    pub fn enter_worktree(&self, branch: &str) -> Result<WorktreeSession, String> {
        // Check if already in a worktree session
        if self.in_worktree_session() {
            return Err("Already in a worktree session. Use git_worktree exit first.".to_string());
        }

        // Validate branch name
        if branch.is_empty() {
            return Err("Branch name is required".to_string());
        }
        if branch
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '{' | '}'))
        {
            return Err("Invalid branch name".to_string());
        }

        // Get current HEAD commit for later diffing
        let original_head = super::git_gix::head_short(&self.project_root);

        // Generate worktree path as sibling directory
        let repo_name = self
            .project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let sanitized_branch = branch.replace('/', "-");
        let worktree_path = self
            .project_root
            .parent()
            .unwrap_or(&self.project_root)
            .join(format!("{repo_name}-wt-{sanitized_branch}"));

        if worktree_path.exists() {
            return Err(format!(
                "Worktree path already exists: {}",
                worktree_path.display()
            ));
        }

        // Create the worktree with a new branch
        let output = Command::new("git")
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree_path)
            .current_dir(&self.project_root)
            .output()
            .map_err(|e| format!("Failed to create worktree: {e}"))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("git worktree add failed: {}", stderr.trim()));
        }

        let session = WorktreeSession {
            worktree_path: worktree_path.clone(),
            branch_name: branch.to_string(),
            original_root: self.project_root.clone(),
            original_head_commit: if original_head.is_empty() {
                None
            } else {
                Some(original_head)
            },
        };

        // Update session state
        if let Ok(mut guard) = self.worktree_session.lock() {
            *guard = Some(session.clone());
        }

        // Clear file state cache (paths are relative to project root)
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }

        Ok(session)
    }

    /// Exit the current worktree session.
    /// `action`: "keep" preserves the worktree; "remove" deletes it.
    /// `discard_changes`: required when removing a worktree with uncommitted changes.
    pub fn exit_worktree(&self, action: &str, discard_changes: bool) -> Result<String, String> {
        let session = {
            let guard = self.worktree_session.lock().map_err(|_| "Lock poisoned")?;
            guard.clone().ok_or("Not in a worktree session")?
        };

        // Count uncommitted changes and commits
        let (changed_files, commits) = count_worktree_changes(
            &session.worktree_path,
            session.original_head_commit.as_deref(),
        );

        if action == "remove" && (changed_files > 0 || commits > 0) && !discard_changes {
            let mut parts = Vec::new();
            if changed_files > 0 {
                parts.push(format!("{} uncommitted file(s)", changed_files));
            }
            if commits > 0 {
                parts.push(format!("{} commit(s) on {}", commits, session.branch_name));
            }
            return Err(format!(
                "Worktree has {}. Set discard_changes=true to confirm removal, or use action='keep' to preserve.",
                parts.join(" and ")
            ));
        }

        let worktree_path_str = session.worktree_path.display().to_string();
        let branch_name = session.branch_name.clone();
        let original_root = session.original_root.clone();

        // Clear session state first
        if let Ok(mut guard) = self.worktree_session.lock() {
            *guard = None;
        }

        // Clear file state cache
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }

        if action == "remove" {
            // Remove the worktree
            let _ = Command::new("git")
                .args(["worktree", "remove", "--force"])
                .arg(&session.worktree_path)
                .current_dir(&original_root)
                .output();

            // Also delete the branch
            let _ = Command::new("git")
                .args(["branch", "-D", &branch_name])
                .current_dir(&original_root)
                .output();

            let discard_note = if changed_files > 0 || commits > 0 {
                format!(
                    " Discarded {} file(s) and {} commit(s).",
                    changed_files, commits
                )
            } else {
                String::new()
            };

            Ok(format!(
                "✓ Exited and removed worktree at {}.{} Session restored to {}",
                worktree_path_str,
                discard_note,
                original_root.display()
            ))
        } else {
            // Keep the worktree
            Ok(format!(
                "✓ Exited worktree. Work preserved at {} on branch {}. Session restored to {}",
                worktree_path_str,
                branch_name,
                original_root.display()
            ))
        }
    }

    /// Execute git_worktree tool with enter/exit session management.
    pub(super) fn git_worktree(&self, args: &Value) -> String {
        let action = match args.get("action").and_then(Value::as_str) {
            Some(a) => a,
            None => {
                return "Error: 'action' is required (enter, exit, add, list, remove)".to_string();
            }
        };

        match action {
            "enter" => {
                let branch = match args.get("branch").and_then(Value::as_str) {
                    Some(b) if !b.is_empty() => b,
                    _ => return "Error: 'branch' is required for enter".to_string(),
                };
                match self.enter_worktree(branch) {
                    Ok(session) => format!(
                        "✓ Entered worktree\n  Branch: {}\n  Path: {}\n  Session is now working in the worktree. Use `git_worktree exit` to leave.",
                        session.branch_name,
                        session.worktree_path.display()
                    ),
                    Err(e) => format!("Error: {e}"),
                }
            }
            "exit" => {
                let exit_action = args
                    .get("exit_action")
                    .and_then(Value::as_str)
                    .unwrap_or("keep");
                let discard = args
                    .get("discard_changes")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match self.exit_worktree(exit_action, discard) {
                    Ok(msg) => msg,
                    Err(e) => format!("Error: {e}"),
                }
            }
            "add" | "create" => super::git_gix::worktree_add(&self.project_root, args),
            "list" | "ls" => super::git_gix::worktree_list(&self.project_root),
            "remove" | "rm" | "delete" => super::git_gix::worktree_remove(&self.project_root, args),
            _ => format!(
                "Error: unknown worktree action '{action}'. Use: enter, exit, add, list, remove"
            ),
        }
    }
}
