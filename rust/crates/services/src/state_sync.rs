//! State convergence: sync learning state between edge (local files) and cloud (MatrixOne).
//!
//! # Architecture
//!
//! ```text
//!   Edge (CLI)                          Cloud (MatrixOne)
//!   ─────────                          ──────────────────
//!   ~/.mo-agent/learning/            learning_snapshots table
//!     {profile}.json         ──push──▶  (user_id, profile, json)
//!                            ◀──pull──
//!
//!   ~/.mo-agent/sessions/            agent_sessions + agent_events
//!     workspace.yaml         ──push──▶  (metadata sync)
//!     journal.jsonl          ──push──▶  (event ingestion)
//!
//!   User preferences         ◀──pull──  user_preferences table
//! ```
//!
//! # Sync Protocol
//!
//! - **Local-first**: Edge always writes locally first, then async pushes to cloud
//! - **Last-writer-wins**: For preferences, most recent update wins
//! - **Merge-on-pull**: For learning data, entity/pattern observations are merged
//! - **Conflict resolution**: Higher observation count wins for entities; union for patterns
//! - **Idempotent**: Repeated pushes produce same result (UPSERT semantics)

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ─── Sync Types ─────────────────────────────────────────────────────────────

/// Direction of a sync operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncDirection {
    /// Edge → Cloud
    Push,
    /// Cloud → Edge
    Pull,
}

/// Result of a sync operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub direction: SyncDirection,
    pub sync_type: String,
    pub success: bool,
    pub items_synced: u32,
    pub message: String,
    /// New cloud version after successful push (for optimistic locking).
    #[serde(default)]
    pub new_version: Option<i64>,
    /// Whether this was a conflict (version mismatch).
    #[serde(default)]
    pub is_conflict: bool,
}

impl SyncResult {
    pub fn ok(direction: SyncDirection, sync_type: &str, items: u32) -> Self {
        Self {
            direction,
            sync_type: sync_type.to_string(),
            success: true,
            items_synced: items,
            message: "ok".to_string(),
            new_version: None,
            is_conflict: false,
        }
    }

    pub fn ok_with_version(direction: SyncDirection, sync_type: &str, items: u32, version: i64) -> Self {
        Self {
            direction,
            sync_type: sync_type.to_string(),
            success: true,
            items_synced: items,
            message: "ok".to_string(),
            new_version: Some(version),
            is_conflict: false,
        }
    }

    pub fn err(direction: SyncDirection, sync_type: &str, msg: impl Into<String>) -> Self {
        Self {
            direction,
            sync_type: sync_type.to_string(),
            success: false,
            items_synced: 0,
            message: msg.into(),
            new_version: None,
            is_conflict: false,
        }
    }

    pub fn conflict(direction: SyncDirection, sync_type: &str, msg: impl Into<String>) -> Self {
        Self {
            direction,
            sync_type: sync_type.to_string(),
            success: false,
            items_synced: 0,
            message: msg.into(),
            new_version: None,
            is_conflict: true,
        }
    }
}

/// Learning snapshot with version for optimistic locking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedSnapshot {
    /// The JSON-serialized learning snapshot.
    pub json: String,
    /// Cloud version number (for optimistic locking).
    pub version: i64,
}

/// Metadata about the current sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncStatus {
    pub learning_last_push: Option<String>,
    pub learning_last_pull: Option<String>,
    pub preferences_last_sync: Option<String>,
    pub pending_pushes: u32,
    pub last_error: Option<String>,
    /// Last known cloud version (for optimistic locking).
    #[serde(default)]
    pub cloud_version: Option<i64>,
}

// ─── State Sync Service Trait ───────────────────────────────────────────────

/// Abstract sync service for learning state convergence.
///
/// Implementations:
/// - `LocalOnlySyncService` — no-op for offline/edge-only mode
/// - `MatrixOneSyncService` — full cloud sync via database
/// - Mock implementations for testing
///
/// # Optimistic Locking
///
/// The `push_learning_versioned` method uses optimistic locking to prevent
/// concurrent sessions from overwriting each other's changes:
///
/// 1. Call `pull_learning_versioned` to get `(json, version)`
/// 2. Merge cloud data with local changes
/// 3. Call `push_learning_versioned(expected_version=version)` to push
/// 4. If another session pushed in between, returns `is_conflict=true`
/// 5. On conflict, re-pull, re-merge, and retry
///
/// The non-versioned `push_learning` method always succeeds (last-writer-wins).
#[async_trait]
pub trait StateSyncService: Send + Sync {
    /// Push local learning snapshot to cloud (last-writer-wins, no version check).
    ///
    /// Use `push_learning_versioned` for concurrent-safe updates.
    async fn push_learning(
        &self,
        user_id: &str,
        profile: &str,
        snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
    ) -> SyncResult;

    /// Push local learning snapshot with optimistic locking.
    ///
    /// - `expected_version`: The version returned by the last `pull_learning_versioned`.
    ///   Pass `None` to create a new snapshot (fails if one already exists).
    ///
    /// Returns:
    /// - `success=true, new_version=Some(v)` on success
    /// - `success=false, is_conflict=true` if version mismatch (another session pushed)
    /// - `success=false, is_conflict=false` on other errors
    async fn push_learning_versioned(
        &self,
        user_id: &str,
        profile: &str,
        snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
        expected_version: Option<i64>,
    ) -> SyncResult;

    /// Pull learning snapshot from cloud (without version).
    async fn pull_learning(&self, user_id: &str, profile: &str) -> Result<Option<String>, String>;

    /// Pull learning snapshot with version for optimistic locking.
    ///
    /// Returns `None` if no snapshot exists, or `Some((json, version))`.
    async fn pull_learning_versioned(
        &self,
        user_id: &str,
        profile: &str,
    ) -> Result<Option<VersionedSnapshot>, String>;

    /// Push a user preference to cloud.
    async fn push_preference(&self, user_id: &str, key: &str, value: &str) -> SyncResult;

    /// Pull a user preference from cloud.
    async fn pull_preference(&self, user_id: &str, key: &str) -> Result<Option<String>, String>;

    /// Pull all preferences for a user.
    async fn pull_all_preferences(&self, user_id: &str) -> Result<Vec<(String, String)>, String>;

    /// Get current sync status.
    async fn status(&self) -> SyncStatus;
}

// ─── Local-Only Implementation (No Cloud) ───────────────────────────────────

/// No-op implementation for edge-only mode.
/// All operations succeed instantly without network calls.
pub struct LocalOnlySyncService;

#[async_trait]
impl StateSyncService for LocalOnlySyncService {
    async fn push_learning(
        &self,
        _user_id: &str,
        _profile: &str,
        _snapshot_json: &str,
        _entity_count: u32,
        _pattern_count: u32,
        _has_calibration: bool,
    ) -> SyncResult {
        SyncResult::ok(SyncDirection::Push, "learning", 0)
    }

    async fn push_learning_versioned(
        &self,
        _user_id: &str,
        _profile: &str,
        _snapshot_json: &str,
        _entity_count: u32,
        _pattern_count: u32,
        _has_calibration: bool,
        _expected_version: Option<i64>,
    ) -> SyncResult {
        // Local-only: always succeeds with version 0
        SyncResult::ok_with_version(SyncDirection::Push, "learning", 0, 0)
    }

    async fn pull_learning(
        &self,
        _user_id: &str,
        _profile: &str,
    ) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn pull_learning_versioned(
        &self,
        _user_id: &str,
        _profile: &str,
    ) -> Result<Option<VersionedSnapshot>, String> {
        Ok(None)
    }

    async fn push_preference(&self, _user_id: &str, _key: &str, _value: &str) -> SyncResult {
        SyncResult::ok(SyncDirection::Push, "preference", 0)
    }

    async fn pull_preference(&self, _user_id: &str, _key: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn pull_all_preferences(&self, _user_id: &str) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }

    async fn status(&self) -> SyncStatus {
        SyncStatus::default()
    }
}

// ─── MatrixOne Cloud Implementation ─────────────────────────────────────────

/// Full cloud sync via MatrixOne database.
///
/// Uses sqlx connection pool for async operations. Implements UPSERT semantics
/// via INSERT ... ON DUPLICATE KEY UPDATE for idempotent push operations.
///
/// Tables used:
/// - `learning_snapshots` — cross-session learning state
/// - `user_preferences` — user settings
/// - `session_sync_log` — audit trail
pub struct MatrixOneSyncService {
    pool: sqlx::Pool<sqlx::MySql>,
}

impl MatrixOneSyncService {
    /// Create from an existing connection pool.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>) -> Self {
        Self { pool }
    }

    /// Log a sync operation to the audit table.
    #[allow(clippy::too_many_arguments)]
    async fn log_sync(
        &self,
        user_id: &str,
        session_id: &str,
        sync_type: &str,
        direction: SyncDirection,
        payload_size: usize,
        status: &str,
        error_msg: Option<&str>,
    ) {
        let dir_str = match direction {
            SyncDirection::Push => "push",
            SyncDirection::Pull => "pull",
        };
        let _ = sqlx::query(
            "INSERT INTO session_sync_log \
             (sync_id, user_id, session_id, sync_type, sync_direction, payload_size, status, error_message, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW())",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(user_id)
        .bind(session_id)
        .bind(sync_type)
        .bind(dir_str)
        .bind(payload_size as i64)
        .bind(status)
        .bind(error_msg)
        .execute(&self.pool)
        .await;
    }
}

#[async_trait]
impl StateSyncService for MatrixOneSyncService {
    async fn push_learning(
        &self,
        user_id: &str,
        profile: &str,
        snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
    ) -> SyncResult {
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let has_cal = if has_calibration { 1i32 } else { 0 };

        // Two-step UPSERT: UPDATE existing row, then INSERT if no row existed.
        // MatrixOne may not support ON DUPLICATE KEY UPDATE on UNIQUE keys,
        // so we use an explicit UPDATE-then-INSERT pattern.
        let updated = sqlx::query(
            "UPDATE learning_snapshots SET \
                snapshot_json = ?, \
                entity_count = ?, \
                pattern_count = ?, \
                has_calibration = ?, \
                version = version + 1, \
                updated_at = NOW() \
             WHERE user_id = ? AND profile_name = ?",
        )
        .bind(snapshot_json)
        .bind(entity_count as i64)
        .bind(pattern_count as i64)
        .bind(has_cal)
        .bind(user_id)
        .bind(profile)
        .execute(&self.pool)
        .await;

        let result = match updated {
            Ok(r) if r.rows_affected() > 0 => Ok(r),
            Ok(_) => {
                // No existing row — insert fresh
                sqlx::query(
                    "INSERT INTO learning_snapshots \
                     (snapshot_id, user_id, profile_name, snapshot_json, entity_count, \
                      pattern_count, has_calibration, version, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, 1, NOW(), NOW())",
                )
                .bind(&snapshot_id)
                .bind(user_id)
                .bind(profile)
                .bind(snapshot_json)
                .bind(entity_count as i64)
                .bind(pattern_count as i64)
                .bind(has_cal)
                .execute(&self.pool)
                .await
            }
            Err(e) => Err(e),
        };

        match result {
            Ok(_) => {
                self.log_sync(
                    user_id,
                    "",
                    "learning",
                    SyncDirection::Push,
                    snapshot_json.len(),
                    "success",
                    None,
                )
                .await;
                SyncResult::ok(SyncDirection::Push, "learning", 1)
            }
            Err(e) => {
                let msg = format!("push_learning: {e}");
                self.log_sync(
                    user_id,
                    "",
                    "learning",
                    SyncDirection::Push,
                    0,
                    "error",
                    Some(&msg),
                )
                .await;
                SyncResult::err(SyncDirection::Push, "learning", msg)
            }
        }
    }

    async fn push_learning_versioned(
        &self,
        user_id: &str,
        profile: &str,
        snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
        expected_version: Option<i64>,
    ) -> SyncResult {
        let snapshot_id = uuid::Uuid::new_v4().to_string();
        let has_cal = if has_calibration { 1i32 } else { 0 };

        match expected_version {
            Some(ver) => {
                // Optimistic lock: UPDATE only if version matches
                let updated = sqlx::query(
                    "UPDATE learning_snapshots SET \
                        snapshot_json = ?, \
                        entity_count = ?, \
                        pattern_count = ?, \
                        has_calibration = ?, \
                        version = version + 1, \
                        updated_at = NOW() \
                     WHERE user_id = ? AND profile_name = ? AND version = ?",
                )
                .bind(snapshot_json)
                .bind(entity_count as i64)
                .bind(pattern_count as i64)
                .bind(has_cal)
                .bind(user_id)
                .bind(profile)
                .bind(ver)
                .execute(&self.pool)
                .await;

                match updated {
                    Ok(r) if r.rows_affected() > 0 => {
                        let new_ver = ver + 1;
                        self.log_sync(
                            user_id,
                            "",
                            "learning_versioned",
                            SyncDirection::Push,
                            snapshot_json.len(),
                            "success",
                            None,
                        )
                        .await;
                        SyncResult::ok_with_version(SyncDirection::Push, "learning", 1, new_ver)
                    }
                    Ok(_) => {
                        // No rows affected — version mismatch (conflict)
                        self.log_sync(
                            user_id,
                            "",
                            "learning_versioned",
                            SyncDirection::Push,
                            0,
                            "conflict",
                            Some(&format!("expected version {ver}")),
                        )
                        .await;
                        SyncResult::conflict(
                            SyncDirection::Push,
                            "learning",
                            format!("version conflict: expected {ver}, snapshot was modified by another session"),
                        )
                    }
                    Err(e) => {
                        let msg = format!("push_learning_versioned: {e}");
                        self.log_sync(
                            user_id,
                            "",
                            "learning_versioned",
                            SyncDirection::Push,
                            0,
                            "error",
                            Some(&msg),
                        )
                        .await;
                        SyncResult::err(SyncDirection::Push, "learning", msg)
                    }
                }
            }
            None => {
                // No expected version — create new (fail if exists)
                let inserted = sqlx::query(
                    "INSERT INTO learning_snapshots \
                     (snapshot_id, user_id, profile_name, snapshot_json, entity_count, \
                      pattern_count, has_calibration, version, created_at, updated_at) \
                     VALUES (?, ?, ?, ?, ?, ?, ?, 1, NOW(), NOW())",
                )
                .bind(&snapshot_id)
                .bind(user_id)
                .bind(profile)
                .bind(snapshot_json)
                .bind(entity_count as i64)
                .bind(pattern_count as i64)
                .bind(has_cal)
                .execute(&self.pool)
                .await;

                match inserted {
                    Ok(_) => {
                        self.log_sync(
                            user_id,
                            "",
                            "learning_versioned",
                            SyncDirection::Push,
                            snapshot_json.len(),
                            "success",
                            None,
                        )
                        .await;
                        SyncResult::ok_with_version(SyncDirection::Push, "learning", 1, 1)
                    }
                    Err(e) => {
                        // Likely duplicate key — snapshot already exists
                        let msg = format!("push_learning_versioned (new): {e}");
                        let is_dup = msg.contains("Duplicate") || msg.contains("duplicate");
                        self.log_sync(
                            user_id,
                            "",
                            "learning_versioned",
                            SyncDirection::Push,
                            0,
                            if is_dup { "conflict" } else { "error" },
                            Some(&msg),
                        )
                        .await;
                        if is_dup {
                            SyncResult::conflict(
                                SyncDirection::Push,
                                "learning",
                                "snapshot already exists; use expected_version to update",
                            )
                        } else {
                            SyncResult::err(SyncDirection::Push, "learning", msg)
                        }
                    }
                }
            }
        }
    }

    async fn pull_learning(&self, user_id: &str, profile: &str) -> Result<Option<String>, String> {
        let row = sqlx::query(
            "SELECT snapshot_json FROM learning_snapshots \
             WHERE user_id = ? AND profile_name = ? \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(profile)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("pull_learning: {e}"))?;

        match row {
            Some(row) => {
                use sqlx::Row;
                let json: String = row
                    .try_get("snapshot_json")
                    .map_err(|e| format!("pull_learning decode: {e}"))?;
                self.log_sync(
                    user_id,
                    "",
                    "learning",
                    SyncDirection::Pull,
                    json.len(),
                    "success",
                    None,
                )
                .await;
                Ok(Some(json))
            }
            None => Ok(None),
        }
    }

    async fn pull_learning_versioned(
        &self,
        user_id: &str,
        profile: &str,
    ) -> Result<Option<VersionedSnapshot>, String> {
        let row = sqlx::query(
            "SELECT snapshot_json, version FROM learning_snapshots \
             WHERE user_id = ? AND profile_name = ? \
             ORDER BY updated_at DESC LIMIT 1",
        )
        .bind(user_id)
        .bind(profile)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("pull_learning_versioned: {e}"))?;

        match row {
            Some(row) => {
                use sqlx::Row;
                let json: String = row
                    .try_get("snapshot_json")
                    .map_err(|e| format!("pull_learning_versioned decode json: {e}"))?;
                let version: i64 = row
                    .try_get("version")
                    .map_err(|e| format!("pull_learning_versioned decode version: {e}"))?;
                self.log_sync(
                    user_id,
                    "",
                    "learning_versioned",
                    SyncDirection::Pull,
                    json.len(),
                    "success",
                    None,
                )
                .await;
                Ok(Some(VersionedSnapshot { json, version }))
            }
            None => Ok(None),
        }
    }

    async fn push_preference(&self, user_id: &str, key: &str, value: &str) -> SyncResult {
        let pref_id = uuid::Uuid::new_v4().to_string();

        let result = sqlx::query(
            "INSERT INTO user_preferences (pref_id, user_id, pref_key, pref_value, updated_at) \
             VALUES (?, ?, ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE pref_value = VALUES(pref_value), updated_at = NOW()",
        )
        .bind(&pref_id)
        .bind(user_id)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await;

        match result {
            Ok(_) => SyncResult::ok(SyncDirection::Push, "preference", 1),
            Err(e) => SyncResult::err(SyncDirection::Push, "preference", format!("push_pref: {e}")),
        }
    }

    async fn pull_preference(&self, user_id: &str, key: &str) -> Result<Option<String>, String> {
        let row = sqlx::query(
            "SELECT pref_value FROM user_preferences WHERE user_id = ? AND pref_key = ?",
        )
        .bind(user_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("pull_pref: {e}"))?;

        match row {
            Some(row) => {
                use sqlx::Row;
                let val: String = row
                    .try_get("pref_value")
                    .map_err(|e| format!("pull_pref decode: {e}"))?;
                Ok(Some(val))
            }
            None => Ok(None),
        }
    }

    async fn pull_all_preferences(&self, user_id: &str) -> Result<Vec<(String, String)>, String> {
        let rows = sqlx::query(
            "SELECT pref_key, pref_value FROM user_preferences WHERE user_id = ? ORDER BY pref_key",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("pull_all_prefs: {e}"))?;

        use sqlx::Row;
        let prefs = rows
            .iter()
            .filter_map(|row| {
                let key: String = row.try_get("pref_key").ok()?;
                let val: String = row.try_get("pref_value").ok()?;
                Some((key, val))
            })
            .collect();
        Ok(prefs)
    }

    async fn status(&self) -> SyncStatus {
        // Query latest sync timestamps from audit log
        let learning_push = sqlx::query(
            "SELECT CAST(created_at AS CHAR) AS created_at FROM session_sync_log \
             WHERE sync_type = 'learning' AND sync_direction = 'push' AND status = 'success' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| {
            use sqlx::Row;
            row.try_get::<String, _>("created_at").ok()
        });

        let learning_pull = sqlx::query(
            "SELECT CAST(created_at AS CHAR) AS created_at FROM session_sync_log \
             WHERE sync_type = 'learning' AND sync_direction = 'pull' AND status = 'success' \
             ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| {
            use sqlx::Row;
            row.try_get::<String, _>("created_at").ok()
        });

        let pending: u32 =
            sqlx::query("SELECT COUNT(*) as cnt FROM session_sync_log WHERE status = 'pending'")
                .fetch_optional(&self.pool)
                .await
                .ok()
                .flatten()
                .and_then(|row| {
                    use sqlx::Row;
                    row.try_get::<i64, _>("cnt").ok().map(|c| c as u32)
                })
                .unwrap_or(0);

        let last_err = sqlx::query(
            "SELECT error_message FROM session_sync_log \
             WHERE status = 'error' ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .and_then(|row| {
            use sqlx::Row;
            row.try_get::<Option<String>, _>("error_message")
                .ok()
                .flatten()
        });

        SyncStatus {
            learning_last_push: learning_push,
            learning_last_pull: learning_pull,
            preferences_last_sync: None,
            pending_pushes: pending,
            last_error: last_err,
            cloud_version: None, // Could be fetched from DB if needed
        }
    }
}

// ─── Preference Constants ───────────────────────────────────────────────────

/// Well-known preference keys.
pub mod pref_keys {
    pub const EXPLAIN_MODE: &str = "explain_mode";
    pub const DEFAULT_MODEL: &str = "default_model";
    pub const TOOL_BUDGET: &str = "tool_budget_tokens";
    pub const CHECKPOINT_INTERVAL: &str = "checkpoint_interval";
    pub const FOCUS_ENTITIES: &str = "focus_entities";
    pub const LANGUAGE: &str = "language";
}

// ─── File-based Preference Store ────────────────────────────────────────────

/// Load preferences from a local JSON file.
pub fn load_local_preferences(path: &Path) -> Result<Vec<(String, String)>, String> {
    let data = match std::fs::read_to_string(path) {
        Ok(d) => d,
        Err(_) => return Ok(Vec::new()),
    };
    let map: std::collections::HashMap<String, String> =
        serde_json::from_str(&data).map_err(|e| format!("parse prefs: {e}"))?;
    Ok(map.into_iter().collect())
}

/// Save preferences to a local JSON file (atomic write).
pub fn save_local_preferences(path: &Path, prefs: &[(String, String)]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir: {e}"))?;
    }
    let map: std::collections::HashMap<&str, &str> = prefs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let json = serde_json::to_string_pretty(&map).map_err(|e| format!("serialize: {e}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &json).map_err(|e| format!("write: {e}"))?;
    std::fs::rename(&tmp, path).map_err(|e| format!("rename: {e}"))?;
    Ok(())
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // ── SyncResult ──

    #[test]
    fn sync_result_ok_and_err() {
        let ok = SyncResult::ok(SyncDirection::Push, "learning", 5);
        assert!(ok.success);
        assert_eq!(ok.items_synced, 5);
        assert_eq!(ok.direction, SyncDirection::Push);

        let err = SyncResult::err(SyncDirection::Pull, "learning", "network timeout");
        assert!(!err.success);
        assert_eq!(err.message, "network timeout");
    }

    // ── LocalOnlySyncService ──

    #[tokio::test]
    async fn local_only_push_succeeds() {
        let svc = LocalOnlySyncService;
        let result = svc
            .push_learning("user1", "default", "{}", 0, 0, false)
            .await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn local_only_pull_returns_none() {
        let svc = LocalOnlySyncService;
        let result = svc.pull_learning("user1", "default").await;
        assert!(result.unwrap().is_none());
    }

    #[tokio::test]
    async fn local_only_preferences() {
        let svc = LocalOnlySyncService;
        let push = svc.push_preference("user1", "key", "value").await;
        assert!(push.success);
        let pull = svc.pull_preference("user1", "key").await;
        assert!(pull.unwrap().is_none());
    }

    // ── File-based preferences ──

    #[test]
    fn preferences_roundtrip_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("prefs.json");

        let prefs = vec![
            ("model".to_string(), "gpt-4".to_string()),
            ("language".to_string(), "zh-CN".to_string()),
        ];
        save_local_preferences(&path, &prefs).unwrap();

        let loaded = load_local_preferences(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().any(|(k, v)| k == "model" && v == "gpt-4"));
    }

    #[test]
    fn load_nonexistent_preferences_returns_empty() {
        let prefs = load_local_preferences(Path::new("/nonexistent/prefs.json")).unwrap();
        assert!(prefs.is_empty());
    }

    #[test]
    fn preferences_atomic_write() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("atomic-prefs.json");

        save_local_preferences(&path, &[("k".into(), "v".into())]).unwrap();
        assert!(path.exists());
        assert!(!path.with_extension("json.tmp").exists());
    }

    // ── Preference key constants ──

    #[test]
    fn pref_keys_are_defined() {
        assert_eq!(pref_keys::EXPLAIN_MODE, "explain_mode");
        assert_eq!(pref_keys::DEFAULT_MODEL, "default_model");
        assert_eq!(pref_keys::TOOL_BUDGET, "tool_budget_tokens");
    }

    // ── SyncStatus ──

    #[test]
    fn sync_status_default_is_clean() {
        let status = SyncStatus::default();
        assert!(status.learning_last_push.is_none());
        assert_eq!(status.pending_pushes, 0);
        assert!(status.last_error.is_none());
    }

    // ── Serialization ──

    #[test]
    fn sync_result_json_roundtrip() {
        let result = SyncResult::ok(SyncDirection::Push, "learning", 3);
        let json = serde_json::to_string(&result).unwrap();
        let loaded: SyncResult = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.items_synced, 3);
        assert!(loaded.success);
    }

    #[tokio::test]
    async fn local_only_status_is_default() {
        let svc = LocalOnlySyncService;
        let status = svc.status().await;
        assert!(status.last_error.is_none());
        assert_eq!(status.pending_pushes, 0);
    }

    // ── MatrixOneSyncService tests (mock-based) ──

    #[test]
    fn sync_direction_serializes_correctly() {
        // Verify JSON serialization of direction (used in sync logs)
        let push_json = serde_json::to_string(&SyncDirection::Push).unwrap();
        let pull_json = serde_json::to_string(&SyncDirection::Pull).unwrap();

        assert_ne!(
            push_json, pull_json,
            "Push and Pull must serialize differently"
        );

        let push_back: SyncDirection = serde_json::from_str(&push_json).unwrap();
        let pull_back: SyncDirection = serde_json::from_str(&pull_json).unwrap();

        assert_eq!(push_back, SyncDirection::Push);
        assert_eq!(pull_back, SyncDirection::Pull);
    }

    #[test]
    fn sync_result_ok_contains_expected_fields() {
        let result = SyncResult::ok(SyncDirection::Push, "learning", 5);

        assert!(result.success);
        assert_eq!(result.direction, SyncDirection::Push);
        assert_eq!(result.sync_type, "learning");
        assert_eq!(result.items_synced, 5);
        assert_eq!(result.message, "ok");
    }

    #[test]
    fn sync_result_err_contains_error_message() {
        let result = SyncResult::err(SyncDirection::Pull, "preferences", "connection refused");

        assert!(!result.success);
        assert_eq!(result.direction, SyncDirection::Pull);
        assert_eq!(result.sync_type, "preferences");
        assert_eq!(result.items_synced, 0);
        assert_eq!(result.message, "connection refused");
    }

    #[test]
    fn sync_result_json_roundtrip_preserves_all_fields() {
        let original = SyncResult {
            direction: SyncDirection::Push,
            sync_type: "learning".to_string(),
            success: true,
            items_synced: 10,
            message: "synced 10 entities".to_string(),
            new_version: Some(5),
            is_conflict: false,
        };

        let json = serde_json::to_string(&original).unwrap();
        let restored: SyncResult = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.direction, original.direction);
        assert_eq!(restored.sync_type, original.sync_type);
        assert_eq!(restored.success, original.success);
        assert_eq!(restored.items_synced, original.items_synced);
        assert_eq!(restored.message, original.message);
        assert_eq!(restored.new_version, original.new_version);
        assert_eq!(restored.is_conflict, original.is_conflict);
    }

    #[test]
    fn sync_status_default_has_clean_state() {
        let status = SyncStatus::default();

        assert!(status.learning_last_push.is_none());
        assert!(status.learning_last_pull.is_none());
        assert!(status.preferences_last_sync.is_none());
        assert_eq!(status.pending_pushes, 0);
        assert!(status.last_error.is_none());
        assert!(status.cloud_version.is_none());
    }

    #[test]
    fn sync_status_with_values_roundtrips_through_json() {
        let original = SyncStatus {
            learning_last_push: Some("2024-01-01T00:00:00Z".to_string()),
            learning_last_pull: Some("2024-01-02T00:00:00Z".to_string()),
            preferences_last_sync: Some("2024-01-03T00:00:00Z".to_string()),
            pending_pushes: 3,
            last_error: Some("connection refused".to_string()),
            cloud_version: Some(42),
        };

        let json = serde_json::to_string(&original).unwrap();
        let restored: SyncStatus = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.learning_last_push, original.learning_last_push);
        assert_eq!(restored.pending_pushes, original.pending_pushes);
        assert_eq!(restored.last_error, original.last_error);
        assert_eq!(restored.cloud_version, original.cloud_version);
    }

    #[tokio::test]
    async fn local_only_push_learning_is_noop_but_succeeds() {
        let svc = LocalOnlySyncService;

        // Push with actual data
        let result = svc
            .push_learning(
                "user1",
                "default",
                r#"{"entities":[{"name":"test","count":5}]}"#,
                1,
                0,
                true,
            )
            .await;

        // Should succeed (no-op)
        assert!(result.success, "LocalOnly should always succeed");
        assert_eq!(result.items_synced, 0, "LocalOnly doesn't actually sync");
    }

    #[tokio::test]
    async fn local_only_pull_learning_returns_none_for_any_user() {
        let svc = LocalOnlySyncService;

        // Try pulling for different users/profiles
        let result1 = svc.pull_learning("user1", "default").await;
        let result2 = svc.pull_learning("user2", "work").await;

        assert!(result1.unwrap().is_none());
        assert!(result2.unwrap().is_none());
    }

    #[tokio::test]
    async fn local_only_push_and_pull_preference_roundtrip() {
        let svc = LocalOnlySyncService;

        // Push preference
        let push_result = svc.push_preference("user1", "model", "gpt-4").await;
        assert!(push_result.success);

        // Pull returns none (LocalOnly has no storage)
        let pull_result = svc.pull_preference("user1", "model").await;
        assert!(pull_result.unwrap().is_none());
    }

    #[tokio::test]
    async fn local_only_pull_all_preferences_returns_empty() {
        let svc = LocalOnlySyncService;

        let result = svc.pull_all_preferences("user1").await;
        assert!(result.unwrap().is_empty());
    }

    #[tokio::test]
    async fn local_only_status_reflects_no_activity() {
        let svc = LocalOnlySyncService;

        let status = svc.status().await;

        assert!(status.last_error.is_none());
        assert_eq!(status.pending_pushes, 0);
        assert!(status.learning_last_push.is_none());
    }

    // ── Optimistic Locking Tests ──

    #[tokio::test]
    async fn local_only_versioned_push_succeeds_with_version_zero() {
        let svc = LocalOnlySyncService;

        let result = svc
            .push_learning_versioned("user1", "default", "{}", 0, 0, false, None)
            .await;

        assert!(result.success);
        assert_eq!(result.new_version, Some(0)); // LocalOnly always returns 0
        assert!(!result.is_conflict);
    }

    #[tokio::test]
    async fn local_only_versioned_pull_returns_none() {
        let svc = LocalOnlySyncService;

        let result = svc.pull_learning_versioned("user1", "default").await;

        assert!(result.unwrap().is_none());
    }

    #[test]
    fn sync_result_conflict_has_is_conflict_flag() {
        let result = SyncResult::conflict(SyncDirection::Push, "learning", "version mismatch");

        assert!(!result.success);
        assert!(result.is_conflict);
        assert!(result.message.contains("version"));
    }

    #[test]
    fn versioned_snapshot_roundtrips_through_json() {
        let original = VersionedSnapshot {
            json: r#"{"entities": []}"#.to_string(),
            version: 42,
        };

        let serialized = serde_json::to_string(&original).unwrap();
        let restored: VersionedSnapshot = serde_json::from_str(&serialized).unwrap();

        assert_eq!(restored.json, original.json);
        assert_eq!(restored.version, original.version);
    }

    #[test]
    fn sync_result_ok_with_version_includes_version() {
        let result = SyncResult::ok_with_version(SyncDirection::Push, "learning", 1, 5);

        assert!(result.success);
        assert_eq!(result.new_version, Some(5));
        assert!(!result.is_conflict);
    }
}
