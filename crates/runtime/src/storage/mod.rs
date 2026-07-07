//! Storage type definitions for sandbox/edge workspace configuration.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// MountType — how storage attaches to the execution environment
// ---------------------------------------------------------------------------

/// How a volume is mounted into an execution environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MountType {
    /// Bind mount (local path → sandbox path).
    Bind,
    /// Network file system mount.
    Nfs,
    /// S3-backed FUSE mount.
    S3,
}

// ---------------------------------------------------------------------------
// StorageAccess — what a provider can reach
// ---------------------------------------------------------------------------

/// Describes a storage volume a provider can access.
///
/// This drives storage-aware routing: when a `ToolRequest` specifies
/// a storage requirement, the registry filters to providers that can
/// reach the requested path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StorageAccess {
    /// Path inside the execution environment where the volume is mounted.
    pub mount_path: String,
    /// How the volume is mounted.
    pub mount_type: MountType,
    /// Whether the volume is read-only from the provider's perspective.
    pub read_only: bool,
}

// ---------------------------------------------------------------------------
// WorkspaceSource — where the user's workspace comes from
// ---------------------------------------------------------------------------

/// How the workspace is provided to an execution environment.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceSource {
    /// Pre-provisioned user volume attached at startup.
    UserVolume,
    /// Git clone from a remote repository.
    GitClone {
        /// Clone URL.
        url: String,
    },
    /// Uploaded tarball / project archive.
    Upload,
}
