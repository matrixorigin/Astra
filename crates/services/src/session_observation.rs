//! Bounded observation recovery from canonical completion facts.
//! No chat transaction writes an observation job or a second transcript.
use crate::MaterializedConversationV1;
use astra_core::SharedPool;
use astra_turn_types::SessionContextHeadV1;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::session_context_coordinator::{
    DatabaseSessionContextCoordinator, SessionContextCoordinator,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservationCheckpoint {
    pub observed_turn: u32,
    pub attempts: u32,
    pub retry_at_ms: i64,
    pub revision: u64,
    /// Exact write receipt; affected-row counts are not reliable on MatrixOne.
    pub receipt_id: Option<String>,
}

pub struct ObservationCandidate {
    pub key: astra_turn_types::SessionKeyV1,
    pub turn: u32,
    head: Result<SessionContextHeadV1, String>,
    pub checkpoint: ObservationCheckpoint,
}

#[derive(Clone)]
pub struct SessionObservationStore {
    pool: SharedPool,
}

impl SessionObservationStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Allocation metadata bounds a complete sweep, including deletion gaps.
    /// This does not aggregate or scan retained Session heads. Concurrent inserts
    /// and late commits are rediscovered on the next sweep.
    pub async fn catalog_high_water(&self) -> Result<u64, String> {
        let next: Option<u64> = sqlx::query_scalar(
            "SELECT AUTO_INCREMENT FROM information_schema.TABLES
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'session_context_heads'",
        )
        .fetch_one(self.pool.get())
        .await
        .map_err(|_| "observation catalog bound failed")?;
        next.map(|next| next.saturating_sub(1))
            .ok_or("observation catalog sequence unavailable".into())
    }

    /// A primary-key interval contains at most 64 allocated identities. Both
    /// bounds reach storage; LIMIT/ORDER BY alone cannot bound MatrixOne input.
    pub async fn key_page(&self, after: u64) -> Result<Vec<(String, String)>, String> {
        sqlx::query_as(KEY_PAGE_SQL)
            .bind(after)
            .bind(after.saturating_add(64))
            .fetch_all(self.pool.get())
            .await
            .map_err(|_| "observation key page failed".into())
    }

    pub async fn release_lease(&self, lease_name: &str, pod_id: &str) -> Result<(), String> {
        sqlx::query("DELETE FROM sweeper_leases WHERE sweeper_name = ? AND owner_pod_id = ?")
            .bind(lease_name)
            .bind(pod_id)
            .execute(self.pool.get())
            .await
            .map_err(|_| "observation lease release failed")?;
        Ok(())
    }

    /// Read eligibility only for this page. Concrete predicates on *each*
    /// table are deliberate: joining a limited head page is not sufficient
    /// for MatrixOne to avoid scanning the entire checkpoint catalog.
    pub async fn pending_keys(
        &self,
        after: u64,
        keys: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if keys.len() > 64 {
            return Err("observation page exceeds limit".into());
        }
        eligibility_query(after, keys)
            .build_query_as()
            .fetch_all(self.pool.get())
            .await
            .map_err(|_| "observation eligibility page failed".into())
    }

    /// Capture eligibility and the immutable context head, without payloads.
    /// Materialization must bind that head's generation/cursor directly at the
    /// storage access: MatrixOne does not push derived-row join bounds down.
    pub async fn next(
        &self,
        lease_name: &str,
        pod_id: &str,
        user: &str,
        session: &str,
    ) -> Result<Option<ObservationCandidate>, String> {
        let sql = candidate_sql();
        let query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(user)
            .bind(session)
            .bind(user)
            .bind(session)
            .bind(user)
            .bind(session)
            .bind(lease_name)
            .bind(pod_id);
        let Some(row) = query
            .fetch_optional(self.pool.get())
            .await
            .map_err(|_| "observation discovery failed")?
        else {
            return Ok(None);
        };
        let key = astra_turn_types::SessionKeyV1 {
            schema_version: astra_turn_types::SESSION_COORDINATION_SCHEMA_VERSION,
            isolation_domain: row
                .try_get("isolation_domain")
                .map_err(|_| "invalid observation key")?,
            owner_user_id: row
                .try_get("owner_user_id")
                .map_err(|_| "invalid observation key")?,
            session_id: row
                .try_get("session_id")
                .map_err(|_| "invalid observation key")?,
            branch_id: row
                .try_get("branch_id")
                .map_err(|_| "invalid observation key")?,
        };
        let turn = u32::try_from(
            row.try_get::<i64, _>("head_completed_turn")
                .map_err(|_| "invalid observation turn")?,
        )
        .map_err(|_| "invalid observation turn")?;
        let head = serde_json::from_str::<SessionContextHeadV1>(
            &row.try_get::<String, _>("head_json")
                .map_err(|_| "invalid observation head")?,
        )
        .map_err(|_| "invalid observation head".to_owned())
        .and_then(|head| {
            if head.key != key || head.cursor.completed_turn != turn {
                Err("observation head identity mismatch".into())
            } else {
                Ok(head)
            }
        });
        let checkpoint = row
            .try_get::<Option<String>, _>("observation_json")
            .map_err(|_| "invalid observation checkpoint")?
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|_| "invalid observation checkpoint")?
            .unwrap_or_default();
        Ok(Some(ObservationCandidate {
            key,
            turn,
            head,
            checkpoint,
        }))
    }

    pub async fn materialize(
        &self,
        candidate: &ObservationCandidate,
    ) -> Result<MaterializedConversationV1, String> {
        let head = candidate.head.as_ref().map_err(Clone::clone)?;
        DatabaseSessionContextCoordinator::new(self.pool.clone())
            .materialize(head)
            .await
            .map_err(|_| "observation canonical context could not be materialized".into())
    }

    /// One atomic checkpoint, independent of extraction/governance scheduling.
    /// The lease and revision fence stale owners; JSON_SET preserves fields
    /// owned by the canonical lifecycle even when another worker updates them.
    pub async fn finish(
        &self,
        lease_name: &str,
        pod_id: &str,
        candidate: &ObservationCandidate,
        success: bool,
    ) -> Result<(), String> {
        let mut checkpoint = candidate.checkpoint.clone();
        let receipt_id = uuid::Uuid::new_v4().to_string();
        checkpoint.receipt_id = Some(receipt_id.clone());
        checkpoint.revision = checkpoint
            .revision
            .checked_add(1)
            .ok_or("observation revision overflow")?;
        if success {
            checkpoint.observed_turn = checkpoint.observed_turn.max(candidate.turn);
            checkpoint.attempts = 0;
            checkpoint.retry_at_ms = 0;
        } else {
            checkpoint.attempts = checkpoint.attempts.saturating_add(1);
        }
        let backoff_ms = 5_000_i64 * (1_i64 << checkpoint.attempts.saturating_sub(1).min(8));
        let backoff_ms = backoff_ms.min(960_000);
        let json =
            serde_json::to_string(&checkpoint).map_err(|_| "invalid observation checkpoint")?;
        let mut connection = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|_| "observation checkpoint connection failed")?;
        sqlx::query(
            "INSERT INTO agent_session_memory_work (user_id, session_id, state_json, complete)
             SELECT s.user_id, s.session_id, JSON_OBJECT('observation', JSON_SET(JSON_EXTRACT(?, '$'), '$.retry_at_ms', CASE WHEN CAST(? AS SIGNED) = 1 THEN 0 ELSE CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) + ? END)), 1
             FROM agent_sessions s WHERE s.user_id = ? AND s.session_id = ? AND s.delete_requested_at IS NULL
               AND EXISTS (SELECT 1 FROM sweeper_leases l WHERE l.sweeper_name = ?
                           AND l.owner_pod_id = ? AND l.expires_at > NOW(6))
             ON DUPLICATE KEY UPDATE state_json = CASE
               WHEN COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(state_json, '$.observation.revision')) AS UNSIGNED), 0) = ?
               THEN JSON_SET(state_json, '$.observation', JSON_SET(JSON_EXTRACT(?, '$'), '$.retry_at_ms', CASE WHEN CAST(? AS SIGNED) = 1 THEN 0 ELSE CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) + ? END)) ELSE state_json END"
        ).bind(&json).bind(success).bind(backoff_ms).bind(&candidate.key.owner_user_id).bind(&candidate.key.session_id)
            .bind(lease_name).bind(pod_id).bind(candidate.checkpoint.revision).bind(json).bind(success).bind(backoff_ms)
            .execute(&mut *connection).await.map_err(|_| "observation checkpoint failed")?;
        let stored: Option<String> = sqlx::query_scalar(
            "SELECT JSON_UNQUOTE(JSON_EXTRACT(state_json, '$.observation.receipt_id'))
             FROM agent_session_memory_work WHERE user_id = ? AND session_id = ?",
        )
        .bind(&candidate.key.owner_user_id)
        .bind(&candidate.key.session_id)
        .fetch_optional(&mut *connection)
        .await
        .map_err(|_| "observation checkpoint receipt unavailable")?
        .flatten();
        if stored.as_deref() != Some(receipt_id.as_str()) {
            return Err("observation checkpoint fence rejected or receipt superseded".into());
        }
        Ok(())
    }
}

// Physical identity is stable and independent of user/Session spelling. The
// owner-scoped logical identity remains unique and authoritative for all reads.
const KEY_PAGE_SQL: &str = "SELECT owner_user_id, session_id FROM session_context_heads
    WHERE catalog_seq > ? AND catalog_seq <= ?
      AND isolation_domain = 'server' AND branch_id = 'main'
    ORDER BY catalog_seq";

fn eligibility_query(after: u64, keys: &[(String, String)]) -> sqlx::QueryBuilder<sqlx::MySql> {
    let mut query = sqlx::QueryBuilder::new(
        "SELECT h.owner_user_id, h.session_id FROM (
           SELECT owner_user_id, session_id, completed_turn FROM session_context_heads
           WHERE catalog_seq > ",
    );
    query
        .push_bind(after)
        .push(" AND catalog_seq <= ")
        .push_bind(after.saturating_add(64));
    query.push(
        " AND isolation_domain = 'server' AND branch_id = 'main'
             AND head_json IS NOT NULL AND completed_turn > 0 AND ",
    );
    for (user_column, next) in [
        ("owner_user_id", ") h JOIN (SELECT user_id, session_id FROM agent_sessions WHERE delete_requested_at IS NULL AND "),
        ("user_id", ") s ON s.user_id = h.owner_user_id AND s.session_id = h.session_id LEFT JOIN (SELECT user_id, session_id, state_json FROM agent_session_memory_work WHERE "),
        ("user_id", ") w ON w.user_id = h.owner_user_id AND w.session_id = h.session_id"),
    ].into_iter() {
        // These names are fixed above, never supplied by a caller.
        let heads = user_column == "owner_user_id";
        query.push(if heads {
            "(isolation_domain, owner_user_id, session_id, branch_id) IN ("
        } else {
            "(user_id, session_id) IN ("
        });
        for (i, (user, session)) in keys.iter().enumerate() {
            if i > 0 { query.push(", "); }
            query.push(if heads { "('server', " } else { "(" })
                .push_bind(user.clone()).push(", ").push_bind(session.clone())
                .push(if heads { ", 'main')" } else { ")" });
        }
        query.push(")").push(next);
    }
    query.push(" WHERE h.completed_turn > COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(w.state_json, '$.observation.observed_turn')) AS SIGNED), 0)
        AND COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(w.state_json, '$.observation.retry_at_ms')) AS SIGNED), 0)
            <= CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED)
        ORDER BY h.owner_user_id, h.session_id");
    query
}

fn candidate_sql() -> &'static str {
    "SELECT h.head_json, h.observation_json, h.isolation_domain, h.owner_user_id, h.session_id,
                h.branch_id, h.completed_turn AS head_completed_turn
         FROM (
           SELECT h.*, CAST(JSON_EXTRACT(w.state_json, '$.observation') AS CHAR) AS observation_json
           FROM (SELECT * FROM session_context_heads WHERE isolation_domain = 'server'
                 AND branch_id = 'main' AND owner_user_id = ? AND session_id = ?) h
           JOIN (SELECT user_id, session_id FROM agent_sessions
                 WHERE user_id = ? AND session_id = ? AND delete_requested_at IS NULL) s
             ON s.user_id = h.owner_user_id AND s.session_id = h.session_id
           LEFT JOIN (SELECT user_id, session_id, state_json FROM agent_session_memory_work
                      WHERE user_id = ? AND session_id = ?) w
             ON w.user_id = h.owner_user_id AND w.session_id = h.session_id
           WHERE h.head_json IS NOT NULL AND h.completed_turn > 0
             AND h.completed_turn > COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(w.state_json, '$.observation.observed_turn')) AS SIGNED), 0)
             AND COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(w.state_json, '$.observation.retry_at_ms')) AS SIGNED), 0)
                 <= CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED)
             AND EXISTS (SELECT 1 FROM sweeper_leases l WHERE l.sweeper_name = ?
                         AND l.owner_pod_id = ? AND l.expires_at > NOW(6))
         ) h"
}

#[cfg(test)]
mod tests {
    use super::*;

    // Inspect the actual storage operator, not just the query's LIMIT/output.
    fn assert_scans_bounded(plan: &[String], maximum: u64) {
        let text = plan.join("\n");
        let lines: Vec<_> = text.lines().collect();
        let mut scans = 0;
        for pair in lines.windows(2) {
            if pair[0].contains("Table Scan") || pair[0].contains("Index Scan") {
                let input = pair[1]
                    .split("inputRows=")
                    .nth(1)
                    .expect("EXPLAIN ANALYZE must report actual scan input")
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
                assert!(
                    input <= maximum,
                    "unbounded storage scan: {} / {}",
                    pair[0],
                    pair[1]
                );
                scans += 1;
            }
        }
        assert!(scans > 0, "must inspect executed storage scans");
    }

    async fn prepared_plan(
        pool: &SharedPool,
        sql: &str,
        args: &[serde_json::Value],
    ) -> Vec<String> {
        let mut connection = pool.get().acquire().await.unwrap();
        let mut variables = Vec::new();
        for (index, value) in args.iter().enumerate() {
            let literal = match value {
                serde_json::Value::String(value) => {
                    assert!(
                        value
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    );
                    format!("'{value}'")
                }
                serde_json::Value::Number(value) => value.to_string(),
                _ => panic!("unsupported synthetic plan argument"),
            };
            let variable = format!("@plan_p{index}");
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("SET {variable} = {literal}")))
                .execute(&mut *connection)
                .await
                .unwrap();
            variables.push(variable);
        }
        // Match the repository's prepared MatrixOne plan probes. COM_STMT_PREPARE
        // of EXPLAIN itself is not supported by all MatrixOne versions.
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "PREPARE observation_plan FROM '{}'",
            sql.replace('\'', "''")
        )))
        .execute(&mut *connection)
        .await
        .unwrap();
        let rows = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "EXPLAIN ANALYZE FORCE EXECUTE observation_plan USING {}",
            variables.join(",")
        )))
        .fetch_all(&mut *connection)
        .await
        .unwrap();
        sqlx::raw_sql("DEALLOCATE PREPARE observation_plan")
            .execute(&mut *connection)
            .await
            .unwrap();
        rows.iter().map(|row| row.get::<String, _>(0)).collect()
    }

    // Exercise valid production commits separately from synthetic plan metadata.
    // Reading a newer head between discovery and materialization would mix the
    // observation watermark with a different transcript.
    async fn assert_captured_head_is_materialized(pool: &SharedPool) {
        use crate::AcquireWriterAndReserveTurnOutcome;
        use astra_turn_types::{
            ActorContextV1, ActorKindV1, AuthorityEpochsV1, CANONICAL_TURN_DELTA_SCHEMA_VERSION,
            CanonicalDeltaModeV1, CanonicalTurnDeltaV1, CoordinatorMutationV1, SessionKeyV1,
            SessionSurfaceV1,
        };
        use serde_json::json;
        let key = SessionKeyV1::owner_session("server", "synthetic-owner", "captured-head", "main");
        let actor = ActorContextV1::owner_user(
            &key.owner_user_id,
            "observation-test",
            ActorKindV1::Server,
            SessionSurfaceV1::Server,
            None,
            AuthorityEpochsV1::default(),
        );
        sqlx::query("INSERT INTO agent_sessions (user_id,session_id,status) VALUES (?,?,'active')")
            .bind(&key.owner_user_id)
            .bind(&key.session_id)
            .execute(pool.get())
            .await
            .unwrap();
        let coordinator = DatabaseSessionContextCoordinator::new(pool.clone());
        let store = SessionObservationStore::new(pool.clone());
        let mut cursor = None;
        let mut captured = None;
        let mut catalog_bound = None;
        for turn in 1..=3 {
            let reservation = match coordinator
                .acquire_writer_and_reserve_turn(
                    &key,
                    cursor.as_ref(),
                    &actor,
                    std::time::Duration::from_secs(30),
                    &format!("writer-{turn}"),
                    &format!("turn-{turn}"),
                    None,
                )
                .await
                .unwrap()
            {
                AcquireWriterAndReserveTurnOutcome::Ready { reservation, .. } => reservation,
                other => panic!("unexpected admission: {other:?}"),
            };
            let result = coordinator
                .commit_turn_and_release_writer(
                    &reservation,
                    CanonicalTurnDeltaV1 {
                        schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                        completed_turn: turn,
                        journal_event_seq: u64::from(turn),
                        conversation_seq: u64::from(turn),
                        compaction_generation: u64::from(turn == 3),
                        config_version_id: None,
                        mode: if turn == 3 {
                            CanonicalDeltaModeV1::Replace
                        } else {
                            CanonicalDeltaModeV1::Append
                        },
                        logical_segments: vec![vec![
                            json!({"role":"assistant", "content":format!("turn-{turn}")}),
                        ]],
                    },
                    &format!("commit-{turn}"),
                )
                .await
                .unwrap();
            cursor = Some(match result {
                CoordinatorMutationV1::Applied { cursor } => cursor,
                other => panic!("unexpected commit: {other:?}"),
            });
            let bound = store.catalog_high_water().await.unwrap();
            if let Some(previous) = catalog_bound {
                assert_eq!(
                    bound, previous,
                    "re-admission/Append/Replace must not allocate new catalog identities"
                );
            }
            catalog_bound = Some(bound);
            if let Some(candidate) = captured.take() {
                let materialized = store.materialize(&candidate).await.unwrap();
                assert_eq!(materialized.head, *candidate.head.as_ref().unwrap());
                assert_eq!(materialized.messages.len(), (turn - 1) as usize);
                assert_eq!(
                    materialized.messages.last().unwrap()["content"],
                    format!("turn-{}", turn - 1)
                );
                // SQL execution success is not a durable acknowledgement.
                sqlx::query("UPDATE sweeper_leases SET expires_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE sweeper_name = 'plan-test'")
                    .execute(pool.get()).await.unwrap();
                assert!(
                    store
                        .finish("plan-test", "plan-worker", &candidate, true)
                        .await
                        .is_err(),
                    "an expired lease must not acknowledge an unstored checkpoint"
                );
                sqlx::query("UPDATE sweeper_leases SET expires_at = DATE_ADD(NOW(6), INTERVAL 1 HOUR) WHERE sweeper_name = 'plan-test'")
                    .execute(pool.get()).await.unwrap();
                store
                    .finish("plan-test", "plan-worker", &candidate, true)
                    .await
                    .unwrap();
                assert!(
                    store
                        .finish("plan-test", "plan-worker", &candidate, false)
                        .await
                        .is_err(),
                    "a stale revision must not acknowledge a rejected retry clock"
                );
            }
            // Finishing a captured older turn must leave the new turn pending.
            let candidate = store
                .next(
                    "plan-test",
                    "plan-worker",
                    &key.owner_user_id,
                    &key.session_id,
                )
                .await
                .unwrap()
                .expect("newer committed turn remains eligible");
            assert_eq!(candidate.turn, turn);
            captured = Some(candidate);
        }
        let candidate = captured.unwrap();
        assert_eq!(
            store.materialize(&candidate).await.unwrap().messages,
            vec![json!({"role":"assistant", "content":"turn-3"})]
        );
        store
            .finish("plan-test", "plan-worker", &candidate, true)
            .await
            .unwrap();
        assert!(
            store
                .next(
                    "plan-test",
                    "plan-worker",
                    &key.owner_user_id,
                    &key.session_id
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    #[ignore = "requires isolated MatrixOne: ASTRA_TEST_DB_IT=1; 100000 synthetic retained Sessions"]
    async fn retained_catalog_plans_keep_payload_reads_selective() {
        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let mut settings = astra_core::MatrixOneSettings::from_env();
        settings.database = "mysql".into();
        let admin = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&settings.database_url_with_password())
            .await
            .unwrap();
        settings.database = format!(
            "astra_test_observation_plan_{}",
            uuid::Uuid::new_v4().simple()
        );
        settings.db_pool_max_connections = 2;
        settings.db_pool_min_connections = 1;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE DATABASE `{}`",
            settings.database
        )))
        .execute(&admin)
        .await
        .unwrap();
        crate::storage::ensure_core_schema(&settings, "mysql")
            .await
            .unwrap();
        let pool = SharedPool::new(&settings).await.unwrap();
        for statement in [
            "INSERT INTO session_context_heads (isolation_domain,owner_user_id,session_id,branch_id,head_json,completed_turn,conversation_seq,compaction_generation)
             SELECT 'server', 'synthetic-owner', LPAD(CAST(result AS CHAR),8,'0'), 'main', '{}', 1, 1, 0 FROM generate_series(1,100000) g",
            "INSERT INTO agent_sessions (user_id,session_id,status) SELECT owner_user_id,session_id,'active' FROM session_context_heads",
            "INSERT INTO agent_session_memory_work (user_id,session_id,state_json,complete) SELECT owner_user_id,session_id,JSON_OBJECT('observation',JSON_OBJECT('observed_turn',1)),1 FROM session_context_heads",
            "INSERT INTO conversation_manifest_nodes (isolation_domain,owner_user_id,session_id,branch_id,manifest_root,completed_turn,conversation_seq,compaction_generation,canonical_segment_bytes,total_canonical_bytes,total_message_count,manifest_json,content_json)
             SELECT isolation_domain,owner_user_id,session_id,branch_id,LPAD(session_id,64,'0'),1,1,0,1,1,1,'{}','[]' FROM session_context_heads",
            "INSERT INTO session_forks (isolation_domain,owner_user_id,fork_id,parent_session_id,parent_branch_id,child_session_id,child_branch_id,idempotency_hash,request_hash,state,manifest_json)
             SELECT isolation_domain,owner_user_id,session_id,'parent','main',session_id,'main',LPAD(session_id,64,'0'),LPAD(session_id,64,'0'),'active','{}' FROM session_context_heads",
            "INSERT INTO sweeper_leases (sweeper_name,owner_pod_id,expires_at) VALUES ('plan-test','plan-worker',DATE_ADD(NOW(), INTERVAL 1 HOUR))",
        ] { sqlx::query(statement).execute(pool.get()).await.unwrap(); }
        let store = SessionObservationStore::new(pool.clone());
        let page = store.key_page(50000).await.unwrap();
        assert_eq!(page.len(), 64);
        assert_eq!(page.first().unwrap().1, "00050001");
        assert_eq!(page.last().unwrap().1, "00050064");
        // Empty eligibility must still yield a next page, rather than restarting
        // discovery at the beginning or loading any of the 100000 transcripts.
        assert!(store.pending_keys(50000, &page).await.unwrap().is_empty());
        let next = store.key_page(50064).await.unwrap();
        assert_eq!(next.first().unwrap().1, "00050065");
        for sparse in [false, true] {
            // Check the production discovery storage operator, not only the
            // eligibility/payload queries. Cover first, middle and last pages.
            for after in [0_u64, 50000, 99936] {
                let plan = prepared_plan(
                    &pool,
                    KEY_PAGE_SQL,
                    &[serde_json::json!(after), serde_json::json!(after + 64)],
                )
                .await;
                assert_scans_bounded(&plan, 8192);
                assert_eq!(store.key_page(after).await.unwrap().len(), 64);
            }
            assert!(store.catalog_high_water().await.unwrap() >= 100000);
            if sparse {
                sqlx::query("UPDATE agent_session_memory_work SET state_json = '{}' WHERE user_id = 'synthetic-owner' AND session_id = '00050032'")
                    .execute(pool.get()).await.unwrap();
                assert_eq!(
                    store.pending_keys(50000, &page).await.unwrap(),
                    vec![("synthetic-owner".into(), "00050032".into())]
                );
            }
            let builder = eligibility_query(50000, &page);
            let arguments: Vec<_> = [serde_json::json!(50000), serde_json::json!(50064)]
                .into_iter()
                .chain((0..3).flat_map(|_| {
                    page.iter().flat_map(|(user, session)| {
                        [serde_json::json!(user), serde_json::json!(session)]
                    })
                }))
                .collect();
            let plan = prepared_plan(&pool, builder.sql().as_str(), &arguments).await;
            // MatrixOne reads storage blocks (8192 rows), even for a 64-key IN.
            // A full catalog/JSON join would read all 100000 rows and fail.
            assert_scans_bounded(&plan, 8192);
            assert!(!plan.join("\n").contains("conversation_manifest_nodes"));
            let candidate = prepared_plan(
                &pool,
                candidate_sql(),
                serde_json::json!([
                    "synthetic-owner",
                    "00050032",
                    "synthetic-owner",
                    "00050032",
                    "synthetic-owner",
                    "00050032",
                    "plan-test",
                    "plan-worker"
                ])
                .as_array()
                .unwrap(),
            )
            .await;
            assert_scans_bounded(&candidate, 8192);
            assert!(!candidate.join("\n").contains("conversation_manifest_nodes"));
        }
        let plan = prepared_plan(
            &pool,
            &crate::session_context_coordinator::canonical_materialization_sql(),
            serde_json::json!([
                "server",
                "synthetic-owner",
                "00050032",
                "main",
                0,
                1,
                "server",
                "synthetic-owner",
                "00050032",
                "main"
            ])
            .as_array()
            .unwrap(),
        )
        .await;
        assert_scans_bounded(&plan, 8192);
        // A live Session retains immutable nodes from old compaction generations.
        // Session-key selectivity alone must not read that entire version history.
        for statement in [
            "INSERT INTO session_context_heads (isolation_domain,owner_user_id,session_id,branch_id,head_json,completed_turn,conversation_seq,compaction_generation)
             VALUES ('server','synthetic-owner','retained-generations','main','{}',100,100000,99)",
            "INSERT INTO agent_sessions (user_id,session_id,status) VALUES ('synthetic-owner','retained-generations','active')",
            "INSERT INTO conversation_manifest_nodes (isolation_domain,owner_user_id,session_id,branch_id,manifest_root,completed_turn,conversation_seq,compaction_generation,canonical_segment_bytes,total_canonical_bytes,total_message_count,manifest_json,content_json)
             SELECT 'server','synthetic-owner','retained-generations','main',LPAD(CAST(result + 100000 AS CHAR),64,'0'),1,result,FLOOR((result - 1) / 1000),1,1,1,'{}','[]' FROM generate_series(1,100000) g",
        ] { sqlx::query(statement).execute(pool.get()).await.unwrap(); }
        let candidate = prepared_plan(
            &pool,
            candidate_sql(),
            serde_json::json!([
                "synthetic-owner",
                "retained-generations",
                "synthetic-owner",
                "retained-generations",
                "synthetic-owner",
                "retained-generations",
                "plan-test",
                "plan-worker"
            ])
            .as_array()
            .unwrap(),
        )
        .await;
        assert_scans_bounded(&candidate, 8192);
        assert!(!candidate.join("\n").contains("conversation_manifest_nodes"));
        let materialization = prepared_plan(
            &pool,
            &crate::session_context_coordinator::canonical_materialization_sql(),
            serde_json::json!([
                "server",
                "synthetic-owner",
                "retained-generations",
                "main",
                99,
                100000,
                "server",
                "synthetic-owner",
                "retained-generations",
                "main"
            ])
            .as_array()
            .unwrap(),
        )
        .await;
        assert_scans_bounded(&materialization, 8192);
        // Never send the malformed synthetic plan bodies through the decoder.
        // Use a distinct Session with valid canonical commits for the race check.
        assert_captured_head_is_materialized(&pool).await;
        pool.get().close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP DATABASE `{}`",
            settings.database
        )))
        .execute(&admin)
        .await
        .unwrap();
        admin.close().await;
    }
}
