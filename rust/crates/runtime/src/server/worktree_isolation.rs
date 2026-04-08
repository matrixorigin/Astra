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
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tokio::process::Command;

// ─── Types ──────────────────────────────────────────────────────────────────

/// Sanitize agent_id for safe use in git branch names and filesystem paths.
fn sanitize_agent_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .collect()
}

/// Manages git worktrees for multi-agent parallel file editing.
pub struct WorktreeManager {
    repo_root: PathBuf,
    worktree_base: PathBuf,
    active: HashMap<String, WorktreeInfo>,
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
        let worktree_base = std::env::temp_dir().join("mo-agent-worktrees");
        Self {
            repo_root,
            worktree_base,
            active: HashMap::new(),
        }
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
    pub async fn create_worktrees(
        &mut self,
        delegation_id: &str,
        agent_ids: &[String],
    ) -> Result<HashMap<String, PathBuf>, WorktreeError> {
        let base_commit = self.current_head().await?;
        let del_prefix = &delegation_id[..delegation_id.len().min(8)];

        // Ensure base directory exists
        tokio::fs::create_dir_all(&self.worktree_base).await?;

        for agent_id in agent_ids {
            let safe_id = sanitize_agent_id(agent_id);
            let branch_name = format!("agent/{safe_id}-{del_prefix}");
            let wt_path = self.worktree_base.join(&branch_name.replace('/', "_"));

            // git worktree add -b <branch> <path> HEAD
            let output = Command::new("git")
                .args([
                    "worktree",
                    "add",
                    "-b",
                    &branch_name,
                    wt_path.to_str().unwrap_or_default(),
                    "HEAD",
                ])
                .current_dir(&self.repo_root)
                .output()
                .await?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
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
    async fn has_commits_since(
        &self,
        branch: &str,
        base: &str,
    ) -> Result<bool, WorktreeError> {
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
    pub async fn merge_worktrees(
        &self,
        delegation_id: &str,
        merge_order: &[String],
    ) -> Result<MergeResult, WorktreeError> {
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

            // Attempt merge
            let output = Command::new("git")
                .args(["merge", "--no-ff", "-m", &format!("merge agent {agent_id} from team delegation {}", &delegation_id[..delegation_id.len().min(8)]), &info.branch_name])
                .current_dir(&self.repo_root)
                .output()
                .await?;

            if output.status.success() {
                result.merged.push(agent_id.clone());
            } else {
                // Merge conflict
                let conflict_files = self.get_conflict_files().await?;
                let conflict_snapshot = CompositeSnapshot {
                    snapshot_id: format!("conflict-{}-{agent_id}", &delegation_id[..delegation_id.len().min(8)]),
                    session_id: delegation_id.to_string(),
                    turn: 0,
                    created_at: chrono::Utc::now().to_rfc3339(),
                    label: Some(format!("merge conflict for {agent_id}")),
                    refs: vec![SnapshotRef::GitCommit(info.base_commit.clone())],
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

        Ok(result)
    }

    /// Remove all worktrees and their temporary branches.
    pub async fn cleanup(&mut self) -> Result<(), WorktreeError> {
        for (_, info) in self.active.drain() {
            // git worktree remove <path> --force
            let _ = Command::new("git")
                .args([
                    "worktree",
                    "remove",
                    "--force",
                    info.worktree_path.to_str().unwrap_or_default(),
                ])
                .current_dir(&self.repo_root)
                .output()
                .await;

            // git branch -D <branch>
            let _ = Command::new("git")
                .args(["branch", "-D", &info.branch_name])
                .current_dir(&self.repo_root)
                .output()
                .await;
        }
        Ok(())
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
        let paths = mgr
            .create_worktrees("del-12345678", &agents)
            .await
            .unwrap();

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
        mgr.create_worktrees("del-aabbccdd", &agents)
            .await
            .unwrap();

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
        let paths = mgr.create_worktrees("del-conflict1", &agents).await.unwrap();

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
}
