//! Observation recovery owned by canonical completion facts.
//! Completion sets a due marker in its existing head write. Idle discovery reads
//! only the due index: no historical sweep, extra per-turn SQL or transcript copy.
use crate::MaterializedConversationV1;
use crate::session_context_coordinator::{
    DatabaseSessionContextCoordinator, SessionContextCoordinator,
};
use astra_core::SharedPool;
use astra_turn_types::SessionContextHeadV1;
use serde::{Deserialize, Serialize};
use sqlx::Row;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservationCheckpoint {
    pub observed_turn: u32,
    pub attempts: u32,
    pub retry_at_ms: i64,
    pub revision: u64,
    pub receipt_id: Option<String>,
}

pub struct ObservationCandidate {
    pub key: astra_turn_types::SessionKeyV1,
    pub turn: u32,
    head: Result<SessionContextHeadV1, String>,
    pub checkpoint: ObservationCheckpoint,
    lease_token: String,
}

#[derive(Clone)]
pub struct SessionObservationStore {
    pool: SharedPool,
    owner: String,
}

// Completion, retry and lease expiry share one due timestamp. The far-future
// sentinel marks idle heads. It leads the physical primary key because MatrixOne
// does not reliably prune a nullable secondary due index. The logical SessionKey
// stays UNIQUE; no historical allocation cursor or extra job projection exists.
const DUE_SQL: &str = "SELECT owner_user_id, session_id FROM session_context_heads
    WHERE observation_due_at <= NOW(6) AND isolation_domain = 'server' AND branch_id = 'main'
    ORDER BY observation_due_at, owner_user_id, session_id LIMIT 64";
const CLAIM_SQL: &str = "UPDATE session_context_heads
    SET observation_lease_token = ?, observation_due_at = CASE WHEN EXISTS (
        SELECT 1 FROM agent_sessions WHERE user_id = ? AND session_id = ? AND delete_requested_at IS NULL)
        THEN DATE_ADD(NOW(6), INTERVAL 90 SECOND) ELSE '9999-12-31 00:00:00' END
    WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'
      AND observation_due_at <= NOW(6)";
const CANDIDATE_SQL: &str = "SELECT head_json, completed_turn, observation_json FROM session_context_heads
    WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'
      AND observation_lease_token = ? AND observation_due_at > NOW(6)
      AND observation_due_at < '9999-12-31 00:00:00'";

impl SessionObservationStore {
    pub fn new(pool: SharedPool) -> Self {
        Self {
            pool,
            owner: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub async fn due_keys(&self) -> Result<Vec<(String, String)>, String> {
        sqlx::query_as(DUE_SQL)
            .fetch_all(self.pool.get())
            .await
            .map_err(|_| "observation due discovery failed".into())
    }

    /// Per-Session CAS, not a bucket lease. A held Session never owns another
    /// Session's slot. The 90-second lease exceeds the bounded 60s attempt + 5s
    /// receipt budget; cancellation is recovered through expiry, not a scan.
    pub async fn claim(
        &self,
        user: &str,
        session: &str,
    ) -> Result<Option<ObservationCandidate>, String> {
        let lease_token = format!("{}:{}", self.owner, uuid::Uuid::new_v4());
        let mut connection = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|_| "observation claim connection failed")?;
        sqlx::query(CLAIM_SQL)
            .bind(&lease_token)
            .bind(user)
            .bind(session)
            .bind(user)
            .bind(session)
            .execute(&mut *connection)
            .await
            .map_err(|_| "observation claim failed")?;
        // Read the token back, rather than relying on affected_rows semantics.
        let Some(row) = sqlx::query(CANDIDATE_SQL)
            .bind(user)
            .bind(session)
            .bind(&lease_token)
            .fetch_optional(&mut *connection)
            .await
            .map_err(|_| "observation claim receipt failed")?
        else {
            return Ok(None);
        };
        let key = astra_turn_types::SessionKeyV1::owner_session("server", user, session, "main");
        let turn = u32::try_from(
            row.try_get::<i64, _>("completed_turn")
                .map_err(|_| "invalid observation turn")?,
        )
        .map_err(|_| "invalid observation turn")?;
        let head = row
            .try_get::<Option<String>, _>("head_json")
            .map_err(|_| "invalid observation head")?
            .ok_or("missing observation head".to_owned())
            .and_then(|json| {
                serde_json::from_str::<SessionContextHeadV1>(&json)
                    .map_err(|_| "invalid observation head".to_owned())
            })
            .and_then(|head| {
                if head.key == key && head.cursor.completed_turn == turn {
                    Ok(head)
                } else {
                    Err("observation head identity mismatch".into())
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
            lease_token,
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

    /// The checkpoint and next due state change in the same fenced head write.
    /// A completion racing this acknowledgement keeps the newer turn pending;
    /// completion preserves an existing retry deadline or in-flight lease.
    pub async fn finish(
        &self,
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
        let backoff_ms =
            (5_000_i64 * (1_i64 << checkpoint.attempts.saturating_sub(1).min(8))).min(960_000);
        let json =
            serde_json::to_string(&checkpoint).map_err(|_| "invalid observation checkpoint")?;
        let mut connection = self
            .pool
            .get()
            .acquire()
            .await
            .map_err(|_| "observation checkpoint connection failed")?;
        sqlx::query("UPDATE session_context_heads
            SET observation_json = JSON_SET(JSON_EXTRACT(?, '$'), '$.retry_at_ms', CASE WHEN CAST(? AS SIGNED) = 1 THEN 0 ELSE CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED) + ? END),
                observation_due_at = CASE WHEN CAST(? AS SIGNED) = 0 THEN DATE_ADD(NOW(6), INTERVAL ? MICROSECOND)
                    WHEN completed_turn > ? THEN NOW(6) ELSE '9999-12-31 00:00:00' END,
                observation_lease_token = NULL
            WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'
              AND observation_lease_token = ? AND observation_due_at > NOW(6)
              AND COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.revision')) AS UNSIGNED), 0) = ?")
            .bind(json).bind(success).bind(backoff_ms).bind(success).bind(backoff_ms * 1000).bind(candidate.turn)
            .bind(&candidate.key.owner_user_id).bind(&candidate.key.session_id).bind(&candidate.lease_token).bind(candidate.checkpoint.revision)
            .execute(&mut *connection).await.map_err(|_| "observation checkpoint failed")?;
        let stored: Option<String> = sqlx::query_scalar("SELECT JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.receipt_id'))
            FROM session_context_heads WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'")
            .bind(&candidate.key.owner_user_id).bind(&candidate.key.session_id).fetch_optional(&mut *connection).await
            .map_err(|_| "observation checkpoint receipt unavailable")?.flatten();
        if stored.as_deref() != Some(receipt_id.as_str()) {
            return Err("observation checkpoint fence rejected or receipt superseded".into());
        }
        Ok(())
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub async fn expire_leases_for_test(&self) {
        sqlx::query("UPDATE session_context_heads SET observation_due_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE observation_lease_token LIKE ?")
            .bind(format!("{}:%", self.owner)).execute(self.pool.get()).await.unwrap();
    }
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
            "EXPLAIN ANALYZE FORCE EXECUTE observation_plan{}",
            if variables.is_empty() {
                String::new()
            } else {
                format!(" USING {}", variables.join(","))
            }
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
        for turn in 1..=4 {
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
                        compaction_generation: u64::from(turn >= 3),
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
            if let Some(candidate) = captured.take() {
                let materialized = store.materialize(&candidate).await.unwrap();
                assert_eq!(materialized.head, *candidate.head.as_ref().unwrap());
                assert_eq!(materialized.messages.len(), (turn - 1) as usize);
                assert_eq!(
                    materialized.messages.last().unwrap()["content"],
                    format!("turn-{}", turn - 1)
                );
                // SQL execution success is not a durable acknowledgement.
                sqlx::query("UPDATE session_context_heads SET observation_due_at = DATE_SUB(NOW(6), INTERVAL 1 SECOND) WHERE session_id = 'captured-head'")
                    .execute(pool.get()).await.unwrap();
                assert!(
                    store.finish(&candidate, true).await.is_err(),
                    "an expired lease must not acknowledge an unstored checkpoint"
                );
                sqlx::query("UPDATE session_context_heads SET observation_due_at = DATE_ADD(NOW(6), INTERVAL 90 SECOND) WHERE session_id = 'captured-head'")
                    .execute(pool.get()).await.unwrap();
                store.finish(&candidate, true).await.unwrap();
                assert!(
                    store.finish(&candidate, false).await.is_err(),
                    "a stale revision must not acknowledge a rejected retry clock"
                );
            }
            // Finishing an older turn leaves new work pending. After a fully
            // acknowledged idle head, a later completion must re-arm it too.
            let candidate = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Some(candidate) = store
                        .claim(&key.owner_user_id, &key.session_id)
                        .await
                        .unwrap()
                    {
                        break candidate;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
            })
            .await
            .expect("committed work must become due");
            assert_eq!(candidate.turn, turn);
            if turn == 3 {
                assert_eq!(
                    store.materialize(&candidate).await.unwrap().messages,
                    vec![json!({"role":"assistant", "content":"turn-3"})]
                );
                store.finish(&candidate, true).await.unwrap();
                assert!(
                    store
                        .claim(&key.owner_user_id, &key.session_id)
                        .await
                        .unwrap()
                        .is_none()
                );
            } else {
                captured = Some(candidate);
            }
        }
        let candidate = captured.unwrap();
        assert_eq!(
            store.materialize(&candidate).await.unwrap().messages,
            vec![
                json!({"role":"assistant", "content":"turn-3"}),
                json!({"role":"assistant", "content":"turn-4"})
            ]
        );
        store.finish(&candidate, true).await.unwrap();
        assert!(
            store
                .claim(&key.owner_user_id, &key.session_id)
                .await
                .unwrap()
                .is_none()
        );
        // A deletion requested before claim retires the pending marker without
        // returning a candidate, even when the canonical head still exists.
        sqlx::query(
            "UPDATE agent_sessions SET delete_requested_at=NOW(6) WHERE user_id=? AND session_id=?",
        )
        .bind(&key.owner_user_id)
        .bind(&key.session_id)
        .execute(pool.get())
        .await
        .unwrap();
        sqlx::query("UPDATE session_context_heads SET observation_due_at=NOW(6) WHERE owner_user_id=? AND session_id=?")
            .bind(&key.owner_user_id).bind(&key.session_id).execute(pool.get()).await.unwrap();
        assert!(
            store
                .claim(&key.owner_user_id, &key.session_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .due_keys()
                .await
                .unwrap()
                .contains(&(key.owner_user_id, key.session_id))
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
            "UPDATE session_context_heads SET observation_json = JSON_OBJECT('observed_turn',1)",
            "INSERT INTO conversation_manifest_nodes (isolation_domain,owner_user_id,session_id,branch_id,manifest_root,completed_turn,conversation_seq,compaction_generation,canonical_segment_bytes,total_canonical_bytes,total_message_count,manifest_json,content_json)
             SELECT isolation_domain,owner_user_id,session_id,branch_id,LPAD(session_id,64,'0'),1,1,0,1,1,1,'{}','[]' FROM session_context_heads",
            "INSERT INTO session_forks (isolation_domain,owner_user_id,fork_id,parent_session_id,parent_branch_id,child_session_id,child_branch_id,idempotency_hash,request_hash,state,manifest_json)
             SELECT isolation_domain,owner_user_id,session_id,'parent','main',session_id,'main',LPAD(session_id,64,'0'),LPAD(session_id,64,'0'),'active','{}' FROM session_context_heads",
        ] { sqlx::query(statement).execute(pool.get()).await.unwrap(); }
        let store = SessionObservationStore::new(pool.clone());
        assert!(store.due_keys().await.unwrap().is_empty());
        let plan = prepared_plan(&pool, DUE_SQL, &[]).await;
        assert_scans_bounded(&plan, 8192);
        // Due discovery skips unchanged history, including empty/sparse active
        // sets. It must not join payloads or retained checkpoint history.
        for session in ["00000001", "00050032", "00100000"] {
            sqlx::query("UPDATE session_context_heads SET observation_due_at=NOW(6), observation_json=NULL WHERE owner_user_id='synthetic-owner' AND session_id=?")
                .bind(session).execute(pool.get()).await.unwrap();
        }
        assert_eq!(store.due_keys().await.unwrap().len(), 3);
        let plan = prepared_plan(&pool, DUE_SQL, &[]).await;
        assert_scans_bounded(&plan, 8192);
        assert!(!plan.join("\n").contains("conversation_manifest_nodes"));
        let candidate = prepared_plan(
            &pool,
            CANDIDATE_SQL,
            &[
                serde_json::json!("synthetic-owner"),
                serde_json::json!("00050032"),
                serde_json::json!("unused-token"),
            ],
        )
        .await;
        assert_scans_bounded(&candidate, 8192);
        // Updating the physical due key must also stay selective, including
        // Session deletion checks and secondary/unique-index maintenance.
        let claim = prepared_plan(
            &pool,
            CLAIM_SQL,
            &[
                serde_json::json!("plan-token"),
                serde_json::json!("synthetic-owner"),
                serde_json::json!("00050032"),
                serde_json::json!("synthetic-owner"),
                serde_json::json!("00050032"),
            ],
        )
        .await;
        assert_scans_bounded(&claim, 8192);
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
