//! Incremental Sync Protocol — Delta encoding, versioning, and API contracts.
//!
//! This module defines the core types and operations for the incremental sync protocol
//! between edge (CLI) and cloud (MatrixOne). Full snapshots are ~40KB; typical deltas
//! are 2-5KB (85-90% reduction).
//!
//! # Architecture
//!
//! ```text
//! Edge (CLI)                          Cloud (MatrixOne)
//! ─────────                          ──────────────────
//! DeltaBatch ─────────POST────────▶  Delta log table
//!          ▲                         (compacted periodically)
//!          └─────────GET─────────────┘
//! ```
//!
//! # Protocol Flow
//!
//! 1. Client calls `get_changes(since_version)` to fetch pending deltas
//! 2. Client applies deltas locally (or falls back to full-sync if delta too large)
//! 3. Client creates `DeltaBatch` with local changes
//! 4. Client calls `apply_delta(batch)` with optimistic locking
//! 5. On conflict, client re-pulls and retries with merged changes

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

// ─────────────────────────────────────────────────────────────────────────────
// Delta Encoding Format
// ─────────────────────────────────────────────────────────────────────────────

/// Delta operation types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaOpType {
    /// Add new element at path; error if exists.
    Add,
    /// Replace element at path; error if missing.
    Replace,
    /// Remove element at path; noop if already removed.
    Remove,
    /// Deep merge object at path; creates if missing.
    Merge,
}

/// A single delta operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaOp {
    /// Operation type.
    pub op: DeltaOpType,
    /// JSON Pointer path (RFC 6901).
    pub path: String,
    /// Value for add/replace/merge operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<serde_json::Value>,
    /// Expected old value for conflict detection (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_value: Option<serde_json::Value>,
}

impl DeltaOp {
    /// Create an add operation.
    pub fn add(path: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        Self {
            op: DeltaOpType::Add,
            path: path.into(),
            value: Some(value.into()),
            old_value: None,
        }
    }

    /// Create a replace operation.
    pub fn replace(path: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        Self {
            op: DeltaOpType::Replace,
            path: path.into(),
            value: Some(value.into()),
            old_value: None,
        }
    }

    /// Create a replace operation with conflict detection.
    pub fn replace_with_old(
        path: impl Into<String>,
        value: impl Into<serde_json::Value>,
        old_value: impl Into<serde_json::Value>,
    ) -> Self {
        Self {
            op: DeltaOpType::Replace,
            path: path.into(),
            value: Some(value.into()),
            old_value: Some(old_value.into()),
        }
    }

    /// Create a remove operation.
    pub fn remove(path: impl Into<String>) -> Self {
        Self {
            op: DeltaOpType::Remove,
            path: path.into(),
            value: None,
            old_value: None,
        }
    }

    /// Create a merge operation.
    pub fn merge(path: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        Self {
            op: DeltaOpType::Merge,
            path: path.into(),
            value: Some(value.into()),
            old_value: None,
        }
    }
}

/// Tombstone entry for deleted items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tombstone {
    /// Entity/pattern identifier.
    pub key: String,
    /// Deletion timestamp (ISO 8601).
    pub deleted_at: String,
    /// Version at time of deletion.
    pub version: i64,
}

/// A batch of delta operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaBatch {
    /// Client's base version.
    pub source_version: i64,
    /// Expected version after apply.
    pub target_version: i64,
    /// Reference checkpoint for compaction.
    pub checkpoint_id: String,
    /// Delta operations.
    pub operations: Vec<DeltaOp>,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// Number of entity changes.
    pub entity_count: u32,
    /// Number of pattern changes.
    pub pattern_count: u32,
    /// Tombstones for deleted items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tombstones: Option<Vec<Tombstone>>,
}

impl DeltaBatch {
    /// Create a new empty delta batch.
    pub fn new(source_version: i64, checkpoint_id: impl Into<String>) -> Self {
        Self {
            source_version,
            target_version: source_version + 1,
            checkpoint_id: checkpoint_id.into(),
            operations: Vec::new(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            entity_count: 0,
            pattern_count: 0,
            tombstones: None,
        }
    }

    /// Add an operation to the batch.
    pub fn push(&mut self, op: DeltaOp) {
        self.operations.push(op);
        self.target_version = self.source_version + self.operations.len() as i64;
    }

    /// Check if batch has any operations.
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Count of operations.
    pub fn len(&self) -> usize {
        self.operations.len()
    }

    /// Approximate size in bytes.
    pub fn approx_size(&self) -> usize {
        serde_json::to_string(self).map(|s| s.len()).unwrap_or(0)
    }

    /// Validate the delta batch.
    pub fn validate(&self) -> Result<(), DeltaError> {
        if self.source_version < 0 {
            return Err(DeltaError::InvalidVersion(
                "source_version must be non-negative".into(),
            ));
        }
        if self.target_version <= self.source_version {
            return Err(DeltaError::InvalidVersion(
                "target_version must be greater than source_version".into(),
            ));
        }
        if self.operations.len() as i64 != self.target_version - self.source_version {
            // This is a warning condition, not necessarily an error
        }
        if self.checkpoint_id.is_empty() {
            return Err(DeltaError::InvalidCheckpoint(
                "checkpoint_id cannot be empty".into(),
            ));
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Versioning Strategy
// ─────────────────────────────────────────────────────────────────────────────

/// Version vector for session state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionVector {
    /// Monotonic counter incremented on each mutation.
    pub version: i64,
    /// Session that owns this version.
    pub session_id: String,
    /// Timestamp of last update.
    pub updated_at: String,
    /// Hash of state for integrity verification.
    pub state_hash: String,
}

impl VersionVector {
    /// Create a new version vector.
    pub fn new(session_id: impl Into<String>, version: i64, state_hash: impl Into<String>) -> Self {
        Self {
            version,
            session_id: session_id.into(),
            updated_at: chrono::Utc::now().to_rfc3339(),
            state_hash: state_hash.into(),
        }
    }

    /// Increment version with new state hash.
    pub fn increment(&mut self, new_hash: impl Into<String>) {
        self.version += 1;
        self.state_hash = new_hash.into();
        self.updated_at = chrono::Utc::now().to_rfc3339();
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Checkpoint Mechanism
// ─────────────────────────────────────────────────────────────────────────────

/// State reference type for checkpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateRefType {
    /// Inline state stored directly.
    Inline,
    /// State stored in S3/object storage.
    S3,
    /// State stored in database.
    Db,
}

/// State reference within a checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRef {
    /// Type of storage.
    #[serde(rename = "type")]
    pub ref_type: StateRefType,
    /// Location identifier (URL, key, etc.).
    pub location: String,
    /// SHA-256 checksum of state.
    pub checksum: String,
}

/// Delta range covered by checkpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaRange {
    /// Start version (inclusive).
    pub from_version: i64,
    /// End version (inclusive).
    pub to_version: i64,
}

/// Checkpoint for state recovery and delta compaction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    /// Unique checkpoint identifier.
    pub id: String,
    /// Version at checkpoint.
    pub version: i64,
    /// ISO 8601 timestamp.
    pub timestamp: String,
    /// State snapshot reference.
    pub state_ref: StateRef,
    /// Delta log range.
    pub delta_range: DeltaRange,
    /// Expiry timestamp for garbage collection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
}

/// Checkpoint creation options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointOptions {
    /// Custom checkpoint name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Time-to-live in days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttl_days: Option<u32>,
}

impl Default for CheckpointOptions {
    fn default() -> Self {
        Self {
            name: None,
            ttl_days: Some(30),
        }
    }
}

/// Checkpoint trigger configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointTriggers {
    /// Create checkpoint after N deltas.
    pub max_deltas: u32,
    /// Create checkpoint if version lag > N.
    pub max_version_gap: u32,
    /// Create checkpoint every N hours.
    pub time_interval_hours: u32,
}

impl Default for CheckpointTriggers {
    fn default() -> Self {
        Self {
            max_deltas: 100,
            max_version_gap: 50,
            time_interval_hours: 24,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// API Request/Response Types
// ─────────────────────────────────────────────────────────────────────────────

/// Request to get changes since a version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetChangesRequest {
    /// Base version (exclusive).
    pub since_version: i64,
    /// Maximum deltas to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Preferred checkpoint for compaction.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
}

impl Default for GetChangesRequest {
    fn default() -> Self {
        Self {
            since_version: 0,
            limit: Some(100),
            checkpoint_id: None,
        }
    }
}

/// Response from get changes request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetChangesResponse {
    /// Current server version.
    pub current_version: i64,
    /// Checkpoint ID for reference.
    pub checkpoint_id: String,
    /// List of delta batches.
    pub deltas: Vec<DeltaBatch>,
    /// Whether more deltas exist.
    pub has_more: bool,
    /// Sync type indicator.
    pub sync_type: SyncType,
}

/// Sync type indicator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncType {
    /// Incremental delta sync.
    Incremental,
    /// Full state sync.
    Full,
}

/// Conflict resolution strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConflictResolution {
    /// Server value wins (default).
    #[default]
    ServerWins,
    /// Client value wins.
    ClientWins,
    /// Attempt deep merge.
    Merge,
}

/// Apply delta options.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyOptions {
    /// Validate without applying.
    #[serde(default)]
    pub validate_only: bool,
    /// All-or-nothing apply.
    #[serde(default = "default_true")]
    pub atomic: bool,
    /// Conflict resolution strategy.
    #[serde(default)]
    pub conflict_resolution: ConflictResolution,
}

impl Default for ApplyOptions {
    fn default() -> Self {
        Self {
            validate_only: false,
            atomic: true,
            conflict_resolution: ConflictResolution::default(),
        }
    }
}

fn default_true() -> bool {
    true
}

/// Request to apply a delta batch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyDeltaRequest {
    /// Expected current version (for optimistic locking).
    pub expected_version: i64,
    /// Delta batch to apply.
    pub batch: DeltaBatch,
    /// Apply options.
    #[serde(default)]
    pub options: ApplyOptions,
}

/// Individual conflict detail.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    /// Path where conflict occurred.
    pub path: String,
    /// Server value at path.
    pub server_value: serde_json::Value,
    /// Client value at path.
    pub client_value: serde_json::Value,
}

/// Response from apply delta request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyDeltaResponse {
    /// Whether apply succeeded.
    pub success: bool,
    /// New version after apply.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_version: Option<i64>,
    /// Number of operations applied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub applied_operations: Option<u32>,
    /// Conflicts if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflicts: Option<Vec<Conflict>>,
    /// Whether a new checkpoint was created.
    #[serde(default)]
    pub checkpoint_created: bool,
    /// Error code if failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Full state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateSnapshot {
    /// Current version.
    pub version: i64,
    /// Checkpoint ID.
    pub checkpoint_id: String,
    /// State data.
    pub state: serde_json::Value,
    /// SHA-256 checksum.
    pub checksum: String,
}

/// Request for full state sync (fallback).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetStateRequest {
    /// Specific checkpoint ID (optional).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint_id: Option<String>,
    /// Force full sync even if deltas available.
    #[serde(default)]
    pub force_full: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Error Types
// ─────────────────────────────────────────────────────────────────────────────

/// Delta protocol errors.
#[derive(Debug, Clone, Error, PartialEq)]
pub enum DeltaError {
    #[error("invalid version: {0}")]
    InvalidVersion(String),

    #[error("invalid checkpoint: {0}")]
    InvalidCheckpoint(String),

    #[error("version conflict: expected {expected}, found {actual}")]
    VersionConflict { expected: i64, actual: i64 },

    #[error("delta too large: {size} bytes exceeds threshold {threshold}")]
    DeltaTooLarge { size: usize, threshold: usize },

    #[error("version expired: {version} older than oldest available {oldest}")]
    VersionExpired { version: i64, oldest: i64 },

    #[error("validation failed: {0}")]
    ValidationFailed(String),

    #[error("checkpoint not found: {0}")]
    CheckpointNotFound(String),

    #[error("operation not allowed: {0}")]
    OperationNotAllowed(String),
}

impl DeltaError {
    /// Convert to HTTP status code.
    pub fn status_code(&self) -> u16 {
        match self {
            Self::InvalidVersion(_) | Self::InvalidCheckpoint(_) | Self::ValidationFailed(_) => 400,
            Self::VersionConflict { .. } => 409,
            Self::DeltaTooLarge { .. } => 409,
            Self::VersionExpired { .. } => 410,
            Self::CheckpointNotFound(_) => 404,
            Self::OperationNotAllowed(_) => 403,
        }
    }

    /// Check if error is retryable.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::VersionConflict { .. })
    }

    /// Convert to error response JSON.
    pub fn to_error_response(&self) -> serde_json::Value {
        serde_json::json!({
            "error": self.error_code(),
            "message": self.to_string(),
        })
    }

    fn error_code(&self) -> &'static str {
        match self {
            Self::InvalidVersion(_) => "invalid_version",
            Self::InvalidCheckpoint(_) => "invalid_checkpoint",
            Self::VersionConflict { .. } => "version_conflict",
            Self::DeltaTooLarge { .. } => "delta_too_large",
            Self::VersionExpired { .. } => "version_expired",
            Self::ValidationFailed(_) => "validation_failed",
            Self::CheckpointNotFound(_) => "checkpoint_not_found",
            Self::OperationNotAllowed(_) => "operation_not_allowed",
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Protocol Service Trait
// ─────────────────────────────────────────────────────────────────────────────

/// Core protocol operations for incremental sync.
#[async_trait]
pub trait IncrementalSyncProtocol: Send + Sync {
    /// Get changes since a specific version.
    ///
    /// Returns delta batches from `since_version` to current.
    /// If delta is too large, returns error indicating full sync required.
    async fn get_changes(
        &self,
        session_id: &str,
        request: GetChangesRequest,
    ) -> Result<GetChangesResponse, DeltaError>;

    /// Apply a delta batch with optimistic locking.
    ///
    /// Uses `expected_version` for optimistic locking.
    /// Returns conflict details if version mismatch.
    async fn apply_delta(
        &self,
        session_id: &str,
        request: ApplyDeltaRequest,
    ) -> Result<ApplyDeltaResponse, DeltaError>;

    /// Get full state snapshot (fallback for large deltas).
    ///
    /// Used when incremental sync is not possible or efficient.
    async fn get_state(
        &self,
        session_id: &str,
        request: GetStateRequest,
    ) -> Result<StateSnapshot, DeltaError>;

    /// Create a manual checkpoint.
    ///
    /// Checkpoints enable recovery and delta compaction.
    async fn create_checkpoint(
        &self,
        session_id: &str,
        options: CheckpointOptions,
    ) -> Result<Checkpoint, DeltaError>;

    /// Get current version vector for a session.
    async fn get_version(&self, session_id: &str) -> Result<VersionVector, DeltaError>;

    /// Resolve conflicts with server-side merge.
    ///
    /// Used when automatic conflict resolution fails.
    async fn resolve_conflicts(
        &self,
        session_id: &str,
        conflicts: Vec<Conflict>,
        resolution: ConflictResolution,
    ) -> Result<ApplyDeltaResponse, DeltaError>;
}

// ─────────────────────────────────────────────────────────────────────────────
// Delta Application Engine
// ─────────────────────────────────────────────────────────────────────────────

/// Engine for applying delta operations to state.
pub struct DeltaEngine;

impl DeltaEngine {
    /// Apply a delta batch to a state value.
    pub fn apply(state: &mut serde_json::Value, batch: &DeltaBatch) -> Result<u32, DeltaError> {
        let mut applied = 0u32;

        for op in &batch.operations {
            Self::apply_op(state, op)?;
            applied += 1;
        }

        Ok(applied)
    }

    /// Apply a single delta operation.
    pub fn apply_op(state: &mut serde_json::Value, op: &DeltaOp) -> Result<(), DeltaError> {
        match op.op {
            DeltaOpType::Add => Self::apply_add(state, &op.path, op.value.as_ref()),
            DeltaOpType::Replace => Self::apply_replace(state, &op.path, op.value.as_ref()),
            DeltaOpType::Remove => Self::apply_remove(state, &op.path),
            DeltaOpType::Merge => Self::apply_merge(state, &op.path, op.value.as_ref()),
        }
    }

    fn apply_add(
        state: &mut serde_json::Value,
        path: &str,
        value: Option<&serde_json::Value>,
    ) -> Result<(), DeltaError> {
        let value = value
            .ok_or_else(|| DeltaError::ValidationFailed("add operation requires value".into()))?;

        let ptr = json_pointer(path)?;
        let parent = Self::get_parent_mut(state, &ptr)?;

        if let Some(key) = ptr.last() {
            match parent {
                serde_json::Value::Object(map) => {
                    if map.contains_key(key) {
                        return Err(DeltaError::ValidationFailed(format!(
                            "cannot add: path '{}' already exists",
                            path
                        )));
                    }
                    map.insert(key.to_string(), value.clone());
                }
                serde_json::Value::Array(arr) => {
                    let idx = key.parse::<usize>().map_err(|_| {
                        DeltaError::ValidationFailed(format!("invalid array index: {}", key))
                    })?;
                    if idx > arr.len() {
                        return Err(DeltaError::ValidationFailed(format!(
                            "array index {} out of bounds",
                            idx
                        )));
                    }
                    arr.insert(idx, value.clone());
                }
                _ => {
                    return Err(DeltaError::ValidationFailed(format!(
                        "cannot add to non-container at '{}'",
                        path
                    )));
                }
            }
        } else {
            // Empty path - replace root
            *state = value.clone();
        }

        Ok(())
    }

    fn apply_replace(
        state: &mut serde_json::Value,
        path: &str,
        value: Option<&serde_json::Value>,
    ) -> Result<(), DeltaError> {
        let value = value.ok_or_else(|| {
            DeltaError::ValidationFailed("replace operation requires value".into())
        })?;

        if path.is_empty() || path == "/" {
            *state = value.clone();
            return Ok(());
        }

        let ptr = json_pointer(path)?;
        let parent = Self::get_parent_mut(state, &ptr)?;

        if let Some(key) = ptr.last() {
            match parent {
                serde_json::Value::Object(map) => {
                    if !map.contains_key(key) {
                        return Err(DeltaError::ValidationFailed(format!(
                            "cannot replace: path '{}' not found",
                            path
                        )));
                    }
                    map.insert(key.to_string(), value.clone());
                }
                serde_json::Value::Array(arr) => {
                    let idx = key.parse::<usize>().map_err(|_| {
                        DeltaError::ValidationFailed(format!("invalid array index: {}", key))
                    })?;
                    if idx >= arr.len() {
                        return Err(DeltaError::ValidationFailed(format!(
                            "array index {} out of bounds",
                            idx
                        )));
                    }
                    arr[idx] = value.clone();
                }
                _ => {
                    return Err(DeltaError::ValidationFailed(format!(
                        "cannot replace in non-container at '{}'",
                        path
                    )));
                }
            }
        }

        Ok(())
    }

    fn apply_remove(state: &mut serde_json::Value, path: &str) -> Result<(), DeltaError> {
        if path.is_empty() || path == "/" {
            return Err(DeltaError::ValidationFailed("cannot remove root".into()));
        }

        let ptr = json_pointer(path)?;
        let parent = Self::get_parent_mut(state, &ptr)?;

        if let Some(key) = ptr.last() {
            match parent {
                serde_json::Value::Object(map) => {
                    map.remove(key);
                }
                serde_json::Value::Array(arr) => {
                    let idx = key.parse::<usize>().map_err(|_| {
                        DeltaError::ValidationFailed(format!("invalid array index: {}", key))
                    })?;
                    if idx < arr.len() {
                        arr.remove(idx);
                    }
                    // No error if already removed
                }
                _ => {}
            }
        }

        Ok(())
    }

    fn apply_merge(
        state: &mut serde_json::Value,
        path: &str,
        value: Option<&serde_json::Value>,
    ) -> Result<(), DeltaError> {
        let value = value
            .ok_or_else(|| DeltaError::ValidationFailed("merge operation requires value".into()))?;

        if path.is_empty() || path == "/" {
            match (state, value) {
                (serde_json::Value::Object(target), serde_json::Value::Object(source)) => {
                    merge_objects(target, source);
                }
                (state_ref, val) => {
                    *state_ref = val.clone();
                }
            }
            return Ok(());
        }

        let ptr = json_pointer(path)?;
        let parent = Self::get_parent_mut(state, &ptr)?;

        if let Some(key) = ptr.last() {
            match parent {
                serde_json::Value::Object(map) => {
                    if let Some(existing) = map.get_mut(key) {
                        if let (
                            serde_json::Value::Object(target),
                            serde_json::Value::Object(source),
                        ) = (existing, value)
                        {
                            merge_objects(target, source);
                        } else {
                            map.insert(key.to_string(), value.clone());
                        }
                    } else {
                        map.insert(key.to_string(), value.clone());
                    }
                }
                _ => {
                    return Err(DeltaError::ValidationFailed(format!(
                        "cannot merge into non-object at '{}'",
                        path
                    )));
                }
            }
        }

        Ok(())
    }

    fn get_parent_mut<'a>(
        state: &'a mut serde_json::Value,
        ptr: &[String],
    ) -> Result<&'a mut serde_json::Value, DeltaError> {
        if ptr.is_empty() {
            return Ok(state);
        }

        let mut current = state;
        let parent_len = ptr.len().saturating_sub(1);

        for key in ptr.iter().take(parent_len) {
            current = navigate_mut(current, key)?;
        }

        Ok(current)
    }
}

/// Parse JSON pointer path into components.
fn json_pointer(path: &str) -> Result<Vec<String>, DeltaError> {
    if path.is_empty() {
        return Ok(Vec::new());
    }

    let path = if let Some(stripped) = path.strip_prefix('/') {
        stripped
    } else {
        path
    };

    Ok(path.split('/').map(decode_json_pointer).collect())
}

/// Decode JSON pointer escape sequences.
fn decode_json_pointer(s: &str) -> String {
    s.replace("~1", "/").replace("~0", "~")
}

/// Navigate to a child value mutably.
fn navigate_mut<'a>(
    parent: &'a mut serde_json::Value,
    key: &str,
) -> Result<&'a mut serde_json::Value, DeltaError> {
    match parent {
        serde_json::Value::Object(map) => map.get_mut(key).ok_or_else(|| {
            DeltaError::ValidationFailed(format!("path segment '{}' not found", key))
        }),
        serde_json::Value::Array(arr) => {
            let idx = key.parse::<usize>().map_err(|_| {
                DeltaError::ValidationFailed(format!("invalid array index: {}", key))
            })?;
            arr.get_mut(idx).ok_or_else(|| {
                DeltaError::ValidationFailed(format!("array index {} out of bounds", idx))
            })
        }
        _ => Err(DeltaError::ValidationFailed(
            "cannot navigate into non-container".to_string(),
        )),
    }
}

/// Deep merge source object into target.
fn merge_objects(
    target: &mut serde_json::Map<String, serde_json::Value>,
    source: &serde_json::Map<String, serde_json::Value>,
) {
    for (key, value) in source {
        if let Some(existing) = target.get_mut(key) {
            if let (serde_json::Value::Object(t), serde_json::Value::Object(s)) = (existing, value)
            {
                merge_objects(t, s);
            } else {
                target.insert(key.clone(), value.clone());
            }
        } else {
            target.insert(key.clone(), value.clone());
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Full Sync Fallback Logic
// ─────────────────────────────────────────────────────────────────────────────

/// Thresholds for full sync fallback.
#[derive(Debug, Clone, Copy)]
pub struct FallbackThresholds {
    /// Delta size as percentage of full state (0-100).
    pub size_percentage: u8,
    /// Maximum cumulative delta operations.
    pub max_operations: u32,
    /// Maximum version gap.
    pub max_version_gap: i64,
}

impl Default for FallbackThresholds {
    fn default() -> Self {
        Self {
            size_percentage: 50,
            max_operations: 500,
            max_version_gap: 100,
        }
    }
}

/// Check if full sync should be used instead of delta sync.
pub fn should_fallback_to_full_sync(
    delta_size: usize,
    full_state_size: usize,
    operation_count: u32,
    version_gap: i64,
    thresholds: &FallbackThresholds,
) -> bool {
    if full_state_size == 0 {
        return false;
    }

    let size_pct = (delta_size * 100) / full_state_size;

    size_pct > thresholds.size_percentage as usize
        || operation_count > thresholds.max_operations
        || version_gap > thresholds.max_version_gap
}

/// Metrics for sync operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncMetrics {
    /// Sync duration in milliseconds.
    pub sync_duration_ms: u64,
    /// Bytes sent to server.
    pub bytes_sent: u64,
    /// Bytes received from server.
    pub bytes_received: u64,
    /// Number of operations applied.
    pub operations_applied: u32,
    /// Number of conflicts resolved.
    pub conflicts_resolved: u32,
    /// Whether full sync was used.
    pub fallback_to_full_sync: bool,
    /// Compression ratio achieved (0-100).
    pub compression_ratio: u8,
}

impl SyncMetrics {
    /// Calculate compression ratio from sizes.
    pub fn calculate_compression(original: usize, compressed: usize) -> u8 {
        if original == 0 {
            return 0;
        }
        let ratio = ((original - compressed) * 100) / original;
        ratio.min(100) as u8
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HTTP/gRPC Endpoint Constants
// ─────────────────────────────────────────────────────────────────────────────

/// HTTP endpoint paths for sync protocol.
pub mod endpoints {
    /// Base path for sync API.
    pub const BASE: &str = "/api/v1/sync";
    /// Get changes since version.
    pub const CHANGES: &str = "/api/v1/sync/changes";
    /// Apply delta batch.
    pub const APPLY: &str = "/api/v1/sync/apply";
    /// Get full state.
    pub const STATE: &str = "/api/v1/sync/state";
    /// Create checkpoint.
    pub const CHECKPOINT: &str = "/api/v1/sync/checkpoint";
    /// Resolve conflicts.
    pub const RESOLVE: &str = "/api/v1/sync/resolve";
}

/// gRPC service name.
pub const GRPC_SERVICE_NAME: &str = "sync.v1.SyncService";

/// gRPC method names.
pub mod grpc_methods {
    pub const STREAM_CHANGES: &str = "StreamChanges";
    pub const APPLY_DELTA: &str = "ApplyDelta";
    pub const GET_STATE: &str = "GetState";
    pub const CREATE_CHECKPOINT: &str = "CreateCheckpoint";
    pub const RESOLVE_CONFLICTS: &str = "ResolveConflicts";
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delta_op_constructors() {
        let add = DeltaOp::add("/entities/test", serde_json::json!({"name": "test"}));
        assert!(matches!(add.op, DeltaOpType::Add));
        assert_eq!(add.path, "/entities/test");

        let replace = DeltaOp::replace("/version", 42);
        assert!(matches!(replace.op, DeltaOpType::Replace));

        let remove = DeltaOp::remove("/old");
        assert!(matches!(remove.op, DeltaOpType::Remove));

        let merge = DeltaOp::merge("/config", serde_json::json!({"key": "value"}));
        assert!(matches!(merge.op, DeltaOpType::Merge));
    }

    #[test]
    fn delta_batch_basic() {
        let mut batch = DeltaBatch::new(10, "cp-1");
        assert_eq!(batch.source_version, 10);
        assert!(batch.is_empty());

        batch.push(DeltaOp::add("/a", 1));
        assert_eq!(batch.len(), 1);
        assert!(!batch.is_empty());

        assert!(batch.validate().is_ok());
    }

    #[test]
    fn delta_batch_validation() {
        let mut batch = DeltaBatch::new(-1, "cp-1");
        assert!(batch.validate().is_err());

        batch.source_version = 0;
        batch.target_version = 0;
        assert!(batch.validate().is_err());

        batch.checkpoint_id.clear();
        assert!(batch.validate().is_err());
    }

    #[test]
    fn version_vector_increment() {
        let mut vv = VersionVector::new("session-1", 1, "hash1");
        assert_eq!(vv.version, 1);

        vv.increment("hash2");
        assert_eq!(vv.version, 2);
        assert_eq!(vv.state_hash, "hash2");
    }

    #[test]
    fn delta_engine_add() {
        let mut state = serde_json::json!({"a": 1});
        let op = DeltaOp::add("/b", 2);

        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert_eq!(state["b"], 2);
    }

    #[test]
    fn delta_engine_add_duplicate_fails() {
        let mut state = serde_json::json!({"a": 1});
        let op = DeltaOp::add("/a", 2);

        assert!(DeltaEngine::apply_op(&mut state, &op).is_err());
    }

    #[test]
    fn delta_engine_replace() {
        let mut state = serde_json::json!({"a": 1, "b": {"c": 2}});
        let op = DeltaOp::replace("/b/c", 3);

        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert_eq!(state["b"]["c"], 3);
    }

    #[test]
    fn delta_engine_replace_missing_fails() {
        let mut state = serde_json::json!({"a": 1});
        let op = DeltaOp::replace("/nonexistent", 2);

        assert!(DeltaEngine::apply_op(&mut state, &op).is_err());
    }

    #[test]
    fn delta_engine_remove() {
        let mut state = serde_json::json!({"a": 1, "b": 2});
        let op = DeltaOp::remove("/a");

        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert!(state.get("a").is_none());
        assert_eq!(state["b"], 2);
    }

    #[test]
    fn delta_engine_merge() {
        let mut state = serde_json::json!({"config": {"a": 1, "b": 2}});
        let op = DeltaOp::merge("/config", serde_json::json!({"b": 3, "c": 4}));

        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert_eq!(state["config"]["a"], 1);
        assert_eq!(state["config"]["b"], 3);
        assert_eq!(state["config"]["c"], 4);
    }

    #[test]
    fn delta_engine_array_ops() {
        let mut state = serde_json::json!({"arr": [1, 2, 3]});

        // Add at index
        let op = DeltaOp::add("/arr/1", 99);
        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert_eq!(state["arr"], serde_json::json!([1, 99, 2, 3]));

        // Replace
        let op = DeltaOp::replace("/arr/0", 0);
        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert_eq!(state["arr"][0], 0);

        // Remove
        let op = DeltaOp::remove("/arr/2");
        DeltaEngine::apply_op(&mut state, &op).unwrap();
        assert_eq!(state["arr"], serde_json::json!([0, 99, 3]));
    }

    #[test]
    fn delta_engine_batch() {
        let mut state = serde_json::json!({"version": 1, "data": {"x": 1}});
        let batch = DeltaBatch {
            source_version: 1,
            target_version: 3,
            checkpoint_id: "cp-1".into(),
            operations: vec![
                DeltaOp::replace("/version", 2),
                DeltaOp::merge("/data", serde_json::json!({"y": 2})),
            ],
            timestamp: "2024-01-01T00:00:00Z".into(),
            entity_count: 0,
            pattern_count: 0,
            tombstones: None,
        };

        let applied = DeltaEngine::apply(&mut state, &batch).unwrap();
        assert_eq!(applied, 2);
        assert_eq!(state["version"], 2);
        assert_eq!(state["data"]["x"], 1);
        assert_eq!(state["data"]["y"], 2);
    }

    #[test]
    fn json_pointer_decoding() {
        assert_eq!(decode_json_pointer("a/b"), "a/b");
        assert_eq!(decode_json_pointer("a~1b"), "a/b");
        assert_eq!(decode_json_pointer("a~0b"), "a~b");
        assert_eq!(decode_json_pointer("a~0~1b"), "a~/b");
    }

    #[test]
    fn json_pointer_parsing() {
        let ptr = json_pointer("/foo/bar").unwrap();
        assert_eq!(ptr, vec!["foo", "bar"]);

        let ptr = json_pointer("/foo~1bar/baz").unwrap();
        assert_eq!(ptr, vec!["foo/bar", "baz"]);

        let ptr = json_pointer("").unwrap();
        assert!(ptr.is_empty());
    }

    #[test]
    fn fallback_thresholds() {
        let thresholds = FallbackThresholds::default();

        // Delta is 60% of full state - should fallback
        assert!(should_fallback_to_full_sync(60, 100, 10, 10, &thresholds));

        // Delta is 40% of full state - should not fallback
        assert!(!should_fallback_to_full_sync(40, 100, 10, 10, &thresholds));

        // Too many operations
        assert!(should_fallback_to_full_sync(10, 100, 600, 10, &thresholds));

        // Large version gap
        assert!(should_fallback_to_full_sync(10, 100, 10, 200, &thresholds));
    }

    #[test]
    fn sync_metrics_compression() {
        assert_eq!(SyncMetrics::calculate_compression(100, 25), 75);
        assert_eq!(SyncMetrics::calculate_compression(100, 100), 0);
        assert_eq!(SyncMetrics::calculate_compression(0, 0), 0);
    }

    #[test]
    fn error_codes() {
        let err = DeltaError::VersionConflict {
            expected: 1,
            actual: 2,
        };
        assert_eq!(err.status_code(), 409);
        assert!(err.is_retryable());
        assert_eq!(err.error_code(), "version_conflict");

        let err = DeltaError::DeltaTooLarge {
            size: 100,
            threshold: 50,
        };
        assert_eq!(err.status_code(), 409);
        assert!(!err.is_retryable());
        assert_eq!(err.error_code(), "delta_too_large");

        let err = DeltaError::ValidationFailed("test".into());
        assert_eq!(err.status_code(), 400);
        assert!(!err.is_retryable());
    }

    #[test]
    fn conflict_resolution_default() {
        assert_eq!(
            ConflictResolution::default(),
            ConflictResolution::ServerWins
        );
    }

    #[test]
    fn apply_options_default() {
        let opts = ApplyOptions::default();
        assert!(!opts.validate_only);
        assert!(opts.atomic);
        assert_eq!(opts.conflict_resolution, ConflictResolution::ServerWins);
    }

    // --- DeltaError methods ---

    #[test]
    fn delta_error_status_codes() {
        assert_eq!(DeltaError::InvalidVersion("v".into()).status_code(), 400);
        assert_eq!(DeltaError::InvalidCheckpoint("c".into()).status_code(), 400);
        assert_eq!(DeltaError::ValidationFailed("f".into()).status_code(), 400);
        assert_eq!(DeltaError::VersionConflict { expected: 1, actual: 2 }.status_code(), 409);
        assert_eq!(DeltaError::DeltaTooLarge { size: 100, threshold: 50 }.status_code(), 409);
        assert_eq!(DeltaError::VersionExpired { version: 1, oldest: 5 }.status_code(), 410);
        assert_eq!(DeltaError::CheckpointNotFound("x".into()).status_code(), 404);
        assert_eq!(DeltaError::OperationNotAllowed("x".into()).status_code(), 403);
    }

    #[test]
    fn delta_error_retryable() {
        assert!(DeltaError::VersionConflict { expected: 1, actual: 2 }.is_retryable());
        assert!(!DeltaError::InvalidVersion("v".into()).is_retryable());
        assert!(!DeltaError::CheckpointNotFound("x".into()).is_retryable());
        assert!(!DeltaError::DeltaTooLarge { size: 1, threshold: 1 }.is_retryable());
    }

    #[test]
    fn delta_error_response_json() {
        let err = DeltaError::VersionConflict { expected: 1, actual: 2 };
        let resp = err.to_error_response();
        assert_eq!(resp["error"], "version_conflict");
        assert!(resp["message"].as_str().unwrap().contains("expected 1"));
    }

    #[test]
    fn delta_error_display() {
        let err = DeltaError::DeltaTooLarge { size: 100, threshold: 50 };
        assert!(err.to_string().contains("100"));
        assert!(err.to_string().contains("50"));
    }

    // --- DeltaBatch approx_size and validate ---

    #[test]
    fn delta_batch_approx_size() {
        let batch = DeltaBatch::new(1, "cp1");
        let initial = batch.approx_size();
        let mut batch2 = DeltaBatch::new(1, "cp1");
        batch2.push(DeltaOp::add("/a", serde_json::json!("x")));
        assert!(batch2.approx_size() > initial);
    }
}
