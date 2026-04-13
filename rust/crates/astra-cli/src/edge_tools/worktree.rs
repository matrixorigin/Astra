//! Git worktree session management for isolated branch work.
//!
//! Provides enter/exit workflow for git worktrees with session tracking,
//! change counting, and safe cleanup.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::SystemTime;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitWorktreeRollbackEntry {
    sequence: u64,
    pub worktree_path: PathBuf,
    pub branch_name: String,
    pub original_root: PathBuf,
    pub turn_index: u32,
    pub timestamp: SystemTime,
    pub original_head_commit: Option<String>,
    pub delete_branch_on_rollback: bool,
    pub session_scoped: bool,
}

#[derive(Debug, Default)]
pub(crate) struct GitWorktreeRollbackJournal {
    entries: Vec<GitWorktreeRollbackEntry>,
    next_sequence: u64,
}

impl GitWorktreeRollbackJournal {
    #[allow(clippy::too_many_arguments)]
    fn record(
        &mut self,
        worktree_path: PathBuf,
        branch_name: String,
        original_root: PathBuf,
        turn_index: u32,
        original_head_commit: Option<String>,
        delete_branch_on_rollback: bool,
        session_scoped: bool,
    ) {
        self.entries.push(GitWorktreeRollbackEntry {
            sequence: self.next_sequence,
            worktree_path,
            branch_name,
            original_root,
            turn_index,
            timestamp: SystemTime::now(),
            original_head_commit,
            delete_branch_on_rollback,
            session_scoped,
        });
        self.next_sequence = self.next_sequence.saturating_add(1);
    }

    fn list(&self) -> Vec<GitWorktreeRollbackEntry> {
        self.entries.iter().rev().cloned().collect()
    }

    fn restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitWorktreeRollbackEntry> {
        self.restore_plan_for_turn_since(turn_index, 0)
    }

    fn restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitWorktreeRollbackEntry> {
        self.entries
            .iter()
            .rev()
            .filter(|entry| entry.turn_index == turn_index && entry.sequence >= checkpoint)
            .cloned()
            .collect()
    }

    fn checkpoint(&self) -> u64 {
        self.next_sequence
    }

    fn remove_worktree(&mut self, worktree_path: &Path) -> bool {
        if let Some(index) = self
            .entries
            .iter()
            .rposition(|entry| entry.worktree_path == worktree_path)
        {
            self.entries.remove(index);
            true
        } else {
            false
        }
    }
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

fn delete_worktree_branch(original_root: &Path, branch_name: &str) -> Result<(), String> {
    let output = Command::new("git")
        .args(["branch", "-D", branch_name])
        .current_dir(original_root)
        .output()
        .map_err(|error| format!("failed to delete worktree branch '{branch_name}': {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "failed to delete worktree branch '{branch_name}': {}",
            stderr.trim()
        ))
    }
}

impl ToolExecutor {
    fn normalize_worktree_path(&self, worktree_path: &Path) -> PathBuf {
        if worktree_path.is_absolute() {
            worktree_path.to_path_buf()
        } else {
            self.project_root.join(worktree_path)
        }
    }

    fn record_git_worktree_rollback(
        &self,
        worktree_path: PathBuf,
        branch_name: String,
        original_root: PathBuf,
        original_head_commit: Option<String>,
        delete_branch_on_rollback: bool,
        session_scoped: bool,
    ) {
        let turn_index = self
            .journal_turn_index
            .load(std::sync::atomic::Ordering::Relaxed);
        match self.git_worktree_journal.lock() {
            Ok(mut journal) => journal.record(
                worktree_path,
                branch_name,
                original_root,
                turn_index,
                original_head_commit,
                delete_branch_on_rollback,
                session_scoped,
            ),
            Err(poisoned) => poisoned.into_inner().record(
                worktree_path,
                branch_name,
                original_root,
                turn_index,
                original_head_commit,
                delete_branch_on_rollback,
                session_scoped,
            ),
        }
    }

    fn git_worktree_entries(&self) -> Vec<GitWorktreeRollbackEntry> {
        match self.git_worktree_journal.lock() {
            Ok(journal) => journal.list(),
            Err(poisoned) => poisoned.into_inner().list(),
        }
    }

    fn git_worktree_restore_plan_for_turn(&self, turn_index: u32) -> Vec<GitWorktreeRollbackEntry> {
        match self.git_worktree_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn(turn_index),
            Err(poisoned) => poisoned.into_inner().restore_plan_for_turn(turn_index),
        }
    }

    fn git_worktree_restore_plan_for_turn_since(
        &self,
        turn_index: u32,
        checkpoint: u64,
    ) -> Vec<GitWorktreeRollbackEntry> {
        match self.git_worktree_journal.lock() {
            Ok(journal) => journal.restore_plan_for_turn_since(turn_index, checkpoint),
            Err(poisoned) => poisoned
                .into_inner()
                .restore_plan_for_turn_since(turn_index, checkpoint),
        }
    }

    pub(crate) fn git_worktree_journal_checkpoint(&self) -> u64 {
        match self.git_worktree_journal.lock() {
            Ok(journal) => journal.checkpoint(),
            Err(poisoned) => poisoned.into_inner().checkpoint(),
        }
    }

    fn remove_git_worktree_rollback(&self, worktree_path: &Path) {
        match self.git_worktree_journal.lock() {
            Ok(mut journal) => {
                journal.remove_worktree(worktree_path);
            }
            Err(poisoned) => {
                poisoned.into_inner().remove_worktree(worktree_path);
            }
        }
    }

    fn maybe_restore_session_after_manual_worktree_removal(
        &self,
        worktree_path: &Path,
    ) -> Option<String> {
        let session = self.get_worktree_session()?;
        if session.worktree_path != worktree_path {
            return None;
        }
        if let Ok(mut guard) = self.worktree_session.lock() {
            *guard = None;
        }
        self.clear_file_state();
        Some(session.original_root.display().to_string())
    }

    fn rollback_git_worktree_entry_json(entry: &GitWorktreeRollbackEntry) -> serde_json::Value {
        let mut value = serde_json::Map::from_iter([
            (
                "worktree_path".to_string(),
                Value::String(entry.worktree_path.display().to_string()),
            ),
            (
                "branch".to_string(),
                Value::String(entry.branch_name.clone()),
            ),
            (
                "original_root".to_string(),
                Value::String(entry.original_root.display().to_string()),
            ),
            (
                "turn_index".to_string(),
                Value::Number(serde_json::Number::from(entry.turn_index)),
            ),
            (
                "delete_branch_on_rollback".to_string(),
                Value::Bool(entry.delete_branch_on_rollback),
            ),
            (
                "session_scoped".to_string(),
                Value::Bool(entry.session_scoped),
            ),
        ]);
        if let Some(original_head_commit) = entry.original_head_commit.as_ref() {
            value.insert(
                "original_head_commit".to_string(),
                Value::String(original_head_commit.clone()),
            );
        }
        Value::Object(value)
    }

    fn rollback_recorded_git_worktree(
        &self,
        entry: &GitWorktreeRollbackEntry,
    ) -> Result<(bool, Option<String>), String> {
        if !entry.worktree_path.exists() {
            return Err(format!(
                "recorded worktree path no longer exists: {}",
                entry.worktree_path.display()
            ));
        }

        let (changed_files, commits) =
            count_worktree_changes(&entry.worktree_path, entry.original_head_commit.as_deref());
        if changed_files > 0 || commits > 0 {
            return Err(format!(
                "recorded worktree at {} is no longer clean ({} changed file(s), {} commit(s) since creation)",
                entry.worktree_path.display(),
                changed_files,
                commits
            ));
        }

        let output = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&entry.worktree_path)
            .current_dir(&entry.original_root)
            .output()
            .map_err(|error| {
                format!(
                    "git worktree remove failed for {}: {error}",
                    entry.worktree_path.display()
                )
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "git worktree remove failed for {}: {}",
                entry.worktree_path.display(),
                stderr.trim()
            ));
        }

        let session_restored = self
            .maybe_restore_session_after_manual_worktree_removal(&entry.worktree_path)
            .is_some();
        let branch_warning = if entry.delete_branch_on_rollback {
            delete_worktree_branch(&entry.original_root, &entry.branch_name).err()
        } else {
            None
        };

        Ok((session_restored, branch_warning))
    }

    pub(crate) fn rollback_git_worktrees(&self, args: &Value) -> String {
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let explicit_turn_index = if scope == "turn" {
            match args.get("turn_index").and_then(Value::as_u64) {
                Some(turn_index) => Some(turn_index),
                None => {
                    return serde_json::json!({
                        "success": false,
                        "error": "missing 'turn_index' for scope=turn",
                    })
                    .to_string();
                }
            }
        } else {
            None
        };
        let after_sequence = args
            .get("worktree_after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match scope {
            "list" => {
                let entries = self
                    .git_worktree_entries()
                    .into_iter()
                    .map(|entry| Self::rollback_git_worktree_entry_json(&entry))
                    .collect::<Vec<_>>();
                serde_json::json!({
                    "success": true,
                    "scope": "list",
                    "total_entries": entries.len(),
                    "entries": entries,
                    "summary": format!(
                        "Listed {} recorded git worktree rollback entr{}",
                        entries.len(),
                        if entries.len() == 1 { "y" } else { "ies" }
                    ),
                })
                .to_string()
            }
            "turn" | "current_turn" => {
                let turn_index = explicit_turn_index.unwrap_or_else(|| {
                    self.journal_turn_index
                        .load(std::sync::atomic::Ordering::Relaxed) as u64
                }) as u32;
                let plan = if after_sequence > 0 {
                    self.git_worktree_restore_plan_for_turn_since(turn_index, after_sequence)
                } else {
                    self.git_worktree_restore_plan_for_turn(turn_index)
                };
                let mut restored = Vec::new();
                let mut failed = Vec::new();
                for entry in &plan {
                    match self.rollback_recorded_git_worktree(entry) {
                        Ok((session_restored, branch_warning)) => {
                            self.remove_git_worktree_rollback(&entry.worktree_path);
                            let mut restored_entry = Self::rollback_git_worktree_entry_json(entry)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            if session_restored {
                                restored_entry
                                    .insert("session_restored".to_string(), Value::Bool(true));
                            }
                            if let Some(warning) = branch_warning {
                                restored_entry
                                    .insert("warning".to_string(), Value::String(warning));
                            }
                            restored.push(Value::Object(restored_entry));
                        }
                        Err(error) => {
                            let mut failed_entry = Self::rollback_git_worktree_entry_json(entry)
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            failed_entry.insert("error".to_string(), Value::String(error));
                            failed.push(Value::Object(failed_entry));
                        }
                    }
                }
                let success = !restored.is_empty() && failed.is_empty();
                let summary = if plan.is_empty() {
                    format!("No recorded git worktree rollback handles found for turn {turn_index}")
                } else if failed.is_empty() {
                    format!(
                        "Removed {} recorded git worktree{} for turn {turn_index}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" }
                    )
                } else {
                    format!(
                        "Removed {} recorded git worktree{} for turn {turn_index} with {} failure{}",
                        restored.len(),
                        if restored.len() == 1 { "" } else { "s" },
                        failed.len(),
                        if failed.len() == 1 { "" } else { "s" }
                    )
                };
                serde_json::json!({
                    "success": success,
                    "scope": scope,
                    "turn_index": turn_index,
                    "restored": restored,
                    "failed": failed,
                    "summary": summary,
                })
                .to_string()
            }
            other => serde_json::json!({
                "success": false,
                "error": format!(
                    "unknown scope `{other}`. Supported: current_turn, turn, list"
                ),
            })
            .to_string(),
        }
    }

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

    pub(crate) fn git_worktree_with_metadata(&self, args: &Value) -> super::ToolExecutionOutcome {
        let action = match args.get("action").and_then(Value::as_str) {
            Some(a) => a,
            None => {
                return super::ToolExecutionOutcome {
                    output: "Error: 'action' is required (enter, exit, add, list, remove)"
                        .to_string(),
                    tool_result_fields: None,
                };
            }
        };

        match action {
            "enter" => {
                let branch = match args.get("branch").and_then(Value::as_str) {
                    Some(b) if !b.is_empty() => b,
                    _ => {
                        return super::ToolExecutionOutcome {
                            output: "Error: 'branch' is required for enter".to_string(),
                            tool_result_fields: None,
                        };
                    }
                };
                match self.enter_worktree(branch) {
                    Ok(session) => {
                        self.record_git_worktree_rollback(
                            session.worktree_path.clone(),
                            session.branch_name.clone(),
                            session.original_root.clone(),
                            session.original_head_commit.clone(),
                            true,
                            true,
                        );
                        let mut tool_result_fields = serde_json::Map::from_iter([
                            (
                                "worktree_path".to_string(),
                                Value::String(session.worktree_path.display().to_string()),
                            ),
                            (
                                "branch".to_string(),
                                Value::String(session.branch_name.clone()),
                            ),
                            ("delete_branch_on_rollback".to_string(), Value::Bool(true)),
                            ("session_scoped".to_string(), Value::Bool(true)),
                        ]);
                        if let Some(original_head_commit) = session.original_head_commit.as_ref() {
                            tool_result_fields.insert(
                                "original_head_commit".to_string(),
                                Value::String(original_head_commit.clone()),
                            );
                        }
                        super::ToolExecutionOutcome {
                            output: format!(
                                "✓ Entered worktree\n  Branch: {}\n  Path: {}\n  Session is now working in the worktree. Use `git_worktree exit` to leave.",
                                session.branch_name,
                                session.worktree_path.display()
                            ),
                            tool_result_fields: Some(tool_result_fields),
                        }
                    }
                    Err(e) => super::ToolExecutionOutcome {
                        output: format!("Error: {e}"),
                        tool_result_fields: None,
                    },
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
                let existing_session = (exit_action == "remove")
                    .then(|| self.get_worktree_session())
                    .flatten();
                match self.exit_worktree(exit_action, discard) {
                    Ok(msg) => {
                        if exit_action == "remove"
                            && let Some(session) = existing_session
                        {
                            self.remove_git_worktree_rollback(&session.worktree_path);
                        }
                        super::ToolExecutionOutcome {
                            output: msg,
                            tool_result_fields: None,
                        }
                    }
                    Err(e) => super::ToolExecutionOutcome {
                        output: format!("Error: {e}"),
                        tool_result_fields: None,
                    },
                }
            }
            "add" | "create" => {
                let outcome = super::git_gix::worktree_add_with_metadata(&self.project_root, args);
                if let Some(fields) = outcome.tool_result_fields.as_ref()
                    && let Some(worktree_path) = fields.get("worktree_path").and_then(Value::as_str)
                {
                    self.record_git_worktree_rollback(
                        PathBuf::from(worktree_path),
                        fields
                            .get("branch")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                        self.project_root.clone(),
                        fields
                            .get("original_head_commit")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                        fields
                            .get("delete_branch_on_rollback")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        false,
                    );
                }
                outcome
            }
            "list" | "ls" => super::ToolExecutionOutcome {
                output: super::git_gix::worktree_list(&self.project_root),
                tool_result_fields: None,
            },
            "remove" | "rm" | "delete" => {
                let normalized_path = args
                    .get("path")
                    .and_then(Value::as_str)
                    .map(|path| self.normalize_worktree_path(Path::new(path)));
                let mut output = super::git_gix::worktree_remove(&self.project_root, args);
                if !output.starts_with("Error:")
                    && let Some(worktree_path) = normalized_path.as_ref()
                {
                    self.remove_git_worktree_rollback(worktree_path);
                    if let Some(original_root) =
                        self.maybe_restore_session_after_manual_worktree_removal(worktree_path)
                    {
                        output.push_str(&format!("\n  Session restored to {original_root}"));
                    }
                }
                super::ToolExecutionOutcome {
                    output,
                    tool_result_fields: None,
                }
            }
            _ => super::ToolExecutionOutcome {
                output: format!(
                    "Error: unknown worktree action '{action}'. Use: enter, exit, add, list, remove"
                ),
                tool_result_fields: None,
            },
        }
    }

    /// Execute git_worktree tool with enter/exit session management.
    pub(super) fn git_worktree(&self, args: &Value) -> String {
        self.git_worktree_with_metadata(args).output
    }
}
