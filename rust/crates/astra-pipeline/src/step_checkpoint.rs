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

use astra_services::SessionArtifactStore;

use crate::journal_crypto::JournalCrypto;
use crate::journal_crypto::hex_decode;
use crate::journal_crypto::hex_encode;
use crate::step_protocol::{
    CheckpointTier, HeavyCheckpoint, LightCheckpoint, StepCheckpoint, StepEvent, StepEventStore,
};
use std::sync::OnceLock;

fn journal_crypto() -> &'static JournalCrypto {
    static CRYPTO: OnceLock<JournalCrypto> = OnceLock::new();
    CRYPTO.get_or_init(JournalCrypto::from_env_or_local_key)
}

/// Directory name within session workspace for step checkpoints.
const STEP_CHECKPOINT_DIR: &str = "step_checkpoints";

/// Maximum number of light checkpoints to retain (older ones pruned).
const MAX_LIGHT_CHECKPOINTS: usize = 50;

/// Get the step checkpoint directory for a session.
/// Decrypt checkpoint content. All checkpoints are encrypted.
pub(crate) fn decrypt_checkpoint(content: &str) -> Option<String> {
    let bytes = hex_decode(content.trim())?;
    let decrypted = journal_crypto().decrypt(&bytes)?;
    String::from_utf8(decrypted).ok()
}

fn encrypt_checkpoint(content: &str) -> String {
    let encrypted = journal_crypto().encrypt(content.as_bytes());
    hex_encode(&encrypted)
}

fn sync_parent_dir(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    std::fs::File::open(parent)?.sync_all()
}

fn write_atomic_encrypted_text(path: &Path, content: &str) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "encrypted artifact path must have a parent directory",
        ));
    };
    std::fs::create_dir_all(dir)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid file name")
        })?;
    let tmp_path = dir.join(format!(".tmp-{file_name}"));
    let encrypted = encrypt_checkpoint(content);
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(encrypted.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp_path, path)?;
    sync_parent_dir(path)
}

fn append_encrypted_line(path: &Path, content: &str) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "encrypted artifact path must have a parent directory",
        ));
    };
    std::fs::create_dir_all(dir)?;
    let existed = path.exists();
    let needs_leading_newline = existed && file_needs_trailing_newline(path)?;
    let encrypted = encrypt_checkpoint(content);
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    if needs_leading_newline {
        file.write_all(b"\n")?;
    }
    writeln!(file, "{encrypted}")?;
    file.sync_data()?;
    if !existed {
        sync_parent_dir(path)?;
    }
    Ok(())
}

fn file_needs_trailing_newline(path: &Path) -> std::io::Result<bool> {
    let metadata = std::fs::metadata(path)?;
    if metadata.len() == 0 {
        return Ok(false);
    }

    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::End(-1))?;
    let mut last = [0_u8; 1];
    file.read_exact(&mut last)?;
    Ok(last[0] != b'\n')
}

fn checkpoint_dir_for(session_id: &str) -> std::io::Result<PathBuf> {
    astra_services::local_session_artifact_store()
        .session_path(session_id, STEP_CHECKPOINT_DIR)
        .map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("invalid session_id for checkpoint dir: {e}"),
            )
        })
}

/// Write a step checkpoint to local filesystem.
/// Returns the path where the checkpoint was written.
pub fn write_step_checkpoint(
    session_id: &str,
    number: u32,
    checkpoint: &StepCheckpoint,
) -> std::io::Result<PathBuf> {
    let dir = checkpoint_dir_for(session_id)?;
    std::fs::create_dir_all(&dir)?;

    let tier = match checkpoint {
        StepCheckpoint::Light(_) => "light",
        StepCheckpoint::Heavy(_) => "heavy",
    };
    let filename = format!("{:06}-{}.json", number, tier);
    let path = dir.join(&filename);

    let json = serde_json::to_string(checkpoint)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    write_atomic_encrypted_text(&path, &json)?;

    // Prune old light checkpoints if too many
    if tier == "light" {
        prune_light_checkpoints(&dir)?;
    }

    Ok(path)
}

/// Delete a step checkpoint by number and tier.
pub fn delete_step_checkpoint(session_id: &str, number: u32, tier: &str) -> std::io::Result<()> {
    let dir = checkpoint_dir_for(session_id)?;
    let filename = format!("{:06}-{}.json", number, tier);
    let path = dir.join(&filename);
    match std::fs::remove_file(&path) {
        Ok(()) => sync_parent_dir(&path)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    Ok(())
}

/// Read the latest heavy checkpoint for session recovery.
/// Returns None if no heavy checkpoint exists.
pub fn read_latest_heavy_checkpoint(session_id: &str) -> std::io::Result<Option<HeavyCheckpoint>> {
    let dir = checkpoint_dir_for(session_id)?;
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

    for entry in &heavy_files {
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(e) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Skipping unreadable checkpoint {:?}: {}",
                    entry.file_name(),
                    e
                );
                continue;
            }
        };
        let decrypted = match decrypt_checkpoint(&content) {
            Some(plain) => plain,
            None => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Skipping heavy checkpoint {:?}: decryption failed (key rotation or tampering?)",
                    entry.file_name()
                );
                continue;
            }
        };
        let checkpoint: StepCheckpoint = match serde_json::from_str(&decrypted) {
            Ok(cp) => cp,
            Err(e) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Skipping corrupted checkpoint {:?}: {}",
                    entry.file_name(),
                    e
                );
                continue;
            }
        };
        match checkpoint {
            StepCheckpoint::Heavy(boxed) => return Ok(Some(*boxed)),
            _ => continue,
        }
    }
    Ok(None)
}

/// Read the latest light checkpoint (for quick cursor restore).
pub fn read_latest_light_checkpoint(session_id: &str) -> std::io::Result<Option<LightCheckpoint>> {
    let dir = checkpoint_dir_for(session_id)?;
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

    for entry in &all_files {
        let content = match std::fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(e) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Skipping unreadable checkpoint {:?}: {}",
                    entry.file_name(),
                    e
                );
                continue;
            }
        };
        let decrypted = match decrypt_checkpoint(&content) {
            Some(plain) => plain,
            None => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Skipping checkpoint {:?}: decryption failed (key rotation or tampering?)",
                    entry.file_name()
                );
                continue;
            }
        };
        let checkpoint: StepCheckpoint = match serde_json::from_str(&decrypted) {
            Ok(cp) => cp,
            Err(e) => {
                astra_core::agent_warn!(
                    "checkpoint",
                    "Skipping corrupted checkpoint {:?}: {}",
                    entry.file_name(),
                    e
                );
                continue;
            }
        };
        match checkpoint {
            StepCheckpoint::Light(light) => return Ok(Some(light)),
            StepCheckpoint::Heavy(heavy) => return Ok(Some(heavy.light)),
        }
    }
    Ok(None)
}

/// List all checkpoint numbers and tiers for a session.
pub fn list_checkpoints(session_id: &str) -> std::io::Result<Vec<(u32, CheckpointTier)>> {
    let dir = checkpoint_dir_for(session_id)?;
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
pub fn read_breakpoint_index(
    session_id: &str,
) -> std::io::Result<crate::step_protocol::BreakpointIndex> {
    let path = checkpoint_dir_for(session_id)?.join("breakpoints.json");
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
    let dir = checkpoint_dir_for(session_id)?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("composite_snapshots.json");
    let json = serde_json::to_string_pretty(index)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    write_atomic_encrypted_text(&path, &json)
}

/// Read the composite snapshot index from disk.
pub fn read_composite_snapshot_index(
    session_id: &str,
) -> std::io::Result<astra_core::composite_snapshot::CompositeSnapshotIndex> {
    let path = checkpoint_dir_for(session_id)?.join("composite_snapshots.json");
    if !path.exists() {
        return Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let decrypted = match decrypt_checkpoint(&content) {
        Some(plain) => plain,
        None => {
            astra_core::agent_warn!(
                "checkpoint",
                "composite_snapshots.json decryption failed (key rotation or tampering?), returning empty index"
            );
            return Ok(astra_core::composite_snapshot::CompositeSnapshotIndex::default());
        }
    };
    let mut index: astra_core::composite_snapshot::CompositeSnapshotIndex =
        serde_json::from_str(&decrypted)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    index.normalize_versions();
    Ok(index)
}

// ═══════════════════════════════════════════════════════════════════════════════
// File-Backed StepEventStore (JSONL)
// ═══════════════════════════════════════════════════════════════════════════════

/// File path for step events JSONL.
pub(crate) fn events_path_for(session_id: &str) -> std::io::Result<PathBuf> {
    Ok(session_dir_for(session_id)?.join("step_events.jsonl"))
}

pub(crate) fn session_dir_for(session_id: &str) -> std::io::Result<PathBuf> {
    astra_services::local_session_artifact_store()
        .session_dir(session_id)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))
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
        let events = Self::load_events_lenient(session_id);
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

    fn parse_event_line(line: &str) -> std::io::Result<Option<StepEvent>> {
        if line.trim().is_empty() {
            return Ok(None);
        }
        let Some(json) = decrypt_checkpoint(line) else {
            tracing::warn!("skipping undecryptable step event JSONL line");
            return Ok(None);
        };
        match serde_json::from_str::<StepEvent>(&json) {
            Ok(event) => Ok(Some(event)),
            Err(error)
                if matches!(
                    error.classify(),
                    serde_json::error::Category::Syntax | serde_json::error::Category::Eof
                ) =>
            {
                tracing::warn!(error = %error, "skipping malformed step event JSONL line");
                Ok(None)
            }
            Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
        }
    }

    fn load_events_lenient(session_id: &str) -> Vec<StepEvent> {
        let path = match events_path_for(session_id) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = %error,
                    "invalid session_id while loading step events leniently"
                );
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        if !path.exists() {
            return events;
        }

        use std::io::BufRead;
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(
                    session_id,
                    error = %error,
                    "failed to open step events for lenient replay"
                );
                return events;
            }
        };
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        error = %error,
                        "failed to read step event line during lenient replay"
                    );
                    break;
                }
            };
            match Self::parse_event_line(&line) {
                Ok(Some(event)) => events.push(event),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(
                        session_id,
                        error = %error,
                        "skipping invalid step event during lenient replay"
                    );
                }
            }
        }
        events
    }

    fn load_events_matching(
        session_id: &str,
        mut keep: impl FnMut(&StepEvent) -> bool,
    ) -> std::io::Result<Vec<StepEvent>> {
        let mut events = Vec::new();
        Self::for_each_event(session_id, |event| {
            if keep(event) {
                events.push(event.clone());
            }
        })?;
        Ok(events)
    }

    /// Stream persisted events without materializing the whole journal.
    pub fn for_each_event(
        session_id: &str,
        mut visit: impl FnMut(&StepEvent),
    ) -> std::io::Result<()> {
        let path = events_path_for(session_id)?;
        if !path.exists() {
            return Ok(());
        }
        use std::io::BufRead;
        let file = std::fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        for line in reader.lines() {
            let line = line?;
            if let Some(event) = Self::parse_event_line(&line)? {
                visit(&event);
            }
        }
        Ok(())
    }

    /// Stream only events written at or after a checkpoint timestamp. Recovery
    /// uses this instead of materializing the entire long-session journal.
    pub fn load_events_created_at_or_after(
        session_id: &str,
        checkpoint_created_at: u64,
    ) -> std::io::Result<Vec<StepEvent>> {
        Self::load_events_matching(session_id, |event| {
            event.created_at >= checkpoint_created_at
        })
    }

    /// Append a single event to the JSONL file.
    fn persist_event(&self, event: &StepEvent) -> std::io::Result<()> {
        let dir = session_dir_for(&self.session_id)?;
        std::fs::create_dir_all(&dir)?;
        let path = events_path_for(&self.session_id)?;
        let json = serde_json::to_string(event)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        append_encrypted_line(&path, &json)
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
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
            interruption: None,
            approval_overrides: None,
            consecutive_context_window_errors: 0,
            compaction_state: None,
            pipeline_state: None,
            config_version_id: None,
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
        let dir = checkpoint_dir_for(&session_id).unwrap();

        // Clean up from any previous run
        let _ = std::fs::remove_dir_all(&dir);

        let light = make_light("step-write-test", 1.0);
        let cp = StepCheckpoint::Light(light);
        let result = write_step_checkpoint(&session_id, 1, &cp);
        assert!(result.is_ok());
        let path = result.unwrap();
        assert!(path.exists());

        // Read back (decrypt the hex-encoded encrypted content)
        let raw = std::fs::read_to_string(&path).unwrap();
        let json = decrypt_checkpoint(&raw).expect("decrypt checkpoint");
        let restored: StepCheckpoint = serde_json::from_str(&json).unwrap();
        match restored {
            StepCheckpoint::Light(l) => assert_eq!(l.step_id, "step-write-test"),
            _ => panic!("Expected Light"),
        }

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn delete_step_checkpoint_removes_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "delete-existing";
        let checkpoint = StepCheckpoint::Light(make_light("step-delete", 1.0));
        let path = write_step_checkpoint(session_id, 7, &checkpoint).unwrap();
        assert!(path.exists());

        delete_step_checkpoint(session_id, 7, "light").unwrap();

        assert!(!path.exists());
    }

    #[test]
    fn delete_step_checkpoint_ignores_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());

        delete_step_checkpoint("delete-missing", 99, "heavy").unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn delete_step_checkpoint_surfaces_permission_denied() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "delete-perms";
        let checkpoint = StepCheckpoint::Light(make_light("step-delete-perms", 1.0));
        let path = write_step_checkpoint(session_id, 3, &checkpoint).unwrap();
        let dir = path.parent().expect("checkpoint dir").to_path_buf();

        let original_permissions = std::fs::metadata(&dir).unwrap().permissions();
        let mut readonly_permissions = original_permissions.clone();
        readonly_permissions.set_mode(0o555);
        std::fs::set_permissions(&dir, readonly_permissions).unwrap();

        let result = delete_step_checkpoint(session_id, 3, "light");

        std::fs::set_permissions(&dir, original_permissions).unwrap();

        let error = result.expect_err("readonly checkpoint dir should deny deletion");
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(
            path.exists(),
            "failed delete must leave checkpoint untouched"
        );
    }

    #[test]
    fn composite_snapshot_index_is_encrypted_at_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(tmp.path());
        let session_id = "test-composite-snapshot-index";

        let index = astra_core::composite_snapshot::CompositeSnapshotIndex::default();
        write_composite_snapshot_index(session_id, &index).unwrap();

        let raw = std::fs::read_to_string(
            checkpoint_dir_for(session_id)
                .unwrap()
                .join("composite_snapshots.json"),
        )
        .unwrap();
        assert!(
            !raw.trim_start().starts_with('{'),
            "composite snapshot index should be encrypted at rest"
        );

        let restored = read_composite_snapshot_index(session_id).unwrap();
        assert!(restored.snapshots.is_empty());
    }

    #[test]
    fn read_latest_heavy_on_empty_returns_none() {
        let result = read_latest_heavy_checkpoint("nonexistent-session-xyz-42");
        assert!(result.is_ok());
        assert!(result.unwrap().is_none());
    }

    // ── FileBackedEventStore tests ──────────────────────────────────────────

    use crate::step_protocol::StepEventType;

    fn unique_session_id(prefix: &str) -> String {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{prefix}-{}-{now}", std::process::id())
    }

    fn make_event(id: &str, step_id: &str, event_type: StepEventType) -> StepEvent {
        StepEvent {
            event_id: id.to_string(),
            step_id: step_id.to_string(),
            event_type,
            agent_id: None,
            caused_by: vec![],
            payload: None,
            canonical_event_id: None,
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
        let path = events_path_for(&session_id).unwrap();

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
        let _ = std::fs::remove_dir(session_dir_for(&session_id).unwrap());
    }

    #[test]
    fn file_event_store_recovers_valid_events_after_torn_encrypted_tail() {
        let session_id = unique_session_id("test-torn-encrypted-tail");
        let path = events_path_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(&session_id).unwrap());

        {
            let mut store = FileBackedEventStore::empty(&session_id);
            store.append(make_event("e1", "s1", StepEventType::StepCreated));
            store.append(make_event("e2", "s1", StepEventType::ToolCallCompleted));
        }

        let torn_json =
            serde_json::to_string(&make_event("torn", "s1", StepEventType::ToolCallCompleted))
                .unwrap();
        let torn_encrypted = encrypt_checkpoint(&torn_json);
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&torn_encrypted.as_bytes()[..17]).unwrap();
        }

        let store = FileBackedEventStore::new(&session_id);
        let ids: Vec<_> = store
            .all_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let recovered = FileBackedEventStore::load_events_created_at_or_after(&session_id, 0)
            .expect("torn encrypted tail must not fail recovery");
        let ids: Vec<_> = recovered
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let _ = std::fs::remove_dir_all(session_dir_for(&session_id).unwrap());
    }

    #[test]
    fn append_after_torn_encrypted_tail_keeps_new_events_readable() {
        let session_id = unique_session_id("test-append-after-torn-encrypted-tail");
        let path = events_path_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(&session_id).unwrap());

        {
            let mut store = FileBackedEventStore::empty(&session_id);
            store.append(make_event("e1", "s1", StepEventType::StepCreated));
        }

        let torn_json =
            serde_json::to_string(&make_event("torn", "s1", StepEventType::ToolCallCompleted))
                .unwrap();
        let torn_encrypted = encrypt_checkpoint(&torn_json);
        {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap();
            file.write_all(&torn_encrypted.as_bytes()[..17]).unwrap();
        }

        {
            let mut store = FileBackedEventStore::empty(&session_id);
            store.append(make_event("e2", "s1", StepEventType::ToolCallCompleted));
        }

        let store = FileBackedEventStore::new(&session_id);
        let ids: Vec<_> = store
            .all_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let _ = std::fs::remove_dir_all(session_dir_for(&session_id).unwrap());
    }

    #[test]
    fn file_event_store_loads_recovery_window_without_full_store_materialization() {
        let session_id = format!("test-recovery-window-{}", std::process::id());
        let path = events_path_for(&session_id).unwrap();
        let _ = std::fs::remove_file(&path);

        {
            let mut store = FileBackedEventStore::empty(&session_id);
            for idx in 0..10 {
                let mut event =
                    make_event(&format!("e{idx}"), "s1", StepEventType::ToolCallCompleted);
                event.created_at = idx * 100;
                store.append(event);
            }
        }

        let events = FileBackedEventStore::load_events_created_at_or_after(&session_id, 500)
            .expect("load recovery window");
        let ids: Vec<_> = events.iter().map(|event| event.event_id.as_str()).collect();
        assert_eq!(ids, vec!["e5", "e6", "e7", "e8", "e9"]);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(session_dir_for(&session_id).unwrap());
    }

    #[test]
    fn file_event_store_fails_on_corrupt_event_json() {
        let session_id = format!("test-corrupt-event-json-{}", std::process::id());
        let path = events_path_for(&session_id).unwrap();
        let _ = std::fs::remove_file(&path);
        std::fs::create_dir_all(session_dir_for(&session_id).unwrap()).expect("session dir");
        append_encrypted_line(&path, r#"{"not":"a step event"}"#).expect("append corrupt event");

        let error = FileBackedEventStore::load_events_created_at_or_after(&session_id, 0)
            .expect_err("corrupt event json should fail recovery");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(session_dir_for(&session_id).unwrap());
    }

    #[test]
    fn file_event_store_new_skips_invalid_event_without_emptying_history() {
        let session_id = unique_session_id("test-lenient-invalid-event");
        let path = events_path_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(session_dir_for(&session_id).unwrap());
        std::fs::create_dir_all(session_dir_for(&session_id).unwrap()).expect("session dir");

        let e1 = serde_json::to_string(&make_event("e1", "s1", StepEventType::StepCreated))
            .expect("serialize e1");
        let e2 = serde_json::to_string(&make_event("e2", "s1", StepEventType::ToolCallCompleted))
            .expect("serialize e2");
        let content = format!(
            "{}\n{}\n{}\n",
            encrypt_checkpoint(&e1),
            encrypt_checkpoint(r#"{"not":"a step event"}"#),
            encrypt_checkpoint(&e2)
        );
        std::fs::write(&path, content).expect("write mixed event log");

        let store = FileBackedEventStore::new(&session_id);
        let ids: Vec<_> = store
            .all_events()
            .iter()
            .map(|event| event.event_id.as_str())
            .collect();
        assert_eq!(ids, vec!["e1", "e2"]);

        let _ = std::fs::remove_dir_all(session_dir_for(&session_id).unwrap());
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
        let dir = checkpoint_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid heavy checkpoint (encrypted, as production writes)
        let heavy = make_heavy("step-ok", vec![json!({"role": "user", "content": "hello"})]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();
        let encrypted = encrypt_checkpoint(&json_str);
        std::fs::write(dir.join("000002-heavy.json"), &encrypted).unwrap();

        // Write a corrupted heavy checkpoint with a higher number
        std::fs::write(dir.join("000003-heavy.json"), "NOT VALID JSON{{{").unwrap();

        // read_latest_heavy should skip 000003 (corrupted) and fall back to 000002 (valid).
        let result = read_latest_heavy_checkpoint(&session_id);
        assert!(
            result.is_ok(),
            "Corrupted checkpoint must not propagate error: {:?}",
            result.err()
        );
        let cp = result.unwrap();
        assert!(cp.is_some(), "must fall back to valid checkpoint");
        assert_eq!(cp.unwrap().light.step_id, "step-ok");

        // Clean up
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_light_skips_corrupted_json_files() {
        let session_id = format!("test-corrupt-light-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid light checkpoint (encrypted, as production writes)
        let light = make_light("step-ok", 0.5);
        let cp = StepCheckpoint::Light(light);
        let json_str = serde_json::to_string(&cp).unwrap();
        let encrypted = encrypt_checkpoint(&json_str);
        std::fs::write(dir.join("000001-light.json"), &encrypted).unwrap();

        // Write a corrupted light checkpoint with higher number
        std::fs::write(dir.join("000002-light.json"), "GARBAGE").unwrap();

        // read_latest_light tries 000002 first → corrupted → falls back to 000001
        let result = read_latest_light_checkpoint(&session_id);
        assert!(
            result.is_ok(),
            "Corrupted light checkpoint must not propagate error: {:?}",
            result.err()
        );
        let cp = result.unwrap();
        assert!(cp.is_some(), "must fall back to valid checkpoint");
        assert_eq!(cp.unwrap().step_id, "step-ok");

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn file_event_store_skips_malformed_jsonl_lines() {
        let session_id = format!("test-malformed-jsonl-{}", std::process::id());
        let dir = session_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a JSONL file with some valid and some malformed lines
        let valid_event = make_event("e1", "s1", StepEventType::StepCreated);
        let valid_json = serde_json::to_string(&valid_event).unwrap();

        // Encrypt all lines (both valid and malformed) before writing
        let encrypted_valid = encrypt_checkpoint(&valid_json);
        let encrypted_malformed = encrypt_checkpoint("NOT VALID JSON");
        let encrypted_malformed2 = encrypt_checkpoint("{");

        let content = format!(
            "{}\n{}\n{}\n{}\n",
            encrypted_valid, encrypted_malformed, encrypted_malformed2, encrypted_valid
        );
        std::fs::write(events_path_for(&session_id).unwrap(), &content).unwrap();

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
        let dir = checkpoint_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid checkpoint (encrypted, as production writes)
        let heavy = make_heavy("step-1", vec![]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();
        let encrypted = encrypt_checkpoint(&json_str);
        std::fs::write(dir.join("000001-heavy.json"), &encrypted).unwrap();

        let result = read_latest_heavy_checkpoint(&session_id);
        assert!(result.is_ok());
        let cp = result.unwrap();
        assert!(cp.is_some());
        assert_eq!(cp.unwrap().light.step_id, "step-1");

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// P2-I: When the latest heavy checkpoint is corrupted, recovery must
    /// fall back to the previous valid checkpoint instead of returning an error.
    #[test]
    fn corrupted_latest_checkpoint_falls_back_to_previous() {
        let session_id = format!("test-corrupt-fallback-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid checkpoint as #1 (encrypted, as production writes)
        let heavy = make_heavy(
            "step-valid",
            vec![json!({"role": "user", "content": "hello"})],
        );
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();
        let encrypted = encrypt_checkpoint(&json_str);
        std::fs::write(dir.join("000001-heavy.json"), &encrypted).unwrap();

        // Write a CORRUPTED checkpoint as #2 (latest)
        std::fs::write(dir.join("000002-heavy.json"), "{{{{CORRUPTED JSON!!!!").unwrap();

        // Recovery must return the valid checkpoint, not an error
        let result = read_latest_heavy_checkpoint(&session_id);
        assert!(
            result.is_ok(),
            "corrupted latest must not propagate error: {:?}",
            result.err()
        );
        let cp = result.unwrap();
        assert!(cp.is_some(), "must fall back to previous valid checkpoint");
        assert_eq!(
            cp.unwrap().light.step_id,
            "step-valid",
            "must return the valid checkpoint, not the corrupted one"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    /// P1-E: write_step_checkpoint uses fsync before rename.
    /// Simulates power loss by truncating the temp file to 0 bytes after write
    /// but before rename. The read path must fall back to the previous checkpoint.
    /// P1-E: Orphaned temp files (from interrupted writes) must be ignored
    /// by the checkpoint reader, falling back to the previous valid checkpoint.
    #[test]
    fn orphaned_temp_file_ignored_by_reader() {
        let session_id = format!("test-fsync-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a valid checkpoint first (encrypted, as production writes)
        let heavy = make_heavy("step-valid", vec![]);
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json_str = serde_json::to_string(&cp).unwrap();
        let encrypted = encrypt_checkpoint(&json_str);
        std::fs::write(dir.join("000001-heavy.json"), &encrypted).unwrap();

        // Simulate power loss: a corrupted temp file left behind (never renamed)
        // This represents a crash after write but before rename.
        std::fs::write(dir.join(".tmp-000002-heavy.json"), b"").unwrap();

        // The read path must return the valid checkpoint, ignoring the temp file
        let result = read_latest_heavy_checkpoint(&session_id);
        let cp = result
            .expect("must succeed")
            .expect("must find valid checkpoint");
        assert_eq!(
            cp.light.step_id, "step-valid",
            "must return valid checkpoint, not be confused by orphaned temp file"
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    // =======================================================================
    // Regression: checkpoint_dir_for must NOT panic on invalid session_id (C2)
    // =======================================================================

    #[test]
    fn checkpoint_dir_for_returns_err_on_traversal_session_id() {
        // Regression: checkpoint_dir_for used .expect() which would panic
        // on invalid session_id (e.g. path traversal). Must return Err.
        let result = checkpoint_dir_for("../../etc/passwd");
        assert!(
            result.is_err(),
            "checkpoint_dir_for must return Err for path-traversal session_id, \
             got Ok({:?})",
            result
        );
    }

    #[test]
    fn checkpoint_dir_for_returns_err_on_empty_session_id() {
        let result = checkpoint_dir_for("");
        assert!(
            result.is_err(),
            "checkpoint_dir_for must return Err for empty session_id, got Ok({:?})",
            result
        );
    }

    #[test]
    fn write_step_checkpoint_returns_err_on_invalid_session_id() {
        let light = make_light("step-invalid-id", 1.0);
        let cp = StepCheckpoint::Light(light);
        let result = write_step_checkpoint("../../etc/passwd", 1, &cp);
        assert!(
            result.is_err(),
            "write_step_checkpoint must return Err for invalid session_id, got Ok"
        );
    }

    #[test]
    fn read_latest_heavy_checkpoint_returns_err_on_invalid_session_id() {
        let result = read_latest_heavy_checkpoint("../../etc/passwd");
        assert!(
            result.is_err(),
            "read_latest_heavy_checkpoint must return Err for invalid session_id, got Ok"
        );
    }

    // =======================================================================
    // Regression: decrypt_checkpoint must NOT fall back to plaintext (W1)
    // =======================================================================

    #[test]
    fn decrypt_checkpoint_returns_none_for_plaintext_json() {
        // Plain JSON is not valid hex → hex_decode fails → returns None.
        let plaintext = r#"{"heavy":{"turn":1}}"#;
        assert!(
            decrypt_checkpoint(plaintext).is_none(),
            "decrypt_checkpoint must return None for non-encrypted content"
        );
    }

    #[test]
    fn read_latest_heavy_checkpoint_skips_unencrypted_file() {
        // Write a plaintext checkpoint file (simulating an attacker-written file
        // or a corrupted encrypted file that fails decryption).
        let session_id = format!("test-unencrypted-heavy-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Write a plaintext JSON file (not hex-encoded, not encrypted)
        let bad_path = dir.join("000099-heavy.json");
        std::fs::write(&bad_path, r#"{"Light":{"step_id":"malicious"}}"#).unwrap();

        // read_latest_heavy_checkpoint must skip it and return Ok(None).
        let result = read_latest_heavy_checkpoint(&session_id).unwrap();
        assert!(
            result.is_none(),
            "unencrypted heavy checkpoint file must be rejected, got {:?}",
            result
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_latest_light_checkpoint_skips_unencrypted_file() {
        let session_id = format!("test-unencrypted-light-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let bad_path = dir.join("000099-light.json");
        std::fs::write(&bad_path, r#"{"Light":{"step_id":"malicious"}}"#).unwrap();

        let result = read_latest_light_checkpoint(&session_id).unwrap();
        assert!(
            result.is_none(),
            "unencrypted light checkpoint file must be rejected, got {:?}",
            result
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn read_composite_snapshot_index_returns_default_on_unencrypted_file() {
        let session_id = format!("test-unencrypted-composite-{}", std::process::id());
        let dir = checkpoint_dir_for(&session_id).unwrap();
        std::fs::create_dir_all(&dir).unwrap();

        let bad_path = dir.join("composite_snapshots.json");
        std::fs::write(&bad_path, r#"{"snapshots":[]}"#).unwrap();

        // Must return empty index, not parse the plaintext.
        let result = read_composite_snapshot_index(&session_id).unwrap();
        assert!(
            result.snapshots.is_empty(),
            "unencrypted composite snapshot must return empty index, got {} snapshots",
            result.snapshots.len()
        );

        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }
}
