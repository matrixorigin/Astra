//! Portable workspace snapshot contracts.
//!
//! The Edge or User Runner captures these manifests.  A Server only stores
//! the validated, content-addressed result and never reads an Edge path.
//! Entries describe the final workspace state plus enough change information
//! to explain what will be restored on another materialization.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use thiserror::Error;

pub const WORKSPACE_SNAPSHOT_MANIFEST_SCHEMA_VERSION: u32 = 1;
const WORKSPACE_SNAPSHOT_HASH_DOMAIN: &[u8] = b"astra.workspace-snapshot-manifest.v1\0";
const MAX_ID_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ENTRIES: usize = 1_000_000;
const MAX_EXCLUSIONS: usize = 4096;
const MAX_DATA_SOURCES: usize = 64;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSnapshotEntryKindV1 {
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSnapshotChangeV1 {
    Unchanged,
    Added,
    Modified,
    Deleted,
    Renamed,
}

/// One normalized path in a snapshot.  A deleted entry is a tombstone and
/// therefore has no content digest or blob reference.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotEntryV1 {
    pub path: String,
    pub kind: WorkspaceSnapshotEntryKindV1,
    pub change: WorkspaceSnapshotChangeV1,
    pub mode: u32,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blob_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symlink_target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renamed_from: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotRepositoryV1 {
    pub repository_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_tree: Option<String>,
    #[serde(default)]
    pub submodules: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotCaptureV1 {
    pub fingerprint_before: String,
    pub fingerprint_after: String,
    pub captured_at: String,
    pub consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotContentV1 {
    pub content_root: String,
    pub total_bytes: u64,
    pub blob_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pack_ref: Option<String>,
}

/// Versioned data state associated with the code workspace.  MatrixOne
/// Git4Data is represented as a provider here so code and data can be
/// restored together without making the generic workspace contract depend on
/// a database client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSnapshotDataProviderV1 {
    Git,
    MatrixoneGit4Data,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotDataSourceV1 {
    pub provider: WorkspaceSnapshotDataProviderV1,
    pub source_id: String,
    pub version: String,
    pub content_root: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotExclusionV1 {
    pub pattern: String,
    pub reason: String,
}

/// A portable, immutable description of one captured workspace boundary.
/// Content is uploaded and verified before a recovery point can reference it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceSnapshotManifestV1 {
    pub schema_version: u32,
    pub snapshot_id: String,
    pub logical_workspace_id: String,
    pub repository: WorkspaceSnapshotRepositoryV1,
    pub capture: WorkspaceSnapshotCaptureV1,
    pub entries: Vec<WorkspaceSnapshotEntryV1>,
    #[serde(default)]
    pub exclusions: Vec<WorkspaceSnapshotExclusionV1>,
    #[serde(default)]
    pub data_sources: Vec<WorkspaceSnapshotDataSourceV1>,
    pub content: WorkspaceSnapshotContentV1,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WorkspaceSnapshotValidationError {
    #[error("unsupported workspace snapshot schema version {actual}")]
    UnsupportedSchema { actual: u32 },
    #[error("{field} must be a non-empty safe identifier")]
    InvalidIdentity { field: &'static str },
    #[error("{field} exceeds the {maximum} byte limit")]
    Oversized { field: &'static str, maximum: usize },
    #[error("{field} must be a normalized relative path")]
    InvalidPath { field: &'static str },
    #[error("{field} must be a sha256:<64 lowercase hex> digest")]
    InvalidDigest { field: &'static str },
    #[error("{field} must be a native Git object id (40 or 64 lowercase hex characters)")]
    InvalidGitObjectId { field: &'static str },
    #[error("workspace capture fingerprints differ or capture was not marked consistent")]
    InconsistentCapture,
    #[error("workspace snapshot contains duplicate path {path}")]
    DuplicatePath { path: String },
    #[error("workspace snapshot entries are not in canonical path order")]
    NonCanonicalEntryOrder,
    #[error("workspace snapshot has too many entries (maximum {maximum})")]
    TooManyEntries { maximum: usize },
    #[error("workspace snapshot has too many exclusions (maximum {maximum})")]
    TooManyExclusions { maximum: usize },
    #[error("workspace snapshot has too many data sources (maximum {maximum})")]
    TooManyDataSources { maximum: usize },
    #[error("workspace snapshot contains duplicate data source {source_id}")]
    DuplicateDataSource { source_id: String },
    #[error("workspace snapshot content aggregate does not match entries: {field}")]
    ContentAggregateMismatch { field: &'static str },
    #[error("workspace snapshot content root does not match canonical entries")]
    ContentRootMismatch,
    #[error("entry {path} has invalid content fields for {kind:?} / {change:?}")]
    InvalidEntryContent {
        path: String,
        kind: WorkspaceSnapshotEntryKindV1,
        change: WorkspaceSnapshotChangeV1,
    },
    #[error("entry {path} has an invalid rename source")]
    InvalidRenameSource { path: String },
    #[error("entry {path} has a symlink target that escapes the workspace")]
    SymlinkEscapesWorkspace { path: String },
    #[error("workspace snapshot serialization failed: {0}")]
    Serialization(String),
}

impl WorkspaceSnapshotManifestV1 {
    /// Compute the content identity from canonical entry metadata and data
    /// source references.  File digests bind the actual bytes; the later
    /// content verifier additionally proves that every referenced blob is
    /// available and has the declared digest.
    pub fn computed_content_root(&self) -> Result<String, WorkspaceSnapshotValidationError> {
        #[derive(Serialize)]
        struct ContentIdentity<'a> {
            entries: &'a [WorkspaceSnapshotEntryV1],
            data_sources: &'a [WorkspaceSnapshotDataSourceV1],
        }

        let identity = ContentIdentity {
            entries: &self.entries,
            data_sources: &self.data_sources,
        };
        let mut counter = CountingWriter::default();
        serde_json::to_writer(&mut counter, &identity)
            .map_err(|error| WorkspaceSnapshotValidationError::Serialization(error.to_string()))?;
        let mut digest = Sha256::new();
        digest.update(b"astra.workspace-content.v1\0");
        digest.update(counter.bytes.to_be_bytes());
        serde_json::to_writer(&mut DigestWriter(&mut digest), &identity)
            .map_err(|error| WorkspaceSnapshotValidationError::Serialization(error.to_string()))?;
        Ok(format!("sha256:{:x}", digest.finalize()))
    }

    pub fn validate(&self) -> Result<(), WorkspaceSnapshotValidationError> {
        if self.schema_version != WORKSPACE_SNAPSHOT_MANIFEST_SCHEMA_VERSION {
            return Err(WorkspaceSnapshotValidationError::UnsupportedSchema {
                actual: self.schema_version,
            });
        }
        validate_identity("snapshot_id", &self.snapshot_id)?;
        validate_identity("logical_workspace_id", &self.logical_workspace_id)?;
        validate_identity("repository.repository_id", &self.repository.repository_id)?;
        for (field, value) in [
            (
                "capture.fingerprint_before",
                &self.capture.fingerprint_before,
            ),
            ("capture.fingerprint_after", &self.capture.fingerprint_after),
            ("capture.captured_at", &self.capture.captured_at),
        ] {
            validate_identity(field, value)?;
        }
        if !self.capture.consistent
            || self.capture.fingerprint_before != self.capture.fingerprint_after
        {
            return Err(WorkspaceSnapshotValidationError::InconsistentCapture);
        }
        if let Some(commit) = &self.repository.base_commit {
            validate_git_object_id("repository.base_commit", commit)?;
        }
        if let Some(tree) = &self.repository.base_tree {
            validate_git_object_id("repository.base_tree", tree)?;
        }
        for submodule in &self.repository.submodules {
            validate_identity("repository.submodules", submodule)?;
        }
        validate_digest("content.content_root", &self.content.content_root)?;
        if let Some(pack_ref) = &self.content.pack_ref {
            validate_identity("content.pack_ref", pack_ref)?;
        }
        if self.data_sources.len() > MAX_DATA_SOURCES {
            return Err(WorkspaceSnapshotValidationError::TooManyDataSources {
                maximum: MAX_DATA_SOURCES,
            });
        }
        let mut data_source_ids = std::collections::BTreeSet::new();
        for source in &self.data_sources {
            validate_identity("data_source.source_id", &source.source_id)?;
            validate_identity("data_source.version", &source.version)?;
            validate_digest("data_source.content_root", &source.content_root)?;
            if let Some(schema_ref) = &source.schema_ref {
                validate_identity("data_source.schema_ref", schema_ref)?;
            }
            if !data_source_ids.insert(&source.source_id) {
                return Err(WorkspaceSnapshotValidationError::DuplicateDataSource {
                    source_id: source.source_id.clone(),
                });
            }
        }
        if self.entries.len() > MAX_ENTRIES {
            return Err(WorkspaceSnapshotValidationError::TooManyEntries {
                maximum: MAX_ENTRIES,
            });
        }
        let mut computed_total_bytes = 0_u64;
        let mut computed_blob_refs = std::collections::BTreeSet::new();
        let mut previous_path: Option<&str> = None;
        for entry in &self.entries {
            validate_relative_path("entry.path", &entry.path)?;
            if previous_path.is_some_and(|previous| previous >= entry.path.as_str()) {
                if previous_path == Some(entry.path.as_str()) {
                    return Err(WorkspaceSnapshotValidationError::DuplicatePath {
                        path: entry.path.clone(),
                    });
                }
                return Err(WorkspaceSnapshotValidationError::NonCanonicalEntryOrder);
            }
            previous_path = Some(&entry.path);
            if entry.mode > 0o7777 {
                return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                    path: entry.path.clone(),
                    kind: entry.kind,
                    change: entry.change,
                });
            }
            if entry.change == WorkspaceSnapshotChangeV1::Deleted {
                if entry.size != 0
                    || entry.digest.is_some()
                    || entry.blob_ref.is_some()
                    || entry.symlink_target.is_some()
                {
                    return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                        path: entry.path.clone(),
                        kind: entry.kind,
                        change: entry.change,
                    });
                }
            } else {
                match entry.kind {
                    WorkspaceSnapshotEntryKindV1::File => {
                        let Some(digest) = entry.digest.as_deref() else {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        };
                        validate_digest("entry.digest", digest)?;
                        if entry.blob_ref.is_none() {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        }
                        let Some(blob_ref) = entry.blob_ref.as_ref() else {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        };
                        validate_identity("entry.blob_ref", blob_ref)?;
                        if entry.symlink_target.is_some() {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        }
                        computed_total_bytes = computed_total_bytes.checked_add(entry.size).ok_or(
                            WorkspaceSnapshotValidationError::ContentAggregateMismatch {
                                field: "content.total_bytes",
                            },
                        )?;
                        computed_blob_refs.insert(blob_ref);
                    }
                    WorkspaceSnapshotEntryKindV1::Directory => {
                        if entry.size != 0
                            || entry.digest.is_some()
                            || entry.blob_ref.is_some()
                            || entry.symlink_target.is_some()
                        {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        }
                    }
                    WorkspaceSnapshotEntryKindV1::Symlink => {
                        let Some(target) = entry.symlink_target.as_deref() else {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        };
                        if !symlink_target_stays_within_workspace(&entry.path, target) {
                            return Err(
                                WorkspaceSnapshotValidationError::SymlinkEscapesWorkspace {
                                    path: entry.path.clone(),
                                },
                            );
                        }
                        if entry.size != 0 || entry.digest.is_some() || entry.blob_ref.is_some() {
                            return Err(WorkspaceSnapshotValidationError::InvalidEntryContent {
                                path: entry.path.clone(),
                                kind: entry.kind,
                                change: entry.change,
                            });
                        }
                    }
                }
            }
            if let Some(renamed_from) = &entry.renamed_from {
                validate_relative_path("entry.renamed_from", renamed_from)?;
                if entry.change != WorkspaceSnapshotChangeV1::Renamed || renamed_from == &entry.path
                {
                    return Err(WorkspaceSnapshotValidationError::InvalidRenameSource {
                        path: entry.path.clone(),
                    });
                }
            } else if entry.change == WorkspaceSnapshotChangeV1::Renamed {
                return Err(WorkspaceSnapshotValidationError::InvalidRenameSource {
                    path: entry.path.clone(),
                });
            }
        }
        if self.exclusions.len() > MAX_EXCLUSIONS {
            return Err(WorkspaceSnapshotValidationError::TooManyExclusions {
                maximum: MAX_EXCLUSIONS,
            });
        }
        let mut previous_pattern: Option<&str> = None;
        for exclusion in &self.exclusions {
            validate_identity("exclusion.pattern", &exclusion.pattern)?;
            validate_description("exclusion.reason", &exclusion.reason)?;
            if previous_pattern.is_some_and(|previous| previous >= exclusion.pattern.as_str()) {
                return Err(WorkspaceSnapshotValidationError::NonCanonicalEntryOrder);
            }
            previous_pattern = Some(&exclusion.pattern);
        }
        if self.content.total_bytes != computed_total_bytes {
            return Err(WorkspaceSnapshotValidationError::ContentAggregateMismatch {
                field: "content.total_bytes",
            });
        }
        if self.content.blob_count != computed_blob_refs.len() as u64 {
            return Err(WorkspaceSnapshotValidationError::ContentAggregateMismatch {
                field: "content.blob_count",
            });
        }
        if self.content.content_root != self.computed_content_root()? {
            return Err(WorkspaceSnapshotValidationError::ContentRootMismatch);
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, WorkspaceSnapshotValidationError> {
        self.validate()?;
        serde_json::to_vec(self)
            .map_err(|error| WorkspaceSnapshotValidationError::Serialization(error.to_string()))
    }

    pub fn content_hash(&self) -> Result<String, WorkspaceSnapshotValidationError> {
        self.validate()?;
        let mut counter = CountingWriter::default();
        serde_json::to_writer(&mut counter, self)
            .map_err(|error| WorkspaceSnapshotValidationError::Serialization(error.to_string()))?;
        let mut digest = Sha256::new();
        digest.update(WORKSPACE_SNAPSHOT_HASH_DOMAIN);
        digest.update(counter.bytes.to_be_bytes());
        serde_json::to_writer(&mut DigestWriter(&mut digest), self)
            .map_err(|error| WorkspaceSnapshotValidationError::Serialization(error.to_string()))?;
        Ok(format!("sha256:{:x}", digest.finalize()))
    }
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| io::Error::other("workspace snapshot byte count overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct DigestWriter<'a>(&'a mut Sha256);

impl Write for DigestWriter<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0.update(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn validate_identity(
    field: &'static str,
    value: &str,
) -> Result<(), WorkspaceSnapshotValidationError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        if value.len() > MAX_ID_BYTES {
            return Err(WorkspaceSnapshotValidationError::Oversized {
                field,
                maximum: MAX_ID_BYTES,
            });
        }
        return Err(WorkspaceSnapshotValidationError::InvalidIdentity { field });
    }
    Ok(())
}

fn validate_description(
    field: &'static str,
    value: &str,
) -> Result<(), WorkspaceSnapshotValidationError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value.bytes().any(|byte| byte.is_ascii_control())
    {
        if value.len() > MAX_ID_BYTES {
            return Err(WorkspaceSnapshotValidationError::Oversized {
                field,
                maximum: MAX_ID_BYTES,
            });
        }
        return Err(WorkspaceSnapshotValidationError::InvalidIdentity { field });
    }
    Ok(())
}

fn validate_digest(
    field: &'static str,
    value: &str,
) -> Result<(), WorkspaceSnapshotValidationError> {
    if value.len() != 71
        || !value.starts_with("sha256:")
        || !value[7..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkspaceSnapshotValidationError::InvalidDigest { field });
    }
    Ok(())
}

fn validate_git_object_id(
    field: &'static str,
    value: &str,
) -> Result<(), WorkspaceSnapshotValidationError> {
    // Git records the object id in its native object format. SHA-1
    // repositories use 40 lowercase hex characters and SHA-256 repositories
    // use 64; unlike content digests these values have no `sha256:` prefix.
    if !matches!(value.len(), 40 | 64)
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(WorkspaceSnapshotValidationError::InvalidGitObjectId { field });
    }
    Ok(())
}

fn validate_relative_path(
    field: &'static str,
    value: &str,
) -> Result<(), WorkspaceSnapshotValidationError> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value.starts_with('/')
        || value.contains('\\')
        || value.contains('\0')
        || value.contains(':')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        if value.len() > MAX_PATH_BYTES {
            return Err(WorkspaceSnapshotValidationError::Oversized {
                field,
                maximum: MAX_PATH_BYTES,
            });
        }
        return Err(WorkspaceSnapshotValidationError::InvalidPath { field });
    }
    Ok(())
}

fn symlink_target_stays_within_workspace(path: &str, target: &str) -> bool {
    if target.is_empty()
        || target.starts_with('/')
        || target.contains('\\')
        || target.contains('\0')
        || target.contains(':')
    {
        return false;
    }
    let mut components = path.split('/').collect::<Vec<_>>();
    components.pop();
    for component in target.split('/') {
        match component {
            "" | "." => {}
            ".." => {
                if components.pop().is_none() {
                    return false;
                }
            }
            value => components.push(value),
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn sample_snapshot() -> WorkspaceSnapshotManifestV1 {
        let mut snapshot = WorkspaceSnapshotManifestV1 {
            schema_version: WORKSPACE_SNAPSHOT_MANIFEST_SCHEMA_VERSION,
            snapshot_id: "snapshot-a".into(),
            logical_workspace_id: "workspace-a".into(),
            repository: WorkspaceSnapshotRepositoryV1 {
                repository_id: "repo-a".into(),
                base_commit: Some("a".repeat(40)),
                base_tree: Some("b".repeat(64)),
                submodules: vec![],
            },
            capture: WorkspaceSnapshotCaptureV1 {
                fingerprint_before: digest('c'),
                fingerprint_after: digest('c'),
                captured_at: "2026-09-16T00:00:00Z".into(),
                consistent: true,
            },
            entries: vec![
                WorkspaceSnapshotEntryV1 {
                    path: "src/main.rs".into(),
                    kind: WorkspaceSnapshotEntryKindV1::File,
                    change: WorkspaceSnapshotChangeV1::Modified,
                    mode: 0o644,
                    size: 12,
                    digest: Some(digest('d')),
                    blob_ref: Some("blob-main".into()),
                    symlink_target: None,
                    renamed_from: None,
                },
                WorkspaceSnapshotEntryV1 {
                    path: "src/tool".into(),
                    kind: WorkspaceSnapshotEntryKindV1::Symlink,
                    change: WorkspaceSnapshotChangeV1::Added,
                    mode: 0o777,
                    size: 0,
                    digest: None,
                    blob_ref: None,
                    symlink_target: Some("main.rs".into()),
                    renamed_from: None,
                },
            ],
            exclusions: vec![WorkspaceSnapshotExclusionV1 {
                pattern: ".env".into(),
                reason: "credentials".into(),
            }],
            data_sources: vec![WorkspaceSnapshotDataSourceV1 {
                provider: WorkspaceSnapshotDataProviderV1::MatrixoneGit4Data,
                source_id: "analytics".into(),
                version: "commit-42".into(),
                content_root: digest('f'),
                schema_ref: Some("analytics.public".into()),
            }],
            content: WorkspaceSnapshotContentV1 {
                content_root: digest('e'),
                total_bytes: 12,
                blob_count: 1,
                pack_ref: Some("pack-a".into()),
            },
        };
        snapshot.content.content_root = snapshot.computed_content_root().unwrap();
        snapshot
    }

    #[test]
    fn validates_git_like_entries_and_hashes_canonically() {
        let snapshot = sample_snapshot();
        snapshot.validate().unwrap();
        assert!(snapshot.content_hash().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn rejects_self_asserted_content_aggregates_or_root() {
        let mut snapshot = sample_snapshot();
        snapshot.content.total_bytes += 1;
        assert!(matches!(
            snapshot.validate(),
            Err(WorkspaceSnapshotValidationError::ContentAggregateMismatch {
                field: "content.total_bytes"
            })
        ));

        let mut snapshot = sample_snapshot();
        snapshot.content.content_root = digest('e');
        assert_eq!(
            snapshot.validate(),
            Err(WorkspaceSnapshotValidationError::ContentRootMismatch)
        );
    }

    #[test]
    fn rejects_path_traversal_and_inconsistent_capture() {
        let mut snapshot = sample_snapshot();
        snapshot.entries[0].path = "../secrets".into();
        assert!(matches!(
            snapshot.validate(),
            Err(WorkspaceSnapshotValidationError::InvalidPath { .. })
        ));

        let mut snapshot = sample_snapshot();
        snapshot.capture.fingerprint_after = digest('f');
        assert_eq!(
            snapshot.validate(),
            Err(WorkspaceSnapshotValidationError::InconsistentCapture)
        );
    }

    #[test]
    fn rejects_prefixed_content_digest_as_git_object_id() {
        let mut snapshot = sample_snapshot();
        snapshot.repository.base_commit = Some(digest('a'));
        assert_eq!(
            snapshot.validate(),
            Err(WorkspaceSnapshotValidationError::InvalidGitObjectId {
                field: "repository.base_commit"
            })
        );
    }

    #[test]
    fn accepts_internal_symlink_and_rejects_escape() {
        assert!(symlink_target_stays_within_workspace("src/tool", "main.rs"));
        assert!(symlink_target_stays_within_workspace(
            "src/deep/tool",
            "../main.rs"
        ));
        assert!(!symlink_target_stays_within_workspace(
            "src/tool",
            "../../secrets"
        ));
    }

    #[test]
    fn rejects_unknown_fields() {
        let mut value = serde_json::to_value(sample_snapshot()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_field".into(), json!(true));
        assert!(serde_json::from_value::<WorkspaceSnapshotManifestV1>(value).is_err());
    }
}
