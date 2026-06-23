//! State convergence: sync metadata and preferences between edge (local files) and cloud (MatrixOne).
//!
//! # Architecture
//!
//! ```text
//!   Edge (CLI)                          Cloud (MatrixOne)
//!   ─────────                          ──────────────────
//!   ~/.astra/sessions/            agent_sessions + agent_events
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
//! - **Idempotent**: Repeated pushes produce same result (UPSERT semantics)

use astra_core::is_duplicate_key_error;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use std::path::Path;

/// Bounded channel capacity for the async audit writer. If the channel is full,
/// audit entries are dropped (acceptable — audit is observability, not business logic).
const AUDIT_CHANNEL_CAPACITY: usize = 256;
/// Flush audit entries after this many accumulate.
const AUDIT_FLUSH_BATCH_SIZE: usize = 64;
/// Flush audit entries after this duration even if batch is not full.
const AUDIT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

// ─── Async Audit Writer ────────────────────────────────────────────────────

/// A single audit row destined for `session_sync_log`. Fire-and-forget.
pub struct SyncAuditEntry {
    pub user_id: String,
    pub session_id: String,
    pub sync_type: String,
    pub direction: SyncDirection,
    pub payload_size: usize,
    pub status: String,
    pub error_message: Option<String>,
}

/// Non-blocking handle for emitting audit entries into a bounded channel.
/// If the channel is full the entry is silently dropped — audit is best-effort.
#[derive(Clone)]
pub struct SyncAuditWriter {
    tx: tokio::sync::mpsc::Sender<SyncAuditEntry>,
}

impl SyncAuditWriter {
    pub fn log(&self, entry: SyncAuditEntry) {
        match self.tx.try_send(entry) {
            Ok(()) => {}
            Err(tokio::sync::mpsc::error::TrySendError::Full(dropped)) => {
                static DROPPED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let n = DROPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
                if n.is_power_of_two() || n == 1 {
                    tracing::warn!(
                        target: "astra_services::audit",
                        session_id = %dropped.session_id,
                        sync_type = %dropped.sync_type,
                        total_dropped = n,
                        "sync audit channel full, entry dropped"
                    );
                }
            }
            Err(tokio::sync::mpsc::error::TrySendError::Closed(dropped)) => {
                tracing::error!(
                    target: "astra_services::audit",
                    session_id = %dropped.session_id,
                    sync_type = %dropped.sync_type,
                    "sync audit channel closed (flusher dead?), entry dropped"
                );
            }
        }
    }
}

/// Handle returned by [`spawn_audit_flusher`]. Groups the writer, shutdown
/// token, and background task join handle.
pub struct AuditFlusherHandle {
    pub writer: SyncAuditWriter,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub join_handle: tokio::task::JoinHandle<()>,
}

/// Spawn the audit flusher background task.
///
/// Clean shutdown: call `handle.shutdown.cancel()`, then `.await handle.join_handle`.
pub fn spawn_audit_flusher(pool: sqlx::Pool<sqlx::MySql>) -> AuditFlusherHandle {
    let (tx, rx) = tokio::sync::mpsc::channel(AUDIT_CHANNEL_CAPACITY);
    let token = tokio_util::sync::CancellationToken::new();
    let join_handle = tokio::spawn(run_audit_flusher(rx, pool, token.clone()));
    AuditFlusherHandle {
        writer: SyncAuditWriter { tx },
        shutdown: token,
        join_handle,
    }
}

async fn run_audit_flusher(
    mut rx: tokio::sync::mpsc::Receiver<SyncAuditEntry>,
    pool: sqlx::Pool<sqlx::MySql>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let mut buf: Vec<SyncAuditEntry> = Vec::with_capacity(AUDIT_FLUSH_BATCH_SIZE);
    loop {
        let deadline = tokio::time::sleep(AUDIT_FLUSH_INTERVAL);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                entry = rx.recv() => {
                    match entry {
                        Some(e) => {
                            buf.push(e);
                            if buf.len() >= AUDIT_FLUSH_BATCH_SIZE { break; }
                        }
                        None => {
                            flush_audit_batch(&pool, &mut buf).await;
                            return;
                        }
                    }
                }
                _ = &mut deadline => { break; }
                _ = shutdown.cancelled() => {
                    while let Ok(e) = rx.try_recv() {
                        buf.push(e);
                    }
                    flush_audit_batch(&pool, &mut buf).await;
                    return;
                }
            }
        }
        flush_audit_batch(&pool, &mut buf).await;
    }
}

async fn flush_audit_batch(pool: &sqlx::Pool<sqlx::MySql>, buf: &mut Vec<SyncAuditEntry>) {
    if buf.is_empty() {
        return;
    }
    let mut sql = String::from(
        "INSERT INTO session_sync_log \
         (sync_id, user_id, session_id, sync_type, sync_direction, \
          payload_size, status, error_message, created_at) VALUES ",
    );
    for (i, _) in buf.iter().enumerate() {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push_str("(?, ?, ?, ?, ?, ?, ?, ?, NOW())");
    }
    let mut q = sqlx::query(&sql);
    for entry in buf.iter() {
        let dir_str = match entry.direction {
            SyncDirection::Push => "push",
            SyncDirection::Pull => "pull",
        };
        q = q
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(&entry.user_id)
            .bind(&entry.session_id)
            .bind(&entry.sync_type)
            .bind(dir_str)
            .bind(entry.payload_size as i64)
            .bind(&entry.status)
            .bind(entry.error_message.as_deref());
    }
    if let Err(e) = q.execute(pool).await {
        tracing::warn!(
            target: "astra_services::audit",
            batch_size = buf.len(),
            error = %e,
            "failed to flush sync audit batch"
        );
    }
    buf.clear();
}

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

    pub fn ok_with_version(
        direction: SyncDirection,
        sync_type: &str,
        items: u32,
        version: i64,
    ) -> Self {
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

/// One row from `plan_templates`, serialized for edge pull sync.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanTemplateSyncRow {
    pub template_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    pub goal_pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_type: Option<String>,
    pub template_json: String,
    pub success_rate: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub avg_completion_time: Option<i32>,
    pub use_count: i32,
}

const MAX_PREFERENCE_SYNC_ROWS: i64 = 128;
const MAX_PLAN_TEMPLATE_SYNC_ROWS: i64 = 500;
const MAX_PLAN_SYNC_ROWS: i64 = 200;
const MAX_PLAN_STEP_RUN_SYNC_ROWS: i64 = 2000;

/// Upper bounds on untrusted step-run fields pushed from edges. Rejects
/// nonsensical clock values and oversized audit strings before they reach
/// the DB, matching the caps enforced by the HTTP plan handlers.
///
/// `STEP_RUN_CLOCK_SKEW_YEARS` accepts ±10 years from the cloud's current
/// time — a legitimate offline CLI session lasting more than a decade is
/// unrealistic, while clocks off by centuries (1970 default, 2099 default)
/// are a clear sign of a misconfigured device.
const STEP_RUN_CLOCK_SKEW_YEARS: i64 = 10;
const STEP_RUN_MAX_ERROR_LEN: usize = 10_000;
const STEP_RUN_MAX_ARTIFACT_REF_LEN: usize = 1_000;

fn validate_step_run_timestamps(
    started_at: chrono::DateTime<chrono::Utc>,
    finished_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<(), String> {
    let now = chrono::Utc::now();
    let window = chrono::Duration::days(365 * STEP_RUN_CLOCK_SKEW_YEARS);
    if started_at < now - window || started_at > now + window {
        return Err(format!(
            "started_at={started_at} outside ±{STEP_RUN_CLOCK_SKEW_YEARS}y skew window; \
             reject suspected-bad-clock edge"
        ));
    }
    if let Some(finished) = finished_at {
        if finished < started_at {
            return Err(format!(
                "finished_at={finished} is before started_at={started_at}; \
                 causally impossible"
            ));
        }
        if finished < now - window || finished > now + window {
            return Err(format!(
                "finished_at={finished} outside ±{STEP_RUN_CLOCK_SKEW_YEARS}y skew window"
            ));
        }
    }
    Ok(())
}

fn validate_step_run_strings(run: &PlanStepRunSyncRow) -> Result<(), String> {
    if let Some(ref e) = run.error
        && e.len() > STEP_RUN_MAX_ERROR_LEN
    {
        return Err(format!(
            "step_run.error exceeds {STEP_RUN_MAX_ERROR_LEN} chars ({} got)",
            e.len()
        ));
    }
    if let Some(ref a) = run.artifact_ref
        && a.len() > STEP_RUN_MAX_ARTIFACT_REF_LEN
    {
        return Err(format!(
            "step_run.artifact_ref exceeds {STEP_RUN_MAX_ARTIFACT_REF_LEN} chars ({} got)",
            a.len()
        ));
    }
    Ok(())
}

/// One row from `plans`, serialized for edge↔cloud sync. Plans carry the
/// full serialized `PlanModeState` in `plan_json` so edge can hydrate an
/// identical view without needing every column in the table.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanSyncRow {
    pub plan_id: String,
    pub user_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub goal: String,
    pub phase: String,
    pub version: i64,
    pub plan_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_md: Option<String>,
    pub progress_pct: i32,
    /// Denormalized subtask count, maintained by `PlanRepository::save`.
    /// Let the list endpoint render a card without parsing `plan_json`.
    pub subtask_count: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// One row from `plan_step_runs`. The run history is the audit chain that
/// makes plan execution traceable across sessions.
///
/// `started_at` / `finished_at` are the edge's actual execution timestamps
/// — when a CLI executes offline and later syncs, the cloud must preserve
/// the original timeline, not overwrite with sync-time `NOW()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStepRunSyncRow {
    pub run_id: String,
    pub plan_id: String,
    pub subtask_id: String,
    pub attempt: i32,
    pub status: String,
    pub session_id: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_ref: Option<String>,
}

/// Metadata about the current sync state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncStatus {
    pub preferences_last_sync: Option<String>,
    pub pending_pushes: u32,
    pub last_error: Option<String>,
}

// ─── State Sync Service Trait ───────────────────────────────────────────────

/// Abstract sync service for edge↔cloud metadata convergence.
///
/// Implementations:
/// - `LocalOnlySyncService` — no-op for offline/edge-only mode
/// - `MatrixOneSyncService` — full cloud sync via database
/// - Mock implementations for testing
#[async_trait]
pub trait StateSyncService: Send + Sync {
    /// Push a user preference to cloud.
    async fn push_preference(&self, user_id: &str, key: &str, value: &str) -> SyncResult;

    /// Pull a user preference from cloud.
    async fn pull_preference(&self, user_id: &str, key: &str) -> Result<Option<String>, String>;

    /// Pull all preferences for a user.
    async fn pull_all_preferences(&self, user_id: &str) -> Result<Vec<(String, String)>, String>;

    /// JSON array of [`PlanTemplateSyncRow`] for the user plus global rows (`user_id IS NULL`).
    async fn pull_plan_templates_pack(&self, user_id: &str) -> Result<String, String>;

    /// JSON array of [`PlanSyncRow`] for the user's owned plans (newest first).
    /// Returns `{"plans": [...], "step_runs": [...]}` — the `plans` table rows
    /// and matching `plan_step_runs` history so edge can replay attempt chains
    /// offline. One envelope simplifies atomic edge-side hydration.
    async fn pull_plans_pack(&self, user_id: &str) -> Result<String, String>;

    /// Accept a JSON envelope `{"plans": [...], "step_runs": [...]}` produced
    /// by edge while offline. Upserts plans (optimistic version check) and
    /// appends previously-unseen step-runs (idempotent by `run_id`).
    async fn push_plans_pack(&self, user_id: &str, pack_json: &str) -> Result<String, String>;

    /// JSON array of [`crate::task_orchestrator::TaskRecord`] for the user (`agent_tasks`).
    async fn pull_tasks_pack(&self, user_id: &str) -> Result<String, String>;

    /// Apply a task pack from an edge that holds valid leases (`holder_agent_id`).
    async fn push_tasks_pack_held(
        &self,
        user_id: &str,
        holder_agent_id: &str,
        pack_json: &str,
    ) -> Result<crate::multi_agent::TasksPackPushResult, String>;

    /// Get current sync status.
    async fn status(&self) -> SyncStatus;
}

// ─── Local-Only Implementation (No Cloud) ───────────────────────────────────

/// No-op implementation for edge-only mode.
/// All operations succeed instantly without network calls.
pub struct LocalOnlySyncService;

#[async_trait]
impl StateSyncService for LocalOnlySyncService {
    async fn push_preference(&self, _user_id: &str, _key: &str, _value: &str) -> SyncResult {
        SyncResult::ok(SyncDirection::Push, "preference", 0)
    }

    async fn pull_preference(&self, _user_id: &str, _key: &str) -> Result<Option<String>, String> {
        Ok(None)
    }

    async fn pull_all_preferences(&self, _user_id: &str) -> Result<Vec<(String, String)>, String> {
        Ok(Vec::new())
    }

    async fn pull_plan_templates_pack(&self, _user_id: &str) -> Result<String, String> {
        Ok("[]".to_string())
    }

    async fn pull_plans_pack(&self, _user_id: &str) -> Result<String, String> {
        Ok(r#"{"plans":[],"step_runs":[]}"#.to_string())
    }

    async fn push_plans_pack(&self, _user_id: &str, _pack_json: &str) -> Result<String, String> {
        Ok(r#"{"applied":0,"skipped":0}"#.to_string())
    }

    async fn pull_tasks_pack(&self, _user_id: &str) -> Result<String, String> {
        Ok("[]".to_string())
    }

    async fn push_tasks_pack_held(
        &self,
        _user_id: &str,
        _holder_agent_id: &str,
        _pack_json: &str,
    ) -> Result<crate::multi_agent::TasksPackPushResult, String> {
        Ok(crate::multi_agent::TasksPackPushResult::default())
    }

    async fn status(&self) -> SyncStatus {
        SyncStatus::default()
    }
}

// ─── MatrixOne Cloud Implementation ─────────────────────────────────────────

/// Full cloud sync via MatrixOne database.
///
/// Uses sqlx connection pool for async operations. Strongly-consistent state uses
/// explicit update/insert flows, while audit-style records remain append-only.
///
/// Tables used:
/// - `user_preferences` — user settings
/// - `session_sync_log` — audit trail (written via async `SyncAuditWriter`)
pub struct MatrixOneSyncService {
    pub(crate) pool: sqlx::Pool<sqlx::MySql>,
    pub(crate) audit: SyncAuditWriter,
}

impl MatrixOneSyncService {
    /// Create from an existing connection pool and shared audit writer.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>, audit: SyncAuditWriter) -> Self {
        Self { pool, audit }
    }
}

#[async_trait]
impl StateSyncService for MatrixOneSyncService {
    async fn push_preference(&self, user_id: &str, key: &str, value: &str) -> SyncResult {
        let pref_id = uuid::Uuid::new_v4().to_string();

        // Read current value + version for audit trail
        let old_row = sqlx::query(
            "SELECT pref_value, version FROM user_preferences WHERE user_id = ? AND pref_key = ?",
        )
        .bind(user_id)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();

        let (old_value, old_version): (Option<String>, Option<i32>) = {
            use sqlx::Row;
            match &old_row {
                Some(row) => (row.try_get("pref_value").ok(), row.try_get("version").ok()),
                None => (None, None),
            }
        };

        // Skip write if value unchanged
        if old_value.as_deref() == Some(value) {
            return SyncResult::ok(SyncDirection::Push, "preference", 0);
        }

        let new_version = old_version.unwrap_or(0) + 1;

        // Upsert with version increment
        let update_result = sqlx::query(
            "UPDATE user_preferences SET pref_value = ?, version = ?, updated_at = NOW() \
             WHERE user_id = ? AND pref_key = ?",
        )
        .bind(value)
        .bind(new_version)
        .bind(user_id)
        .bind(key)
        .execute(&self.pool)
        .await;

        let result = match update_result {
            Ok(r) if r.rows_affected() > 0 => Ok(r),
            Ok(_) => {
                let inserted = sqlx::query(
                    "INSERT INTO user_preferences (pref_id, user_id, pref_key, pref_value, version, updated_at) \
                     VALUES (?, ?, ?, ?, ?, NOW())",
                )
                .bind(&pref_id)
                .bind(user_id)
                .bind(key)
                .bind(value)
                .bind(new_version)
                .execute(&self.pool)
                .await;

                match inserted {
                    Ok(r) => Ok(r),
                    Err(e) if is_duplicate_key_error(&e) => {
                        sqlx::query(
                            "UPDATE user_preferences SET pref_value = ?, version = ?, updated_at = NOW() \
                             WHERE user_id = ? AND pref_key = ?",
                        )
                        .bind(value)
                        .bind(new_version)
                        .bind(user_id)
                        .bind(key)
                        .execute(&self.pool)
                        .await
                    }
                    Err(e) => Err(e),
                }
            }
            Err(e) => Err(e),
        };

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
            "SELECT pref_key, pref_value \
             FROM user_preferences \
             WHERE user_id = ? \
             ORDER BY pref_key \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(MAX_PREFERENCE_SYNC_ROWS)
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

    async fn pull_plan_templates_pack(&self, user_id: &str) -> Result<String, String> {
        let rows = sqlx::query(
            "SELECT template_id, user_id, goal_pattern, project_type, template_json, \
              success_rate, avg_completion_time, use_count \
              FROM plan_templates \
              WHERE user_id = ? OR user_id IS NULL \
              ORDER BY updated_at DESC \
              LIMIT ?",
        )
        .bind(user_id)
        .bind(MAX_PLAN_TEMPLATE_SYNC_ROWS)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("pull_plan_templates_pack: {e}"))?;

        use sqlx::Row;
        let mut items: Vec<PlanTemplateSyncRow> = Vec::with_capacity(rows.len());
        for row in rows {
            let template_id: String = row
                .try_get("template_id")
                .map_err(|e| format!("pull_plan_templates_pack template_id: {e}"))?;
            let user_id_col: Option<String> = row
                .try_get("user_id")
                .map_err(|e| format!("pull_plan_templates_pack user_id: {e}"))?;
            let goal_pattern: String = row
                .try_get("goal_pattern")
                .map_err(|e| format!("pull_plan_templates_pack goal_pattern: {e}"))?;
            let project_type: Option<String> = row
                .try_get("project_type")
                .map_err(|e| format!("pull_plan_templates_pack project_type: {e}"))?;
            let template_json: String = row
                .try_get("template_json")
                .map_err(|e| format!("pull_plan_templates_pack template_json: {e}"))?;
            let success_rate: f32 = row
                .try_get::<f64, _>("success_rate")
                .map_err(|e| format!("pull_plan_templates_pack success_rate: {e}"))?
                as f32;
            let avg_completion_time: Option<i32> = row
                .try_get("avg_completion_time")
                .map_err(|e| format!("pull_plan_templates_pack avg_completion_time: {e}"))?;
            let use_count: i32 = row
                .try_get::<i64, _>("use_count")
                .map_err(|e| format!("pull_plan_templates_pack use_count: {e}"))?
                as i32;
            items.push(PlanTemplateSyncRow {
                template_id,
                user_id: user_id_col,
                goal_pattern,
                project_type,
                template_json,
                success_rate,
                avg_completion_time,
                use_count,
            });
        }
        serde_json::to_string(&items).map_err(|e| format!("pull_plan_templates_pack json: {e}"))
    }

    async fn pull_plans_pack(&self, user_id: &str) -> Result<String, String> {
        use sqlx::Row;

        // 1. Plans owned by this user, newest first.
        let plan_rows = sqlx::query(
            "SELECT plan_id, user_id, session_id, goal, phase, version, plan_json, \
                    plan_md, progress_pct, subtask_count, created_by \
             FROM plans \
             WHERE user_id = ? \
             ORDER BY updated_at DESC \
             LIMIT ?",
        )
        .bind(user_id)
        .bind(MAX_PLAN_SYNC_ROWS)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| format!("pull_plans_pack plans: {e}"))?;

        let mut plans: Vec<PlanSyncRow> = Vec::with_capacity(plan_rows.len());
        let mut plan_ids: Vec<String> = Vec::with_capacity(plan_rows.len());
        for row in plan_rows {
            let plan_id: String = row
                .try_get("plan_id")
                .map_err(|e| format!("pull_plans_pack plan_id: {e}"))?;
            plan_ids.push(plan_id.clone());
            plans.push(PlanSyncRow {
                plan_id,
                user_id: row
                    .try_get("user_id")
                    .map_err(|e| format!("pull_plans_pack user_id: {e}"))?,
                session_id: row
                    .try_get("session_id")
                    .map_err(|e| format!("pull_plans_pack session_id: {e}"))?,
                goal: row
                    .try_get("goal")
                    .map_err(|e| format!("pull_plans_pack goal: {e}"))?,
                phase: row
                    .try_get("phase")
                    .map_err(|e| format!("pull_plans_pack phase: {e}"))?,
                version: row
                    .try_get("version")
                    .map_err(|e| format!("pull_plans_pack version: {e}"))?,
                plan_json: row
                    .try_get("plan_json")
                    .map_err(|e| format!("pull_plans_pack plan_json: {e}"))?,
                plan_md: row
                    .try_get("plan_md")
                    .map_err(|e| format!("pull_plans_pack plan_md: {e}"))?,
                progress_pct: row
                    .try_get("progress_pct")
                    .map_err(|e| format!("pull_plans_pack progress_pct: {e}"))?,
                subtask_count: row
                    .try_get("subtask_count")
                    .map_err(|e| format!("pull_plans_pack subtask_count: {e}"))?,
                created_by: row
                    .try_get("created_by")
                    .map_err(|e| format!("pull_plans_pack created_by: {e}"))?,
            });
        }

        // 2. Step-run history for those plans (bounded so we don't ship
        //    unbounded history on every pull).
        let mut step_runs: Vec<PlanStepRunSyncRow> = Vec::new();
        if !plan_ids.is_empty() {
            let placeholders = (0..plan_ids.len())
                .map(|_| "?")
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT run_id, plan_id, subtask_id, attempt, status, session_id, \
                        started_at, finished_at, request_id, error, artifact_ref \
                 FROM plan_step_runs \
                 WHERE plan_id IN ({placeholders}) \
                 ORDER BY started_at DESC \
                 LIMIT ?"
            );
            let mut q = sqlx::query(&sql);
            for id in &plan_ids {
                q = q.bind(id);
            }
            q = q.bind(MAX_PLAN_STEP_RUN_SYNC_ROWS);
            let rows = q
                .fetch_all(&self.pool)
                .await
                .map_err(|e| format!("pull_plans_pack step_runs: {e}"))?;
            step_runs.reserve(rows.len());
            for row in rows {
                step_runs.push(PlanStepRunSyncRow {
                    run_id: row
                        .try_get("run_id")
                        .map_err(|e| format!("pull_plans_pack run_id: {e}"))?,
                    plan_id: row
                        .try_get("plan_id")
                        .map_err(|e| format!("pull_plans_pack run_plan_id: {e}"))?,
                    subtask_id: row
                        .try_get("subtask_id")
                        .map_err(|e| format!("pull_plans_pack subtask_id: {e}"))?,
                    attempt: row
                        .try_get("attempt")
                        .map_err(|e| format!("pull_plans_pack attempt: {e}"))?,
                    status: row
                        .try_get("status")
                        .map_err(|e| format!("pull_plans_pack status: {e}"))?,
                    session_id: row
                        .try_get("session_id")
                        .map_err(|e| format!("pull_plans_pack step session_id: {e}"))?,
                    started_at: row
                        .try_get("started_at")
                        .map_err(|e| format!("pull_plans_pack started_at: {e}"))?,
                    finished_at: row
                        .try_get("finished_at")
                        .map_err(|e| format!("pull_plans_pack finished_at: {e}"))?,
                    request_id: row
                        .try_get("request_id")
                        .map_err(|e| format!("pull_plans_pack request_id: {e}"))?,
                    error: row
                        .try_get("error")
                        .map_err(|e| format!("pull_plans_pack error: {e}"))?,
                    artifact_ref: row
                        .try_get("artifact_ref")
                        .map_err(|e| format!("pull_plans_pack artifact_ref: {e}"))?,
                });
            }
        }

        serde_json::to_string(&serde_json::json!({
            "plans": plans,
            "step_runs": step_runs,
        }))
        .map_err(|e| format!("pull_plans_pack json: {e}"))
    }

    async fn push_plans_pack(&self, user_id: &str, pack_json: &str) -> Result<String, String> {
        #[derive(Deserialize)]
        struct Pack {
            #[serde(default)]
            plans: Vec<PlanSyncRow>,
            #[serde(default)]
            step_runs: Vec<PlanStepRunSyncRow>,
        }
        let pack: Pack =
            serde_json::from_str(pack_json).map_err(|e| format!("push_plans_pack parse: {e}"))?;

        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| format!("push_plans_pack begin: {e}"))?;

        // Partition incoming plans into user-owned (to process) and cross-user
        // (to reject up-front). Cross-user drops never hit the DB.
        let mut skipped_plans = 0usize;
        let owned_plans: Vec<&PlanSyncRow> = pack
            .plans
            .iter()
            .filter(|p| {
                if p.user_id == user_id {
                    true
                } else {
                    skipped_plans += 1;
                    false
                }
            })
            .collect();

        // Batch prefetch stored versions for the plans we intend to touch —
        // one SELECT with an IN() list instead of one per plan. Empty input
        // skips the round-trip entirely.
        let mut stored_versions: std::collections::HashMap<String, i64> =
            std::collections::HashMap::with_capacity(owned_plans.len());
        if !owned_plans.is_empty() {
            let placeholders = std::iter::repeat_n("?", owned_plans.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql =
                format!("SELECT plan_id, version FROM plans WHERE plan_id IN ({placeholders})");
            let mut q = sqlx::query(&sql);
            for plan in &owned_plans {
                q = q.bind(&plan.plan_id);
            }
            let rows = q
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| format!("push_plans_pack batch select versions: {e}"))?;
            for row in rows {
                let id: String = row
                    .try_get("plan_id")
                    .map_err(|e| format!("push_plans_pack version row plan_id: {e}"))?;
                let v: i64 = row
                    .try_get("version")
                    .map_err(|e| format!("push_plans_pack version row version: {e}"))?;
                stored_versions.insert(id, v);
            }
        }

        let mut applied_plans = 0usize;
        for plan in &owned_plans {
            // Optimistic concurrency: only accept if the incoming version is
            // strictly newer than the stored version. Ties and regressions are
            // skipped so a stale edge pack can't clobber cloud updates.
            if let Some(&stored) = stored_versions.get(&plan.plan_id)
                && stored >= plan.version
            {
                skipped_plans += 1;
                continue;
            }

            sqlx::query(
                "INSERT INTO plans \
                     (plan_id, user_id, session_id, goal, phase, version, \
                      plan_json, plan_md, progress_pct, subtask_count, created_by, \
                      created_at, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), NOW(6)) \
                 ON DUPLICATE KEY UPDATE \
                     session_id = VALUES(session_id), \
                     goal = VALUES(goal), \
                     phase = VALUES(phase), \
                     version = VALUES(version), \
                     plan_json = VALUES(plan_json), \
                     plan_md = VALUES(plan_md), \
                     progress_pct = VALUES(progress_pct), \
                     subtask_count = VALUES(subtask_count), \
                     updated_at = NOW(6)",
            )
            .bind(&plan.plan_id)
            .bind(&plan.user_id)
            .bind(plan.session_id.as_deref())
            .bind(&plan.goal)
            .bind(&plan.phase)
            .bind(plan.version)
            .bind(&plan.plan_json)
            .bind(plan.plan_md.as_deref())
            .bind(plan.progress_pct)
            .bind(plan.subtask_count)
            .bind(plan.created_by.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("push_plans_pack upsert plan: {e}"))?;
            applied_plans += 1;
        }

        // Batch prefetch owners for every distinct plan_id referenced by the
        // step_runs we might apply. One IN() query instead of N, and owners
        // we just UPSERTed in this tx are already visible through the row
        // cache. Include the plans we just applied so the owner lookup
        // doesn't miss newly-inserted plans.
        let mut run_plan_ids: std::collections::HashSet<&str> =
            pack.step_runs.iter().map(|r| r.plan_id.as_str()).collect();
        // Plans inserted earlier in the tx are visible to this SELECT (same
        // connection, same transaction), so no special-casing needed — but
        // we still read from the DB so step_runs referencing a completely
        // fresh plan_id that only appears in this pack are resolved via the
        // UPSERTed row.
        let mut owners: std::collections::HashMap<String, String> =
            std::collections::HashMap::with_capacity(run_plan_ids.len());
        if !run_plan_ids.is_empty() {
            let placeholders = std::iter::repeat_n("?", run_plan_ids.len())
                .collect::<Vec<_>>()
                .join(",");
            let sql =
                format!("SELECT plan_id, user_id FROM plans WHERE plan_id IN ({placeholders})");
            let mut q = sqlx::query(&sql);
            // Hash iteration order is random; capture the order so we can
            // bind in the same order as the placeholders.
            let ordered: Vec<&str> = run_plan_ids.drain().collect();
            for id in &ordered {
                q = q.bind(*id);
            }
            let rows = q
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| format!("push_plans_pack batch select owners: {e}"))?;
            for row in rows {
                let id: String = row
                    .try_get("plan_id")
                    .map_err(|e| format!("push_plans_pack owner row plan_id: {e}"))?;
                let uid: String = row
                    .try_get("user_id")
                    .map_err(|e| format!("push_plans_pack owner row user_id: {e}"))?;
                owners.insert(id, uid);
            }
        }

        let mut applied_runs = 0usize;
        let mut skipped_runs = 0usize;
        for run in &pack.step_runs {
            // Verify the plan exists and is owned — rejects orphan + cross-user runs.
            match owners.get(&run.plan_id) {
                Some(owner_id) if owner_id == user_id => { /* ok */ }
                _ => {
                    skipped_runs += 1;
                    continue;
                }
            }

            // Bounds-check the untrusted edge-supplied fields. A malformed
            // run is skipped (not an error for the whole pack) so one bad
            // row doesn't discard a batch of otherwise-valid attempts.
            if let Err(reason) = validate_step_run_timestamps(run.started_at, run.finished_at) {
                tracing::warn!(
                    target: "astra_services::state_sync",
                    user_id = %user_id,
                    plan_id = %run.plan_id,
                    run_id = %run.run_id,
                    reason = %reason,
                    "push_plans_pack skipping run with bad timestamps"
                );
                skipped_runs += 1;
                continue;
            }
            if let Err(reason) = validate_step_run_strings(run) {
                tracing::warn!(
                    target: "astra_services::state_sync",
                    user_id = %user_id,
                    plan_id = %run.plan_id,
                    run_id = %run.run_id,
                    reason = %reason,
                    "push_plans_pack skipping run with oversized fields"
                );
                skipped_runs += 1;
                continue;
            }

            // INSERT IGNORE so edge can replay a pack safely without failing
            // on previously-persisted run ids (append-only semantics).
            //
            // started_at / finished_at come from the edge's actual execution
            // timestamps, NOT the sync-time NOW(). Offline executions must
            // preserve their real timeline or the audit chain collapses into
            // the moment of sync.
            let res = sqlx::query(
                "INSERT IGNORE INTO plan_step_runs \
                     (run_id, plan_id, subtask_id, attempt, status, session_id, \
                      started_at, finished_at, request_id, error, artifact_ref) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&run.run_id)
            .bind(&run.plan_id)
            .bind(&run.subtask_id)
            .bind(run.attempt)
            .bind(&run.status)
            .bind(&run.session_id)
            .bind(run.started_at)
            .bind(run.finished_at)
            .bind(&run.request_id)
            .bind(run.error.as_deref())
            .bind(run.artifact_ref.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(|e| format!("push_plans_pack insert run: {e}"))?;
            if res.rows_affected() == 0 {
                skipped_runs += 1;
            } else {
                applied_runs += 1;
            }
        }

        tx.commit()
            .await
            .map_err(|e| format!("push_plans_pack commit: {e}"))?;

        serde_json::to_string(&serde_json::json!({
            "plans_applied": applied_plans,
            "plans_skipped": skipped_plans,
            "step_runs_applied": applied_runs,
            "step_runs_skipped": skipped_runs,
        }))
        .map_err(|e| format!("push_plans_pack result json: {e}"))
    }

    async fn pull_tasks_pack(&self, user_id: &str) -> Result<String, String> {
        crate::multi_agent::pull_tasks_pack_mysql(&self.pool, user_id).await
    }

    async fn push_tasks_pack_held(
        &self,
        user_id: &str,
        holder_agent_id: &str,
        pack_json: &str,
    ) -> Result<crate::multi_agent::TasksPackPushResult, String> {
        crate::multi_agent::push_tasks_pack_held_mysql(
            &self.pool,
            user_id,
            holder_agent_id,
            pack_json,
        )
        .await
    }

    async fn status(&self) -> SyncStatus {
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
            preferences_last_sync: None,
            pending_pushes: pending,
            last_error: last_err,
        }
    }
}

// ─── Preference Constants ───────────────────────────────────────────────────

/// Well-known preference keys.
pub mod pref_keys {
    pub const EXPLAIN_MODE: &str = "explain_mode";
    pub const DEFAULT_MODEL: &str = "default_model";
    pub const CHECKPOINT_INTERVAL: &str = "checkpoint_interval";
    pub const FOCUS_ENTITIES: &str = "focus_entities";
    pub const LANGUAGE: &str = "language";
    /// JSON array of persistently blocked tool names (survives across sessions).
    pub const BLOCKED_TOOLS: &str = "blocked_tools";
    /// Background memory-extraction agent. "true"/"false". Default: true.
    pub const AUTO_MEMORY_ENABLED: &str = "auto_memory_enabled";
    /// Desktop notifications on turn completion. "true"/"false". Default: true.
    pub const NOTIFICATIONS_ENABLED: &str = "notifications_enabled";
    /// Notification delivery method: "auto", "osc9", "bell", or "off". Default: "auto".
    pub const NOTIFICATION_METHOD: &str = "notification_method";
    /// Minimum elapsed seconds before a desktop notification is sent. Default: 10.
    pub const NOTIFICATION_THRESHOLD_SECS: &str = "notification_threshold_secs";
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
    async fn local_only_preferences() {
        let svc = LocalOnlySyncService;
        let push = svc.push_preference("user1", "key", "value").await;
        assert!(push.success);
        let pull = svc.pull_preference("user1", "key").await;
        assert!(pull.unwrap().is_none());
    }

    #[tokio::test]
    async fn local_only_plan_templates_pack_is_empty_array() {
        let svc = LocalOnlySyncService;
        let j = svc.pull_plan_templates_pack("u1").await.unwrap();
        assert_eq!(j, "[]");
    }

    #[test]
    fn plan_template_sync_row_limit_is_bounded() {
        assert_eq!(MAX_PLAN_TEMPLATE_SYNC_ROWS, 500);
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
        assert_eq!(pref_keys::NOTIFICATION_METHOD, "notification_method");
    }

    // ── SyncStatus ──

    #[test]
    fn sync_status_default_is_clean() {
        let status = SyncStatus::default();
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

    #[test]
    fn audit_constants_are_reasonable() {
        assert_eq!(AUDIT_CHANNEL_CAPACITY, 256);
        assert_eq!(AUDIT_FLUSH_BATCH_SIZE, 64);
        assert_eq!(AUDIT_FLUSH_INTERVAL, std::time::Duration::from_secs(1));
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

        assert!(status.preferences_last_sync.is_none());
        assert_eq!(status.pending_pushes, 0);
        assert!(status.last_error.is_none());
    }

    #[test]
    fn sync_status_with_values_roundtrips_through_json() {
        let original = SyncStatus {
            preferences_last_sync: Some("2024-01-03T00:00:00Z".to_string()),
            pending_pushes: 3,
            last_error: Some("connection refused".to_string()),
        };

        let json = serde_json::to_string(&original).unwrap();
        let restored: SyncStatus = serde_json::from_str(&json).unwrap();

        assert_eq!(
            restored.preferences_last_sync,
            original.preferences_last_sync
        );
        assert_eq!(restored.pending_pushes, original.pending_pushes);
        assert_eq!(restored.last_error, original.last_error);
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

    #[test]
    fn preference_sync_row_limit_is_bounded() {
        assert_eq!(MAX_PREFERENCE_SYNC_ROWS, 128);
    }

    #[tokio::test]
    async fn local_only_status_reflects_no_activity() {
        let svc = LocalOnlySyncService;

        let status = svc.status().await;

        assert!(status.last_error.is_none());
        assert_eq!(status.pending_pushes, 0);
    }

    #[test]
    fn sync_result_conflict_has_is_conflict_flag() {
        let result = SyncResult::conflict(SyncDirection::Push, "learning", "version mismatch");

        assert!(!result.success);
        assert!(result.is_conflict);
        assert!(result.message.contains("version"));
    }

    #[test]
    fn sync_result_ok_with_version_includes_version() {
        let result = SyncResult::ok_with_version(SyncDirection::Push, "learning", 1, 5);

        assert!(result.success);
        assert_eq!(result.new_version, Some(5));
        assert!(!result.is_conflict);
    }

    // ── SyncAuditWriter / flusher tests ──

    fn make_entry(i: usize) -> SyncAuditEntry {
        SyncAuditEntry {
            user_id: format!("u{i}"),
            session_id: "s1".into(),
            sync_type: "test".into(),
            direction: SyncDirection::Push,
            payload_size: i,
            status: "success".into(),
            error_message: None,
        }
    }

    fn dummy_pool() -> sqlx::Pool<sqlx::MySql> {
        sqlx::pool::PoolOptions::<sqlx::MySql>::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(1))
            .connect_lazy("mysql://invalid:x@127.0.0.1:1/none")
            .expect("lazy pool")
    }

    #[test]
    fn audit_writer_log_is_nonblocking_when_channel_full() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let writer = SyncAuditWriter { tx };
        writer.log(make_entry(1));
        writer.log(make_entry(2)); // channel full — must not block
    }

    #[test]
    fn audit_writer_log_handles_closed_channel_without_panic() {
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        let writer = SyncAuditWriter { tx };
        drop(rx);
        writer.log(make_entry(1)); // Closed — error! log, no panic
    }

    #[tokio::test]
    async fn flusher_exits_on_channel_close_with_1_entry() {
        let pool = dummy_pool();
        let token = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        tx.send(make_entry(0)).await.unwrap();
        drop(tx);

        let jh = tokio::spawn(run_audit_flusher(rx, pool, token));
        tokio::time::timeout(std::time::Duration::from_secs(5), jh)
            .await
            .expect("flusher should exit within 5s")
            .expect("flusher should not panic");
    }

    #[tokio::test]
    async fn flusher_exits_on_channel_close_with_exact_batch() {
        let pool = dummy_pool();
        let token = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        for i in 0..AUDIT_FLUSH_BATCH_SIZE {
            tx.send(make_entry(i)).await.unwrap();
        }
        drop(tx);

        let jh = tokio::spawn(run_audit_flusher(rx, pool, token));
        tokio::time::timeout(std::time::Duration::from_secs(5), jh)
            .await
            .expect("flusher should exit within 5s")
            .expect("flusher should not panic");
    }

    #[tokio::test]
    async fn flusher_exits_on_channel_close_with_batch_plus_one() {
        let pool = dummy_pool();
        let token = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        for i in 0..AUDIT_FLUSH_BATCH_SIZE + 1 {
            tx.send(make_entry(i)).await.unwrap();
        }
        drop(tx);

        let jh = tokio::spawn(run_audit_flusher(rx, pool, token));
        tokio::time::timeout(std::time::Duration::from_secs(5), jh)
            .await
            .expect("flusher should exit within 5s")
            .expect("flusher should not panic");
    }

    #[tokio::test]
    async fn flusher_exits_on_cancel_with_senders_alive() {
        let pool = dummy_pool();
        let token = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        for i in 0..5 {
            tx.send(make_entry(i)).await.unwrap();
        }

        let jh = tokio::spawn(run_audit_flusher(rx, pool, token.clone()));
        token.cancel();

        tokio::time::timeout(std::time::Duration::from_secs(5), jh)
            .await
            .expect("flusher should exit within 5s after cancel")
            .expect("flusher should not panic");
        // tx is still alive — exit was caused by cancellation, not channel close.
        drop(tx);
    }

    #[tokio::test]
    async fn flusher_flushes_partial_batch_on_timeout() {
        let pool = dummy_pool();
        let token = tokio_util::sync::CancellationToken::new();
        let (tx, rx) = tokio::sync::mpsc::channel(128);
        tx.send(make_entry(0)).await.unwrap();

        let jh = tokio::spawn(run_audit_flusher(rx, pool, token));
        // Wait past the flush interval, then close to let flusher exit.
        tokio::time::sleep(AUDIT_FLUSH_INTERVAL + std::time::Duration::from_millis(100)).await;
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(5), jh)
            .await
            .expect("flusher should exit within 5s")
            .expect("flusher should not panic");
    }

    #[tokio::test(start_paused = true)]
    async fn flusher_shutdown_drain_collects_buffered_entries() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<SyncAuditEntry>(128);
        let token = tokio_util::sync::CancellationToken::new();
        for i in 0..10 {
            tx.send(make_entry(i)).await.unwrap();
        }
        // Read one via recv to prove entries are pending.
        let first = rx.recv().await.unwrap();
        token.cancel();
        // Simulate the flusher's cancellation drain path.
        let mut buf = vec![first];
        while let Ok(e) = rx.try_recv() {
            buf.push(e);
        }
        assert_eq!(buf.len(), 10, "all 10 entries must be collected");
    }

    #[test]
    fn audit_flusher_handle_struct_fields_accessible() {
        fn _assert_fields(h: AuditFlusherHandle) {
            let _w: SyncAuditWriter = h.writer;
            let _s: tokio_util::sync::CancellationToken = h.shutdown;
            let _j: tokio::task::JoinHandle<()> = h.join_handle;
        }
    }
}
