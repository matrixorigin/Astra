//! Bounded observation recovery from canonical completion facts.
//! No chat transaction writes an observation job or a second transcript.
use crate::MaterializedConversationV1;
use astra_core::SharedPool;
use astra_turn_types::SessionContextHeadV1;
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::session_context_coordinator::{
    DatabaseSessionContextCoordinator, LoadedCanonicalContext, qualified_manifest_content_columns,
};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservationCheckpoint {
    pub observed_turn: u32,
    pub attempts: u32,
    pub retry_at_ms: i64,
    pub revision: u64,
}

pub struct ObservationCandidate {
    pub key: astra_turn_types::SessionKeyV1,
    pub turn: u32,
    head: Result<SessionContextHeadV1, String>,
    pub checkpoint: ObservationCheckpoint,
    // Keep row decoding inside the canonical coordinator, including scope,
    // content hashes, chain ordering and inherited fork authorization.
    loaded: Result<LoadedCanonicalContext, String>,
}

#[derive(Clone)]
pub struct SessionObservationStore {
    pool: SharedPool,
}

impl SessionObservationStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Discover only a bounded key page, before any checkpoint/Session joins or
    /// JSON eligibility checks. MatrixOne may sort the remaining narrow keys;
    /// this must never pull all retained heads, checkpoints or transcripts.
    pub async fn key_page(
        &self,
        after: Option<(&str, &str)>,
    ) -> Result<Vec<(String, String)>, String> {
        sqlx::query_as(KEY_PAGE_SQL)
            .bind(after.map(|(user, _)| user))
            .bind(after.map(|(user, _)| user))
            .bind(after.map(|(user, _)| user))
            .bind(after.map(|(_, session)| session))
            .fetch_all(self.pool.get())
            .await
            .map_err(|_| "observation key page failed".into())
    }

    /// Read eligibility only for this page. Concrete predicates on *each*
    /// table are deliberate: joining a limited head page is not sufficient
    /// for MatrixOne to avoid scanning the entire checkpoint catalog.
    pub async fn pending_keys(
        &self,
        keys: &[(String, String)],
    ) -> Result<Vec<(String, String)>, String> {
        if keys.is_empty() {
            return Ok(Vec::new());
        }
        if keys.len() > 64 {
            return Err("observation page exceeds limit".into());
        }
        eligibility_query(keys)
            .build_query_as()
            .fetch_all(self.pool.get())
            .await
            .map_err(|_| "observation eligibility page failed".into())
    }

    /// Every storage relation is constrained by a concrete owner/Session key.
    /// Do not join the global catalog to manifests: MatrixOne does not push
    /// those derived-row join keys down into storage scans.
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
            .bind(pod_id)
            .bind(user)
            .bind(session)
            .bind(user)
            .bind(session);
        let mut rows = query.fetch(self.pool.get());
        let Some(row) = rows
            .try_next()
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
        let mut loaded = head
            .as_ref()
            .map(|_| LoadedCanonicalContext::default())
            .map_err(Clone::clone);
        if let (Ok(head), Ok(content)) = (&head, &mut loaded)
            && content.push_row(head, &row).is_err()
        {
            loaded = Err("invalid observation context".into());
        }
        drop(row);
        while let Some(row) = rows
            .try_next()
            .await
            .map_err(|_| "observation discovery failed")?
        {
            if let (Ok(head), Ok(content)) = (&head, &mut loaded)
                && content.push_row(head, &row).is_err()
            {
                loaded = Err("invalid observation context".into());
            }
        }
        Ok(Some(ObservationCandidate {
            key,
            turn,
            head,
            checkpoint,
            loaded,
        }))
    }

    pub async fn materialize(
        &self,
        candidate: &mut ObservationCandidate,
    ) -> Result<MaterializedConversationV1, String> {
        let head = candidate.head.as_ref().map_err(Clone::clone)?;
        let loaded = std::mem::replace(
            &mut candidate.loaded,
            Err("observation already materialized".into()),
        )?;
        DatabaseSessionContextCoordinator::new(self.pool.clone())
            .materialize_loaded(head, loaded)
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
            .execute(self.pool.get()).await.map_err(|_| "observation checkpoint failed")?;
        Ok(())
    }
}

// Only keys are read by this traversal. Keep completed/head/JSON predicates out
// of it: eligibility must not cause an idle page to traverse the entire catalog.
const KEY_PAGE_SQL: &str = "SELECT owner_user_id, session_id FROM session_context_heads
    WHERE isolation_domain = 'server' AND branch_id = 'main'
      AND (? IS NULL OR owner_user_id > ? OR (owner_user_id = ? AND session_id > ?))
    ORDER BY owner_user_id, session_id LIMIT 64";

fn eligibility_query(keys: &[(String, String)]) -> sqlx::QueryBuilder<sqlx::MySql> {
    let mut query = sqlx::QueryBuilder::new(
        "SELECT h.owner_user_id, h.session_id FROM (
           SELECT owner_user_id, session_id, completed_turn FROM session_context_heads
           WHERE isolation_domain = 'server' AND branch_id = 'main'
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

fn candidate_sql() -> String {
    let columns = qualified_manifest_content_columns("m");
    format!(
        "SELECT h.head_json, h.observation_json, h.isolation_domain, h.owner_user_id, h.session_id,
                h.branch_id, h.completed_turn AS head_completed_turn, {columns},
                f.manifest_json AS fork_manifest_json
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
         ) h
         LEFT JOIN (SELECT * FROM conversation_manifest_nodes
                    WHERE isolation_domain = 'server' AND branch_id = 'main'
                      AND owner_user_id = ? AND session_id = ? AND reachable = 1) m
           ON m.compaction_generation = h.compaction_generation AND m.conversation_seq <= h.conversation_seq
         LEFT JOIN (SELECT manifest_json FROM session_forks FORCE INDEX (uq_session_forks_child)
                    WHERE isolation_domain = 'server' AND child_branch_id = 'main'
                      AND owner_user_id = ? AND child_session_id = ? AND state = 'active') f ON TRUE"
    )
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
        let page = store
            .key_page(Some(("synthetic-owner", "00050000")))
            .await
            .unwrap();
        assert_eq!(page.len(), 64);
        assert_eq!(page.first().unwrap().1, "00050001");
        assert_eq!(page.last().unwrap().1, "00050064");
        // Empty eligibility must still yield a next page, rather than restarting
        // discovery at the beginning or loading any of the 100000 transcripts.
        assert!(store.pending_keys(&page).await.unwrap().is_empty());
        let next = store
            .key_page(Some((&page[63].0, &page[63].1)))
            .await
            .unwrap();
        assert_eq!(next.first().unwrap().1, "00050065");
        for sparse in [false, true] {
            if sparse {
                sqlx::query("UPDATE agent_session_memory_work SET state_json = '{}' WHERE user_id = 'synthetic-owner' AND session_id = '00050032'")
                    .execute(pool.get()).await.unwrap();
                assert_eq!(
                    store.pending_keys(&page).await.unwrap(),
                    vec![("synthetic-owner".into(), "00050032".into())]
                );
            }
            let builder = eligibility_query(&page);
            let arguments: Vec<_> = (0..3)
                .flat_map(|_| {
                    page.iter().flat_map(|(user, session)| {
                        [serde_json::json!(user), serde_json::json!(session)]
                    })
                })
                .collect();
            let plan = prepared_plan(&pool, builder.sql().as_str(), &arguments).await;
            // MatrixOne reads storage blocks (8192 rows), even for a 64-key IN.
            // A full catalog/JSON join would read all 100000 rows and fail.
            assert_scans_bounded(&plan, 8192);
            assert!(!plan.join("\n").contains("conversation_manifest_nodes"));
            let candidate = prepared_plan(
                &pool,
                &candidate_sql(),
                serde_json::json!([
                    "synthetic-owner",
                    "00050032",
                    "synthetic-owner",
                    "00050032",
                    "synthetic-owner",
                    "00050032",
                    "plan-test",
                    "plan-worker",
                    "synthetic-owner",
                    "00050032",
                    "synthetic-owner",
                    "00050032"
                ])
                .as_array()
                .unwrap(),
            )
            .await;
            assert_scans_bounded(&candidate, 8192);
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
        // The malformed synthetic bodies are intentional: this fixture tests
        // storage plans. Canonical hash/lineage semantics have separate tests.
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
