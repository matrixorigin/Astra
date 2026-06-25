//! Delegation outcome tracking for coordination pattern selection.
//!
//! This is retained separately from tuning: it records observed delegation
//! outcomes and exposes the historically preferred pattern for a scenario. It
//! does not mutate runtime configuration.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use serde::{Deserialize, Serialize};

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
pub struct DelegationOutcomeTracker {
    data: RwLock<HashMap<String, OutcomeStats>>,
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
            data: RwLock::new(HashMap::new()),
            storage_path: None,
        }
    }

    /// Create a tracker with persistent storage.
    pub fn with_storage(path: PathBuf) -> Self {
        let data = if path.exists() {
            match std::fs::read(&path) {
                Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
                Err(_) => HashMap::new(),
            }
        } else {
            HashMap::new()
        };
        Self {
            data: RwLock::new(data),
            storage_path: Some(path),
        }
    }

    fn key(scenario: &str, pattern: &str) -> String {
        format!("{scenario}:{pattern}")
    }

    /// Record a delegation outcome.
    pub fn record(&self, scenario: &str, pattern: &str, succeeded: bool) {
        let mut map = self.data.write().unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(Self::key(scenario, pattern)).or_default();
        if succeeded {
            entry.successes += 1;
        } else {
            entry.failures += 1;
        }
    }

    /// Get outcome stats for a specific scenario/pattern pair.
    pub fn stats(&self, scenario: &str, pattern: &str) -> Option<OutcomeStats> {
        self.data
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::key(scenario, pattern))
            .cloned()
    }

    /// Return the best historical pattern for a scenario.
    pub fn preferred_pattern(&self, scenario: &str, min_observations: u32) -> Option<String> {
        let map = self.data.read().unwrap_or_else(|e| e.into_inner());
        let prefix = format!("{scenario}:");
        let mut best: Option<(String, f64)> = None;

        for (key, stats) in map.iter() {
            if !key.starts_with(&prefix) || stats.total() < min_observations {
                continue;
            }
            let pattern = &key[prefix.len()..];
            let rate = stats.success_rate();
            if best.as_ref().is_none_or(|(_, best_rate)| rate > *best_rate) {
                best = Some((pattern.to_string(), rate));
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
        tracker.persist();

        let reloaded = DelegationOutcomeTracker::with_storage(path);
        let stats = reloaded.stats("research", "parallel").unwrap();
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 0);
    }
}
