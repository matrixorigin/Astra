//! Git worktree session management for isolated branch work.
//!
//! Provides enter/exit workflow for git worktrees with session tracking,
//! change counting, and safe cleanup.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::ToolExecutor;

pub(super) fn workspace_safe_for_cleanup(path: &Path) -> bool {
    astra_tools::workspace_observation::workspace_ownership_is_unsettled(path) == Some(false)
        && astra_tools::workspace_observation::workspace_observation_is_quarantined(path)
            == Some(false)
}

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
    /// Identity of the checkout's own Git metadata directory.
    pub(crate) git_identity: String,
    /// Set when a saved checkout cannot safely be rebound on resume.
    pub(crate) recovery_error: Option<String>,
}

fn validate_saved_worktree(
    project_root: &Path,
    original_root: &Path,
    selected_path: &Path,
    expected_identity: Option<&str>,
) -> Result<(), String> {
    let original = original_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let selected = selected_path
        .canonicalize()
        .map_err(|error| error.to_string())?;
    let current = project_root
        .canonicalize()
        .map_err(|error| error.to_string())?;
    if !current.starts_with(&original) && !current.starts_with(&selected) {
        return Err("the current directory does not belong to the saved session workspace".into());
    }
    let common_dir = |path: &Path| -> Result<PathBuf, String> {
        let repo = gix::open(path).map_err(|error| error.to_string())?;
        if repo
            .workdir()
            .and_then(|dir| dir.canonicalize().ok())
            .as_deref()
            != Some(path)
        {
            return Err("the saved checkout is no longer a Git worktree".into());
        }
        repo.common_dir()
            .canonicalize()
            .map_err(|error| error.to_string())
    };
    if common_dir(&original)? != common_dir(&selected)? {
        return Err("the saved checkout no longer belongs to the original repository".into());
    }
    if let Some(expected) = expected_identity
        && (expected.is_empty() || worktree_identity(&selected)? != expected)
    {
        return Err("the saved checkout has been replaced".into());
    }
    Ok(())
}

fn worktree_identity(path: &Path) -> Result<String, String> {
    let repo = gix::open(path).map_err(|error| error.to_string())?;
    let metadata = std::fs::metadata(repo.git_dir()).map_err(|error| error.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        Ok(format!(
            "{}:{:?}",
            repo.git_dir().display(),
            metadata.created().ok()
        ))
    }
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
    pub(super) active_session: std::sync::Arc<std::sync::Mutex<Option<WorktreeSession>>>,
    pub(super) transition: std::sync::Arc<tokio::sync::RwLock<()>>,
    restored_session_id: Option<String>,
    session_root: Option<PathBuf>,
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
    branch: &str,
) -> Result<(usize, usize), WorktreeError> {
    // Count uncommitted files
    let status = astra_tools::git_gix::inspect_bound_git_worktree(
        worktree_path,
        &["status", "--porcelain", "--ignored=matching"],
    )?;
    if !status.status.success() || !status.stderr.is_empty() {
        return Err(astra_tools::git_gix::GitProcessError::Exit(
            "failed to inspect the exact bound worktree status".to_string(),
        )
        .into());
    }
    let changed_files = String::from_utf8_lossy(&status.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .count();

    // Treat any history change as user work, including a reset to an ancestor.
    let commits = if let Some(base) = original_head {
        let output = astra_tools::git_gix::inspect_bound_git_worktree(
            worktree_path,
            &["rev-list", "--count", &format!("{base}...HEAD")],
        )?;
        if !output.status.success() || !output.stderr.is_empty() {
            return Err(astra_tools::git_gix::GitProcessError::Exit(
                "failed to inspect commits in the exact bound worktree".to_string(),
            )
            .into());
        }
        let head_changes: usize = String::from_utf8_lossy(&output.stdout)
            .trim()
            .parse()
            .map_err(|_| "git returned an invalid structural commit count".to_string())?;
        // HEAD can be detached or moved independently of the branch that
        // cleanup would delete. Inspect that exact ref too.
        let branch_ref = format!("refs/heads/{branch}^{{commit}}");
        let branch_head = astra_tools::git_gix::inspect_bound_git_worktree(
            worktree_path,
            &["rev-parse", "--verify", "--end-of-options", &branch_ref],
        )?;
        if !branch_head.status.success()
            || !branch_head.stderr.is_empty()
            || String::from_utf8_lossy(&branch_head.stdout).trim() != base
        {
            head_changes.max(1)
        } else {
            head_changes
        }
    } else {
        // An unknown creation baseline is not evidence that removal is safe.
        1
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
    pub(super) fn workspace_selection_is_unknown(&self) -> bool {
        self.worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|session| {
                session.branch_name.is_empty() && session.recovery_error.is_some()
            })
    }

    pub(super) fn accept_current_workspace(&self) -> Result<String, String> {
        let session = self
            .get_worktree_session()
            .ok_or("Workspace selection has changed")?;
        if !session.branch_name.is_empty() || session.recovery_error.is_none() {
            return Err("Workspace selection has changed".into());
        }
        if let Some(session_id) = self.active_session_id()
            && astra_services::session_workspace::read_workspace(&session_id)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::InvalidData)
        {
            astra_services::session_workspace::backup_invalid_workspace_file(&session_id).map_err(
                |error| format!("Could not preserve the damaged workspace metadata: {error}"),
            )?;
        }
        self.persist_selected_worktree(None, &session.original_root)?;
        *self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.clear_file_state();
        Ok(format!(
            "Using workspace {}.",
            session.original_root.display()
        ))
    }

    fn persist_selected_worktree(
        &self,
        session: Option<&WorktreeSession>,
        root: &Path,
    ) -> Result<(), String> {
        let Some(session_id) = self.active_session_id().filter(|id| !id.is_empty()) else {
            return Ok(());
        };
        let selected =
            session.map(
                |session| astra_services::session_workspace::SelectedWorktree::Selected {
                    path: session.worktree_path.clone(),
                    branch: session.branch_name.clone(),
                    original_root: session.original_root.clone(),
                    original_head_commit: session.original_head_commit.clone(),
                    git_identity: session.git_identity.clone(),
                },
            );
        astra_services::session_workspace::update_workspace(
            &session_id,
            || {
                astra_services::session_workspace::WorkspaceMetadata::with_context(
                    &session_id,
                    "default",
                    &root.to_string_lossy(),
                    None,
                )
            },
            |metadata| {
                metadata.cwd = root.to_string_lossy().into_owned();
                metadata.selected_worktree = selected;
            },
        )
        .map_err(|error| format!("could not save the selected worktree: {error}"))
    }

    pub(super) fn restore_selected_worktree(&self) {
        let Some(session_id) = self.active_session_id().filter(|id| !id.is_empty()) else {
            return;
        };
        let mut owner = self
            .git_worktree_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if owner.restored_session_id.as_deref() == Some(session_id.as_str()) {
            return;
        }
        let mut current = self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if current.is_some() {
            owner.restored_session_id = Some(session_id);
            return;
        }
        match astra_services::session_workspace::read_workspace_optional(&session_id) {
            Ok(Some(metadata)) => {
                if let Some(astra_services::session_workspace::SelectedWorktree::Selected {
                    path,
                    branch,
                    original_root,
                    original_head_commit,
                    git_identity,
                }) = metadata.selected_worktree
                {
                    owner.session_root = Some(original_root.clone());
                    let error = validate_saved_worktree(
                        &self.project_root,
                        &original_root,
                        &path,
                        Some(&git_identity),
                    )
                    .err();
                    *current = Some(WorktreeSession {
                        worktree_path: path,
                        branch_name: branch,
                        original_root,
                        original_head_commit,
                        git_identity,
                        recovery_error: error,
                    });
                } else if matches!(
                    metadata.selected_worktree,
                    Some(astra_services::session_workspace::SelectedWorktree::Unknown)
                ) {
                    *current = Some(WorktreeSession {
                        worktree_path: self.project_root.clone(),
                        branch_name: String::new(),
                        original_root: self.project_root.clone(),
                        original_head_commit: None,
                        git_identity: String::new(),
                        recovery_error: Some("the saved workspace selection was lost; confirm the current directory before using local tools".into()),
                    });
                } else {
                    let original = PathBuf::from(metadata.cwd);
                    // The saved session root is usable only when the current
                    // directory is that root or another checkout of its repo.
                    let same_root = original.is_absolute()
                        && original
                            .canonicalize()
                            .ok()
                            .zip(self.project_root.canonicalize().ok())
                            .is_some_and(|(original, current)| original == current);
                    if same_root
                        || validate_saved_worktree(
                            &self.project_root,
                            &original,
                            &self.project_root,
                            None,
                        )
                        .is_ok()
                    {
                        owner.session_root = Some(original);
                    }
                }
            }
            Ok(None) => {}
            Err(error) => {
                *current = Some(WorktreeSession {
                    worktree_path: self.project_root.clone(),
                    branch_name: String::new(),
                    original_root: self.project_root.clone(),
                    original_head_commit: None,
                    git_identity: String::new(),
                    recovery_error: Some(format!(
                        "session workspace metadata is unavailable: {error}"
                    )),
                });
            }
        }
        owner.restored_session_id = Some(session_id);
    }

    pub(super) fn worktree_recovery_error(&self, name: &str, args: &Value) -> Option<String> {
        // An unavailable checkout should not disable independent capabilities.
        let registry = astra_turn_core::tool::categories::registry();
        if registry.get(name).is_some_and(|tool| {
            tool.category == astra_turn_core::tool::categories::ToolCategory::Consultative
                || tool
                    .flags
                    .contains(astra_turn_core::tool::categories::ToolFlags::WEB)
                || tool
                    .flags
                    .contains(astra_turn_core::tool::categories::ToolFlags::MEMORY)
                || tool
                    .flags
                    .contains(astra_turn_core::tool::categories::ToolFlags::MATRIXONE)
        }) {
            return None;
        }
        if name == "worktree"
            && args.get("action").and_then(Value::as_str) == Some("exit")
            && args
                .get("exit_action")
                .and_then(Value::as_str)
                .unwrap_or("keep")
                == "keep"
        {
            return None;
        }
        let mut current = self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let session = current.as_mut()?;
        // The checkout may have been restored, removed, or replaced since the
        // previous call. Validate at the point where local tools will use it.
        if !session.branch_name.is_empty() {
            session.recovery_error = validate_saved_worktree(
                &self.project_root,
                &session.original_root,
                &session.worktree_path,
                Some(&session.git_identity),
            )
            .err();
        }
        let error = session.recovery_error.as_ref()?;
        Some(format!(
            "The session's selected worktree is unavailable: {error}. Restore that worktree or leave it with worktree action=exit and exit_action=keep."
        ))
    }

    fn normalize_worktree_path(&self, worktree_path: &Path) -> PathBuf {
        if worktree_path.is_absolute() {
            worktree_path.to_path_buf()
        } else {
            self.session_project_root().join(worktree_path)
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

    pub(super) fn git_worktree_restore_plan_for_turn_since(
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

    /// Callers hold the parent repository lease before acquiring checkout
    /// leases, so concurrent executors cannot delete an active checkout.
    pub(super) async fn acquire_git_worktree_rollback_leases(
        &self,
        args: &Value,
        already_held: Option<&Path>,
        parent: &astra_tools::workspace_observation::WorkspaceObservationLease,
        cancel: Option<&tokio_util::sync::CancellationToken>,
    ) -> Result<
        Vec<astra_tools::workspace_observation::WorkspaceObservationLease>,
        astra_tools::workspace_observation::WorkspaceLeaseFailure,
    > {
        // On macOS rollback never removes a checkout, so acquiring its lease
        // would only delay the refusal when another process is still using it.
        if cfg!(target_os = "macos") {
            return Ok(Vec::new());
        }
        let scope = args
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("current_turn");
        let turn = match scope {
            "turn" => args
                .get("turn_index")
                .and_then(Value::as_u64)
                .map(|v| v as u32),
            "current_turn" => Some(
                self.journal_turn_index
                    .load(std::sync::atomic::Ordering::Relaxed),
            ),
            _ => None,
        };
        let Some(turn) = turn else {
            return Ok(Vec::new());
        };
        let checkpoint = args
            .get("worktree_after_sequence")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut roots: Vec<_> = self
            .git_worktree_restore_plan_for_turn_since(turn, checkpoint)
            .into_iter()
            .map(|entry| entry.worktree_path)
            .filter(|path| {
                path != &self.session_project_root() && already_held != Some(path.as_path())
            })
            .collect();
        roots.sort();
        roots.dedup();
        let mut leases: Vec<astra_tools::workspace_observation::WorkspaceObservationLease> =
            Vec::with_capacity(roots.len());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
        for root in roots {
            let scope = leases
                .iter()
                .find(|lease| lease.covers_workspace(&root))
                .unwrap_or(parent);
            let lease =
                astra_tools::workspace_observation::acquire_workspace_mutation_lease_with_parent(
                    &root,
                    scope,
                    cancel,
                    deadline.saturating_duration_since(tokio::time::Instant::now()),
                )
                .await?;
            leases.push(lease);
        }
        Ok(leases)
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
    ) -> Result<Option<String>, String> {
        let Some(session) = self.get_worktree_session() else {
            return Ok(None);
        };
        if session.worktree_path != worktree_path {
            return Ok(None);
        }
        self.persist_selected_worktree(None, &session.original_root)?;
        *self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
        self.clear_file_state();
        Ok(Some(session.original_root.display().to_string()))
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
        if cfg!(target_os = "macos") {
            return Err(
                "The worktree was kept. Check for running processes before removing it with Git."
                    .into(),
            );
        }
        if !workspace_safe_for_cleanup(&entry.worktree_path) {
            return Err("A previous command may still be running; the worktree was kept.".into());
        }
        if !entry.worktree_path.exists() {
            return Err(format!(
                "recorded worktree path no longer exists: {}",
                entry.worktree_path.display()
            ));
        }

        let (changed_files, commits) = count_worktree_changes(
            &entry.worktree_path,
            entry.original_head_commit.as_deref(),
            &entry.branch_name,
        )
        .map_err(|error| error.to_string())?;
        // Inspection itself may run configured Git helpers. Recheck before
        // deleting anything if their descendants could not be accounted for.
        if !workspace_safe_for_cleanup(&entry.worktree_path) {
            return Err("A command may still be running; the worktree was kept.".into());
        }
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
            .maybe_restore_session_after_manual_worktree_removal(&entry.worktree_path)?
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
        let selected = self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(|session| session.worktree_path.clone());
        selected.unwrap_or_else(|| self.session_project_root())
    }

    pub(super) fn session_project_root(&self) -> PathBuf {
        self.git_worktree_journal
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .session_root
            .clone()
            .unwrap_or_else(|| self.project_root.clone())
    }

    /// Check if there is an active worktree session.
    pub fn in_worktree_session(&self) -> bool {
        self.get_worktree_session().is_some()
    }

    /// Get a clone of the current worktree session state, if any.
    pub fn get_worktree_session(&self) -> Option<WorktreeSession> {
        self.worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
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
        start_point: Option<&str>,
    ) -> Result<WorktreeSession, WorktreeError> {
        // Check if already in a worktree session
        if self.in_worktree_session() {
            return Err(
                ("Already in a worktree session. Use worktree action=exit first.".to_string())
                    .into(),
            );
        }
        let project_root = self.session_project_root();
        if astra_tools::workspace_observation::workspace_ownership_is_unsettled(&project_root)
            != Some(false)
        {
            return Err(
                "Workspace process ownership is unsettled; no worktree was created.".into(),
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

        // Resolve the requested ref before creating anything, and use the
        // exact checkout commit as the cleanup baseline.
        let revision = format!("{}^{{commit}}", start_point.unwrap_or("HEAD"));
        let resolved = astra_tools::git_gix::prepare_bound_git_command(&project_root)
            .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?
            .args(["rev-parse", "--verify", "--end-of-options", &revision])
            .output()
            .map_err(astra_tools::git_gix::GitProcessError::Execution)?;
        if !resolved.status.success() {
            return Err("Worktree start point is not an existing commit".into());
        }
        let original_head = String::from_utf8_lossy(&resolved.stdout).trim().to_string();

        // Generate worktree path as sibling directory
        let repo_name = project_root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("repo");
        let sanitized_branch = branch.replace('/', "-");
        let worktree_path = project_root
            .parent()
            .unwrap_or(&project_root)
            .join(format!("{repo_name}-wt-{sanitized_branch}"));

        let mut command = astra_tools::git_gix::prepare_bound_git_command(&project_root)
            .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?;
        command.args(["worktree", "add", "-b", branch]);
        command.arg(&worktree_path).arg(&original_head);
        let output = match command.output_with_ownership() {
            Ok(output) => output,
            Err(error) => {
                if error.started && error.ownership.is_none() {
                    astra_tools::workspace_observation::mark_workspace_observation_unsettled(
                        &worktree_path,
                    );
                }
                return Err(astra_tools::git_gix::GitProcessError::Execution(error).into());
            }
        };

        if !output.output.status.success() {
            let stderr = String::from_utf8_lossy(&output.output.stderr);
            return Err(astra_tools::git_gix::GitProcessError::Exit(format!(
                "git worktree add failed: {}. Worktree files may remain at {}",
                stderr.trim(),
                worktree_path.display()
            ))
            .into());
        }

        let session = WorktreeSession {
            worktree_path: worktree_path.clone(),
            branch_name: branch.to_string(),
            original_root: project_root,
            original_head_commit: Some(original_head),
            git_identity: worktree_identity(&worktree_path).map_err(|error| {
                WorktreeError::Domain(format!(
                    "Cannot identify the new worktree: {error}. Worktree was kept at {}",
                    worktree_path.display()
                ))
            })?,
            recovery_error: None,
        };

        // Persist before switching so a resumed session keeps the same root.
        // On failure the created checkout is retained and its path is shown.
        self.persist_selected_worktree(Some(&session), &session.original_root)
            .map_err(|error| {
                WorktreeError::Domain(format!(
                    "{error}. Worktree was kept at {}",
                    worktree_path.display()
                ))
            })?;
        *self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(session.clone());

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
            let guard = self
                .worktree_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            guard.clone().ok_or("Not in a worktree session")?
        };

        if session.recovery_error.is_some() && session.branch_name.is_empty() {
            return Err(
                "The workspace selection needs confirmation in the interactive session.".into(),
            );
        }
        if session.recovery_error.is_some() && action == "keep" {
            self.persist_selected_worktree(None, &session.original_root)?;
            *self
                .worktree_session
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
            self.clear_file_state();
            return Ok(format!(
                "Returned to {}. The previous worktree was not removed; check {} if it still exists.",
                session.original_root.display(),
                session.worktree_path.display()
            ));
        }

        let preserved_for_safety = cfg!(target_os = "macos") && action == "remove";
        let action = if preserved_for_safety { "keep" } else { action };

        if action == "remove" && !workspace_safe_for_cleanup(&session.worktree_path) {
            return Err("A previous command may still be running; the worktree was kept. Choose keep to return to the original workspace.".into());
        }

        // Keeping the checkout needs no Git inspection and remains available
        // even if Git is unavailable or the workspace is being repaired.
        let (changed_files, commits) = if action == "remove" {
            let changes = count_worktree_changes(
                &session.worktree_path,
                session.original_head_commit.as_deref(),
                &session.branch_name,
            )?;
            if !workspace_safe_for_cleanup(&session.worktree_path) {
                return Err("A command may still be running; the worktree was kept. Choose keep to return to the original workspace.".into());
            }
            changes
        } else {
            (0, 0)
        };

        if action == "remove" && (changed_files > 0 || commits > 0) && !discard_changes {
            let mut parts = Vec::new();
            if changed_files > 0 {
                parts.push(format!("{} uncommitted file(s)", changed_files));
            }
            if commits > 0 {
                parts.push(format!(
                    "{} changed commit(s) in {}",
                    commits, session.branch_name
                ));
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
        // Only forget the session once removal succeeded (or keep was selected)
        // and the session's durable selection has been updated.
        self.persist_selected_worktree(None, &session.original_root)?;
        *self
            .worktree_session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
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
            let note = if preserved_for_safety {
                " Automatic removal is unavailable on macOS; check for running processes before removing it with Git."
            } else {
                ""
            };
            Ok(format!(
                "✓ Exited worktree. Work preserved at {} on branch {}. Session restored to {}{}",
                worktree_path_str,
                branch_name,
                original_root.display(),
                note
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
                let start_point = args.get("start_point").and_then(Value::as_str);
                match self.enter_worktree_result(branch, start_point) {
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
                                "✓ Entered worktree\n  Branch: {}\n  Commit: {}\n  Path: {}\n  Session is now working in the worktree. Use `worktree` action=`exit` to leave.",
                                session.branch_name,
                                session.original_head_commit.as_deref().unwrap_or("unknown"),
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
                            && !cfg!(target_os = "macos")
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
