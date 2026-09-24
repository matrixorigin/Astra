//! Git worktree session management for isolated branch work.
//!
//! Provides enter/exit workflow for git worktrees with session tracking,
//! change counting, and safe cleanup.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::ToolExecutor;

#[derive(Debug)]
enum WorktreeError {
    Domain(String),
    Process(astra_tools::git_gix::GitProcessError),
}
impl From<String> for WorktreeError {
    fn from(value: String) -> Self {
        Self::Domain(value)
    }
}
impl From<&str> for WorktreeError {
    fn from(value: &str) -> Self {
        Self::Domain(value.into())
    }
}
impl From<astra_tools::git_gix::GitProcessError> for WorktreeError {
    fn from(value: astra_tools::git_gix::GitProcessError) -> Self {
        Self::Process(value)
    }
}
impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Domain(message) => write!(f, "{message}"),
            Self::Process(error) => write!(f, "{error}"),
        }
    }
}
impl WorktreeError {
    fn into_outcome(self) -> super::ToolExecutionOutcome {
        match self {
            Self::Domain(message) => {
                super::ToolExecutionOutcome::error(format!("Error: {message}"))
            }
            Self::Process(error) => error.into_outcome("worktree"),
        }
    }
}

/// State for an active worktree session created by `worktree(action=enter)`.
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
    /// Tree identity of the pinned creation commit.
    pub source_tree: String,
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

/// Count uncommitted changes and new commits in a worktree since a baseline commit.
/// Returns (changed_files, commits). Used by `exit_worktree` to warn before discarding work.
fn count_worktree_changes(
    worktree_path: &Path,
    original_head: Option<&str>,
) -> Result<(usize, usize), WorktreeError> {
    // Count uncommitted files
    let status = astra_tools::git_gix::prepare_bound_git_command(worktree_path)
        .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?
        .args(["status", "--porcelain"])
        .output()
        .map_err(astra_tools::git_gix::GitProcessError::Execution)?;
    if !status.status.success() {
        return Err(astra_tools::git_gix::GitProcessError::Exit(
            "failed to inspect the exact bound worktree status".to_string(),
        )
        .into());
    }
    let changed_files = String::from_utf8_lossy(&status.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    // Count commits since baseline
    let commits = if let Some(base) = original_head {
        let output = astra_tools::git_gix::prepare_bound_git_command(worktree_path)
            .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?
            .args(["rev-list", "--count", &format!("{base}..HEAD")])
            .output()
            .map_err(astra_tools::git_gix::GitProcessError::Execution)?;
        if !output.status.success() {
            return Err(astra_tools::git_gix::GitProcessError::Exit(
                "failed to inspect commits in the exact bound worktree".to_string(),
            )
            .into());
        }
        String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| "git returned an invalid structural commit count".to_string())?
    } else {
        0
    };

    Ok((changed_files, commits))
}

fn delete_worktree_branch(original_root: &Path, branch_name: &str) -> Result<(), String> {
    let output = astra_tools::git_gix::prepare_bound_git_command(original_root)?
        .args(["branch", "-D", branch_name])
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
            count_worktree_changes(&entry.worktree_path, entry.original_head_commit.as_deref())
                .map_err(|error| error.to_string())?;
        if changed_files > 0 || commits > 0 {
            return Err(format!(
                "recorded worktree at {} is no longer clean ({} changed file(s), {} commit(s) since creation)",
                entry.worktree_path.display(),
                changed_files,
                commits
            ));
        }

        let output = astra_tools::git_gix::prepare_bound_git_command(&entry.original_root)?
            .args(["worktree", "remove", "--force"])
            .arg(&entry.worktree_path)
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
        self.enter_worktree_result(branch, None)
            .map_err(|error| error.to_string())
    }
    fn enter_worktree_result(
        &self,
        branch: &str,
        source_commit: Option<&str>,
    ) -> Result<WorktreeSession, WorktreeError> {
        // Check if already in a worktree session
        if self.in_worktree_session() {
            return Err(
                ("Already in a worktree session. Use worktree action=exit first.".to_string())
                    .into(),
            );
        }

        // Validate branch name
        if branch.is_empty() {
            return Err(("Branch name is required".to_string()).into());
        }
        if branch
            .chars()
            .any(|c| matches!(c, ';' | '|' | '&' | '`' | '$' | '(' | ')' | '{' | '}'))
        {
            return Err(("Invalid branch name".to_string()).into());
        }

        // Resolve once and pass the immutable identity to creation, even when
        // the caller selected HEAD. A moving branch cannot change the baseline.
        let revision = format!("{}^{{commit}}", source_commit.unwrap_or("HEAD"));
        let resolved = astra_tools::git_gix::prepare_bound_git_command(&self.project_root)
            .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?
            .args(["rev-parse", "--verify", "--end-of-options", &revision])
            .output()
            .map_err(astra_tools::git_gix::GitProcessError::Execution)?;
        if !resolved.status.success() {
            return Err("source_commit must resolve to an existing commit".into());
        }
        let original_head = String::from_utf8_lossy(&resolved.stdout).trim().to_string();
        if !matches!(original_head.len(), 40 | 64)
            || !original_head.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Git returned an invalid source commit identity".into());
        }

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
            return Err(
                (format!("Worktree path already exists: {}", worktree_path.display())).into(),
            );
        }

        // Create the worktree with a new branch
        let output = astra_tools::git_gix::prepare_bound_git_command(&self.project_root)
            .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?
            .args(["worktree", "add", "-b", branch])
            .arg(&worktree_path)
            .arg(&original_head)
            .output()
            .map_err(astra_tools::git_gix::GitProcessError::Execution)?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(astra_tools::git_gix::GitProcessError::Exit(format!(
                "git worktree add failed: {}",
                stderr.trim()
            ))
            .into());
        }

        let verification_error = |error: String| {
            WorktreeError::Domain(format!(
                "Worktree source verification failed: {error}. Created workspace preserved at {} on branch {branch}. Session was not switched.",
                worktree_path.display()
            ))
        };
        let identity = astra_tools::git_gix::prepare_bound_git_command(&worktree_path)
            .map_err(|error| verification_error(error.to_string()))?
            .args(["rev-parse", "HEAD", "HEAD^{tree}"])
            .output()
            .map_err(|error| verification_error(error.to_string()))?;
        let identity_text = String::from_utf8_lossy(&identity.stdout);
        let mut identities = identity_text.lines();
        let head = identities.next().unwrap_or_default();
        let source_tree = identities.next().unwrap_or_default();
        if !identity.status.success()
            || head != original_head
            || source_tree.len() != original_head.len()
            || !source_tree.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(verification_error(
                "created HEAD/tree did not match the pinned source".to_string(),
            ));
        }

        let session = WorktreeSession {
            worktree_path: worktree_path.clone(),
            branch_name: branch.to_string(),
            source_tree: source_tree.to_string(),
            original_root: self.project_root.clone(),
            original_head_commit: Some(original_head),
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
        self.exit_worktree_result(action, discard_changes)
            .map_err(|error| error.to_string())
    }
    fn exit_worktree_result(
        &self,
        action: &str,
        discard_changes: bool,
    ) -> Result<String, WorktreeError> {
        let session = {
            let guard = self.worktree_session.lock().map_err(|_| "Lock poisoned")?;
            guard.clone().ok_or("Not in a worktree session")?
        };

        // Count uncommitted changes and commits
        let (changed_files, commits) = count_worktree_changes(
            &session.worktree_path,
            session.original_head_commit.as_deref(),
        )?;

        if action == "remove" && (changed_files > 0 || commits > 0) && !discard_changes {
            let mut parts = Vec::new();
            if changed_files > 0 {
                parts.push(format!("{} uncommitted file(s)", changed_files));
            }
            if commits > 0 {
                parts.push(format!("{} commit(s) on {}", commits, session.branch_name));
            }
            return Err((format!(
                "Worktree has {}. Set discard_changes=true to confirm removal, or use action='keep' to preserve.",
                parts.join(" and ")
            )).into());
        }

        let worktree_path_str = session.worktree_path.display().to_string();
        let branch_name = session.branch_name.clone();
        let original_root = session.original_root.clone();
        let cleanup_commands = if action == "remove" {
            Some((
                astra_tools::git_gix::prepare_bound_git_command(&original_root)
                    .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?,
                astra_tools::git_gix::prepare_bound_git_command(&original_root)
                    .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?,
            ))
        } else {
            None
        };

        let branch_warning =
            if let Some((mut remove_command, mut delete_branch_command)) = cleanup_commands {
                let removed = remove_command
                    .args(["worktree", "remove", "--force"])
                    .arg(&session.worktree_path)
                    .output()
                    .map_err(astra_tools::git_gix::GitProcessError::Execution)?;
                if !removed.status.success() {
                    return Err(astra_tools::git_gix::GitProcessError::Exit(format!(
                        "Worktree removal failed; session remains active: {}",
                        String::from_utf8_lossy(&removed.stderr).trim()
                    ))
                    .into());
                }
                match delete_branch_command
                    .args(["branch", "-D", &branch_name])
                    .output()
                {
                    Ok(out) if out.status.success() => String::new(),
                    Ok(out) => format!(
                        " Branch cleanup failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    Err(error) => format!(" Branch cleanup failed: {error}"),
                }
            } else {
                String::new()
            };
        // Only forget the session once removal succeeded (or keep was selected).
        if let Ok(mut guard) = self.worktree_session.lock() {
            *guard = None;
        }
        if let Ok(mut state) = self.file_state.lock() {
            state.clear();
        }
        if action == "remove" {
            let discard_note = if changed_files > 0 || commits > 0 {
                format!(
                    " Discarded {} file(s) and {} commit(s).",
                    changed_files, commits
                )
            } else {
                String::new()
            };

            Ok(format!(
                "✓ Exited and removed worktree at {}.{} Session restored to {}{}",
                worktree_path_str,
                discard_note,
                original_root.display(),
                branch_warning
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

    pub(crate) fn worktree_with_metadata(&self, args: &Value) -> super::ToolExecutionOutcome {
        match args.get("action").and_then(Value::as_str) {
            Some("enter") => {
                let branch = match args.get("branch").and_then(Value::as_str) {
                    Some(b) if !b.is_empty() => b,
                    _ => {
                        return super::ToolExecutionOutcome::error(
                            "Error: 'branch' is required for enter".to_string(),
                        );
                    }
                };
                let source_commit = match args.get("source_commit") {
                    None => None,
                    Some(Value::String(value)) if !value.trim().is_empty() => Some(value.as_str()),
                    Some(_) => {
                        return super::ToolExecutionOutcome::error(
                            "Error: source_commit must be a non-empty commit reference".to_string(),
                        );
                    }
                };
                match self.enter_worktree_result(branch, source_commit) {
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
                            (
                                "source_tree".to_string(),
                                Value::String(session.source_tree.clone()),
                            ),
                            ("delete_branch_on_rollback".to_string(), Value::Bool(true)),
                            ("session_scoped".to_string(), Value::Bool(true)),
                        ]);
                        if let Some(original_head_commit) = session.original_head_commit.as_ref() {
                            tool_result_fields.insert(
                                "source_commit".to_string(),
                                Value::String(original_head_commit.clone()),
                            );
                            tool_result_fields.insert(
                                "original_head_commit".to_string(),
                                Value::String(original_head_commit.clone()),
                            );
                        }
                        super::ToolExecutionOutcome {
                            output: format!(
                                "✓ Entered worktree\n  Branch: {}\n  Path: {}\n  Session is now working in the worktree. Use `worktree` action=`exit` to leave.",
                                session.branch_name,
                                session.worktree_path.display()
                            ),
                            tool_result_fields: Some(tool_result_fields),
                            is_error: false,
                        }
                    }
                    Err(e) => e.into_outcome(),
                }
            }
            Some("exit") => {
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
                match self.exit_worktree_result(exit_action, discard) {
                    Ok(msg) => {
                        if exit_action == "remove"
                            && let Some(session) = existing_session
                        {
                            self.remove_git_worktree_rollback(&session.worktree_path);
                        }
                        super::ToolExecutionOutcome {
                            output: msg,
                            tool_result_fields: None,
                            is_error: false,
                        }
                    }
                    Err(e) => e.into_outcome(),
                }
            }
            _ => super::ToolExecutionOutcome::error(
                "Error: worktree requires action=enter or action=exit".to_string(),
            ),
        }
    }

    /// Execute the session worktree lifecycle.
    pub(super) fn worktree(&self, args: &Value) -> String {
        self.worktree_with_metadata(args).output
    }
}
