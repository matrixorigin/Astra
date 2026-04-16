//! Step checkpoint persistence — write/read StepCheckpoint JSON to local filesystem.
//!
//! Stores checkpoints at:
//! `~/.astra/sessions/<session_id>/step_checkpoints/<number>-<tier>.json`
//!
//! Also provides a file-backed StepEventStore that writes events as JSONL:
//! `~/.astra/sessions/<session_id>/step_events.jsonl`
//!
//! Light checkpoints (~1KB) written after each tool completion.
//! Heavy checkpoints (~10-100KB) written after each turn's verdict.
//! On crash recovery, the latest heavy checkpoint restores full session state.

use std::path::{Path, PathBuf};

use crate::step_protocol::{
    CheckpointTier, HeavyCheckpoint, LightCheckpoint, StepCheckpoint, StepEvent, StepEventStore,
};

/// Directory name within session workspace for step checkpoints.
const STEP_CHECKPOINT_DIR: &str = "step_checkpoints";

/// Maximum number of light checkpoints to retain (older ones pruned).
const MAX_LIGHT_CHECKPOINTS: usize = 50;

/// Get the step checkpoint directory for a session.
fn checkpoint_dir_for(session_id: &str) -> PathBuf {
    astra_services::session_journal::local_sessions_dir()
        .join(session_id)
        .join(STEP_CHECKPOINT_DIR)
}

/// Write a step checkpoint to local filesystem.
/// Returns the path where the checkpoint was written.
pub fn write_step_checkpoint(
    session_id: &str,
    number: u32,
    checkpoint: &StepCheckpoint,
) -> std::io::Result<PathBuf> {
    let dir = checkpoint_dir_for(session_id);
    std::fs::create_dir_all(&dir)?;

    let tier = match checkpoint {
        StepCheckpoint::Light(_) => "light",
        StepCheckpoint::Heavy(_) => "heavy",
    };
    let filename = format!("{:06}-{}.json", number, tier);
    let path = dir.join(&filename);

    let json = serde_json::to_string(checkpoint)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Atomic write: write to temp file, then rename
    let tmp_path = dir.join(format!(".tmp-{}", filename));
    std::fs::write(&tmp_path, &json)?;
    std::fs::rename(&tmp_path, &path)?;

    // Prune old light checkpoints if too many
    if tier == "light" {
        prune_light_checkpoints(&dir)?;
    }

    Ok(path)
}

/// Read the latest heavy checkpoint for session recovery.
/// Returns None if no heavy checkpoint exists.
pub fn read_latest_heavy_checkpoint(session_id: &str) -> std::io::Result<Option<HeavyCheckpoint>> {
    let dir = checkpoint_dir_for(session_id);
    if !dir.exists() {
        return Ok(None);
    }

    let mut heavy_files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read checkpoint dir entry: {}",
                    err
                );
                None
            }
        })
        .filter(|e| e.file_name().to_string_lossy().ends_with("-heavy.json"))
        .collect();

    // Sort by name descending (latest = highest number)
    heavy_files.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

    if let Some(entry) = heavy_files.first() {
        let content = std::fs::read_to_string(entry.path())?;
        let checkpoint: StepCheckpoint = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match checkpoint {
            StepCheckpoint::Heavy(boxed) => Ok(Some(*boxed)),
            _ => Ok(None),
        }
    } else {
        Ok(None)
    }
}

/// Read the latest light checkpoint (for quick cursor restore).
pub fn read_latest_light_checkpoint(session_id: &str) -> std::io::Result<Option<LightCheckpoint>> {
    let dir = checkpoint_dir_for(session_id);
    if !dir.exists() {
        return Ok(None);
    }

    // Any checkpoint contains cursor info — find the highest numbered file
    let mut all_files: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read checkpoint dir entry: {}",
                    err
                );
                None
            }
        })
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .collect();

    all_files.sort_by_key(|b| std::cmp::Reverse(b.file_name()));

    if let Some(entry) = all_files.first() {
        let content = std::fs::read_to_string(entry.path())?;
        let checkpoint: StepCheckpoint = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        match checkpoint {
            StepCheckpoint::Light(light) => Ok(Some(light)),
            StepCheckpoint::Heavy(heavy) => Ok(Some(heavy.light)),
        }
    } else {
        Ok(None)
    }
}

/// List all checkpoint numbers and tiers for a session.
pub fn list_checkpoints(session_id: &str) -> std::io::Result<Vec<(u32, CheckpointTier)>> {
    let dir = checkpoint_dir_for(session_id);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut result = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read dir entry during list: {}",
                    err
                );
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if let Some(rest) = name.strip_suffix(".json")
            && let Some((num_str, tier_str)) = rest.split_once('-')
            && let Ok(num) = num_str.parse::<u32>()
        {
            let tier = match tier_str {
                "light" => CheckpointTier::Light,
                "heavy" => CheckpointTier::Heavy,
                _ => continue,
            };
            result.push((num, tier));
        }
    }
    result.sort_by_key(|(n, _)| *n);
    Ok(result)
}

/// Remove old light checkpoints, keeping only the most recent MAX_LIGHT_CHECKPOINTS.
fn prune_light_checkpoints(dir: &Path) -> std::io::Result<()> {
    let mut light_files: Vec<_> = std::fs::read_dir(dir)?
        .filter_map(|e| match e {
            Ok(entry) => Some(entry),
            Err(err) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Failed to read dir entry during prune: {}",
                    err
                );
                None
            }
        })
        .filter(|e| e.file_name().to_string_lossy().ends_with("-light.json"))
        .collect();

    if light_files.len() <= MAX_LIGHT_CHECKPOINTS {
        return Ok(());
    }

    // Sort ascending by name, remove oldest
    light_files.sort_by_key(|a| a.file_name());
    let to_remove = light_files.len() - MAX_LIGHT_CHECKPOINTS;
    for entry in light_files.into_iter().take(to_remove) {
        if let Err(err) = std::fs::remove_file(entry.path()) {
            astra_core::agent_warn!(
                "checkpoint",
                "Failed to prune light checkpoint {:?}: {}",
                entry.file_name(),
                err
            );
        }
    }

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════════
// Breakpoint Index I/O
// ═══════════════════════════════════════════════════════════════════════════════

pub fn write_breakpoint_index(
    session_id: &str,
    index: &crate::step_protocol::BreakpointIndex,
) -> std::io::Result<()> {
    let dir = checkpoint_dir_for(session_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("breakpoints.json");
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = dir.join(".breakpoints.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn read_breakpoint_index(
    session_id: &str,
) -> std::io::Result<crate::step_protocol::BreakpointIndex> {
    let path = checkpoint_dir_for(session_id).join("breakpoints.json");
    if !path.exists() {
        return Ok(crate::step_protocol::BreakpointIndex::default());
    }
    let content = std::fs::read_to_string(&path)?;
    serde_json::from_str(&content)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

// ─── Composite Snapshot I/O ──────────────────────────────────────────────────

/// Persist the composite snapshot index to disk (atomic write).
pub fn write_composite_snapshot_index(
    session_id: &str,
    index: &astra_core::composite_snapshot::CompositeSnapshotIndex,
) -> std::io::Result<()> {
    let dir = checkpoint_dir_for(session_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("composite_snapshots.json");
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let tmp = dir.join(".composite_snapshots.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Read the composite snapshot index from disk.
pub fn read_composite_snapshot_index(
    session_id: &str,
) -> std::io::Result<astra_core::composite_snapshot::CompositeSnapshotIndex> {
    let path = checkpoint_dir_for(session_id).join("composite_snapshots.json");
    if !path.exists() {
        return Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let mut index: astra_core::composite_snapshot::CompositeSnapshotIndex =
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    index.normalize_versions();
    Ok(index)
}

// ═══════════════════════════════════════════════════════════════════════════════
// File-Backed StepEventStore (JSONL)
// ═══════════════════════════════════════════════════════════════════════════════

/// File path for step events JSONL.
fn events_path_for(session_id: &str) -> PathBuf {
    session_dir_for(session_id).join("step_events.jsonl")
}

fn session_dir_for(session_id: &str) -> PathBuf {
    astra_services::session_journal::local_sessions_dir().join(session_id)
}

/// File-backed event store: in-memory DAG + append-only JSONL on disk.
/// Writes are immediate (no buffering) for crash safety.
pub struct FileBackedEventStore {
    session_id: String,
    events: Vec<StepEvent>,
}

impl FileBackedEventStore {
    /// Create a new store for a session, loading existing events from disk.
    pub fn new(session_id: &str) -> Self {
        let events = Self::load_events(session_id).unwrap_or_default();
        Self {
            session_id: session_id.to_string(),
            events,
        }
    }

    /// Create empty (for tests or ephemeral sessions).
    pub fn empty(session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            events: Vec::new(),
        }
    }

    /// Load events from JSONL file.
    fn load_events(session_id: &str) -> std::io::Result<Vec<StepEvent>> {
        let path = events_path_for(session_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let mut events = Vec::new();
        for line in content.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(event) = serde_json::from_str::<StepEvent>(line) {
                events.push(event);
            }
            // Skip malformed lines (best-effort)
        }
        Ok(events)
    }

    /// Append a single event to the JSONL file.
    fn persist_event(&self, event: &StepEvent) -> std::io::Result<()> {
        let dir = session_dir_for(&self.session_id);
        std::fs::create_dir_all(&dir)?;
        let path = events_path_for(&self.session_id);
        let json = serde_json::to_string(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        writeln!(file, "{}", json)?;
        Ok(())
    }

    /// Get all events (for audit/replay).
    pub fn all_events(&self) -> &[StepEvent] {
        &self.events
    }

    /// Event count.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
}

impl StepEventStore for FileBackedEventStore {
    fn append(&mut self, event: StepEvent) {
        if let Err(err) = self.persist_event(&event) {
            astra_core::agent_warn!(
                "event_store",
                "Failed to persist step event {}: {}",
                event.event_id,
                err
            );
        }
        self.events.push(event);
    }

    fn events_for_step(&self, step_id: &str) -> Vec<&StepEvent> {
        self.events
            .iter()
            .filter(|e| e.step_id == step_id)
            .collect()
    }

    fn ancestors(&self, event_id: &str) -> Vec<&StepEvent> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back(event_id.to_string());
        visited.insert(event_id.to_string());

        while let Some(current) = queue.pop_front() {
            if let Some(ev) = self.events.iter().find(|e| e.event_id == current) {
                if ev.event_id != event_id {
                    result.push(ev);
                }
                for parent in &ev.caused_by {
                    if visited.insert(parent.clone()) {
                        queue.push_back(parent.clone());
                    }
                }
            }
        }
        result
    }

    fn descendants(&self, event_id: &str) -> Vec<&StepEvent> {
        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        let mut visited = std::collections::HashSet::new();
        queue.push_back(event_id.to_string());
        visited.insert(event_id.to_string());

        while let Some(current) = queue.pop_front() {
            for ev in &self.events {
                if ev.caused_by.contains(&current) && visited.insert(ev.event_id.clone()) {
                    result.push(ev);
                    queue.push_back(ev.event_id.clone());
                }
            }
        }
        result
    }

    fn leaves(&self) -> Vec<&StepEvent> {
        let parent_ids: std::collections::HashSet<&str> = self
            .events
            .iter()
            .flat_map(|e| e.caused_by.iter().map(|s| s.as_str()))
            .collect();
        self.events
            .iter()
            .filter(|e| !parent_ids.contains(e.event_id.as_str()))
            .collect()
    }

    fn len(&self) -> usize {
        self.events.len()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::step_protocol::{ExecutionCursor, PROTOCOL_VERSION};
    use serde_json::json;

    fn make_light(step_id: &str, progress: f64) -> LightCheckpoint {
        LightCheckpoint {
            protocol_version: PROTOCOL_VERSION,
            cursor: ExecutionCursor::for_act(1),
            step_id: step_id.to_string(),
            task_id: "task-1".to_string(),
            agent_id: "agent-1".to_string(),
            progress,
            total_tokens: 1000,
            created_at: 1234567890,
        }
    }

    fn make_heavy(step_id: &str, messages: Vec<serde_json::Value>) -> HeavyCheckpoint {
        HeavyCheckpoint {
            light: make_light(step_id, 0.5),
            messages,
            budget_remaining_tokens: 50000,
            budget_remaining_rounds: 8,
            blocked_tools: vec!["bash".to_string()],
            recent_tools: vec!["grep".to_string(), "read_file".to_string()],
            learning_snapshot_id: None,
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
        }
    }

    #[test]
    fn write_and_read_light_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("step_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();

        let light = make_light("step-1", 0.75);
        let cp = StepCheckpoint::Light(light);
        let json_str = serde_json::to_string(&cp).unwrap();

        let path = dir.join("000001-light.json");
        std::fs::write(&path, &json_str).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&content).unwrap();
        match restored {
            StepCheckpoint::Light(l) => {
                assert_eq!(l.step_id, "step-1");
                assert!((l.progress - 0.75).abs() < f64::EPSILON);
                assert_eq!(l.protocol_version, PROTOCOL_VERSION);
            }
            _ => panic!("Expected Light checkpoint"),
        }
    }

    #[test]
    fn write_and_read_heavy_checkpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("step_checkpoints");
        std::fs::create_dir_all(&dir).unwrap();

        let msgs = vec![
            json!({"role": "user", "content": "fix the bug"}),
            json!({"role": "assistant", "content": "I'll check the code."}),
        ];
        let heavy = make_heavy("step-2", msgs);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();

        let path = dir.join("000002-heavy.json");
        std::fs::write(&path, &json_str).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&content).unwrap();
        match restored {
            StepCheckpoint::Heavy(h) => {
                assert_eq!(h.light.step_id, "step-2");
                assert_eq!(h.messages.len(), 2);
                assert_eq!(h.budget_remaining_tokens, 50000);
                assert_eq!(h.blocked_tools, vec!["bash"]);
                assert_eq!(h.recent_tools, vec!["grep", "read_file"]);
            }
            _ => panic!("Expected Heavy checkpoint"),
        }
    }

    #[test]
    fn checkpoint_serialization_roundtrip() {
        let light = make_light("step-rt", 0.33);
        let cp = StepCheckpoint::Light(light);
        let json_str = serde_json::to_string_pretty(&cp).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&json_str).unwrap();
        let json2 = serde_json::to_string_pretty(&restored).unwrap();
        assert_eq!(json_str, json2, "Round-trip serialization must be stable");
    }

    #[test]
    fn heavy_checkpoint_preserves_cjk_messages() {
        let msgs = vec![
            json!({"role": "system", "content": "You are a helpful assistant."}),
            json!({"role": "user", "content": "帮我查一下PR状态"}),
            json!({"role": "assistant", "content": "好的，让我查看一下。"}),
        ];
        let heavy = make_heavy("step-cjk", msgs);
        let json_str = serde_json::to_string(&heavy).unwrap();
        let restored: HeavyCheckpoint = serde_json::from_str(&json_str).unwrap();
        assert_eq!(restored.messages.len(), 3);
        assert_eq!(restored.messages[1]["content"], "帮我查一下PR状态");
    }

    #[test]
    fn heavy_checkpoint_interruption_roundtrip() {
        let irj = json!({
            "kind": "rate_limited",
            "resumable": true,
            "has_checkpoint": true,
            "tool_calls_completed": 7,
            "turns_completed": 3,
            "remaining_turns": 7,
            "user_message": "[rate_limited] 7 tool call(s) completed."
        });
        let mut heavy = make_heavy("step-irq", vec![json!({"role":"user","content":"hi"})]);
        heavy.interruption = Some(irj.clone());

        let json_str = serde_json::to_string(&heavy).unwrap();
        let restored: HeavyCheckpoint = serde_json::from_str(&json_str).unwrap();
        assert_eq!(restored.interruption, Some(irj));
    }

    #[test]
    fn heavy_checkpoint_without_interruption_deserializes() {
        // Backward compat: old checkpoints without the interruption field.
        let json_str = r#"{
            "light": {"protocol_version": 1, "cursor": {"phase": "Perceive", "slots": [], "parallel": false}, "step_id": "s", "task_id": "t", "agent_id": "a", "progress": 0.5, "total_tokens": 0, "created_at": 0},
            "messages": [],
            "budget_remaining_tokens": 0,
            "budget_remaining_rounds": 0,
            "blocked_tools": [],
            "recent_tools": []
        }"#;
        let heavy: HeavyCheckpoint = serde_json::from_str(json_str).unwrap();
        assert!(heavy.interruption.is_none());
    }

    #[test]
    fn checkpoint_tier_from_filename() {
        let name = "000005-heavy.json";
        let rest = name.strip_suffix(".json").unwrap();
        let (num_str, tier_str) = rest.split_once('-').unwrap();
        assert_eq!(num_str.parse::<u32>().unwrap(), 5);
        assert_eq!(tier_str, "heavy");

        let name2 = "000012-light.json";
        let rest2 = name2.strip_suffix(".json").unwrap();
        let (num_str2, tier_str2) = rest2.split_once('-').unwrap();
        assert_eq!(num_str2.parse::<u32>().unwrap(), 12);
        assert_eq!(tier_str2, "light");
    }

    #[test]
    fn prune_respects_max_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        for i in 0..(MAX_LIGHT_CHECKPOINTS + 10) {
            let name = format!("{:06}-light.json", i);
            std::fs::write(dir.join(&name), "{}").unwrap();
        }

        prune_light_checkpoints(dir).unwrap();

        let remaining: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with("-light.json"))
            .collect();

        assert_eq!(remaining.len(), MAX_LIGHT_CHECKPOINTS);
    }

    #[test]
    fn prune_keeps_heavy_checkpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();

        for i in 0..5 {
            std::fs::write(dir.join(format!("{:06}-light.json", i)), "{}").unwrap();
            std::fs::write(dir.join(format!("{:06}-heavy.json", i)), "{}").unwrap();
        }

        prune_light_checkpoints(dir).unwrap();

        let heavy_count = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with("-heavy.json"))
            .count();

        assert_eq!(heavy_count, 5, "Heavy checkpoints must not be pruned");
    }

    #[test]
    fn write_step_checkpoint_creates_dir_and_file() {
        // Use a unique session ID with tempdir-like suffix to avoid collision
        let session_id = format!("test-step-cp-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id);

        // Clean up from any previous run
        let _ = std::fs::remove_dir_all(&dir);

        let light = make_light("step-write-test", 1.0);
        let cp = StepCheckpoint::Light(light);
        let result = write_step_checkpoint(&session_id, 1, &cp);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.exists());

        // Read back
        let content = std::fs::read_to_string(&path).unwrap();
        let restored: StepCheckpoint = serde_json::from_str(&content).unwrap();
        match restored {
            StepCheckpoint::Light(l) => assert_eq!(l.step_id, "step-write-test"),
            _ => panic!("Expected Light"),
        }

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_latest_heavy_on_empty_returns_none() {
        let result = read_latest_heavy_checkpoint("nonexistent-session-xyz-42");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // ── FileBackedEventStore tests ──────────────────────────────────────────

    use crate::step_protocol::StepEventType;

    fn make_event(id: &str, step_id: &str, event_type: StepEventType) -> StepEvent {
        StepEvent {
            event_id: id.to_string(),
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            created_at: 1000,
        }
    }

    #[test]
    fn file_event_store_append_and_count() {
        let mut store = FileBackedEventStore::empty("test-events-empty");
        assert_eq!(store.event_count(), 0);

        store.append(make_event("e1", "step-1", StepEventType::StepCreated));
        store.append(make_event("e2", "step-1", StepEventType::ToolCallStarted));
        assert_eq!(store.event_count(), 2);
    }

    #[test]
    fn file_event_store_events_for_step() {
        let mut store = FileBackedEventStore::empty("test-events-step");
        store.append(make_event("e1", "step-1", StepEventType::StepCreated));
        store.append(make_event("e2", "step-2", StepEventType::StepCreated));
        store.append(make_event("e3", "step-1", StepEventType::ToolCallCompleted));

        let step1_events = store.events_for_step("step-1");
        assert_eq!(step1_events.len(), 2);
        let step2_events = store.events_for_step("step-2");
        assert_eq!(step2_events.len(), 1);
    }

    #[test]
    fn file_event_store_ancestors() {
        let mut store = FileBackedEventStore::empty("test-events-ancestors");
        store.append(make_event("root", "s1", StepEventType::StepCreated));

        let mut child = make_event("child", "s1", StepEventType::ToolCallStarted);
        child.caused_by = vec!["root".to_string()];
        store.append(child);

        let mut grandchild = make_event("grandchild", "s1", StepEventType::ToolCallCompleted);
        grandchild.caused_by = vec!["child".to_string()];
        store.append(grandchild);

        let ancestors = store.ancestors("grandchild");
        assert_eq!(ancestors.len(), 2);
        let ids: Vec<&str> = ancestors.iter().map(|e| e.event_id.as_str()).collect();
        assert!(ids.contains(&"root"));
        assert!(ids.contains(&"child"));
    }

    #[test]
    fn file_event_store_descendants() {
        let mut store = FileBackedEventStore::empty("test-events-desc");
        store.append(make_event("root", "s1", StepEventType::StepCreated));

        let mut child = make_event("child", "s1", StepEventType::ToolCallStarted);
        child.caused_by = vec!["root".to_string()];
        store.append(child);

        let desc = store.descendants("root");
        assert_eq!(desc.len(), 1);
        assert_eq!(desc[0].event_id, "child");
    }

    #[test]
    fn file_event_store_leaves() {
        let mut store = FileBackedEventStore::empty("test-events-leaves");
        store.append(make_event("root", "s1", StepEventType::StepCreated));

        let mut child = make_event("child", "s1", StepEventType::ToolCallCompleted);
        child.caused_by = vec!["root".to_string()];
        store.append(child);

        let leaves = store.leaves();
        assert_eq!(leaves.len(), 1);
        assert_eq!(leaves[0].event_id, "child");
    }

    #[test]
    fn file_event_store_persist_and_reload() {
        let session_id = format!("test-persist-events-{}", std::process::id());
        let path = events_path_for(&session_id);

        // Clean up from previous runs
        let _ = std::fs::remove_file(&path);

        {
            let mut store = FileBackedEventStore::empty(&session_id);
            store.append(make_event("e1", "s1", StepEventType::StepCreated));
            store.append(make_event("e2", "s1", StepEventType::ToolCallCompleted));
        }

        // Reload from disk
        let store2 = FileBackedEventStore::new(&session_id);
        assert_eq!(store2.event_count(), 2);
        assert_eq!(store2.all_events()[0].event_id, "e1");
        assert_eq!(store2.all_events()[1].event_id, "e2");

        // Clean up
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(session_dir_for(&session_id));
    }

    #[test]
    fn file_event_store_handles_empty_session() {
        let store = FileBackedEventStore::new("nonexistent-event-session-xyz");
        assert_eq!(store.event_count(), 0);
        assert!(store.all_events().is_empty());
    }

    // ── Corruption robustness tests (regression for silent IO fix) ──────

    #[test]
    fn read_heavy_skips_corrupted_json_files() {
        let session_id = format!("test-corrupt-heavy-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid heavy checkpoint
        let heavy = make_heavy("step-ok", vec![json!({"role": "user", "content": "hello"})]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();
        std::fs::write(dir.join("000002-heavy.json"), &json_str).unwrap();

        // Write a corrupted heavy checkpoint with a higher number
        std::fs::write(dir.join("000003-heavy.json"), "NOT VALID JSON{{{").unwrap();

        // read_latest_heavy should attempt 000003 first (highest), fail to parse,
        // and return an InvalidData error — the corruption is not silently swallowed.
        let result = read_latest_heavy_checkpoint(&session_id);
        assert!(
            result.is_err(),
            "Corrupted checkpoint JSON should return error"
        );

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_light_skips_corrupted_json_files() {
        let session_id = format!("test-corrupt-light-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid light checkpoint
        let light = make_light("step-ok", 0.5);
        let cp = StepCheckpoint::Light(light);
        let json_str = serde_json::to_string(&cp).unwrap();
        std::fs::write(dir.join("000001-light.json"), &json_str).unwrap();

        // Write a corrupted light checkpoint with higher number
        std::fs::write(dir.join("000002-light.json"), "GARBAGE").unwrap();

        // read_latest_light tries 000002 first → error
        let result = read_latest_light_checkpoint(&session_id);
        assert!(
            result.is_err(),
            "Corrupted light checkpoint should return error"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn file_event_store_skips_malformed_jsonl_lines() {
        let session_id = format!("test-malformed-jsonl-{}", std::process::id());
        let dir = session_dir_for(&session_id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a JSONL file with some valid and some malformed lines
        let valid_event = make_event("e1", "s1", StepEventType::StepCreated);
        let valid_json = serde_json::to_string(&valid_event).unwrap();

        let content = format!("{}\nNOT VALID JSON\n{{\n{}\n", valid_json, valid_json,);
        std::fs::write(events_path_for(&session_id), &content).unwrap();

        // Load should skip malformed lines, keep valid ones
        let store = FileBackedEventStore::new(&session_id);
        assert_eq!(
            store.event_count(),
            2,
            "Should load 2 valid events, skip 2 malformed lines"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dir_entry_errors_do_not_crash_read_heavy() {
        // Regression: filter_map(|e| e.ok()) was silent. Now logs warnings.
        // This test verifies the function still works when dir entries are fine.
        let session_id = format!("test-dir-ok-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid checkpoint
        let heavy = make_heavy("step-1", vec![]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();
        std::fs::write(dir.join("000001-heavy.json"), &json_str).unwrap();

        let result = read_latest_heavy_checkpoint(&session_id);
        assert!(result.is_ok());
        let cp = result.unwrap();
        assert!(cp.is_some());
        assert_eq!(cp.unwrap().light.step_id, "step-1");

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
