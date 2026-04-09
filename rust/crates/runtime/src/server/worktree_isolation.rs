//! Git worktree isolation for multi-agent parallel file editing.
//!
//! When a team executes with [`WorktreeMode::Isolated`], each agent gets its own
//! git worktree branched from the current HEAD. After execution, worktrees are
//! merged back with 3-way merge and cleaned up.
//!
//! # Lifecycle
//!
//! ```text
//! create_worktrees()  →  per-agent branches + worktree dirs
//!       ↓
//! [agents execute in their own worktrees]
//!       ↓
//! merge_worktrees()   →  3-way merge back to main branch
//!       ↓
//! cleanup()           →  remove worktree dirs + temp branches
//! ```

use astra_core::composite_snapshot::{CompositeSnapshot, SnapshotRef};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;

// ─── Types ──────────────────────────────────────────────────────────────────

/// Sanitize agent_id for safe use in git branch names and filesystem paths.
/// Returns `None` if the sanitized result is empty.
fn sanitize_agent_id(id: &str) -> Option<String> {
    let sanitized: String = id
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if sanitized.is_empty() {
        None
    } else {
        Some(sanitized)
    }
}

/// Shared lock for serializing git operations on a single repository.
///
/// Multiple concurrent team executions on the same repo must hold this lock
/// during merge operations to prevent git index corruption.
pub type RepoLock = Arc<Mutex<()>>;

/// Create a new [`RepoLock`].
pub fn new_repo_lock() -> RepoLock {
    Arc::new(Mutex::new(()))
}

/// Manages git worktrees for multi-agent parallel file editing.
pub struct WorktreeManager {
    repo_root: PathBuf,
    worktree_base: PathBuf,
    active: HashMap<String, WorktreeInfo>,
    repo_lock: RepoLock,
    /// Optional conflict resolver for LLM-assisted merge conflict resolution.
    conflict_resolver: Option<Arc<dyn super::conflict_resolver::ConflictResolver>>,
    /// Task context for the conflict resolver (team task description).
    task_context: String,
}

/// Metadata for a single agent's worktree.
#[derive(Debug, Clone)]
pub struct WorktreeInfo {
    pub agent_id: String,
    pub branch_name: String,
    pub worktree_path: PathBuf,
    pub base_commit: String,
    pub snapshot: Option<CompositeSnapshot>,
}

/// Result of merging all agent worktrees back.
#[derive(Debug, Default)]
pub struct MergeResult {
    /// Agents whose branches were successfully merged.
    pub merged: Vec<String>,
    /// Agents that had no new commits.
    pub skipped: Vec<String>,
    /// Agents whose branches conflicted.
    pub conflicts: Vec<ConflictInfo>,
}

/// Information about a merge conflict.
#[derive(Debug)]
pub struct ConflictInfo {
    pub agent_id: String,
    pub files: Vec<String>,
    /// Snapshot at the point before the failed merge, enabling rollback.
    pub snapshot: CompositeSnapshot,
}

/// Errors from worktree operations.
#[derive(Debug)]
pub enum WorktreeError {
    Git(String),
    Io(std::io::Error),
    NoWorktree(String),
}

impl std::fmt::Display for WorktreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Git(msg) => write!(f, "git error: {msg}"),
            Self::Io(e) => write!(f, "io error: {e}"),
            Self::NoWorktree(id) => write!(f, "no worktree for agent '{id}'"),
        }
    }
}

impl std::error::Error for WorktreeError {}

impl From<std::io::Error> for WorktreeError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

// ─── Implementation ─────────────────────────────────────────────────────────

impl WorktreeManager {
    /// Create a new manager rooted at the given git repository.
    pub fn new(repo_root: PathBuf) -> Self {
        let worktree_base = astra_core::worktree_base_path();
        Self {
            repo_root,
            worktree_base,
            active: HashMap::new(),
            repo_lock: new_repo_lock(),
            conflict_resolver: None,
            task_context: String::new(),
        }
    }

    /// Create a new manager with a shared repository lock.
    ///
    /// Use this when multiple team executions may target the same repo
    /// concurrently — pass the same [`RepoLock`] to each manager.
    pub fn with_repo_lock(mut self, lock: RepoLock) -> Self {
        self.repo_lock = lock;
        self
    }

    /// Set an LLM-based (or other) conflict resolver for automatic merge
    /// conflict resolution.
    pub fn with_conflict_resolver(
        mut self,
        resolver: Arc<dyn super::conflict_resolver::ConflictResolver>,
        task_context: String,
    ) -> Self {
        self.conflict_resolver = Some(resolver);
        self.task_context = task_context;
        self
    }

    /// Override the base directory for worktrees (useful for testing).
    pub fn with_worktree_base(mut self, base: PathBuf) -> Self {
        self.worktree_base = base;
        self
    }

    /// Get the current HEAD commit SHA.
    async fn current_head(&self) -> Result<String, WorktreeError> {
        let output = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&self.repo_root)
            .output()
            .await?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Create independent worktrees for each agent.
    ///
    /// Each agent gets its own branch (`agent/{id}-{delegation_id_prefix}`) and
    /// worktree directory. Returns a map of agent_id → worktree path.
    ///
    /// Holds the repository lock to prevent concurrent git operations.
    pub async fn create_worktrees(
        &mut self,
        delegation_id: &str,
        agent_ids: &[String],
    ) -> Result<HashMap<String, PathBuf>, WorktreeError> {
        // Reject duplicate agent_ids early — they'd produce identical branch names
        let mut seen = HashSet::with_capacity(agent_ids.len());
        for id in agent_ids {
            if !seen.insert(id.as_str()) {
                return Err(WorktreeError::Git(format!(
                    "duplicate agent_id '{id}' in create_worktrees"
                )));
            }
        }

        let _lock = self.repo_lock.clone().lock_owned().await;
        let base_commit = self.current_head().await?;
        let del_prefix = &delegation_id[..delegation_id.len().min(8)];

        // Ensure base directory exists
        tokio::fs::create_dir_all(&self.worktree_base).await?;

        for agent_id in agent_ids {
            let safe_id = sanitize_agent_id(agent_id).ok_or_else(|| {
                WorktreeError::Git(format!(
                    "agent_id '{agent_id}' contains no valid characters for branch names"
                ))
            })?;
            let branch_name = format!("agent/{safe_id}-{del_prefix}");
            let wt_path = self.worktree_base.join(branch_name.replace('/', "_"));

            let wt_path_str = wt_path.to_str().ok_or_else(|| {
                WorktreeError::Git(format!(
                    "worktree path contains invalid UTF-8: {}",
                    wt_path.to_string_lossy()
                ))
            })?;

            // git worktree add -b <branch> <path> HEAD
            let output = Command::new("git")
                .args(["worktree", "add", "-b", &branch_name, wt_path_str, "HEAD"])
                .current_dir(&self.repo_root)
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                // Clean up any worktrees already created before this failure
                let _ = self.cleanup().await;
                return Err(WorktreeError::Git(format!(
                    "failed to create worktree for {agent_id}: {stderr}"
                )));
            }

            let snapshot = CompositeSnapshot {
                snapshot_id: format!("wt-{del_prefix}-{agent_id}"),
                session_id: delegation_id.to_string(),
                turn: 0,
                created_at: chrono::Utc::now().to_rfc3339(),
                label: Some(format!("worktree fork for {agent_id}")),
                refs: vec![SnapshotRef::GitCommit(base_commit.clone())],
            };

            self.active.insert(
                agent_id.clone(),
                WorktreeInfo {
                    agent_id: agent_id.clone(),
                    branch_name,
                    worktree_path: wt_path,
                    base_commit: base_commit.clone(),
                    snapshot: Some(snapshot),
                },
            );
        }

        Ok(self
            .active
            .iter()
            .map(|(id, info)| (id.clone(), info.worktree_path.clone()))
            .collect())
    }

    /// Check if a branch has commits beyond the base.
    async fn has_commits_since(&self, branch: &str, base: &str) -> Result<bool, WorktreeError> {
        let output = Command::new("git")
            .args(["log", "--oneline", &format!("{base}..{branch}")])
            .current_dir(&self.repo_root)
            .output()
            .await?;
        Ok(!output.stdout.is_empty())
    }

    /// Get list of conflicting files from a failed merge.
    async fn get_conflict_files(&self) -> Result<Vec<String>, WorktreeError> {
        let output = Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=U"])
            .current_dir(&self.repo_root)
            .output()
            .await?;
        Ok(String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|l| l.to_string())
            .collect())
    }

    /// Abort a merge in progress.
    async fn git_merge_abort(&self) -> Result<(), WorktreeError> {
        let output = Command::new("git")
            .args(["merge", "--abort"])
            .current_dir(&self.repo_root)
            .output()
            .await?;
        if !output.status.success() {
            return Err(WorktreeError::Git(
                String::from_utf8_lossy(&output.stderr).to_string(),
            ));
        }
        Ok(())
    }

    /// Merge all agent worktrees back to the current branch.
    ///
    /// `merge_order` determines priority when conflicts arise — earlier agents'
    /// changes are merged first and take precedence.
    ///
    /// Holds the repository lock for the duration of all merges to prevent
    /// concurrent git operations from corrupting the index.
    pub async fn merge_worktrees(
        &self,
        delegation_id: &str,
        merge_order: &[String],
    ) -> Result<MergeResult, WorktreeError> {
        let _lock = self.repo_lock.clone().lock_owned().await;
        let mut result = MergeResult::default();

        for agent_id in merge_order {
            let info = self
                .active
                .get(agent_id)
                .ok_or_else(|| WorktreeError::NoWorktree(agent_id.clone()))?;

            // Check if agent made any changes
            let has_changes = self
                .has_commits_since(&info.branch_name, &info.base_commit)
                .await?;
            if !has_changes {
                result.skipped.push(agent_id.clone());
                continue;
            }

            // Capture current HEAD before this merge attempt so that the
            // conflict snapshot enables rollback to the correct state (which
            // includes prior successful merges, not the original base).
            let pre_merge_head = self.current_head().await?;

            // Attempt merge
            let output = Command::new("git")
                .args([
                    "merge",
                    "--no-ff",
                    "-m",
                    &format!(
                        "merge agent {agent_id} from team delegation {}",
                        &delegation_id[..delegation_id.len().min(8)]
                    ),
                    &info.branch_name,
                ])
                .current_dir(&self.repo_root)
                .output()
                .await?;

            if output.status.success() {
                result.merged.push(agent_id.clone());
            } else {
                // Merge conflict — try LLM resolution if a resolver is configured
                let conflict_files = self.get_conflict_files().await?;

                let resolved_by_llm = if let Some(ref resolver) = self.conflict_resolver {
                    // Extract base/ours/theirs while merge is still in progress
                    let file_conflicts = super::conflict_resolver::extract_file_conflicts(
                        &self.repo_root,
                        &conflict_files,
                    )
                    .await;

                    let resolution = resolver
                        .resolve_conflicts(agent_id, &self.task_context, &file_conflicts)
                        .await;

                    if resolution.failed.is_empty() && !resolution.resolved.is_empty() {
                        // All files resolved — apply and commit
                        match super::conflict_resolver::apply_resolutions(
                            &self.repo_root,
                            agent_id,
                            delegation_id,
                            &resolution.resolved,
                        )
                        .await
                        {
                            Ok(()) => {
                                eprintln!(
                                    "[worktree] LLM resolved {} conflict(s) for agent {agent_id}",
                                    resolution.resolved.len()
                                );
                                true
                            }
                            Err(e) => {
                                eprintln!(
                                    "[worktree] LLM resolution apply failed for {agent_id}: {e}"
                                );
                                false
                            }
                        }
                    } else {
                        eprintln!(
                            "[worktree] LLM could not resolve all conflicts for {agent_id} \
                             ({} resolved, {} failed)",
                            resolution.resolved.len(),
                            resolution.failed.len()
                        );
                        false
                    }
                } else {
                    false
                };

                if resolved_by_llm {
                    result.merged.push(agent_id.clone());
                } else {
                    let pre_merge_head_ref = pre_merge_head.clone();
                    let conflict_snapshot = CompositeSnapshot {
                        snapshot_id: format!(
                            "conflict-{}-{agent_id}",
                            &delegation_id[..delegation_id.len().min(8)]
                        ),
                        session_id: delegation_id.to_string(),
                        turn: 0,
                        created_at: chrono::Utc::now().to_rfc3339(),
                        label: Some(format!("merge conflict for {agent_id}")),
                        refs: vec![SnapshotRef::GitCommit(pre_merge_head_ref)],
                    };
                    result.conflicts.push(ConflictInfo {
                        agent_id: agent_id.clone(),
                        files: conflict_files,
                        snapshot: conflict_snapshot,
                    });
                    // Abort the failed merge so we can continue with others
                    self.git_merge_abort().await?;
                }
            }
        }

        Ok(result)
    }

    /// Remove all worktrees and their temporary branches.
    ///
    /// Logs warnings for individual failures but continues cleaning up
    /// remaining worktrees. Returns an error only if *all* removals failed.
    pub async fn cleanup(&mut self) -> Result<(), WorktreeError> {
        let mut failures = 0usize;
        let total = self.active.len();

        for (_, info) in self.active.drain() {
            // git worktree remove <path> --force
            let wt_result = Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    &info.worktree_path.to_string_lossy(),
                ])
                .current_dir(&self.repo_root)
                .output()
                .await;

            if let Err(e) = &wt_result {
                eprintln!(
                    "[worktree] warning: failed to remove worktree {:?}: {}",
                    info.worktree_path, e
                );
                failures += 1;
            } else if let Ok(out) = &wt_result {
                if !out.status.success() {
                    eprintln!(
                        "[worktree] warning: git worktree remove {:?} exited {}: {}",
                        info.worktree_path,
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                    failures += 1;
                }
            }

            // git branch -D <branch>
            let br_result = Command::new("git")
                .args(["branch", "-D", &info.branch_name])
                .current_dir(&self.repo_root)
                .output()
                .await;

            if let Err(e) = &br_result {
                eprintln!(
                    "[worktree] warning: failed to delete branch {}: {}",
                    info.branch_name, e
                );
            } else if let Ok(out) = &br_result {
                if !out.status.success() {
                    eprintln!(
                        "[worktree] warning: git branch -D {} exited {}: {}",
                        info.branch_name,
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
            }
        }

        if failures > 0 && failures == total {
            Err(WorktreeError::Git(format!(
                "cleanup failed: all {} worktree removals failed",
                total
            )))
        } else {
            Ok(())
        }
    }

    /// Get the worktree path for a specific agent.
    pub fn worktree_path(&self, agent_id: &str) -> Option<&Path> {
        self.active.get(agent_id).map(|i| i.worktree_path.as_path())
    }

    /// Get info for all active worktrees.
    pub fn active_worktrees(&self) -> &HashMap<String, WorktreeInfo> {
        &self.active
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_manager(repo: &Path) -> WorktreeManager {
        let base = repo.join("_worktrees");
        WorktreeManager::new(repo.to_path_buf()).with_worktree_base(base)
    }

    /// Initialise a bare-minimum git repo in a temp dir.
    async fn init_test_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let dir = dir.to_path_buf();
            let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            async move {
                Command::new("git")
                    .args(&args)
                    .current_dir(&dir)
                    .output()
                    .await
                    .expect("git command failed")
            }
        };

        run(&["init"]).await;
        run(&["config", "user.email", "test@test.com"]).await;
        run(&["config", "user.name", "Test"]).await;

        tokio::fs::write(dir.join("README.md"), "# test\n")
            .await
            .unwrap();
        run(&["add", "."]).await;
        run(&["commit", "-m", "init"]).await;
    }

    #[tokio::test]
    async fn create_and_cleanup_worktrees() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);

        let agents = vec!["alpha".to_string(), "beta".to_string()];
        let paths = mgr.create_worktrees("del-12345678", &agents).await.unwrap();

        assert_eq!(paths.len(), 2);
        for (_, p) in &paths {
            assert!(p.exists(), "worktree path should exist: {p:?}");
        }
        assert_eq!(mgr.active_worktrees().len(), 2);

        // Verify worktree_path accessor
        assert!(mgr.worktree_path("alpha").is_some());
        assert!(mgr.worktree_path("gamma").is_none());

        // Verify snapshots
        let info = &mgr.active["alpha"];
        assert!(info.snapshot.is_some());
        assert!(info.snapshot.as_ref().unwrap().has_git_commit());

        // Cleanup
        mgr.cleanup().await.unwrap();
        assert!(mgr.active_worktrees().is_empty());
    }

    #[tokio::test]
    async fn merge_skips_unchanged_branches() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["agent-a".to_string()];
        mgr.create_worktrees("del-aabbccdd", &agents).await.unwrap();

        // Don't commit anything in the worktree → should be skipped
        let result = mgr
            .merge_worktrees("del-aabbccdd", &["agent-a".to_string()])
            .await
            .unwrap();

        assert!(result.merged.is_empty());
        assert_eq!(result.skipped, vec!["agent-a"]);
        assert!(result.conflicts.is_empty());

        mgr.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn merge_successful_with_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["coder".to_string()];
        let paths = mgr.create_worktrees("del-11223344", &agents).await.unwrap();

        // Make a commit in the agent's worktree
        let wt_path = &paths["coder"];
        tokio::fs::write(wt_path.join("new_file.txt"), "hello from coder\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(wt_path)
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "coder adds file"])
            .current_dir(wt_path)
            .output()
            .await
            .unwrap();

        let result = mgr
            .merge_worktrees("del-11223344", &["coder".to_string()])
            .await
            .unwrap();

        assert_eq!(result.merged, vec!["coder"]);
        assert!(result.conflicts.is_empty());

        // Verify the file is now in main
        assert!(repo.join("new_file.txt").exists());

        mgr.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn merge_detects_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["a1".to_string(), "a2".to_string()];
        let paths = mgr
            .create_worktrees("del-conflict1", &agents)
            .await
            .unwrap();

        // Agent a1 modifies README.md
        tokio::fs::write(paths["a1"].join("README.md"), "agent a1 version\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a1 changes readme"])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();

        // Agent a2 also modifies README.md differently
        tokio::fs::write(paths["a2"].join("README.md"), "agent a2 version\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a2 changes readme"])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();

        // Merge a1 first (should succeed), then a2 (should conflict)
        let result = mgr
            .merge_worktrees("del-conflict1", &["a1".to_string(), "a2".to_string()])
            .await
            .unwrap();

        assert_eq!(result.merged, vec!["a1"]);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].agent_id, "a2");
        assert!(result.conflicts[0].files.contains(&"README.md".to_string()));

        mgr.cleanup().await.unwrap();
    }

    #[test]
    fn sanitize_agent_id_rejects_empty() {
        assert!(sanitize_agent_id("@@@").is_none());
        assert!(sanitize_agent_id("").is_none());
        assert_eq!(sanitize_agent_id("ok-1").unwrap(), "ok-1");
        assert_eq!(sanitize_agent_id("a!b@c").unwrap(), "abc");
    }

    #[tokio::test]
    async fn create_worktree_rejects_invalid_agent_id() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["@@@".to_string()];
        let result = mgr.create_worktrees("del-12345678", &agents).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("no valid characters"), "got: {err}");
    }

    #[tokio::test]
    async fn conflict_snapshot_references_pre_merge_head() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["a1".to_string(), "a2".to_string()];
        let paths = mgr
            .create_worktrees("del-snaptest1", &agents)
            .await
            .unwrap();
        let original_base = mgr.active["a1"].base_commit.clone();

        // a1: modify README.md
        tokio::fs::write(paths["a1"].join("README.md"), "a1 version\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a1"])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();

        // a2: modify README.md differently (will conflict)
        tokio::fs::write(paths["a2"].join("README.md"), "a2 version\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a2"])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();

        let result = mgr
            .merge_worktrees("del-snaptest1", &["a1".into(), "a2".into()])
            .await
            .unwrap();

        assert_eq!(result.merged, vec!["a1"]);
        assert_eq!(result.conflicts.len(), 1);

        // The conflict snapshot should reference the post-a1-merge HEAD,
        // NOT the original base commit.
        let conflict_snap = &result.conflicts[0].snapshot;
        let snap_commit = conflict_snap
            .refs
            .iter()
            .find_map(|r| match r {
                SnapshotRef::GitCommit(sha) => Some(sha.clone()),
                _ => None,
            })
            .expect("conflict snapshot should have a GitCommit ref");

        assert_ne!(
            snap_commit, original_base,
            "conflict snapshot should reference pre-merge HEAD (after a1 merged), not original base"
        );

        mgr.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn create_worktree_rejects_duplicate_agent_ids() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["coder".to_string(), "coder".to_string()];
        let result = mgr.create_worktrees("del-dup12345", &agents).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[tokio::test]
    async fn merge_empty_order_returns_empty_result() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let mut mgr = make_manager(&repo);
        let agents = vec!["alpha".to_string()];
        mgr.create_worktrees("del-empty1234", &agents)
            .await
            .unwrap();

        let result = mgr.merge_worktrees("del-empty1234", &[]).await.unwrap();
        assert!(result.merged.is_empty());
        assert!(result.skipped.is_empty());
        assert!(result.conflicts.is_empty());

        mgr.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn llm_resolver_auto_resolves_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let resolver: Arc<dyn super::super::conflict_resolver::ConflictResolver> =
            Arc::new(super::super::conflict_resolver::TheirsWinsResolver);
        let base = repo.join("_worktrees");
        let mut mgr = WorktreeManager::new(repo.clone())
            .with_worktree_base(base)
            .with_conflict_resolver(resolver, "test task".to_string());

        let agents = vec!["a1".to_string(), "a2".to_string()];
        let paths = mgr.create_worktrees("del-resolve1", &agents).await.unwrap();

        // a1: modify README.md
        tokio::fs::write(paths["a1"].join("README.md"), "a1 content\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a1"])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();

        // a2: modify README.md differently (conflict)
        tokio::fs::write(paths["a2"].join("README.md"), "a2 content\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a2"])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();

        // With TheirsWinsResolver, a2's conflict should be auto-resolved
        let result = mgr
            .merge_worktrees("del-resolve1", &["a1".to_string(), "a2".to_string()])
            .await
            .unwrap();

        // Both should be merged (a1 directly, a2 via LLM resolution)
        assert_eq!(
            result.merged.len(),
            2,
            "expected both agents merged, got: {:?}",
            result.merged
        );
        assert!(
            result.conflicts.is_empty(),
            "expected no conflicts, got: {:?}",
            result.conflicts
        );

        // Verify the resolved content is a2's version (theirs wins)
        let final_content = tokio::fs::read_to_string(repo.join("README.md"))
            .await
            .unwrap();
        assert_eq!(final_content, "a2 content\n");

        mgr.cleanup().await.unwrap();
    }

    #[tokio::test]
    async fn failing_resolver_falls_back_to_conflict() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().to_path_buf();
        init_test_repo(&repo).await;

        let resolver: Arc<dyn super::super::conflict_resolver::ConflictResolver> =
            Arc::new(super::super::conflict_resolver::FailingResolver);
        let base = repo.join("_worktrees");
        let mut mgr = WorktreeManager::new(repo.clone())
            .with_worktree_base(base)
            .with_conflict_resolver(resolver, "test task".to_string());

        let agents = vec!["a1".to_string(), "a2".to_string()];
        let paths = mgr.create_worktrees("del-failres1", &agents).await.unwrap();

        // Both modify same file
        tokio::fs::write(paths["a1"].join("README.md"), "a1\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a1"])
            .current_dir(&paths["a1"])
            .output()
            .await
            .unwrap();

        tokio::fs::write(paths["a2"].join("README.md"), "a2\n")
            .await
            .unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "a2"])
            .current_dir(&paths["a2"])
            .output()
            .await
            .unwrap();

        let result = mgr
            .merge_worktrees("del-failres1", &["a1".to_string(), "a2".to_string()])
            .await
            .unwrap();

        // a1 merged fine, a2 should fall back to conflict since resolver fails
        assert_eq!(result.merged, vec!["a1"]);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.conflicts[0].agent_id, "a2");

        mgr.cleanup().await.unwrap();
    }
}
