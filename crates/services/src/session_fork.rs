//! Fork a local session journal + workspace for experimentation, multi-agent branches, or cloud sync lineage.
//!
//! Produces a new session id, writes `session_fork` + `session_start` + copied events (excluding the
//! parent's `session_start` / `session_end`), and a new `workspace.yaml` with parent linkage.
//!
//! ## git4data extensions
//!
//! Layers 5.1–5.4 add data-branching, multi-session exploration, tuning experiments,
//! and cross-branch learning aggregation on top of the base fork primitive.

use crate::SessionArtifactStore;
use crate::session_journal::{
    JournalEvent, JournalEventType, JournalWriter, SessionLineage, journal_file_path, read_journal,
    validate_session_id,
};
use crate::session_workspace::{self, WorkspaceMetadata};
use sha2::{Digest, Sha256};
use std::{
    ffi::{OsStr, OsString},
    io::Write,
    path::PathBuf,
};

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
    pub snapshot_spec: Option<SnapshotSpec>,
}

/// Result of a successful fork.
#[derive(Debug, Clone)]
pub struct ForkSessionResult {
    pub new_session_id: String,
    /// Turn-like events copied from parent (excludes synthetic fork/start lines).
    pub events_copied: usize,
    /// The actual turn number the session was forked after.
    pub forked_at_turn: u32,
    /// Data-branch name generated when `DataBranchOptions::create_data_branch` was set.
    pub data_branch_name: Option<String>,
    /// Composite snapshot at the fork point — references to all state dimensions
    /// that were captured. The caller can use this to restore any subset.
    pub fork_snapshot: Option<CompositeSnapshot>,
    /// Frozen, content-addressed state evidence at the fork cursor. Missing
    /// dimensions are explicit gaps rather than silent partial recovery.
    pub state_manifest: astra_sync_protocol::SessionStateManifestV1,
}

pub use astra_core::composite_snapshot::{
    CompositeSnapshot, DataSnapshotRef, MemorySnapshotRef, SnapshotRef, SnapshotSpec,
};

struct ForkArtifactGuard {
    session_id: String,
    committed: bool,
}

impl ForkArtifactGuard {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            committed: false,
        }
    }

    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ForkArtifactGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let _ = std::fs::remove_file(journal_file_path(&self.session_id));
        let _ = std::fs::remove_dir_all(session_workspace::workspace_dir_for(&self.session_id));
    }
}

#[derive(Debug)]
struct StepCheckpointDirEntry {
    name: OsString,
    path: PathBuf,
}

impl StepCheckpointDirEntry {
    fn from_dir_entry(entry: std::fs::DirEntry) -> Self {
        Self {
            name: entry.file_name(),
            path: entry.path(),
        }
    }
}

fn collect_step_checkpoint_entries<I>(entries: I) -> Result<Vec<StepCheckpointDirEntry>, String>
where
    I: IntoIterator<Item = Result<StepCheckpointDirEntry, std::io::Error>>,
{
    let mut entries = entries
        .into_iter()
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("read step_checkpoints entry: {e}"))?;
    entries.sort_by_key(|entry| entry.name.clone());
    Ok(entries)
}

fn is_step_checkpoint_file_name(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    name.ends_with("-heavy.json") || name.ends_with("-light.json")
}

fn sha256_file(path: &std::path::Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    Ok(format!("sha256:{:x}", Sha256::digest(bytes)))
}

fn checkpoint_directory_hash(path: &std::path::Path) -> Result<Option<String>, String> {
    if !path.is_dir() {
        return Ok(None);
    }
    let entries = collect_step_checkpoint_entries(
        std::fs::read_dir(path)
            .map_err(|error| format!("read {}: {error}", path.display()))?
            .map(|entry| entry.map(StepCheckpointDirEntry::from_dir_entry)),
    )?;
    let mut hasher = Sha256::new();
    let mut count = 0_usize;
    for entry in entries {
        if !is_step_checkpoint_file_name(&entry.name) {
            continue;
        }
        hasher.update(entry.name.to_string_lossy().as_bytes());
        let bytes = std::fs::read(&entry.path)
            .map_err(|error| format!("read checkpoint {}: {error}", entry.path.display()))?;
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
        count += 1;
    }
    Ok((count > 0).then(|| format!("sha256:{:x}", hasher.finalize())))
}

fn persist_state_manifest(
    session_id: &str,
    manifest: &astra_sync_protocol::SessionStateManifestV1,
) -> Result<(), String> {
    let directory = session_workspace::workspace_dir_for(session_id);
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create state manifest directory: {error}"))?;
    let path = directory.join("state-manifest-v1.json");
    let temporary = directory.join("state-manifest-v1.json.tmp");
    let bytes = serde_json::to_vec(manifest)
        .map_err(|error| format!("serialize state manifest: {error}"))?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)
        .map_err(|error| format!("open state manifest temp file: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("write state manifest: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("sync state manifest: {error}"))?;
    drop(file);
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("commit state manifest: {error}"))?;
    std::fs::File::open(&directory)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("sync state manifest directory: {error}"))
}

/// Fork parent journal into a new session file and workspace metadata.
///
/// Fails if the target journal path already exists or the parent journal is empty.
pub fn fork_local_session(opts: ForkSessionOptions) -> Result<ForkSessionResult, String> {
    let parent = opts.parent_session_id.trim().to_string();
    if parent.is_empty() {
        return Err("parent_session_id is empty".into());
    }
    validate_session_id(&parent).map_err(|e| format!("invalid parent session ID: {e}"))?;

    let new_id = opts
        .new_session_id
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    validate_session_id(&new_id).map_err(|e| format!("invalid new session ID: {e}"))?;

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

    let parent_workspace = match session_workspace::read_workspace(&parent) {
        Ok(ws) => Some(ws),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("read parent workspace: {e}")),
    };
    let workspace_turn_count = parent_workspace.as_ref().map(|w| w.turn_count);
    let max_turn = workspace_turn_count.unwrap_or_else(|| {
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

    let artifact_guard = ForkArtifactGuard::new(new_id.clone());
    let writer = JournalWriter::new(&new_id).map_err(|e| e.to_string())?;
    for evt in &out {
        writer.append(evt).map_err(|e| e.to_string())?;
    }
    eprintln!(
        "[audit] forked session {parent} → {new_id} (turn {forked_at_turn}, {copied} events copied)"
    );

    let mut ws = parent_workspace
        .unwrap_or_else(|| WorkspaceMetadata::new(&parent, model.as_deref().unwrap_or("default")));
    ws.session_id = new_id.clone();
    ws.parent_session_id = Some(parent.clone());
    ws.fork_note = opts.label.clone();
    ws.forked_at_turn = Some(forked_at_turn);
    // Carry forward an existing correlation id, else use parent session id as chain root for multi-agent / audit.
    ws.correlation_id = ws.correlation_id.clone().or_else(|| Some(parent.clone()));
    ws.turn_count = forked_at_turn;
    ws.agent_role = None;
    let now = chrono::Utc::now().to_rfc3339();
    ws.created_at = now.clone();
    ws.updated_at = now;
    ws.status = "active".to_string();
    session_workspace::write_workspace(&ws).map_err(|e| e.to_string())?;

    // Copy step checkpoints only for full-history forks. When forking from an
    // earlier turn, the parent's latest heavy checkpoint may describe future
    // conversation state that is no longer present in the child journal.
    // Skipping checkpoint copy in that case is strictly safer than restoring
    // stale future context. Higher-level CLI flows synthesize a fresh child
    // snapshot immediately after the fork completes.
    if workspace_turn_count.is_some() && forked_at_turn == max_turn {
        let store = crate::local_session_artifact_store();
        let parent_cp_dir = store.session_dir(&parent)?.join("step_checkpoints");
        if parent_cp_dir.is_dir() {
            let new_cp_dir = store.session_dir(&new_id)?.join("step_checkpoints");
            std::fs::create_dir_all(&new_cp_dir)
                .map_err(|e| format!("create step_checkpoints dir: {e}"))?;
            let entries = collect_step_checkpoint_entries(
                std::fs::read_dir(&parent_cp_dir)
                    .map_err(|e| format!("read step_checkpoints: {e}"))?
                    .map(|entry| entry.map(StepCheckpointDirEntry::from_dir_entry)),
            )?;
            for entry in entries {
                if !is_step_checkpoint_file_name(&entry.name) {
                    continue;
                }
                let name_str = entry.name.to_string_lossy();
                std::fs::copy(entry.path, new_cp_dir.join(&entry.name))
                    .map_err(|e| format!("copy checkpoint {name_str}: {e}"))?;
            }
        }
    }

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

    use astra_sync_protocol::{
        SessionStateDimension as Dimension, SessionStateManifestEntry as Entry,
        SessionStateManifestV1,
    };
    let child_journal = journal_file_path(&new_id);
    let child_workspace = session_workspace::workspace_file_path(&new_id)
        .map_err(|error| format!("resolve child workspace manifest: {error}"))?;
    let checkpoint_dir = crate::local_session_artifact_store()
        .session_dir(&new_id)?
        .join("step_checkpoints");
    let checkpoint_entry = match checkpoint_directory_hash(&checkpoint_dir)? {
        Some(hash) => Entry::referenced_hash(
            Dimension::Checkpoint,
            checkpoint_dir.to_string_lossy(),
            hash,
        )?,
        None => Entry::gap(
            Dimension::Checkpoint,
            "no checkpoint covers the frozen fork cursor",
        ),
    };
    let memory_entry = match fork_snapshot
        .as_ref()
        .and_then(CompositeSnapshot::memory_snapshot)
    {
        Some(memory) => Entry::referenced(
            Dimension::Memory,
            memory
                .path
                .clone()
                .unwrap_or_else(|| format!("memory://{}@{}", memory.profile, memory.epoch)),
            &serde_json::to_vec(memory)
                .map_err(|error| format!("serialize memory snapshot reference: {error}"))?,
        ),
        None => Entry::gap(
            Dimension::Memory,
            "no memory snapshot was frozen at the fork cursor",
        ),
    };
    let state_manifest = SessionStateManifestV1::new(
        &parent,
        &new_id,
        format!("turn:{forked_at_turn}"),
        vec![
            Entry::referenced_hash(
                Dimension::Transcript,
                child_journal.to_string_lossy(),
                sha256_file(&child_journal)?,
            )?,
            checkpoint_entry,
            Entry::referenced_hash(
                Dimension::Workspace,
                child_workspace.to_string_lossy(),
                sha256_file(&child_workspace)?,
            )?,
            Entry::gap(
                Dimension::Task,
                "local fork has no task snapshot at the frozen cursor",
            ),
            Entry::gap(
                Dimension::Artifact,
                "artifact ownership was not frozen into this local fork",
            ),
            Entry::gap(
                Dimension::Invocation,
                "invocation ledger was not frozen into this local fork",
            ),
            memory_entry,
        ],
    )?;
    persist_state_manifest(&new_id, &state_manifest)?;

    artifact_guard.commit();
    Ok(ForkSessionResult {
        new_session_id: new_id,
        events_copied: copied,
        forked_at_turn,
        data_branch_name,
        fork_snapshot,
        state_manifest,
    })
}

// ---------------------------------------------------------------------------
// Layer 5.2 — Multi-Session Exploration
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_journal::{
        JournalDirGuard, JournalEvent, JournalEventType, JournalWriter, journal_file_path,
        read_journal,
    };
    use crate::session_workspace;

    /// Redirect journal + workspace I/O to a temp dir. Without this, every
    /// fork test writes to the user's real `~/.astra/sessions`, adding
    /// hundreds of ms of real disk work per test. Returns the guard + tempdir
    /// (hold both for the test's lifetime).
    fn isolated_sessions_dir() -> (tempfile::TempDir, JournalDirGuard) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let guard = JournalDirGuard::new(tmp.path());
        (tmp, guard)
    }

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

    fn fake_checkpoint_entry(name: &str) -> StepCheckpointDirEntry {
        StepCheckpointDirEntry {
            name: OsString::from(name),
            path: PathBuf::from(name),
        }
    }

    #[test]
    fn collect_step_checkpoint_entries_sorts_and_fails_loudly_on_entry_error() {
        let entries = collect_step_checkpoint_entries(vec![
            Ok(fake_checkpoint_entry("000002-light.json")),
            Ok(fake_checkpoint_entry("000001-heavy.json")),
        ])
        .expect("entries collect");

        let names = entries
            .iter()
            .map(|entry| entry.name.to_string_lossy().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names, vec!["000001-heavy.json", "000002-light.json"]);

        let err = collect_step_checkpoint_entries(vec![
            Ok(fake_checkpoint_entry("000001-heavy.json")),
            Err(std::io::Error::other("directory entry vanished")),
        ])
        .expect_err("entry errors must not be dropped");
        assert!(
            err.contains("read step_checkpoints entry") && err.contains("directory entry vanished"),
            "entry error should be explicit: {err}"
        );
    }

    #[test]
    fn step_checkpoint_file_name_filter_accepts_only_checkpoint_files() {
        assert!(is_step_checkpoint_file_name(OsStr::new(
            "000001-heavy.json"
        )));
        assert!(is_step_checkpoint_file_name(OsStr::new(
            "000002-light.json"
        )));
        assert!(!is_step_checkpoint_file_name(OsStr::new(
            "composite_snapshots.json"
        )));
        assert!(!is_step_checkpoint_file_name(OsStr::new("000003.json")));
    }

    /// Cleanup test session files (journal + workspace dir under `~/.astra/sessions`).
    fn cleanup_session(session_id: &str) {
        let _ = std::fs::remove_file(journal_file_path(session_id));
        let ws_dir = session_workspace::workspace_dir_for(session_id);
        let _ = std::fs::remove_dir_all(&ws_dir);
    }

    #[test]
    fn fork_none_uses_latest_turn() {
        let (_tmp, _guard) = isolated_sessions_dir();
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
        assert_eq!(result.state_manifest.as_of_cursor, "turn:5");
        assert!(!result.state_manifest.is_complete());
        assert!(
            session_workspace::workspace_dir_for(&result.new_session_id)
                .join("state-manifest-v1.json")
                .is_file()
        );
        let restored_manifest: astra_sync_protocol::SessionStateManifestV1 =
            serde_json::from_slice(
                &std::fs::read(
                    session_workspace::workspace_dir_for(&result.new_session_id)
                        .join("state-manifest-v1.json"),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(restored_manifest, result.state_manifest);

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
        let (_tmp, _guard) = isolated_sessions_dir();
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
        let (_tmp, _guard) = isolated_sessions_dir();
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
        let (_tmp, _guard) = isolated_sessions_dir();
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
        let (_tmp, _guard) = isolated_sessions_dir();
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
        let (_tmp, _guard) = isolated_sessions_dir();
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

    #[test]
    fn fork_copies_step_checkpoints() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 3);

        // Create fake checkpoints in parent
        let store = crate::local_session_artifact_store();
        let cp_dir = store
            .session_dir(&parent_id)
            .unwrap()
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("000001-heavy.json"), r#"{"turn":1}"#).unwrap();
        std::fs::write(cp_dir.join("000002-light.json"), r#"{"turn":2}"#).unwrap();
        std::fs::write(cp_dir.join("000003-heavy.json"), r#"{"turn":3}"#).unwrap();
        std::fs::write(cp_dir.join("composite_snapshots.json"), "{}").unwrap();

        let opts = ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: None,
            forked_after_turn: None,
            data_branch: None,
            snapshot_spec: None,
        };
        let result = fork_local_session(opts).expect("fork should succeed");

        let new_cp_dir = store
            .session_dir(&result.new_session_id)
            .unwrap()
            .join("step_checkpoints");
        assert!(new_cp_dir.exists(), "step_checkpoints dir should be copied");
        assert!(
            new_cp_dir.join("000001-heavy.json").exists(),
            "heavy checkpoint should be copied"
        );
        assert!(
            new_cp_dir.join("000002-light.json").exists(),
            "light checkpoint should be copied"
        );
        assert!(
            new_cp_dir.join("000003-heavy.json").exists(),
            "heavy checkpoint should be copied"
        );
        assert!(
            !new_cp_dir.join("composite_snapshots.json").exists(),
            "composite_snapshots.json should NOT be copied"
        );

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }

    #[test]
    fn fork_before_latest_does_not_copy_future_step_checkpoints() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 4);

        let store = crate::local_session_artifact_store();
        let cp_dir = store
            .session_dir(&parent_id)
            .unwrap()
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("000001-heavy.json"), r#"{"turn":1}"#).unwrap();
        std::fs::write(cp_dir.join("000004-heavy.json"), r#"{"turn":4}"#).unwrap();

        let result = fork_local_session(ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: None,
            forked_after_turn: Some(2),
            data_branch: None,
            snapshot_spec: None,
        })
        .expect("fork should succeed");

        let child_cp_dir = store
            .session_dir(&result.new_session_id)
            .unwrap()
            .join("step_checkpoints");
        assert!(
            !child_cp_dir.exists(),
            "earlier-turn fork must not copy parent checkpoints that may encode future state"
        );

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }

    #[test]
    fn fork_without_workspace_does_not_copy_step_checkpoints() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let parent_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 3);

        let store = crate::local_session_artifact_store();
        let cp_dir = store
            .session_dir(&parent_id)
            .unwrap()
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::write(cp_dir.join("000003-heavy.json"), r#"{"turn":3}"#).unwrap();
        std::fs::remove_file(
            session_workspace::workspace_dir_for(&parent_id).join("workspace.yaml"),
        )
        .unwrap();

        let result = fork_local_session(ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: None,
            label: None,
            forked_after_turn: None,
            data_branch: None,
            snapshot_spec: None,
        })
        .expect("fork should succeed");

        let child_cp_dir = store
            .session_dir(&result.new_session_id)
            .unwrap()
            .join("step_checkpoints");
        assert!(
            !child_cp_dir.exists(),
            "without workspace turn_count we should not infer that latest journal events are full history"
        );

        cleanup_session(&parent_id);
        cleanup_session(&result.new_session_id);
    }

    #[test]
    fn fork_rejects_corrupt_parent_workspace_before_writing_child_artifacts() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let parent_id = uuid::Uuid::new_v4().to_string();
        let child_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 3);

        std::fs::write(
            session_workspace::workspace_dir_for(&parent_id).join("workspace.yaml"),
            ":\nnot-valid-yaml",
        )
        .unwrap();

        let error = fork_local_session(ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: Some(child_id.clone()),
            label: None,
            forked_after_turn: None,
            data_branch: None,
            snapshot_spec: None,
        })
        .expect_err("corrupt parent workspace should fail fork");

        assert!(error.contains("read parent workspace"), "{error}");
        assert!(
            !journal_file_path(&child_id).exists(),
            "fork must not create the child journal when parent workspace is unreadable"
        );
        assert!(
            !session_workspace::workspace_dir_for(&child_id).exists(),
            "fork must not create child workspace artifacts on parent workspace read failure"
        );

        cleanup_session(&parent_id);
    }

    #[test]
    fn fork_cleans_up_child_artifacts_when_checkpoint_copy_fails() {
        let (_tmp, _guard) = isolated_sessions_dir();
        let parent_id = uuid::Uuid::new_v4().to_string();
        let child_id = uuid::Uuid::new_v4().to_string();
        setup_test_session(&parent_id, 3);

        let store = crate::local_session_artifact_store();
        let cp_dir = store
            .session_dir(&parent_id)
            .unwrap()
            .join("step_checkpoints");
        std::fs::create_dir_all(&cp_dir).unwrap();
        std::fs::create_dir(cp_dir.join("000003-heavy.json")).unwrap();

        let error = fork_local_session(ForkSessionOptions {
            parent_session_id: parent_id.clone(),
            new_session_id: Some(child_id.clone()),
            label: None,
            forked_after_turn: None,
            data_branch: None,
            snapshot_spec: None,
        })
        .expect_err("checkpoint copy failure should fail fork");

        assert!(error.contains("copy checkpoint"), "{error}");
        assert!(
            !journal_file_path(&child_id).exists(),
            "failed fork must remove the child journal"
        );
        assert!(
            !session_workspace::workspace_dir_for(&child_id).exists(),
            "failed fork must remove child workspace and checkpoint artifacts"
        );

        cleanup_session(&parent_id);
    }
}
