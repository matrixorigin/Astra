//! Fork a local session journal + workspace for experimentation, multi-agent branches, or cloud sync lineage.
//!
//! Produces a new session id, writes `session_fork` + `session_start` + copied events (excluding the
//! parent's `session_start` / `session_end`), and a new `workspace.yaml` with parent linkage.
//!
//! ## git4data extensions
//!
//! Layers 5.1–5.4 add data-branching, multi-session exploration, tuning experiments,
//! and cross-branch learning aggregation on top of the base fork primitive.

use crate::session_journal::{
    JournalEvent, JournalEventType, JournalWriter, SessionLineage, journal_file_path, read_journal,
};
use crate::session_workspace::{self, WorkspaceMetadata};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Layer 5.1 — Session Fork with Data Branching
// ---------------------------------------------------------------------------

/// Options for coupling a session fork with a MatrixOne data branch.
#[derive(Debug, Clone, Default)]
pub struct DataBranchOptions {
    /// Whether to create a corresponding data branch.
    pub create_data_branch: bool,
    /// Source database to branch from (uses platform DB if None).
    pub source_db: Option<String>,
    /// Optional snapshot name to branch from.
    pub from_snapshot: Option<String>,
}

/// Options for [`fork_local_session`].
#[derive(Debug, Clone)]
pub struct ForkSessionOptions {
    pub parent_session_id: String,
    /// When `None`, a new UUID v4 is generated.
    pub new_session_id: Option<String>,
    pub label: Option<String>,
    /// Explicit turn to fork after. When `None`, forks from the latest turn
    /// (derived from workspace metadata or journal turn count).
    pub forked_after_turn: Option<u32>,
    /// When set with `create_data_branch: true`, the fork result will contain a
    /// generated data branch name. The caller is responsible for issuing
    /// `CREATE DATABASE ... FROM ... WITH SNAPSHOT` against MatrixOne.
    pub data_branch: Option<DataBranchOptions>,
    /// When set, a `CompositeSnapshot` is built at the fork point.
    /// Each filled field produces a `SnapshotRef` in the snapshot.
    #[allow(dead_code)]
    pub snapshot_spec: Option<SnapshotSpec>,
}

/// Result of a successful fork.
#[derive(Debug, Clone)]
pub struct ForkSessionResult {
    pub new_session_id: String,
    /// Turn-like events copied from parent (excludes synthetic fork/start lines).
    pub events_copied: usize,
    /// Data-branch name generated when `DataBranchOptions::create_data_branch` was set.
    pub data_branch_name: Option<String>,
    /// Composite snapshot at the fork point — references to all state dimensions
    /// that were captured. The caller can use this to restore any subset.
    pub fork_snapshot: Option<CompositeSnapshot>,
}

pub use astra_core::composite_snapshot::{
    CompositeSnapshot, DataSnapshotRef, MemorySnapshotRef, SnapshotRef, SnapshotSpec,
};

/// Fork parent journal into a new session file and workspace metadata.
///
/// Fails if the target journal path already exists or the parent journal is empty.
pub fn fork_local_session(opts: ForkSessionOptions) -> Result<ForkSessionResult, String> {
    let parent = opts.parent_session_id.trim().to_string();
    if parent.is_empty() {
        return Err("parent_session_id is empty".into());
    }

    let new_id = opts
        .new_session_id
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

    let dest = journal_file_path(&new_id);
    if dest.exists() {
        return Err(format!(
            "refusing to fork: journal file already exists for session {new_id}"
        ));
    }

    let events = read_journal(&parent).map_err(|e| e.to_string())?;
    if events.is_empty() {
        return Err(format!("parent session {parent} has no journal events"));
    }

    let model = events
        .iter()
        .find_map(|e| {
            (e.event_type == JournalEventType::SessionStart)
                .then_some(e.model.clone())
                .flatten()
        })
        .or_else(|| events.iter().find_map(|e| e.model.clone()));

    let max_turn = session_workspace::read_workspace(&parent)
        .map(|w| w.turn_count)
        .unwrap_or_else(|_| {
            events
                .iter()
                .filter(|e| e.event_type == JournalEventType::Turn)
                .count() as u32
        });
    let forked_at_turn = match opts.forked_after_turn {
        Some(t) if t <= max_turn => t,
        Some(t) => {
            return Err(format!(
                "forked_after_turn ({t}) exceeds session turn count ({max_turn})"
            ));
        }
        None => max_turn,
    };

    let lineage = SessionLineage {
        parent_session_id: parent.clone(),
        forked_after_turn: Some(forked_at_turn),
        label: opts.label.clone(),
    };

    let fork_evt = JournalEvent::session_fork(
        Some(new_id.as_str()),
        lineage.clone(),
        opts.label.as_deref(),
    );

    let mut start = JournalEvent::session_start(Some(new_id.as_str()), model.as_deref());
    start.session_lineage = Some(lineage);

    let mut out: Vec<JournalEvent> = vec![fork_evt, start];
    let mut copied = 0usize;

    let mut turn_seen = 0u32;
    for mut evt in events {
        if matches!(
            evt.event_type,
            JournalEventType::SessionStart | JournalEventType::SessionEnd
        ) {
            continue;
        }
        if evt.event_type == JournalEventType::Turn {
            turn_seen += 1;
            if turn_seen > forked_at_turn {
                break;
            }
        }
        evt.session_id = Some(new_id.clone());
        out.push(evt);
        copied += 1;
    }

    let writer = JournalWriter::new(&new_id).map_err(|e| e.to_string())?;
    for evt in &out {
        writer.append(evt).map_err(|e| e.to_string())?;
    }

    let mut ws = session_workspace::read_workspace(&parent)
        .unwrap_or_else(|_| WorkspaceMetadata::new(&parent, model.as_deref().unwrap_or("default")));
    ws.session_id = new_id.clone();
    ws.parent_session_id = Some(parent.clone());
    ws.fork_note = opts.label.clone();
    ws.forked_at_turn = Some(forked_at_turn);
    // Carry forward an existing correlation id, else use parent session id as chain root for multi-agent / audit.
    ws.correlation_id = session_workspace::read_workspace(&parent)
        .ok()
        .and_then(|w| w.correlation_id.clone())
        .or_else(|| Some(parent.clone()));
    ws.turn_count = forked_at_turn;
    ws.agent_role = None;
    let now = chrono::Utc::now().to_rfc3339();
    ws.created_at = now.clone();
    ws.updated_at = now;
    ws.status = "active".to_string();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;

    let data_branch_name = opts
        .data_branch
        .filter(|db| db.create_data_branch)
        .map(|_| format!("session_fork_{new_id}"));

    let fork_snapshot = opts.snapshot_spec.map(|spec| {
        spec.build(
            uuid::Uuid::new_v4().to_string(),
            new_id.clone(),
            forked_at_turn,
            Some(format!("fork from {parent} at turn {forked_at_turn}")),
        )
    });

    Ok(ForkSessionResult {
        new_session_id: new_id,
        events_copied: copied,
        data_branch_name,
        fork_snapshot,
    })
}

// ---------------------------------------------------------------------------
// Layer 5.2 — Multi-Session Exploration
// ---------------------------------------------------------------------------

/// Options for multi-session parallel exploration.
#[derive(Debug, Clone)]
pub struct ExploreOptions {
    /// Parent session to fork from.
    pub parent_session_id: String,
    /// Turn to fork from (current turn if None).
    pub fork_after_turn: Option<u32>,
    /// Number of parallel exploration branches to create.
    pub branch_count: usize,
    /// Label prefix for branches.
    pub label_prefix: String,
    /// Whether to create data branches for each exploration.
    pub with_data_branches: bool,
}

/// Result of creating exploration branches.
#[derive(Debug, Clone)]
pub struct ExploreResult {
    /// Created session IDs for each branch.
    pub branch_session_ids: Vec<String>,
    /// Data branch names (if data branching was requested).
    pub data_branch_names: Vec<String>,
    /// Events copied per branch.
    pub events_copied: Vec<usize>,
    /// Composite snapshots at each branch's fork point.
    pub fork_snapshots: Vec<Option<CompositeSnapshot>>,
}

/// Error from [`create_exploration_branches`] when one branch fails mid-sweep.
#[derive(Debug, Clone)]
pub enum ExploreError {
    /// A fork failed before any branch was created (e.g. invalid parent).
    Setup(String),
    /// Some branches were created before a fork failed.
    /// The caller is responsible for cleaning up `created`.
    PartialFailure {
        created: Box<ExploreResult>,
        failed_branch: usize,
        total_branches: usize,
        error: String,
    },
}

impl std::fmt::Display for ExploreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Setup(e) => write!(f, "exploration setup failed: {e}"),
            Self::PartialFailure {
                created,
                failed_branch,
                total_branches,
                error,
            } => write!(
                f,
                "branch {failed_branch}/{total_branches} failed: {error} \
                 ({} branches already created: [{}])",
                created.as_ref().branch_session_ids.len(),
                created.as_ref().branch_session_ids.join(", "),
            ),
        }
    }
}

impl std::error::Error for ExploreError {}

impl ExploreError {
    /// Extract successfully created branches for cleanup, regardless of variant.
    pub fn into_partial_result(self) -> Option<ExploreResult> {
        match self {
            Self::Setup(_) => None,
            Self::PartialFailure { created, .. } => Some(*created),
        }
    }
}

/// Fork the parent session into `branch_count` parallel exploration branches.
///
/// Each branch gets a label like `{label_prefix}-1`, `{label_prefix}-2`, etc.
/// When `with_data_branches` is true, every fork also produces a data branch name
/// that the caller can materialise via `CREATE DATABASE … FROM … WITH SNAPSHOT`.
pub fn create_exploration_branches(opts: &ExploreOptions) -> Result<ExploreResult, ExploreError> {
    let mut result = ExploreResult {
        branch_session_ids: Vec::new(),
        data_branch_names: Vec::new(),
        events_copied: Vec::new(),
        fork_snapshots: Vec::new(),
    };

    for i in 0..opts.branch_count {
        let label = format!("{}-{}", opts.label_prefix, i + 1);
        let fork_opts = ForkSessionOptions {
            parent_session_id: opts.parent_session_id.clone(),
            new_session_id: None,
            label: Some(label),
            forked_after_turn: opts.fork_after_turn,
            data_branch: if opts.with_data_branches {
                Some(DataBranchOptions {
                    create_data_branch: true,
                    ..Default::default()
                })
            } else {
                None
            },
            snapshot_spec: None,
        };

        match fork_local_session(fork_opts) {
            Ok(fork_result) => {
                result.branch_session_ids.push(fork_result.new_session_id);
                if let Some(db_name) = fork_result.data_branch_name {
                    result.data_branch_names.push(db_name);
                }
                result.events_copied.push(fork_result.events_copied);
                result.fork_snapshots.push(fork_result.fork_snapshot);
            }
            Err(e) => {
                let err = if result.branch_session_ids.is_empty() {
                    ExploreError::Setup(e)
                } else {
                    ExploreError::PartialFailure {
                        created: Box::new(result),
                        failed_branch: i + 1,
                        total_branches: opts.branch_count,
                        error: e,
                    }
                };
                return Err(err);
            }
        }
    }

    Ok(result)
}

// ---------------------------------------------------------------------------
// Layer 5.3 — Tuning Experiments
// ---------------------------------------------------------------------------

/// Configuration for parameter tuning via branched experiments.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneConfig {
    /// Parameter name being tuned.
    pub parameter: String,
    /// Values to test.
    pub values: Vec<serde_json::Value>,
    /// Number of golden sessions to replay per value.
    pub golden_session_count: usize,
    /// Quality metric to optimize.
    pub metric: String,
}

/// Result of a single tuning experiment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneExperimentResult {
    /// Parameter value tested.
    pub value: serde_json::Value,
    /// Quality metric achieved.
    pub metric_value: f64,
    /// Session ID of the experiment branch.
    pub branch_session_id: String,
    /// Optional data branch name.
    pub data_branch_name: Option<String>,
    /// Composite snapshot at the start of the experiment (if one was taken).
    #[serde(default)]
    pub baseline_snapshot: Option<CompositeSnapshot>,
}

/// Aggregate result of a tuning sweep.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TuneSweepResult {
    pub config: TuneConfig,
    pub results: Vec<TuneExperimentResult>,
    /// Best parameter value found.
    pub best_value: Option<serde_json::Value>,
    /// Best metric achieved.
    pub best_metric: Option<f64>,
}

// ---------------------------------------------------------------------------
// Layer 5.4 — Cross-Branch Learning Aggregation
// ---------------------------------------------------------------------------

/// Aggregate learning outcomes across exploration branches.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrossBranchLearning {
    /// Branch session IDs that were analyzed.
    pub branch_ids: Vec<String>,
    /// Tools that succeeded consistently across branches.
    pub consistently_successful_tools: Vec<String>,
    /// Tools that failed consistently across branches.
    pub consistently_failing_tools: Vec<String>,
    /// Patterns that emerged across branches.
    pub common_patterns: Vec<String>,
    /// Recommended winning branch (highest quality).
    pub recommended_branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::{
        JournalEvent, JournalEventType, JournalWriter, journal_file_path, read_journal,
    };
    use crate::session_workspace;

    /// Create a test session with N turns in its journal + workspace.
    fn setup_test_session(session_id: &str, num_turns: u32) {
        let writer = JournalWriter::new(session_id).expect("create journal writer");
        let start = JournalEvent::session_start(Some(session_id), Some("test-model"));
        writer.append(&start).unwrap();
        for t in 1..=num_turns {
            let turn = JournalEvent::turn(
                Some(session_id),
                t,
                Some("test-model"),
                &format!("turn-{t} input"),
                &format!("turn-{t} output"),
                0,
                0,
                0,
                0,
            );
            writer.append(&turn).unwrap();
        }
        let mut ws = session_workspace::WorkspaceMetadata::new(session_id, "test-model");
        ws.turn_count = num_turns;
        session_workspace::write_workspace(&ws).unwrap();
    }

    /// Cleanup test session files (journal + workspace dir under `~/.astra/sessions`).
    fn cleanup_session(session_id: &str) {
        let _ = std::fs::remove_file(journal_file_path(session_id));
        let ws_dir = session_workspace::workspace_dir_for(session_id);
        let _ = std::fs::remove_dir_all(&ws_dir);
    }

    #[test]
    fn fork_none_uses_latest_turn() {
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 5);

        let opts = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: Some("test-fork-latest".to_string()),
            forked_after_turn: None,
            data_branch: None,
            snapshot_spec: None,
        };
        let result = fork_local_session(opts).expect("fork should succeed");
        assert_eq!(result.events_copied, 5, "all turn events should be copied");

        let forked_events = read_journal(&result.new_session_id).unwrap();
        let turn_events: Vec<_> = forked_events
            .iter()
            .filter(|e| e.event_type == JournalEventType::Turn)
            .collect();
        assert_eq!(turn_events.len(), 5);

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }

    #[test]
    fn fork_at_turn_3_truncates_history() {
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 5);

        let opts = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: Some("test-fork-t3".to_string()),
            forked_after_turn: Some(3),
            data_branch: None,
            snapshot_spec: None,
        };
        let result = fork_local_session(opts).expect("fork should succeed");

        let forked_events = read_journal(&result.new_session_id).unwrap();
        let turn_events: Vec<_> = forked_events
            .iter()
            .filter(|e| e.event_type == JournalEventType::Turn)
            .collect();
        assert_eq!(turn_events.len(), 3, "only turns 1-3 should be in fork");
        assert!(turn_events.iter().all(|e| e.turn.unwrap_or(0) <= 3));

        let ws = session_workspace::read_workspace(&result.new_session_id).unwrap();
        assert_eq!(ws.turn_count, 3);

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }

    #[test]
    fn fork_at_turn_0_creates_empty_session() {
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 3);

        let opts = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: None,
            forked_after_turn: Some(0),
            data_branch: None,
            snapshot_spec: None,
        };
        let result = fork_local_session(opts).expect("fork should succeed");

        let forked_events = read_journal(&result.new_session_id).unwrap();
        let turn_events: Vec<_> = forked_events
            .iter()
            .filter(|e| e.event_type == JournalEventType::Turn)
            .collect();
        assert_eq!(turn_events.len(), 0, "no turns should be copied at turn 0");

        let ws = session_workspace::read_workspace(&result.new_session_id).unwrap();
        assert_eq!(ws.turn_count, 0);

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }

    #[test]
    fn fork_beyond_max_turn_returns_error() {
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 3);

        let opts = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: None,
            forked_after_turn: Some(10),
            data_branch: None,
            snapshot_spec: None,
        };
        let result = fork_local_session(opts);
        assert!(result.is_err(), "should reject turn > max");
        let err = result.unwrap_err();
        assert!(
            err.contains("exceeds"),
            "error should mention exceeding: {err}"
        );

        cleanup_session(&parent_id);
    }

    #[test]
    fn fork_at_max_turn_equals_none() {
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 4);

        let opts_explicit = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: Some("explicit-4".to_string()),
            forked_after_turn: Some(4),
            data_branch: None,
            snapshot_spec: None,
        };
        let r1 = fork_local_session(opts_explicit).expect("fork at max should work");

        let opts_none = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: Some("none-latest".to_string()),
            forked_after_turn: None,
            data_branch: None,
            snapshot_spec: None,
        };
        let r2 = fork_local_session(opts_none).expect("fork at None should work");

        assert_eq!(
            r1.events_copied, r2.events_copied,
            "forked_after_turn=Some(max) and None should copy same events"
        );

        cleanup_session(&parent_id);
        cleanup_session(&r1.new_session_id);
        cleanup_session(&r2.new_session_id);
    }

    #[test]
    fn fork_preserves_lineage_with_correct_turn() {
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 5);

        let opts = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: Some("lineage-test".to_string()),
            forked_after_turn: Some(2),
            data_branch: None,
            snapshot_spec: None,
        };
        let result = fork_local_session(opts).unwrap();

        let events = read_journal(&result.new_session_id).unwrap();
        let fork_event = events
            .iter()
            .find(|e| e.event_type == JournalEventType::SessionFork)
            .expect("fork event should exist");
        let lineage = fork_event.session_lineage.as_ref().expect("lineage");
        assert_eq!(lineage.parent_session_id, parent_id);
        assert_eq!(lineage.forked_after_turn, Some(2));

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }
}
