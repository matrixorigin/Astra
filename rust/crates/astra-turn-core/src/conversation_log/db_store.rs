//! Database-backed [`CslStore`] for web-agent deployments.
//!
//! Uses the `conversation_log` table with composite PK `(session_id, seq)`.
//! All writes are INSERT-only; GC is a batch DELETE.

use async_trait::async_trait;
use sqlx::{Row, mysql::MySqlRow, query};

use astra_core::{MatrixOneSettings, SharedPool, connect_matrixone};

use super::{CslEntry, CslStore, CslStoreError, materialize};

/// Database-backed CSL store. Each session's entries live in the
/// `conversation_log` table, keyed by `(session_id, seq)`.
#[derive(Clone, Debug)]
pub struct DbCslStore {
    matrixone: MatrixOneSettings,
    pool: Option<SharedPool>,
}

impl DbCslStore {
    pub fn new(matrixone: MatrixOneSettings) -> Self {
        Self {
            matrixone,
            pool: None,
        }
    }

    pub fn with_pool(mut self, pool: SharedPool) -> Self {
        self.pool = Some(pool);
        self
    }

    async fn get_pool(&self) -> Result<sqlx::Pool<sqlx::MySql>, CslStoreError> {
        if let Some(ref pool) = self.pool {
            return Ok(pool.get().clone());
        }
        connect_matrixone(&self.matrixone)
            .await
            .map_err(|e| CslStoreError::Other(format!("pool connect: {e}")))
    }

    fn entry_from_row(row: &MySqlRow) -> Result<CslEntry, CslStoreError> {
        let payload: String = row
            .try_get("payload")
            .map_err(|e| CslStoreError::Other(format!("missing payload column: {e}")))?;
        Ok(serde_json::from_str(&payload)?)
    }
}

#[async_trait]
impl CslStore for DbCslStore {
    async fn append(
        &self,
        session_id: &str,
        entry: &CslEntry,
        meta: &super::AppendMeta,
    ) -> Result<(), CslStoreError> {
        let pool = self.get_pool().await?;
        let payload = serde_json::to_string(entry)?;
        let entry_type: i8 = if entry.is_snapshot() { 0 } else { 1 };

        query(
            "INSERT INTO conversation_log \
             (session_id, seq, turn, entry_type, trace_id, message_count, payload) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
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
        let pool = self.get_pool().await?;

        // First check if any snapshot exists — avoids deserializing all rows
        // when the subquery would return -1 (no snapshot → seq >= -1 → all rows).
        let has_snapshot: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM conversation_log \
             WHERE session_id = ? AND entry_type = 0)",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap_or(false);

        if !has_snapshot {
            return Ok(Vec::new());
        }

        let rows = query(
            "SELECT payload FROM conversation_log \
             WHERE session_id = ? AND seq >= ( \
                 SELECT MAX(seq) FROM conversation_log \
                 WHERE session_id = ? AND entry_type = 0 \
             ) \
             ORDER BY seq ASC",
        )
        .bind(session_id)
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
        let pool = self.get_pool().await?;
        let rows = query(
            "SELECT payload FROM conversation_log \
             WHERE session_id = ? AND seq > ? \
             ORDER BY seq ASC",
        )
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
        let pool = self.get_pool().await?;
        let result = query("DELETE FROM conversation_log WHERE session_id = ? AND seq < ?")
            .bind(session_id)
            .bind(before_seq as i64)
            .execute(&pool)
            .await
            .map_err(|e| CslStoreError::Other(format!("truncate: {e}")))?;

        Ok(result.rows_affected())
    }

    async fn fork(
        &self,
        parent_session_id: &str,
        new_session_id: &str,
        fork_after_turn: u32,
    ) -> Result<u64, CslStoreError> {
        let pool = self.get_pool().await?;

        let mut tx = pool
            .begin()
            .await
            .map_err(|e| CslStoreError::Other(format!("fork begin tx: {e}")))?;

        // Load parent entries up to fork_after_turn.
        let rows = query(
            "SELECT payload FROM conversation_log \
             WHERE session_id = ? AND turn <= ? \
             ORDER BY seq ASC",
        )
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
             (session_id, seq, turn, entry_type, payload) \
             VALUES (?, 1, ?, 0, ?)",
        )
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
        let pool = self.get_pool().await?;
        let rows = query(
            "SELECT seq FROM conversation_log \
             WHERE session_id = ? AND entry_type = 0 \
             ORDER BY seq ASC",
        )
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
    // The DB must have the `conversation_log` table created (via ensure_core_schema).

    async fn test_store() -> DbCslStore {
        let settings = MatrixOneSettings {
            host: std::env::var("MO_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: std::env::var("MO_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(6001),
            user: std::env::var("MO_USER").unwrap_or_else(|_| "root".into()),
            password: std::env::var("MO_PASSWORD").unwrap_or_else(|_| "111".into()),
            database: std::env::var("MO_DATABASE").unwrap_or_else(|_| "astra_test".into()),
        };
        let store = DbCslStore::new(settings);

        // Ensure table exists for tests.
        let pool = store.get_pool().await.expect("DB connection required");
        query(
            "CREATE TABLE IF NOT EXISTS conversation_log (
                session_id    VARCHAR(64) NOT NULL,
                seq           BIGINT NOT NULL,
                turn          INT NOT NULL,
                entry_type    TINYINT NOT NULL,
                trace_id      VARCHAR(64) DEFAULT NULL,
                message_count INT DEFAULT NULL,
                payload       MEDIUMTEXT NOT NULL,
                created_at    DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),
                PRIMARY KEY (session_id, seq),
                INDEX idx_csl_snapshot (session_id, entry_type, seq DESC),
                INDEX idx_csl_turn (session_id, turn)
            )",
        )
        .execute(&pool)
        .await
        .expect("create table");

        store
    }

    async fn cleanup(store: &DbCslStore, session_id: &str) {
        let pool = store.get_pool().await.unwrap();
        query("DELETE FROM conversation_log WHERE session_id = ?")
            .bind(session_id)
            .execute(&pool)
            .await
            .ok();
    }

    #[tokio::test]
    #[ignore]
    async fn db_append_and_load_roundtrip() {
        let store = test_store().await;
        let sid = &format!("db-test-roundtrip-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;

        let snap = make_snapshot(0, 1, vec![user_msg("hello")]);
        store.append(sid, &snap, &meta()).await.unwrap();

        let delta = make_delta(1, 2, vec![user_msg("turn2"), assistant_msg("resp2")]);
        store.append(sid, &delta, &meta()).await.unwrap();

        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert_eq!(entries.len(), 2);
        assert!(entries[0].is_snapshot());
        assert!(!entries[1].is_snapshot());

        let state = materialize(&entries).unwrap();
        assert_eq!(state.messages.len(), 3);
        assert_eq!(state.messages[0]["content"], "hello");
        assert_eq!(state.messages[2]["content"], "resp2");

        cleanup(&store, sid).await;
    }

    #[tokio::test]
    #[ignore]
    async fn db_load_snapshot_latest() {
        let store = test_store().await;
        let sid = &format!("db-test-snap-latest-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;

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
    #[ignore]
    async fn db_truncate_gc() {
        let store = test_store().await;
        let sid = &format!("db-test-truncate-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;

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
    #[ignore]
    async fn db_fork() {
        let store = test_store().await;
        let parent = &format!("db-test-fork-parent-{}", uuid::Uuid::new_v4());
        let child = &format!("db-test-fork-child-{}", uuid::Uuid::new_v4());
        cleanup(&store, parent).await;
        cleanup(&store, child).await;

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
    #[ignore]
    async fn db_load_after() {
        let store = test_store().await;
        let sid = &format!("db-test-load-after-{}", uuid::Uuid::new_v4());
        cleanup(&store, sid).await;

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
    #[ignore]
    async fn db_load_nonexistent_returns_empty() {
        let store = test_store().await;
        let sid = &format!("db-test-nonexistent-{}", uuid::Uuid::new_v4());
        let entries = store.load_from_latest_snapshot(sid).await.unwrap();
        assert!(entries.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn db_fork_preserves_tool_results() {
        let store = test_store().await;
        let parent = &format!("db-test-fork-tool-parent-{}", uuid::Uuid::new_v4());
        let child = &format!("db-test-fork-tool-child-{}", uuid::Uuid::new_v4());
        cleanup(&store, parent).await;
        cleanup(&store, child).await;

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
