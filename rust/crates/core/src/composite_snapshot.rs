//! Composite Snapshot — a bag-of-references across multiple state dimensions.
//!
//! A checkpoint is not a single blob; it's a collection of **optional** references
//! to different slices of state:
//!
//! - **Session state**: heavy checkpoint (conversation, tools, budget)
//! - **Data snapshot**: MatrixOne snapshot/branch (git4data)
//! - **Memory snapshot**: learning state (PatternLibrary, EntityGraph, calibration)
//! - **Git commit**: workspace code version
//! - **Workspace state**: session workspace metadata
//!
//! Any combination is valid. A quick debug checkpoint might only capture session state,
//! while a tuning experiment anchor captures all five dimensions.
//!
//! The runtime treats data snapshot references as opaque locators — only the
//! data layer (MatrixOne adapter) knows how to materialise or restore from them.
//! The business layer declares bindings via [`SnapshotSpec`].
//!
//! ## Timestamp formats
//!
//! [`DataSnapshotRef::timestamp`] uses ISO 8601 strings because MatrixOne's
//! `SHOW SNAPSHOTS` returns human-readable timestamps; keeping the same format
//! avoids a lossy conversion round-trip.
//!
//! [`MemorySnapshotRef::epoch`] uses `u64` (Unix epoch seconds) because the
//! Memoria persistence layer indexes snapshots by epoch — matching that format
//! keeps lookups zero-cost.

use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

/// A typed reference to one dimension of state at a point in time.
///
/// Each variant carries just enough information to **locate** the state —
/// never the state itself. This keeps the snapshot index lightweight while
/// enabling full rollback/fork/tuning across all dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ref")]
pub enum SnapshotRef {
    /// Session execution state (heavy checkpoint on local disk).
    /// Value: relative path or checkpoint number, e.g. `"000005-heavy.json"`.
    SessionState(String),

    /// MatrixOne data snapshot (for git4data rollback/branch).
    DataSnapshot(DataSnapshotRef),

    /// Learning/memory state (PatternLibrary, EntityGraph, calibration).
    MemorySnapshot(MemorySnapshotRef),

    /// Git commit for the workspace code at this point.
    /// Value: full SHA-1 hash.
    GitCommit(String),

    /// Workspace metadata (session_workspace.yaml).
    /// Value: session_id (workspace can be loaded from session dir).
    WorkspaceState(String),
}

/// Reference to a MatrixOne data-level snapshot.
///
/// The business layer decides *which* databases/tables to snapshot and fills this
/// in. The runtime treats it as an opaque locator — only the data layer
/// (MatrixOne adapter) knows how to `RESTORE ... FROM SNAPSHOT`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataSnapshotRef {
    /// Snapshot name (for `RESTORE ... FROM SNAPSHOT 'name'`).
    pub snapshot_name: String,
    /// Database(s) included in this snapshot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub databases: Vec<String>,
    /// Timestamp (ISO 8601) the snapshot was taken at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
    /// If this was created as part of a data branch, the branch name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch_name: Option<String>,
}

/// Reference to a persisted learning/memory snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MemorySnapshotRef {
    /// Profile name owning this snapshot.
    pub profile: String,
    /// Epoch seconds of the snapshot.
    pub epoch: u64,
    /// Path to the snapshot file (relative to `.astra/`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

/// A composite snapshot — a bag of optional references to different state dimensions.
///
/// Any combination of components is valid:
/// - A quick checkpoint might only have `SessionState`.
/// - A full breakpoint has `SessionState + MemorySnapshot + WorkspaceState`.
/// - A tuning experiment anchor adds `DataSnapshot + GitCommit`.
///
/// Rollback/fork/resume can select which components to restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotDimension {
    SessionState,
    Data,
    Memory,
    Git,
    Workspace,
}

fn ordered_dimensions() -> [SnapshotDimension; 5] {
    [
        SnapshotDimension::SessionState,
        SnapshotDimension::Data,
        SnapshotDimension::Memory,
        SnapshotDimension::Git,
        SnapshotDimension::Workspace,
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeSnapshotIdentity {
    pub snapshot_id: String,
    pub session_id: String,
    pub turn: u32,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default)]
    pub version: u64,
}

impl From<&CompositeSnapshot> for CompositeSnapshotIdentity {
    fn from(snapshot: &CompositeSnapshot) -> Self {
        Self {
            snapshot_id: snapshot.snapshot_id.clone(),
            session_id: snapshot.session_id.clone(),
            turn: snapshot.turn,
            created_at: snapshot.created_at.clone(),
            label: snapshot.label.clone(),
            version: snapshot.version,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotRefChange {
    pub dimension: SnapshotDimension,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<SnapshotRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<SnapshotRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeSnapshotDiff {
    pub from: CompositeSnapshotIdentity,
    pub to: CompositeSnapshotIdentity,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ref_changes: Vec<SnapshotRefChange>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompositeSnapshotError {
    SessionMismatch {
        expected: String,
        found: String,
    },
    VersionConflict {
        expected: u64,
        found: u64,
    },
    ApplySourceMismatch {
        expected_snapshot_id: String,
        found_snapshot_id: String,
    },
    MergeConflict {
        dimension: SnapshotDimension,
        left: Box<SnapshotRef>,
        right: Box<SnapshotRef>,
    },
}

impl std::fmt::Display for CompositeSnapshotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SessionMismatch { expected, found } => {
                write!(
                    f,
                    "snapshot session mismatch: expected {expected}, found {found}"
                )
            }
            Self::VersionConflict { expected, found } => {
                write!(
                    f,
                    "snapshot version conflict: expected next version {expected}, found {found}"
                )
            }
            Self::ApplySourceMismatch {
                expected_snapshot_id,
                found_snapshot_id,
            } => write!(
                f,
                "snapshot diff source mismatch: expected {expected_snapshot_id}, found {found_snapshot_id}"
            ),
            Self::MergeConflict { dimension, .. } => {
                write!(f, "snapshot merge conflict on {dimension:?}")
            }
        }
    }
}

impl std::error::Error for CompositeSnapshotError {}

pub trait StateDiff: Sized {
    type Diff;
    type Error;

    fn diff(&self, target: &Self) -> Self::Diff;
    fn apply(&self, diff: &Self::Diff) -> Result<Self, Self::Error>;
    fn merge(&self, other: &Self) -> Result<Self, Self::Error>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeSnapshot {
    /// Unique identifier.
    pub snapshot_id: String,
    /// Session this snapshot belongs to.
    pub session_id: String,
    /// Turn number at snapshot time.
    pub turn: u32,
    /// ISO 8601 creation timestamp.
    pub created_at: String,
    /// Monotonic per-session version assigned when written to a snapshot index.
    #[serde(default)]
    pub version: u64,
    /// Human-readable label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The set of state references captured at this point.
    pub refs: Vec<SnapshotRef>,
}

impl CompositeSnapshot {
    pub fn session_state(&self) -> Option<&str> {
        self.refs.iter().find_map(|r| match r {
            SnapshotRef::SessionState(s) => Some(s.as_str()),
            _ => None,
        })
    }

    pub fn data_snapshot(&self) -> Option<&DataSnapshotRef> {
        self.refs.iter().find_map(|r| match r {
            SnapshotRef::DataSnapshot(d) => Some(d),
            _ => None,
        })
    }

    pub fn memory_snapshot(&self) -> Option<&MemorySnapshotRef> {
        self.refs.iter().find_map(|r| match r {
            SnapshotRef::MemorySnapshot(m) => Some(m),
            _ => None,
        })
    }

    pub fn git_commit(&self) -> Option<&str> {
        self.refs.iter().find_map(|r| match r {
            SnapshotRef::GitCommit(sha) => Some(sha.as_str()),
            _ => None,
        })
    }

    pub fn workspace_state(&self) -> Option<&str> {
        self.refs.iter().find_map(|r| match r {
            SnapshotRef::WorkspaceState(session_id) => Some(session_id.as_str()),
            _ => None,
        })
    }

    pub fn identity(&self) -> CompositeSnapshotIdentity {
        CompositeSnapshotIdentity::from(self)
    }

    pub fn ref_for_dimension(&self, dimension: SnapshotDimension) -> Option<&SnapshotRef> {
        self.refs.iter().find(|reference| {
            matches!(
                (dimension, reference),
                (
                    SnapshotDimension::SessionState,
                    SnapshotRef::SessionState(_)
                ) | (SnapshotDimension::Data, SnapshotRef::DataSnapshot(_))
                    | (SnapshotDimension::Memory, SnapshotRef::MemorySnapshot(_))
                    | (SnapshotDimension::Git, SnapshotRef::GitCommit(_))
                    | (SnapshotDimension::Workspace, SnapshotRef::WorkspaceState(_))
            )
        })
    }

    pub fn has_session_state(&self) -> bool {
        self.refs
            .iter()
            .any(|r| matches!(r, SnapshotRef::SessionState(_)))
    }

    pub fn has_data_snapshot(&self) -> bool {
        self.refs
            .iter()
            .any(|r| matches!(r, SnapshotRef::DataSnapshot(_)))
    }

    pub fn has_memory_snapshot(&self) -> bool {
        self.refs
            .iter()
            .any(|r| matches!(r, SnapshotRef::MemorySnapshot(_)))
    }

    pub fn has_git_commit(&self) -> bool {
        self.refs
            .iter()
            .any(|r| matches!(r, SnapshotRef::GitCommit(_)))
    }

    /// List which dimensions this snapshot covers (for display).
    pub fn dimensions(&self) -> Vec<&'static str> {
        let mut dims = Vec::new();
        for r in &self.refs {
            match r {
                SnapshotRef::SessionState(_) => dims.push("session"),
                SnapshotRef::DataSnapshot(_) => dims.push("data"),
                SnapshotRef::MemorySnapshot(_) => dims.push("memory"),
                SnapshotRef::GitCommit(_) => dims.push("git"),
                SnapshotRef::WorkspaceState(_) => dims.push("workspace"),
            }
        }
        dims
    }
}

impl StateDiff for CompositeSnapshot {
    type Diff = CompositeSnapshotDiff;
    type Error = CompositeSnapshotError;

    fn diff(&self, target: &Self) -> Self::Diff {
        let ref_changes = ordered_dimensions()
            .into_iter()
            .filter_map(|dimension| {
                let before = self.ref_for_dimension(dimension).cloned();
                let after = target.ref_for_dimension(dimension).cloned();
                (before != after).then_some(SnapshotRefChange {
                    dimension,
                    before,
                    after,
                })
            })
            .collect();
        CompositeSnapshotDiff {
            from: self.identity(),
            to: target.identity(),
            ref_changes,
        }
    }

    fn apply(&self, diff: &Self::Diff) -> Result<Self, Self::Error> {
        if self.snapshot_id != diff.from.snapshot_id {
            return Err(CompositeSnapshotError::ApplySourceMismatch {
                expected_snapshot_id: diff.from.snapshot_id.clone(),
                found_snapshot_id: self.snapshot_id.clone(),
            });
        }
        if self.session_id != diff.from.session_id || self.session_id != diff.to.session_id {
            return Err(CompositeSnapshotError::SessionMismatch {
                expected: diff.from.session_id.clone(),
                found: self.session_id.clone(),
            });
        }

        let refs = ordered_dimensions()
            .into_iter()
            .filter_map(|dimension| {
                diff.ref_changes
                    .iter()
                    .find(|change| change.dimension == dimension)
                    .map(|change| change.after.clone())
                    .unwrap_or_else(|| self.ref_for_dimension(dimension).cloned())
            })
            .collect();

        Ok(CompositeSnapshot {
            snapshot_id: diff.to.snapshot_id.clone(),
            session_id: diff.to.session_id.clone(),
            turn: diff.to.turn,
            created_at: diff.to.created_at.clone(),
            version: diff.to.version,
            label: diff.to.label.clone(),
            refs,
        })
    }

    fn merge(&self, other: &Self) -> Result<Self, Self::Error> {
        if self.session_id != other.session_id {
            return Err(CompositeSnapshotError::SessionMismatch {
                expected: self.session_id.clone(),
                found: other.session_id.clone(),
            });
        }

        let mut refs = Vec::new();
        for dimension in ordered_dimensions() {
            match (
                self.ref_for_dimension(dimension).cloned(),
                other.ref_for_dimension(dimension).cloned(),
            ) {
                (Some(left), Some(right)) if left != right => {
                    return Err(CompositeSnapshotError::MergeConflict {
                        dimension,
                        left: Box::new(left),
                        right: Box::new(right),
                    });
                }
                (Some(reference), _) | (_, Some(reference)) => refs.push(reference),
                (None, None) => {}
            }
        }

        let label = match (&self.label, &other.label) {
            (Some(left), Some(right)) if left == right => Some(left.clone()),
            (Some(left), Some(right)) => Some(format!("merge:{left}|{right}")),
            (Some(label), None) | (None, Some(label)) => Some(label.clone()),
            (None, None) => None,
        };

        Ok(CompositeSnapshot {
            snapshot_id: format!(
                "merge-{}-{}",
                &self.snapshot_id[..8.min(self.snapshot_id.len())],
                &other.snapshot_id[..8.min(other.snapshot_id.len())]
            ),
            session_id: self.session_id.clone(),
            turn: self.turn.max(other.turn),
            created_at: chrono::Utc::now().to_rfc3339(),
            version: self.version.max(other.version).saturating_add(1),
            label,
            refs,
        })
    }
}

/// Index of composite snapshots for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeSnapshotIndex {
    pub snapshots: Vec<CompositeSnapshot>,
}

impl CompositeSnapshotIndex {
    pub fn normalize_versions(&mut self) {
        let mut next_version = 1;
        for snapshot in &mut self.snapshots {
            if snapshot.version == 0 {
                snapshot.version = next_version;
            }
            next_version = snapshot.version.saturating_add(1);
        }
    }

    pub fn current_version(&self) -> u64 {
        self.snapshots
            .iter()
            .enumerate()
            .map(|(index, snapshot)| {
                if snapshot.version == 0 {
                    index as u64 + 1
                } else {
                    snapshot.version
                }
            })
            .max()
            .unwrap_or(0)
    }

    pub fn append(
        &mut self,
        snapshot: &mut CompositeSnapshot,
    ) -> Result<(), CompositeSnapshotError> {
        self.normalize_versions();
        if let Some(existing_session_id) =
            self.snapshots.first().map(|existing| &existing.session_id)
            && existing_session_id != &snapshot.session_id
        {
            return Err(CompositeSnapshotError::SessionMismatch {
                expected: existing_session_id.clone(),
                found: snapshot.session_id.clone(),
            });
        }

        let expected_version = self.current_version().saturating_add(1);
        match snapshot.version {
            0 => snapshot.version = expected_version,
            found if found == expected_version => {}
            found => {
                return Err(CompositeSnapshotError::VersionConflict {
                    expected: expected_version,
                    found,
                });
            }
        }

        self.snapshots.push(snapshot.clone());
        Ok(())
    }
}

/// Specification of which state dimensions to include when creating a composite snapshot.
///
/// The business layer fills this in based on context — a tuning experiment wants
/// data + memory + git, while a quick debug fork only needs session state.
#[derive(Debug, Clone, Default)]
pub struct SnapshotSpec {
    /// Heavy checkpoint reference (e.g. `"000005-heavy.json"`).
    /// When `Some`, a `SessionState` ref is added with this value.
    /// When `None`, session state is omitted.
    pub session_state_ref: Option<String>,
    /// Include data snapshot reference (caller provides snapshot locator).
    pub data_snapshot: Option<DataSnapshotRef>,
    /// Include memory/learning snapshot reference.
    pub memory_snapshot: Option<MemorySnapshotRef>,
    /// Include git commit SHA (caller provides).
    pub git_commit: Option<String>,
    /// Include workspace state reference.
    pub include_workspace: bool,
}

impl SnapshotSpec {
    /// Build a `CompositeSnapshot` from this spec.
    pub fn build(
        &self,
        snapshot_id: String,
        session_id: String,
        turn: u32,
        label: Option<String>,
    ) -> CompositeSnapshot {
        let mut refs = Vec::new();
        if let Some(ref checkpoint_ref) = self.session_state_ref {
            refs.push(SnapshotRef::SessionState(checkpoint_ref.clone()));
        }
        if let Some(ds) = &self.data_snapshot {
            refs.push(SnapshotRef::DataSnapshot(ds.clone()));
        }
        if let Some(ms) = &self.memory_snapshot {
            refs.push(SnapshotRef::MemorySnapshot(ms.clone()));
        }
        if let Some(sha) = &self.git_commit {
            refs.push(SnapshotRef::GitCommit(sha.clone()));
        }
        if self.include_workspace {
            refs.push(SnapshotRef::WorkspaceState(session_id.clone()));
        }
        let created_at = chrono::Utc::now().to_rfc3339();
        CompositeSnapshot {
            snapshot_id,
            session_id,
            turn,
            created_at,
            version: 0,
            label,
            refs,
        }
    }
}

// ─── Data Snapshot Provider Trait ─────────────────────────────────────────────

/// Abstraction for the business/data layer to participate in composite snapshots.
///
/// The runtime knows nothing about MatrixOne snapshots, branches, or databases.
/// Instead, it calls this trait when a composite snapshot is being created or
/// restored, and the implementor decides:
///
/// - **Which databases/tables** belong to the current session context
/// - **How to create a snapshot** (e.g. `CREATE SNAPSHOT ... FOR ACCOUNT`)
/// - **How to restore** from a `DataSnapshotRef` (e.g. `RESTORE ... FROM SNAPSHOT`)
///
/// # Lifecycle
///
/// ```text
/// create_snapshot()  ──→  DataSnapshotRef   ──→  stored in CompositeSnapshot
///                                                  │
///                         restore_snapshot() ◄─────┘  (on rollback/fork/resume)
/// ```
///
/// # Binding Strategy
///
/// The provider decides the binding between session context and data:
///
/// 1. **Session-scoped**: one snapshot per session (simple, broad)
/// 2. **Turn-scoped**: snapshot after each successful turn (fine-grained, expensive)
/// 3. **Explicit**: only snapshot when the user or plan requests it
/// 4. **Task-scoped**: snapshot at plan/subtask boundaries
///
/// The runtime doesn't prescribe the strategy — it just calls the trait.
pub trait DataSnapshotProvider: Send + Sync {
    /// Create a snapshot of the data state relevant to this session.
    ///
    /// Returns a `DataSnapshotRef` that can be stored in a `CompositeSnapshot`.
    /// The `context` provides session metadata so the provider can decide
    /// which databases to include and how to name the snapshot.
    ///
    /// Returning `Ok(None)` means "no data to snapshot" (perfectly valid).
    fn create_snapshot(
        &self,
        context: &SnapshotContext,
    ) -> Pin<Box<dyn Future<Output = Result<Option<DataSnapshotRef>, String>> + Send + '_>>;

    /// Restore data state from a previously created snapshot reference.
    ///
    /// The provider translates the `DataSnapshotRef` back into the
    /// appropriate SQL commands (e.g. `RESTORE ACCOUNT {acc} DATABASE {db} FROM SNAPSHOT`).
    fn restore_snapshot(
        &self,
        snapshot: &DataSnapshotRef,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>>;

    /// Check if a snapshot still exists and is restorable.
    fn snapshot_exists(
        &self,
        snapshot: &DataSnapshotRef,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>>;

    /// List databases that this provider manages for the given session.
    ///
    /// Used to pre-fill `SnapshotSpec.data_snapshot.databases` when
    /// the caller doesn't specify them explicitly.
    fn bound_databases(
        &self,
        session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>>;
}

/// Context passed to `DataSnapshotProvider` when creating a snapshot.
///
/// Carries enough session metadata for the provider to decide *what* to snapshot
/// and *how* to name it.
#[derive(Debug, Clone)]
pub struct SnapshotContext {
    /// Current session ID.
    pub session_id: String,
    /// Current turn number.
    pub turn: u32,
    /// Optional label (e.g. "pre-fork", "tuning-baseline").
    pub label: Option<String>,
    /// Task type hint (helps provider decide scope).
    pub task_type: Option<String>,
    /// Explicit list of databases to include (overrides provider's default).
    pub databases: Option<Vec<String>>,
}

/// A no-op provider for environments without data snapshot support.
///
/// Always returns `None` for snapshots, making the data dimension
/// absent from any `CompositeSnapshot`.
#[cfg(test)]
pub struct NoopDataSnapshotProvider;

#[cfg(test)]
impl DataSnapshotProvider for NoopDataSnapshotProvider {
    fn create_snapshot(
        &self,
        _context: &SnapshotContext,
    ) -> Pin<Box<dyn Future<Output = Result<Option<DataSnapshotRef>, String>> + Send + '_>> {
        Box::pin(async { Ok(None) })
    }

    fn restore_snapshot(
        &self,
        _snapshot: &DataSnapshotRef,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + '_>> {
        Box::pin(async { Ok(()) })
    }

    fn snapshot_exists(
        &self,
        _snapshot: &DataSnapshotRef,
    ) -> Pin<Box<dyn Future<Output = Result<bool, String>> + Send + '_>> {
        Box::pin(async { Ok(false) })
    }

    fn bound_databases(
        &self,
        _session_id: &str,
    ) -> Pin<Box<dyn Future<Output = Vec<String>> + Send + '_>> {
        Box::pin(async { Vec::new() })
    }
}

// ─── Composite Snapshot Builder ──────────────────────────────────────────────

/// Builder for constructing a `CompositeSnapshot` from a mix of
/// synchronous refs and async provider calls.
pub struct CompositeSnapshotBuilder {
    session_id: String,
    turn: u32,
    label: Option<String>,
    refs: Vec<SnapshotRef>,
}

impl CompositeSnapshotBuilder {
    pub fn new(session_id: impl Into<String>, turn: u32) -> Self {
        Self {
            session_id: session_id.into(),
            turn,
            label: None,
            refs: Vec::new(),
        }
    }

    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn session_state(mut self, checkpoint_ref: impl Into<String>) -> Self {
        self.refs
            .push(SnapshotRef::SessionState(checkpoint_ref.into()));
        self
    }

    pub fn memory_snapshot(mut self, ms: MemorySnapshotRef) -> Self {
        self.refs.push(SnapshotRef::MemorySnapshot(ms));
        self
    }

    pub fn git_commit(mut self, sha: impl Into<String>) -> Self {
        self.refs.push(SnapshotRef::GitCommit(sha.into()));
        self
    }

    pub fn workspace_state(mut self, session_id: impl Into<String>) -> Self {
        self.refs
            .push(SnapshotRef::WorkspaceState(session_id.into()));
        self
    }

    pub fn data_snapshot(mut self, ds: DataSnapshotRef) -> Self {
        self.refs.push(SnapshotRef::DataSnapshot(ds));
        self
    }
    /// Build the final `CompositeSnapshot`.
    pub fn build(self) -> CompositeSnapshot {
        let snapshot_id = format!(
            "{}-t{}-{}",
            &self.session_id[..8.min(self.session_id.len())],
            self.turn,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        );
        let created_at = chrono::Utc::now().to_rfc3339();
        CompositeSnapshot {
            snapshot_id,
            session_id: self.session_id,
            turn: self.turn,
            created_at,
            version: 0,
            label: self.label,
            refs: self.refs,
        }
    }
}

// ─── Restore Selector ────────────────────────────────────────────────────────

/// Which dimensions to restore from a `CompositeSnapshot`.
///
/// By default, all present dimensions are restored. Callers can selectively
/// disable dimensions they don't want.
#[derive(Debug, Clone)]
pub struct RestoreSelector {
    pub restore_session_state: bool,
    pub restore_data: bool,
    pub restore_memory: bool,
    pub restore_git: bool,
    pub restore_workspace: bool,
}

impl Default for RestoreSelector {
    fn default() -> Self {
        Self {
            restore_session_state: true,
            restore_data: true,
            restore_memory: true,
            restore_git: true,
            restore_workspace: true,
        }
    }
}

impl RestoreSelector {
    /// Only restore session state (fast, no side effects on data/git).
    pub fn session_only() -> Self {
        Self {
            restore_session_state: true,
            restore_data: false,
            restore_memory: false,
            restore_git: false,
            restore_workspace: false,
        }
    }

    /// Filter a snapshot's refs to only the selected dimensions.
    pub fn filter_refs<'a>(&self, snapshot: &'a CompositeSnapshot) -> Vec<&'a SnapshotRef> {
        snapshot
            .refs
            .iter()
            .filter(|r| match r {
                SnapshotRef::SessionState(_) => self.restore_session_state,
                SnapshotRef::DataSnapshot(_) => self.restore_data,
                SnapshotRef::MemorySnapshot(_) => self.restore_memory,
                SnapshotRef::GitCommit(_) => self.restore_git,
                SnapshotRef::WorkspaceState(_) => self.restore_workspace,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_creates_snapshot_with_all_dimensions() {
        let snap = CompositeSnapshotBuilder::new("session-123", 5)
            .label("test-snapshot")
            .session_state("000005-heavy.json")
            .memory_snapshot(MemorySnapshotRef {
                profile: "default".to_string(),
                epoch: 1700000000,
                path: Some("learning.json".to_string()),
            })
            .git_commit("abc1234def5678")
            .workspace_state("session-123")
            .data_snapshot(DataSnapshotRef {
                snapshot_name: "snap_t5".to_string(),
                databases: vec!["mydb".to_string()],
                timestamp: Some("2026-01-01T00:00:00Z".to_string()),
                branch_name: None,
            })
            .build();

        assert_eq!(snap.session_id, "session-123");
        assert_eq!(snap.turn, 5);
        assert_eq!(snap.version, 0);
        assert_eq!(snap.label.as_deref(), Some("test-snapshot"));
        assert_eq!(snap.refs.len(), 5);

        assert!(snap.has_session_state());
        assert!(snap.has_data_snapshot());
        assert!(snap.has_memory_snapshot());
        assert!(snap.has_git_commit());

        assert_eq!(snap.session_state(), Some("000005-heavy.json"));
        assert_eq!(snap.git_commit(), Some("abc1234def5678"));
        assert_eq!(snap.data_snapshot().unwrap().snapshot_name, "snap_t5");
        assert_eq!(snap.memory_snapshot().unwrap().epoch, 1700000000);
    }

    #[test]
    fn builder_partial_dimensions() {
        let snap = CompositeSnapshotBuilder::new("s1", 0)
            .session_state("000000-heavy.json")
            .build();

        assert!(snap.has_session_state());
        assert!(!snap.has_data_snapshot());
        assert!(!snap.has_memory_snapshot());
        assert!(!snap.has_git_commit());
        assert_eq!(snap.refs.len(), 1);
    }

    #[test]
    fn dimensions_lists_present_dimensions() {
        let snap = CompositeSnapshotBuilder::new("s1", 1)
            .session_state("cp")
            .git_commit("abc")
            .build();

        let dims = snap.dimensions();
        assert_eq!(dims, vec!["session", "git"]);
    }

    #[test]
    fn snapshot_spec_builds_correctly() {
        let spec = SnapshotSpec {
            session_state_ref: Some("000003-heavy.json".to_string()),
            data_snapshot: Some(DataSnapshotRef {
                snapshot_name: "snap1".to_string(),
                databases: vec![],
                timestamp: None,
                branch_name: None,
            }),
            memory_snapshot: None,
            git_commit: Some("deadbeef".to_string()),
            include_workspace: false,
        };
        let snap = spec.build("id1".into(), "sess1".into(), 3, Some("test".into()));
        assert_eq!(snap.snapshot_id, "id1");
        assert_eq!(snap.session_id, "sess1");
        assert_eq!(snap.turn, 3);
        assert_eq!(snap.version, 0);
        assert_eq!(snap.refs.len(), 3); // session + data + git
        assert!(snap.has_session_state());
        assert_eq!(snap.session_state(), Some("000003-heavy.json"));
        assert!(snap.has_data_snapshot());
        assert!(snap.has_git_commit());
        assert!(!snap.has_memory_snapshot());
    }

    #[test]
    fn restore_selector_session_only() {
        let snap = CompositeSnapshotBuilder::new("s1", 1)
            .session_state("cp")
            .git_commit("abc")
            .data_snapshot(DataSnapshotRef {
                snapshot_name: "sn".to_string(),
                databases: vec![],
                timestamp: None,
                branch_name: None,
            })
            .build();

        let selector = RestoreSelector::session_only();
        let filtered = selector.filter_refs(&snap);
        assert_eq!(filtered.len(), 1);
        assert!(matches!(filtered[0], SnapshotRef::SessionState(_)));
    }

    #[test]
    fn restore_selector_default_returns_all() {
        let snap = CompositeSnapshotBuilder::new("s1", 1)
            .session_state("cp")
            .git_commit("abc")
            .build();

        let selector = RestoreSelector::default();
        let filtered = selector.filter_refs(&snap);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn serde_roundtrip() {
        let snap = CompositeSnapshotBuilder::new("s1", 2)
            .label("round-trip")
            .session_state("000002-heavy.json")
            .git_commit("cafe1234")
            .data_snapshot(DataSnapshotRef {
                snapshot_name: "snap_test".to_string(),
                databases: vec!["db1".to_string(), "db2".to_string()],
                timestamp: Some("2026-04-01T00:00:00Z".to_string()),
                branch_name: Some("branch_x".to_string()),
            })
            .memory_snapshot(MemorySnapshotRef {
                profile: "prod".to_string(),
                epoch: 1711929600,
                path: None,
            })
            .build();

        let json = serde_json::to_string(&snap).expect("serialize");
        let deser: CompositeSnapshot = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deser.snapshot_id, snap.snapshot_id);
        assert_eq!(deser.session_id, "s1");
        assert_eq!(deser.turn, 2);
        assert_eq!(deser.version, 0);
        assert_eq!(deser.refs.len(), 4);
        assert!(deser.has_session_state());
        assert!(deser.has_data_snapshot());
        assert!(deser.has_memory_snapshot());
        assert!(deser.has_git_commit());
        assert_eq!(deser.data_snapshot().unwrap().databases, vec!["db1", "db2"]);
        assert_eq!(
            deser.data_snapshot().unwrap().branch_name.as_deref(),
            Some("branch_x")
        );
    }

    #[test]
    fn composite_snapshot_index_serde() {
        let index = CompositeSnapshotIndex {
            snapshots: vec![
                CompositeSnapshotBuilder::new("s1", 1)
                    .session_state("a")
                    .build(),
                CompositeSnapshotBuilder::new("s1", 2)
                    .session_state("b")
                    .git_commit("x")
                    .build(),
            ],
        };
        let json = serde_json::to_string_pretty(&index).unwrap();
        let deser: CompositeSnapshotIndex = serde_json::from_str(&json).unwrap();
        assert_eq!(deser.snapshots.len(), 2);
        assert_eq!(deser.snapshots[1].refs.len(), 2);
    }

    #[test]
    fn index_append_assigns_monotonic_versions() {
        let mut index = CompositeSnapshotIndex {
            snapshots: vec![
                CompositeSnapshotBuilder::new("s1", 1)
                    .session_state("a")
                    .build(),
                CompositeSnapshotBuilder::new("s1", 2)
                    .session_state("b")
                    .build(),
            ],
        };
        let mut next = CompositeSnapshotBuilder::new("s1", 3)
            .session_state("c")
            .build();

        index.append(&mut next).unwrap();

        assert_eq!(index.snapshots[0].version, 1);
        assert_eq!(index.snapshots[1].version, 2);
        assert_eq!(next.version, 3);
        assert_eq!(index.snapshots[2].version, 3);
    }

    #[test]
    fn diff_and_apply_recreate_target_snapshot() {
        let mut base = CompositeSnapshotBuilder::new("s1", 1)
            .session_state("000001-heavy.json")
            .workspace_state("s1")
            .build();
        base.snapshot_id = "snap-a".into();
        base.created_at = "2026-04-12T00:00:00Z".into();
        base.version = 1;

        let mut target = CompositeSnapshotBuilder::new("s1", 2)
            .session_state("000002-heavy.json")
            .git_commit("deadbeef")
            .workspace_state("s1")
            .build();
        target.snapshot_id = "snap-b".into();
        target.created_at = "2026-04-12T00:01:00Z".into();
        target.label = Some("next".into());
        target.version = 2;

        let diff = base.diff(&target);
        assert_eq!(diff.ref_changes.len(), 2);

        let applied = base.apply(&diff).unwrap();
        assert_eq!(applied, target);
    }

    #[test]
    fn merge_combines_non_conflicting_dimensions() {
        let mut left = CompositeSnapshotBuilder::new("s1", 1)
            .session_state("000001-heavy.json")
            .build();
        left.snapshot_id = "left".into();
        left.version = 2;

        let mut right = CompositeSnapshotBuilder::new("s1", 3)
            .git_commit("deadbeef")
            .workspace_state("s1")
            .build();
        right.snapshot_id = "right".into();
        right.version = 4;

        let merged = left.merge(&right).unwrap();
        assert_eq!(merged.session_state(), Some("000001-heavy.json"));
        assert_eq!(merged.git_commit(), Some("deadbeef"));
        assert_eq!(merged.workspace_state(), Some("s1"));
        assert_eq!(merged.version, 5);
    }

    #[test]
    fn merge_rejects_conflicting_dimensions() {
        let mut left = CompositeSnapshotBuilder::new("s1", 1)
            .git_commit("abc")
            .build();
        left.snapshot_id = "left".into();
        let mut right = CompositeSnapshotBuilder::new("s1", 1)
            .git_commit("def")
            .build();
        right.snapshot_id = "right".into();

        let err = left.merge(&right).unwrap_err();
        assert!(matches!(
            err,
            CompositeSnapshotError::MergeConflict {
                dimension: SnapshotDimension::Git,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn noop_provider_returns_none() {
        let provider = NoopDataSnapshotProvider;
        let ctx = SnapshotContext {
            session_id: "s1".into(),
            turn: 1,
            label: None,
            task_type: None,
            databases: None,
        };
        let result = provider.create_snapshot(&ctx).await.unwrap();
        assert!(result.is_none());
        assert!(
            !provider
                .snapshot_exists(&DataSnapshotRef {
                    snapshot_name: "x".into(),
                    databases: vec![],
                    timestamp: None,
                    branch_name: None,
                })
                .await
                .unwrap()
        );
    }
}
