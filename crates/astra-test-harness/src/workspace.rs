//! Workspace isolation for live harness runs.
//!
//! A live harness invocation must not claim the checkout in which the caller
//! may already have an active TUI session. The suite's source repository is
//! inspected once, then each root execution job receives its own temporary
//! detached worktree. Follow-up turns stay in that same worktree. An
//! explicitly supplied working directory is left untouched because it is an
//! intentional execution boundary owned by the caller.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tempfile::{Builder, TempDir};

/// Immutable Git input selected for one harness invocation.
///
/// The repository may receive a new clean commit while a long run is in
/// flight.  The revision is therefore captured at admission and every job
/// checkout is created from this object rather than from the repository's
/// then-current `HEAD`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceSnapshot {
    repository_root: PathBuf,
    revision: String,
}

impl SourceSnapshot {
    /// Capture a clean repository and its exact commit ID.
    pub fn capture(repository_root: impl AsRef<Path>) -> Result<Self> {
        let repository_root = repository_root.as_ref().to_path_buf();
        ensure_source_clean(&repository_root)?;
        let revision = git_revision(&repository_root)?;
        Ok(Self {
            repository_root,
            revision,
        })
    }

    pub fn repository_root(&self) -> &Path {
        &self.repository_root
    }

    pub fn revision(&self) -> &str {
        &self.revision
    }
}

/// Find the Git repository that contains the suite directory.
///
/// Only the suite path is authoritative. The binary location and process CWD
/// are deliberately not considered: a suite outside the source repository
/// must not silently run in an unrelated Astra checkout.
pub fn repository_root_for_suite(suite_dir: &Path) -> Result<Option<PathBuf>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(suite_dir)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .with_context(|| {
            format!(
                "inspect Git source repository for harness suite {}",
                suite_dir.display()
            )
        })?;
    if output.status.success() {
        let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if root.is_empty() {
            anyhow::bail!(
                "Git reported an empty source repository for harness suite {}",
                suite_dir.display()
            );
        }
        return Ok(Some(PathBuf::from(root)));
    }

    let detail = command_detail(&output.stderr, &output.stdout);
    if is_non_repository_diagnostic(&detail) {
        return Ok(None);
    }
    anyhow::bail!(
        "could not determine the Git source repository for harness suite {}: {}",
        suite_dir.display(),
        detail
    )
}

/// Reject a dirty source repository before creating a detached worktree.
///
/// A worktree from a commit cannot represent uncommitted or untracked source
/// changes. Failing closed keeps the files visible to the agent at one
/// committed source snapshot. Binary provenance is resolved separately by
/// the caller and is never inferred from this check.
pub fn ensure_source_clean(repository_root: &Path) -> Result<()> {
    let output = Command::new("git")
        .current_dir(repository_root)
        .args(["status", "--porcelain=v1", "--untracked-files=all"])
        .output()
        .with_context(|| {
            format!(
                "inspect source changes before isolating harness worktree {}",
                repository_root.display()
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "could not inspect source changes in {}: {}",
            repository_root.display(),
            command_detail(&output.stderr, &output.stdout)
        );
    }
    if !output.stdout.is_empty() {
        anyhow::bail!(
            "harness source repository {} has uncommitted or untracked changes; commit the source being evaluated or pass --working-dir explicitly",
            repository_root.display()
        );
    }
    Ok(())
}

/// Resolve and validate the source snapshot for a suite run.
///
/// A suite outside a Git worktree intentionally keeps the caller's execution
/// directory semantics.  A Git suite must be clean at admission so the
/// snapshot names exactly the source that is being evaluated.
pub fn source_snapshot_for_suite(suite_dir: &Path) -> Result<Option<SourceSnapshot>> {
    let Some(repository_root) = repository_root_for_suite(suite_dir)? else {
        return Ok(None);
    };
    Ok(Some(SourceSnapshot::capture(repository_root)?))
}

/// Validate that the source still represents the snapshot during admission.
///
/// This is intentionally separate from [`IsolatedWorkspace::create`]. Once a
/// suite is admitted, a later clean commit is harmless because jobs are
/// pinned to `snapshot.revision`; before admission, loading cases from one
/// revision and then executing worktrees from another would produce mixed
/// evidence and must fail closed.
pub fn ensure_snapshot_unchanged(snapshot: &SourceSnapshot) -> Result<()> {
    ensure_source_clean(snapshot.repository_root())?;
    let current = git_revision(snapshot.repository_root())?;
    if current != snapshot.revision() {
        anyhow::bail!(
            "harness source repository {} changed during admission (captured {}, now {}); restart the run from one committed source snapshot",
            snapshot.repository_root().display(),
            snapshot.revision(),
            current
        );
    }
    Ok(())
}

fn git_revision(repository_root: &Path) -> Result<String> {
    let output = Command::new("git")
        .current_dir(repository_root)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .with_context(|| {
            format!(
                "resolve source revision for harness repository {}",
                repository_root.display()
            )
        })?;
    if !output.status.success() {
        anyhow::bail!(
            "could not resolve source revision in {}: {}",
            repository_root.display(),
            command_detail(&output.stderr, &output.stdout)
        );
    }
    let revision = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if revision.is_empty() || revision.chars().any(char::is_whitespace) {
        anyhow::bail!(
            "Git returned an invalid empty/whitespace source revision for {}",
            repository_root.display()
        );
    }
    Ok(revision)
}

/// A temporary detached checkout used by one live root execution job.
///
/// The checkout is registered with Git for the lifetime of this value and is
/// removed from the worktree registry before the temporary directory is
/// dropped. The source checkout is never changed.
pub struct IsolatedWorkspace {
    temp_root: TempDir,
    checkout: PathBuf,
    repository_root: PathBuf,
}

impl IsolatedWorkspace {
    /// Create a detached checkout from the run-start source snapshot.
    pub fn create(source: &SourceSnapshot) -> Result<Self> {
        // Re-check at the job boundary as well as at CLI setup. A developer
        // may edit the source while a long suite is running. A dirty source
        // is rejected, while a clean commit/checkout is harmless because the
        // worktree below is pinned to the immutable run-start revision.
        ensure_source_clean(source.repository_root())?;
        let temp_root = Builder::new()
            .prefix("astra-harness-workspace-")
            .tempdir()
            .context("create temporary harness workspace directory")?;
        let checkout = temp_root.path().join("source");

        let output = Command::new("git")
            .current_dir(source.repository_root())
            .args(["worktree", "add", "--detach"])
            .arg(&checkout)
            .arg(&source.revision)
            .output()
            .with_context(|| {
                format!(
                    "create isolated harness worktree from {}",
                    source.repository_root().display()
                )
            })?;
        if !output.status.success() {
            let detail = command_detail(&output.stderr, &output.stdout);
            anyhow::bail!(
                "could not create isolated harness worktree from {}: {}",
                source.repository_root().display(),
                detail
            );
        }

        Ok(Self {
            temp_root,
            checkout,
            repository_root: source.repository_root().to_path_buf(),
        })
    }

    /// Absolute path to the isolated checkout.
    pub fn path(&self) -> &Path {
        &self.checkout
    }
}

impl Drop for IsolatedWorkspace {
    fn drop(&mut self) {
        match Command::new("git")
            .current_dir(&self.repository_root)
            .args(["worktree", "remove", "--force"])
            .arg(&self.checkout)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => eprintln!(
                "[astra-test] WARNING: failed to remove isolated worktree {} (git status {}); run `git worktree prune` after confirming no harness process is using it",
                self.checkout.display(),
                status
            ),
            Err(error) => eprintln!(
                "[astra-test] WARNING: failed to remove isolated worktree {}: {error}; run `git worktree prune` after confirming no harness process is using it",
                self.checkout.display()
            ),
        }
        let _ = &self.temp_root;
    }
}

fn is_non_repository_diagnostic(detail: &str) -> bool {
    let lower = detail.to_ascii_lowercase();
    lower.contains("not a git repository")
        || lower.contains("outside a work tree")
        || lower.contains("must be run in a work tree")
}

fn command_detail(stderr: &[u8], stdout: &[u8]) -> String {
    let stderr = String::from_utf8_lossy(stderr).trim().to_string();
    if !stderr.is_empty() {
        return stderr;
    }
    let stdout = String::from_utf8_lossy(stdout).trim().to_string();
    if !stdout.is_empty() {
        return stdout;
    }
    "git exited without diagnostic output".to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        IsolatedWorkspace, SourceSnapshot, ensure_source_clean, repository_root_for_suite,
        source_snapshot_for_suite,
    };
    use std::path::Path;
    use std::process::Command;

    fn git(repo: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(repo)
            .args(args)
            .output()
            .expect("git should be installed");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn clean_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().expect("repo tempdir");
        git(repo.path(), &["init", "--quiet"]);
        git(
            repo.path(),
            &["config", "user.email", "harness@example.invalid"],
        );
        git(repo.path(), &["config", "user.name", "Astra Harness"]);
        std::fs::write(repo.path().join("README.md"), "fixture\n").expect("fixture");
        git(repo.path(), &["add", "README.md"]);
        git(repo.path(), &["commit", "--quiet", "-m", "fixture"]);
        repo
    }

    #[test]
    fn creates_and_removes_a_detached_worktree() {
        let repo = clean_repo();
        let source = repository_root_for_suite(repo.path())
            .expect("discover source")
            .expect("git repo gets isolated");
        ensure_source_clean(&source).expect("clean source");
        let snapshot = source_snapshot_for_suite(repo.path())
            .expect("snapshot source")
            .expect("git repo gets isolated");
        let workspace = IsolatedWorkspace::create(&snapshot).expect("create worktree");
        let checkout = workspace.path().to_path_buf();
        assert_eq!(
            std::fs::read_to_string(checkout.join("README.md")).expect("checkout fixture"),
            "fixture\n"
        );
        drop(workspace);
        assert!(!checkout.exists(), "checkout should be removed on drop");
        let worktrees = Command::new("git")
            .current_dir(repo.path())
            .args(["worktree", "list", "--porcelain"])
            .output()
            .expect("list worktrees");
        let listed = String::from_utf8_lossy(&worktrees.stdout);
        assert!(!listed.contains(checkout.to_string_lossy().as_ref()));
    }

    #[test]
    fn explicit_source_changes_fail_closed() {
        let repo = clean_repo();
        std::fs::write(repo.path().join("uncommitted.txt"), "dirty\n").expect("dirty fixture");
        let error = ensure_source_clean(repo.path()).expect_err("dirty source must fail");
        assert!(error.to_string().contains("uncommitted or untracked"));
    }

    #[test]
    fn non_git_suite_does_not_use_binary_or_process_cwd_repository() {
        let suite = tempfile::tempdir().expect("suite tempdir");
        let root = repository_root_for_suite(suite.path()).expect("inspect suite");
        assert!(root.is_none());
    }

    #[test]
    fn every_job_checkout_stays_on_the_run_start_revision() {
        let repo = clean_repo();
        let snapshot = source_snapshot_for_suite(repo.path())
            .expect("snapshot source")
            .expect("git repo gets isolated");
        let initial_revision = snapshot.revision().to_string();

        let first = IsolatedWorkspace::create(&snapshot).expect("first worktree");
        git(repo.path(), &["checkout", "--detach", "-q", "HEAD"]);
        std::fs::write(repo.path().join("next.txt"), "next\n").expect("next fixture");
        git(repo.path(), &["add", "next.txt"]);
        git(repo.path(), &["commit", "--quiet", "-m", "next"]);

        let second = IsolatedWorkspace::create(&snapshot).expect("second worktree");
        for checkout in [first.path(), second.path()] {
            let output = Command::new("git")
                .current_dir(checkout)
                .args(["rev-parse", "HEAD"])
                .output()
                .expect("resolve checkout revision");
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8_lossy(&output.stdout).trim(),
                initial_revision
            );
            assert!(!checkout.join("next.txt").exists());
        }
    }

    #[test]
    fn invalid_source_snapshot_fails_with_actionable_git_error() {
        let source_dir = tempfile::tempdir().expect("source tempdir");
        let source = SourceSnapshot {
            repository_root: source_dir.path().to_path_buf(),
            revision: "deadbeef".into(),
        };
        let error = match IsolatedWorkspace::create(&source) {
            Ok(_) => panic!("non-repository source must fail"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("could not inspect source changes")
        );
    }

    #[test]
    fn admission_rejects_a_clean_revision_change_after_snapshot_capture() {
        let repo = clean_repo();
        let snapshot = SourceSnapshot::capture(repo.path()).expect("capture source");
        std::fs::write(repo.path().join("during-admission.txt"), "changed\n")
            .expect("changed fixture");
        git(repo.path(), &["add", "during-admission.txt"]);
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "during admission"],
        );

        let error = super::ensure_snapshot_unchanged(&snapshot)
            .expect_err("clean revision movement must be rejected before admission");
        assert!(error.to_string().contains("changed during admission"));
        assert!(error.to_string().contains(snapshot.revision()));
    }
}
