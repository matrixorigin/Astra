//! Read-only Git worktree subjects under the canonical workspace observation lease.
//!
//! The executor lease precedes the private Git lock. Observation hashes tracked
//! changes and developer-owned untracked files without materializing a full diff.
use astra_services::work::WorkContentHash;
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::time::timeout;
const GIT_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);
const WORKSPACE_LEASE_WAIT: Duration = Duration::from_secs(120);
const HASH_BUFFER_BYTES: usize = 64 * 1024;
const REVISION_DOMAIN: &[u8] = b"astra.git-worktree.v1\0";

#[derive(Debug, Error)]
pub enum GitWorkspaceObservationError {
    #[error("workspace path is unavailable")]
    WorkspaceUnavailable,
    #[error("workspace is not a canonical Git worktree root")]
    NotWorktreeRoot,
    #[error("Git provider is unavailable")]
    ProviderUnavailable,
    #[error("Git provider operation timed out")]
    Timeout,
    #[error("Git provider rejected a read-only observation")]
    ObservationRejected,
    #[error("workspace Git lock was unavailable")]
    LockUnavailable,
    #[error("Git returned an unsafe worktree path")]
    UnsafePath,
    #[error("workspace observation I/O failed: {0}")]
    Io(#[from] std::io::Error),
}

/// Observe the committed, tracked-diff, and untracked-file state without
/// materializing repository history or loading a complete diff into memory.
/// Ignored files are intentionally outside the developer-work subject.
pub async fn observe_git_worktree_revision(
    workspace_root: &Path,
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    let _workspace_lease =
        crate::workspace_observation::acquire_workspace_observation_lease_with_options(
            workspace_root,
            None,
            WORKSPACE_LEASE_WAIT,
        )
        .await
        .map_err(|_| GitWorkspaceObservationError::WorkspaceUnavailable)?;
    timeout(
        GIT_OPERATION_TIMEOUT,
        observe_git_worktree_revision_serialized(workspace_root),
    )
    .await
    .map_err(|_| GitWorkspaceObservationError::Timeout)?
}

async fn observe_git_worktree_revision_serialized(
    workspace_root: &Path,
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    let root = canonical_git_worktree_root(workspace_root).await?;
    let _workspace_lock = acquire_workspace_lock(&root).await?;
    observe_git_worktree_revision_locked(&root).await
}

async fn canonical_git_worktree_root(
    workspace_root: &Path,
) -> Result<PathBuf, GitWorkspaceObservationError> {
    let root = workspace_root
        .canonicalize()
        .map_err(|_| GitWorkspaceObservationError::WorkspaceUnavailable)?;
    if !root.is_dir() {
        return Err(GitWorkspaceObservationError::WorkspaceUnavailable);
    }
    let discovered_root = git_small_output(&root, &["rev-parse", "--show-toplevel"]).await?;
    let discovered_root = PathBuf::from(
        std::str::from_utf8(trim_ascii(&discovered_root))
            .map_err(|_| GitWorkspaceObservationError::NotWorktreeRoot)?,
    )
    .canonicalize()
    .map_err(|_| GitWorkspaceObservationError::NotWorktreeRoot)?;
    if discovered_root != root {
        return Err(GitWorkspaceObservationError::NotWorktreeRoot);
    }
    Ok(root)
}

async fn observe_git_worktree_revision_locked(
    root: &Path,
) -> Result<WorkContentHash, GitWorkspaceObservationError> {
    let mut hasher = Sha256::new();
    hasher.update(REVISION_DOMAIN);
    let head = git_small_output(root, &["rev-parse", "HEAD"]).await?;
    hash_field(&mut hasher, b"head", trim_ascii(&head));
    hash_git_diff(root, &mut hasher).await?;
    hash_untracked_files(root, &mut hasher).await?;
    WorkContentHash::parse(format!("sha256:{:x}", hasher.finalize()))
        .map_err(|_| GitWorkspaceObservationError::ObservationRejected)
}

struct WorkspaceLock(std::fs::File);

impl Drop for WorkspaceLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.0);
    }
}

async fn acquire_workspace_lock(
    root: &Path,
) -> Result<WorkspaceLock, GitWorkspaceObservationError> {
    let encoded_path =
        git_small_output(root, &["rev-parse", "--git-path", "astra-workspace.lock"]).await?;
    let lock_path = PathBuf::from(
        std::str::from_utf8(trim_ascii(&encoded_path))
            .map_err(|_| GitWorkspaceObservationError::UnsafePath)?,
    );
    let lock_path = if lock_path.is_absolute() {
        lock_path
    } else {
        root.join(lock_path)
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)?;
    let deadline = tokio::time::Instant::now() + GIT_OPERATION_TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(WorkspaceLock(file)),
            Err(error) if error.kind() == fs2::lock_contended_error().kind() => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(GitWorkspaceObservationError::LockUnavailable);
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => return Err(GitWorkspaceObservationError::Io(error)),
        }
    }
}

async fn git_small_output(
    root: &Path,
    args: &[&str],
) -> Result<Vec<u8>, GitWorkspaceObservationError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                GitWorkspaceObservationError::ProviderUnavailable
            } else {
                GitWorkspaceObservationError::Io(error)
            }
        })?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(GitWorkspaceObservationError::ObservationRejected)
    }
}

async fn hash_git_diff(
    root: &Path,
    hasher: &mut Sha256,
) -> Result<(), GitWorkspaceObservationError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--binary",
            "--full-index",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            "--diff-algorithm=myers",
            "--ignore-submodules=none",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            "HEAD",
            "--",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_spawn_error)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or(GitWorkspaceObservationError::ObservationRejected)?;
    let digest = digest_reader(&mut stdout).await?;
    hash_field(hasher, b"diff", &digest);
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(GitWorkspaceObservationError::ObservationRejected)
    }
}

async fn hash_untracked_files(
    root: &Path,
    hasher: &mut Sha256,
) -> Result<(), GitWorkspaceObservationError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "ls-files",
            "--others",
            "--exclude-standard",
            "--deduplicate",
            "-z",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(map_spawn_error)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(GitWorkspaceObservationError::ObservationRejected)?;
    let mut paths = BufReader::new(stdout);
    let mut encoded_path = Vec::new();
    loop {
        encoded_path.clear();
        let bytes = paths.read_until(0, &mut encoded_path).await?;
        if bytes == 0 {
            break;
        }
        if encoded_path.last() != Some(&0) {
            return Err(GitWorkspaceObservationError::ObservationRejected);
        }
        encoded_path.pop();
        let relative = path_from_git_bytes(&encoded_path)?;
        if !safe_relative_path(&relative) {
            return Err(GitWorkspaceObservationError::UnsafePath);
        }
        hash_field(hasher, b"untracked-path", &encoded_path);
        let absolute = root.join(relative);
        let metadata = tokio::fs::symlink_metadata(&absolute).await?;
        if metadata.file_type().is_symlink() {
            let target = tokio::fs::read_link(&absolute).await?;
            hash_field(hasher, b"symlink", target.as_os_str().as_encoded_bytes());
        } else if metadata.is_file() {
            hash_regular_file_mode(hasher, &metadata);
            let mut file = tokio::fs::File::open(&absolute).await?;
            let digest = digest_reader(&mut file).await?;
            hash_field(hasher, b"file-content", &digest);
        } else {
            return Err(GitWorkspaceObservationError::UnsafePath);
        }
    }
    let status = child.wait().await?;
    if status.success() {
        Ok(())
    } else {
        Err(GitWorkspaceObservationError::ObservationRejected)
    }
}

async fn digest_reader(reader: &mut (impl AsyncRead + Unpin)) -> Result<[u8; 32], std::io::Error> {
    let mut hasher = Sha256::new();
    let mut buffer = vec![0; HASH_BUFFER_BYTES];
    loop {
        let bytes = reader.read(&mut buffer).await?;
        if bytes == 0 {
            return Ok(hasher.finalize().into());
        }
        hasher.update(&buffer[..bytes]);
    }
}

#[cfg(unix)]
fn hash_regular_file_mode(hasher: &mut Sha256, metadata: &std::fs::Metadata) {
    use std::os::unix::fs::PermissionsExt;
    let mode = if metadata.permissions().mode() & 0o111 == 0 {
        b"100644".as_slice()
    } else {
        b"100755".as_slice()
    };
    hash_field(hasher, b"file-mode", mode);
}

#[cfg(not(unix))]
fn hash_regular_file_mode(hasher: &mut Sha256, _metadata: &std::fs::Metadata) {
    hash_field(hasher, b"file-mode", b"100644");
}

fn hash_field(hasher: &mut Sha256, name: &[u8], value: &[u8]) {
    hasher.update(name);
    hasher.update([0]);
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value);
}

fn trim_ascii(value: &[u8]) -> &[u8] {
    value
        .strip_suffix(b"\n")
        .and_then(|value| value.strip_suffix(b"\r").or(Some(value)))
        .unwrap_or(value)
}

fn safe_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

#[cfg(unix)]
fn path_from_git_bytes(value: &[u8]) -> Result<PathBuf, GitWorkspaceObservationError> {
    use std::os::unix::ffi::OsStringExt;
    Ok(PathBuf::from(std::ffi::OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(value: &[u8]) -> Result<PathBuf, GitWorkspaceObservationError> {
    String::from_utf8(value.to_vec())
        .map(PathBuf::from)
        .map_err(|_| GitWorkspaceObservationError::UnsafePath)
}

fn map_spawn_error(error: std::io::Error) -> GitWorkspaceObservationError {
    if error.kind() == std::io::ErrorKind::NotFound {
        GitWorkspaceObservationError::ProviderUnavailable
    } else {
        GitWorkspaceObservationError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    fn git(root: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Astra Test")
            .env("GIT_AUTHOR_EMAIL", "astra@example.invalid")
            .env("GIT_COMMITTER_NAME", "Astra Test")
            .env("GIT_COMMITTER_EMAIL", "astra@example.invalid")
            .status()
            .expect("start git fixture command");
        assert!(status.success(), "git fixture command failed: {args:?}");
    }
    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temp repository");
        git(directory.path(), &["init", "--quiet"]);
        fs::write(directory.path().join("file.txt"), "before\n").expect("seed file");
        git(directory.path(), &["add", "file.txt"]);
        git(directory.path(), &["commit", "--quiet", "-m", "initial"]);
        directory
    }

    #[tokio::test]
    async fn nested_directory_is_not_silently_promoted_to_repository_root() {
        let repository = repository();
        let nested = repository.path().join("nested");
        fs::create_dir(&nested).expect("nested directory");
        assert!(matches!(
            observe_git_worktree_revision(&nested).await,
            Err(GitWorkspaceObservationError::NotWorktreeRoot)
        ));
    }

    #[tokio::test]
    async fn observation_waits_for_the_workspace_mutation_boundary() {
        let repository = repository();
        let root = canonical_git_worktree_root(repository.path())
            .await
            .expect("canonical worktree");
        let mutation_lock = acquire_workspace_lock(&root)
            .await
            .expect("hold materialization boundary");
        let observed_root = root.clone();
        let mut observation =
            tokio::spawn(async move { observe_git_worktree_revision(&observed_root).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut observation)
                .await
                .is_err(),
            "reconciliation must not observe a worktree while mutation owns its boundary"
        );
        drop(mutation_lock);
        observation
            .await
            .expect("observation task")
            .expect("observation after mutation boundary");
    }

    #[tokio::test]
    async fn subject_tracks_developer_files_but_excludes_ignored_outputs() {
        let repository = repository();
        fs::write(repository.path().join(".gitignore"), "build/\n").expect("gitignore");
        git(repository.path(), &["add", ".gitignore"]);
        git(
            repository.path(),
            &["commit", "--quiet", "-m", "ignore outputs"],
        );
        let base = observe_git_worktree_revision(repository.path())
            .await
            .expect("observe base");
        git(
            repository.path(),
            &["config", "diff.algorithm", "histogram"],
        );
        git(repository.path(), &["config", "color.ui", "always"]);
        assert_eq!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe with hostile display config"),
            base,
            "local display/diff preferences must not change canonical identity"
        );

        fs::write(repository.path().join("file.txt"), "tracked change\n").expect("tracked edit");
        assert_ne!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe tracked edit"),
            base
        );
        fs::write(repository.path().join("file.txt"), "before\n").expect("restore tracked file");
        fs::write(repository.path().join("new.txt"), "untracked\n").expect("untracked file");
        assert_ne!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe untracked file"),
            base
        );
        fs::remove_file(repository.path().join("new.txt")).expect("remove untracked file");
        fs::create_dir(repository.path().join("build")).expect("ignored directory");
        fs::write(repository.path().join("build/output.bin"), b"ignored").expect("ignored output");
        assert_eq!(
            observe_git_worktree_revision(repository.path())
                .await
                .expect("observe ignored output"),
            base
        );
    }
}
