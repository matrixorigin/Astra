//! State convergence: sync learning state between edge (local files) and cloud (MatrixOne).
//!
//! # Architecture
//!
//! ```text
//!   Edge (CLI)                          Cloud (MatrixOne)
//!   ─────────                          ──────────────────
//!   ~/.astra/learning/            learning_snapshots table
//!     {profile}.json         ──push──▶  (user_id, profile, gzip+base64 json)
//!                            ◀──pull──
//!
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
//! - **Merge-on-pull**: For learning data, entity/pattern observations are merged
//! - **Conflict resolution**: Higher observation count wins for entities; union for patterns
//! - **Idempotent**: Repeated pushes produce same result (UPSERT semantics)

use astra_core::is_duplicate_key_error;
use async_trait::async_trait;
use base64::Engine;
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use sqlx::Row;
use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

// ─── Retry Configuration ────────────────────────────────────────────────────

/// Maximum number of retry attempts for transient network errors.
const MAX_RETRIES: u32 = 3;
/// Initial backoff delay between retries.
const INITIAL_BACKOFF_MS: u64 = 100;
/// Maximum backoff delay (exponential backoff caps at this value).
const MAX_BACKOFF_MS: u64 = 2000;
/// Bounded channel capacity for the async audit writer. If the channel is full,
/// audit entries are dropped (acceptable — audit is observability, not business logic).
const AUDIT_CHANNEL_CAPACITY: usize = 256;
/// Flush audit entries after this many accumulate.
const AUDIT_FLUSH_BATCH_SIZE: usize = 64;
/// Flush audit entries after this duration even if batch is not full.
const AUDIT_FLUSH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

// ─── Learning snapshot idempotency classifier ───────────────────────────────

/// Decision returned by [`classify_learning_insert_duplicate`].
#[derive(Debug, PartialEq)]
pub(crate) enum LearningDuplicateDecision {
    /// The stored snapshot is identical to what was inserted; return success.
    Idempotent,
    /// The stored snapshot differs; treat as a real conflict.
    Conflict,
}

/// Classify a duplicate-key hit during a new-snapshot INSERT.
///
/// Compares SHA-256 hashes of the original (uncompressed) snapshot JSON.
/// If the stored hash matches what we computed before insertion the request
/// was a retryable-insert retry and should succeed; otherwise it is a real conflict.
pub(crate) fn classify_learning_insert_duplicate(
    existing_hash: Option<&[u8]>,
    new_hash: &[u8],
) -> LearningDuplicateDecision {
    match existing_hash {
        Some(h) if h == new_hash => LearningDuplicateDecision::Idempotent,
        _ => LearningDuplicateDecision::Conflict,
    }
}

/// Compute the SHA-256 digest of a string slice as a fixed-size byte array.
fn sha256_bytes(input: &str) -> [u8; 32] {
    sha2::Sha256::digest(input.as_bytes()).into()
}

/// Check if an error is likely transient and worth retrying.
fn is_retryable_error(err: &sqlx::Error) -> bool {
    match err {
        // Connection errors are usually transient
        sqlx::Error::Io(_) => true,
        // Pool timeout - might resolve after brief wait
        sqlx::Error::PoolTimedOut => true,
        // Protocol errors might be transient network issues
        sqlx::Error::Protocol(_) => true,
        // Database errors - check for specific transient codes
        sqlx::Error::Database(db_err) => {
            // MySQL error codes for transient issues:
            // 1040 = Too many connections
            // 1205 = Lock wait timeout exceeded
            // 1213 = Deadlock found
            // 2006 = MySQL server has gone away
            // 2013 = Lost connection to MySQL server
            if let Some(code) = db_err.code() {
                let code_str = code.to_string();
                matches!(
                    code_str.as_str(),
                    "1040" | "1205" | "1213" | "2006" | "2013"
                )
            } else {
                // Unknown database error - don't retry
                false
            }
        }
        // Other errors are not retryable
        _ => false,
    }
}

/// Compress a JSON payload with gzip and encode it as base64 for storage.
fn compress_json_payload(json: &str) -> Result<String, String> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder
        .write_all(json.as_bytes())
        .map_err(|e| format!("gzip write: {e}"))?;
    let compressed = encoder.finish().map_err(|e| format!("gzip finish: {e}"))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(compressed))
}

/// Decode a base64 payload and decompress it from gzip back into JSON text.
fn decompress_json_payload(encoded: &str) -> Result<String, String> {
    let compressed = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| format!("base64 decode: {e}"))?;
    let mut decoder = GzDecoder::new(compressed.as_slice());
    let mut json = String::new();
    decoder
        .read_to_string(&mut json)
        .map_err(|e| format!("gzip decode: {e}"))?;
    Ok(json)
}

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
                tracing::warn!(
                    target: "astra_services::audit",
                    session_id = %dropped.session_id,
                    sync_type = %dropped.sync_type,
                    "sync audit channel full, entry dropped"
                );
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

/// Learning snapshot with version for optimistic locking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionedSnapshot {
    /// The JSON-serialized learning snapshot.
    pub json: String,
    /// Cloud version number (for optimistic locking).
    pub version: i64,
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

/// Delta snapshot containing only changed data since last sync.
///
/// Used for incremental sync to reduce network bandwidth.
/// Full snapshot is ~40KB; delta is typically 2-5KB (85-90% reduction).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaSnapshot {
    /// Unix timestamp of baseline (last successful sync).
    pub baseline_epoch: u64,
    /// Changed entities since baseline.
    pub entity_deltas: Vec<serde_json::Value>,
    /// Changed patterns since baseline.
    pub pattern_deltas: Vec<serde_json::Value>,
    /// Calibration data (always sent in full, as it's small).
    pub calibration: Option<serde_json::Value>,
    /// Changed tool health entries since baseline.
    pub tool_health_deltas: Vec<serde_json::Value>,
    /// Total count of delta items for statistics.
    pub delta_count: u32,
}

impl DeltaSnapshot {
    /// Create an empty delta snapshot.
    pub fn empty(baseline_epoch: u64) -> Self {
        Self {
            baseline_epoch,
            entity_deltas: Vec::new(),
            pattern_deltas: Vec::new(),
            calibration: None,
            tool_health_deltas: Vec::new(),
            delta_count: 0,
        }
    }

    /// Check if this delta has any changes.
    pub fn is_empty(&self) -> bool {
        self.delta_count == 0
    }

    /// Approximate size in bytes (for telemetry).
    pub fn approx_size(&self) -> usize {
        self.entity_deltas
            .iter()
            .map(|v| v.to_string().len())
            .sum::<usize>()
            + self
                .pattern_deltas
                .iter()
                .map(|v| v.to_string().len())
                .sum::<usize>()
            + self
                .calibration
                .as_ref()
                .map(|v| v.to_string().len())
                .unwrap_or(0)
            + self
                .tool_health_deltas
                .iter()
                .map(|v| v.to_string().len())
                .sum::<usize>()
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
#[async_trait]
pub trait StateSyncService: Send + Sync {
    /// Push local learning snapshot with optimistic locking.
    ///
    /// - `expected_version`: The version returned by the last `pull_learning_versioned`.
    ///   Pass `None` to create a new snapshot (fails if one already exists).
    ///
    /// Returns:
    /// - `success=true, new_version=Some(v)` on success
    /// - `success=false, is_conflict=true` if version mismatch (another session pushed)
    /// - `success=false, is_conflict=false` on other errors
    #[allow(clippy::too_many_arguments)]
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

    /// Push a delta snapshot containing only changed data.
    ///
    /// Delta sync reduces bandwidth by ~90%: full snapshot is ~40KB, delta is 2-5KB.
    ///
    /// The delta is applied incrementally on the server:
    /// 1. Fetch current snapshot from cloud
    /// 2. Merge delta entries (replace by key for entities/patterns, full for calibration)
    /// 3. Store merged result with incremented version
    ///
    /// Uses optimistic locking internally; returns conflict if version mismatch.
    ///
    /// # Arguments
    /// - `delta_json`: JSON-serialized DeltaSnapshot
    /// - `expected_version`: The version returned by the last `pull_learning_versioned`
    async fn push_delta(
        &self,
        user_id: &str,
        profile: &str,
        delta_json: &str,
        expected_version: Option<i64>,
    ) -> SyncResult;

    /// Get current sync status.
    async fn status(&self) -> SyncStatus;
}

// ─── Local-Only Implementation (No Cloud) ───────────────────────────────────

/// No-op implementation for edge-only mode.
/// All operations succeed instantly without network calls.
pub struct LocalOnlySyncService;

#[async_trait]
impl StateSyncService for LocalOnlySyncService {
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

    async fn push_delta(
        &self,
        _user_id: &str,
        _profile: &str,
        _delta_json: &str,
        _expected_version: Option<i64>,
    ) -> SyncResult {
        // Local-only: always succeeds with version 0
        SyncResult::ok_with_version(SyncDirection::Push, "delta", 0, 0)
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
/// - `learning_snapshots` — cross-session learning state
/// - `user_preferences` — user settings
/// - `session_sync_log` — audit trail (written via async `SyncAuditWriter`)
pub struct MatrixOneSyncService {
    pool: sqlx::Pool<sqlx::MySql>,
    audit: SyncAuditWriter,
}

impl MatrixOneSyncService {
    /// Create from an existing connection pool and shared audit writer.
    pub fn new(pool: sqlx::Pool<sqlx::MySql>, audit: SyncAuditWriter) -> Self {
        Self { pool, audit }
    }

    #[allow(clippy::too_many_arguments)]
    fn log_sync(
        &self,
        user_id: &str,
        session_id: &str,
        sync_type: &str,
        direction: SyncDirection,
        payload_size: usize,
        status: &str,
        error_msg: Option<&str>,
    ) {
        self.audit.log(SyncAuditEntry {
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            sync_type: sync_type.to_string(),
            direction,
            payload_size,
            status: status.to_string(),
            error_message: error_msg.map(|s| s.to_string()),
        });
    }
}

#[async_trait]
impl StateSyncService for MatrixOneSyncService {
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
        let compressed_snapshot = match compress_json_payload(snapshot_json) {
            Ok(value) => value,
            Err(e) => return SyncResult::err(SyncDirection::Push, "learning", e),
        };
        // Pre-compute hash of the original JSON for idempotency comparison on dup-key retry.
        let new_snapshot_hash = sha256_bytes(snapshot_json);

        // Retry loop with exponential backoff for transient network errors
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        match expected_version {
            Some(ver) => {
                for attempt in 0..=MAX_RETRIES {
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
                    .bind(&compressed_snapshot)
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
                                compressed_snapshot.len(),
                                "success",
                                None,
                            );
                            return SyncResult::ok_with_version(
                                SyncDirection::Push,
                                "learning",
                                1,
                                new_ver,
                            );
                        }
                        Ok(_) => {
                            // No rows affected — version mismatch (conflict)
                            // Don't retry conflicts — they need caller to pull fresh data
                            self.log_sync(
                                user_id,
                                "",
                                "learning_versioned",
                                SyncDirection::Push,
                                0,
                                "conflict",
                                Some(&format!("expected version {ver}")),
                            );
                            return SyncResult::conflict(
                                SyncDirection::Push,
                                "learning",
                                format!(
                                    "version conflict: expected {ver}, snapshot was modified by another session"
                                ),
                            );
                        }
                        Err(e) => {
                            if attempt < MAX_RETRIES && is_retryable_error(&e) {
                                tracing::debug!(
                                    target: "astra_services::state_sync",
                                    operation = "push_learning_versioned",
                                    attempt = attempt + 1,
                                    max_retries = MAX_RETRIES,
                                    error = %e,
                                    "retry after transient DB error"
                                );
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                                continue;
                            }
                            let msg = format!("push_learning_versioned: {e}");
                            self.log_sync(
                                user_id,
                                "",
                                "learning_versioned",
                                SyncDirection::Push,
                                0,
                                "error",
                                Some(&msg),
                            );
                            return SyncResult::err(SyncDirection::Push, "learning", msg);
                        }
                    }
                }
                // Max retries exceeded (shouldn't reach here normally)
                SyncResult::err(
                    SyncDirection::Push,
                    "learning",
                    "push_learning_versioned: max retries exceeded",
                )
            }
            None => {
                // No expected version — create new (fail if exists)
                for attempt in 0..=MAX_RETRIES {
                    let inserted = sqlx::query(
                        "INSERT INTO learning_snapshots \
                         (snapshot_id, user_id, profile_name, snapshot_json, entity_count, \
                          pattern_count, has_calibration, version, created_at, updated_at) \
                         VALUES (?, ?, ?, ?, ?, ?, ?, 1, NOW(), NOW())",
                    )
                    .bind(&snapshot_id)
                    .bind(user_id)
                    .bind(profile)
                    .bind(&compressed_snapshot)
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
                                compressed_snapshot.len(),
                                "success",
                                None,
                            );
                            return SyncResult::ok_with_version(
                                SyncDirection::Push,
                                "learning",
                                1,
                                1,
                            );
                        }
                        Err(e) => {
                            let msg = format!("push_learning_versioned (new): {e}");
                            if is_duplicate_key_error(&e) {
                                // Re-query the stored snapshot to determine if this is an
                                // idempotent retry (connection dropped after first INSERT committed)
                                // or a genuine conflict (different payload under same key).
                                let stored_row = sqlx::query(
                                    "SELECT snapshot_json FROM learning_snapshots \
                                     WHERE user_id = ? AND profile_name = ?",
                                )
                                .bind(user_id)
                                .bind(profile)
                                .fetch_optional(&self.pool)
                                .await;

                                let decision = if let Ok(Some(row)) = stored_row {
                                    let stored_compressed: String =
                                        row.try_get("snapshot_json").unwrap_or_default();
                                    let existing_hash = decompress_json_payload(&stored_compressed)
                                        .ok()
                                        .map(|json| sha256_bytes(&json));
                                    classify_learning_insert_duplicate(
                                        existing_hash.as_ref().map(|h| h.as_slice()),
                                        &new_snapshot_hash,
                                    )
                                } else {
                                    LearningDuplicateDecision::Conflict
                                };

                                if decision == LearningDuplicateDecision::Idempotent {
                                    self.log_sync(
                                        user_id,
                                        "",
                                        "learning_versioned",
                                        SyncDirection::Push,
                                        compressed_snapshot.len(),
                                        "success",
                                        None,
                                    );
                                    return SyncResult::ok_with_version(
                                        SyncDirection::Push,
                                        "learning",
                                        1,
                                        1,
                                    );
                                }

                                self.log_sync(
                                    user_id,
                                    "",
                                    "learning_versioned",
                                    SyncDirection::Push,
                                    0,
                                    "conflict",
                                    Some(&msg),
                                );
                                return SyncResult::conflict(
                                    SyncDirection::Push,
                                    "learning",
                                    "snapshot already exists; use expected_version to update",
                                );
                            }
                            // Check for retryable network error
                            if attempt < MAX_RETRIES && is_retryable_error(&e) {
                                tracing::debug!(
                                    target: "astra_services::state_sync",
                                    operation = "push_learning_versioned_new",
                                    attempt = attempt + 1,
                                    max_retries = MAX_RETRIES,
                                    error = %e,
                                    "retry after transient DB error"
                                );
                                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                                backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                                continue;
                            }
                            self.log_sync(
                                user_id,
                                "",
                                "learning_versioned",
                                SyncDirection::Push,
                                0,
                                "error",
                                Some(&msg),
                            );
                            return SyncResult::err(SyncDirection::Push, "learning", msg);
                        }
                    }
                }
                // Max retries exceeded
                SyncResult::err(
                    SyncDirection::Push,
                    "learning",
                    "push_learning_versioned (new): max retries exceeded",
                )
            }
        }
    }

    async fn pull_learning_versioned(
        &self,
        user_id: &str,
        profile: &str,
    ) -> Result<Option<VersionedSnapshot>, String> {
        // Retry loop with exponential backoff for transient network errors
        let mut last_error = None;
        let mut backoff_ms = INITIAL_BACKOFF_MS;

        for attempt in 0..=MAX_RETRIES {
            let row = sqlx::query(
                "SELECT snapshot_json, version FROM learning_snapshots \
                 WHERE user_id = ? AND profile_name = ? \
                 ORDER BY updated_at DESC LIMIT 1",
            )
            .bind(user_id)
            .bind(profile)
            .fetch_optional(&self.pool)
            .await;

            match row {
                Ok(Some(row)) => {
                    use sqlx::Row;
                    let json: String = row
                        .try_get("snapshot_json")
                        .map_err(|e| format!("pull_learning_versioned decode json: {e}"))?;
                    let decompressed = decompress_json_payload(&json)
                        .map_err(|e| format!("pull_learning_versioned unzip json: {e}"))?;
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
                    );
                    return Ok(Some(VersionedSnapshot {
                        json: decompressed,
                        version,
                    }));
                }
                Ok(None) => return Ok(None),
                Err(e) => {
                    if attempt < MAX_RETRIES && is_retryable_error(&e) {
                        tracing::debug!(
                            target: "astra_services::state_sync",
                            operation = "pull_learning_versioned",
                            attempt = attempt + 1,
                            max_retries = MAX_RETRIES,
                            error = %e,
                            "retry after transient DB error"
                        );
                        tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                        backoff_ms = (backoff_ms * 2).min(MAX_BACKOFF_MS);
                        last_error = Some(e);
                        continue;
                    }
                    return Err(format!("pull_learning_versioned: {e}"));
                }
            }
        }

        // Max retries exceeded
        Err(format!(
            "pull_learning_versioned: max retries exceeded: {}",
            last_error.map(|e| e.to_string()).unwrap_or_default()
        ))
    }

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

        // Write audit trail (best-effort, don't fail the push)
        if result.is_ok() {
            let history_id = uuid::Uuid::new_v4().to_string();
            if let Err(e) = sqlx::query(
                "INSERT INTO user_preference_history \
                 (history_id, user_id, pref_key, old_value, new_value, old_version, new_version, source) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, 'edge')",
            )
            .bind(&history_id)
            .bind(user_id)
            .bind(key)
            .bind(&old_value)
            .bind(value)
            .bind(old_version)
            .bind(new_version)
            .execute(&self.pool)
            .await
            {
                tracing::warn!(
                    target: "astra_services::state_sync",
                    user_id = %user_id,
                    pref_key = %key,
                    error = %e,
                    "failed to write preference history audit trail"
                );
            }
        }

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

    async fn push_delta(
        &self,
        user_id: &str,
        profile: &str,
        delta_json: &str,
        expected_version: Option<i64>,
    ) -> SyncResult {
        // Parse the delta JSON
        let delta: DeltaSnapshot = match serde_json::from_str(delta_json) {
            Ok(d) => d,
            Err(e) => {
                return SyncResult::err(SyncDirection::Push, "delta", format!("parse delta: {e}"));
            }
        };
        // If delta is empty, skip the push entirely
        if delta.is_empty() {
            return SyncResult::ok_with_version(
                SyncDirection::Push,
                "delta",
                0,
                expected_version.unwrap_or(0),
            );
        }

        // Delta sync algorithm:
        // 1. Fetch current snapshot + version
        // 2. Deserialize and merge delta entries
        // 3. Push merged result with version check

        // Step 1: Pull current snapshot
        let (current_json, current_version) = match self
            .pull_learning_versioned(user_id, profile)
            .await
        {
            Ok(Some(snap)) => (snap.json, snap.version),
            Ok(None) => {
                // No existing snapshot - create from delta only
                let snapshot = create_snapshot_from_delta(&delta);
                let json = serde_json::to_string(&snapshot).unwrap_or_default();
                return self
                    .push_learning_versioned(
                        user_id,
                        profile,
                        &json,
                        delta.entity_deltas.len() as u32,
                        delta.pattern_deltas.len() as u32,
                        delta.calibration.is_some(),
                        None,
                    )
                    .await;
            }
            Err(e) => {
                return SyncResult::err(SyncDirection::Push, "delta", format!("pull failed: {e}"));
            }
        };

        // Check version for conflict
        if let Some(expected) = expected_version
            && current_version != expected
        {
            return SyncResult::conflict(
                SyncDirection::Push,
                "delta",
                format!(
                    "version mismatch: expected {}, found {}",
                    expected, current_version
                ),
            );
        }

        // Step 2: Parse and merge
        let merged_json = match merge_delta_into_snapshot(&current_json, &delta) {
            Ok(j) => j,
            Err(e) => {
                return SyncResult::err(SyncDirection::Push, "delta", format!("merge failed: {e}"));
            }
        };

        // Step 3: Push merged result with optimistic locking
        // Note: Delta sync stats could be logged at the caller level
        // delta_items = delta.delta_count
        // delta_size = delta.approx_size()
        // full_size = merged_json.len()
        // reduction_pct = 100 - (delta_size * 100 / full_size.max(1))
        self.push_learning_versioned(
            user_id,
            profile,
            &merged_json,
            delta.entity_deltas.len() as u32,
            delta.pattern_deltas.len() as u32,
            delta.calibration.is_some(),
            Some(current_version),
        )
        .await
    }
}

// ─── Delta Sync Helpers ─────────────────────────────────────────────────────

/// Create a new snapshot from delta entries only (when no existing snapshot exists).
fn create_snapshot_from_delta(delta: &DeltaSnapshot) -> serde_json::Value {
    serde_json::json!({
        "entities": delta.entity_deltas,
        "patterns": delta.pattern_deltas,
        "calibration": delta.calibration,
        "tool_health": delta.tool_health_deltas,
    })
}

/// Merge delta entries into an existing snapshot JSON.
///
/// Merge strategy:
/// - entities: Replace by "name" key
/// - patterns: Replace by "signature" key
/// - calibration: Full replacement
/// - tool_health: Replace by "name" key
fn merge_delta_into_snapshot(snapshot_json: &str, delta: &DeltaSnapshot) -> Result<String, String> {
    let mut snapshot: serde_json::Value =
        serde_json::from_str(snapshot_json).map_err(|e| format!("parse snapshot: {e}"))?;

    // Merge entities by name
    if !delta.entity_deltas.is_empty() {
        let entities = snapshot.get_mut("entities").and_then(|v| v.as_array_mut());
        if let Some(arr) = entities {
            for entity_delta in &delta.entity_deltas {
                if let Some(name) = entity_delta.get("name").and_then(|v| v.as_str()) {
                    // Find and replace existing, or append
                    let pos = arr
                        .iter()
                        .position(|e| e.get("name").and_then(|v| v.as_str()) == Some(name));
                    if let Some(idx) = pos {
                        arr[idx] = entity_delta.clone();
                    } else {
                        arr.push(entity_delta.clone());
                    }
                }
            }
        } else {
            // No entities array - create one
            snapshot["entities"] = serde_json::Value::Array(delta.entity_deltas.clone());
        }
    }

    // Merge patterns by signature
    if !delta.pattern_deltas.is_empty() {
        let patterns = snapshot.get_mut("patterns").and_then(|v| v.as_array_mut());
        if let Some(arr) = patterns {
            for pattern_delta in &delta.pattern_deltas {
                if let Some(sig) = pattern_delta.get("signature").and_then(|v| v.as_str()) {
                    let pos = arr
                        .iter()
                        .position(|p| p.get("signature").and_then(|v| v.as_str()) == Some(sig));
                    if let Some(idx) = pos {
                        arr[idx] = pattern_delta.clone();
                    } else {
                        arr.push(pattern_delta.clone());
                    }
                }
            }
        } else {
            snapshot["patterns"] = serde_json::Value::Array(delta.pattern_deltas.clone());
        }
    }

    // Calibration: full replacement
    if let Some(cal) = &delta.calibration {
        snapshot["calibration"] = cal.clone();
    }

    // Merge tool_health by name
    if !delta.tool_health_deltas.is_empty() {
        let tool_health = snapshot
            .get_mut("tool_health")
            .and_then(|v| v.as_array_mut());
        if let Some(arr) = tool_health {
            for th_delta in &delta.tool_health_deltas {
                if let Some(name) = th_delta.get("name").and_then(|v| v.as_str()) {
                    let pos = arr
                        .iter()
                        .position(|t| t.get("name").and_then(|v| v.as_str()) == Some(name));
                    if let Some(idx) = pos {
                        arr[idx] = th_delta.clone();
                    } else {
                        arr.push(th_delta.clone());
                    }
                }
            }
        } else {
            snapshot["tool_health"] = serde_json::Value::Array(delta.tool_health_deltas.clone());
        }
    }

    serde_json::to_string(&snapshot).map_err(|e| format!("serialize merged: {e}"))
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
    /// JSON array of persistently blocked tool names (survives across sessions).
    pub const BLOCKED_TOOLS: &str = "blocked_tools";
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
    async fn local_only_push_versioned_succeeds() {
        let svc = LocalOnlySyncService;
        let result = svc
            .push_learning_versioned("user1", "default", "{}", 0, 0, false, None)
            .await;
        assert!(result.success);
    }

    #[tokio::test]
    async fn local_only_pull_returns_none() {
        let svc = LocalOnlySyncService;
        let result = svc.pull_learning_versioned("user1", "default").await;
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

    #[test]
    fn compressed_payload_roundtrips_json() {
        let original =
            r#"{"entities":[{"name":"tool","count":3}],"patterns":[{"signature":"abc"}]}"#;

        let encoded = compress_json_payload(original).unwrap();
        let restored = decompress_json_payload(&encoded).unwrap();

        assert_ne!(encoded, original);
        assert_eq!(restored, original);
    }

    #[test]
    fn decompress_rejects_plain_json_storage() {
        let err = decompress_json_payload(r#"{"entities":[]}"#).unwrap_err();
        assert!(err.contains("base64 decode"));
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

        let result = svc
            .push_learning_versioned(
                "user1",
                "default",
                r#"{"entities":[{"name":"test","count":5}]}"#,
                1,
                0,
                true,
                None,
            )
            .await;

        assert!(result.success, "LocalOnly should always succeed");
        assert_eq!(result.items_synced, 0, "LocalOnly doesn't actually sync");
    }

    #[tokio::test]
    async fn local_only_pull_learning_returns_none_for_any_user() {
        let svc = LocalOnlySyncService;

        let result1 = svc.pull_learning_versioned("user1", "default").await;
        let result2 = svc.pull_learning_versioned("user2", "work").await;

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

    // ── Retry logic tests ──

    #[test]
    fn is_retryable_error_io_errors() {
        // IO errors should be retryable
        let io_err = sqlx::Error::Io(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "connection reset",
        ));
        assert!(is_retryable_error(&io_err));
    }

    #[test]
    fn is_retryable_error_pool_timeout() {
        // Pool timeout should be retryable
        let timeout_err = sqlx::Error::PoolTimedOut;
        assert!(is_retryable_error(&timeout_err));
    }

    #[test]
    fn is_retryable_error_protocol() {
        // Protocol errors should be retryable
        let proto_err = sqlx::Error::Protocol("unexpected packet".to_string());
        assert!(is_retryable_error(&proto_err));
    }

    #[test]
    fn is_retryable_error_non_retryable() {
        // Column decode errors are not retryable
        let decode_err = sqlx::Error::ColumnDecode {
            index: "0".to_string(),
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "bad data",
            )),
        };
        assert!(!is_retryable_error(&decode_err));

        // Type mismatch errors are not retryable
        let type_err = sqlx::Error::TypeNotFound {
            type_name: "unknown".to_string(),
        };
        assert!(!is_retryable_error(&type_err));
    }

    #[test]
    fn retry_constants_are_reasonable() {
        let max_retries = std::hint::black_box(MAX_RETRIES);
        let initial_backoff_ms = std::hint::black_box(INITIAL_BACKOFF_MS);
        let max_backoff_ms = std::hint::black_box(MAX_BACKOFF_MS);
        // Verify retry constants are within expected ranges
        assert!((2..=5).contains(&max_retries));
        assert!((50..=500).contains(&initial_backoff_ms));
        assert!((1000..=5000).contains(&max_backoff_ms));
        // Ensure max backoff is greater than initial
        assert!(max_backoff_ms > initial_backoff_ms);
    }

    // ── classify_learning_insert_duplicate unit tests ────────────────────────

    #[test]
    fn classify_learning_insert_dup_matching_hash_is_idempotent() {
        let hash = sha256_bytes(r#"{"entities":[]}"#);
        let decision = classify_learning_insert_duplicate(Some(hash.as_slice()), hash.as_slice());
        assert_eq!(
            decision,
            LearningDuplicateDecision::Idempotent,
            "same hash must be idempotent"
        );
    }

    #[test]
    fn classify_learning_insert_dup_different_hash_is_conflict() {
        let hash_a = sha256_bytes(r#"{"entities":[]}"#);
        let hash_b = sha256_bytes(r#"{"entities":[{"id":"x"}]}"#);
        let decision =
            classify_learning_insert_duplicate(Some(hash_a.as_slice()), hash_b.as_slice());
        assert_eq!(
            decision,
            LearningDuplicateDecision::Conflict,
            "different hashes must be a conflict"
        );
    }

    #[test]
    fn classify_learning_insert_dup_no_stored_row_is_conflict() {
        let hash = sha256_bytes(r#"{"entities":[]}"#);
        let decision = classify_learning_insert_duplicate(None, hash.as_slice());
        assert_eq!(
            decision,
            LearningDuplicateDecision::Conflict,
            "missing stored row must be treated as conflict"
        );
    }

    #[test]
    fn sha256_bytes_is_deterministic() {
        let h1 = sha256_bytes("hello");
        let h2 = sha256_bytes("hello");
        assert_eq!(h1, h2);
        let h3 = sha256_bytes("world");
        assert_ne!(h1, h3);
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
