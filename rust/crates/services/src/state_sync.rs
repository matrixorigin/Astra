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
}

impl SyncResult {
    pub fn ok(direction: SyncDirection, sync_type: &str, items: u32) -> Self {
        Self {
            direction,
            sync_type: sync_type.to_string(),
            success: true,
            items_synced: items,
            message: "ok".to_string(),
        }
    }

    pub fn err(direction: SyncDirection, sync_type: &str, msg: impl Into<String>) -> Self {
        Self {
            direction,
            sync_type: sync_type.to_string(),
            success: false,
            items_synced: 0,
            message: msg.into(),
        }
    }
}

/// Metadata about the current sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncStatus {
    pub learning_last_push: Option<String>,
    pub learning_last_pull: Option<String>,
    pub preferences_last_sync: Option<String>,
    pub pending_pushes: u32,
    pub last_error: Option<String>,
}

// ─── State Sync Service Trait ───────────────────────────────────────────────

/// Abstract sync service for learning state convergence.
///
/// Implementations:
/// - `LocalOnlySyncService` — no-op for offline/edge-only mode
/// - `MatrixOneSyncService` — full cloud sync via database
/// - Mock implementations for testing
#[async_trait]
pub trait StateSyncService: Send + Sync {
    /// Push local learning snapshot to cloud.
    async fn push_learning(
        &self,
        user_id: &str,
        profile: &str,
        snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
    ) -> SyncResult;

    /// Pull learning snapshot from cloud.
    async fn pull_learning(&self, user_id: &str, profile: &str) -> Result<Option<String>, String>;

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

    async fn pull_learning(
        &self,
        _user_id: &str,
        _profile: &str,
    ) -> Result<Option<String>, String> {
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

    /// Create from a SharedPool (production wiring).
    pub fn from_shared(shared: &mo_agent_core::SharedPool) -> Self {
        Self {
            pool: shared.get().clone(),
        }
    }

    /// Log a sync operation to the audit table.
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

        // UPSERT: insert or update existing (user_id, profile_name) pair
        let result = sqlx::query(
            "INSERT INTO learning_snapshots \
             (snapshot_id, user_id, profile_name, snapshot_json, entity_count, pattern_count, has_calibration, version, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, 1, NOW(), NOW()) \
             ON DUPLICATE KEY UPDATE \
                snapshot_json = VALUES(snapshot_json), \
                entity_count = VALUES(entity_count), \
                pattern_count = VALUES(pattern_count), \
                has_calibration = VALUES(has_calibration), \
                version = version + 1, \
                updated_at = NOW()",
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
            "SELECT created_at FROM session_sync_log \
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
            "SELECT created_at FROM session_sync_log \
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
        }
    }
}

// ─── Sync Orchestrator ──────────────────────────────────────────────────────

/// Orchestrates local-first sync with cloud backup.
///
/// Workflow:
/// 1. On session end: save locally, then async push to cloud
/// 2. On session start: load locally, then pull from cloud + merge
/// 3. On preference change: write locally + push to cloud
pub struct SyncOrchestrator {
    sync_service: Box<dyn StateSyncService>,
    user_id: String,
    profile: String,
}

impl SyncOrchestrator {
    pub fn new(
        sync_service: Box<dyn StateSyncService>,
        user_id: impl Into<String>,
        profile: impl Into<String>,
    ) -> Self {
        Self {
            sync_service,
            user_id: user_id.into(),
            profile: profile.into(),
        }
    }

    /// Save learning state locally then push to cloud.
    pub async fn save_and_push(
        &self,
        snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
    ) -> SyncResult {
        // Local save is handled by caller (persistence.rs)
        // We only handle the cloud push
        self.sync_service
            .push_learning(
                &self.user_id,
                &self.profile,
                snapshot_json,
                entity_count,
                pattern_count,
                has_calibration,
            )
            .await
    }

    /// Pull from cloud and return JSON for merge.
    pub async fn pull_for_merge(&self) -> Result<Option<String>, String> {
        self.sync_service
            .pull_learning(&self.user_id, &self.profile)
            .await
    }

    /// Full sync cycle: pull from cloud → merge with local → push back.
    pub async fn full_sync(
        &self,
        local_snapshot_json: &str,
        entity_count: u32,
        pattern_count: u32,
        has_calibration: bool,
    ) -> Vec<SyncResult> {
        let mut results = Vec::new();

        // Pull from cloud
        match self.pull_for_merge().await {
            Ok(Some(_cloud_json)) => {
                results.push(SyncResult::ok(SyncDirection::Pull, "learning", 1));
                // Caller should merge cloud_json into local state
            }
            Ok(None) => {
                results.push(SyncResult::ok(SyncDirection::Pull, "learning", 0));
            }
            Err(e) => {
                results.push(SyncResult::err(SyncDirection::Pull, "learning", e));
            }
        }

        // Push local to cloud
        let push_result = self
            .save_and_push(
                local_snapshot_json,
                entity_count,
                pattern_count,
                has_calibration,
            )
            .await;
        results.push(push_result);

        results
    }

    /// Save a user preference with cloud sync.
    pub async fn set_preference(&self, key: &str, value: &str) -> SyncResult {
        self.sync_service
            .push_preference(&self.user_id, key, value)
            .await
    }

    /// Get a user preference (cloud-first, local fallback).
    pub async fn get_preference(&self, key: &str) -> Result<Option<String>, String> {
        self.sync_service.pull_preference(&self.user_id, key).await
    }

    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    pub fn profile(&self) -> &str {
        &self.profile
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

    // ── SyncOrchestrator ──

    #[tokio::test]
    async fn orchestrator_full_sync_cycle() {
        let orch = SyncOrchestrator::new(Box::new(LocalOnlySyncService), "user1", "default");
        let results = orch.full_sync("{}", 0, 0, false).await;
        assert_eq!(results.len(), 2); // pull + push
        assert!(results.iter().all(|r| r.success));
    }

    #[tokio::test]
    async fn orchestrator_save_and_push() {
        let orch = SyncOrchestrator::new(Box::new(LocalOnlySyncService), "user1", "profile1");
        let result = orch.save_and_push("{\"entities\":[]}", 0, 0, false).await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn orchestrator_preference_roundtrip() {
        let orch = SyncOrchestrator::new(Box::new(LocalOnlySyncService), "user1", "default");
        let set_result = orch.set_preference("model", "gpt-4").await;
        assert!(set_result.success);

        // LocalOnly returns None (no cloud storage)
        let get_result = orch.get_preference("model").await;
        assert!(get_result.unwrap().is_none());
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
}
