//! Git worktree session management for isolated branch work.
//!
//! Provides enter/exit workflow for git worktrees with session tracking,
//! change counting, and safe cleanup.

use std::path::{Path, PathBuf};

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
    Unsupported(String),
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
            Self::Domain(message) | Self::Unsupported(message) => write!(f, "{message}"),
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
            Self::Unsupported(message) => {
                let evidence = astra_core::ToolFailureEvidence::new(
                    astra_core::ErrorKind::ToolUnavailable,
                    astra_core::ToolFailureCause::CapabilityUnavailable,
                    false,
                    vec![astra_core::ToolRecoveryAction::InspectStructuredFailure],
                );
                let mut outcome = super::ToolExecutionOutcome::error_with_evidence(
                    format!("Error: {message}"),
                    evidence,
                );
                let fields = outcome.tool_result_fields.get_or_insert_default();
                astra_tools::execution_outcome::insert_not_executed_fact(fields);
                fields.insert("side_effects_maybe".into(), Value::Bool(false));
                outcome
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
    /// Creation baseline for a new worktree; absent for a selected existing branch.
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

fn existing_worktree_for_branch(
    root: &Path,
    branch: &str,
) -> Result<Option<PathBuf>, WorktreeError> {
    let output = astra_tools::git_gix::inspect_bound_git_worktree(
        root,
        &["worktree", "list", "--porcelain", "-z"],
    )?;
    if !output.status.success() {
        return Err(astra_tools::git_gix::GitProcessError::Exit(format!(
            "could not list worktrees: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
        .into());
    }

    let target = format!("branch refs/heads/{branch}");
    let mut path = None;
    let mut found = None;
    for field in output.stdout.split(|byte| *byte == 0) {
        if let Some(value) = field.strip_prefix(b"worktree ") {
            path = Some(value);
        } else if field == target.as_bytes() {
            let value = path.ok_or("Git returned a worktree branch without a path")?;
            #[cfg(unix)]
            let candidate = {
                use std::os::unix::ffi::OsStringExt;
                PathBuf::from(std::ffi::OsString::from_vec(value.to_vec()))
            };
            #[cfg(not(unix))]
            let candidate = PathBuf::from(
                std::str::from_utf8(value).map_err(|_| "Git returned an invalid worktree path")?,
            );
            if found.replace(candidate).is_some() {
                return Err(
                    "The branch is checked out in multiple worktrees; select it with Git first."
                        .into(),
                );
            }
        } else if field.is_empty() {
            path = None;
        }
    }
    Ok(found)
}

fn resolve_worktree_start_point(root: &Path, start_point: &str) -> Result<String, WorktreeError> {
    let revision = format!("{start_point}^{{commit}}");
    let resolved = astra_tools::git_gix::inspect_bound_git_worktree(
        root,
        &["rev-parse", "--verify", "--end-of-options", &revision],
    )?;
    if !resolved.status.success() {
        return Err("Worktree start point is not an existing commit".into());
    }
    Ok(String::from_utf8_lossy(&resolved.stdout).trim().to_string())
}

#[derive(Debug, Default)]
pub(crate) struct WorktreeSessionOwner {
    pub(super) active_session: std::sync::Arc<std::sync::Mutex<Option<WorktreeSession>>>,
    pub(super) transition: std::sync::Arc<tokio::sync::RwLock<()>>,
    restored_session_id: Option<String>,
    session_root: Option<PathBuf>,
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
        &[
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "status",
            "--porcelain",
            "--ignored=matching",
            "--untracked-files=all",
            "--ignore-submodules=none",
        ],
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
        // A selected branch is preserved, but detached or switched HEAD may
        // contain commits that removing the checkout would abandon.
        let repo = gix::open(worktree_path).map_err(|error| error.to_string())?;
        let expected_branch = format!("refs/heads/{branch}");
        let on_preserved_branch = repo
            .head_name()
            .ok()
            .flatten()
            .is_some_and(|head| head.to_string() == expected_branch)
            && repo.head_id().is_ok();
        usize::from(!on_preserved_branch)
    };

    Ok((changed_files, commits))
}

fn ensure_worktree_index_is_visible(worktree_path: &Path) -> Result<(), WorktreeError> {
    let output =
        astra_tools::git_gix::inspect_bound_git_worktree(worktree_path, &["ls-files", "-v", "-z"])?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(astra_tools::git_gix::GitProcessError::Exit(
            "failed to inspect the worktree index".to_string(),
        )
        .into());
    }
    // Git status ignores paths marked assume-unchanged or skip-worktree, so a
    // clean status cannot establish that removing their files is safe.
    if output.stdout.split(|byte| *byte == 0).any(|entry| {
        entry.len() >= 2 && entry[1] == b' ' && (entry[0].is_ascii_lowercase() || entry[0] == b'S')
    }) {
        return Err("Git skips checking some tracked files in this worktree; they may contain changes. Inspect them before setting discard_changes=true, or use exit_action=keep.".into());
    }
    Ok(())
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
            .worktree_owner
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
        self.worktree_owner
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

    /// Enter a worktree session, creating a checkout or selecting an existing one.
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
        if branch.starts_with('-')
            || branch == "HEAD"
            || gix::refs::FullName::try_from(format!("refs/heads/{branch}")).is_err()
        {
            return Err("Invalid Git branch name".into());
        }
        let existing = existing_worktree_for_branch(&project_root, branch)?;
        let (worktree_path, original_head) = if let Some(path) = existing {
            validate_saved_worktree(&project_root, &project_root, &path, None).map_err(
                |error| {
                    WorktreeError::Domain(format!(
                        "Cannot select the existing worktree at {}: {error}",
                        path.display()
                    ))
                },
            )?;
            if path.canonicalize().ok() == project_root.canonicalize().ok() {
                return Err("This branch is already checked out in the current workspace.".into());
            }
            let repo = gix::open(&path).map_err(|error| error.to_string())?;
            let actual_branch = repo.head_name().map_err(|error| error.to_string())?;
            if actual_branch.as_ref().map(ToString::to_string)
                != Some(format!("refs/heads/{branch}"))
            {
                return Err("The worktree branch changed while selecting it; try again.".into());
            }
            if let Some(start_point) = start_point
                && repo
                    .head_id()
                    .map_err(|error| error.to_string())?
                    .to_string()
                    != resolve_worktree_start_point(&project_root, start_point)?
            {
                return Err("The existing worktree is at a different commit than start_point; omit start_point to select it without changing its branch.".into());
            }
            // An existing branch is user-owned: its creation baseline is not
            // known, and selecting it must never grant authority to delete it.
            (path, None)
        } else {
            let original_head =
                resolve_worktree_start_point(&project_root, start_point.unwrap_or("HEAD"))?;
            let repo_name = project_root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("repo");
            let sanitized_branch = branch.replace('/', "-");
            let path = project_root
                .parent()
                .unwrap_or(&project_root)
                .join(format!("{repo_name}-wt-{sanitized_branch}"));

            let mut command = astra_tools::git_gix::prepare_bound_git_command(&project_root)
                .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?;
            command.args(["worktree", "add", "-b", branch]);
            command.arg(&path).arg(&original_head);
            let output = match command.output_with_ownership() {
                Ok(output) => output,
                Err(error) => {
                    if error.started && error.ownership.is_none() {
                        astra_tools::workspace_observation::mark_workspace_observation_unsettled(
                            &path,
                        );
                    }
                    return Err(astra_tools::git_gix::GitProcessError::Execution(error).into());
                }
            };
            if !output.output.status.success() {
                return Err(astra_tools::git_gix::GitProcessError::Exit(format!(
                    "git worktree add failed: {}. Worktree files may remain at {}",
                    String::from_utf8_lossy(&output.output.stderr).trim(),
                    path.display()
                ))
                .into());
            }
            (path, Some(original_head))
        };

        // Git may report the same checkout through a resolved path (for
        // example, /private/var instead of /var on macOS). Keep one identity
        // for session selection across turns and executors.
        let worktree_path = worktree_path.canonicalize().map_err(|error| {
            WorktreeError::Domain(format!(
                "Cannot resolve the worktree: {error}. Worktree was kept at {}",
                worktree_path.display()
            ))
        })?;
        let session = WorktreeSession {
            worktree_path: worktree_path.clone(),
            branch_name: branch.to_string(),
            original_root: project_root,
            original_head_commit: original_head,
            git_identity: worktree_identity(&worktree_path).map_err(|error| {
                WorktreeError::Domain(format!(
                    "Cannot identify the worktree: {error}. Worktree was kept at {}",
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

        if cfg!(target_os = "macos") && action == "remove" {
            return Err(WorktreeError::Unsupported(format!(
                "Automatic worktree removal is unavailable on macOS. Nothing was removed or discarded; the session is still in {}. Use exit_action=keep to leave, then check for running processes before removing the worktree with Git.",
                session.worktree_path.display()
            )));
        }

        if action == "remove" && !workspace_safe_for_cleanup(&session.worktree_path) {
            return Err("A previous command may still be running; the worktree was kept. Choose keep to return to the original workspace.".into());
        }

        // Keeping the checkout needs no Git inspection and remains available
        // even if Git is unavailable or the workspace is being repaired.
        let (changed_files, commits) = if action == "remove" {
            let (changed_files, commits) = count_worktree_changes(
                &session.worktree_path,
                session.original_head_commit.as_deref(),
                &session.branch_name,
            )?;
            if !workspace_safe_for_cleanup(&session.worktree_path) {
                return Err("A command may still be running; the worktree was kept. Choose keep to return to the original workspace.".into());
            }
            (changed_files, commits)
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
        if action == "remove" && !discard_changes {
            ensure_worktree_index_is_visible(&session.worktree_path)?;
            if !workspace_safe_for_cleanup(&session.worktree_path) {
                return Err("A command may still be running; the worktree was kept. Choose keep to return to the original workspace.".into());
            }
        }

        let worktree_path_str = session.worktree_path.display().to_string();
        let branch_name = session.branch_name.clone();
        let original_root = session.original_root.clone();
        let cleanup_commands = if action == "remove" {
            Some((
                astra_tools::git_gix::prepare_bound_git_command(&original_root)
                    .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)?,
                session
                    .original_head_commit
                    .as_ref()
                    .map(|_| {
                        astra_tools::git_gix::prepare_bound_git_command(&original_root)
                            .map_err(astra_tools::git_gix::GitProcessError::RepositoryBinding)
                    })
                    .transpose()?,
            ))
        } else {
            None
        };

        let branch_warning = if let Some((mut remove_command, delete_branch_command)) =
            cleanup_commands
        {
            remove_command.args(["worktree", "remove"]);
            if discard_changes {
                remove_command.arg("--force");
            }
            let removed = remove_command
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
            match delete_branch_command {
                Some(mut command) => match command.args(["branch", "-D", &branch_name]).output() {
                    Ok(out) if out.status.success() => String::new(),
                    Ok(out) => format!(
                        " Branch cleanup failed: {}",
                        String::from_utf8_lossy(&out.stderr).trim()
                    ),
                    Err(error) => format!(" Branch cleanup failed: {error}"),
                },
                None => format!(" Existing branch {branch_name} was preserved."),
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
            let discard_note = if discard_changes {
                " Local changes may have been discarded."
            } else {
                ""
            };

            Ok(format!(
                "✓ Exited and removed worktree at {}.{} Session restored to {}{}",
                worktree_path_str,
                discard_note,
                original_root.display(),
                branch_warning
            ))
        } else {
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
                let start_point = args.get("start_point").and_then(Value::as_str);
                match self.enter_worktree_result(branch, start_point) {
                    Ok(session) => {
                        let created = session.original_head_commit.is_some();
                        let mut tool_result_fields = serde_json::Map::from_iter([
                            (
                                "worktree_path".to_string(),
                                Value::String(session.worktree_path.display().to_string()),
                            ),
                            (
                                "branch".to_string(),
                                Value::String(session.branch_name.clone()),
                            ),
                            ("session_scoped".to_string(), Value::Bool(true)),
                        ]);
                        if let Some(original_head_commit) = session.original_head_commit.as_ref() {
                            tool_result_fields.insert(
                                "original_head_commit".to_string(),
                                Value::String(original_head_commit.clone()),
                            );
                        }
                        super::ToolExecutionOutcome {
                            sandbox_approval_scope: None,
                            output: format!(
                                "✓ Entered {}worktree\n  Branch: {}\n{}  Path: {}\n  Session is now working in the worktree. Use `worktree` action=`exit` to leave.",
                                if created { "" } else { "existing " },
                                session.branch_name,
                                session
                                    .original_head_commit
                                    .as_ref()
                                    .map(|head| format!("  Commit: {head}\n"))
                                    .unwrap_or_default(),
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
                match self.exit_worktree_result(exit_action, discard) {
                    Ok(msg) => super::ToolExecutionOutcome {
                        sandbox_approval_scope: None,
                        output: msg,
                        tool_result_fields: None,
                        is_error: false,
                    },
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

#[cfg(test)]
mod tests {
    use super::{count_worktree_changes, ensure_worktree_index_is_visible};
    use std::process::Command;

    #[test]
    fn cleanup_inspection_does_not_trust_hidden_git_index_flags() {
        let repo = crate::edge_tools::tests::worktree_tests::init_temp_git_repo();
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .current_dir(repo.path())
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git {args:?}: {output:?}");
        };
        assert!(ensure_worktree_index_is_visible(repo.path()).is_ok());
        for (set, clear) in [
            ("--assume-unchanged", "--no-assume-unchanged"),
            ("--skip-worktree", "--no-skip-worktree"),
        ] {
            git(&["update-index", set, "tracked.txt"]);
            assert!(ensure_worktree_index_is_visible(repo.path()).is_err());
            git(&["update-index", clear, "tracked.txt"]);
        }
        git(&["config", "status.showUntrackedFiles", "no"]);
        std::fs::write(repo.path().join("untracked.txt"), "local work\n").unwrap();
        let (files, _) = count_worktree_changes(repo.path(), None, "unused").unwrap();
        assert_eq!(
            files, 1,
            "repository configuration must not hide untracked work"
        );
    }
}
