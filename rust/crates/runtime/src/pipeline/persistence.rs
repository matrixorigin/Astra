//! Cross-session persistence for pipeline learning modules.
//!
//! Serializes EntityGraph, PatternLibrary, and ProgressiveCalibrator
//! into a single JSON file at `~/.mo-agent/learning/<profile>.json`.
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
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

// ─── Snapshot Format ─────────────────────────────────────────────────────────

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
    /// Optional for backward compatibility with existing snapshots.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_health: Vec<ToolHealthEntry>,
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

impl Default for LearningSnapshot {
    fn default() -> Self {
        Self {
            version: 1,
            snapshot_epoch: 0,
            entities: Vec::new(),
            patterns: Vec::new(),
            calibration: None,
            tool_health: Vec::new(),
        }
    }
}

// ─── File I/O ────────────────────────────────────────────────────────────────

/// Default directory for learning state files.
pub fn learning_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".mo-agent")
        .join("learning")
}

/// Full path for a profile's learning state file.
pub fn learning_path(profile: &str) -> PathBuf {
    learning_dir().join(format!("{profile}.json"))
}

/// Load a learning snapshot from disk. Returns `None` if the file doesn't exist
/// or can't be parsed (graceful degradation — never blocks startup).
pub fn load_snapshot(profile: &str) -> Option<LearningSnapshot> {
    let path = learning_path(profile);
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
    std::fs::rename(&tmp, &path).map_err(|e| format!("rename: {e}"))?;

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
    let snapshot =
        export_from_modules_with_health(entity_graph, pattern_library, calibrator, tool_health);
    // Only save if there's something to persist
    if snapshot.entities.is_empty()
        && snapshot.patterns.is_empty()
        && snapshot.calibration.is_none()
        && snapshot.tool_health.is_empty()
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
            );
        }
        {
            let mut c = cal.lock().unwrap();
            c.record("fetch", Some(DomainHint::GitHub), TaskType::Fetch, false);
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
            graph.learn("rust", DomainHint::Code, &["bash".into()]);
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
            .learn("rust", DomainHint::Code, &["bash".into()]);

        // Module set 2 knows about "matrixorigin"
        eg2.lock().unwrap().learn(
            "matrixorigin",
            DomainHint::GitHub,
            &["github_search".into()],
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
            );
            g.learn(
                "matrixorigin",
                DomainHint::GitHub,
                &["github_search".into()],
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
            );
            l.record_outcome(
                &["github_search".into()],
                TaskType::Fetch,
                Some(DomainHint::GitHub),
                true,
                0.8,
            );
        }
        {
            let mut c = cal.lock().unwrap();
            c.record("fetch", Some(DomainHint::GitHub), TaskType::Fetch, false);
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
        };
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tool_health.len(), 2);
        assert_eq!(loaded.tool_health[0].name, "bash");
        assert!((loaded.tool_health[0].failure_rate - 0.3).abs() < 0.01);
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
        };
        save_snapshot_to(&path, &snapshot).unwrap();
        let loaded = load_snapshot_from(&path).unwrap();
        assert_eq!(loaded.tool_health.len(), 1);
        assert_eq!(loaded.tool_health[0].name, "github_ci_status");
        assert_eq!(loaded.tool_health[0].total_failures, 4);
    }
}
