//! Cross-session persistence for pipeline learning modules.
//!
//! Serializes EntityGraph, PatternLibrary, and ProgressiveCalibrator
//! into a single JSON file at `~/.astra/learning/<profile>.json`.
//! On session start, loads and merges prior knowledge; on session end, saves.
//!
//! # Design
//!
//! - One file per profile (user isolation).
//! - Merge-on-load (highest observation count wins) — safe for concurrent sessions.
//! - Atomic write (write to tmp, rename) — no corruption on crash.
//! - Forward-compatible: unknown JSON keys are silently ignored.

use super::calibration::{CalibrationExport, ProgressiveCalibrator};
use super::entity::{EntityGraph, EntityKnowledge};
use super::pattern::{PatternLibrary, ToolChainPattern};
use crate::evolution::service::PersistedActiveCanary;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ─── Snapshot Format ─────────────────────────────────────────────────────────

/// Delta snapshot containing only changed data since last sync.
///
/// Used for incremental sync to reduce network bandwidth.
/// Full snapshot is ~40KB; delta is typically 2-5KB (85-90% reduction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaSnapshot {
    /// Unix timestamp of baseline (last successful sync).
    pub baseline_epoch: u64,
    /// Changed entities since baseline.
    pub entity_deltas: Vec<serde_json::Value>,
    /// Changed patterns since baseline.
    pub pattern_deltas: Vec<serde_json::Value>,
    /// Calibration data (always sent in full, as it's small).
    pub calibration: Option<serde_json::Value>,
    /// Changed tool health entries since baseline.
    pub tool_health_deltas: Vec<serde_json::Value>,
    /// Total count of delta items for statistics.
    pub delta_count: u32,
}

impl DeltaSnapshot {
    /// Check if this delta has any changes.
    pub fn is_empty(&self) -> bool {
        self.delta_count == 0
    }

    /// Approximate size in bytes (for telemetry).
    pub fn approx_size(&self) -> usize {
        self.entity_deltas
            .iter()
            .map(|v| v.to_string().len())
            .sum::<usize>()
            + self
                .pattern_deltas
                .iter()
                .map(|v| v.to_string().len())
                .sum::<usize>()
            + self
                .calibration
                .as_ref()
                .map(|v| v.to_string().len())
                .unwrap_or(0)
            + self
                .tool_health_deltas
                .iter()
                .map(|v| v.to_string().len())
                .sum::<usize>()
    }
}

/// Complete learning state snapshot for one profile.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearningSnapshot {
    /// Format version for forward compatibility.
    pub version: u32,
    /// Epoch seconds when this snapshot was created/exported.
    /// Used for whole-snapshot conflict resolution in cloud sync.
    #[serde(default)]
    pub snapshot_epoch: u64,
    /// Entity knowledge graph entries.
    pub entities: Vec<EntityKnowledge>,
    /// Tool chain patterns.
    pub patterns: Vec<ToolChainPattern>,
    /// Calibration data.
    pub calibration: Option<CalibrationExport>,
    /// Persistent tool health data (cross-session error budgets).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_health: Vec<ToolHealthEntry>,
    /// Active canary state needed to continue bounded promotion/rollback after restart.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_canary: Option<PersistedActiveCanary>,
}

/// Persistent tool health entry for cross-session learning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolHealthEntry {
    pub name: String,
    pub total_calls: usize,
    pub total_failures: usize,
    /// Stored failure rate (0.0-1.0) rather than raw consecutive count.
    /// This avoids carrying session-local "consecutive" state across sessions.
    pub failure_rate: f64,
    /// Epoch seconds when this entry was last updated. Used for conflict resolution:
    /// most-recently-updated wins when merging local and cloud entries.
    #[serde(default)]
    pub last_updated_epoch: u64,
}

/// Local-only sync metadata for cross-session delta sync bookkeeping.
///
/// This is intentionally stored outside the main learning snapshot so it can
/// track "what was last synced to cloud" without polluting the user-facing
/// learning state persisted locally and in cloud.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningSyncMetadata {
    /// Last successfully synced tool health baseline used for delta export.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synced_tool_health: Vec<ToolHealthEntry>,
}

impl Default for LearningSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            snapshot_epoch: 0,
            entities: Vec::new(),
            patterns: Vec::new(),
            calibration: None,
            tool_health: Vec::new(),
            active_canary: None,
        }
    }
}

// ─── File I/O ────────────────────────────────────────────────────────────────

/// Default directory for learning state files.
pub fn learning_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("learning")
}

/// Full path for a profile's learning state file.
pub fn learning_path(profile: &str) -> PathBuf {
    learning_dir().join(format!("{profile}.json"))
}

/// Full path for a profile's local sync metadata file.
pub fn learning_sync_metadata_path(profile: &str) -> PathBuf {
    learning_dir().join(format!("{profile}.sync.json"))
}

/// Load a learning snapshot from disk. Returns `None` if the file doesn't exist
/// or can't be parsed (graceful degradation — never blocks startup).
pub fn load_snapshot(profile: &str) -> Option<LearningSnapshot> {
    let path = learning_path(profile);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Load local sync metadata for a profile.
pub fn load_sync_metadata(profile: &str) -> Option<LearningSyncMetadata> {
    let path = learning_sync_metadata_path(profile);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save a learning snapshot atomically (write to tmp, rename).
/// Returns Ok(()) on success, Err with message on failure (non-fatal).
pub fn save_snapshot(profile: &str, snapshot: &LearningSnapshot) -> Result<(), String> {
    let path = learning_path(profile);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }

    let json = serde_json::to_string_pretty(snapshot).map_err(|e| format!("serialize: {e}"))?;

    // Atomic write: tmp file + rename
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        // Clean up orphaned tmp file before returning error.
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }

    Ok(())
}

/// Save local sync metadata atomically (write to tmp, rename).
pub fn save_sync_metadata(profile: &str, metadata: &LearningSyncMetadata) -> Result<(), String> {
    let path = learning_sync_metadata_path(profile);
    save_sync_metadata_to(&path, metadata)
}

/// Load sync metadata from a custom path (for testing).
pub fn load_sync_metadata_from(path: &Path) -> Option<LearningSyncMetadata> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save sync metadata to a custom path (for testing).
pub fn save_sync_metadata_to(path: &Path, metadata: &LearningSyncMetadata) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }

    let json = serde_json::to_string_pretty(metadata).map_err(|e| format!("serialize: {e}"))?;

    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;

    Ok(())
}

// ─── High-level Operations ───────────────────────────────────────────────────

/// Export current learning state from shared pipeline modules into a snapshot.
pub fn export_from_modules(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) -> LearningSnapshot {
    export_from_modules_with_health(entity_graph, pattern_library, calibrator, &[])
}

/// Export all learning modules into a snapshot, including tool health.
pub fn export_from_modules_with_health(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &[ToolHealthEntry],
) -> LearningSnapshot {
    export_from_modules_with_health_and_canary(
        entity_graph,
        pattern_library,
        calibrator,
        tool_health,
        None,
    )
}

/// Export all learning modules into a snapshot, including tool health and active canary state.
pub fn export_from_modules_with_health_and_canary(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &[ToolHealthEntry],
    active_canary: Option<PersistedActiveCanary>,
) -> LearningSnapshot {
    let entities = entity_graph.lock().map(|g| g.export()).unwrap_or_default();
    let patterns = pattern_library
        .lock()
        .map(|l| l.export())
        .unwrap_or_default();
    let calibration = calibrator.lock().map(|c| c.export()).ok();
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    LearningSnapshot {
        version: 1,
        snapshot_epoch: now_epoch,
        entities,
        patterns,
        calibration,
        tool_health: tool_health.to_vec(),
        active_canary,
    }
}

/// Merge a loaded snapshot into shared pipeline modules.
pub fn merge_into_modules(
    snapshot: &LearningSnapshot,
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) {
    if let Ok(mut graph) = entity_graph.lock() {
        graph.merge(&snapshot.entities);
    }
    if let Ok(mut library) = pattern_library.lock() {
        library.merge(&snapshot.patterns);
    }
    if let Some(ref cal_data) = snapshot.calibration
        && let Ok(mut cal) = calibrator.lock()
    {
        cal.merge(cal_data);
    }
}

/// Merge two sets of tool health entries using timestamp-based conflict resolution.
///
/// Strategy:
/// - For entries present in both local and cloud: most-recently-updated wins
///   (by `last_updated_epoch`). If epochs are equal, higher `total_calls` wins.
///   If both are zero (legacy data without timestamps), local wins.
/// - Cloud-only entries are always added (new cross-device knowledge).
/// - Local-only entries are always kept.
///
/// Returns the merged set and a count of (cloud_wins, cloud_only_added).
pub fn merge_tool_health(
    local: &[ToolHealthEntry],
    cloud: &[ToolHealthEntry],
) -> (Vec<ToolHealthEntry>, usize, usize) {
    use std::collections::HashMap;

    let mut by_name: HashMap<String, ToolHealthEntry> = HashMap::new();
    for entry in local {
        by_name.insert(entry.name.clone(), entry.clone());
    }

    let mut cloud_wins = 0usize;
    let mut cloud_only = 0usize;

    for cloud_entry in cloud {
        match by_name.get(&cloud_entry.name) {
            Some(local_entry) => {
                // Both exist — timestamp wins, fallback to total_calls, fallback to local
                let use_cloud = if cloud_entry.last_updated_epoch != local_entry.last_updated_epoch
                {
                    cloud_entry.last_updated_epoch > local_entry.last_updated_epoch
                } else if cloud_entry.total_calls != local_entry.total_calls {
                    cloud_entry.total_calls > local_entry.total_calls
                } else {
                    false // tie → local wins
                };
                if use_cloud {
                    by_name.insert(cloud_entry.name.clone(), cloud_entry.clone());
                    cloud_wins += 1;
                }
            }
            None => {
                // Cloud-only → always add
                by_name.insert(cloud_entry.name.clone(), cloud_entry.clone());
                cloud_only += 1;
            }
        }
    }

    let merged: Vec<ToolHealthEntry> = by_name.into_values().collect();
    (merged, cloud_wins, cloud_only)
}

/// Save learning state from shared modules to disk.
pub fn save_learning_state(
    profile: &str,
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) -> Result<(), String> {
    save_learning_state_with_health(profile, entity_graph, pattern_library, calibrator, &[])
}

/// Save learning state with tool health data included.
pub fn save_learning_state_with_health(
    profile: &str,
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &[ToolHealthEntry],
) -> Result<(), String> {
    save_learning_state_with_health_and_canary(
        profile,
        entity_graph,
        pattern_library,
        calibrator,
        tool_health,
        None,
    )
}

/// Save learning state with tool health and active canary state included.
pub fn save_learning_state_with_health_and_canary(
    profile: &str,
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &[ToolHealthEntry],
    active_canary: Option<PersistedActiveCanary>,
) -> Result<(), String> {
    let snapshot = export_from_modules_with_health_and_canary(
        entity_graph,
        pattern_library,
        calibrator,
        tool_health,
        active_canary,
    );
    // Only save if there's something to persist
    if snapshot.entities.is_empty()
        && snapshot.patterns.is_empty()
        && snapshot.calibration.is_none()
        && snapshot.tool_health.is_empty()
        && snapshot.active_canary.is_none()
    {
        return Ok(());
    }
    save_snapshot(profile, &snapshot)
}

/// Load learning state from disk and merge into shared modules.
/// Returns true if a snapshot was found and merged.
pub fn load_learning_state(
    profile: &str,
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) -> bool {
    match load_snapshot(profile) {
        Some(snapshot) => {
            merge_into_modules(&snapshot, entity_graph, pattern_library, calibrator);
            true
        }
        None => false,
    }
}

/// Load tool health entries from a profile's learning snapshot.
/// Returns empty vec on missing/corrupt file (graceful degradation).
pub fn load_tool_health(profile: &str) -> Vec<ToolHealthEntry> {
    load_snapshot(profile)
        .map(|s| s.tool_health)
        .unwrap_or_default()
}

/// Load the last successfully synced tool-health baseline for delta sync.
pub fn load_synced_tool_health(profile: &str) -> Vec<ToolHealthEntry> {
    load_sync_metadata(profile)
        .map(|m| m.synced_tool_health)
        .unwrap_or_default()
}

/// Save the last successfully synced tool-health baseline for delta sync.
pub fn save_synced_tool_health(profile: &str, entries: &[ToolHealthEntry]) -> Result<(), String> {
    save_sync_metadata(
        profile,
        &LearningSyncMetadata {
            synced_tool_health: entries.to_vec(),
        },
    )
}

/// Load a snapshot from a custom path (for testing or server-side use).
pub fn load_snapshot_from(path: &Path) -> Option<LearningSnapshot> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

/// Save a snapshot to a custom path (for testing or server-side use).
pub fn save_snapshot_to(path: &Path, snapshot: &LearningSnapshot) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(snapshot).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

// ─── Delta Export ───────────────────────────────────────────────────────────

/// Export only dirty (changed) data from modules since last sync.
///
/// Returns:
/// - `Some(DeltaSnapshot)` if there are any dirty items
/// - `None` if nothing has changed
///
/// After calling this, you should call `clear_dirty()` on each module
/// only after successful sync to cloud.
pub fn export_dirty_from_modules(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &crate::turn::tool_health::ToolHealthTracker,
) -> Option<DeltaSnapshot> {
    let mut delta = DeltaSnapshot {
        baseline_epoch: 0,
        entity_deltas: Vec::new(),
        pattern_deltas: Vec::new(),
        calibration: None,
        tool_health_deltas: Vec::new(),
        delta_count: 0,
    };

    // Export dirty entities
    if let Ok(graph) = entity_graph.lock()
        && graph.has_dirty()
    {
        delta.baseline_epoch = graph.last_sync_epoch();
        let dirty_entities = graph.export_dirty();
        for ent in dirty_entities {
            if let Ok(json) = serde_json::to_value(&ent) {
                delta.entity_deltas.push(json);
                delta.delta_count += 1;
            }
        }
    }

    // Export dirty patterns
    if let Ok(library) = pattern_library.lock()
        && library.has_dirty()
    {
        if delta.baseline_epoch == 0 {
            delta.baseline_epoch = library.last_sync_epoch();
        }
        let dirty_patterns = library.export_dirty();
        for pat in dirty_patterns {
            if let Ok(json) = serde_json::to_value(&pat) {
                delta.pattern_deltas.push(json);
                delta.delta_count += 1;
            }
        }
    }

    // Export calibration if dirty (always sent in full since it's small)
    if let Ok(cal) = calibrator.lock()
        && cal.has_dirty()
    {
        if delta.baseline_epoch == 0 {
            delta.baseline_epoch = cal.last_sync_epoch();
        }
        if let Ok(json) = serde_json::to_value(cal.export()) {
            delta.calibration = Some(json);
            delta.delta_count += 1;
        }
    }

    // Export dirty tool health
    if tool_health.has_dirty() {
        if delta.baseline_epoch == 0 {
            delta.baseline_epoch = tool_health.last_sync_epoch();
        }
        let dirty_tools = tool_health.export_dirty();
        for th in dirty_tools {
            if let Ok(json) = serde_json::to_value(&th) {
                delta.tool_health_deltas.push(json);
                delta.delta_count += 1;
            }
        }
    }

    if delta.delta_count > 0 {
        Some(delta)
    } else {
        None
    }
}

/// Export only dirty learning data from modules since last sync.
///
/// Unlike `export_dirty_from_modules`, this excludes tool health deltas.
/// This is useful for callers that only have access to the persistent
/// tool-health snapshot, not the live `ToolHealthTracker`.
pub fn export_dirty_learning_from_modules(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) -> Option<DeltaSnapshot> {
    let mut delta = DeltaSnapshot {
        baseline_epoch: 0,
        entity_deltas: Vec::new(),
        pattern_deltas: Vec::new(),
        calibration: None,
        tool_health_deltas: Vec::new(),
        delta_count: 0,
    };

    if let Ok(graph) = entity_graph.lock()
        && graph.has_dirty()
    {
        delta.baseline_epoch = graph.last_sync_epoch();
        for ent in graph.export_dirty() {
            if let Ok(json) = serde_json::to_value(&ent) {
                delta.entity_deltas.push(json);
                delta.delta_count += 1;
            }
        }
    }

    if let Ok(library) = pattern_library.lock()
        && library.has_dirty()
    {
        if delta.baseline_epoch == 0 {
            delta.baseline_epoch = library.last_sync_epoch();
        }
        for pat in library.export_dirty() {
            if let Ok(json) = serde_json::to_value(&pat) {
                delta.pattern_deltas.push(json);
                delta.delta_count += 1;
            }
        }
    }

    if let Ok(cal) = calibrator.lock()
        && cal.has_dirty()
    {
        if delta.baseline_epoch == 0 {
            delta.baseline_epoch = cal.last_sync_epoch();
        }
        if let Ok(json) = serde_json::to_value(cal.export()) {
            delta.calibration = Some(json);
            delta.delta_count += 1;
        }
    }

    if delta.delta_count > 0 {
        Some(delta)
    } else {
        None
    }
}

/// Clear dirty flags from all modules after successful sync.
pub fn clear_dirty_in_modules(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &mut crate::turn::tool_health::ToolHealthTracker,
) {
    if let Ok(mut graph) = entity_graph.lock() {
        graph.clear_dirty();
    }
    if let Ok(mut library) = pattern_library.lock() {
        library.clear_dirty();
    }
    if let Ok(mut cal) = calibrator.lock() {
        cal.clear_dirty();
    }
    tool_health.clear_dirty();
}

/// Clear dirty flags from learning modules after successful sync.
pub fn clear_dirty_learning_in_modules(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) {
    if let Ok(mut graph) = entity_graph.lock() {
        graph.clear_dirty();
    }
    if let Ok(mut library) = pattern_library.lock() {
        library.clear_dirty();
    }
    if let Ok(mut cal) = calibrator.lock() {
        cal.clear_dirty();
    }
}

/// Check if any module has dirty data needing sync.
pub fn has_dirty_data(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
    tool_health: &crate::turn::tool_health::ToolHealthTracker,
) -> bool {
    let entities_dirty = entity_graph.lock().map(|g| g.has_dirty()).unwrap_or(false);
    let patterns_dirty = pattern_library
        .lock()
        .map(|l| l.has_dirty())
        .unwrap_or(false);
    let calibration_dirty = calibrator.lock().map(|c| c.has_dirty()).unwrap_or(false);
    let tools_dirty = tool_health.has_dirty();

    entities_dirty || patterns_dirty || calibration_dirty || tools_dirty
}

/// Check if any learning module has dirty data needing sync.
pub fn has_dirty_learning_data(
    entity_graph: &Arc<Mutex<EntityGraph>>,
    pattern_library: &Arc<Mutex<PatternLibrary>>,
    calibrator: &Arc<Mutex<ProgressiveCalibrator>>,
) -> bool {
    let entities_dirty = entity_graph.lock().map(|g| g.has_dirty()).unwrap_or(false);
    let patterns_dirty = pattern_library
        .lock()
        .map(|l| l.has_dirty())
        .unwrap_or(false);
    let calibration_dirty = calibrator.lock().map(|c| c.has_dirty()).unwrap_or(false);

    entities_dirty || patterns_dirty || calibration_dirty
}

/// Export tool health entries that changed relative to the last synced baseline.
pub fn export_tool_health_delta(
    current: &[ToolHealthEntry],
    baseline: &[ToolHealthEntry],
) -> Vec<serde_json::Value> {
    let baseline_map: std::collections::HashMap<&str, &ToolHealthEntry> = baseline
        .iter()
        .map(|entry| (entry.name.as_str(), entry))
        .collect();

    current
        .iter()
        .filter(|entry| match baseline_map.get(entry.name.as_str()) {
            Some(prev) => {
                prev.total_calls != entry.total_calls
                    || prev.total_failures != entry.total_failures
                    || (prev.failure_rate - entry.failure_rate).abs() > f64::EPSILON
                    || prev.last_updated_epoch != entry.last_updated_epoch
            }
            None => true,
        })
        .filter_map(|entry| serde_json::to_value(entry).ok())
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::routing::{DomainHint, TaskType};
    use tempfile::TempDir;

    #[allow(clippy::type_complexity)]
    fn make_modules() -> (
        Arc<Mutex<EntityGraph>>,
        Arc<Mutex<PatternLibrary>>,
        Arc<Mutex<ProgressiveCalibrator>>,
    ) {
        (
            Arc::new(Mutex::new(EntityGraph::new())),
            Arc::new(Mutex::new(PatternLibrary::new())),
            Arc::new(Mutex::new(ProgressiveCalibrator::new(0.15))),
        )
    }

    #[test]
    fn snapshot_roundtrip_json() {
        let snapshot = LearningSnapshot {
            version: 1,
            snapshot_epoch: 0,
            entities: vec![],
            patterns: vec![],
            calibration: None,
            tool_health: Vec::new(),
            active_canary: None,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.version, 1);
    }

    #[test]
    fn snapshot_with_entities_roundtrip() {
        let (eg, pl, cal) = make_modules();

        // Learn something
        {
            let mut graph = eg.lock().unwrap();
            graph.learn(
                "matrixorigin",
                DomainHint::GitHub,
                &["github_search".into()],
                None,
            );
        }
        {
            let mut lib = pl.lock().unwrap();
            lib.record_outcome(
                &["github_search".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
        }
        {
            let mut c = cal.lock().unwrap();
            c.record(
                "fetch",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                false,
                None,
            );
        }

        let snapshot = export_from_modules(&eg, &pl, &cal);
        assert!(!snapshot.entities.is_empty());
        assert!(!snapshot.patterns.is_empty());
        assert!(snapshot.calibration.is_some());

        let json = serde_json::to_string_pretty(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(loaded.entities.len(), snapshot.entities.len());
        assert_eq!(loaded.patterns.len(), snapshot.patterns.len());
    }

    #[test]
    fn save_load_file_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.json");

        let (eg, pl, cal) = make_modules();
        {
            let mut graph = eg.lock().unwrap();
            graph.learn("rust", DomainHint::Code, &["bash".into()], None);
        }

        let snapshot = export_from_modules(&eg, &pl, &cal);
        save_snapshot_to(&path, &snapshot).unwrap();

        let loaded = load_snapshot_from(&path).unwrap();
        assert_eq!(loaded.entities.len(), 1);
        assert_eq!(loaded.entities[0].name, "rust");
    }

    #[test]
    fn merge_combines_knowledge() {
        let (eg1, pl1, cal1) = make_modules();
        let (eg2, pl2, cal2) = make_modules();

        // Module set 1 knows about "rust"
        eg1.lock()
            .unwrap()
            .learn("rust", DomainHint::Code, &["bash".into()], None);

        // Module set 2 knows about "matrixorigin"
        eg2.lock().unwrap().learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into()],
            None,
        );

        // Export set 1, merge into set 2
        let snapshot1 = export_from_modules(&eg1, &pl1, &cal1);
        merge_into_modules(&snapshot1, &eg2, &pl2, &cal2);

        // Set 2 now knows both entities
        let graph = eg2.lock().unwrap();
        assert!(graph.domain_for("rust").is_some());
        assert!(graph.domain_for("matrixorigin").is_some());
    }

    #[test]
    fn load_nonexistent_returns_none() {
        let snapshot = load_snapshot("nonexistent-profile-xyz-12345");
        assert!(snapshot.is_none());
    }

    #[test]
    fn save_skips_empty_state() {
        let (eg, pl, cal) = make_modules();
        // Empty modules — should be a no-op
        let result = save_learning_state("test-empty", &eg, &pl, &cal);
        assert!(result.is_ok());
    }

    #[test]
    fn atomic_write_no_partial_files() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("atomic.json");

        let snapshot = LearningSnapshot::default();
        save_snapshot_to(&path, &snapshot).unwrap();

        // Tmp file should be cleaned up
        assert!(!path.with_extension("json.tmp").exists());
        // Main file should exist
        assert!(path.exists());
    }

    #[test]
    fn forward_compatible_ignores_unknown_fields() {
        let json = r#"{
            "version": 1,
            "entities": [],
            "patterns": [],
            "calibration": null,
            "future_field": "ignored"
        }"#;
        let loaded: LearningSnapshot = serde_json::from_str(json).unwrap();
        assert_eq!(loaded.version, 1);
    }

    #[test]
    fn full_lifecycle_save_then_load_into_fresh_modules() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("lifecycle.json");

        // Session 1: learn and save
        let (eg, pl, cal) = make_modules();
        {
            let mut g = eg.lock().unwrap();
            g.learn(
                "matrixorigin",
                DomainHint::GitHub,
                &["github_search".into()],
                None,
            );
            g.learn(
                "matrixorigin",
                DomainHint::GitHub,
                &["github_search".into()],
                None,
            );
        }
        {
            let mut l = pl.lock().unwrap();
            l.record_outcome(
                &["github_search".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.9,
                None,
            );
            l.record_outcome(
                &["github_search".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.8,
                None,
            );
        }
        {
            let mut c = cal.lock().unwrap();
            c.record(
                "fetch",
                Some(DomainHint::GitHub),
                TaskType::Fetch,
                false,
                None,
            );
        }
        let snapshot = export_from_modules(&eg, &pl, &cal);
        save_snapshot_to(&path, &snapshot).unwrap();

        // Session 2: fresh modules, load prior knowledge
        let (eg2, pl2, cal2) = make_modules();
        let loaded = load_snapshot_from(&path).unwrap();
        merge_into_modules(&loaded, &eg2, &pl2, &cal2);

        // Verify entity graph has knowledge from session 1
        let graph = eg2.lock().unwrap();
        assert_eq!(graph.domain_for("matrixorigin"), Some(DomainHint::GitHub));
        assert!(graph.confidence_for("matrixorigin") > 0.0);

        // Verify pattern library has patterns from session 1
        let lib = pl2.lock().unwrap();
        let suggestions = lib.suggest(TaskType::Fetch, Some(DomainHint::GitHub), 5);
        assert!(!suggestions.is_empty());

        // Verify calibrator has data from session 1
        let c = cal2.lock().unwrap();
        assert!(c.tracked_domain_count() > 0);
    }

    // ── Tool Health Persistence Tests ──

    #[test]
    fn tool_health_roundtrip_in_snapshot() {
        let snapshot = LearningSnapshot {
            version: 1,
            snapshot_epoch: 0,
            entities: vec![],
            patterns: vec![],
            calibration: None,
            tool_health: vec![
                ToolHealthEntry {
                    name: "bash".to_string(),
                    total_calls: 10,
                    total_failures: 3,
                    failure_rate: 0.3,
                    last_updated_epoch: 0,
                },
                ToolHealthEntry {
                    name: "read_file".to_string(),
                    total_calls: 50,
                    total_failures: 0,
                    failure_rate: 0.0,
                    last_updated_epoch: 0,
                },
            ],
            active_canary: None,
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tool_health.len(), 2);
        assert_eq!(loaded.tool_health[0].name, "bash");
        assert!((loaded.tool_health[0].failure_rate - 0.3).abs() < 0.01);
    }

    #[test]
    fn active_canary_roundtrip_in_snapshot() {
        let snapshot = LearningSnapshot {
            version: 1,
            snapshot_epoch: 0,
            entities: vec![],
            patterns: vec![],
            calibration: None,
            tool_health: Vec::new(),
            active_canary: Some(PersistedActiveCanary {
                proposal: crate::evolution::types::EvolutionProposal {
                    id: "canary-1".into(),
                    signal: crate::evolution::types::EvolutionSignal::ToolFailure {
                        tool_name: "bash".into(),
                        error_snippet: "timed out".into(),
                        skill_context: None,
                        turn_id: "t1".into(),
                    },
                    axis: crate::evolution::types::EvolutionAxis::Calibration {
                        axis: crate::evolution::types::CalibrationAxis::Intent("fetch".into()),
                        adjustment: 0.10,
                    },
                    confidence: 0.8,
                    reasoning: "Nudge fetch threshold".into(),
                    created_at: 42,
                    status: crate::evolution::types::ApprovalStatus::CanaryActive,
                    promotion_verdict: Some(crate::evolution::types::ProposalPromotionVerdict {
                        recommendation:
                            crate::evolution::types::ProposalPromotionRecommendation::Canary,
                        confidence_score: 0.8,
                        support_score: 0.62,
                        safety_score: 0.75,
                        overall_score: 0.70,
                        evidence: vec!["persisted".into()],
                        blockers: Vec::new(),
                        rollback_hint: Some("apply inverse calibration adjustment -0.10".into()),
                    }),
                },
                rollback_patterns: None,
                rollback_calibration: Some(ProgressiveCalibrator::default().export()),
            }),
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        let active = loaded
            .active_canary
            .expect("active canary should roundtrip");
        assert_eq!(active.proposal.id, "canary-1");
        assert_eq!(
            active.proposal.status,
            crate::evolution::types::ApprovalStatus::CanaryActive
        );
        assert!(active.rollback_calibration.is_some());
    }

    #[test]
    fn tool_health_backward_compatible_load() {
        // Old snapshots without tool_health field should still parse
        let json = r#"{"version":1,"entities":[],"patterns":[],"calibration":null}"#;
        let loaded: LearningSnapshot = serde_json::from_str(json).unwrap();
        assert!(loaded.tool_health.is_empty());
    }

    #[test]
    fn tool_health_empty_not_serialized() {
        let snapshot = LearningSnapshot::default();
        let json = serde_json::to_string(&snapshot).unwrap();
        // When tool_health is empty, it should be skipped in JSON (skip_serializing_if)
        assert!(!json.contains("tool_health"));
    }

    #[test]
    fn tool_health_persists_to_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test_health.json");
        let snapshot = LearningSnapshot {
            version: 1,
            snapshot_epoch: 0,
            entities: vec![],
            patterns: vec![],
            calibration: None,
            tool_health: vec![ToolHealthEntry {
                name: "github_ci_status".to_string(),
                total_calls: 5,
                total_failures: 4,
                failure_rate: 0.8,
                last_updated_epoch: 0,
            }],
            active_canary: None,
        };
        save_snapshot_to(&path, &snapshot).unwrap();
        let loaded = load_snapshot_from(&path).unwrap();
        assert_eq!(loaded.tool_health.len(), 1);
        assert_eq!(loaded.tool_health[0].name, "github_ci_status");
        assert_eq!(loaded.tool_health[0].total_failures, 4);
    }

    #[test]
    fn sync_metadata_roundtrip() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("default.sync.json");

        let metadata = LearningSyncMetadata {
            synced_tool_health: vec![ToolHealthEntry {
                name: "bash".to_string(),
                total_calls: 7,
                total_failures: 2,
                failure_rate: 2.0 / 7.0,
                last_updated_epoch: 123,
            }],
        };
        save_sync_metadata_to(&path, &metadata).unwrap();
        let loaded = load_sync_metadata_from(&path).unwrap();
        assert_eq!(loaded.synced_tool_health.len(), 1);
        assert_eq!(loaded.synced_tool_health[0].name, "bash");
    }

    #[test]
    fn missing_sync_metadata_returns_empty_tool_health_baseline() {
        let baseline = load_synced_tool_health("missing-sync-metadata-profile-xyz");
        assert!(baseline.is_empty());
    }

    // ── Delta Sync Tests ──

    #[test]
    fn entity_graph_dirty_tracking() {
        let mut graph = EntityGraph::new();

        // Initially not dirty
        assert!(!graph.has_dirty());

        // Learn about an entity
        graph.learn("rust", DomainHint::Code, &["read_file".into()], None);

        // Now dirty
        assert!(graph.has_dirty());

        // Export dirty
        let dirty = graph.export_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].name, "rust");

        // Clear dirty
        graph.clear_dirty();
        assert!(!graph.has_dirty());
        assert!(graph.export_dirty().is_empty());
    }

    #[test]
    fn pattern_library_dirty_tracking() {
        let mut library = PatternLibrary::new();

        // Initially not dirty
        assert!(!library.has_dirty());

        // Record a pattern
        library.record_outcome(
            &["github_search".into(), "github_list_prs".into()],
            TaskType::Fetch,
            Some(DomainHint::GitHub),
            true,
            0.8,
            None,
        );

        // Now dirty
        assert!(library.has_dirty());

        // Export dirty
        let dirty = library.export_dirty();
        assert_eq!(dirty.len(), 1);

        // Clear dirty
        library.clear_dirty();
        assert!(!library.has_dirty());
    }

    #[test]
    fn calibrator_dirty_tracking() {
        let mut cal = ProgressiveCalibrator::new(0.5);

        // Initially not dirty
        assert!(!cal.has_dirty());

        // Record a calibration
        cal.record(
            "github",
            Some(DomainHint::GitHub),
            TaskType::Fetch,
            false,
            None,
        );

        // Now dirty
        assert!(cal.has_dirty());

        // Clear dirty
        cal.clear_dirty();
        assert!(!cal.has_dirty());
    }

    #[test]
    fn tool_health_dirty_tracking() {
        use crate::turn::tool_health::ToolHealthTracker;

        let mut tracker = ToolHealthTracker::new();

        // Initially not dirty
        assert!(!tracker.has_dirty());

        // Record usage
        tracker.record_success("bash");
        assert!(tracker.has_dirty());

        // Export dirty
        let dirty = tracker.export_dirty();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].name, "bash");

        // Clear dirty
        tracker.clear_dirty();
        assert!(!tracker.has_dirty());
    }

    #[test]
    fn delta_snapshot_is_empty_when_no_changes() {
        let delta = DeltaSnapshot {
            baseline_epoch: 0,
            entity_deltas: vec![],
            pattern_deltas: vec![],
            calibration: None,
            tool_health_deltas: vec![],
            delta_count: 0,
        };
        assert!(delta.is_empty());
    }

    #[test]
    fn delta_snapshot_not_empty_with_changes() {
        let delta = DeltaSnapshot {
            baseline_epoch: 0,
            entity_deltas: vec![serde_json::json!({"name": "test"})],
            pattern_deltas: vec![],
            calibration: None,
            tool_health_deltas: vec![],
            delta_count: 1,
        };
        assert!(!delta.is_empty());
    }

    #[test]
    fn tool_health_delta_empty_when_unchanged() {
        let entries = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 3,
            total_failures: 1,
            failure_rate: 1.0 / 3.0,
            last_updated_epoch: 42,
        }];
        let delta = export_tool_health_delta(&entries, &entries);
        assert!(delta.is_empty());
    }

    #[test]
    fn tool_health_delta_includes_changed_entries() {
        let baseline = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 3,
            total_failures: 1,
            failure_rate: 1.0 / 3.0,
            last_updated_epoch: 42,
        }];
        let current = vec![ToolHealthEntry {
            name: "bash".to_string(),
            total_calls: 4,
            total_failures: 1,
            failure_rate: 0.25,
            last_updated_epoch: 99,
        }];
        let delta = export_tool_health_delta(&current, &baseline);
        assert_eq!(delta.len(), 1);
        assert_eq!(delta[0].get("name").and_then(|v| v.as_str()), Some("bash"));
    }
}
