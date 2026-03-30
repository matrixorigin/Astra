//! Delta Log — Local change tracking for edge-cloud session state synchronization.
//!
//! Provides `ChangeAccumulator` which records create/update/delete operations
//! with monotonic versioning instead of overwriting full state. This enables
//! efficient incremental sync and reduces memory overhead.

#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Operation type for state mutations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeOp {
    /// Create a new key-value pair.
    Create,
    /// Update an existing key-value pair.
    Update,
    /// Delete a key.
    Delete,
}

/// A single delta entry representing a state mutation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaEntry {
    /// Monotonic version number (increments globally).
    pub version: u64,
    /// Timestamp in milliseconds since epoch.
    pub timestamp_ms: u64,
    /// The key being mutated.
    pub key: String,
    /// The operation type.
    pub op: ChangeOp,
    /// The value (None for Delete operations).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Previous version this entry is based on (for conflict detection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub based_on: Option<u64>,
}

/// Accumulates state mutations in a delta log format.
///
/// Instead of overwriting full state, records incremental changes
/// which can be replayed or synced to the cloud.
pub struct ChangeAccumulator {
    /// Monotonic version counter (shared across all accumulators if cloned).
    version_counter: Arc<AtomicU64>,
    /// The delta log entries (append-only).
    entries: Vec<DeltaEntry>,
    /// Current state snapshot (for efficient reads).
    state: HashMap<String, serde_json::Value>,
    /// Index of last version per key for fast conflict detection.
    key_versions: HashMap<String, u64>,
    /// Maximum size limit for the delta log (in bytes, approximate).
    max_size_bytes: usize,
}

impl Default for ChangeAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeAccumulator {
    /// Create a new ChangeAccumulator with default settings.
    pub fn new() -> Self {
        Self::with_config(10 * 1024 * 1024) // 10MB default
    }

    /// Create a ChangeAccumulator with a specific size limit.
    pub fn with_config(max_size_bytes: usize) -> Self {
        Self {
            version_counter: Arc::new(AtomicU64::new(1)),
            entries: Vec::new(),
            state: HashMap::new(),
            key_versions: HashMap::new(),
            max_size_bytes,
        }
    }

    /// Create a ChangeAccumulator from an existing state and version counter.
    /// Used when cloning or restoring from snapshot.
    fn with_state(
        version_counter: Arc<AtomicU64>,
        initial_version: u64,
        state: HashMap<String, serde_json::Value>,
    ) -> Self {
        let key_versions: HashMap<_, _> =
            state.keys().map(|k| (k.clone(), initial_version)).collect();
        Self {
            version_counter,
            entries: Vec::new(),
            state,
            key_versions,
            max_size_bytes: 10 * 1024 * 1024,
        }
    }

    /// Get the next monotonic version number.
    fn next_version(&self) -> u64 {
        self.version_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// Record a create operation.
    ///
    /// Returns the version number if successful, or an error if the key already exists.
    pub fn create(
        &mut self,
        key: impl Into<String>,
        value: impl Serialize,
    ) -> Result<u64, DeltaError> {
        let key = key.into();

        if self.state.contains_key(&key) {
            return Err(DeltaError::KeyAlreadyExists(key));
        }

        let version = self.next_version();
        let timestamp_ms = current_timestamp_ms();
        let json_value = serde_json::to_value(value)
            .map_err(|e| DeltaError::SerializationError(e.to_string()))?;

        // Update state
        self.state.insert(key.clone(), json_value.clone());
        self.key_versions.insert(key.clone(), version);

        // Append to delta log
        self.entries.push(DeltaEntry {
            version,
            timestamp_ms,
            key,
            op: ChangeOp::Create,
            value: Some(json_value),
            based_on: None,
        });

        self.maybe_compact();
        Ok(version)
    }

    /// Record an update operation.
    ///
    /// Returns the version number if successful, or an error if the key doesn't exist.
    pub fn update(
        &mut self,
        key: impl Into<String>,
        value: impl Serialize,
    ) -> Result<u64, DeltaError> {
        let key = key.into();

        let based_on = self
            .key_versions
            .get(&key)
            .copied()
            .ok_or_else(|| DeltaError::KeyNotFound(key.clone()))?;

        let version = self.next_version();
        let timestamp_ms = current_timestamp_ms();
        let json_value = serde_json::to_value(value)
            .map_err(|e| DeltaError::SerializationError(e.to_string()))?;

        // Update state
        self.state.insert(key.clone(), json_value.clone());
        self.key_versions.insert(key.clone(), version);

        // Append to delta log
        self.entries.push(DeltaEntry {
            version,
            timestamp_ms,
            key,
            op: ChangeOp::Update,
            value: Some(json_value),
            based_on: Some(based_on),
        });

        self.maybe_compact();
        Ok(version)
    }

    /// Record a delete operation.
    ///
    /// Returns the version number if successful, or an error if the key doesn't exist.
    pub fn delete(&mut self, key: impl Into<String>) -> Result<u64, DeltaError> {
        let key = key.into();

        let based_on = self
            .key_versions
            .get(&key)
            .copied()
            .ok_or_else(|| DeltaError::KeyNotFound(key.clone()))?;

        let version = self.next_version();
        let timestamp_ms = current_timestamp_ms();

        // Update state
        self.state.remove(&key);
        self.key_versions.remove(&key);

        // Append to delta log
        self.entries.push(DeltaEntry {
            version,
            timestamp_ms,
            key,
            op: ChangeOp::Delete,
            value: None,
            based_on: Some(based_on),
        });

        self.maybe_compact();
        Ok(version)
    }

    /// Get the current value for a key.
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.state.get(key)
    }

    /// Check if a key exists.
    pub fn contains_key(&self, key: &str) -> bool {
        self.state.contains_key(key)
    }

    /// Get all delta entries since a specific version (exclusive).
    pub fn deltas_since(&self, since_version: u64) -> Vec<&DeltaEntry> {
        self.entries
            .iter()
            .filter(|e| e.version > since_version)
            .collect()
    }

    /// Get all delta entries.
    pub fn all_deltas(&self) -> &[DeltaEntry] {
        &self.entries
    }

    /// Get the latest version number.
    pub fn latest_version(&self) -> u64 {
        self.version_counter.load(Ordering::SeqCst) - 1
    }

    /// Get the version of a specific key.
    pub fn key_version(&self, key: &str) -> Option<u64> {
        self.key_versions.get(key).copied()
    }

    /// Get the number of entries in the delta log.
    pub fn delta_count(&self) -> usize {
        self.entries.len()
    }

    /// Get the approximate memory overhead of the delta log.
    /// Returns (delta_log_bytes, state_bytes, total_bytes).
    pub fn memory_usage(&self) -> (usize, usize, usize) {
        let state_bytes = self
            .state
            .iter()
            .map(|(k, v)| k.len() + serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
            .sum::<usize>();

        let delta_bytes = self
            .entries
            .iter()
            .map(|e| {
                let base = e.key.len() + std::mem::size_of::<DeltaEntry>();
                let value_bytes = e
                    .value
                    .as_ref()
                    .map(|v| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0))
                    .unwrap_or(0);
                base + value_bytes
            })
            .sum::<usize>();

        let overhead = std::mem::size_of::<Self>()
            + self.key_versions.capacity()
                * (std::mem::size_of::<String>() + std::mem::size_of::<u64>());

        (
            delta_bytes,
            state_bytes,
            delta_bytes + state_bytes + overhead,
        )
    }

    /// Calculate memory overhead percentage compared to full state size.
    /// Returns percentage (0.0-100.0) representing delta overhead.
    pub fn overhead_percentage(&self) -> f64 {
        let (delta_bytes, state_bytes, _) = self.memory_usage();
        if state_bytes == 0 {
            return 0.0;
        }
        (delta_bytes as f64 / state_bytes as f64) * 100.0
    }

    /// Create a full snapshot entry that captures current state.
    /// This can be used for compaction or initial sync.
    pub fn create_snapshot_entry(&self) -> Result<DeltaEntry, DeltaError> {
        let version = self.next_version();
        let timestamp_ms = current_timestamp_ms();

        let snapshot_value = serde_json::to_value(&self.state)
            .map_err(|e| DeltaError::SerializationError(e.to_string()))?;

        Ok(DeltaEntry {
            version,
            timestamp_ms,
            key: "__snapshot__".to_string(),
            op: ChangeOp::Create,
            value: Some(snapshot_value),
            based_on: None,
        })
    }

    /// Clear the delta log after creating a snapshot.
    /// Call this after syncing to cloud to free memory.
    pub fn clear_deltas(&mut self) {
        self.entries.clear();
        self.entries.shrink_to_fit();
    }

    /// Compact the delta log by removing redundant entries.
    /// Keeps only the latest operation per key.
    pub fn compact(&mut self) {
        let mut latest_per_key: HashMap<String, DeltaEntry> = HashMap::new();

        for entry in &self.entries {
            // Skip deletes that we can coalesce
            if entry.op == ChangeOp::Delete {
                latest_per_key.remove(&entry.key);
            } else {
                latest_per_key.insert(entry.key.clone(), entry.clone());
            }
        }

        // Rebuild entries sorted by version
        let mut new_entries: Vec<DeltaEntry> = latest_per_key.into_values().collect();
        new_entries.sort_by_key(|e| e.version);

        self.entries = new_entries;
    }

    /// Check if compaction is needed and perform it.
    fn maybe_compact(&mut self) {
        let (delta_bytes, _, _) = self.memory_usage();
        if delta_bytes > self.max_size_bytes {
            self.compact();
        }
    }

    /// Create a fork of this accumulator with shared version counter.
    /// Useful for creating isolated sessions that share versioning.
    pub fn fork(&self) -> Self {
        Self::with_state(
            Arc::clone(&self.version_counter),
            self.latest_version(),
            self.state.clone(),
        )
    }

    /// Apply a batch of external deltas to this accumulator.
    /// Used for merging changes from the cloud.
    pub fn apply_deltas(&mut self, deltas: &[DeltaEntry]) -> Result<Vec<u64>, DeltaError> {
        let mut applied_versions = Vec::new();

        for entry in deltas {
            // Skip if we already have this version
            if entry.version <= self.latest_version() {
                continue;
            }

            // Update version counter if needed
            let current_max = self.latest_version();
            if entry.version > current_max {
                let diff = entry.version - current_max;
                for _ in 0..diff {
                    self.next_version();
                }
            }

            // Apply the operation
            match entry.op {
                ChangeOp::Create | ChangeOp::Update => {
                    if let Some(ref value) = entry.value {
                        self.state.insert(entry.key.clone(), value.clone());
                        self.key_versions.insert(entry.key.clone(), entry.version);
                    }
                }
                ChangeOp::Delete => {
                    self.state.remove(&entry.key);
                    self.key_versions.remove(&entry.key);
                }
            }

            applied_versions.push(entry.version);
        }

        Ok(applied_versions)
    }
}

/// Errors that can occur during delta operations.
#[derive(Debug, Clone, PartialEq)]
pub enum DeltaError {
    KeyAlreadyExists(String),
    KeyNotFound(String),
    SerializationError(String),
}

impl std::fmt::Display for DeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeltaError::KeyAlreadyExists(key) => write!(f, "Key already exists: {}", key),
            DeltaError::KeyNotFound(key) => write!(f, "Key not found: {}", key),
            DeltaError::SerializationError(e) => write!(f, "Serialization error: {}", e),
        }
    }
}

impl std::error::Error for DeltaError {}

/// Get current timestamp in milliseconds.
fn current_timestamp_ms() -> u64 {
    use std::time::SystemTime;
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ═══════════════════════════════════════════════════════════ Tests ═════
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_create_operation() {
        let mut acc = ChangeAccumulator::new();
        let version = acc.create("key1", "value1").unwrap();

        assert_eq!(version, 1);
        assert_eq!(
            acc.get("key1"),
            Some(&serde_json::Value::String("value1".to_string()))
        );
        assert_eq!(acc.delta_count(), 1);
        assert_eq!(acc.key_version("key1"), Some(1));
    }

    #[test]
    fn test_create_duplicate_key_fails() {
        let mut acc = ChangeAccumulator::new();
        acc.create("key1", "value1").unwrap();

        let result = acc.create("key1", "value2");
        assert!(matches!(result, Err(DeltaError::KeyAlreadyExists(_))));
    }

    #[test]
    fn test_update_operation() {
        let mut acc = ChangeAccumulator::new();
        acc.create("key1", "value1").unwrap();
        let version = acc.update("key1", "value2").unwrap();

        assert_eq!(version, 2);
        assert_eq!(
            acc.get("key1"),
            Some(&serde_json::Value::String("value2".to_string()))
        );
        assert_eq!(acc.delta_count(), 2);
        assert_eq!(acc.key_version("key1"), Some(2));
    }

    #[test]
    fn test_update_nonexistent_key_fails() {
        let mut acc = ChangeAccumulator::new();
        let result = acc.update("key1", "value1");
        assert!(matches!(result, Err(DeltaError::KeyNotFound(_))));
    }

    #[test]
    fn test_delete_operation() {
        let mut acc = ChangeAccumulator::new();
        acc.create("key1", "value1").unwrap();
        let version = acc.delete("key1").unwrap();

        assert_eq!(version, 2);
        assert_eq!(acc.get("key1"), None);
        assert!(!acc.contains_key("key1"));
        assert_eq!(acc.delta_count(), 2);
    }

    #[test]
    fn test_delete_nonexistent_key_fails() {
        let mut acc = ChangeAccumulator::new();
        let result = acc.delete("key1");
        assert!(matches!(result, Err(DeltaError::KeyNotFound(_))));
    }

    #[test]
    fn test_deltas_since() {
        let mut acc = ChangeAccumulator::new();
        acc.create("key1", "value1").unwrap(); // v1
        acc.update("key1", "value2").unwrap(); // v2
        acc.create("key2", "valueA").unwrap(); // v3

        let deltas = acc.deltas_since(1);
        assert_eq!(deltas.len(), 2);
        assert_eq!(deltas[0].version, 2);
        assert_eq!(deltas[1].version, 3);
    }

    #[test]
    fn test_monotonic_versioning() {
        let mut acc = ChangeAccumulator::new();
        let v1 = acc.create("a", 1).unwrap();
        let v2 = acc.create("b", 2).unwrap();
        let v3 = acc.update("a", 3).unwrap();

        assert!(v1 < v2);
        assert!(v2 < v3);
        assert_eq!(acc.latest_version(), 3);
    }

    #[test]
    fn test_shared_version_counter_on_fork() {
        let mut acc1 = ChangeAccumulator::new();
        acc1.create("key1", "value1").unwrap(); // v1

        let mut acc2 = acc1.fork();
        let v2 = acc2.create("key2", "value2").unwrap(); // v2 from shared counter
        let v3 = acc1.create("key3", "value3").unwrap(); // v3 from shared counter

        assert_eq!(v2, 2);
        assert_eq!(v3, 3);
    }

    #[test]
    fn test_memory_overhead_with_updates() {
        let mut acc = ChangeAccumulator::new();

        // Create many keys with large values
        for i in 0..100 {
            let value = "x".repeat(100);
            acc.create(format!("key_{}", i), value).unwrap();
        }

        // Update some keys multiple times to create deltas
        for _ in 0..5 {
            for i in 0..50 {
                let value = "y".repeat(100);
                acc.update(format!("key_{}", i), value).unwrap();
            }
        }

        let overhead = acc.overhead_percentage();
        println!("Memory overhead: {:.2}%", overhead);

        // After many updates, overhead will be high until compaction
        // This test verifies the measurement works, not a specific threshold
        assert!(overhead > 0.0, "Should have some overhead after updates");

        // After compaction, overhead should be reduced
        acc.compact();
        let after_compact = acc.overhead_percentage();
        println!("After compaction: {:.2}%", after_compact);
        assert!(
            after_compact < overhead,
            "Compaction should reduce overhead"
        );
    }

    #[test]
    fn test_compaction_reduces_size() {
        let mut acc = ChangeAccumulator::new();

        // Create and update many times
        acc.create("key1", "value1").unwrap();
        for i in 0..10 {
            acc.update("key1", format!("value{}", i)).unwrap();
        }

        let before_count = acc.delta_count();
        acc.compact();
        let after_count = acc.delta_count();

        assert!(
            after_count < before_count,
            "Compaction should reduce delta count"
        );
        assert_eq!(after_count, 1); // Only latest update should remain
    }

    #[test]
    fn test_compaction_removes_redundant_entries() {
        let mut acc = ChangeAccumulator::new();

        acc.create("a", 1).unwrap();
        acc.update("a", 2).unwrap();
        acc.update("a", 3).unwrap();
        acc.delete("a").unwrap();

        acc.compact();

        // After compaction and deletion, key 'a' should have no entries
        assert_eq!(acc.delta_count(), 0);
        assert_eq!(acc.get("a"), None);
    }

    #[test]
    fn test_apply_external_deltas() {
        let mut acc = ChangeAccumulator::new();
        acc.create("local_key", "local_value").unwrap();

        let external_deltas = vec![
            DeltaEntry {
                version: 10,
                timestamp_ms: 1000,
                key: "remote_key".to_string(),
                op: ChangeOp::Create,
                value: Some(serde_json::json!("remote_value")),
                based_on: None,
            },
            DeltaEntry {
                version: 11,
                timestamp_ms: 1001,
                key: "local_key".to_string(),
                op: ChangeOp::Update,
                value: Some(serde_json::json!("updated_value")),
                based_on: Some(1),
            },
        ];

        let applied = acc.apply_deltas(&external_deltas).unwrap();
        assert_eq!(applied.len(), 2);
        assert_eq!(
            acc.get("remote_key"),
            Some(&serde_json::json!("remote_value"))
        );
        assert_eq!(
            acc.get("local_key"),
            Some(&serde_json::json!("updated_value"))
        );
    }

    #[test]
    fn test_serialization_roundtrip() {
        let mut acc = ChangeAccumulator::new();
        acc.create("key1", "value1").unwrap();
        acc.update("key1", "value2").unwrap();

        let deltas = acc.all_deltas();
        let json = serde_json::to_string(deltas).unwrap();
        let restored: Vec<DeltaEntry> = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.len(), 2);
        assert_eq!(restored[0].key, "key1");
        assert_eq!(restored[0].op, ChangeOp::Create);
        assert_eq!(restored[1].op, ChangeOp::Update);
    }

    #[test]
    fn test_json_values() {
        let mut acc = ChangeAccumulator::new();
        let complex_value = serde_json::json!({
            "name": "test",
            "count": 42,
            "items": ["a", "b", "c"]
        });

        acc.create("config", complex_value.clone()).unwrap();
        assert_eq!(acc.get("config"), Some(&complex_value));
    }

    #[test]
    fn test_delta_entry_serialization_omits_none_fields() {
        let entry = DeltaEntry {
            version: 1,
            timestamp_ms: 1000,
            key: "test".to_string(),
            op: ChangeOp::Delete,
            value: None,
            based_on: None,
        };

        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("value"));
        assert!(!json.contains("based_on"));
        assert!(json.contains("delete"));
    }

    #[test]
    fn test_clear_deltas_frees_memory() {
        let mut acc = ChangeAccumulator::new();

        for i in 0..100 {
            acc.create(format!("key{}", i), format!("value{}", i))
                .unwrap();
        }

        assert_eq!(acc.delta_count(), 100);

        acc.clear_deltas();
        assert_eq!(acc.delta_count(), 0);

        // State should still be accessible
        assert_eq!(acc.get("key0"), Some(&serde_json::json!("value0")));
    }

    #[test]
    fn test_latest_version_after_operations() {
        let mut acc = ChangeAccumulator::new();
        assert_eq!(acc.latest_version(), 0);

        acc.create("a", 1).unwrap();
        assert_eq!(acc.latest_version(), 1);

        acc.update("a", 2).unwrap();
        assert_eq!(acc.latest_version(), 2);

        acc.delete("a").unwrap();
        assert_eq!(acc.latest_version(), 3);
    }

    #[test]
    fn test_delta_entry_has_timestamp() {
        let mut acc = ChangeAccumulator::new();
        let before = current_timestamp_ms();
        acc.create("key", "value").unwrap();
        let after = current_timestamp_ms();

        let entry = &acc.all_deltas()[0];
        assert!(entry.timestamp_ms >= before);
        assert!(entry.timestamp_ms <= after);
    }

    #[test]
    fn test_memory_usage_calculation() {
        let mut acc = ChangeAccumulator::new();
        acc.create("key1", "value1").unwrap();

        let (delta, state, total) = acc.memory_usage();
        assert!(delta > 0);
        assert!(state > 0);
        assert!(total >= delta + state);
    }

    #[test]
    fn test_empty_accumulator_overhead_is_zero() {
        let acc = ChangeAccumulator::new();
        assert_eq!(acc.overhead_percentage(), 0.0);
    }

    #[test]
    fn test_snapshot_creation() {
        let mut acc = ChangeAccumulator::new();
        acc.create("a", 1).unwrap();
        acc.create("b", 2).unwrap();

        let snapshot = acc.create_snapshot_entry().unwrap();
        assert_eq!(snapshot.key, "__snapshot__");
        assert_eq!(snapshot.op, ChangeOp::Create);
        assert!(snapshot.value.is_some());
    }
}
