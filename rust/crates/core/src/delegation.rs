//! Delegation outcome tracking for coordination pattern selection.
//!
//! This is retained separately from tuning: it records observed delegation
//! outcomes and exposes the historically preferred pattern for a scenario. It
//! does not mutate runtime configuration.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

const DEFAULT_MAX_ENTRIES: usize = 1000;

/// Per-(scenario, pattern) outcome statistics for coordination auto-select.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OutcomeStats {
    pub successes: u32,
    pub failures: u32,
}

impl OutcomeStats {
    /// Success rate as a fraction in [0, 1]. Returns 0.5 when no data.
    pub fn success_rate(&self) -> f64 {
        let total = self.successes + self.failures;
        if total == 0 {
            0.5
        } else {
            self.successes as f64 / total as f64
        }
    }

    /// Total observations.
    pub fn total(&self) -> u32 {
        self.successes + self.failures
    }
}

/// Tracks delegation outcomes per (scenario, pattern) pair.
///
/// Uses a nested `BTreeMap<scenario, BTreeMap<pattern, stats>>` to eliminate
/// delimiter-collision bugs that arise from concatenating two arbitrary strings
/// into a single key. Bounding is enforced on total (scenario, pattern) pairs.
pub struct DelegationOutcomeTracker {
    data: RwLock<BTreeMap<String, BTreeMap<String, OutcomeStats>>>,
    max_entries: usize,
    storage_path: Option<PathBuf>,
}

impl Default for DelegationOutcomeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DelegationOutcomeTracker {
    /// Create an in-memory tracker.
    pub fn new() -> Self {
        Self {
            data: RwLock::new(BTreeMap::new()),
            max_entries: DEFAULT_MAX_ENTRIES,
            storage_path: None,
        }
    }

    /// Create a tracker with persistent storage.
    pub fn with_storage(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
                Err(_) => BTreeMap::new(),
            }
        } else {
            BTreeMap::new()
        };
        Self {
            data: RwLock::new(data),
            max_entries: DEFAULT_MAX_ENTRIES,
            storage_path: Some(path),
        }
    }

    /// Count total (scenario, pattern) entries across all scenarios.
    fn total_entries(data: &BTreeMap<String, BTreeMap<String, OutcomeStats>>) -> usize {
        data.values().map(|inner| inner.len()).sum()
    }

    /// Evict the entry with the fewest total observations when over capacity.
    fn evict_least_observed(data: &mut BTreeMap<String, BTreeMap<String, OutcomeStats>>) {
        let mut worst: Option<(String, String, u32)> = None;
        for (scenario, inner) in data.iter() {
            for (pattern, stats) in inner.iter() {
                let total = stats.total();
                if worst.as_ref().is_none_or(|(_, _, t)| total < *t) {
                    worst = Some((scenario.clone(), pattern.clone(), total));
                }
            }
        }
        if let Some((scenario, pattern, _)) = worst
            && let Some(inner) = data.get_mut(&scenario) {
                inner.remove(&pattern);
                if inner.is_empty() {
                    data.remove(&scenario);
                }
            }
    }

    /// Record a delegation outcome. Auto-persists and enforces capacity bounds.
    pub fn record(&self, scenario: &str, pattern: &str, succeeded: bool) {
        {
            let mut map = self.data.write().unwrap_or_else(|e| e.into_inner());
            let entry = map
                .entry(scenario.to_string())
                .or_default()
                .entry(pattern.to_string())
                .or_default();
            if succeeded {
                entry.successes += 1;
            } else {
                entry.failures += 1;
            }
            // Enforce capacity bound.
            while Self::total_entries(&map) > self.max_entries {
                Self::evict_least_observed(&mut map);
            }
        }
        self.persist();
    }

    /// Get outcome stats for a specific scenario/pattern pair.
    pub fn stats(&self, scenario: &str, pattern: &str) -> Option<OutcomeStats> {
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(scenario)
            .and_then(|inner| inner.get(pattern))
            .cloned()
    }

    /// Return the best historical pattern for a scenario.
    pub fn preferred_pattern(&self, scenario: &str, min_observations: u32) -> Option<String> {
        let map = self.data.read().unwrap_or_else(|e| e.into_inner());
        let inner = map.get(scenario)?;
        let mut best: Option<(String, f64)> = None;

        for (pattern, stats) in inner.iter() {
            if stats.total() < min_observations {
                continue;
            }
            let rate = stats.success_rate();
            if best.as_ref().is_none_or(|(_, best_rate)| rate > *best_rate) {
                best = Some((pattern.clone(), rate));
            }
        }
        best.map(|(pattern, _)| pattern)
    }

    /// Persist data to storage using an atomic rename.
    pub fn persist(&self) {
        let Some(path) = &self.storage_path else {
            return;
        };
        if let Some(parent) = path.parent()
            && let Err(err) = std::fs::create_dir_all(parent)
        {
            eprintln!("[delegation-outcomes] failed to create storage directory: {err}");
            return;
        }
        let data = {
            let map = self.data.read().unwrap_or_else(|e| e.into_inner());
            let Ok(data) = serde_json::to_vec_pretty(&*map) else {
                return;
            };
            data
        };
        let tmp = path.with_extension("tmp");
        if let Err(err) = std::fs::write(&tmp, data) {
            eprintln!("[delegation-outcomes] failed to write temp file: {err}");
            return;
        }
        if let Err(err) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("[delegation-outcomes] failed to rename temp file: {err}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_pattern_requires_min_observations() {
        let tracker = DelegationOutcomeTracker::new();
        tracker.record("coding", "solo", true);
        tracker.record("coding", "team", false);
        tracker.record("coding", "team", true);
        tracker.record("coding", "team", true);

        assert_eq!(
            tracker.preferred_pattern("coding", 3).as_deref(),
            Some("team")
        );
        assert_eq!(tracker.preferred_pattern("coding", 4), None);
    }

    #[test]
    fn persists_and_reloads_outcomes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("delegation.json");

        let tracker = DelegationOutcomeTracker::with_storage(path.clone());
        tracker.record("research", "parallel", true);

        let reloaded = DelegationOutcomeTracker::with_storage(path);
        let stats = reloaded.stats("research", "parallel").unwrap();
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 0);
    }

    #[test]
    fn scenario_with_colons_does_not_collide() {
        let tracker = DelegationOutcomeTracker::new();
        tracker.record("a:b", "c", true);
        tracker.record("a", "b:c", false);

        assert_eq!(tracker.stats("a:b", "c").unwrap().successes, 1);
        assert_eq!(tracker.stats("a", "b:c").unwrap().failures, 1);
        assert!(tracker.stats("a", "b").is_none());
    }

    #[test]
    fn bounded_eviction_removes_least_observed() {
        let tracker = DelegationOutcomeTracker {
            max_entries: 2,
            ..DelegationOutcomeTracker::new()
        };
        tracker.record("s1", "p1", true);
        tracker.record("s1", "p1", true);
        tracker.record("s2", "p2", true);
        // Now at capacity (2 entries). Adding a third should evict s2:p2 (1 obs).
        tracker.record("s3", "p3", true);

        assert!(tracker.stats("s1", "p1").is_some());
        assert!(tracker.stats("s3", "p3").is_some());
        assert!(tracker.stats("s2", "p2").is_none());
    }
}
