//! Capture and materialize portable workspace snapshots.
//!
//! The manifest in [`crate::WorkspaceSnapshotManifestV1`] is intentionally a
//! pure contract.  This module is the provider-side implementation for the
//! first supported boundary: one Git worktree whose file contents can be
//! read by the caller.  It never follows a directory symlink, never reads
//! `.git` or Astra private state, and refuses to install into an existing
//! destination.
//!
//! The returned package is an in-memory representation so callers can choose
//! their durable bytes store (database, object storage, or an Edge upload).
//! Production callers should stream each blob to that store as soon as it is
//! produced; the package form keeps the provider contract deterministic and
//! makes the integrity and materialization rules directly testable.

use crate::{
    WORKSPACE_SNAPSHOT_MANIFEST_SCHEMA_VERSION, WorkspaceSnapshotCaptureV1,
    WorkspaceSnapshotChangeV1, WorkspaceSnapshotContentV1, WorkspaceSnapshotEntryKindV1,
    WorkspaceSnapshotEntryV1, WorkspaceSnapshotExclusionV1, WorkspaceSnapshotManifestV1,
    WorkspaceSnapshotRepositoryV1,
};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use thiserror::Error;
use uuid::Uuid;

#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::io::{FromRawFd, RawFd};

/// Capture options for a local Git worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshotCaptureOptions {
    pub snapshot_id: String,
    pub logical_workspace_id: String,
    /// Include untracked, non-ignored files only when the user explicitly
    /// chose to carry them.  Tracked files are always captured, including
    /// tracked files with local modifications.
    pub include_untracked: bool,
}

impl WorkspaceSnapshotCaptureOptions {
    pub fn new(snapshot_id: impl Into<String>, logical_workspace_id: impl Into<String>) -> Self {
        Self {
            snapshot_id: snapshot_id.into(),
            logical_workspace_id: logical_workspace_id.into(),
            include_untracked: false,
        }
    }
}

/// A verified package of manifest metadata and content-addressed file blobs.
/// The manifest is not considered complete until [`Self::verify`] succeeds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshotPackage {
    pub manifest: WorkspaceSnapshotManifestV1,
    pub blobs: BTreeMap<String, Vec<u8>>,
}

impl WorkspaceSnapshotPackage {
    pub fn verify(&self) -> Result<(), WorkspaceSnapshotCaptureError> {
        self.manifest
            .validate()
            .map_err(WorkspaceSnapshotCaptureError::InvalidManifest)?;
        for entry in &self.manifest.entries {
            if is_excluded_path(&entry.path) {
                return Err(WorkspaceSnapshotCaptureError::UnsafePath(
                    entry.path.clone(),
                ));
            }
            if entry.change == WorkspaceSnapshotChangeV1::Deleted
                || entry.kind != WorkspaceSnapshotEntryKindV1::File
            {
                continue;
            }
            let blob_ref = entry
                .blob_ref
                .as_deref()
                .ok_or_else(|| WorkspaceSnapshotCaptureError::MissingBlob(entry.path.clone()))?;
            let bytes = self
                .blobs
                .get(blob_ref)
                .ok_or_else(|| WorkspaceSnapshotCaptureError::MissingBlob(entry.path.clone()))?;
            let digest = content_digest(bytes);
            if entry.digest.as_deref() != Some(digest.as_str()) {
                return Err(WorkspaceSnapshotCaptureError::BlobDigestMismatch {
                    path: entry.path.clone(),
                });
            }
            if entry.size != bytes.len() as u64 {
                return Err(WorkspaceSnapshotCaptureError::BlobSizeMismatch {
                    path: entry.path.clone(),
                });
            }
        }
        let referenced = self
            .manifest
            .entries
            .iter()
            .filter_map(|entry| entry.blob_ref.as_deref())
            .collect::<BTreeSet<_>>();
        if let Some(unexpected) = self
            .blobs
            .keys()
            .find(|blob_ref| !referenced.contains(blob_ref.as_str()))
        {
            return Err(WorkspaceSnapshotCaptureError::UnexpectedBlob(
                unexpected.clone(),
            ));
        }
        validate_materialization_layout(self)?;
        Ok(())
    }
}

/// Evidence returned after a package has been installed atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshotMaterializationReceipt {
    pub target: PathBuf,
    pub snapshot_id: String,
    pub manifest_hash: String,
    pub content_root: String,
    pub file_count: u64,
    pub byte_size: u64,
}

#[derive(Debug, Error)]
pub enum WorkspaceSnapshotCaptureError {
    #[error("workspace root must be an absolute directory: {0}")]
    InvalidRoot(PathBuf),
    #[error("workspace is not a Git worktree: {0}")]
    NotGitWorktree(String),
    #[error("Git command {command:?} failed: {message}")]
    GitCommand { command: String, message: String },
    #[error("workspace path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("workspace path is unsafe: {0}")]
    UnsafePath(String),
    #[error("workspace path is not supported: {path}: {reason}")]
    UnsupportedPath { path: String, reason: String },
    #[error("workspace changed while it was being captured")]
    ConcurrentMutation,
    #[error("could not read workspace path {path}: {source}")]
    ReadPath { path: String, source: io::Error },
    #[error("could not write snapshot path {path}: {source}")]
    WritePath { path: String, source: io::Error },
    #[error("snapshot destination already exists: {0}")]
    DestinationExists(PathBuf),
    #[error("snapshot destination has no usable parent: {0}")]
    DestinationParentMissing(PathBuf),
    #[error("snapshot installation failed: {0}")]
    Install(String),
    #[error("snapshot manifest is invalid: {0}")]
    InvalidManifest(#[source] crate::WorkspaceSnapshotValidationError),
    #[error("snapshot is missing the blob for {0}")]
    MissingBlob(String),
    #[error("snapshot blob digest does not match {path}")]
    BlobDigestMismatch { path: String },
    #[error("snapshot blob size does not match {path}")]
    BlobSizeMismatch { path: String },
    #[error("snapshot contains a duplicate blob reference with different bytes: {0}")]
    DuplicateBlob(String),
    #[error("snapshot contains an unreferenced content blob: {0}")]
    UnexpectedBlob(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct GitStatusEntry {
    path: String,
    change: WorkspaceSnapshotChangeV1,
    renamed_from: Option<String>,
}

/// Capture one local Git worktree into a verified package.
pub fn capture_git_worktree(
    root: &Path,
    options: &WorkspaceSnapshotCaptureOptions,
) -> Result<WorkspaceSnapshotPackage, WorkspaceSnapshotCaptureError> {
    let root = canonical_root(root)?;
    let git_root = git_output(&root, &["rev-parse", "--show-toplevel"])?;
    let git_root = canonical_root(Path::new(git_root.trim()))?;
    if git_root != root {
        return Err(WorkspaceSnapshotCaptureError::InvalidRoot(root));
    }

    let workspace_root = workspace_root_handle(&root)?;
    ensure_workspace_root_binding(&root, &workspace_root)?;
    let repository_id = repository_identity(&root)?;
    let (base_commit, base_tree) = git_basis(&root)?;
    let status = git_status(&root)?;
    let paths = git_workspace_paths(&root, options.include_untracked)?;
    ensure_workspace_root_binding(&root, &workspace_root)?;

    let before = capture_entries(&root, &paths, &status, &workspace_root)?;
    let fingerprint_before = fingerprint_entries(&before);
    let mut blobs = BTreeMap::new();
    let mut entries = Vec::with_capacity(before.len() + status.len());
    for captured in &before {
        let entry = captured.entry.clone();
        if let (Some(blob_ref), Some(bytes)) = (entry.blob_ref.as_ref(), captured.bytes.as_ref()) {
            if let Some(existing) = blobs.get(blob_ref) {
                if existing != bytes {
                    return Err(WorkspaceSnapshotCaptureError::DuplicateBlob(
                        blob_ref.clone(),
                    ));
                }
            } else {
                blobs.insert(blob_ref.clone(), bytes.clone());
            }
        }
        entries.push(entry);
    }

    // A deleted tracked path is not present in the current filesystem and
    // therefore needs an explicit tombstone in the manifest.
    let captured_paths = before
        .iter()
        .map(|captured| captured.entry.path.as_str())
        .collect::<BTreeSet<_>>();
    for item in status.values() {
        if item.change == WorkspaceSnapshotChangeV1::Deleted
            && !captured_paths.contains(item.path.as_str())
        {
            entries.push(WorkspaceSnapshotEntryV1 {
                path: item.path.clone(),
                kind: WorkspaceSnapshotEntryKindV1::File,
                change: WorkspaceSnapshotChangeV1::Deleted,
                mode: 0,
                size: 0,
                digest: None,
                blob_ref: None,
                symlink_target: None,
                renamed_from: item.renamed_from.clone(),
            });
        }
    }
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries.dedup_by(|left, right| left.path == right.path);

    let after_status = git_status(&root)?;
    let after_all_paths = git_workspace_paths(&root, options.include_untracked)?;
    let after = capture_entries(&root, &after_all_paths, &after_status, &workspace_root)?;
    let after_basis = git_basis(&root)?;
    ensure_workspace_root_binding(&root, &workspace_root)?;
    if fingerprint_before != fingerprint_entries(&after)
        || status != after_status
        || paths != after_all_paths
        || (base_commit.as_deref(), base_tree.as_deref())
            != (after_basis.0.as_deref(), after_basis.1.as_deref())
    {
        return Err(WorkspaceSnapshotCaptureError::ConcurrentMutation);
    }
    let total_bytes = entries
        .iter()
        .filter(|entry| {
            entry.kind == WorkspaceSnapshotEntryKindV1::File
                && entry.change != WorkspaceSnapshotChangeV1::Deleted
        })
        .map(|entry| entry.size)
        .sum();

    let mut manifest = WorkspaceSnapshotManifestV1 {
        schema_version: WORKSPACE_SNAPSHOT_MANIFEST_SCHEMA_VERSION,
        snapshot_id: options.snapshot_id.clone(),
        logical_workspace_id: options.logical_workspace_id.clone(),
        repository: WorkspaceSnapshotRepositoryV1 {
            repository_id,
            base_commit,
            base_tree,
            submodules: Vec::new(),
        },
        capture: WorkspaceSnapshotCaptureV1 {
            fingerprint_before: fingerprint_before.clone(),
            fingerprint_after: fingerprint_before,
            captured_at: Utc::now().to_rfc3339(),
            consistent: true,
        },
        entries,
        exclusions: vec![
            WorkspaceSnapshotExclusionV1 {
                pattern: ".astra/".to_string(),
                reason: "Astra local state and credentials are never copied".to_string(),
            },
            WorkspaceSnapshotExclusionV1 {
                pattern: ".git/".to_string(),
                reason: "repository internals are reconstructed on the target".to_string(),
            },
        ],
        data_sources: Vec::new(),
        content: WorkspaceSnapshotContentV1 {
            content_root: String::new(),
            total_bytes,
            blob_count: blobs.len() as u64,
            pack_ref: Some(format!("workspace-snapshot-pack:{}", options.snapshot_id)),
        },
    };
    manifest.content.content_root = manifest
        .computed_content_root()
        .map_err(WorkspaceSnapshotCaptureError::InvalidManifest)?;
    let package = WorkspaceSnapshotPackage { manifest, blobs };
    package.verify()?;
    Ok(package)
}

/// Install a verified snapshot into a new directory.
pub fn materialize_workspace_snapshot(
    package: &WorkspaceSnapshotPackage,
    target: &Path,
) -> Result<WorkspaceSnapshotMaterializationReceipt, WorkspaceSnapshotCaptureError> {
    package.verify()?;
    if !target.is_absolute() {
        return Err(WorkspaceSnapshotCaptureError::InvalidRoot(
            target.to_path_buf(),
        ));
    }
    let parent = target
        .parent()
        .filter(|parent| parent.is_dir())
        .ok_or_else(|| {
            WorkspaceSnapshotCaptureError::DestinationParentMissing(target.to_path_buf())
        })?
        .canonicalize()
        .map_err(|source| WorkspaceSnapshotCaptureError::WritePath {
            path: target.display().to_string(),
            source,
        })?;
    let target_name = target
        .file_name()
        .ok_or_else(|| WorkspaceSnapshotCaptureError::InvalidRoot(target.to_path_buf()))?;
    // Resolve the destination parent once and use that fixed path for every
    // subsequent check and the atomic install.  Keeping the original alias
    // here would allow a symlinked parent to be retargeted while the staging
    // tree is being built, installing the snapshot somewhere other than the
    // parent we validated.
    let target = parent.join(target_name);
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            return Err(WorkspaceSnapshotCaptureError::DestinationExists(target));
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            return Err(WorkspaceSnapshotCaptureError::WritePath {
                path: target.display().to_string(),
                source,
            });
        }
    }
    let staging = parent.join(format!(
        ".{}.astra-snapshot-{}",
        target_name.to_string_lossy(),
        Uuid::now_v7().simple()
    ));
    fs::create_dir(&staging).map_err(|source| WorkspaceSnapshotCaptureError::WritePath {
        path: staging.display().to_string(),
        source,
    })?;

    let install_result = install_entries(package, &staging);
    if let Err(error) = install_result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    match fs::symlink_metadata(&target) {
        Ok(_) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(WorkspaceSnapshotCaptureError::DestinationExists(
                target.to_path_buf(),
            ));
        }
        Err(source) if source.kind() == io::ErrorKind::NotFound => {}
        Err(source) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(WorkspaceSnapshotCaptureError::WritePath {
                path: target.display().to_string(),
                source,
            });
        }
    }
    rename_noreplace(&staging, &target).map_err(|source| {
        let _ = fs::remove_dir_all(&staging);
        if source.kind() == io::ErrorKind::AlreadyExists {
            WorkspaceSnapshotCaptureError::DestinationExists(target.to_path_buf())
        } else {
            WorkspaceSnapshotCaptureError::Install(format!(
                "rename '{}' to '{}': {source}",
                staging.display(),
                target.display()
            ))
        }
    })?;
    let manifest_hash = package
        .manifest
        .content_hash()
        .map_err(WorkspaceSnapshotCaptureError::InvalidManifest)?;
    Ok(WorkspaceSnapshotMaterializationReceipt {
        target: target.to_path_buf(),
        snapshot_id: package.manifest.snapshot_id.clone(),
        manifest_hash,
        content_root: package.manifest.content.content_root.clone(),
        file_count: package
            .manifest
            .entries
            .iter()
            .filter(|entry| entry.kind == WorkspaceSnapshotEntryKindV1::File)
            .filter(|entry| entry.change != WorkspaceSnapshotChangeV1::Deleted)
            .count() as u64,
        byte_size: package.manifest.content.total_bytes,
    })
}

#[derive(Debug)]
struct CapturedEntry {
    entry: WorkspaceSnapshotEntryV1,
    bytes: Option<Vec<u8>>,
}

enum WorkspacePathContent {
    File { mode: u32, bytes: Vec<u8> },
    Symlink { mode: u32, target: PathBuf },
    Directory,
    Special,
}

fn canonical_root(root: &Path) -> Result<PathBuf, WorkspaceSnapshotCaptureError> {
    if !root.is_absolute() || !root.is_dir() {
        return Err(WorkspaceSnapshotCaptureError::InvalidRoot(
            root.to_path_buf(),
        ));
    }
    root.canonicalize()
        .map_err(|source| WorkspaceSnapshotCaptureError::ReadPath {
            path: root.display().to_string(),
            source,
        })
}

fn repository_identity(root: &Path) -> Result<String, WorkspaceSnapshotCaptureError> {
    let remote = git_output_optional(root, &["config", "--get", "remote.origin.url"])?
        .filter(|remote| !remote.trim().is_empty());
    let identity = remote.unwrap_or_else(|| {
        format!(
            "{}:{}",
            root.display(),
            git_output_optional(root, &["rev-parse", "--git-common-dir"])
                .ok()
                .flatten()
                .unwrap_or_default()
        )
    });
    let mut digest = Sha256::new();
    digest.update(b"astra.workspace-repository.v1\0");
    digest.update(identity.as_bytes());
    Ok(format!("repo-{:x}", digest.finalize()))
}

fn git_basis(
    root: &Path,
) -> Result<(Option<String>, Option<String>), WorkspaceSnapshotCaptureError> {
    let commit = git_output_optional(root, &["rev-parse", "HEAD"])?;
    let tree = match commit.as_deref() {
        Some(commit) => {
            let revision = format!("{commit}^{{tree}}");
            git_output_optional(root, &["rev-parse", &revision])?
        }
        None => None,
    };
    Ok((commit, tree))
}

fn git_output(root: &Path, args: &[&str]) -> Result<String, WorkspaceSnapshotCaptureError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| WorkspaceSnapshotCaptureError::GitCommand {
            command: args.join(" "),
            message: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(WorkspaceSnapshotCaptureError::NotGitWorktree(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    String::from_utf8(output.stdout).map_err(|_| WorkspaceSnapshotCaptureError::GitCommand {
        command: args.join(" "),
        message: "Git returned non-UTF-8 output".to_string(),
    })
}

fn is_not_found(error: &WorkspaceSnapshotCaptureError) -> bool {
    matches!(
        error,
        WorkspaceSnapshotCaptureError::ReadPath { source, .. }
            if source.kind() == io::ErrorKind::NotFound
    )
}

#[cfg(unix)]
fn capture_workspace_path(
    _root: &Path,
    relative: &str,
    workspace_root: &WorkspaceRoot,
) -> Result<WorkspacePathContent, WorkspaceSnapshotCaptureError> {
    let (parent, leaf) = open_workspace_parent(workspace_root, relative)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return Err(read_path_error(relative, io::Error::last_os_error()));
    }
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    let mode = mode_bits(stat.st_mode);
    if file_type == libc::S_IFLNK {
        let target = read_link_at(parent.as_raw_fd(), &leaf)
            .map_err(|source| read_path_error(relative, source))?;
        return Ok(WorkspacePathContent::Symlink { mode, target });
    }
    if file_type == libc::S_IFREG {
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(read_path_error(relative, io::Error::last_os_error()));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        let mut opened_stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        if unsafe { libc::fstat(fd, opened_stat.as_mut_ptr()) } != 0 {
            return Err(read_path_error(relative, io::Error::last_os_error()));
        }
        let opened_stat = unsafe { opened_stat.assume_init() };
        if opened_stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
                path: relative.to_string(),
                reason: "workspace path changed to a non-regular file".to_string(),
            });
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|source| read_path_error(relative, source))?;
        return Ok(WorkspacePathContent::File {
            mode: mode_bits(opened_stat.st_mode),
            bytes,
        });
    }
    if file_type == libc::S_IFDIR {
        Ok(WorkspacePathContent::Directory)
    } else {
        Ok(WorkspacePathContent::Special)
    }
}

#[cfg(not(unix))]
fn capture_workspace_path(
    _root: &Path,
    relative: &str,
    _workspace_root: &WorkspaceRoot,
) -> Result<WorkspacePathContent, WorkspaceSnapshotCaptureError> {
    Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
        path: relative.to_string(),
        reason: "secure workspace capture is unsupported on this platform".to_string(),
    })
}

fn read_path_error(path: &str, source: io::Error) -> WorkspaceSnapshotCaptureError {
    WorkspaceSnapshotCaptureError::ReadPath {
        path: path.to_string(),
        source,
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn mode_bits(mode: libc::mode_t) -> u32 {
    (mode as u32) & 0o7777
}

#[cfg(unix)]
struct OwnedFd(RawFd);

#[cfg(unix)]
type WorkspaceRoot = OwnedFd;

#[cfg(not(unix))]
#[derive(Debug, Clone, Copy)]
struct WorkspaceRoot;

#[cfg(unix)]
impl OwnedFd {
    fn as_raw_fd(&self) -> RawFd {
        self.0
    }
}

#[cfg(unix)]
impl Drop for OwnedFd {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.0);
        }
    }
}

#[cfg(unix)]
fn workspace_root_handle(root: &Path) -> Result<WorkspaceRoot, WorkspaceSnapshotCaptureError> {
    open_workspace_root(root, &root.display().to_string())
}

#[cfg(not(unix))]
fn workspace_root_handle(_root: &Path) -> Result<WorkspaceRoot, WorkspaceSnapshotCaptureError> {
    Ok(WorkspaceRoot)
}

#[cfg(unix)]
fn ensure_workspace_root_binding(
    root: &Path,
    workspace_root: &WorkspaceRoot,
) -> Result<(), WorkspaceSnapshotCaptureError> {
    let descriptor_identity = directory_identity(workspace_root.as_raw_fd())
        .map_err(|source| read_path_error(&root.display().to_string(), source))?;
    let path_metadata = fs::metadata(root)
        .map_err(|source| read_path_error(&root.display().to_string(), source))?;
    if descriptor_identity != (path_metadata.dev(), path_metadata.ino()) {
        return Err(WorkspaceSnapshotCaptureError::ConcurrentMutation);
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_workspace_root_binding(
    _root: &Path,
    _workspace_root: &WorkspaceRoot,
) -> Result<(), WorkspaceSnapshotCaptureError> {
    Ok(())
}

#[cfg(unix)]
#[allow(clippy::unnecessary_cast)]
fn directory_identity(fd: RawFd) -> io::Result<(u64, u64)> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let stat = unsafe { stat.assume_init() };
    Ok((stat.st_dev as u64, stat.st_ino as u64))
}

#[cfg(unix)]
fn open_workspace_parent(
    root: &WorkspaceRoot,
    relative: &str,
) -> Result<(OwnedFd, std::ffi::CString), WorkspaceSnapshotCaptureError> {
    validate_relative_path(relative)?;
    let components = relative.split('/').collect::<Vec<_>>();
    let fd = unsafe { libc::fcntl(root.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(read_path_error(relative, io::Error::last_os_error()));
    }
    let mut parent = OwnedFd(fd);
    for component in &components[..components.len().saturating_sub(1)] {
        let name = std::ffi::CString::new(component.as_bytes()).map_err(|_| {
            read_path_error(
                relative,
                io::Error::new(io::ErrorKind::InvalidInput, "workspace path contains NUL"),
            )
        })?;
        let child = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if child < 0 {
            let source = io::Error::last_os_error();
            if matches!(source.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
                    path: relative.to_string(),
                    reason: "an ancestor path is not a directory or is a symlink".to_string(),
                });
            }
            return Err(read_path_error(relative, source));
        }
        parent = OwnedFd(child);
    }
    let leaf =
        std::ffi::CString::new(components[components.len() - 1].as_bytes()).map_err(|_| {
            read_path_error(
                relative,
                io::Error::new(io::ErrorKind::InvalidInput, "workspace path contains NUL"),
            )
        })?;
    Ok((parent, leaf))
}

#[cfg(unix)]
fn open_workspace_root(
    root: &Path,
    relative: &str,
) -> Result<OwnedFd, WorkspaceSnapshotCaptureError> {
    let root_fd = std::ffi::CString::new("/").expect("literal has no NUL");
    let fd = unsafe {
        libc::open(
            root_fd.as_ptr(),
            libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY,
        )
    };
    if fd < 0 {
        return Err(read_path_error(relative, io::Error::last_os_error()));
    }
    let mut current = OwnedFd(fd);
    for component in root.as_os_str().as_bytes().split(|byte| *byte == b'/') {
        if component.is_empty() {
            continue;
        }
        let name = std::ffi::CString::new(component).map_err(|_| {
            read_path_error(
                relative,
                io::Error::new(io::ErrorKind::InvalidInput, "workspace root contains NUL"),
            )
        })?;
        let child = unsafe {
            libc::openat(
                current.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            )
        };
        if child < 0 {
            let source = io::Error::last_os_error();
            if matches!(source.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
                    path: root.display().to_string(),
                    reason: "workspace root contains a symlink or non-directory ancestor"
                        .to_string(),
                });
            }
            return Err(read_path_error(relative, source));
        }
        current = OwnedFd(child);
    }
    Ok(current)
}

#[cfg(unix)]
fn read_link_at(parent: RawFd, leaf: &std::ffi::CStr) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u8; 4096];
    loop {
        let length = unsafe {
            libc::readlinkat(
                parent,
                leaf.as_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
            )
        };
        if length < 0 {
            return Err(io::Error::last_os_error());
        }
        let length = length as usize;
        if length < buffer.len() {
            buffer.truncate(length);
            return Ok(PathBuf::from(std::ffi::OsString::from_vec(buffer)));
        }
        if buffer.len() >= 1 << 20 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink target exceeds the supported limit",
            ));
        }
        buffer.resize(buffer.len() * 2, 0);
    }
}

fn git_output_optional(
    root: &Path,
    args: &[&str],
) -> Result<Option<String>, WorkspaceSnapshotCaptureError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| WorkspaceSnapshotCaptureError::GitCommand {
            command: args.join(" "),
            message: source.to_string(),
        })?;
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8(output.stdout).map_err(|_| {
        WorkspaceSnapshotCaptureError::GitCommand {
            command: args.join(" "),
            message: "Git returned non-UTF-8 output".to_string(),
        }
    })?;
    Ok((!value.trim().is_empty()).then(|| value.trim().to_string()))
}

fn git_paths(root: &Path, args: &[&str]) -> Result<Vec<String>, WorkspaceSnapshotCaptureError> {
    let output = git_output_bytes(root, args)?;
    output
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| {
            String::from_utf8(path.to_vec())
                .map_err(|_| WorkspaceSnapshotCaptureError::NonUtf8Path(root.to_path_buf()))
        })
        .collect()
}

fn git_workspace_paths(
    root: &Path,
    include_untracked: bool,
) -> Result<BTreeSet<String>, WorkspaceSnapshotCaptureError> {
    let mut paths = git_paths(root, &["ls-files", "-z"])?
        .into_iter()
        .filter(|path| !is_excluded_path(path))
        .collect::<BTreeSet<_>>();
    if include_untracked {
        paths.extend(
            git_paths(root, &["ls-files", "--others", "--exclude-standard", "-z"])?
                .into_iter()
                .filter(|path| !is_excluded_path(path)),
        );
    }
    Ok(paths)
}

fn git_output_bytes(root: &Path, args: &[&str]) -> Result<Vec<u8>, WorkspaceSnapshotCaptureError> {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .map_err(|source| WorkspaceSnapshotCaptureError::GitCommand {
            command: args.join(" "),
            message: source.to_string(),
        })?;
    if !output.status.success() {
        return Err(WorkspaceSnapshotCaptureError::NotGitWorktree(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}

fn git_status(
    root: &Path,
) -> Result<BTreeMap<String, GitStatusEntry>, WorkspaceSnapshotCaptureError> {
    let output = git_output_bytes(
        root,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut fields = output.split(|byte| *byte == 0);
    let mut status = BTreeMap::new();
    while let Some(raw) = fields.next().filter(|raw| !raw.is_empty()) {
        if raw.len() < 4 || raw[2] != b' ' {
            return Err(WorkspaceSnapshotCaptureError::GitCommand {
                command: "status --porcelain=v1 -z".to_string(),
                message: "malformed porcelain record".to_string(),
            });
        }
        let code = [raw[0], raw[1]];
        let path = String::from_utf8(raw[3..].to_vec())
            .map_err(|_| WorkspaceSnapshotCaptureError::NonUtf8Path(root.to_path_buf()))?;
        let (change, renamed_from) = if code == *b"??" {
            (WorkspaceSnapshotChangeV1::Added, None)
        } else if code.contains(&b'R') || code.contains(&b'C') {
            let old = fields
                .next()
                .ok_or_else(|| WorkspaceSnapshotCaptureError::GitCommand {
                    command: "status --porcelain=v1 -z".to_string(),
                    message: "rename record is missing its source path".to_string(),
                })?;
            let old = String::from_utf8(old.to_vec())
                .map_err(|_| WorkspaceSnapshotCaptureError::NonUtf8Path(root.to_path_buf()))?;
            if code.contains(&b'D') {
                (WorkspaceSnapshotChangeV1::Deleted, None)
            } else if code.contains(&b'C') {
                (WorkspaceSnapshotChangeV1::Added, None)
            } else {
                (WorkspaceSnapshotChangeV1::Renamed, Some(old))
            }
        } else if code.contains(&b'D') {
            (WorkspaceSnapshotChangeV1::Deleted, None)
        } else if code.contains(&b'A') {
            (WorkspaceSnapshotChangeV1::Added, None)
        } else {
            (WorkspaceSnapshotChangeV1::Modified, None)
        };
        if is_excluded_path(&path) {
            continue;
        }
        status.insert(
            path.clone(),
            GitStatusEntry {
                path,
                change,
                renamed_from,
            },
        );
    }
    Ok(status)
}

fn is_excluded_path(path: &str) -> bool {
    path == ".git" || path.starts_with(".git/") || path == ".astra" || path.starts_with(".astra/")
}

fn capture_entries(
    root: &Path,
    paths: &BTreeSet<String>,
    status: &BTreeMap<String, GitStatusEntry>,
    workspace_root: &WorkspaceRoot,
) -> Result<Vec<CapturedEntry>, WorkspaceSnapshotCaptureError> {
    let mut captured = Vec::new();
    for path in paths {
        validate_relative_path(path)?;
        let item = status.get(path);
        let change = item
            .map(|item| item.change)
            .unwrap_or(WorkspaceSnapshotChangeV1::Unchanged);
        let renamed_from = item.and_then(|item| item.renamed_from.clone());
        let content = match capture_workspace_path(root, path, workspace_root) {
            Ok(content) => content,
            Err(error) if is_not_found(&error) && change == WorkspaceSnapshotChangeV1::Deleted => {
                // Tracked deletions are represented as tombstones below.
                continue;
            }
            Err(error) => return Err(error),
        };
        match content {
            WorkspacePathContent::Symlink { mode, target } => {
                let target = symlink_target_string(path, &target)?;
                captured.push(CapturedEntry {
                    entry: WorkspaceSnapshotEntryV1 {
                        path: path.clone(),
                        kind: WorkspaceSnapshotEntryKindV1::Symlink,
                        change,
                        mode,
                        size: 0,
                        digest: None,
                        blob_ref: None,
                        symlink_target: Some(target),
                        renamed_from,
                    },
                    bytes: None,
                });
            }
            WorkspacePathContent::File { mode, bytes } => {
                let digest = content_digest(&bytes);
                captured.push(CapturedEntry {
                    entry: WorkspaceSnapshotEntryV1 {
                        path: path.clone(),
                        kind: WorkspaceSnapshotEntryKindV1::File,
                        change,
                        mode,
                        size: bytes.len() as u64,
                        digest: Some(digest.clone()),
                        blob_ref: Some(format!("blob-{}", &digest[7..])),
                        symlink_target: None,
                        renamed_from,
                    },
                    bytes: Some(bytes),
                });
            }
            WorkspacePathContent::Directory => {
                // Git does not list directories, but a submodule appears as a
                // tracked directory.  Refuse it until its object boundary has a
                // first-class snapshot contract.
                return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
                    path: path.clone(),
                    reason: "nested Git worktrees and submodules are not supported yet".to_string(),
                });
            }
            WorkspacePathContent::Special => {
                return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
                    path: path.clone(),
                    reason: "special device files are not portable".to_string(),
                });
            }
        }
    }
    Ok(captured)
}

fn install_entries(
    package: &WorkspaceSnapshotPackage,
    staging: &Path,
) -> Result<(), WorkspaceSnapshotCaptureError> {
    validate_materialization_layout(package)?;
    let mut directories = Vec::new();
    for entry in &package.manifest.entries {
        validate_relative_path(&entry.path)?;
        if entry.change == WorkspaceSnapshotChangeV1::Deleted {
            continue;
        }
        let destination = checked_child_path(staging, &entry.path)?;
        match entry.kind {
            WorkspaceSnapshotEntryKindV1::Directory => {
                fs::create_dir_all(&destination).map_err(|source| {
                    WorkspaceSnapshotCaptureError::WritePath {
                        path: entry.path.clone(),
                        source,
                    }
                })?;
                directories.push(entry);
            }
            WorkspaceSnapshotEntryKindV1::File => {
                let blob_ref = entry.blob_ref.as_deref().ok_or_else(|| {
                    WorkspaceSnapshotCaptureError::MissingBlob(entry.path.clone())
                })?;
                let bytes = package.blobs.get(blob_ref).ok_or_else(|| {
                    WorkspaceSnapshotCaptureError::MissingBlob(entry.path.clone())
                })?;
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(|source| {
                        WorkspaceSnapshotCaptureError::WritePath {
                            path: parent.display().to_string(),
                            source,
                        }
                    })?;
                }
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&destination)
                    .map_err(|source| WorkspaceSnapshotCaptureError::WritePath {
                        path: entry.path.clone(),
                        source,
                    })?;
                file.write_all(bytes).map_err(|source| {
                    WorkspaceSnapshotCaptureError::WritePath {
                        path: entry.path.clone(),
                        source,
                    }
                })?;
                file.sync_all()
                    .map_err(|source| WorkspaceSnapshotCaptureError::WritePath {
                        path: entry.path.clone(),
                        source,
                    })?;
                set_mode(&destination, entry.mode)?;
            }
            WorkspaceSnapshotEntryKindV1::Symlink => {
                let target = entry.symlink_target.as_deref().ok_or_else(|| {
                    WorkspaceSnapshotCaptureError::InvalidManifest(
                        crate::WorkspaceSnapshotValidationError::InvalidEntryContent {
                            path: entry.path.clone(),
                            kind: entry.kind,
                            change: entry.change,
                        },
                    )
                })?;
                if let Some(parent) = destination.parent() {
                    fs::create_dir_all(parent).map_err(|source| {
                        WorkspaceSnapshotCaptureError::WritePath {
                            path: parent.display().to_string(),
                            source,
                        }
                    })?;
                }
                create_symlink(target, &destination).map_err(|source| {
                    WorkspaceSnapshotCaptureError::WritePath {
                        path: entry.path.clone(),
                        source,
                    }
                })?;
            }
        }
    }
    directories.sort_unstable_by(|left, right| {
        right
            .path
            .split('/')
            .count()
            .cmp(&left.path.split('/').count())
    });
    for entry in directories {
        let destination = checked_child_path(staging, &entry.path)?;
        set_mode(&destination, entry.mode)?;
    }
    Ok(())
}

/// Rename a completed staging directory without replacing a destination that
/// appeared after the initial existence check.  Linux and macOS expose an
/// atomic no-replace primitive; unsupported platforms fail closed instead of
/// falling back to an overwrite-capable rename.
fn rename_noreplace(source: &Path, target: &Path) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source contains NUL"))?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target contains NUL"))?;
        let result = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                libc::AT_FDCWD,
                source.as_ptr(),
                libc::AT_FDCWD,
                target.as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let source = std::ffi::CString::new(source.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "source contains NUL"))?;
        let target = std::ffi::CString::new(target.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "target contains NUL"))?;
        let result =
            unsafe { libc::renamex_np(source.as_ptr(), target.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (source, target);
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replace directory installation is unsupported on this platform",
        ))
    }
}

fn validate_materialization_layout(
    package: &WorkspaceSnapshotPackage,
) -> Result<(), WorkspaceSnapshotCaptureError> {
    let mut kinds = BTreeMap::<String, WorkspaceSnapshotEntryKindV1>::new();
    let mut symlinks = BTreeMap::<String, String>::new();
    for entry in &package.manifest.entries {
        if entry.change == WorkspaceSnapshotChangeV1::Deleted {
            continue;
        }
        let mut ancestor = PathBuf::new();
        let components = entry.path.split('/').collect::<Vec<_>>();
        for component in &components[..components.len().saturating_sub(1)] {
            ancestor.push(component);
            if matches!(
                kinds.get(
                    &ancestor
                        .to_string_lossy()
                        .replace(std::path::MAIN_SEPARATOR, "/")
                ),
                Some(WorkspaceSnapshotEntryKindV1::File | WorkspaceSnapshotEntryKindV1::Symlink)
            ) {
                return Err(WorkspaceSnapshotCaptureError::UnsafePath(
                    entry.path.clone(),
                ));
            }
        }
        kinds.insert(entry.path.clone(), entry.kind);
        if entry.kind == WorkspaceSnapshotEntryKindV1::Symlink
            && let Some(target) = &entry.symlink_target
        {
            symlinks.insert(entry.path.clone(), target.clone());
        }
    }
    for path in symlinks.keys() {
        validate_symlink_resolution(path, &symlinks)?;
    }
    Ok(())
}

fn validate_symlink_resolution(
    path: &str,
    symlinks: &BTreeMap<String, String>,
) -> Result<(), WorkspaceSnapshotCaptureError> {
    let mut components = path.split('/').map(str::to_owned).collect::<Vec<_>>();
    let _ = components.pop();
    let Some(target) = symlinks.get(path) else {
        return Err(WorkspaceSnapshotCaptureError::UnsafePath(path.to_string()));
    };
    components.extend(target.split('/').map(str::to_owned));
    let mut active = BTreeSet::new();
    let _ = resolve_symlink_components(&components, Vec::new(), symlinks, &mut active, path)?;
    Ok(())
}

/// Resolve one component sequence while keeping the active expansion stack.
/// A symlink is removed from the stack after its own target has been resolved,
/// so a safe path may visit the same alias more than once while an actual
/// cycle is still rejected.
fn resolve_symlink_components(
    components: &[String],
    initial: Vec<String>,
    symlinks: &BTreeMap<String, String>,
    active: &mut BTreeSet<String>,
    original: &str,
) -> Result<Vec<String>, WorkspaceSnapshotCaptureError> {
    struct Frame {
        components: Vec<String>,
        index: usize,
        active_symlink: Option<String>,
    }

    let mut resolved = initial;
    let mut frames = vec![Frame {
        components: components.to_vec(),
        index: 0,
        active_symlink: None,
    }];
    while !frames.is_empty() {
        if frames
            .last()
            .is_some_and(|frame| frame.index >= frame.components.len())
        {
            let Some(frame) = frames.pop() else {
                break;
            };
            if let Some(symlink) = frame.active_symlink {
                active.remove(&symlink);
            }
            continue;
        }
        let component = {
            let Some(frame) = frames.last_mut() else {
                break;
            };
            let component = frame.components[frame.index].clone();
            frame.index += 1;
            component
        };
        match component.as_str() {
            "" | "." => {}
            ".." => {
                if resolved.pop().is_none() {
                    return Err(WorkspaceSnapshotCaptureError::UnsafePath(
                        original.to_string(),
                    ));
                }
            }
            value => {
                resolved.push(value.to_string());
                let candidate = resolved.join("/");
                let Some(next_target) = symlinks.get(&candidate).cloned() else {
                    continue;
                };
                let _ = resolved.pop();
                if !active.insert(candidate.clone()) {
                    return Err(WorkspaceSnapshotCaptureError::UnsafePath(
                        original.to_string(),
                    ));
                }
                let target_components = next_target
                    .split('/')
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                frames.push(Frame {
                    components: target_components,
                    index: 0,
                    active_symlink: Some(candidate),
                });
            }
        }
    }
    Ok(resolved)
}

fn checked_child_path(
    root: &Path,
    relative: &str,
) -> Result<PathBuf, WorkspaceSnapshotCaptureError> {
    validate_relative_path(relative)?;
    let path = root.join(relative);
    if !path.starts_with(root) {
        return Err(WorkspaceSnapshotCaptureError::UnsafePath(
            relative.to_string(),
        ));
    }
    Ok(path)
}

fn validate_relative_path(path: &str) -> Result<(), WorkspaceSnapshotCaptureError> {
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || path.contains('\0')
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(WorkspaceSnapshotCaptureError::UnsafePath(path.to_string()));
    }
    Ok(())
}

fn symlink_target_string(
    path: &str,
    target: &Path,
) -> Result<String, WorkspaceSnapshotCaptureError> {
    if target.is_absolute() {
        return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
            path: path.to_string(),
            reason: "absolute symlinks are not portable".to_string(),
        });
    }
    let value = target
        .to_str()
        .ok_or_else(|| WorkspaceSnapshotCaptureError::NonUtf8Path(target.to_path_buf()))?
        .replace(std::path::MAIN_SEPARATOR, "/");
    let mut components = path.split('/').collect::<Vec<_>>();
    components.pop();
    for component in value.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return Err(WorkspaceSnapshotCaptureError::UnsupportedPath {
                        path: path.to_string(),
                        reason: "symlink escapes the workspace".to_string(),
                    });
                }
            }
            _ => components.push(component),
        }
    }
    Ok(value)
}

fn content_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn fingerprint_entries(entries: &[CapturedEntry]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.workspace-capture-fingerprint.v1\0");
    for captured in entries {
        let entry = &captured.entry;
        digest.update(entry.path.as_bytes());
        digest.update([0]);
        digest.update(format!("{:?}:{:?}:{}", entry.kind, entry.change, entry.mode).as_bytes());
        digest.update([0]);
        if let Some(bytes) = &captured.bytes {
            digest.update(content_digest(bytes).as_bytes());
        } else if let Some(target) = &entry.symlink_target {
            digest.update(target.as_bytes());
        }
        digest.update([0]);
    }
    content_digest(&digest.finalize())
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), WorkspaceSnapshotCaptureError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        WorkspaceSnapshotCaptureError::WritePath {
            path: path.display().to_string(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), WorkspaceSnapshotCaptureError> {
    Ok(())
}

#[cfg(unix)]
fn create_symlink(target: &str, destination: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, destination)
}

#[cfg(not(unix))]
fn create_symlink(_target: &str, _destination: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "symlinks are not supported on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .expect("git command");
        assert!(
            output.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        git(dir.path(), &["init", "--quiet"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        git(dir.path(), &["config", "user.name", "Snapshot Test"]);
        fs::create_dir_all(dir.path().join("src")).expect("src");
        fs::write(dir.path().join("src/main.txt"), "before\n").expect("file");
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "--quiet", "-m", "initial"]);
        dir
    }

    #[test]
    fn captures_tracked_changes_without_private_git_state() {
        let dir = repository();
        fs::write(dir.path().join("src/main.txt"), "after\n").expect("modify");
        fs::write(dir.path().join("ignored-secret"), "secret").expect("new");
        let package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new("snapshot-1", "workspace-1"),
        )
        .expect("capture");
        package.verify().expect("verify");
        assert_eq!(package.blobs.len(), 1);
        assert!(
            package
                .manifest
                .entries
                .iter()
                .any(|entry| entry.path == "src/main.txt"
                    && entry.change == WorkspaceSnapshotChangeV1::Modified)
        );
        assert!(
            !package
                .manifest
                .entries
                .iter()
                .any(|entry| entry.path == "ignored-secret")
        );
        assert!(
            package
                .manifest
                .exclusions
                .iter()
                .any(|exclusion| exclusion.pattern == ".git/")
        );
    }

    #[test]
    fn explicit_untracked_selection_is_captured() {
        let dir = repository();
        fs::write(dir.path().join("new.txt"), "new\n").expect("new");
        let mut options = WorkspaceSnapshotCaptureOptions::new("snapshot-2", "workspace-2");
        options.include_untracked = true;
        let package = capture_git_worktree(dir.path(), &options).expect("capture");
        assert!(package.manifest.entries.iter().any(|entry| {
            entry.path == "new.txt" && entry.change == WorkspaceSnapshotChangeV1::Added
        }));
    }

    #[test]
    fn deduplicates_blob_bytes_but_counts_each_workspace_file() {
        let dir = repository();
        fs::write(dir.path().join("copy.txt"), "before\n").expect("copy");
        git(dir.path(), &["add", "copy.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "copy"]);
        let package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new("snapshot-duplicate", "workspace-duplicate"),
        )
        .expect("capture");
        package.verify().expect("verify");
        assert_eq!(package.blobs.len(), 1);
        assert_eq!(package.manifest.content.blob_count, 1);
        assert_eq!(package.manifest.content.total_bytes, 14);
    }

    #[test]
    fn private_astra_paths_are_never_captured() {
        let dir = repository();
        fs::create_dir_all(dir.path().join(".astra")).expect("private dir");
        fs::write(dir.path().join(".astra/credentials"), "secret").expect("secret");
        git(dir.path(), &["add", ".astra/credentials"]);
        git(dir.path(), &["commit", "--quiet", "-m", "private"]);
        fs::write(dir.path().join(".astra/untracked"), "secret").expect("untracked secret");
        let mut options =
            WorkspaceSnapshotCaptureOptions::new("snapshot-private", "workspace-private");
        options.include_untracked = true;
        let package = capture_git_worktree(dir.path(), &options).expect("capture");
        assert!(
            package
                .manifest
                .entries
                .iter()
                .all(|entry| !is_excluded_path(&entry.path))
        );
    }

    #[test]
    fn staged_rename_followed_by_delete_is_a_deletion_tombstone() {
        let dir = repository();
        git(dir.path(), &["mv", "src/main.txt", "src/renamed.txt"]);
        fs::remove_file(dir.path().join("src/renamed.txt")).expect("remove renamed file");
        let package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new(
                "snapshot-rename-delete",
                "workspace-rename-delete",
            ),
        )
        .expect("capture");
        assert!(package.manifest.entries.iter().any(|entry| {
            entry.path == "src/renamed.txt" && entry.change == WorkspaceSnapshotChangeV1::Deleted
        }));
    }

    #[test]
    fn git_copy_status_is_consumed_as_an_added_file() {
        let dir = repository();
        git(dir.path(), &["config", "status.renames", "copies"]);
        fs::write(dir.path().join("src/main.txt"), "after\n").expect("modify source");
        git(dir.path(), &["add", "src/main.txt"]);
        fs::write(dir.path().join("copy.txt"), "before\n").expect("copy source");
        git(dir.path(), &["add", "copy.txt"]);
        let package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new("snapshot-copy", "workspace-copy"),
        )
        .expect("capture");
        assert!(package.manifest.entries.iter().any(|entry| {
            entry.path == "copy.txt" && entry.change == WorkspaceSnapshotChangeV1::Added
        }));
    }

    #[test]
    fn shared_blob_references_are_checked_for_every_entry() {
        let dir = repository();
        fs::write(dir.path().join("copy.txt"), "before\n").expect("copy");
        git(dir.path(), &["add", "copy.txt"]);
        git(dir.path(), &["commit", "--quiet", "-m", "copy"]);
        let package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new("snapshot-shared-blob", "workspace-shared-blob"),
        )
        .expect("capture");
        let mut tampered_digest = package.clone();
        let second = tampered_digest
            .manifest
            .entries
            .iter_mut()
            .find(|entry| entry.path == "src/main.txt")
            .expect("second file");
        second.digest = Some(content_digest(b"tampered"));
        tampered_digest.manifest.content.content_root =
            tampered_digest.manifest.computed_content_root().unwrap();
        assert!(matches!(
            tampered_digest.verify(),
            Err(WorkspaceSnapshotCaptureError::BlobDigestMismatch { path }) if path == "src/main.txt"
        ));
        let mut tampered_size = package;
        let second = tampered_size
            .manifest
            .entries
            .iter_mut()
            .find(|entry| entry.path == "src/main.txt")
            .expect("second file");
        second.size += 1;
        tampered_size.manifest.content.total_bytes += 1;
        tampered_size.manifest.content.content_root =
            tampered_size.manifest.computed_content_root().unwrap();
        assert!(matches!(
            tampered_size.verify(),
            Err(WorkspaceSnapshotCaptureError::BlobSizeMismatch { path }) if path == "src/main.txt"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn capture_rejects_an_ancestor_directory_symlink() {
        let dir = repository();
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("secret"), "secret").expect("outside file");
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).expect("symlink");
        let paths = BTreeSet::from(["link/secret".to_string()]);
        let workspace_root = workspace_root_handle(dir.path()).expect("root");
        let error =
            capture_entries(dir.path(), &paths, &BTreeMap::new(), &workspace_root).unwrap_err();
        eprintln!("capture error: {error:?}");
        assert!(matches!(
            error,
            WorkspaceSnapshotCaptureError::UnsupportedPath { .. }
        ));
    }

    #[test]
    fn materialization_rejects_symlink_ancestors_in_manifest() {
        let entries = vec![
            WorkspaceSnapshotEntryV1 {
                path: "a".to_string(),
                kind: WorkspaceSnapshotEntryKindV1::Symlink,
                change: WorkspaceSnapshotChangeV1::Added,
                mode: 0o777,
                size: 0,
                digest: None,
                blob_ref: None,
                symlink_target: Some(".".to_string()),
                renamed_from: None,
            },
            WorkspaceSnapshotEntryV1 {
                path: "a/b".to_string(),
                kind: WorkspaceSnapshotEntryKindV1::File,
                change: WorkspaceSnapshotChangeV1::Added,
                mode: 0o644,
                size: 1,
                digest: Some(content_digest(b"x")),
                blob_ref: Some("blob-x".to_string()),
                symlink_target: None,
                renamed_from: None,
            },
        ];
        let mut package = WorkspaceSnapshotPackage {
            manifest: WorkspaceSnapshotManifestV1 {
                schema_version: WORKSPACE_SNAPSHOT_MANIFEST_SCHEMA_VERSION,
                snapshot_id: "snapshot-layout".to_string(),
                logical_workspace_id: "workspace-layout".to_string(),
                repository: WorkspaceSnapshotRepositoryV1 {
                    repository_id: "repo-layout".to_string(),
                    base_commit: None,
                    base_tree: None,
                    submodules: Vec::new(),
                },
                capture: WorkspaceSnapshotCaptureV1 {
                    fingerprint_before: content_digest(b"capture"),
                    fingerprint_after: content_digest(b"capture"),
                    captured_at: "2026-09-17T00:00:00Z".to_string(),
                    consistent: true,
                },
                entries,
                exclusions: Vec::new(),
                data_sources: Vec::new(),
                content: WorkspaceSnapshotContentV1 {
                    content_root: String::new(),
                    total_bytes: 1,
                    blob_count: 1,
                    pack_ref: None,
                },
            },
            blobs: BTreeMap::from([("blob-x".to_string(), b"x".to_vec())]),
        };
        package.manifest.content.content_root = package.manifest.computed_content_root().unwrap();
        assert!(matches!(
            package.verify(),
            Err(WorkspaceSnapshotCaptureError::UnsafePath(path)) if path == "a/b"
        ));
    }

    #[test]
    fn package_verify_rejects_unreferenced_blob() {
        let dir = repository();
        fs::write(dir.path().join("src/main.txt"), "hello").expect("file");
        let mut package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new("snapshot-extra", "workspace-extra"),
        )
        .expect("capture");
        package.blobs.insert(
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
            b"extra".to_vec(),
        );
        assert!(matches!(
            package.verify(),
            Err(WorkspaceSnapshotCaptureError::UnexpectedBlob(_))
        ));
    }

    #[test]
    fn symlink_resolution_allows_reusing_a_safe_alias() {
        let symlinks = BTreeMap::from([
            ("a".to_string(), ".".to_string()),
            ("dir/link".to_string(), "../a/a".to_string()),
        ]);
        assert!(validate_symlink_resolution("dir/link", &symlinks).is_ok());
    }

    #[test]
    fn symlink_resolution_rejects_a_chained_escape() {
        let symlinks = BTreeMap::from([
            ("a".to_string(), ".".to_string()),
            ("dir/link".to_string(), "../a/..".to_string()),
        ]);
        assert!(matches!(
            validate_symlink_resolution("dir/link", &symlinks),
            Err(WorkspaceSnapshotCaptureError::UnsafePath(path)) if path == "dir/link"
        ));
    }

    #[cfg(unix)]
    #[test]
    fn materialization_requires_new_directory_and_preserves_modes_and_links() {
        let dir = repository();
        fs::write(dir.path().join("src/main.txt"), "after\n").expect("modify");
        std::os::unix::fs::symlink("main.txt", dir.path().join("src/link.txt")).expect("symlink");
        let mut options = WorkspaceSnapshotCaptureOptions::new("snapshot-3", "workspace-3");
        options.include_untracked = true;
        let mut package = capture_git_worktree(dir.path(), &options).expect("capture");
        package.manifest.entries.push(WorkspaceSnapshotEntryV1 {
            path: "empty".to_string(),
            kind: WorkspaceSnapshotEntryKindV1::Directory,
            change: WorkspaceSnapshotChangeV1::Added,
            mode: 0o700,
            size: 0,
            digest: None,
            blob_ref: None,
            symlink_target: None,
            renamed_from: None,
        });
        package
            .manifest
            .entries
            .sort_by(|left, right| left.path.cmp(&right.path));
        package.manifest.content.content_root = package.manifest.computed_content_root().unwrap();
        let parent = tempfile::tempdir().expect("target parent");
        let target = parent.path().join("restored");
        let receipt = materialize_workspace_snapshot(&package, &target).expect("materialize");
        assert_eq!(receipt.target, target);
        assert_eq!(
            fs::read_to_string(target.join("src/main.txt")).unwrap(),
            "after\n"
        );
        assert_eq!(
            fs::read_link(target.join("src/link.txt")).unwrap(),
            PathBuf::from("main.txt")
        );
        assert_eq!(
            fs::metadata(target.join("empty"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o700
        );
        assert!(
            fs::metadata(target.join("src/main.txt"))
                .unwrap()
                .permissions()
                .mode()
                & 0o400
                != 0
        );
        assert!(matches!(
            materialize_workspace_snapshot(&package, &target),
            Err(WorkspaceSnapshotCaptureError::DestinationExists(_))
        ));

        let actual_parent = parent.path().join("actual");
        fs::create_dir(&actual_parent).expect("actual parent");
        let alias_parent = parent.path().join("alias");
        std::os::unix::fs::symlink("actual", &alias_parent).expect("alias parent");
        let alias_target = alias_parent.join("alias-restored");
        let alias_receipt = materialize_workspace_snapshot(&package, &alias_target)
            .expect("materialize through parent alias");
        assert_eq!(alias_receipt.target, actual_parent.join("alias-restored"));
        assert!(actual_parent.join("alias-restored/src/main.txt").is_file());
    }

    #[cfg(unix)]
    #[test]
    fn workspace_parent_duplication_keeps_close_on_exec() {
        let dir = repository();
        let workspace_root = workspace_root_handle(dir.path()).expect("root");
        let (parent, _) = open_workspace_parent(&workspace_root, "main.txt").expect("parent");
        let flags = unsafe { libc::fcntl(parent.as_raw_fd(), libc::F_GETFD) };
        assert!(
            flags >= 0,
            "fcntl(F_GETFD) failed: {}",
            io::Error::last_os_error()
        );
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }

    #[test]
    fn concurrent_mutation_is_not_published() {
        // The provider performs a before/after comparison.  A deterministic
        // unit-level check still guards that a changed package cannot be
        // materialized after its digest is tampered with.
        let dir = repository();
        let mut package = capture_git_worktree(
            dir.path(),
            &WorkspaceSnapshotCaptureOptions::new("snapshot-4", "workspace-4"),
        )
        .expect("capture");
        let blob_ref = package
            .manifest
            .entries
            .iter()
            .find_map(|entry| entry.blob_ref.clone())
            .expect("blob");
        package.blobs.get_mut(&blob_ref).unwrap().push(b'x');
        assert!(matches!(
            package.verify(),
            Err(WorkspaceSnapshotCaptureError::BlobDigestMismatch { .. })
        ));
    }
}
