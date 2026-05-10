//! Cross-session persistence for per-tool health and quality.
//!
//! Stored at `~/.astra/learning/<profile>.json`. The file was historically a
//! multi-module "learning snapshot" (entities, patterns, calibration). Those
//! modules have been deleted. Only the tool-health and tool-quality slices
//! remain.
//!
//! Local sync metadata (last-synced baseline for delta push) lives in a
//! separate file `<profile>.sync.json` so the user-facing learning state is
//! not polluted by sync bookkeeping.
//!
//! # Design
//!
//! - One file per profile (user isolation).
//! - Merge-on-load (timestamp wins) — safe for concurrent sessions.
//! - Atomic write (write to tmp, rename) — no corruption on crash.
//! - Forward-compatible: unknown JSON keys are silently ignored, so legacy
//!   snapshots with `entities` / `patterns` / `calibration` fields still load.

pub use crate::tool_registry_report::{ToolQualityEntry, ToolQualityPersistEntry};
pub use astra_pipeline::ToolHealthEntry;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

// ─── Snapshot Format ─────────────────────────────────────────────────────────

/// Complete persisted state for one profile.
///
/// Unknown legacy fields (`entities`, `patterns`, `calibration`) are accepted
/// on read via serde default-on-missing but never written.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningSnapshot {
    /// Format version. Bumped when persistence layout changes incompatibly.
    pub version: u32,
    /// Epoch seconds when this snapshot was exported.
    #[serde(default)]
    pub snapshot_epoch: u64,
    /// Persistent tool health data (cross-session error budgets).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_health: Vec<ToolHealthEntry>,
    /// Per-tool quality / use tracking carried across sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_quality: Vec<ToolQualityPersistEntry>,
}

/// Local-only sync bookkeeping — "what was last pushed to cloud" — kept out
/// of the main snapshot so it never leaks into cloud storage.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LearningSyncMetadata {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub synced_tool_health: Vec<ToolHealthEntry>,
}

// ─── File I/O ────────────────────────────────────────────────────────────────

pub fn learning_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".astra")
        .join("learning")
}

pub fn learning_path(profile: &str) -> PathBuf {
    learning_dir().join(format!("{profile}.json"))
}

pub fn learning_sync_metadata_path(profile: &str) -> PathBuf {
    learning_dir().join(format!("{profile}.sync.json"))
}

pub fn load_snapshot(profile: &str) -> Option<LearningSnapshot> {
    let path = learning_path(profile);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn load_sync_metadata(profile: &str) -> Option<LearningSyncMetadata> {
    let path = learning_sync_metadata_path(profile);
    let data = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_snapshot(profile: &str, snapshot: &LearningSnapshot) -> Result<(), String> {
    save_snapshot_to(&learning_path(profile), snapshot)
}

pub fn save_sync_metadata(profile: &str, metadata: &LearningSyncMetadata) -> Result<(), String> {
    save_sync_metadata_to(&learning_sync_metadata_path(profile), metadata)
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
pub fn build_snapshot(
    tool_health: &[ToolHealthEntry],
    tool_quality: &[ToolQualityPersistEntry],
) -> LearningSnapshot {
    let now_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    LearningSnapshot {
        version: 1,
        snapshot_epoch: now_epoch,
        tool_health: tool_health.to_vec(),
        tool_quality: tool_quality.to_vec(),
    }
}

/// Save a snapshot for a profile. Skips the write if the snapshot is empty.
pub fn save_learning_state(
    profile: &str,
    tool_health: &[ToolHealthEntry],
    tool_quality: &[ToolQualityPersistEntry],
) -> Result<(), String> {
    let snapshot = build_snapshot(tool_health, tool_quality);
    if snapshot.tool_health.is_empty() && snapshot.tool_quality.is_empty() {
        return Ok(());
    }
    save_snapshot(profile, &snapshot)
}

pub fn load_tool_health(profile: &str) -> Vec<ToolHealthEntry> {
    load_snapshot(profile)
        .map(|s| s.tool_health)
        .unwrap_or_default()
}

pub fn load_tool_quality(profile: &str) -> Vec<ToolQualityPersistEntry> {
    load_snapshot(profile)
        .map(|s| s.tool_quality)
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

/// Merge two sets of tool health entries using timestamp-based conflict
/// resolution.
///
/// - For entries present in both local and cloud: most-recently-updated wins
///   (by `last_updated_epoch`). Tie on epoch → higher `total_calls` wins.
///   Full tie → local wins.
/// - Cloud-only entries are always added.
/// - Local-only entries are always kept.
///
/// Per-signature `recent_outcomes` rings are additively merged so no history
/// is lost regardless of which side wins on totals.
///
/// Returns `(merged, cloud_wins, cloud_only_added)`.
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
                let use_cloud = if cloud_entry.last_updated_epoch != local_entry.last_updated_epoch
                {
                    cloud_entry.last_updated_epoch > local_entry.last_updated_epoch
                } else if cloud_entry.total_calls != local_entry.total_calls {
                    cloud_entry.total_calls > local_entry.total_calls
                } else {
                    false
                };
                let mut merged_entry = if use_cloud {
                    cloud_entry.clone()
                } else {
                    local_entry.clone()
                };
                merged_entry.recent_outcomes = merge_recent_outcomes(
                    &local_entry.recent_outcomes,
                    &cloud_entry.recent_outcomes,
                );
                by_name.insert(cloud_entry.name.clone(), merged_entry);
                if use_cloud {
                    cloud_wins += 1;
                }
            }
            None => {
                by_name.insert(cloud_entry.name.clone(), cloud_entry.clone());
                cloud_only += 1;
            }
        }
    }

    let mut merged: Vec<ToolHealthEntry> = by_name.into_values().collect();
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    (merged, cloud_wins, cloud_only)
}

fn merge_recent_outcomes(
    local: &[astra_pipeline::ToolOutcomeCacheEntry],
    cloud: &[astra_pipeline::ToolOutcomeCacheEntry],
) -> Vec<astra_pipeline::ToolOutcomeCacheEntry> {
    use std::collections::HashMap;

    let mut by_signature: HashMap<String, Vec<astra_pipeline::ToolOutcome>> = HashMap::new();
    for source in [local, cloud] {
        for entry in source {
            by_signature
                .entry(entry.signature.clone())
                .or_default()
                .extend(entry.outcomes.iter().cloned());
        }
    }

    let mut merged: Vec<_> = by_signature
        .into_iter()
        .filter_map(|(signature, mut outcomes)| {
            outcomes.sort_by_key(|outcome| {
                (
                    outcome.at_epoch,
                    outcome.result_hash,
                    outcome.latency_ms,
                    outcome.success,
                )
            });
            outcomes.dedup();
            if outcomes.len() > astra_pipeline::TOOL_OUTCOME_RING_CAPACITY {
                let overflow = outcomes.len() - astra_pipeline::TOOL_OUTCOME_RING_CAPACITY;
                outcomes.drain(..overflow);
            }
            (!outcomes.is_empty()).then_some(astra_pipeline::ToolOutcomeCacheEntry {
                signature,
                outcomes,
            })
        })
        .collect();
    merged.sort_by(|left, right| left.signature.cmp(&right.signature));
    merged
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
        let snapshot = build_snapshot(
            &[sample_health("bash", 10, 2, 1000)],
            &[ToolQualityPersistEntry {
                name: "bash".into(),
                entry: ToolQualityEntry {
                    selections: 5,
                    uses: 4,
                    quality_sum: 3.0,
                },
            }],
        );
        let json = serde_json::to_string(&snapshot).unwrap();
        let loaded: LearningSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.tool_health.len(), 1);
        assert_eq!(loaded.tool_quality.len(), 1);
    }

    #[test]
    fn legacy_fields_are_ignored_on_load() {
        // A legacy snapshot may include entities/patterns/calibration. Those
        // fields no longer exist on LearningSnapshot but must not fail parsing.
        let legacy = r#"{
            "version": 1,
            "snapshot_epoch": 42,
            "entities": [{"name":"x"}],
            "patterns": [{"signature":"x"}],
            "calibration": {"foo": 1},
            "tool_health": [{
                "name": "bash",
                "total_calls": 3,
                "total_failures": 1,
                "failure_rate": 0.33,
                "last_updated_epoch": 99
            }]
        }"#;
        let loaded: LearningSnapshot = serde_json::from_str(legacy).unwrap();
        assert_eq!(loaded.tool_health.len(), 1);
        assert_eq!(loaded.tool_health[0].name, "bash");
    }

    #[test]
    fn merge_tool_health_cloud_newer_wins() {
        let local = vec![sample_health("bash", 5, 1, 100)];
        let cloud = vec![sample_health("bash", 10, 3, 200)];
        let (merged, cloud_wins, cloud_only) = merge_tool_health(&local, &cloud);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].total_calls, 10, "cloud (newer epoch) should win");
        assert_eq!(cloud_wins, 1);
        assert_eq!(cloud_only, 0);
    }

    #[test]
    fn merge_tool_health_local_newer_wins() {
        let local = vec![sample_health("bash", 10, 3, 200)];
        let cloud = vec![sample_health("bash", 5, 1, 100)];
        let (merged, cloud_wins, _) = merge_tool_health(&local, &cloud);
        assert_eq!(merged[0].total_calls, 10);
        assert_eq!(cloud_wins, 0);
    }

    #[test]
    fn merge_tool_health_cloud_only_added() {
        let local = vec![sample_health("bash", 5, 1, 100)];
        let cloud = vec![sample_health("read_file", 3, 0, 150)];
        let (merged, _, cloud_only) = merge_tool_health(&local, &cloud);
        assert_eq!(merged.len(), 2);
        assert_eq!(cloud_only, 1);
    }

    #[test]
    fn merge_tool_health_tie_local_wins() {
        let mut local = sample_health("bash", 7, 2, 100);
        local.failure_rate = 0.25;
        let cloud = sample_health("bash", 7, 2, 100);
        let (merged, cloud_wins, _) = merge_tool_health(&[local.clone()], &[cloud]);
        assert_eq!(merged[0].failure_rate, 0.25);
        assert_eq!(cloud_wins, 0);
    }

    #[test]
    fn save_and_load_snapshot_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("profile.json");
        let snapshot = build_snapshot(&[sample_health("bash", 1, 0, 42)], &[]);
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
        let res = save_learning_state("profile-empty", &[], &[]);
        if let Some(h) = original_home {
            unsafe { std::env::set_var("HOME", h) };
        } else {
            unsafe { std::env::remove_var("HOME") };
        }
        assert!(res.is_ok());
        assert!(
            !tmp.path()
                .join(".astra/learning/profile-empty.json")
                .exists()
        );
    }
}
