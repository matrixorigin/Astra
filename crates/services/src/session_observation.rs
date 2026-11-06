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

    /// One bounded Session per query; the process owner drains at most sixteen
    /// per tick. New turns do not touch the independent persisted retry clock.
    pub async fn next(
        &self,
        lease_name: &str,
        pod_id: &str,
        scope: Option<(&str, &str)>,
        after: Option<(&str, &str)>,
    ) -> Result<Option<ObservationCandidate>, String> {
        let columns = qualified_manifest_content_columns("m");
        let sql = format!(
            "SELECT h.head_json, h.observation_json, h.isolation_domain, h.owner_user_id, h.session_id, h.branch_id, h.completed_turn AS head_completed_turn, {columns},
                    f.manifest_json AS fork_manifest_json
             FROM (
               SELECT h.*, CAST(JSON_EXTRACT(w.state_json, '$.observation') AS CHAR) AS observation_json
               FROM session_context_heads h
               JOIN agent_sessions s ON s.user_id = h.owner_user_id AND s.session_id = h.session_id
               LEFT JOIN agent_session_memory_work w ON w.user_id = h.owner_user_id AND w.session_id = h.session_id
               WHERE s.delete_requested_at IS NULL AND h.head_json IS NOT NULL AND h.completed_turn > 0
                 AND h.isolation_domain = 'server' AND h.branch_id = ?
                 AND (? IS NULL OR h.owner_user_id = ?) AND (? IS NULL OR h.session_id = ?)
                 AND (? IS NULL OR h.owner_user_id > ? OR (h.owner_user_id = ? AND h.session_id > ?))
                 AND h.completed_turn > COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(w.state_json, '$.observation.observed_turn')) AS SIGNED), 0)
                 AND COALESCE(CAST(JSON_UNQUOTE(JSON_EXTRACT(w.state_json, '$.observation.retry_at_ms')) AS SIGNED), 0)
                     <= CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED)
                 AND EXISTS (SELECT 1 FROM sweeper_leases l WHERE l.sweeper_name = ?
                             AND l.owner_pod_id = ? AND l.expires_at > NOW(6))
               ORDER BY h.owner_user_id, h.session_id LIMIT 1
             ) h
             LEFT JOIN conversation_manifest_nodes m
               ON m.isolation_domain = h.isolation_domain AND m.owner_user_id = h.owner_user_id
              AND m.session_id = h.session_id AND m.branch_id = h.branch_id
              AND m.compaction_generation = h.compaction_generation AND m.reachable = 1
              AND m.conversation_seq <= h.conversation_seq
             LEFT JOIN session_forks f
               ON f.isolation_domain = h.isolation_domain AND f.owner_user_id = h.owner_user_id
              AND f.child_session_id = h.session_id AND f.child_branch_id = h.branch_id AND f.state = 'active'"
        );
        let query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(astra_turn_types::DEFAULT_CONVERSATION_BRANCH_ID)
            .bind(scope.map(|(user, _)| user))
            .bind(scope.map(|(user, _)| user))
            .bind(scope.map(|(_, session)| session))
            .bind(scope.map(|(_, session)| session))
            .bind(after.map(|(user, _)| user))
            .bind(after.map(|(user, _)| user))
            .bind(after.map(|(user, _)| user))
            .bind(after.map(|(_, session)| session))
            .bind(lease_name)
            .bind(pod_id);
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
