//! Composite Snapshot — a bag-of-references across multiple state dimensions.
//!
//! A checkpoint is not a single blob; it's a collection of **optional** references
//! to different slices of state:
//!
//! - **Session state**: heavy checkpoint (conversation, tools, budget)
//! - **Data snapshot**: MatrixOne snapshot/branch (git4data)
//! - **Memory snapshot**: learning state (tool health + tool quality persistence)
//! - **Git commit**: workspace code version
//! - **Workspace state**: session workspace metadata
//!
//! Any combination is valid. A quick debug checkpoint might only capture session state,
//! while a tuning experiment anchor captures all five dimensions.
//!
//! The runtime treats data snapshot references as opaque locators — only the
//! data layer (MatrixOne adapter) knows how to materialise or restore from them.
//! Callers construct reference bundles with [`CompositeSnapshotBuilder`];
//! this module does not execute data or Git restoration.

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

/// A typed reference to one dimension of state at a point in time.
///
/// Each variant carries just enough information to **locate** the state —
/// never the state itself. This keeps the snapshot index lightweight while
/// enabling full rollback, fork, and runtime adaptation across all dimensions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "ref")]
pub enum SnapshotRef {
    /// Session execution state (heavy checkpoint on local disk).
    /// Value: relative path or checkpoint number, e.g. `"000005-heavy.json"`.
    SessionState(String),

    /// MatrixOne data snapshot (for git4data rollback/branch).
    DataSnapshot(DataSnapshotRef),

    /// Learning/memory state (tool health + tool quality persistence).
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
    SessionMismatch { expected: String, found: String },
    VersionConflict { expected: u64, found: u64 },
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
        }
    }
}

impl std::error::Error for CompositeSnapshotError {}

pub trait StateDiff: Sized {
    type Diff;

    fn diff(&self, target: &Self) -> Self::Diff;
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
}

/// Index of composite snapshots for a session.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeSnapshotIndex {
    pub snapshots: Vec<CompositeSnapshot>,
}

impl CompositeSnapshotIndex {
    /// Merge independently produced indexes by durable snapshot identity.
    ///
    /// Incoming entries win only when both sides describe the same snapshot.
    /// Versions are then rebuilt deterministically so concurrent writers that
    /// both allocated the same next version remain addressable instead of one
    /// silently replacing the other.
    pub fn merge_by_identity(mut self, incoming: Self) -> Self {
        let mut merged = std::collections::BTreeMap::new();
        for snapshot in self.snapshots.drain(..) {
            merged.insert(snapshot.snapshot_id.clone(), snapshot);
        }
        for snapshot in incoming.snapshots {
            merged.insert(snapshot.snapshot_id.clone(), snapshot);
        }
        let mut snapshots: Vec<_> = merged.into_values().collect();
        snapshots.sort_by(|left, right| {
            (
                left.version == 0,
                left.version,
                left.created_at.as_str(),
                left.snapshot_id.as_str(),
            )
                .cmp(&(
                    right.version == 0,
                    right.version,
                    right.created_at.as_str(),
                    right.snapshot_id.as_str(),
                ))
        });
        for (offset, snapshot) in snapshots.iter_mut().enumerate() {
            snapshot.version = u64::try_from(offset).unwrap_or(u64::MAX).saturating_add(1);
        }
        Self { snapshots }
    }

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

// ─── Composite Snapshot Builder ──────────────────────────────────────────────

/// Builder for constructing a `CompositeSnapshot` from opaque state references.
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
            uuid::Uuid::now_v7()
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
    fn builder_identity_is_unique_for_same_session_turn_and_clock_tick() {
        let first = CompositeSnapshotBuilder::new("same-session", 7).build();
        let second = CompositeSnapshotBuilder::new("same-session", 7).build();

        assert_ne!(first.snapshot_id, second.snapshot_id);
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
    fn index_merge_preserves_concurrent_next_versions() {
        let mut base = CompositeSnapshotBuilder::new("s1", 1)
            .session_state("base")
            .build();
        base.snapshot_id = "base".into();
        base.created_at = "2026-08-08T00:00:00Z".into();
        base.version = 1;
        let mut left = CompositeSnapshotBuilder::new("s1", 2)
            .session_state("left")
            .build();
        left.snapshot_id = "left".into();
        left.created_at = "2026-08-08T00:00:01Z".into();
        left.version = 2;
        let mut right = CompositeSnapshotBuilder::new("s1", 2)
            .session_state("right")
            .build();
        right.snapshot_id = "right".into();
        right.created_at = "2026-08-08T00:00:02Z".into();
        right.version = 2;

        let merged = CompositeSnapshotIndex {
            snapshots: vec![base.clone(), left],
        }
        .merge_by_identity(CompositeSnapshotIndex {
            snapshots: vec![base, right],
        });

        assert_eq!(
            merged
                .snapshots
                .iter()
                .map(|snapshot| snapshot.snapshot_id.as_str())
                .collect::<Vec<_>>(),
            vec!["base", "left", "right"]
        );
        assert_eq!(
            merged
                .snapshots
                .iter()
                .map(|snapshot| snapshot.version)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
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
    fn diff_identifies_target_snapshot_and_changed_dimensions() {
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

        assert_eq!(diff.from, base.identity());
        assert_eq!(diff.to, target.identity());
    }
}
