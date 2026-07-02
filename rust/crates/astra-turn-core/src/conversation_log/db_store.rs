//! Database-backed [`CslStore`] for web-agent deployments.
//!
//! Uses the `conversation_log` table with composite PK `(user_id, session_id, seq)`.
//! All writes are INSERT-only; GC is a batch DELETE.
//!
//! ## Pool lifecycle
//! Prefer constructing via `DbCslStore::new(settings, user_id).with_pool(shared_pool)` —
//! the runtime always provides a `SharedPool` and that path has zero overhead.
//!
//! If `with_pool` is *not* called (e.g. integration tests), the first call to
//! `get_pool` creates a connection pool and caches it behind an `Arc<OnceCell>`
//! so subsequent calls reuse the same pool rather than opening a new one each time.

use std::sync::Arc;

use async_trait::async_trait;
use sqlx::{Row, mysql::MySqlRow, query};
use tokio::sync::OnceCell;

use astra_core::{MatrixOneSettings, SharedPool, connect_matrixone};

use super::{CslEntry, CslStore, CslStoreError, materialize, validate_session_id};

const CSL_TRUNCATE_BATCH_LIMIT: i64 = 1000;

const TRUNCATE_BEFORE_SQL: &str = "DELETE FROM conversation_log \
             WHERE user_id = ? AND session_id = ? AND seq < ? \
             ORDER BY seq ASC \
             LIMIT ?";

/// Database-backed CSL store. Each session's entries live in the
/// `conversation_log` table, keyed by `(user_id, session_id, seq)`.
///
/// Clone is cheap — both `SharedPool` and the lazy `OnceCell` are `Arc`-wrapped.
#[derive(Clone, Debug)]
pub struct DbCslStore {
    matrixone: MatrixOneSettings,
    user_id: String,
    /// Pre-built shared pool (production path via `with_pool`).
    pool: Option<SharedPool>,
    /// Lazily-initialized pool for callers that skip `with_pool` (tests, CLI).
    /// Shared across clones so only one pool is ever created per `DbCslStore` lineage.
    lazy_pool: Arc<OnceCell<sqlx::Pool<sqlx::MySql>>>,
}

impl DbCslStore {
    pub fn new(
        matrixone: MatrixOneSettings,
        user_id: impl Into<String>,
    ) -> Result<Self, CslStoreError> {
        let user_id = user_id.into();
        validate_user_id(&user_id)?;
        Ok(Self {
            matrixone,
            user_id,
            pool: None,
            lazy_pool: Arc::new(OnceCell::new()),
        })
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    /// Returns the pool to use for this request.
    ///
    /// * If `with_pool` was called → returns the pre-built `SharedPool` (zero cost).
    /// * Otherwise → lazily initialises a connection pool **once** and reuses it
    ///   for all subsequent calls, preventing per-call pool creation.
    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, CslStoreError> {
        if let Some(ref pool) = self.pool {
            return Ok(pool.get().clone());
        }
        self.lazy_pool
            .get_or_try_init(|| async {
                connect_matrixone(&self.matrixone)
                    .await
                    .map_err(|e| CslStoreError::Other(format!("pool connect: {e}")))
            })
            .await
            .cloned()
    }

    fn entry_from_row(row: &MySqlRow) -> Result<CslEntry, CslStoreError> {
        let payload: String = row
            .try_get("payload")
            .map_err(|e| CslStoreError::Other(format!("missing payload column: {e}")))?;
        Ok(serde_json::from_str(&payload)?)
    }

    async fn ensure_owner_access(
        &self,
        pool: &sqlx::Pool<sqlx::MySql>,
        session_id: &str,
    ) -> Result<(), CslStoreError> {
        let owned: Option<i32> = sqlx::query_scalar(
            "SELECT 1 FROM agent_sessions \
             WHERE session_id = ? AND user_id = ? LIMIT 1",
        )
        .bind(session_id)
        .bind(&self.user_id)
        .fetch_optional(pool)
        .await
        .map_err(|e| CslStoreError::Other(format!("session owner check: {e}")))?;
        if owned.is_none() {
            return Err(owner_mismatch_error(
                session_id,
                &self.user_id,
                "agent_sessions owner root missing or belongs to another user",
            ));
        }
        Ok(())
    }
}

fn validate_user_id(user_id: &str) -> Result<(), CslStoreError> {
    if user_id.trim().is_empty() {
        return Err(CslStoreError::Other(
            "DbCslStore requires a non-empty user_id".to_string(),
        ));
    }
    if user_id.len() > 64 {
        return Err(CslStoreError::Other(format!(
            "DbCslStore user_id exceeds 64 bytes: {}",
            user_id.len()
        )));
    }
    Ok(())
}

fn owner_mismatch_error(session_id: &str, user_id: &str, reason: &str) -> CslStoreError {
    CslStoreError::Other(format!(
        "conversation_log owner mismatch for session_id={session_id} user_id={user_id}: {reason}"
    ))
}

#[async_trait]
impl CslStore for DbCslStore {
    async fn append(
        &self,
        session_id: &str,
        entry: &CslEntry,
        meta: &super::AppendMeta,
    ) -> Result<(), CslStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        self.ensure_owner_access(&pool, session_id).await?;
        let payload = serde_json::to_string(entry)?;
        let entry_type: i8 = if entry.is_snapshot() { 0 } else { 1 };

        query(
            "INSERT INTO conversation_log \
             (user_id, session_id, seq, turn, entry_type, trace_id, message_count, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&self.user_id)
        .bind(session_id)
        .bind(entry.seq() as i64)
        .bind(entry.turn() as i32)
        .bind(entry_type)
        .bind(meta.trace_id.as_deref())
        .bind(meta.message_count.map(|c| c as i32))
        .bind(&payload)
        .execute(&pool)
        .await
        .map_err(|e| CslStoreError::Other(format!("insert: {e}")))?;

        Ok(())
    }

    async fn load_from_latest_snapshot(
        &self,
        session_id: &str,
    ) -> Result<Vec<CslEntry>, CslStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        self.ensure_owner_access(&pool, session_id).await?;

        // First check if any snapshot exists — avoids deserializing all rows
        // when the subquery would return NULL. MatrixOne's MySQL protocol does
        // not consistently decode SQL existence probes as a Rust bool through
        // sqlx, so use COUNT(*) and propagate decode/query errors instead of
        // silently treating them as an empty log.
        let snapshot_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_log \
             WHERE user_id = ? AND session_id = ? AND entry_type = 0",
        )
        .bind(&self.user_id)
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .map_err(|e| CslStoreError::Other(format!("snapshot count: {e}")))?;

        if snapshot_count <= 0 {
            return Ok(Vec::new());
        }

        let rows = query(
            "SELECT payload FROM conversation_log \
             WHERE user_id = ? AND session_id = ? AND seq >= ( \
                 SELECT MAX(seq) FROM conversation_log \
                 WHERE user_id = ? AND session_id = ? AND entry_type = 0 \
             ) \
             ORDER BY seq ASC",
        )
        .bind(&self.user_id)
        .bind(session_id)
        .bind(&self.user_id)
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(|e| CslStoreError::Other(format!("load from snapshot: {e}")))?;

        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let entries: Vec<CslEntry> = rows
            .iter()
            .map(Self::entry_from_row)
            .collect::<Result<_, _>>()?;
        Ok(entries)
    }

    async fn load_after(
        &self,
        session_id: &str,
        after_seq: u64,
    ) -> Result<Vec<CslEntry>, CslStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        self.ensure_owner_access(&pool, session_id).await?;
        let rows = query(
            "SELECT payload FROM conversation_log \
             WHERE user_id = ? AND session_id = ? AND seq > ? \
             ORDER BY seq ASC",
        )
        .bind(&self.user_id)
        .bind(session_id)
        .bind(after_seq as i64)
        .fetch_all(&pool)
        .await
        .map_err(|e| CslStoreError::Other(format!("load_after: {e}")))?;

        rows.iter().map(Self::entry_from_row).collect()
    }

    async fn truncate_before(
        &self,
        session_id: &str,
        before_seq: u64,
    ) -> Result<u64, CslStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        self.ensure_owner_access(&pool, session_id).await?;
        let before_seq = i64::try_from(before_seq).unwrap_or(i64::MAX);
        let mut total_deleted = 0_u64;
        loop {
            let deleted = query(TRUNCATE_BEFORE_SQL)
                .bind(&self.user_id)
                .bind(session_id)
                .bind(before_seq)
                .bind(CSL_TRUNCATE_BATCH_LIMIT)
                .execute(&pool)
                .await
                .map_err(|e| CslStoreError::Other(format!("truncate: {e}")))?
                .rows_affected();
            total_deleted = total_deleted.checked_add(deleted).ok_or_else(|| {
                CslStoreError::Other("truncate: deleted row total overflow".to_string())
            })?;
            if deleted == 0 {
                break;
            }
        }

        Ok(total_deleted)
    }

    async fn fork(
        &self,
        parent_session_id: &str,
        new_session_id: &str,
        fork_after_turn: u32,
    ) -> Result<u64, CslStoreError> {
        validate_session_id(parent_session_id)?;
        validate_session_id(new_session_id)?;
        let pool = self.get_pool().await?;
        self.ensure_owner_access(&pool, parent_session_id).await?;
        self.ensure_owner_access(&pool, new_session_id).await?;

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| CslStoreError::Other(format!("fork begin tx: {e}")))?;

        // Load parent entries up to fork_after_turn.
        let rows = query(
            "SELECT payload FROM conversation_log \
             WHERE user_id = ? AND session_id = ? AND turn <= ? \
             ORDER BY seq ASC",
        )
        .bind(&self.user_id)
        .bind(parent_session_id)
        .bind(fork_after_turn as i32)
        .fetch_all(&mut *tx)
        .await
        .map_err(|e| CslStoreError::Other(format!("fork read: {e}")))?;

        if rows.is_empty() {
            tx.commit()
                .await
                .map_err(|e| CslStoreError::Other(format!("fork commit: {e}")))?;
            return Ok(0);
        }

        let entries: Vec<CslEntry> = rows
            .iter()
            .map(Self::entry_from_row)
            .collect::<Result<_, _>>()?;

        // Materialize state at fork point, write as single Snapshot at seq=1.
        let mat = materialize(&entries)?;
        let fork_snapshot = CslEntry::Snapshot {
            seq: 1,
            turn: mat.last_turn,
            messages: mat.messages,
            session_state: mat.session_state,
        };

        let payload = serde_json::to_string(&fork_snapshot)?;
        query(
            "INSERT INTO conversation_log \
             (user_id, session_id, seq, turn, entry_type, payload) \
             VALUES (?, ?, 1, ?, 0, ?)",
        )
        .bind(&self.user_id)
        .bind(new_session_id)
        .bind(mat.last_turn as i32)
        .bind(&payload)
        .execute(&mut *tx)
        .await
        .map_err(|e| CslStoreError::Other(format!("fork write: {e}")))?;

        tx.commit()
            .await
            .map_err(|e| CslStoreError::Other(format!("fork commit: {e}")))?;

        Ok(1)
    }

    async fn snapshot_seqs(&self, session_id: &str) -> Result<Vec<u64>, CslStoreError> {
        validate_session_id(session_id)?;
        let pool = self.get_pool().await?;
        self.ensure_owner_access(&pool, session_id).await?;
        let rows = query(
            "SELECT seq FROM conversation_log \
             WHERE user_id = ? AND session_id = ? AND entry_type = 0 \
             ORDER BY seq ASC",
        )
        .bind(&self.user_id)
        .bind(session_id)
        .fetch_all(&pool)
        .await
        .map_err(|e| CslStoreError::Other(format!("snapshot_seqs: {e}")))?;

        Ok(rows.iter().map(|r| r.get::<i64, _>("seq") as u64).collect())
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_log::{AppendMeta, SessionStateCompact};
    use serde_json::json;
    use sqlx::{QueryBuilder, query_as};

    fn meta() -> AppendMeta {
        AppendMeta::default()
    }

    fn user_msg(content: &str) -> serde_json::Value {
        json!({"role": "user", "content": content})
    }

    fn assistant_msg(content: &str) -> serde_json::Value {
        json!({"role": "assistant", "content": content})
    }

    fn tool_result_msg(id: &str, content: &str) -> serde_json::Value {
        json!({"role": "tool", "tool_call_id": id, "content": content})
    }

    fn make_snapshot(seq: u64, turn: u32, msgs: Vec<serde_json::Value>) -> CslEntry {
        CslEntry::Snapshot {
            seq,
            turn,
            messages: msgs,
            session_state: SessionStateCompact::default(),
        }
    }

    fn make_delta(seq: u64, turn: u32, appended: Vec<serde_json::Value>) -> CslEntry {
        CslEntry::TurnDelta {
            seq,
            turn,
            appended,
            state_patch: None,
        }
    }

    // These tests require a live MatrixOne/MySQL instance.
    // Run with: cargo test -p astra-turn-core conversation_log::db_store -- --ignored
    //
    // The DB schema is always created through astra-services' core schema
    // bootstrap so these tests cannot drift into a private CSL table shape.

    #[test]
    fn db_store_tests_do_not_embed_private_core_schema_ddl() {
        let source = include_str!("db_store.rs");
        for table in ["agent_sessions", "conversation_log"] {
            let private_ddl = format!("{}{}", "CREATE TABLE IF NOT EXISTS ", table);
            assert!(
                !source.contains(&private_ddl),
                "DbCslStore tests must use astra_services::ensure_core_schema instead of private {table} DDL"
            );
        }
    }

    #[test]
    fn truncate_before_sql_is_owner_scoped_and_batch_bounded() {
        let normalized = TRUNCATE_BEFORE_SQL
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(
            normalized.contains("WHERE user_id = ? AND session_id = ? AND seq < ?"),
            "CSL truncate must stay owner/session scoped"
        );
        assert!(
            normalized.contains("ORDER BY seq ASC"),
            "CSL truncate must delete in deterministic sequence order"
        );
        assert!(
            normalized.ends_with("LIMIT ?"),
            "CSL truncate must be batch bounded"
        );
        assert!(CSL_TRUNCATE_BATCH_LIMIT > 0);
        assert!(CSL_TRUNCATE_BATCH_LIMIT <= 10_000);
    }

    #[test]
    fn truncate_before_loops_until_batch_is_empty() {
        let source = include_str!("db_store.rs");
        let body = source
            .split("async fn truncate_before")
            .nth(1)
            .and_then(|rest| rest.split("async fn fork").next())
            .expect("truncate_before body");
        assert!(
            body.contains("let before_seq = i64::try_from(before_seq).unwrap_or(i64::MAX);"),
            "truncate_before must not wrap large u64 sequence values into negative BIGINT values"
        );
        assert!(
            body.contains("loop {") && body.contains("if deleted == 0"),
            "truncate_before must keep pruning batches until no rows remain"
        );
        assert!(
            body.contains("checked_add(deleted)"),
            "truncate_before must fail loudly on impossible deleted-row overflow"
        );
        assert!(
            body.contains("CSL_TRUNCATE_BATCH_LIMIT"),
            "truncate_before must bind the batch limit constant"
        );
    }

    async fn test_store() -> DbCslStore {
        test_store_for("csl-test-user").await
    }

    async fn test_store_for(user_id: &str) -> DbCslStore {
        assert_eq!(
            std::env::var("ASTRA_TEST_DB_IT").as_deref(),
            Ok("1"),
            "set ASTRA_TEST_DB_IT=1 to run this ignored test"
        );
        let settings = MatrixOneSettings::from_env();
        let bootstrap_catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::ensure_core_schema(&settings, &bootstrap_catalog)
            .await
            .expect("ensure core schema");
        DbCslStore::new(settings, user_id.to_string()).expect("user-scoped store")
    }

    async fn cleanup(store: &DbCslStore, session_id: &str) {
        cleanup_for_user(store, session_id, &store.user_id).await;
    }

    async fn cleanup_for_user(store: &DbCslStore, session_id: &str, user_id: &str) {
        let pool = store.get_pool().await.unwrap();
        query("DELETE FROM conversation_log WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .ok();
        query("DELETE FROM agent_sessions WHERE session_id = ? AND user_id = ?")
            .bind(session_id)
            .bind(user_id)
            .execute(&pool)
            .await
            .ok();
    }

    async fn create_session(store: &DbCslStore, session_id: &str) {
        create_session_for(store, session_id, &store.user_id).await;
    }

    async fn create_session_for(store: &DbCslStore, session_id: &str, user_id: &str) {
        let pool = store.get_pool().await.unwrap();
        query(
            "INSERT INTO agent_sessions (session_id, user_id, title, status, event_count)
             VALUES (?, ?, 'csl test', 'active', 0)",
        )
        .bind(session_id)
        .bind(user_id)
        .execute(&pool)
        .await
        .expect("insert test session");
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_append_and_load_roundtrip() {
        let store = test_store().await;
        let sid = &format!("db-test-roundtrip-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;
        create_session(&store, sid).await;

        let snap = make_snapshot(0, 1, vec![user_msg("hello")]);
        store.append(sid, &snap, &meta()).await.unwrap();

        let delta = make_delta(1, 2, vec![user_msg("turn2"), assistant_msg("resp2")]);
        store.append(sid, &delta, &meta()).await.unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_snapshot());
        assert!(!entries[1].is_snapshot());

        let row_user_ids: Vec<String> =
            query("SELECT user_id FROM conversation_log WHERE session_id = ? ORDER BY seq ASC")
                .bind(sid)
                .fetch_all(&store.get_pool().await.unwrap())
                .await
                .unwrap()
                .into_iter()
                .map(|row| row.get::<String, _>("user_id"))
                .collect();
        assert_eq!(
            row_user_ids,
            vec![store.user_id.clone(), store.user_id.clone()],
            "DB CSL rows must be physically owner-scoped"
        );

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0]["content"], "hello");
        assert_eq!(state.messages[2]["content"], "resp2");

        cleanup(&store, sid).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_owner_scope_refuses_cross_owner_access_without_overwrite() {
        let owner_user_id = format!("u-csl-owner-{}", uuid::Uuid::new_v4());
        let other_user_id = format!("u-csl-other-{}", uuid::Uuid::new_v4());
        let owner_store = test_store_for(&owner_user_id).await;
        let other_store = test_store_for(&other_user_id).await;
        let sid = &format!("db-test-owner-scope-{}", uuid::Uuid::new_v4());
        cleanup_for_user(&owner_store, sid, &owner_user_id).await;
        cleanup_for_user(&owner_store, sid, &other_user_id).await;
        create_session_for(&owner_store, sid, &owner_user_id).await;

        owner_store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("owner")]), &meta())
            .await
            .unwrap();

        let load_err = other_store
            .load_from_latest_snapshot(sid)
            .await
            .expect_err("other owner must not load an existing CSL");
        assert!(
            load_err
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected load error: {load_err}"
        );

        let append_err = other_store
            .append(
                sid,
                &make_delta(1, 2, vec![assistant_msg("wrong")]),
                &meta(),
            )
            .await
            .expect_err("other owner must not append to an existing CSL");
        assert!(
            append_err
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected append error: {append_err}"
        );

        let truncate_err = other_store
            .truncate_before(sid, 99)
            .await
            .expect_err("other owner must not truncate an existing CSL");
        assert!(
            truncate_err
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected truncate error: {truncate_err}"
        );

        let rows: Vec<(String, i64)> =
            query_as("SELECT user_id, seq FROM conversation_log WHERE session_id = ? ORDER BY seq")
                .bind(sid)
                .fetch_all(&owner_store.get_pool().await.unwrap())
                .await
                .unwrap();
        assert_eq!(
            rows,
            vec![(owner_user_id.clone(), 0)],
            "failed cross-owner operations must not rewrite owner rows"
        );

        cleanup_for_user(&owner_store, sid, &owner_user_id).await;
        cleanup_for_user(&owner_store, sid, &other_user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_owner_reads_ignore_foreign_csl_rows_after_session_owner_check() {
        let owner_user_id = format!("u-csl-owner-{}", uuid::Uuid::new_v4());
        let other_user_id = format!("u-csl-other-{}", uuid::Uuid::new_v4());
        let owner_store = test_store_for(&owner_user_id).await;
        let other_store = test_store_for(&other_user_id).await;
        let sid = &format!("db-test-owner-noise-{}", uuid::Uuid::new_v4());
        cleanup_for_user(&owner_store, sid, &owner_user_id).await;
        cleanup_for_user(&owner_store, sid, &other_user_id).await;
        create_session_for(&owner_store, sid, &owner_user_id).await;

        owner_store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("owner")]), &meta())
            .await
            .unwrap();

        let foreign_payload =
            serde_json::to_string(&make_snapshot(0, 1, vec![user_msg("foreign")])).unwrap();
        query(
            "INSERT INTO conversation_log \
             (user_id, session_id, seq, turn, entry_type, payload) \
             VALUES (?, ?, 0, 1, 0, ?)",
        )
        .bind(&other_user_id)
        .bind(sid)
        .bind(&foreign_payload)
        .execute(&owner_store.get_pool().await.unwrap())
        .await
        .expect("insert foreign CSL noise row");

        owner_store
            .append(
                sid,
                &make_delta(1, 2, vec![assistant_msg("owner delta")]),
                &meta(),
            )
            .await
            .expect("foreign CSL noise must not block owner append");

        let entries = owner_store
            .load_from_latest_snapshot(sid)
            .await
            .expect("owner load should use owner-scoped CSL rows only");
        assert_eq!(entries.len(), 2);
        let materialized = materialize(&entries).unwrap();
        assert_eq!(
            materialized.messages,
            vec![user_msg("owner"), assistant_msg("owner delta")]
        );

        let other_err = other_store
            .load_from_latest_snapshot(sid)
            .await
            .expect_err("foreign CSL row must not grant session ownership");
        assert!(
            other_err
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected other-owner error: {other_err}"
        );

        cleanup_for_user(&owner_store, sid, &owner_user_id).await;
        cleanup_for_user(&owner_store, sid, &other_user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_fork_refuses_mixed_owner_parent_or_child() {
        let owner_user_id = format!("u-csl-owner-{}", uuid::Uuid::new_v4());
        let other_user_id = format!("u-csl-other-{}", uuid::Uuid::new_v4());
        let owner_store = test_store_for(&owner_user_id).await;
        let other_store = test_store_for(&other_user_id).await;
        let parent = &format!("db-test-fork-parent-{}", uuid::Uuid::new_v4());
        let child = &format!("db-test-fork-child-{}", uuid::Uuid::new_v4());
        cleanup_for_user(&owner_store, parent, &owner_user_id).await;
        cleanup_for_user(&owner_store, parent, &other_user_id).await;
        cleanup_for_user(&owner_store, child, &owner_user_id).await;
        cleanup_for_user(&owner_store, child, &other_user_id).await;
        create_session_for(&owner_store, parent, &owner_user_id).await;

        owner_store
            .append(
                parent,
                &make_snapshot(0, 1, vec![user_msg("parent")]),
                &meta(),
            )
            .await
            .unwrap();

        let parent_err = other_store
            .fork(parent, child, 1)
            .await
            .expect_err("other owner must not fork an existing parent CSL");
        assert!(
            parent_err
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected parent fork error: {parent_err}"
        );

        create_session_for(&other_store, child, &other_user_id).await;
        other_store
            .append(
                child,
                &make_snapshot(0, 1, vec![user_msg("other child")]),
                &meta(),
            )
            .await
            .unwrap();
        let child_err = owner_store
            .fork(parent, child, 1)
            .await
            .expect_err("owner must not overwrite a child CSL owned by another user");
        assert!(
            child_err
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected child fork error: {child_err}"
        );

        let child_rows: Vec<(String, i64)> =
            query_as("SELECT user_id, seq FROM conversation_log WHERE session_id = ? ORDER BY seq")
                .bind(child)
                .fetch_all(&owner_store.get_pool().await.unwrap())
                .await
                .unwrap();
        assert_eq!(
            child_rows,
            vec![(other_user_id.clone(), 0)],
            "failed fork must not overwrite child rows owned by another user"
        );

        cleanup_for_user(&owner_store, parent, &owner_user_id).await;
        cleanup_for_user(&owner_store, parent, &other_user_id).await;
        cleanup_for_user(&owner_store, child, &owner_user_id).await;
        cleanup_for_user(&owner_store, child, &other_user_id).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_load_snapshot_latest() {
        let store = test_store().await;
        let sid = &format!("db-test-snap-latest-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;
        create_session(&store, sid).await;

        // Two snapshots with deltas between.
        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("old")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("delta_old")]), &meta())
            .await
            .unwrap();
        store
            .append(
                sid,
                &make_snapshot(2, 3, vec![user_msg("compacted")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(sid, &make_delta(3, 4, vec![user_msg("new")]), &meta())
            .await
            .unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq(), 2);
        assert_eq!(entries[1].seq(), 3);

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 2);
        assert_eq!(state.messages[0]["content"], "compacted");
        assert_eq!(state.messages[1]["content"], "new");

        cleanup(&store, sid).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_truncate_gc() {
        let store = test_store().await;
        let sid = &format!("db-test-truncate-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;
        create_session(&store, sid).await;

        store
            .append(sid, &make_snapshot(0, 1, vec![user_msg("old")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("mid")]), &meta())
            .await
            .unwrap();
        store
            .append(
                sid,
                &make_snapshot(2, 3, vec![user_msg("new_snap")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(sid, &make_delta(3, 4, vec![user_msg("latest")]), &meta())
            .await
            .unwrap();

        let removed = store.truncate_before(sid, 2).await.unwrap();
        assert_eq!(removed, 2);

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq(), 2);
        assert_eq!(entries[1].seq(), 3);

        cleanup(&store, sid).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_truncate_gc_deletes_more_than_one_batch() {
        let store = test_store().await;
        let sid = &format!("db-test-truncate-batch-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;
        create_session(&store, sid).await;

        let pool = store.get_pool().await.unwrap();
        let before_seq = CSL_TRUNCATE_BATCH_LIMIT + 5;
        let total_rows = before_seq + 2;

        let mut builder = QueryBuilder::<sqlx::MySql>::new(
            "INSERT INTO conversation_log \
             (user_id, session_id, seq, turn, entry_type, payload) ",
        );
        builder.push_values(0..total_rows, |mut row, seq| {
            let entry = make_delta(seq as u64, (seq + 1) as u32, vec![user_msg("bulk")]);
            let payload = serde_json::to_string(&entry).expect("serialize CSL test entry");
            row.push_bind(&store.user_id)
                .push_bind(sid)
                .push_bind(seq)
                .push_bind((seq + 1) as i32)
                .push_bind(1_i8)
                .push_bind(payload);
        });
        builder
            .build()
            .execute(&pool)
            .await
            .expect("insert bulk CSL rows");

        let removed = store.truncate_before(sid, before_seq as u64).await.unwrap();
        assert_eq!(
            removed, before_seq as u64,
            "truncate_before must keep deleting beyond one batch"
        );
        assert!(
            removed > CSL_TRUNCATE_BATCH_LIMIT as u64,
            "test must cross the configured truncate batch limit"
        );

        let residual: (i64, Option<i64>, Option<i64>) = query_as(
            "SELECT COUNT(*) AS count, MIN(seq) AS min_seq, MAX(seq) AS max_seq
             FROM conversation_log
             WHERE user_id = ? AND session_id = ?",
        )
        .bind(&store.user_id)
        .bind(sid)
        .fetch_one(&pool)
        .await
        .expect("count residual CSL rows");
        assert_eq!(residual, (2, Some(before_seq), Some(before_seq + 1)));

        cleanup(&store, sid).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_fork() {
        let store = test_store().await;
        let parent = &format!("db-test-fork-parent-{}", uuid::Uuid::new_v4());
        let child = &format!("db-test-fork-child-{}", uuid::Uuid::new_v4());
        cleanup(&store, parent).await;
        cleanup(&store, child).await;
        create_session(&store, parent).await;
        create_session(&store, child).await;

        store
            .append(
                parent,
                &make_snapshot(0, 1, vec![user_msg("t1"), assistant_msg("r1")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(1, 2, vec![user_msg("t2"), assistant_msg("r2")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(2, 3, vec![user_msg("t3"), assistant_msg("r3")]),
                &meta(),
            )
            .await
            .unwrap();

        let count = store.fork(parent, child, 2).await.unwrap();
        assert_eq!(count, 1);

        let entries = store.load_from_latest_snapshot(child).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_snapshot());

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 4); // t1, r1, t2, r2
        assert_eq!(state.messages[0]["content"], "t1");
        assert_eq!(state.messages[3]["content"], "r2");

        cleanup(&store, parent).await;
        cleanup(&store, child).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_load_after() {
        let store = test_store().await;
        let sid = &format!("db-test-load-after-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;
        create_session(&store, sid).await;

        store
            .append(sid, &make_snapshot(0, 1, vec![]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(1, 2, vec![user_msg("a")]), &meta())
            .await
            .unwrap();
        store
            .append(sid, &make_delta(2, 3, vec![user_msg("b")]), &meta())
            .await
            .unwrap();

        let after = store.load_after(sid, 1).await.unwrap();
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].seq(), 2);

        cleanup(&store, sid).await;
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_load_missing_parent_session_fails_closed() {
        let store = test_store().await;
        let sid = &format!("db-test-nonexistent-{}", uuid::Uuid::new_v4());
        let error = store
            .load_from_latest_snapshot(sid)
            .await
            .expect_err("missing parent session must fail closed");
        assert!(
            error
                .to_string()
                .contains("conversation_log owner mismatch"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    #[ignore = "requires live DB"]
    async fn db_fork_preserves_tool_results() {
        let store = test_store().await;
        let parent = &format!("db-test-fork-tool-parent-{}", uuid::Uuid::new_v4());
        let child = &format!("db-test-fork-tool-child-{}", uuid::Uuid::new_v4());
        cleanup(&store, parent).await;
        cleanup(&store, child).await;
        create_session(&store, parent).await;
        create_session(&store, child).await;

        store
            .append(
                parent,
                &make_snapshot(0, 1, vec![user_msg("read file")]),
                &meta(),
            )
            .await
            .unwrap();
        store
            .append(
                parent,
                &make_delta(
                    1,
                    1,
                    vec![tool_result_msg("c1", "fn main() {}"), assistant_msg("done")],
                ),
                &meta(),
            )
            .await
            .unwrap();

        store.fork(parent, child, 1).await.unwrap();

        let entries = store.load_from_latest_snapshot(child).await.unwrap();
        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[1]["role"], "tool");
        assert_eq!(state.messages[1]["content"], "fn main() {}");

        cleanup(&store, parent).await;
        cleanup(&store, child).await;
    }
}
