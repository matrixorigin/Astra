//! Cross-session persistence for per-tool health.
//!
//! Stored at `~/.astra/tool-health/<profile>.json` as a tool-health snapshot.
//!
//! Local sync metadata (last-synced baseline for delta push) lives in a
//! separate file `<profile>.sync.json` so the user-facing learning state is
//! not polluted by sync bookkeeping.
//!
//! # Design
//!
//! - One file per profile (user isolation).
//! - Load persisted snapshots; the tracker preserves historical entries when exporting.
//! - Atomic write (write to tmp, rename) — no corruption on crash.
//! - Unknown JSON keys are ignored.

pub use astra_pipeline::ToolHealthEntry;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ─── Snapshot Format ─────────────────────────────────────────────────────────

/// Complete persisted state for one profile.
///
/// Unknown fields are ignored on read and never written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningSnapshot {
    /// Format version. Bumped when the persistence layout has a breaking change.
    pub version: u32,
    /// Epoch seconds when this snapshot was exported.
    #[serde(default)]
    pub snapshot_epoch: u64,
    /// Persistent tool health data (cross-session error budgets).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_health: Vec<ToolHealthEntry>,
}

/// Local-only sync bookkeeping — "what was last pushed to cloud" — kept out
/// of the main snapshot so it never leaks into cloud storage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningSyncMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synced_tool_health: Vec<ToolHealthEntry>,
}

// ─── File I/O ────────────────────────────────────────────────────────────────

pub fn tool_health_dir() -> PathBuf {
    astra_runtime_env::local_state_root().join("tool-health")
}

pub fn tool_health_path(profile: &str) -> PathBuf {
    tool_health_dir().join(format!("{profile}.json"))
}

pub fn tool_health_sync_metadata_path(profile: &str) -> PathBuf {
    tool_health_dir().join(format!("{profile}.sync.json"))
}

pub fn load_snapshot(profile: &str) -> Option<LearningSnapshot> {
    let path = tool_health_path(profile);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn load_sync_metadata(profile: &str) -> Option<LearningSyncMetadata> {
    let path = tool_health_sync_metadata_path(profile);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_snapshot(profile: &str, snapshot: &LearningSnapshot) -> Result<(), String> {
    save_snapshot_to(&tool_health_path(profile), snapshot)
}

pub fn save_sync_metadata(profile: &str, metadata: &LearningSyncMetadata) -> Result<(), String> {
    save_sync_metadata_to(&tool_health_sync_metadata_path(profile), metadata)
}

pub fn load_snapshot_from(path: &Path) -> Option<LearningSnapshot> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_snapshot_to(path: &Path, snapshot: &LearningSnapshot) -> Result<(), String> {
    atomic_write_json(path, snapshot)
}

pub fn load_sync_metadata_from(path: &Path) -> Option<LearningSyncMetadata> {
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_sync_metadata_to(path: &Path, metadata: &LearningSyncMetadata) -> Result<(), String> {
    atomic_write_json(path, metadata)
}

fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let json = serde_json::to_string_pretty(value).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write: {e}"))?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(())
}

// ─── High-level Operations ───────────────────────────────────────────────────

/// Build a snapshot from current runtime state.
pub fn build_snapshot(tool_health: &[ToolHealthEntry]) -> LearningSnapshot {
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    LearningSnapshot {
        version: 1,
        snapshot_epoch: now_epoch,
        tool_health: tool_health
            .iter()
            .map(super::validated_health_entry)
            .collect(),
    }
}

/// Save a snapshot for a profile. Skips the write if the snapshot is empty.
pub fn save_learning_state(profile: &str, tool_health: &[ToolHealthEntry]) -> Result<(), String> {
    let snapshot = build_snapshot(tool_health);
    if snapshot.tool_health.is_empty() {
        return Ok(());
    }
    save_snapshot(profile, &snapshot)
}

pub fn load_tool_health(profile: &str) -> Vec<ToolHealthEntry> {
    load_snapshot(profile)
        .map(|s| s.tool_health)
        .unwrap_or_default()
}

pub fn load_synced_tool_health(profile: &str) -> Vec<ToolHealthEntry> {
    load_sync_metadata(profile)
        .map(|m| m.synced_tool_health)
        .unwrap_or_default()
}

pub fn save_synced_tool_health(profile: &str, entries: &[ToolHealthEntry]) -> Result<(), String> {
    save_sync_metadata(
        profile,
        &LearningSyncMetadata {
            synced_tool_health: entries.to_vec(),
        },
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_health(name: &str, calls: usize, failures: usize, epoch: u64) -> ToolHealthEntry {
        ToolHealthEntry {
            name: name.into(),
            total_calls: calls,
            total_failures: failures,
            input_validation_failures: 0,
            failure_rate: if calls == 0 {
                0.0
            } else {
                failures as f64 / calls as f64
            },
            last_updated_epoch: epoch,
            recent_outcomes: Vec::new(),
        }
    }

    #[test]
    fn snapshot_roundtrip_empty() {
        let snapshot = LearningSnapshot::default();
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.version, 0);
        assert!(loaded.tool_health.is_empty());
    }

    #[test]
    fn snapshot_with_health_roundtrip() {
        let snapshot = build_snapshot(&[sample_health("bash", 10, 2, 1000)]);
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tool_health.len(), 1);
    }

    #[test]
    fn unknown_fields_are_ignored_on_load() {
        let snapshot = r#"{
            "version": 1,
            "snapshot_epoch": 42,
            "entities": [{"name":"x"}],
            "patterns": [{"signature":"x"}],
            "calibration": {"foo": 1},
            "tool_health": [{
                "name": "bash",
                "total_calls": 3,
                "total_failures": 1,
                "input_validation_failures": 0,
                "failure_rate": 0.33,
                "last_updated_epoch": 99
            }]
        }"#;
        let loaded: LearningSnapshot = serde_json::from_str(snapshot).unwrap();
        assert_eq!(loaded.tool_health.len(), 1);
        assert_eq!(loaded.tool_health[0].name, "bash");
    }

    #[test]
    fn save_and_load_snapshot_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("profile.json");
        let snapshot = build_snapshot(&[sample_health("bash", 1, 0, 42)]);
        save_snapshot_to(&path, &snapshot).unwrap();
        let loaded = load_snapshot_from(&path).unwrap();
        assert_eq!(loaded.tool_health[0].name, "bash");
    }

    #[test]
    fn save_learning_state_skips_empty() {
        let tmp = TempDir::new().unwrap();
        // Using a nonexistent profile under TempDir simulates a fresh install.
        // Calling save with nothing should be a no-op — no file created.
        let original_home = std::env::var_os("HOME");
        // SAFETY: test code, single-threaded env manipulation.
        unsafe { std::env::set_var("HOME", tmp.path()) };
        let res = save_learning_state("profile-empty", &[]);
        if let Some(h) = original_home {
            unsafe { std::env::set_var("HOME", h) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        assert!(res.is_ok());
        assert!(
            !tmp.path()
                .join(".astra/tool-health/profile-empty.json")
                .exists()
        );
    }
}
