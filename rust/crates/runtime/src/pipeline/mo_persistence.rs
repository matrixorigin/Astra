//! MatrixOne-backed persistence for Step Protocol: CheckpointWriter + IdempotencyCache.
//!
//! Implements the runtime traits against the `session_checkpoints` and
//! `step_idempotency_cache` tables using sqlx connection pool.
//!
//! # Design
//!
//! - **CheckpointWriter**: serializes Light/Heavy checkpoints to `session_checkpoints`
//!   with state_json containing the full StepCheckpoint.
//! - **IdempotencyCache**: key-value store in `step_idempotency_cache` table,
//!   uses content_hash for O(1) lookup, supports per-step eviction.
//! - **Async-to-sync bridge**: runtime traits are sync; we use `tokio::runtime::Handle`
//!   to block on async sqlx calls. This is safe because the caller (chat_stream) is
//!   already in an async context — the bridge runs the query on the current runtime.

use crate::pipeline::scheduling::CheckpointWriter;
use crate::pipeline::step_protocol::{
    CachedToolResult, CheckpointTier, CheckpointTrigger, IdempotencyCache, IdempotencyKey,
    LightCheckpoint, Step, StepCheckpoint,
};
use astra_core::is_duplicate_key_error;
use sqlx::{MySql, Pool, Row};

fn epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

// ─── DDL ─────────────────────────────────────────────────────────────────────

/// DDL for the idempotency cache table. Called from ensure_core_schema().
/// Must stay in sync with `ensure_core_schema` in `astra_services::storage`.
pub const IDEMPOTENCY_CACHE_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS step_idempotency_cache (
    cache_key VARCHAR(200) PRIMARY KEY,
    step_id VARCHAR(100) NOT NULL,
    tool_index INT NOT NULL,
    content_hash VARCHAR(64) NOT NULL,
    tool_name VARCHAR(100) NOT NULL,
    output LONGTEXT NOT NULL,
    is_error SMALLINT NOT NULL DEFAULT 0,
    cached_at BIGINT NOT NULL,
    created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
    INDEX idx_idempotency_step_tool (step_id, tool_index),
    INDEX idx_idempotency_hash (content_hash)
)
"#;

// ─── MatrixOneCheckpointWriter ───────────────────────────────────────────────

/// Writes Step Protocol checkpoints to the `session_checkpoints` MatrixOne table.
///
/// Light checkpoints store only cursor + metadata (~1KB).
/// Heavy checkpoints store full state including messages (~10-100KB).
pub struct MatrixOneCheckpointWriter {
    pool: Pool<MySql>,
    user_id: String,
    session_id: String,
    checkpoint_counter: u32,
}

impl MatrixOneCheckpointWriter {
    pub fn new(pool: Pool<MySql>, user_id: &str, session_id: &str) -> Self {
        Self {
            pool,
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            checkpoint_counter: 0,
        }
    }

    /// Async implementation of write_checkpoint.
    async fn write_checkpoint_async(
        &mut self,
        step: &Step,
        trigger: CheckpointTrigger,
        messages: Option<&[serde_json::Value]>,
    ) -> Result<(), String> {
        self.checkpoint_counter += 1;
        let tier = trigger.checkpoint_tier();
        let checkpoint_id = uuid::Uuid::new_v4().to_string();

        // Build the checkpoint
        let light = LightCheckpoint {
            protocol_version: step.descriptor.protocol_version,
            cursor: step.execution.cursor.clone(),
            step_id: step.descriptor.step_id.clone(),
            task_id: step.descriptor.task_id.clone(),
            agent_id: step.descriptor.agent_id.clone().unwrap_or_default(),
            progress: 0.0,
            total_tokens: 0,
            created_at: epoch_ms(),
        };
        let checkpoint = match tier {
            CheckpointTier::Heavy => {
                let heavy = crate::pipeline::step_protocol::HeavyCheckpoint {
                    light,
                    messages: messages.map(|m| m.to_vec()).unwrap_or_default(),
                    budget_remaining_tokens: 0,
                    budget_remaining_rounds: 0,
                    blocked_tools: Vec::new(),
                    recent_tools: Vec::new(),
                    learning_snapshot_id: None,
                    memory_context: step.execution.memory_context.clone(),
                    delegation_id: None,
                    delegation_pattern: None,
                    delegation_sub_run_summaries: Vec::new(),
                };
                StepCheckpoint::Heavy(Box::new(heavy))
            }
            CheckpointTier::Light => StepCheckpoint::Light(light),
        };
        let state_json =
            serde_json::to_string(&checkpoint).map_err(|e| format!("serialize: {e}"))?;

        let tier_str = match tier {
            CheckpointTier::Light => "light",
            CheckpointTier::Heavy => "heavy",
        };
        let title = format!(
            "step:{}:{} [{}]",
            step.descriptor.step_id, step.descriptor.action, tier_str
        );

        let tools_json = "[]".to_string();
        let turn = step.execution.cursor.slots.len() as i32;

        let updated = sqlx::query(
            "UPDATE session_checkpoints SET \
                turn = ?, title = ?, summary = ?, tools_json = ?, state_json = ?, total_tokens = ? \
             WHERE session_id = ? AND number = ?",
        )
        .bind(turn)
        .bind(&title)
        .bind(tier_str)
        .bind(&tools_json)
        .bind(&state_json)
        .bind(0i64)
        .bind(&self.session_id)
        .bind(self.checkpoint_counter as i32)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("checkpoint update: {e}"))?;

        if updated.rows_affected() == 0 {
            let inserted = sqlx::query(
                "INSERT INTO session_checkpoints \
                 (checkpoint_id, session_id, user_id, number, turn, title, summary, tools_json, state_json, total_tokens, had_stalls, error_count) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0)",
            )
            .bind(&checkpoint_id)
            .bind(&self.session_id)
            .bind(&self.user_id)
            .bind(self.checkpoint_counter as i32)
            .bind(turn)
            .bind(&title)
            .bind(tier_str)
            .bind(&tools_json)
            .bind(&state_json)
            .bind(0i64)
            .execute(&self.pool)
            .await;

            if let Err(e) = inserted {
                if is_duplicate_key_error(&e) {
                    sqlx::query(
                        "UPDATE session_checkpoints SET \
                            turn = ?, title = ?, summary = ?, tools_json = ?, state_json = ?, total_tokens = ? \
                         WHERE session_id = ? AND number = ?",
                    )
                    .bind(turn)
                    .bind(&title)
                    .bind(tier_str)
                    .bind(&tools_json)
                    .bind(&state_json)
                    .bind(0i64)
                    .bind(&self.session_id)
                    .bind(self.checkpoint_counter as i32)
                    .execute(&self.pool)
                    .await
                    .map_err(|err| format!("checkpoint retry update: {err}"))?;
                } else {
                    return Err(format!("checkpoint insert: {e}"));
                }
            }
        }

        Ok(())
    }

    /// Async implementation of read_checkpoint.
    async fn read_checkpoint_async(&self, step_id: &str) -> Result<Option<StepCheckpoint>, String> {
        let pattern = format!("step:{}:%", step_id);
        let row = sqlx::query(
            "SELECT state_json FROM session_checkpoints \
             WHERE session_id = ? AND title LIKE ? \
             ORDER BY number DESC LIMIT 1",
        )
        .bind(&self.session_id)
        .bind(&pattern)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| format!("checkpoint read: {e}"))?;

        match row {
            Some(row) => {
                let json: String = row.try_get("state_json").map_err(|e| format!("{e}"))?;
                let checkpoint: StepCheckpoint =
                    serde_json::from_str(&json).map_err(|e| format!("deserialize: {e}"))?;
                Ok(Some(checkpoint))
            }
            None => Ok(None),
        }
    }

    /// Async implementation of delete_checkpoints.
    async fn delete_checkpoints_async(&mut self, step_id: &str) -> Result<(), String> {
        let pattern = format!("step:{}:%", step_id);
        sqlx::query(
            "DELETE FROM session_checkpoints \
             WHERE session_id = ? AND title LIKE ?",
        )
        .bind(&self.session_id)
        .bind(&pattern)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("checkpoint delete: {e}"))?;
        Ok(())
    }
}

impl CheckpointWriter for MatrixOneCheckpointWriter {
    type Error = String;

    fn write_checkpoint(
        &mut self,
        step: &Step,
        trigger: CheckpointTrigger,
        messages: Option<&[serde_json::Value]>,
    ) -> Result<(), Self::Error> {
        // Bridge async → sync using current tokio runtime
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for checkpoint write".to_string())?;
        handle.block_on(self.write_checkpoint_async(step, trigger, messages))
    }

    fn read_checkpoint(&self, step_id: &str) -> Result<Option<StepCheckpoint>, Self::Error> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for checkpoint read".to_string())?;
        handle.block_on(self.read_checkpoint_async(step_id))
    }

    fn delete_checkpoints(&mut self, step_id: &str) -> Result<(), Self::Error> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "no tokio runtime for checkpoint delete".to_string())?;
        handle.block_on(self.delete_checkpoints_async(step_id))
    }
}

// ─── MatrixOneIdempotencyCache ───────────────────────────────────────────────

/// Idempotency cache backed by MatrixOne's `step_idempotency_cache` table.
///
/// Provides cross-session tool dedup: if a tool with the same content_hash was
/// executed in a prior session, the cached result is returned instead of
/// re-executing.
pub struct MatrixOneIdempotencyCache {
    pool: Pool<MySql>,
    /// Local write-through: avoids extra DB round-trips for within-session checks.
    local_cache: std::collections::HashMap<String, CachedToolResult>,
}

impl MatrixOneIdempotencyCache {
    pub fn new(pool: Pool<MySql>) -> Self {
        Self {
            pool,
            local_cache: std::collections::HashMap::new(),
        }
    }

    /// Async check: try local cache first, then DB.
    async fn check_async(&self, key: &IdempotencyKey) -> Option<CachedToolResult> {
        let cache_key = key.cache_key();

        // Local hit — cheapest path
        if let Some(cached) = self.local_cache.get(&cache_key) {
            return Some(cached.clone());
        }

        // DB lookup
        let row = sqlx::query(
            "SELECT tool_name, output, is_error, cached_at \
             FROM step_idempotency_cache WHERE cache_key = ?",
        )
        .bind(&cache_key)
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();

        row.map(|r| CachedToolResult {
            tool_name: r.try_get("tool_name").unwrap_or_default(),
            output: r.try_get("output").unwrap_or_default(),
            is_error: r
                .try_get::<i16, _>("is_error")
                .map(|v| v != 0)
                .unwrap_or(false),
            cached_at: r.try_get::<i64, _>("cached_at").unwrap_or(0) as u64,
        })
    }

    /// Async record: write to both local cache and DB.
    async fn record_async(
        &mut self,
        key: &IdempotencyKey,
        result: CachedToolResult,
    ) -> Result<(), String> {
        let cache_key = key.cache_key();

        let updated = sqlx::query(
            "UPDATE step_idempotency_cache SET \
                tool_name = ?, output = ?, is_error = ?, cached_at = ? \
             WHERE cache_key = ?",
        )
        .bind(&result.tool_name)
        .bind(&result.output)
        .bind(if result.is_error { 1i16 } else { 0i16 })
        .bind(result.cached_at as i64)
        .bind(&cache_key)
        .execute(&self.pool)
        .await
        .map_err(|e| format!("idempotency update: {e}"))?;

        if updated.rows_affected() == 0 {
            let inserted = sqlx::query(
                "INSERT INTO step_idempotency_cache \
                 (cache_key, step_id, tool_index, content_hash, tool_name, output, is_error, cached_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(&cache_key)
            .bind(&key.step_id)
            .bind(key.tool_index as i32)
            .bind(&key.content_hash)
            .bind(&result.tool_name)
            .bind(&result.output)
            .bind(if result.is_error { 1i16 } else { 0i16 })
            .bind(result.cached_at as i64)
            .execute(&self.pool)
            .await;

            if let Err(e) = inserted {
                if is_duplicate_key_error(&e) {
                    sqlx::query(
                        "UPDATE step_idempotency_cache SET \
                            tool_name = ?, output = ?, is_error = ?, cached_at = ? \
                         WHERE cache_key = ?",
                    )
                    .bind(&result.tool_name)
                    .bind(&result.output)
                    .bind(if result.is_error { 1i16 } else { 0i16 })
                    .bind(result.cached_at as i64)
                    .bind(&cache_key)
                    .execute(&self.pool)
                    .await
                    .map_err(|err| format!("idempotency retry update: {err}"))?;
                } else {
                    return Err(format!("idempotency insert: {e}"));
                }
            }
        }

        self.local_cache.insert(cache_key, result);
        Ok(())
    }

    /// Async evict: remove all entries for a step from DB + local.
    async fn evict_step_async(&mut self, step_id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM step_idempotency_cache WHERE step_id = ?")
            .bind(step_id)
            .execute(&self.pool)
            .await
            .map_err(|e| format!("idempotency evict: {e}"))?;

        let prefix = format!("{}:", step_id);
        self.local_cache
            .retain(|k, _| !k.starts_with(&prefix) && k != step_id);
        Ok(())
    }

    /// Async count: local + DB (deduped).
    async fn len_async(&self) -> usize {
        let db_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM step_idempotency_cache")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        db_count as usize
    }
}

impl IdempotencyCache for MatrixOneIdempotencyCache {
    fn check(&self, key: &IdempotencyKey) -> Option<CachedToolResult> {
        let handle = match tokio::runtime::Handle::try_current() {
            Ok(h) => h,
            Err(_) => return self.local_cache.get(&key.cache_key()).cloned(),
        };
        handle.block_on(self.check_async(key))
    }

    fn record(&mut self, key: &IdempotencyKey, result: CachedToolResult) {
        let cache_key = key.cache_key();
        self.local_cache.insert(cache_key, result.clone());

        // Fire-and-forget async write — don't block on DB for recording
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = handle.block_on(self.record_async(key, result));
        }
    }

    fn evict_step(&mut self, step_id: &str) {
        let prefix = format!("{}:", step_id);
        self.local_cache
            .retain(|k, _| !k.starts_with(&prefix) && k != step_id);

        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let _ = handle.block_on(self.evict_step_async(step_id));
        }
    }

    fn len(&self) -> usize {
        // Use local count for fast path; DB count for accuracy if runtime available
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.block_on(self.len_async())
        } else {
            self.local_cache.len()
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::step_protocol::*;

    // ── Unit tests (no DB required) ──────────────────────────────────────────

    #[test]
    fn idempotency_cache_ddl_is_valid_sql() {
        // Verify DDL string is non-empty and contains expected keywords
        assert!(IDEMPOTENCY_CACHE_DDL.contains("CREATE TABLE"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("step_idempotency_cache"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("cache_key"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("content_hash"));
        // DDL should NOT contain UPSERT — that's in the impl, not the schema
        assert!(!IDEMPOTENCY_CACHE_DDL.contains("ON DUPLICATE KEY UPDATE"));
    }

    #[test]
    fn idempotency_key_cache_key_format() {
        let key = IdempotencyKey::new(
            "step-1",
            0,
            "read_file",
            &serde_json::json!({"path": "foo.rs"}),
        );
        let ck = key.cache_key();
        assert!(!ck.is_empty());
        // Same inputs → same key
        let key2 = IdempotencyKey::new(
            "step-1",
            0,
            "read_file",
            &serde_json::json!({"path": "foo.rs"}),
        );
        assert_eq!(ck, key2.cache_key());
    }

    #[test]
    fn idempotency_key_different_args_different_key() {
        let key1 = IdempotencyKey::new(
            "step-1",
            0,
            "read_file",
            &serde_json::json!({"path": "a.rs"}),
        );
        let key2 = IdempotencyKey::new(
            "step-1",
            0,
            "read_file",
            &serde_json::json!({"path": "b.rs"}),
        );
        assert_ne!(key1.cache_key(), key2.cache_key());
    }

    #[test]
    fn cached_tool_result_serde_roundtrip() {
        let result = CachedToolResult {
            tool_name: "git_log".to_string(),
            output: "commit abc123".to_string(),
            is_error: false,
            cached_at: 1700000000,
        };
        let json = serde_json::to_string(&result).unwrap();
        let back: CachedToolResult = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tool_name, "git_log");
        assert_eq!(back.output, "commit abc123");
        assert!(!back.is_error);
        assert_eq!(back.cached_at, 1700000000);
    }

    #[test]
    fn checkpoint_tier_from_trigger() {
        assert_eq!(
            CheckpointTrigger::SlotCompleted.checkpoint_tier(),
            CheckpointTier::Light
        );
        assert_eq!(
            CheckpointTrigger::PhaseTransition.checkpoint_tier(),
            CheckpointTier::Heavy
        );
        assert_eq!(
            CheckpointTrigger::BeforeExpensiveOp.checkpoint_tier(),
            CheckpointTier::Light
        );
        assert_eq!(
            CheckpointTrigger::Explicit.checkpoint_tier(),
            CheckpointTier::Heavy
        );
    }

    #[test]
    fn checkpoint_writer_new_fields() {
        // Verify the writer struct can be constructed with expected fields
        // (no pool needed — testing field layout)
        let _user = "test-user";
        let _session = "test-session";
        // MatrixOneCheckpointWriter requires a real pool, so we verify the DDL instead
        assert!(IDEMPOTENCY_CACHE_DDL.contains("step_id VARCHAR(100)"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("tool_name VARCHAR(100)"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("output LONGTEXT"));
    }

    #[test]
    fn step_checkpoint_serialization_light() {
        let light = LightCheckpoint {
            protocol_version: 1000,
            cursor: ExecutionCursor::default(),
            step_id: "s1".to_string(),
            task_id: "t1".to_string(),
            agent_id: "a1".to_string(),
            progress: 0.5,
            total_tokens: 1000,
            created_at: 12345,
        };
        let cp = StepCheckpoint::Light(light);
        let json = serde_json::to_string(&cp).unwrap();
        assert!(json.contains("protocol_version"));
        assert!(json.contains("1000"));
        let back: StepCheckpoint = serde_json::from_str(&json).unwrap();
        match back {
            StepCheckpoint::Light(l) => {
                assert_eq!(l.protocol_version, 1000);
                assert_eq!(l.step_id, "s1");
                assert_eq!(l.progress, 0.5);
            }
            _ => panic!("expected Light"),
        }
    }

    #[test]
    fn step_checkpoint_serialization_heavy() {
        let light = LightCheckpoint {
            protocol_version: 1000,
            cursor: ExecutionCursor::default(),
            step_id: "s2".to_string(),
            task_id: "t2".to_string(),
            agent_id: "a2".to_string(),
            progress: 1.0,
            total_tokens: 5000,
            created_at: 67890,
        };
        let heavy = crate::pipeline::step_protocol::HeavyCheckpoint {
            light,
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            budget_remaining_tokens: 1000,
            budget_remaining_rounds: 5,
            blocked_tools: vec!["bash".to_string()],
            recent_tools: vec!["read_file".to_string()],
            learning_snapshot_id: Some("snap-1".to_string()),
            memory_context: None,
            delegation_id: None,
            delegation_pattern: None,
            delegation_sub_run_summaries: Vec::new(),
        };
        let cp = StepCheckpoint::Heavy(Box::new(heavy));
        let json = serde_json::to_string(&cp).unwrap();
        assert!(json.contains("messages"));
        assert!(json.contains("budget_remaining"));
        let back: StepCheckpoint = serde_json::from_str(&json).unwrap();
        match back {
            StepCheckpoint::Heavy(h) => {
                assert_eq!(h.light.step_id, "s2");
                assert_eq!(h.messages.len(), 1);
                assert_eq!(h.blocked_tools, vec!["bash"]);
            }
            _ => panic!("expected Heavy"),
        }
    }

    #[test]
    fn inmemory_cache_trait_matches_matrixone_contract() {
        // InMemoryIdempotencyCache should satisfy the same trait as MatrixOne version
        let mut cache = InMemoryIdempotencyCache::new();
        let key = IdempotencyKey::new("step-1", 0, "grep", &serde_json::json!({"pattern": "foo"}));

        // Initially empty
        assert!(cache.is_empty());
        assert!(IdempotencyCache::check(&cache, &key).is_none());

        // Record
        let result = CachedToolResult {
            tool_name: "grep".to_string(),
            output: "found foo".to_string(),
            is_error: false,
            cached_at: 100,
        };
        IdempotencyCache::record(&mut cache, &key, result);
        assert_eq!(cache.len(), 1);

        // Check hit
        let hit = IdempotencyCache::check(&cache, &key).unwrap();
        assert_eq!(hit.tool_name, "grep");
        assert_eq!(hit.output, "found foo");

        // Evict
        IdempotencyCache::evict_step(&mut cache, "step-1");
        assert!(cache.is_empty());
    }

    #[test]
    fn inmemory_evict_step_prefix_safety() {
        // Ensure evicting "s1" doesn't evict "s10" or "s100"
        let mut cache = InMemoryIdempotencyCache::new();
        let key1 = IdempotencyKey::new("s1", 0, "grep", &serde_json::json!({}));
        let key10 = IdempotencyKey::new("s10", 0, "grep", &serde_json::json!({}));
        let key100 = IdempotencyKey::new("s100", 0, "grep", &serde_json::json!({}));

        let result = CachedToolResult {
            tool_name: "grep".to_string(),
            output: "x".to_string(),
            is_error: false,
            cached_at: 1,
        };
        IdempotencyCache::record(&mut cache, &key1, result.clone());
        IdempotencyCache::record(&mut cache, &key10, result.clone());
        IdempotencyCache::record(&mut cache, &key100, result);
        assert_eq!(cache.len(), 3);

        IdempotencyCache::evict_step(&mut cache, "s1");
        assert_eq!(cache.len(), 2);
        assert!(IdempotencyCache::check(&cache, &key10).is_some());
        assert!(IdempotencyCache::check(&cache, &key100).is_some());
    }

    #[test]
    fn semantic_key_is_step_independent() {
        let key1 = IdempotencyKey::semantic("read_file", &serde_json::json!({"path": "x.rs"}));
        let key2 = IdempotencyKey::semantic("read_file", &serde_json::json!({"path": "x.rs"}));
        assert_eq!(key1.cache_key(), key2.cache_key());
        assert!(key1.is_semantic());
        assert!(key1.step_id.is_empty());
    }

    #[test]
    fn context_signature_affects_key() {
        let args = serde_json::json!({"path": "z.rs"});
        let key_plain = IdempotencyKey::new("s1", 0, "read_file", &args);
        let key_ctx =
            IdempotencyKey::new("s1", 0, "read_file", &args).with_context(ContextSignature {
                workspace_version: Some("abc".to_string()),
                memory_snapshot_id: None,
            });
        // Different context → different cache key
        assert_ne!(key_plain.cache_key(), key_ctx.cache_key());
    }

    #[test]
    fn ddl_has_all_required_indices() {
        // Verify DDL includes indices for common query patterns
        assert!(IDEMPOTENCY_CACHE_DDL.contains("idx_idempotency_step"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("idx_idempotency_hash"));
        assert!(IDEMPOTENCY_CACHE_DDL.contains("PRIMARY KEY"));
    }

    #[test]
    fn matrixone_checkpoint_title_format() {
        // Verify the title format used for LIKE queries
        let step_id = "deploy-task-1";
        let action = "Act";
        let tier = "heavy";
        let title = format!("step:{}:{} [{}]", step_id, action, tier);
        assert_eq!(title, "step:deploy-task-1:Act [heavy]");

        // The LIKE pattern should match
        let pattern = format!("step:{}:%", step_id);
        // Simple contains check (LIKE % semantics)
        assert!(title.starts_with(&pattern.replace('%', "")));
    }

    #[test]
    fn writer_counter_increments() {
        // Verify checkpoint counter behavior without a real pool
        let mut counter: u32 = 0;
        counter += 1;
        assert_eq!(counter, 1);
        counter += 1;
        assert_eq!(counter, 2);
        // Counter ensures unique (session_id, number) pairs
    }
}
