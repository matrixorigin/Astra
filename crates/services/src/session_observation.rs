//! Observation recovery owned by canonical completion facts.
//! Completion sets a due marker in its existing head write. Idle discovery reads
//! only the due index: no historical sweep, extra per-turn SQL or transcript copy.
use crate::session_context_coordinator::{
    DatabaseSessionContextCoordinator, MaterializedConversationDeltaV1,
};
use astra_core::SharedPool;
use astra_turn_types::SessionContextHeadV1;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

pub mod window;

#[derive(Debug, thiserror::Error)]
pub enum ObservationError {
    #[error("{0}")]
    SharedDependency(String),
    #[error("{0}")]
    Session(String),
}
impl ObservationError {
    pub fn is_shared_dependency(&self) -> bool {
        matches!(self, Self::SharedDependency(_))
    }
    fn database(message: &str, error: &sqlx::Error) -> Self {
        if shared_database_failure(error) {
            Self::SharedDependency(message.into())
        } else {
            Self::Session(message.into())
        }
    }
}
impl From<String> for ObservationError {
    fn from(message: String) -> Self {
        Self::Session(message)
    }
}
impl From<&str> for ObservationError {
    fn from(message: &str) -> Self {
        Self::Session(message.into())
    }
}
fn shared_database_failure(error: &sqlx::Error) -> bool {
    matches!(
        error,
        sqlx::Error::PoolClosed
            | sqlx::Error::PoolTimedOut
            | sqlx::Error::Io(_)
            | sqlx::Error::Tls(_)
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObservationProgress {
    pub head: SessionContextHeadV1,
    pub cursor: window::ObservationTextCursor,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ObservationCheckpoint {
    pub observed_turn: u32,
    pub observed_head: Option<SessionContextHeadV1>,
    pub memory_operation_segment: bool,
    pub attempts: u32,
    pub revision: u64,
    pub receipt_id: Option<String>,
    pub progress: Option<ObservationProgress>,
}

#[derive(Clone)]
pub struct ObservationCandidate {
    pub key: astra_turn_types::SessionKeyV1,
    pub turn: u32,
    head: Result<SessionContextHeadV1, String>,
    pub checkpoint: ObservationCheckpoint,
    lease_token: String,
    pub next_memory_operation_segment: Option<bool>,
    pub next_progress: Option<ObservationProgress>,
}

#[derive(Clone)]
pub struct SessionObservationStore {
    pool: SharedPool,
    owner: String,
}

pub const OBSERVATION_BUCKET_COUNT: u8 = 16;
const BUCKET_WIDTH: i64 = 1 << 58;

// The high bits hold the stable Session bucket. The low bits are the single
// durable deadline in Unix microseconds. NULL denotes idle. Deadline writes
// replace only the low bits, preserving a direct Session primary key.
pub(crate) fn observation_bucket_prefix(key: &astra_turn_types::SessionKeyV1) -> i64 {
    let mut hash = Sha256::new();
    hash.update(key.owner_user_id.as_bytes());
    hash.update([0]);
    hash.update(key.session_id.as_bytes());
    i64::from(hash.finalize()[0] % OBSERVATION_BUCKET_COUNT) * BUCKET_WIDTH
}

const DISCOVERY_CLOCK_SQL: &str = "SELECT TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6))";

fn due_query(cutoff: i64, buckets: std::ops::Range<u8>) -> sqlx::QueryBuilder<sqlx::MySql> {
    // MatrixOne prunes these prepared scalar ranges at the covering index.
    // Applying LIMIT without a sort lets the scan stop after a batch.
    let mut sql = sqlx::QueryBuilder::new(
        "SELECT owner_user_id, session_id FROM session_context_heads WHERE ",
    );
    for (index, bucket) in buckets.enumerate() {
        if index > 0 {
            sql.push(" OR ");
        }
        let prefix = i64::from(bucket) * BUCKET_WIDTH;
        sql.push("observation_due_key BETWEEN CAST(")
            .push_bind(prefix)
            .push(" AS SIGNED) AND CAST(")
            .push_bind(prefix + cutoff)
            .push(" AS SIGNED)");
    }
    sql.push(" LIMIT 8");
    sql
}

const CLAIM_SQL: &str = "UPDATE session_context_heads AS h
    LEFT JOIN agent_sessions AS s ON s.user_id = h.owner_user_id AND s.session_id = h.session_id
        AND s.delete_requested_at IS NULL
    SET h.observation_lease_token = CASE WHEN s.session_id IS NULL THEN NULL ELSE ? END,
        h.observation_due_key = CASE WHEN s.session_id IS NULL THEN NULL
            ELSE CAST((h.observation_due_key >> 58) << 58 AS SIGNED)
                  + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', DATE_ADD(NOW(6), INTERVAL 90 SECOND)) END
    WHERE h.isolation_domain = 'server' AND h.owner_user_id = ? AND h.session_id = ? AND h.branch_id = 'main'
      AND MOD(h.observation_due_key, 288230376151711744) <= TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6))";
const CANDIDATE_SQL: &str = "SELECT observation_head_json AS head_json, observation_target_turn AS completed_turn, observation_json FROM session_context_heads
    WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'
      AND observation_lease_token = ? AND MOD(observation_due_key, 288230376151711744) > TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6))";

impl SessionObservationStore {
    pub fn new(pool: SharedPool) -> Self {
        Self {
            pool,
            owner: uuid::Uuid::new_v4().to_string(),
        }
    }

    /// One authoritative cutoff for both the preferred and the fallback read.
    /// Binding it as a scalar lets the index prune future-due ranges.
    pub async fn discovery_cutoff(&self) -> Result<i64, ObservationError> {
        let cutoff = sqlx::query_scalar(DISCOVERY_CLOCK_SQL)
            .fetch_one(self.pool.get())
            .await
            .map_err(|_| {
                ObservationError::SharedDependency("observation clock lookup failed".into())
            })?;
        if !(0..BUCKET_WIDTH).contains(&cutoff) {
            return Err(ObservationError::SharedDependency(
                "observation clock is out of range".into(),
            ));
        }
        Ok(cutoff)
    }

    pub async fn due_keys(
        &self,
        cutoff: i64,
        bucket: Option<u8>,
    ) -> Result<Vec<(String, String)>, ObservationError> {
        let buckets = bucket.map_or(0..OBSERVATION_BUCKET_COUNT, |bucket| bucket..bucket + 1);
        due_query(cutoff, buckets)
            .build_query_as()
            .fetch_all(self.pool.get())
            .await
            .map_err(|_| {
                ObservationError::SharedDependency("observation due discovery failed".into())
            })
    }

    /// Per-Session CAS, not a bucket lease. A held Session never owns another
    /// Session's slot. The 90-second lease exceeds the bounded 60s attempt + 5s
    /// receipt budget; cancellation is recovered through expiry, not a scan.
    pub async fn claim(
        &self,
        user: &str,
        session: &str,
    ) -> Result<Option<ObservationCandidate>, ObservationError> {
        let lease_token = format!("{}:{}", self.owner, uuid::Uuid::new_v4());
        let mut connection = self.pool.get().acquire().await.map_err(|error| {
            ObservationError::database("observation claim connection failed", &error)
        })?;
        sqlx::query(CLAIM_SQL)
            .bind(&lease_token)
            .bind(user)
            .bind(session)
            .execute(&mut *connection)
            .await
            .map_err(|error| ObservationError::database("observation claim failed", &error))?;
        // Read the token back, rather than relying on affected_rows semantics.
        let Some(row) = sqlx::query(CANDIDATE_SQL)
            .bind(user)
            .bind(session)
            .bind(&lease_token)
            .fetch_optional(&mut *connection)
            .await
            .map_err(|error| {
                ObservationError::database("observation claim receipt failed", &error)
            })?
        else {
            return Ok(None);
        };
        let key = astra_turn_types::SessionKeyV1::owner_session("server", user, session, "main");
        let mut turn = u32::try_from(
            row.try_get::<i64, _>("completed_turn")
                .map_err(|_| "invalid observation turn")?,
        )
        .map_err(|_| "invalid observation turn")?;
        let mut head = row
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
        let checkpoint: ObservationCheckpoint = row
            .try_get::<Option<String>, _>("observation_json")
            .map_err(|_| "invalid observation checkpoint")?
            .map(|json| serde_json::from_str(&json))
            .transpose()
            .map_err(|_| "invalid observation checkpoint")?
            .unwrap_or_default();
        if let Some(progress) = &checkpoint.progress {
            if progress.head.key != key
                || progress.head.cursor.completed_turn > turn
                || progress.head.cursor.completed_turn <= checkpoint.observed_turn
                || head.as_ref().is_ok_and(|current| {
                    progress.head.cursor.compaction_generation
                        > current.cursor.compaction_generation
                })
            {
                return Err("invalid canonical observation progress head".into());
            }
            // A partial receipt pins the immutable captured range. A newer
            // Append/Replace remains due after this range is fully delivered.
            if head.is_ok() {
                head = Ok(progress.head.clone());
                turn = progress.head.cursor.completed_turn;
            }
        }
        Ok(Some(ObservationCandidate {
            key,
            turn,
            head,
            checkpoint,
            lease_token,
            next_memory_operation_segment: None,
            next_progress: None,
        }))
    }

    pub async fn materialize(
        &self,
        candidate: &ObservationCandidate,
    ) -> Result<MaterializedConversationDeltaV1, ObservationError> {
        let head = candidate.head.as_ref().map_err(Clone::clone)?;
        if candidate
            .checkpoint
            .observed_head
            .as_ref()
            .is_some_and(|base| base.cursor.completed_turn != candidate.checkpoint.observed_turn)
        {
            return Err("observation checkpoint head and watermark disagree".into());
        }
        let value = DatabaseSessionContextCoordinator::new(self.pool.clone())
            .materialize_observation(head, candidate.checkpoint.observed_head.as_ref())
            .await
            .map_err(|error| {
                use crate::session_context_coordinator::SessionContextCoordinatorError;
                let shared = matches!(&error,
                    SessionContextCoordinatorError::Database { source, .. } if shared_database_failure(source));
                let kind = match error {
                    SessionContextCoordinatorError::Database { operation, source } => {
                        let source_kind = match source {
                            sqlx::Error::PoolTimedOut => "pool_timeout",
                            sqlx::Error::PoolClosed => "pool_closed",
                            sqlx::Error::ColumnDecode { .. } | sqlx::Error::Decode(_) => "decode",
                            sqlx::Error::Io(_) => "io",
                            sqlx::Error::Database(_) => "database",
                            _ => "other",
                        };
                        format!("{operation}/{source_kind}")
                    }
                    SessionContextCoordinatorError::DatabaseJson { entity, .. } => {
                        format!("{entity}/json")
                    }
                    SessionContextCoordinatorError::DivergentManifest => {
                        "divergent_manifest".into()
                    }
                    SessionContextCoordinatorError::NeedsRepair(_) => "needs_repair".into(),
                    _ => "validation".into(),
                };
                // Preserve the failing phase without logging URLs, database
                // response text, stored content, or owner identifiers.
                let message = format!("observation canonical context could not be materialized ({kind})");
                if shared { ObservationError::SharedDependency(message) } else { ObservationError::Session(message) }
            })?;
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::CanonicalObservationRead,
            value.loaded_manifest_json_bytes,
            value.loaded_manifest_count,
            0,
        );
        Ok(value)
    }

    /// The checkpoint and next due state change in the same fenced head write.
    /// A completion racing this acknowledgement keeps the newer turn pending;
    /// completion preserves an existing retry deadline or in-flight lease.
    pub async fn finish(
        &self,
        candidate: &ObservationCandidate,
        success: bool,
    ) -> Result<(), ObservationError> {
        let mut checkpoint = candidate.checkpoint.clone();
        let receipt_id = uuid::Uuid::new_v4().to_string();
        checkpoint.receipt_id = Some(receipt_id.clone());
        checkpoint.revision = checkpoint
            .revision
            .checked_add(1)
            .ok_or("observation revision overflow")?;
        let complete = success && candidate.next_progress.is_none();
        if complete {
            checkpoint.observed_turn = checkpoint.observed_turn.max(candidate.turn);
            checkpoint.observed_head = candidate.head.as_ref().ok().cloned();
            // No payload is loaded on denial: conservatively suppress any
            // continuation until the next real human message opens a segment.
            checkpoint.memory_operation_segment =
                candidate.next_memory_operation_segment.unwrap_or(true);
            checkpoint.attempts = 0;
            checkpoint.progress = None;
        } else if success {
            checkpoint.progress = candidate.next_progress.clone();
            checkpoint.attempts = 0;
        } else {
            checkpoint.attempts = checkpoint.attempts.saturating_add(1);
        }
        let backoff_ms = if success {
            0
        } else {
            (5_000_i64 * (1_i64 << checkpoint.attempts.saturating_sub(1).min(8))).min(960_000)
        };
        self.persist(candidate, &checkpoint, Some((complete, backoff_ms)))
            .await
    }

    /// Save only accepted text progress, retaining the existing exact Session
    /// lease. A crash or an uncertain receipt can repeat delivery, never skip it.
    pub async fn checkpoint_progress(
        &self,
        candidate: &mut ObservationCandidate,
        cursor: window::ObservationTextCursor,
    ) -> Result<(), ObservationError> {
        let mut checkpoint = candidate.checkpoint.clone();
        checkpoint.revision = checkpoint
            .revision
            .checked_add(1)
            .ok_or("observation revision overflow")?;
        checkpoint.receipt_id = Some(uuid::Uuid::new_v4().to_string());
        checkpoint.progress = Some(ObservationProgress {
            head: candidate.head.as_ref().map_err(Clone::clone)?.clone(),
            cursor,
        });
        self.persist(candidate, &checkpoint, None).await?;
        candidate.next_progress = checkpoint.progress.clone();
        candidate.checkpoint = checkpoint;
        Ok(())
    }

    pub fn progress(
        candidate: &ObservationCandidate,
        cursor: window::ObservationTextCursor,
    ) -> Result<ObservationProgress, ObservationError> {
        Ok(ObservationProgress {
            head: candidate.head.as_ref().map_err(Clone::clone)?.clone(),
            cursor,
        })
    }

    async fn persist(
        &self,
        candidate: &ObservationCandidate,
        checkpoint: &ObservationCheckpoint,
        release: Option<(bool, i64)>,
    ) -> Result<(), ObservationError> {
        let json =
            serde_json::to_string(&checkpoint).map_err(|_| "invalid observation checkpoint")?;
        let mut connection = self.pool.get().acquire().await.map_err(|error| {
            ObservationError::database("observation checkpoint connection failed", &error)
        })?;
        if let Some((complete, backoff_ms)) = release {
            sqlx::query("UPDATE session_context_heads
            SET observation_json = ?,
                observation_due_key = CASE WHEN CAST(? AS SIGNED) <> 0 AND observation_target_turn <= ? THEN NULL
                    ELSE CAST((observation_due_key >> 58) << 58 AS SIGNED)
                         + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6)) + ? END,
                observation_lease_token = NULL
            WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'
              AND observation_lease_token = ? AND MOD(observation_due_key, 288230376151711744) > TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6))
              AND COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.revision')) AS UNSIGNED), 0) = ?")
            .bind(json).bind(complete).bind(candidate.turn).bind(backoff_ms * 1000)
            .bind(&candidate.key.owner_user_id).bind(&candidate.key.session_id).bind(&candidate.lease_token).bind(candidate.checkpoint.revision)
            .execute(&mut *connection).await.map_err(|error| ObservationError::database("observation checkpoint failed", &error))?;
        } else {
            sqlx::query("UPDATE session_context_heads SET observation_json = ?
                WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'
                  AND observation_lease_token = ? AND MOD(observation_due_key, 288230376151711744) > TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6))
                  AND COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.revision')) AS UNSIGNED), 0) = ?")
                .bind(json).bind(&candidate.key.owner_user_id).bind(&candidate.key.session_id)
                .bind(&candidate.lease_token).bind(candidate.checkpoint.revision)
                .execute(&mut *connection).await.map_err(|error| ObservationError::database("observation progress checkpoint failed", &error))?;
        }
        let stored: Option<String> = sqlx::query_scalar("SELECT JSON_UNQUOTE(JSON_EXTRACT(observation_json, '$.receipt_id'))
            FROM session_context_heads WHERE isolation_domain = 'server' AND owner_user_id = ? AND session_id = ? AND branch_id = 'main'")
            .bind(&candidate.key.owner_user_id).bind(&candidate.key.session_id).fetch_optional(&mut *connection).await
            .map_err(|error| ObservationError::database("observation checkpoint receipt unavailable", &error))?.flatten();
        if stored.as_deref() != checkpoint.receipt_id.as_deref() {
            return Err("observation checkpoint fence rejected or receipt superseded".into());
        }
        Ok(())
    }

    #[cfg(any(test, feature = "e2e-hooks"))]
    pub async fn expire_leases_for_test(&self) {
        sqlx::query("UPDATE session_context_heads SET observation_due_key = CAST((observation_due_key >> 58) << 58 AS SIGNED) + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', DATE_SUB(NOW(6), INTERVAL 1 SECOND)) WHERE observation_lease_token LIKE ?")
            .bind(format!("{}:%", self.owner)).execute(self.pool.get()).await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SessionContextCoordinator;

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
        let coordinator =
            DatabaseSessionContextCoordinator::new(pool.clone()).with_observation_enabled(true);
        let store = SessionObservationStore::new(pool.clone());
        let mut cursor = None;
        let mut captured = None;
        for turn in 1..=20 {
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
                            json!({"role":"assistant", "content": if turn == 14 { format!("turn-{turn} {}", "x".repeat(32768)) } else { format!("turn-{turn}") }}),
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
            if let Some(mut candidate) = captured.take() {
                let materialized = store.materialize(&candidate).await.unwrap_or_else(|error| {
                    panic!(
                        "captured turn {} after {} failed while committing {turn}: {error}",
                        candidate.turn, candidate.checkpoint.observed_turn
                    )
                });
                assert_eq!(materialized.head, *candidate.head.as_ref().unwrap());
                let expected_nodes = if turn == 11 { 3 } else { 1 };
                assert_eq!(materialized.messages.len(), expected_nodes);
                assert_eq!(
                    materialized.loaded_manifest_count,
                    (expected_nodes + materialized.prior_context.len()) as u64
                );
                assert!(materialized.prior_context.len() <= 4);
                if turn == 16 {
                    assert!(
                        materialized.prior_context.is_empty(),
                        "an oversized acknowledged turn must not enter the bounded context"
                    );
                    assert!(
                        materialized.loaded_manifest_json_bytes < 4096,
                        "SQL must exclude the large prior payload, not trim it after transfer"
                    );
                }
                if turn == 17 {
                    assert_eq!(
                        materialized.prior_context.len(),
                        1,
                        "bounded context resumes after the oversized turn"
                    );
                }
                if turn == 6 {
                    let head = candidate.head.as_ref().unwrap();
                    let base = candidate.checkpoint.observed_head.as_ref().unwrap();
                    let mut divergent = base.clone();
                    divergent.latest_manifest_root = "a".repeat(64);
                    assert!(
                        coordinator
                            .materialize_after(head, Some(&divergent))
                            .await
                            .is_err()
                    );
                    let mut corrupted = base.clone();
                    corrupted.total_message_count += 1;
                    assert!(
                        coordinator
                            .materialize_after(head, Some(&corrupted))
                            .await
                            .is_err()
                    );
                    assert!(
                        coordinator
                            .materialize_after(head, Some(head))
                            .await
                            .is_err()
                    );
                    let mut wrong_owner = base.clone();
                    wrong_owner.key.owner_user_id = "other-owner".into();
                    assert!(
                        coordinator
                            .materialize_after(head, Some(&wrong_owner))
                            .await
                            .is_err()
                    );
                }
                assert_eq!(
                    materialized.messages.last().unwrap()["content"],
                    if turn == 15 {
                        format!("turn-14 {}", "x".repeat(32768))
                    } else {
                        format!("turn-{}", turn - 1)
                    }
                );
                // SQL execution success is not a durable acknowledgement.
                sqlx::query("UPDATE session_context_heads SET observation_due_key = CAST((observation_due_key >> 58) << 58 AS SIGNED) + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', DATE_SUB(NOW(6), INTERVAL 1 SECOND)) WHERE session_id = 'captured-head'")
                    .execute(pool.get()).await.unwrap();
                assert!(
                    store.finish(&candidate, true).await.is_err(),
                    "an expired lease must not acknowledge an unstored checkpoint"
                );
                if turn == 2 {
                    assert!(
                        store
                            .checkpoint_progress(
                                &mut candidate,
                                window::ObservationTextCursor {
                                    part: 0,
                                    byte_offset: 1
                                }
                            )
                            .await
                            .is_err(),
                        "an expired lease cannot save a partial text receipt"
                    );
                }
                sqlx::query("UPDATE session_context_heads SET observation_due_key = CAST((observation_due_key >> 58) << 58 AS SIGNED) + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', DATE_ADD(NOW(6), INTERVAL 90 SECOND)) WHERE session_id = 'captured-head'")
                    .execute(pool.get()).await.unwrap();
                if turn == 2 {
                    let stale = candidate.clone();
                    store
                        .checkpoint_progress(
                            &mut candidate,
                            window::ObservationTextCursor {
                                part: 0,
                                byte_offset: 1,
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(candidate.checkpoint.observed_turn, 0);
                    assert!(candidate.checkpoint.progress.is_some());
                    assert!(
                        store.finish(&stale, true).await.is_err(),
                        "partial receipts must fence stale final acknowledgements"
                    );
                    assert!(
                        store
                            .claim(&key.owner_user_id, &key.session_id)
                            .await
                            .unwrap()
                            .is_none(),
                        "partial progress must retain the exact live lease"
                    );
                    candidate.next_progress = None;
                }
                store.finish(&candidate, true).await.unwrap();
                assert!(
                    store.finish(&candidate, false).await.is_err(),
                    "a stale revision must not acknowledge a rejected retry clock"
                );
            }
            // Finishing an older turn leaves new work pending. After a fully
            // acknowledged idle head, a later completion must re-arm it too.
            if turn == 8 || turn == 9 {
                // Simulate an idle/crashed consumer across several completions.
                // The next suffix must contain all three unacknowledged nodes.
                continue;
            }
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
            vec![json!({"role":"assistant", "content":"turn-20"})]
        );
        store.finish(&candidate, true).await.unwrap();
        assert!(
            store
                .claim(&key.owner_user_id, &key.session_id)
                .await
                .unwrap()
                .is_none()
        );
        let head = coordinator.load_head(&key).await.unwrap().unwrap();
        let full = coordinator.materialize(&head).await.unwrap();
        let large = format!("turn-14 {}", "x".repeat(32768));
        assert!(
            full.messages
                .iter()
                .any(|message| message["content"].as_str() == Some(large.as_str())),
            "typed lineage UNION must preserve complete large canonical payloads"
        );
        // Let a large backlog accumulate without observing intermediate turns.
        let mut cursor = head.cursor;
        for turn in 21..=62 {
            let reservation = match coordinator
                .acquire_writer_and_reserve_turn(
                    &key,
                    Some(&cursor),
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
            let content = if turn == 62 {
                "x".repeat(257 * 1024)
            } else {
                format!("bounded-turn-{turn}")
            };
            let result = coordinator
                .commit_turn_and_release_writer(
                    &reservation,
                    CanonicalTurnDeltaV1 {
                        schema_version: CANONICAL_TURN_DELTA_SCHEMA_VERSION,
                        completed_turn: turn,
                        journal_event_seq: u64::from(turn),
                        conversation_seq: u64::from(turn),
                        compaction_generation: 1,
                        config_version_id: None,
                        mode: CanonicalDeltaModeV1::Append,
                        logical_segments: vec![vec![json!({"role":"user", "content": content})]],
                    },
                    &format!("commit-{turn}"),
                )
                .await
                .unwrap();
            let CoordinatorMutationV1::Applied { cursor: next } = result else {
                panic!("unexpected commit: {result:?}");
            };
            cursor = next;
            if turn == 61 {
                let mut candidate = store
                    .claim(&key.owner_user_id, &key.session_id)
                    .await
                    .unwrap()
                    .unwrap();
                let tail = store.materialize(&candidate).await.unwrap();
                assert!(tail.observation_truncated);
                assert_eq!(tail.loaded_manifest_count, 32);
                assert!(tail.loaded_manifest_json_bytes < 64 * 1024);
                assert_eq!(tail.messages.first().unwrap()["content"], "bounded-turn-30");
                assert_eq!(tail.messages.last().unwrap()["content"], "bounded-turn-61");
                let receipt = window::ObservationTextCursor {
                    part: 12,
                    byte_offset: 0,
                };
                store
                    .checkpoint_progress(&mut candidate, receipt)
                    .await
                    .unwrap();
                store.finish(&candidate, false).await.unwrap();
                sqlx::query("UPDATE session_context_heads SET observation_due_key=CAST((observation_due_key >> 58) << 58 AS SIGNED) + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6)) WHERE owner_user_id=? AND session_id=?")
                    .bind(&key.owner_user_id).bind(&key.session_id).execute(pool.get()).await.unwrap();
                let restarted = SessionObservationStore::new(pool.clone());
                let mut resumed = restarted
                    .claim(&key.owner_user_id, &key.session_id)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    resumed.checkpoint.progress.as_ref().unwrap().cursor,
                    receipt
                );
                let replay = restarted.materialize(&resumed).await.unwrap();
                assert_eq!(replay.loaded_manifest_count, tail.loaded_manifest_count);
                assert_eq!(
                    replay.loaded_manifest_json_bytes,
                    tail.loaded_manifest_json_bytes
                );
                assert_eq!(replay.messages, tail.messages);
                resumed.next_progress = None;
                restarted.finish(&resumed, true).await.unwrap();
            }
        }
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
        .expect("the oversized commit must re-arm the completed idle head");
        assert_eq!(candidate.turn, 62);
        let oversized = store.materialize(&candidate).await.unwrap();
        assert!(oversized.observation_truncated);
        assert_eq!(oversized.loaded_manifest_count, 0);
        assert_eq!(oversized.loaded_manifest_json_bytes, 0);
        store.finish(&candidate, true).await.unwrap();

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
        sqlx::query("UPDATE session_context_heads SET observation_due_key=? + TIMESTAMPDIFF(MICROSECOND, '1970-01-01', NOW(6)) WHERE owner_user_id=? AND session_id=?")
            .bind(observation_bucket_prefix(&key)).bind(&key.owner_user_id).bind(&key.session_id).execute(pool.get()).await.unwrap();
        assert!(
            store
                .due_keys(store.discovery_cutoff().await.unwrap(), None)
                .await
                .unwrap()
                .contains(&(key.owner_user_id.clone(), key.session_id.clone()))
        );
        assert!(
            store
                .claim(&key.owner_user_id, &key.session_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !store
                .due_keys(store.discovery_cutoff().await.unwrap(), None)
                .await
                .unwrap()
                .contains(&(key.owner_user_id, key.session_id))
        );
    }

    async fn check_discovery_plan(
        pool: &SharedPool,
        store: &SessionObservationStore,
        cutoff: i64,
        bucket: Option<u8>,
        expected: usize,
        phase: &str,
    ) {
        let buckets = bucket.map_or(0..OBSERVATION_BUCKET_COUNT, |bucket| bucket..bucket + 1);
        let query = due_query(cutoff, buckets.clone());
        let args = buckets
            .flat_map(|bucket| {
                let prefix = i64::from(bucket) * BUCKET_WIDTH;
                [
                    serde_json::json!(prefix),
                    serde_json::json!(prefix + cutoff),
                ]
            })
            .collect::<Vec<_>>();
        let plan = prepared_plan(pool, &query.into_string(), &args).await;
        let plan_text = plan.join("\n");
        assert!(
            plan_text.contains(
                "Index Table Scan on session_context_heads.idx_session_context_observation"
            ),
            "{plan_text}"
        );
        assert!(
            plan_text.contains("Block Filter Cond:") && plan_text.contains("prefix_between"),
            "{plan_text}"
        );
        assert_eq!(plan_text.matches("Table Scan").count(), 1, "{plan_text}");
        assert_scans_bounded(&plan, 8192);
        let actual = store.due_keys(cutoff, bucket).await.unwrap().len();
        assert_eq!(actual, expected);
        let physical = plan_text
            .lines()
            .find(|line| line.contains("inputBlocks"))
            .unwrap();
        eprintln!(
            "observation discovery phase={phase} returned={actual} {}",
            physical.trim()
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
        let cutoff = store.discovery_cutoff().await.unwrap();
        check_discovery_plan(&pool, &store, cutoff, None, 0, "idle").await;

        // Heavy retry and lease populations must be cheap even when the index
        // contains every Session. Use one frozen DB cutoff for all four shapes.
        sqlx::query(
            "UPDATE session_context_heads SET observation_due_key =
                     MOD(CAST(session_id AS UNSIGNED), 16) * 288230376151711744 + ?",
        )
        .bind(cutoff + 600_000_000)
        .execute(pool.get())
        .await
        .unwrap();
        check_discovery_plan(&pool, &store, cutoff, None, 0, "future").await;
        check_discovery_plan(&pool, &store, cutoff, Some(5), 0, "future bucket").await;

        for session in ["00000001", "00050032", "00100000"] {
            sqlx::query(
                "UPDATE session_context_heads SET observation_due_key=?, observation_json=NULL
                         WHERE owner_user_id='synthetic-owner' AND session_id=?",
            )
            .bind(5 * BUCKET_WIDTH + cutoff)
            .bind(session)
            .execute(pool.get())
            .await
            .unwrap();
        }
        check_discovery_plan(&pool, &store, cutoff, None, 3, "sparse due").await;
        check_discovery_plan(&pool, &store, cutoff, Some(5), 3, "sparse due bucket").await;
        check_discovery_plan(&pool, &store, cutoff, Some(6), 0, "other bucket").await;

        // Query the same shape at the future deadline to emulate all 100,000
        // Sessions becoming due. Physical scan input and LIMIT are independent.
        check_discovery_plan(&pool, &store, cutoff + 600_000_000, None, 8, "dense due").await;
        check_discovery_plan(
            &pool,
            &store,
            cutoff + 600_000_000,
            Some(5),
            8,
            "dense due bucket",
        )
        .await;
        let direct_key = serde_json::json!(["server", "synthetic-owner", "00050032", "main"]);
        let direct_key = direct_key.as_array().unwrap();
        for query in [
            "SELECT head_json FROM session_context_heads WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ?",
            "SELECT 1 FROM session_context_heads WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ? FOR UPDATE",
            "UPDATE session_context_heads SET head_json = '{}' WHERE isolation_domain = ? AND owner_user_id = ? AND session_id = ? AND branch_id = ?",
        ] {
            let plan = prepared_plan(&pool, query, direct_key).await;
            assert_scans_bounded(&plan, 1);
            let plan = plan.join("\n");
            assert_eq!(plan.matches("Table Scan").count(), 1, "{plan}");
            assert!(plan.contains("__mo_cpkey_col"), "{plan}");
        }
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
        // Claim and deletion retirement update the covering due index without
        // changing the physical Session key.
        let claim = prepared_plan(
            &pool,
            CLAIM_SQL,
            &[
                serde_json::json!("plan-token"),
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
        // A long fork returns lineage once, not once per history node. Measure
        // actual result values; this is deliberately not a network-byte claim.
        sqlx::query("INSERT INTO session_forks (isolation_domain,owner_user_id,fork_id,parent_session_id,parent_branch_id,child_session_id,child_branch_id,idempotency_hash,request_hash,state,manifest_json)
            VALUES ('server','synthetic-owner','long-fork','parent','main','retained-generations','main',REPEAT('a',64),REPEAT('b',64),'active',?)")
            .bind("x".repeat(2048)).execute(pool.get()).await.unwrap();
        let rows = sqlx::query(sqlx::AssertSqlSafe(
            crate::session_context_coordinator::canonical_materialization_sql(),
        ))
        .bind("server")
        .bind("synthetic-owner")
        .bind("retained-generations")
        .bind("main")
        .bind(99_i64)
        .bind(100000_i64)
        .bind("server")
        .bind("synthetic-owner")
        .bind("retained-generations")
        .bind("main")
        .fetch_all(pool.get())
        .await
        .unwrap();
        let lineage: Vec<_> = rows
            .iter()
            .filter_map(|row| {
                row.try_get::<Option<String>, _>("fork_manifest_json")
                    .unwrap()
            })
            .collect();
        assert_eq!(lineage.len(), 1);
        assert_eq!(lineage.iter().map(String::len).sum::<usize>(), 2048);
        assert_eq!(
            rows.iter()
                .filter(|row| row
                    .try_get::<Option<String>, _>("manifest_root")
                    .unwrap()
                    .is_some())
                .count(),
            1000
        );
        // Same-generation history must be pruned at the sequence boundary too.
        // Selecting only the last suffix row must not scan all 100,000 nodes.
        // Seed a separate range instead of rewriting 100,000 physical primary
        // keys. Fixture setup must not become an unrelated update benchmark.
        sqlx::query("INSERT INTO conversation_manifest_nodes (isolation_domain,owner_user_id,session_id,branch_id,manifest_root,completed_turn,conversation_seq,compaction_generation,canonical_segment_bytes,total_canonical_bytes,total_message_count,manifest_json,content_json)
             SELECT 'server','synthetic-owner','same-generation','main',LPAD(CAST(result + 200000 AS CHAR),64,'0'),1,result,0,1,1,1,'{}','[]' FROM generate_series(1,100000) g")
            .execute(pool.get()).await.unwrap();
        let suffix = prepared_plan(
            &pool,
            &crate::session_context_coordinator::canonical_suffix_materialization_sql(),
            serde_json::json!([
                "server",
                "synthetic-owner",
                "same-generation",
                "main",
                0,
                99999,
                100000
            ])
            .as_array()
            .unwrap(),
        )
        .await;
        assert_scans_bounded(&suffix, 8192);
        let context_plan = prepared_plan(
            &pool,
            &crate::session_context_coordinator::canonical_observation_materialization_sql(),
            serde_json::json!([
                "server",
                "synthetic-owner",
                "same-generation",
                "main",
                0,
                99995,
                100000,
                99999,
                0
            ])
            .as_array()
            .unwrap(),
        )
        .await;
        assert_scans_bounded(&context_plan, 8192);
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
