//! Durable session-memory work owned by the canonical session lifecycle.
//! Request publication precedes turn completion. Short checkpoints and bounded
//! destructive cleanup take the existing session write fence, shared by all pods
//! and turn admission; a lease selects a worker, never grants user authority.

use crate::CancellationSafePoolConnection;
use astra_core::SharedPool;
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Connection, MySql, Row, Transaction, mysql::MySqlRow, types::Json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionMemoryWork {
    /// Transient projection from the locked canonical Session row.
    #[serde(skip)]
    pub session_closed: bool,
    #[serde(skip)]
    pub canonical_turn: u32,
    /// Latest input not yet owned by an extraction attempt.
    pub pending_request: Option<Value>,
    /// Publication metadata survives retirement of the transcript.
    pub latest_turn: Option<u32>,
    pub latest_facts: Option<Value>,
    pub latest_fingerprint: Option<String>,
    pub completed_fingerprint: Option<String>,
    pub completed_turn: Option<u32>,
    pub active_request: Option<Value>,
    pub active_fingerprint: Option<String>,
    /// Frozen before HTTP dispatch so crash recovery never synthesizes a
    /// different payload for an uncertain write.
    pub prepared_working: Option<Value>,
    /// Receipt of the working snapshot actually read/written by the settled attempt.
    pub working_memory_id: Option<String>,
    pub extraction_requested: bool,
    pub finalize_requested: bool,
    /// CLI exit targets its idle generation; explicit close settles all already-admitted work.
    pub finalize_turn: Option<u32>,
    /// Frozen snapshot and facts used by every retry of this generation.
    pub plan: Option<Value>,
    pub receipts: BTreeMap<String, String>,
    pub purged_ids: BTreeSet<String>,
    pub reflection: Option<Value>,
    pub consolidation_complete: bool,
    pub optional_complete: bool,
    pub last_report: Option<Value>,
}

impl SessionMemoryWork {
    pub fn extraction_pending(&self) -> bool {
        self.active_request.is_some()
            || ((self.extraction_requested || self.finalize_requested)
                && self.latest_fingerprint != self.completed_fingerprint)
    }
    pub fn pending(&self) -> bool {
        self.extraction_pending() || (self.finalize_requested && !self.optional_complete)
    }
    pub fn reset_plan(&mut self) {
        self.plan = None;
        self.receipts.clear();
        self.purged_ids.clear();
        self.reflection = None;
        self.consolidation_complete = false;
        self.optional_complete = false;
        self.last_report = None;
    }
}

#[derive(Clone)]
pub struct SessionMemoryLifecycleStore {
    pool: SharedPool,
}

#[derive(Debug, Clone)]
pub struct SessionMemoryWorkClaim {
    pub user_id: String,
    pub session_id: String,
    pub token: String,
    pub work: SessionMemoryWork,
}

enum Persistence {
    Inspect,
    Update,
    Finish { success: bool },
}

fn release_schedule(success: bool) -> (&'static str, &'static str) {
    if success {
        ("NOW(6)", "0")
    } else {
        (
            "DATE_ADD(NOW(6), INTERVAL LEAST(960, 5 * POW(2, LEAST(GREATEST(attempts - 1, 0), 8))) SECOND)",
            "attempts",
        )
    }
}

impl SessionMemoryLifecycleStore {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    pub async fn publish(
        &self,
        user: &str,
        session: &str,
        request: Value,
        fingerprint: String,
    ) -> Result<(), String> {
        self.with_fenced_state(user, session, None, false, move |work| {
            Box::pin(async move {
                if work.session_closed && work.consolidation_complete && work.finalize_requested {
                    return Err("closed generation is already consolidated".into());
                }
                let turn = request["inference_scope"]["turn"]
                    .as_u64()
                    .or_else(|| request["session_facts"]["turn"].as_u64())
                    .ok_or("missing extraction turn")?;
                let turn = u32::try_from(turn)
                    .map_err(|_| "extraction turn exceeds supported memory coordinate")?;
                if work.latest_turn.is_some_and(|previous| turn < previous) {
                    return Ok(());
                }
                if work.latest_fingerprint.as_ref() != Some(&fingerprint) {
                    work.latest_turn = Some(turn);
                    work.latest_facts = Some(request["session_facts"].clone());
                    work.pending_request = Some(request);
                    work.latest_fingerprint = Some(fingerprint);
                    // A follow-up turn cancels stale exit maintenance. Explicit
                    // close rejects new requests at the session fence below.
                    if !work.session_closed && work.finalize_turn.is_none_or(|target| turn > target)
                    {
                        work.finalize_requested = false;
                        work.finalize_turn = None;
                    }
                    work.reset_plan();
                }
                Ok(())
            })
        })
        .await
    }

    /// Repeated exit/close requests preserve stage receipts and retry progress.
    pub async fn request_finalization(&self, user: &str, session: &str) -> Result<(), String> {
        self.with_fenced_state(user, session, None, true, |work| {
            Box::pin(async move {
                if work.canonical_turn > work.completed_turn.unwrap_or(0) {
                    work.reset_plan();
                }
                work.finalize_requested = true;
                let latest_turn = work.latest_turn.unwrap_or(0);
                work.finalize_turn = Some(work.canonical_turn.max(latest_turn));
                Ok(())
            })
        })
        .await
    }

    pub async fn request_extraction(&self, user: &str, session: &str) -> Result<(), String> {
        self.with_fenced_state(user, session, None, false, |work| {
            Box::pin(async move {
                work.extraction_requested = true;
                Ok(())
            })
        })
        .await
    }

    /// Read immutable canonical context only when a handoff was missed. It is
    /// materialized by the existing coordinator, never from transcript/audit projections.
    pub async fn missing_input_head(
        &self,
        user: &str,
        session: &str,
    ) -> Result<Option<astra_turn_types::SessionContextHeadV1>, String> {
        let row = sqlx::query("SELECT w.state_json, h.head_json FROM agent_session_memory_work w JOIN session_context_heads h ON h.owner_user_id = w.user_id AND h.session_id = w.session_id WHERE w.user_id = ? AND w.session_id = ? AND h.head_json IS NOT NULL ORDER BY h.completed_turn DESC, h.updated_at DESC LIMIT 1")
            .bind(user).bind(session).fetch_optional(self.pool.get()).await.map_err(|error| error.to_string())?;
        let Some(row) = row else {
            return Ok(None);
        };
        let work = decode_work(&row)?;
        let head: astra_turn_types::SessionContextHeadV1 = serde_json::from_str(
            &row.try_get::<String, _>("head_json")
                .map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        if head.key.owner_user_id != user || head.key.session_id != session {
            return Err("canonical recovery head ownership mismatch".into());
        }
        Ok((head.cursor.completed_turn > work.latest_turn.unwrap_or(0)).then_some(head))
    }

    pub async fn publish_recovery(
        &self,
        claim: &SessionMemoryWorkClaim,
        request: Value,
        fingerprint: String,
        turn: u32,
    ) -> Result<(), String> {
        self.with_fenced_state(
            &claim.user_id,
            &claim.session_id,
            Some(&claim.token),
            true,
            move |work| {
                Box::pin(async move {
                    if work.canonical_turn != turn {
                        return Err("canonical recovery generation changed".into());
                    }
                    work.latest_turn = Some(turn);
                    work.latest_facts = Some(request["session_facts"].clone());
                    work.pending_request = Some(request);
                    work.latest_fingerprint = Some(fingerprint);
                    work.extraction_requested = true;
                    if !work.session_closed
                        && work.finalize_turn.is_some_and(|target| turn > target)
                    {
                        work.finalize_requested = false;
                        work.finalize_turn = None;
                    }
                    work.reset_plan();
                    if work.active_request.is_none() {
                        work.active_request = work.pending_request.take();
                        work.active_fingerprint = work.latest_fingerprint.clone();
                    }
                    Ok(())
                })
            },
        )
        .await
    }

    pub async fn load_claim_work(
        &self,
        claim: &SessionMemoryWorkClaim,
    ) -> Result<SessionMemoryWork, String> {
        self.inspect_fenced_state(
            &claim.user_id,
            &claim.session_id,
            &claim.token,
            false,
            |work| Box::pin(async move { Ok(work.clone()) }),
        )
        .await
    }

    pub async fn status(&self, user: &str, session: &str) -> Result<Value, String> {
        let row = sqlx::query("SELECT state_json, attempts, CAST(next_attempt_at AS CHAR) AS next_attempt_at FROM agent_session_memory_work WHERE user_id = ? AND session_id = ?")
            .bind(user).bind(session).fetch_optional(self.pool.get()).await.map_err(|error| error.to_string())?;
        let Some(row) = row else {
            return Ok(serde_json::json!({"requested": false, "session_id":session}));
        };
        let work = decode_work(&row)?;
        Ok(
            serde_json::json!({"session_id":session, "requested":work.finalize_requested,
            "pending":work.pending(), "extraction_pending":work.extraction_pending(),
            "completed_turn":work.completed_turn, "consolidation_complete":work.consolidation_complete,
            "optional_complete":work.optional_complete, "last_report":work.last_report,
            "attempts":row.try_get::<i64,_>("attempts").map_err(|error| error.to_string())?,
            "next_attempt_at":row.try_get::<String,_>("next_attempt_at").map_err(|error| error.to_string())?}),
        )
    }

    pub async fn list_due(&self, limit: u16) -> Result<Vec<(String, String)>, String> {
        let rows = sqlx::query("SELECT w.user_id, w.session_id FROM agent_session_memory_work w JOIN agent_sessions s ON s.user_id = w.user_id AND s.session_id = w.session_id WHERE s.delete_requested_at IS NULL AND w.complete = 0 AND w.next_attempt_at <= NOW(6) AND (w.lease_token IS NULL OR w.lease_expires_at <= NOW(6)) ORDER BY w.next_attempt_at, w.user_id, w.session_id LIMIT ?")
            .bind(limit).fetch_all(self.pool.get()).await.map_err(|error| error.to_string())?;
        rows.into_iter()
            .map(|row| {
                Ok((
                    row.try_get("user_id").map_err(|error| error.to_string())?,
                    row.try_get("session_id")
                        .map_err(|error| error.to_string())?,
                ))
            })
            .collect()
    }

    pub async fn claim(
        &self,
        user: &str,
        session: &str,
    ) -> Result<Option<SessionMemoryWorkClaim>, String> {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|error| error.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|error| error.to_string())?;
        lock_session(&mut tx, user, session).await?;
        let Some(row) = sqlx::query("SELECT state_json, (lease_token IS NULL OR lease_expires_at <= NOW(6)) AS available FROM agent_session_memory_work WHERE user_id = ? AND session_id = ? AND complete = 0 AND next_attempt_at <= NOW(6) FOR UPDATE")
            .bind(user).bind(session).fetch_optional(&mut *tx).await.map_err(|error| error.to_string())? else { return Ok(None); };
        if row
            .try_get::<i64, _>("available")
            .map_err(|error| error.to_string())?
            == 0
        {
            return Ok(None);
        }
        let mut work = decode_work(&row)?;
        if work.extraction_pending() && work.active_request.is_none() {
            work.active_request = work.pending_request.take();
            work.active_fingerprint = work.latest_fingerprint.clone();
        }
        let token = uuid::Uuid::new_v4().to_string();
        sqlx::query("UPDATE agent_session_memory_work SET state_json = ?, lease_token = ?, lease_expires_at = DATE_ADD(NOW(6), INTERVAL 90 SECOND), attempts = attempts + 1 WHERE user_id = ? AND session_id = ?")
            .bind(serde_json::to_string(&work).map_err(|error| error.to_string())?).bind(&token).bind(user).bind(session).execute(&mut *tx).await.map_err(|error| error.to_string())?;
        tx.commit().await.map_err(|error| error.to_string())?;
        connection.release();
        Ok(Some(SessionMemoryWorkClaim {
            user_id: user.into(),
            session_id: session.into(),
            token,
            work,
        }))
    }

    /// The callback may perform one bounded HTTP operation while the canonical
    /// session fence is held. This prevents turn admission, close, delete and
    /// another worker from crossing a destructive checkpoint mid-request.
    pub async fn with_fenced_state<R: Send, F>(
        &self,
        user: &str,
        session: &str,
        token: Option<&str>,
        require_idle: bool,
        f: F,
    ) -> Result<R, String>
    where
        F: for<'a> FnOnce(&'a mut SessionMemoryWork) -> BoxFuture<'a, Result<R, String>> + Send,
    {
        self.fenced_state(user, session, token, require_idle, Persistence::Update, f)
            .await
    }

    /// Inspect under the same authority and lease fence without rewriting or
    /// rescheduling the journal. The callback cannot mutate durable state.
    pub async fn inspect_fenced_state<R: Send, F>(
        &self,
        user: &str,
        session: &str,
        token: &str,
        require_idle: bool,
        f: F,
    ) -> Result<R, String>
    where
        F: for<'a> FnOnce(&'a SessionMemoryWork) -> BoxFuture<'a, Result<R, String>> + Send,
    {
        self.fenced_state(
            user,
            session,
            Some(token),
            require_idle,
            Persistence::Inspect,
            move |work| f(work),
        )
        .await
    }

    /// Commit the final checkpoint and its lease release as one transition.
    pub async fn finish_claim<R: Send, F>(
        &self,
        claim: &SessionMemoryWorkClaim,
        require_idle: bool,
        success: bool,
        f: F,
    ) -> Result<R, String>
    where
        F: for<'a> FnOnce(&'a mut SessionMemoryWork) -> BoxFuture<'a, Result<R, String>> + Send,
    {
        self.fenced_state(
            &claim.user_id,
            &claim.session_id,
            Some(&claim.token),
            require_idle,
            Persistence::Finish { success },
            f,
        )
        .await
    }

    async fn fenced_state<R: Send, F>(
        &self,
        user: &str,
        session: &str,
        token: Option<&str>,
        require_idle: bool,
        persistence: Persistence,
        f: F,
    ) -> Result<R, String>
    where
        F: for<'a> FnOnce(&'a mut SessionMemoryWork) -> BoxFuture<'a, Result<R, String>> + Send,
    {
        let mut connection = CancellationSafePoolConnection::acquire(self.pool.get())
            .await
            .map_err(|error| error.to_string())?;
        let mut tx = connection
            .connection_mut()
            .begin()
            .await
            .map_err(|error| error.to_string())?;
        let status = lock_session(&mut tx, user, session).await?;

        if require_idle {
            ensure_idle(&mut tx, user, session).await?;
        }
        if token.is_none() {
            ensure_work_row(&mut tx, user, session).await?;
        }
        let row = sqlx::query("SELECT state_json, lease_token, (lease_expires_at > NOW(6)) AS lease_live FROM agent_session_memory_work WHERE user_id = ? AND session_id = ? FOR UPDATE")
            .bind(user).bind(session).fetch_one(&mut *tx).await.map_err(|error| error.to_string())?;
        if let Some(token) = token
            && (row
                .try_get::<Option<String>, _>("lease_token")
                .map_err(|error| error.to_string())?
                .as_deref()
                != Some(token)
                || row
                    .try_get::<Option<i64>, _>("lease_live")
                    .map_err(|error| error.to_string())?
                    != Some(1))
        {
            return Err("session memory worker lease was superseded".into());
        }
        let mut work = decode_work(&row)?;
        work.session_closed = status == "closed";
        if require_idle {
            work.canonical_turn = canonical_turn_tx(&mut tx, user, session).await?;
        }
        let result = f(&mut work).await?;
        if !matches!(persistence, Persistence::Inspect) {
            let scheduling = match persistence {
                Persistence::Finish { success } => {
                    if success
                        && work.optional_complete
                        && work.consolidation_complete
                        && !work.extraction_pending()
                        && work.prepared_working.is_none()
                    {
                        work.plan = None;
                        work.receipts.clear();
                        work.purged_ids.clear();
                        work.reflection = None;
                    }
                    let (due, attempts) = release_schedule(success);
                    format!(
                        "lease_token = NULL, lease_expires_at = NULL, next_attempt_at = {due}, attempts = {attempts}"
                    )
                }
                _ => "next_attempt_at = NOW(6)".into(),
            };
            let query = format!(
                "UPDATE agent_session_memory_work SET state_json = ?, complete = ?, {scheduling}, updated_at = NOW(6) WHERE user_id = ? AND session_id = ?"
            );
            sqlx::query(sqlx::AssertSqlSafe(query))
                .bind(serde_json::to_string(&work).map_err(|error| error.to_string())?)
                .bind(!work.pending())
                .bind(user)
                .bind(session)
                .execute(&mut *tx)
                .await
                .map_err(|error| error.to_string())?;
        }
        tx.commit().await.map_err(|error| error.to_string())?;
        connection.release();
        Ok(result)
    }

    pub async fn finish_extraction(
        &self,
        claim: &SessionMemoryWorkClaim,
        success: bool,
        turn: u32,
    ) -> Result<(), String> {
        self.finish_claim(claim, false, success, move |work| {
            Box::pin(async move {
                if success {
                    work.completed_fingerprint = work.active_fingerprint.take();
                    work.completed_turn = Some(turn);
                    work.active_request = None;
                    work.prepared_working = None;
                    work.extraction_requested =
                        work.latest_fingerprint != work.completed_fingerprint;
                }
                Ok(())
            })
        })
        .await
    }

    pub async fn release(
        &self,
        claim: &SessionMemoryWorkClaim,
        success: bool,
    ) -> Result<(), String> {
        // A failure backs off but remains durable; successful progress is due
        // immediately for the next stage. Claim expiry recovers process death.
        let (due, attempts) = release_schedule(success);
        let query = format!(
            "UPDATE agent_session_memory_work SET lease_token = NULL, lease_expires_at = NULL, next_attempt_at = {due}, attempts = {attempts}, updated_at = NOW(6) WHERE user_id = ? AND session_id = ? AND lease_token = ?"
        );
        sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(&claim.user_id)
            .bind(&claim.session_id)
            .bind(&claim.token)
            .execute(self.pool.get())
            .await
            .map_err(|error| error.to_string())?;
        Ok(())
    }
}

// Decode the native JSON column. MatrixOne's unbounded CHAR cast defaults to
// 65535 bytes and silently truncates realistic extraction inputs/frozen plans.
fn decode_work(row: &MySqlRow) -> Result<SessionMemoryWork, String> {
    row.try_get::<Json<SessionMemoryWork>, _>("state_json")
        .map(|Json(work)| work)
        .map_err(|error| error.to_string())
}

async fn lock_session(
    tx: &mut Transaction<'_, MySql>,
    user: &str,
    session: &str,
) -> Result<String, String> {
    crate::storage::lock_agent_session_write_fence(tx, session, user)
        .await
        .map_err(|error| error.to_string())?;
    sqlx::query_scalar("SELECT status FROM agent_sessions WHERE user_id = ? AND session_id = ? AND delete_requested_at IS NULL FOR UPDATE")
        .bind(user).bind(session).fetch_optional(&mut **tx).await.map_err(|error| error.to_string())?.ok_or_else(|| "session memory work requires an owned, retained session".into())
}

async fn ensure_idle(
    tx: &mut Transaction<'_, MySql>,
    user: &str,
    session: &str,
) -> Result<(), String> {
    if !execution_is_idle(tx, user, session).await? {
        return Err("session memory finalization waits for execution settlement".into());
    }
    Ok(())
}

async fn execution_is_idle(
    tx: &mut Transaction<'_, MySql>,
    user: &str,
    session: &str,
) -> Result<bool, String> {
    let slot = sqlx::query(
        "SELECT 1 FROM agent_session_execution_slots WHERE user_id = ? AND session_id = ? LIMIT 1 FOR UPDATE",
    )
    .bind(user)
    .bind(session)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| error.to_string())?;
    if slot.is_some() {
        return Ok(false);
    }
    // Terminal Run persistence releases the execution slot before canonical
    // context commit. The coordinator's existing writer/reservation authority
    // still permits that commit, so neither close nor destructive maintenance
    // may treat the old watermark as settled. Use database time and current
    // reads under the shared Session fence, just like the context coordinator.
    let context_writer = sqlx::query(
        "SELECT 1 FROM session_context_heads WHERE owner_user_id = ? AND session_id = ?
         AND (active_writer_expires_at_ms > CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED)
              OR active_reservation_expires_at_ms > CAST(UNIX_TIMESTAMP(NOW(6)) * 1000 AS SIGNED))
         LIMIT 1 FOR UPDATE",
    )
    .bind(user)
    .bind(session)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|error| error.to_string())?;
    Ok(context_writer.is_none())
}

async fn ensure_work_row(
    tx: &mut Transaction<'_, MySql>,
    user: &str,
    session: &str,
) -> Result<(), String> {
    sqlx::query("INSERT IGNORE INTO agent_session_memory_work (user_id, session_id, state_json, complete) VALUES (?, ?, '{}', 1)")
        .bind(user).bind(session).execute(&mut **tx).await.map_err(|error| error.to_string())?;
    Ok(())
}

/// Called inside the session service's close transaction, after its row lock.
pub(crate) async fn request_finalization_tx(
    tx: &mut Transaction<'_, MySql>,
    user: &str,
    session: &str,
) -> Result<(), String> {
    // Closing may occur while an extraction is pending; the recovery worker
    // settles that exact durable generation before constructing a purge plan.
    // The first bounded handoff may have failed before creating any journal.
    // Persist close intent atomically even then, so recovery can discover the
    // session and reconstruct its input from the canonical context head.
    ensure_work_row(tx, user, session).await?;
    let Json(mut work) = sqlx::query_scalar::<_, Json<SessionMemoryWork>>("SELECT state_json FROM agent_session_memory_work WHERE user_id = ? AND session_id = ? FOR UPDATE")
        .bind(user).bind(session).fetch_one(&mut **tx).await.map_err(|error| error.to_string())?;
    work.canonical_turn = canonical_turn_tx(tx, user, session).await?;
    // An admitted turn may not have published its memory request or canonical
    // watermark yet. The shared fence stops new admission, but cannot erase
    // that existing execution debt. Invalidate the prior exit completion so
    // its late handoff (or canonical recovery after restart) remains runnable.
    if work.canonical_turn > work.completed_turn.unwrap_or(0)
        || !execution_is_idle(tx, user, session).await?
    {
        work.reset_plan();
    }
    work.finalize_requested = true;
    work.finalize_turn = None;
    sqlx::query("UPDATE agent_session_memory_work SET state_json = ?, complete = ?, next_attempt_at = NOW(6) WHERE user_id = ? AND session_id = ?")
        .bind(serde_json::to_string(&work).map_err(|error| error.to_string())?).bind(!work.pending()).bind(user).bind(session).execute(&mut **tx).await.map_err(|error| error.to_string())?;
    Ok(())
}

async fn canonical_turn_tx(
    tx: &mut Transaction<'_, MySql>,
    user: &str,
    session: &str,
) -> Result<u32, String> {
    let turn:i64=sqlx::query_scalar("SELECT COALESCE(MAX(completed_turn), 0) FROM session_context_heads WHERE owner_user_id = ? AND session_id = ?")
        .bind(user).bind(session).fetch_one(&mut **tx).await.map_err(|error|error.to_string())?;
    u32::try_from(turn).map_err(|_| "canonical turn exceeds supported memory coordinate".into())
}
