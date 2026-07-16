//! MatrixOne-backed durable tool invocation ledger.
//!
//! This store owns persistence and atomic compare-and-set. Semantic result
//! caching is deliberately outside this module.

use std::collections::BTreeMap;

use astra_core::SharedPool;
use astra_turn_types::{
    DispatchCertainty, ToolInvocationCompletionSource, ToolInvocationDecision,
    ToolInvocationDispatchLease, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationResultPayload,
    ToolInvocationState, ToolInvocationTerminalOutcome,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;
use uuid::Uuid;

const TOOL_INVOCATION_ARCHIVE_VERSION: &str = "tool-invocation-run-archive-v1";
const TOOL_INVOCATION_COMPACTION_BATCH_RECORDS: i64 = 32;
const TOOL_INVOCATION_ARCHIVE_CHUNK_MAX_BYTES: usize = 4 * 1024 * 1024;
const TOOL_INVOCATION_ARCHIVE_RETENTION_DAYS: i64 = 30;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocationCompactionOutcome {
    pub archived_records: usize,
    pub remaining_records: u64,
    pub artifact_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocationRunReconciliationOutcome {
    pub prepared_rejected: u64,
    pub inconsistent_prepared_unknown: u64,
    pub expired_dispatches_unknown: u64,
    pub active_dispatches_remaining: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolInvocationLifecycleDiagnostics {
    pub run_id: Option<String>,
    pub hot_total: u64,
    pub prepared: u64,
    pub dispatched: u64,
    pub succeeded: u64,
    pub failed: u64,
    pub rejected: u64,
    pub outcome_unknown: u64,
    pub rejected_without_dispatch: u64,
    pub archive_chunks: u64,
    pub durable_artifact_references: u64,
    pub reconciliation_events: u64,
    pub compaction_deferred_events: u64,
    pub compaction_cursor_generation: Option<u64>,
    pub compaction_cursor_updated_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ToolInvocationArchiveChunk {
    version: String,
    user_id: String,
    session_id: String,
    run_id: String,
    records: Vec<ToolInvocationRecord>,
}

#[derive(Clone)]
pub struct DatabaseToolInvocationLedger {
    pool: SharedPool,
}

impl DatabaseToolInvocationLedger {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Return an existing identity or insert `Prepared` idempotently.
    ///
    /// Replay is independent of run admission: a terminal run may still own
    /// an authoritative hot or archived result. Only creation of a new
    /// identity is serialized with the run closure boundary. An existing
    /// identity with a different fingerprint is always a hard conflict.
    pub async fn prepare(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
        decision: &ToolInvocationDecision,
    ) -> Result<ToolInvocationPrepareOutcome, ToolInvocationLedgerStoreError> {
        let fingerprint_json = serde_json::to_string(fingerprint).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "fingerprint_json",
                source,
            }
        })?;
        let decision_json = serde_json::to_string(decision).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "decision_json",
                source,
            }
        })?;
        let mut tx = self.pool.get().begin().await?;
        if let Some(record) = load_record_in_tx(&mut tx, identity).await? {
            if !record.fingerprint.same_tool_and_arguments(fingerprint) {
                rollback(tx, "prepare identity conflict").await;
                return Err(ToolInvocationLedgerStoreError::IdentityConflict {
                    identity: identity.clone(),
                });
            }
            tx.commit().await?;
            return Ok(ToolInvocationPrepareOutcome::Existing(record));
        }
        if let Err(error) = lock_executable_run(&mut tx, identity).await {
            rollback(tx, "prepare run admission denied").await;
            if matches!(
                &error,
                ToolInvocationLedgerStoreError::RunNotExecutable { .. }
            ) {
                return match self.load_archived_record(identity).await {
                    Ok(Some(record)) => {
                        if !record.fingerprint.same_tool_and_arguments(fingerprint) {
                            Err(ToolInvocationLedgerStoreError::IdentityConflict {
                                identity: identity.clone(),
                            })
                        } else {
                            Ok(ToolInvocationPrepareOutcome::Existing(record))
                        }
                    }
                    Ok(None)
                    | Err(ToolInvocationLedgerStoreError::TerminalRunRecordUnavailable {
                        ..
                    }) => Err(error),
                    Err(archive_error) => Err(archive_error),
                };
            }
            return Err(error);
        }
        let inserted = sqlx::query(
            "INSERT IGNORE INTO tool_invocation_ledger (
                user_id, session_id, run_id, turn_chain_id, invocation_id,
                identity_key, fingerprint_json, decision_json, state, dispatch_certainty, attempt_count,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'prepared', 'not_dispatched', 0,
                       CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .bind(identity.storage_key())
        .bind(&fingerprint_json)
        .bind(&decision_json)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        let record = load_record_in_tx(&mut tx, identity).await?.ok_or_else(|| {
            ToolInvocationLedgerStoreError::MissingAfterPrepare {
                identity: identity.clone(),
            }
        })?;
        if !record.fingerprint.same_tool_and_arguments(fingerprint) {
            rollback(tx, "prepare identity conflict").await;
            return Err(ToolInvocationLedgerStoreError::IdentityConflict {
                identity: identity.clone(),
            });
        }
        tx.commit().await?;

        Ok(if inserted {
            ToolInvocationPrepareOutcome::Prepared(record)
        } else {
            ToolInvocationPrepareOutcome::Existing(record)
        })
    }

    pub async fn get(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<Option<ToolInvocationRecord>, ToolInvocationLedgerStoreError> {
        let row = select_record_query(identity)
            .fetch_optional(self.pool.get())
            .await?;
        match row {
            Some(row) => decode_record(&row, identity).map(Some),
            None => self.load_archived_record(identity).await,
        }
    }

    /// Bounded, owner-scoped evidence for introspect/reflect. This is a
    /// projection of the ledger, archive, reference, event, and maintenance
    /// authorities; it never controls invocation execution.
    pub async fn lifecycle_diagnostics(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: Option<&str>,
    ) -> Result<ToolInvocationLifecycleDiagnostics, ToolInvocationLedgerStoreError> {
        let hot: (i64, i64, i64, i64, i64, i64, i64, i64) = if let Some(run_id) = run_id {
            sqlx::query_as(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN state = 'prepared' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'dispatched' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'succeeded' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'rejected' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'outcome_unknown' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'rejected' AND dispatch_certainty = 'not_dispatched' THEN 1 ELSE 0 END), 0)
                 FROM tool_invocation_ledger
                 WHERE user_id = ? AND session_id = ? AND run_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .fetch_one(self.pool.get())
            .await?
        } else {
            sqlx::query_as(
                "SELECT COUNT(*),
                        COALESCE(SUM(CASE WHEN state = 'prepared' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'dispatched' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'succeeded' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'failed' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'rejected' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'outcome_unknown' THEN 1 ELSE 0 END), 0),
                        COALESCE(SUM(CASE WHEN state = 'rejected' AND dispatch_certainty = 'not_dispatched' THEN 1 ELSE 0 END), 0)
                 FROM tool_invocation_ledger
                 WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(self.pool.get())
            .await?
        };
        let archive_chunks: i64 = if let Some(run_id) = run_id {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM tool_invocation_archive_chunks
                 WHERE user_id = ? AND session_id = ? AND run_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .fetch_one(self.pool.get())
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM tool_invocation_archive_chunks
                 WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(self.pool.get())
            .await?
        };
        let durable_artifact_references: i64 = if let Some(run_id) = run_id {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM session_artifact_references refs
                 WHERE refs.user_id = ? AND refs.session_id = ?
                   AND refs.reference_kind = 'invocation_ledger'
                   AND (
                       refs.reference_id = ?
                       OR EXISTS (
                           SELECT 1 FROM tool_invocation_ledger ledger
                           WHERE ledger.user_id = refs.user_id
                             AND ledger.session_id = refs.session_id
                             AND ledger.run_id = ?
                             AND ledger.identity_key = refs.reference_id
                       )
                   )",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .bind(run_id)
            .fetch_one(self.pool.get())
            .await?
        } else {
            sqlx::query_scalar(
                "SELECT COUNT(*) FROM session_artifact_references
                 WHERE user_id = ? AND session_id = ?
                   AND reference_kind = 'invocation_ledger'",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(self.pool.get())
            .await?
        };
        let events: (i64, i64) = if let Some(run_id) = run_id {
            sqlx::query_as(
                "SELECT
                    COALESCE(SUM(CASE WHEN event_type = 'tool_invocation_run_reconciled' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN event_type = 'tool_invocation_compaction_deferred' THEN 1 ELSE 0 END), 0)
                 FROM agent_events
                 WHERE user_id = ? AND session_id = ? AND run_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .fetch_one(self.pool.get())
            .await?
        } else {
            sqlx::query_as(
                "SELECT
                    COALESCE(SUM(CASE WHEN event_type = 'tool_invocation_run_reconciled' THEN 1 ELSE 0 END), 0),
                    COALESCE(SUM(CASE WHEN event_type = 'tool_invocation_compaction_deferred' THEN 1 ELSE 0 END), 0)
                 FROM agent_events
                 WHERE user_id = ? AND session_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .fetch_one(self.pool.get())
            .await?
        };
        let cursor: Option<(u64, String)> = sqlx::query_as(
            "SELECT scan_generation, CAST(updated_at AS CHAR)
             FROM maintenance_sweep_cursors
             WHERE sweep_name = 'tool_invocation_compaction_v1'",
        )
        .fetch_optional(self.pool.get())
        .await?;
        let counts = [
            hot.0,
            hot.1,
            hot.2,
            hot.3,
            hot.4,
            hot.5,
            hot.6,
            hot.7,
            archive_chunks,
            durable_artifact_references,
            events.0,
            events.1,
        ]
        .map(|count| {
            u64::try_from(count)
                .map_err(|_| ToolInvocationLedgerStoreError::InvalidCompactionCount(count))
        });
        let [
            hot_total,
            prepared,
            dispatched,
            succeeded,
            failed,
            rejected,
            outcome_unknown,
            rejected_without_dispatch,
            archive_chunks,
            durable_artifact_references,
            reconciliation_events,
            compaction_deferred_events,
        ] = counts;
        Ok(ToolInvocationLifecycleDiagnostics {
            run_id: run_id.map(str::to_string),
            hot_total: hot_total?,
            prepared: prepared?,
            dispatched: dispatched?,
            succeeded: succeeded?,
            failed: failed?,
            rejected: rejected?,
            outcome_unknown: outcome_unknown?,
            rejected_without_dispatch: rejected_without_dispatch?,
            archive_chunks: archive_chunks?,
            durable_artifact_references: durable_artifact_references?,
            reconciliation_events: reconciliation_events?,
            compaction_deferred_events: compaction_deferred_events?,
            compaction_cursor_generation: cursor.as_ref().map(|(generation, _)| *generation),
            compaction_cursor_updated_at: cursor.map(|(_, updated_at)| updated_at),
        })
    }

    /// Reconcile a terminal run before archival. A prepared row proves the
    /// provider boundary was never crossed and becomes an explicit rejection;
    /// an expired or malformed dispatch lease cannot prove the provider
    /// outcome and becomes `OutcomeUnknown`. A live dispatch lease remains
    /// authoritative until it completes or expires.
    pub async fn reconcile_terminal_run(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<ToolInvocationRunReconciliationOutcome, ToolInvocationLedgerStoreError> {
        let mut tx = self.pool.get().begin().await?;
        let run_status = lock_terminal_run(&mut tx, user_id, session_id, run_id).await?;
        let completion_source = ToolInvocationCompletionSource::run_closure(&run_status)?;
        let result = ToolInvocationResultPayload::new(
            serde_json::json!({
                "status": "rejected",
                "reason": "run_closed_before_dispatch",
                "run_status": &run_status,
            })
            .to_string(),
            BTreeMap::from([
                (
                    "error_kind".to_string(),
                    serde_json::Value::String("run_closed".to_string()),
                ),
                (
                    "run_status".to_string(),
                    serde_json::Value::String(run_status.clone()),
                ),
                ("retryable".to_string(), serde_json::Value::Bool(false)),
            ]),
            None,
        )?;
        let outcome = ToolInvocationTerminalOutcome::Rejected {
            result,
            rejection_code: Some("run_closed".to_string()),
            retryable: false,
        };
        let outcome_json = serde_json::to_string(&outcome).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "outcome_json",
                source,
            }
        })?;
        let completion_source_json =
            serde_json::to_string(&completion_source).map_err(|source| {
                ToolInvocationLedgerStoreError::Serialization {
                    field: "completion_source_json",
                    source,
                }
            })?;
        let prepared_rejected = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'rejected', dispatch_certainty = 'not_dispatched',
                 outcome_json = ?, completion_source_json = ?,
                 dispatch_owner = NULL, dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND state = 'prepared' AND attempt_count = 0",
        )
        .bind(outcome_json)
        .bind(completion_source_json)
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let inconsistent_prepared_unknown = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'outcome_unknown', dispatch_certainty = 'unknown',
                 dispatch_owner = NULL, dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND state = 'prepared' AND attempt_count > 0",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let expired_dispatches_unknown = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'outcome_unknown', dispatch_certainty = 'unknown',
                 dispatch_owner = NULL, dispatch_lease_expires_at = NULL,
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND state = 'dispatched'
               AND (dispatch_owner IS NULL
                    OR dispatch_lease_expires_at IS NULL
                    OR dispatch_lease_expires_at <= CURRENT_TIMESTAMP(6))",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let active_dispatches_remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tool_invocation_ledger
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND state = 'dispatched'",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
        let active_dispatches_remaining =
            u64::try_from(active_dispatches_remaining).map_err(|_| {
                ToolInvocationLedgerStoreError::InvalidCompactionCount(active_dispatches_remaining)
            })?;

        if prepared_rejected > 0
            || inconsistent_prepared_unknown > 0
            || expired_dispatches_unknown > 0
        {
            let event_id = Uuid::new_v4().to_string();
            let insert = sqlx::query(
                "INSERT INTO agent_events
                 (event_id, session_id, user_id, run_id, event_type, content, metadata, created_at)
                 VALUES (?, ?, ?, ?, 'tool_invocation_run_reconciled', ?, ?, NOW(6))",
            )
            .bind(&event_id)
            .bind(session_id)
            .bind(user_id)
            .bind(run_id)
            .bind(format!(
                "terminal run invocation reconciliation: prepared_rejected={prepared_rejected}, inconsistent_prepared_unknown={inconsistent_prepared_unknown}, expired_dispatches_unknown={expired_dispatches_unknown}, active_dispatches_remaining={active_dispatches_remaining}"
            ))
            .bind(
                serde_json::json!({
                    "run_status": &run_status,
                    "prepared_rejected": prepared_rejected,
                    "inconsistent_prepared_unknown": inconsistent_prepared_unknown,
                    "expired_dispatches_unknown": expired_dispatches_unknown,
                    "active_dispatches_remaining": active_dispatches_remaining,
                })
                .to_string(),
            )
            .execute(&mut *tx)
            .await?;
            let inserted_events = crate::storage::rows_affected_to_i64(
                insert.rows_affected(),
                "tool invocation reconciliation event",
            )?;
            if inserted_events != 1 {
                return Err(sqlx::Error::Protocol(
                    "tool invocation reconciliation event was not inserted exactly once".into(),
                )
                .into());
            }
            crate::storage::add_agent_session_event_count_or_create(
                &mut *tx,
                session_id,
                user_id,
                inserted_events,
                Some(&event_id),
            )
            .await?;
        }
        if active_dispatches_remaining > 0 {
            let scope = format!("{user_id}\0{session_id}\0{run_id}");
            let event_id = format!("invocation-deferred-{:x}", Sha256::digest(scope.as_bytes()));
            let already_recorded: Option<i8> = sqlx::query_scalar(
                "SELECT 1 FROM agent_events
                 WHERE user_id = ? AND event_id = ?",
            )
            .bind(user_id)
            .bind(&event_id)
            .fetch_optional(&mut *tx)
            .await?;
            if already_recorded.is_none() {
                sqlx::query(
                    "INSERT INTO agent_events
                     (event_id, session_id, user_id, run_id, event_type, content, metadata, created_at)
                     VALUES (?, ?, ?, ?, 'tool_invocation_compaction_deferred', ?, ?, NOW(6))",
                )
                .bind(&event_id)
                .bind(session_id)
                .bind(user_id)
                .bind(run_id)
                .bind(format!(
                    "terminal run retains {active_dispatches_remaining} actively leased tool invocation(s)"
                ))
                .bind(
                    serde_json::json!({
                        "run_status": &run_status,
                        "active_dispatches_remaining": active_dispatches_remaining,
                        "resolution": "wait_for_completion_or_lease_expiry",
                    })
                    .to_string(),
                )
                .execute(&mut *tx)
                .await?;
                crate::storage::add_agent_session_event_count_or_create(
                    &mut *tx,
                    session_id,
                    user_id,
                    1,
                    Some(&event_id),
                )
                .await?;
            }
        }
        tx.commit().await?;
        Ok(ToolInvocationRunReconciliationOutcome {
            prepared_rejected,
            inconsistent_prepared_unknown,
            expired_dispatches_unknown,
            active_dispatches_remaining,
        })
    }

    /// Move a bounded batch of a terminal run's invocation records from the
    /// hot CAS table into one owner-scoped archive artifact. The artifact,
    /// lookup range, durable reference, and hot-row deletion commit together.
    pub async fn compact_terminal_run_batch(
        &self,
        user_id: &str,
        session_id: &str,
        run_id: &str,
    ) -> Result<ToolInvocationCompactionOutcome, ToolInvocationLedgerStoreError> {
        let mut tx = self.pool.get().begin().await?;
        let _run_status = lock_terminal_run(&mut tx, user_id, session_id, run_id).await?;
        let non_terminal_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tool_invocation_ledger
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND state IN ('prepared', 'dispatched')",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
        let non_terminal_count = u64::try_from(non_terminal_count).map_err(|_| {
            ToolInvocationLedgerStoreError::InvalidCompactionCount(non_terminal_count)
        })?;
        if non_terminal_count > 0 {
            return Err(ToolInvocationLedgerStoreError::RunNotQuiescent {
                run_id: run_id.to_string(),
                non_terminal_count,
            });
        }

        let rows = sqlx::query(
            "SELECT turn_chain_id, invocation_id, identity_key,
                    CAST(fingerprint_json AS CHAR) AS fingerprint_json,
                    CAST(decision_json AS CHAR) AS decision_json,
                    CAST(outcome_json AS CHAR) AS outcome_json,
                    CAST(completion_source_json AS CHAR) AS completion_source_json,
                    state, dispatch_certainty, attempt_count, dispatch_owner,
                    CAST(UNIX_TIMESTAMP(dispatch_lease_expires_at) * 1000 AS UNSIGNED)
                        AS dispatch_lease_expires_at_epoch_ms
             FROM tool_invocation_ledger
             WHERE user_id = ? AND session_id = ? AND run_id = ?
             ORDER BY identity_key
             LIMIT ? FOR UPDATE",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .bind(TOOL_INVOCATION_COMPACTION_BATCH_RECORDS)
        .fetch_all(&mut *tx)
        .await?;
        if rows.is_empty() {
            tx.commit().await?;
            return Ok(ToolInvocationCompactionOutcome {
                archived_records: 0,
                remaining_records: 0,
                artifact_id: None,
            });
        }

        let mut records = Vec::with_capacity(rows.len());
        let mut identity_keys = Vec::with_capacity(rows.len());
        for row in &rows {
            let identity = ToolInvocationIdentity::new(
                user_id,
                session_id,
                run_id,
                row.try_get::<String, _>("turn_chain_id")?,
                row.try_get::<String, _>("invocation_id")?,
            )?;
            let stored_identity_key: String = row.try_get("identity_key")?;
            let expected_identity_key = identity.storage_key();
            if stored_identity_key != expected_identity_key {
                return Err(ToolInvocationLedgerStoreError::IdentityKeyMismatch {
                    identity: Box::new(identity),
                    expected: expected_identity_key,
                    actual: stored_identity_key,
                });
            }
            records.push(decode_record(row, &identity)?);
            identity_keys.push(expected_identity_key);
        }
        let chunk = ToolInvocationArchiveChunk {
            version: TOOL_INVOCATION_ARCHIVE_VERSION.to_string(),
            user_id: user_id.to_string(),
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            records,
        };
        let content_json = serde_json::to_string(&chunk).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "archive_chunk",
                source,
            }
        })?;
        if content_json.len() > TOOL_INVOCATION_ARCHIVE_CHUNK_MAX_BYTES {
            return Err(ToolInvocationLedgerStoreError::ArchiveChunkTooLarge {
                actual_bytes: content_json.len(),
                max_bytes: TOOL_INVOCATION_ARCHIVE_CHUNK_MAX_BYTES,
            });
        }
        let first_identity_key = identity_keys.first().cloned().expect("non-empty archive");
        let last_identity_key = identity_keys.last().cloned().expect("non-empty archive");
        let artifact_id = Uuid::now_v7().to_string();
        let content_hash = format!("sha256:{:x}", Sha256::digest(content_json.as_bytes()));
        let retention_until = (chrono::Utc::now()
            + chrono::Duration::days(TOOL_INVOCATION_ARCHIVE_RETENTION_DAYS))
        .naive_utc();
        let metadata = serde_json::json!({
            "contractVersion": TOOL_INVOCATION_ARCHIVE_VERSION,
            "recordCount": chunk.records.len(),
            "encodedBytes": content_json.len(),
            "contentHash": content_hash,
        });
        let chunk_index: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(chunk_index), 0) + 1
             FROM tool_invocation_archive_chunks
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
        if chunk_index <= 0 {
            return Err(ToolInvocationLedgerStoreError::InvalidCompactionCount(
                chunk_index,
            ));
        }
        sqlx::query(
            "INSERT INTO session_artifacts
             (artifact_id, session_id, user_id, artifact_kind, source, content_json,
              metadata, retention_until, created_at)
             VALUES (?, ?, ?, 'tool_invocation_archive_v1', 'invocation_ledger_compactor',
                     ?, ?, ?, CURRENT_TIMESTAMP(6))",
        )
        .bind(&artifact_id)
        .bind(session_id)
        .bind(user_id)
        .bind(&content_json)
        .bind(metadata.to_string())
        .bind(retention_until)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO session_artifact_references
             (user_id, session_id, artifact_id, reference_kind, reference_id, created_at)
             VALUES (?, ?, ?, 'invocation_ledger', ?, CURRENT_TIMESTAMP(6))",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(&artifact_id)
        .bind(run_id)
        .execute(&mut *tx)
        .await?;
        // The hot invocation identity ceases to own large-result evidence in
        // the same transaction that removes its ledger row. Transfer that
        // reachability to the run archive without rewriting the artifact's
        // retention policy; otherwise per-invocation references grow forever
        // in long-lived sessions.
        for record in &chunk.records {
            let invocation_key = record.identity.storage_key();
            let result_artifact_ids = sqlx::query_scalar::<_, String>(
                "SELECT artifact_id FROM session_artifact_references
                 WHERE user_id = ? AND session_id = ?
                   AND reference_kind = 'invocation_ledger' AND reference_id = ?
                 FOR UPDATE",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(&invocation_key)
            .fetch_all(&mut *tx)
            .await?;
            for result_artifact_id in result_artifact_ids {
                sqlx::query(
                    "INSERT IGNORE INTO session_artifact_references
                     (user_id, session_id, artifact_id, reference_kind, reference_id, created_at)
                     VALUES (?, ?, ?, 'invocation_ledger', ?, CURRENT_TIMESTAMP(6))",
                )
                .bind(user_id)
                .bind(session_id)
                .bind(&result_artifact_id)
                .bind(run_id)
                .execute(&mut *tx)
                .await?;
            }
            sqlx::query(
                "DELETE FROM session_artifact_references
                 WHERE user_id = ? AND session_id = ?
                   AND reference_kind = 'invocation_ledger' AND reference_id = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(invocation_key)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO tool_invocation_archive_chunks
             (user_id, session_id, run_id, chunk_index, artifact_id,
              first_identity_key, last_identity_key, record_count, encoded_bytes, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP(6))",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .bind(chunk_index)
        .bind(&artifact_id)
        .bind(&first_identity_key)
        .bind(&last_identity_key)
        .bind(chunk.records.len() as u64)
        .bind(content_json.len() as u64)
        .execute(&mut *tx)
        .await?;
        for record in &chunk.records {
            sqlx::query(
                "DELETE FROM tool_invocation_ledger
                 WHERE user_id = ? AND session_id = ? AND run_id = ?
                   AND turn_chain_id = ? AND invocation_id = ? AND identity_key = ?",
            )
            .bind(user_id)
            .bind(session_id)
            .bind(run_id)
            .bind(&record.identity.turn_chain_id)
            .bind(&record.identity.invocation_id)
            .bind(record.identity.storage_key())
            .execute(&mut *tx)
            .await?;
        }
        let remaining_records: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM tool_invocation_ledger
             WHERE user_id = ? AND session_id = ? AND run_id = ?",
        )
        .bind(user_id)
        .bind(session_id)
        .bind(run_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ToolInvocationCompactionOutcome {
            archived_records: chunk.records.len(),
            remaining_records: u64::try_from(remaining_records).map_err(|_| {
                ToolInvocationLedgerStoreError::InvalidCompactionCount(remaining_records)
            })?,
            artifact_id: Some(artifact_id),
        })
    }

    async fn load_archived_record(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<Option<ToolInvocationRecord>, ToolInvocationLedgerStoreError> {
        let identity_key = identity.storage_key();
        let rows = sqlx::query(
            "SELECT chunks.artifact_id, artifacts.status, artifacts.content_json
             FROM tool_invocation_archive_chunks chunks
             LEFT JOIN session_artifacts artifacts
               ON artifacts.user_id = chunks.user_id
              AND artifacts.session_id = chunks.session_id
              AND artifacts.artifact_id = chunks.artifact_id
             WHERE chunks.user_id = ? AND chunks.session_id = ? AND chunks.run_id = ?
               AND chunks.first_identity_key <= ? AND chunks.last_identity_key >= ?
             ORDER BY chunks.chunk_index LIMIT 2",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity_key)
        .bind(&identity_key)
        .fetch_all(self.pool.get())
        .await?;
        if rows.len() > 1 {
            return Err(ToolInvocationLedgerStoreError::OverlappingArchiveRanges {
                identity: identity.clone(),
            });
        }
        let Some(row) = rows.first() else {
            let status: Option<String> = sqlx::query_scalar(
                "SELECT status FROM agent_runs
                 WHERE user_id = ? AND session_id = ? AND run_id = ?",
            )
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .fetch_optional(self.pool.get())
            .await?;
            if status
                .as_deref()
                .is_some_and(crate::runs::durable_run_status_is_terminal)
            {
                return Err(
                    ToolInvocationLedgerStoreError::TerminalRunRecordUnavailable {
                        identity: identity.clone(),
                    },
                );
            }
            return Ok(None);
        };
        let artifact_id: String = row.try_get("artifact_id")?;
        let status: Option<String> = row.try_get("status")?;
        let content_json: Option<String> = row.try_get("content_json")?;
        if status.as_deref() != Some("active") || content_json.is_none() {
            return Err(ToolInvocationLedgerStoreError::ArchiveUnavailable {
                artifact_id,
                status,
            });
        }
        let content_json = content_json.expect("checked archive content");
        let chunk: ToolInvocationArchiveChunk =
            serde_json::from_str(&content_json).map_err(|source| {
                ToolInvocationLedgerStoreError::InvalidArchive {
                    artifact_id: artifact_id.clone(),
                    source,
                }
            })?;
        if chunk.version != TOOL_INVOCATION_ARCHIVE_VERSION
            || chunk.user_id != identity.user_id
            || chunk.session_id != identity.session_id
            || chunk.run_id != identity.run_id
        {
            return Err(ToolInvocationLedgerStoreError::ArchiveScopeMismatch { artifact_id });
        }
        Ok(chunk
            .records
            .into_iter()
            .find(|record| record.identity == *identity))
    }

    /// Atomically grant one worker the right to cross the provider boundary.
    /// The run closure row is locked and revalidated in the same transaction
    /// as `Prepared -> Dispatched`; a prepared identity is not authority to
    /// start new work after its run closes. MatrixOne's clock owns the lease
    /// deadline, avoiding application-host clock skew in expiry decisions.
    pub async fn claim_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
        lease_duration_ms: u64,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        validate_lease_input(owner_id, lease_duration_ms)?;
        let lease_duration_us = lease_duration_us(lease_duration_ms)?;
        let mut tx = self.pool.get().begin().await?;
        lock_executable_run(&mut tx, identity).await?;
        let updated = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'dispatched', dispatch_certainty = 'dispatched',
                 attempt_count = attempt_count + 1, dispatch_owner = ?,
                 dispatch_lease_expires_at = TIMESTAMPADD(MICROSECOND, ?, CURRENT_TIMESTAMP(6)),
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ? AND state = 'prepared'",
        )
        .bind(owner_id)
        .bind(lease_duration_us)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        let record = load_record_in_tx(&mut tx, identity).await?;
        if updated != 1 {
            rollback(tx, "claim-dispatch mismatch").await;
            return match record {
                Some(actual) => Err(ToolInvocationLedgerStoreError::StateMismatch {
                    identity: identity.clone(),
                    expected: ToolInvocationState::Prepared,
                    actual: actual.state,
                }),
                None => Err(ToolInvocationLedgerStoreError::NotFound {
                    identity: identity.clone(),
                }),
            };
        }
        let record = record.ok_or_else(|| ToolInvocationLedgerStoreError::NotFound {
            identity: identity.clone(),
        })?;
        tx.commit().await?;
        Ok(record)
    }

    pub async fn renew_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
        lease_duration_ms: u64,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        validate_lease_input(owner_id, lease_duration_ms)?;
        let lease_duration_us = lease_duration_us(lease_duration_ms)?;
        let mut tx = self.pool.get().begin().await?;
        sqlx::query(
            "UPDATE tool_invocation_ledger
             SET dispatch_lease_expires_at =
                     TIMESTAMPADD(MICROSECOND, ?, CURRENT_TIMESTAMP(6)),
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ?
               AND state = 'dispatched' AND dispatch_owner = ?",
        )
        .bind(lease_duration_us)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .bind(owner_id)
        .execute(&mut *tx)
        .await?;
        let record = load_record_in_tx(&mut tx, identity).await?.ok_or_else(|| {
            ToolInvocationLedgerStoreError::NotFound {
                identity: identity.clone(),
            }
        })?;
        ensure_dispatched_owner(identity, &record, owner_id)?;
        tx.commit().await?;
        Ok(record)
    }

    /// Return the authoritative row after atomically converting an expired
    /// dispatch to `OutcomeUnknown`. A live lease or concurrent completion is
    /// returned unchanged and never overwritten.
    pub async fn reconcile_expired_dispatch(
        &self,
        identity: &ToolInvocationIdentity,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        let mut tx = self.pool.get().begin().await?;
        sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'outcome_unknown', dispatch_certainty = 'unknown',
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ?
               AND state = 'dispatched'
               AND dispatch_lease_expires_at <= CURRENT_TIMESTAMP(6)",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .execute(&mut *tx)
        .await?;
        let record = load_record_in_tx(&mut tx, identity).await?.ok_or_else(|| {
            ToolInvocationLedgerStoreError::NotFound {
                identity: identity.clone(),
            }
        })?;
        tx.commit().await?;
        Ok(record)
    }

    pub async fn mark_outcome_unknown(
        &self,
        identity: &ToolInvocationIdentity,
        owner_id: &str,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        ToolInvocationDispatchLease::new(owner_id, 1)?;
        let mut tx = self.pool.get().begin().await?;
        let updated = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'outcome_unknown', dispatch_certainty = 'unknown',
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ?
               AND state = 'dispatched' AND dispatch_owner = ?",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .bind(owner_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let record = load_record_in_tx(&mut tx, identity).await?.ok_or_else(|| {
            ToolInvocationLedgerStoreError::NotFound {
                identity: identity.clone(),
            }
        })?;
        if updated != 1 {
            rollback(tx, "mark-outcome-unknown mismatch").await;
            ensure_dispatched_owner(identity, &record, owner_id)?;
            return Err(ToolInvocationLedgerStoreError::StateMismatch {
                identity: identity.clone(),
                expected: ToolInvocationState::Dispatched,
                actual: record.state,
            });
        }
        tx.commit().await?;
        Ok(record)
    }

    /// Atomically persist an acknowledged terminal outcome with its state.
    /// A replay can observe either the pre-terminal state or the complete
    /// typed outcome, never a terminal marker without replay material.
    pub async fn compare_and_complete(
        &self,
        identity: &ToolInvocationIdentity,
        expected: ToolInvocationState,
        owner_id: Option<&str>,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        outcome.validate()?;
        let next = outcome.state();
        if !expected.can_transition_to(next) {
            return Err(ToolInvocationLedgerStoreError::IllegalTransition {
                from: expected,
                to: next,
            });
        }
        let outcome_json = serde_json::to_string(outcome).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "outcome_json",
                source,
            }
        })?;
        let mut tx = self.pool.get().begin().await?;
        let (owner_predicate, owner_id) = completion_owner(expected, owner_id, identity)?;
        let query = format!(
            "UPDATE tool_invocation_ledger
             SET state = ?, dispatch_certainty = 'dispatched', outcome_json = ?,
                 completion_source_json = NULL,
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ? AND state = ?{owner_predicate}"
        );
        let mut query = sqlx::query(&query)
            .bind(state_label(next))
            .bind(outcome_json)
            .bind(&identity.user_id)
            .bind(&identity.session_id)
            .bind(&identity.run_id)
            .bind(&identity.turn_chain_id)
            .bind(&identity.invocation_id)
            .bind(state_label(expected));
        if let Some(owner_id) = owner_id {
            query = query.bind(owner_id);
        }
        let updated = query.execute(&mut *tx).await?.rows_affected();

        let record = load_record_in_tx(&mut tx, identity).await?;
        if updated != 1 {
            let mismatch = match record.as_ref() {
                Some(actual)
                    if expected == ToolInvocationState::Dispatched
                        && actual.state == ToolInvocationState::Dispatched
                        && owner_id.is_some_and(|owner_id| {
                            actual
                                .dispatch_lease
                                .as_ref()
                                .is_none_or(|lease| lease.owner_id != owner_id)
                        }) =>
                {
                    ToolInvocationLedgerStoreError::DispatchOwnerMismatch {
                        identity: identity.clone(),
                    }
                }
                Some(actual) => ToolInvocationLedgerStoreError::StateMismatch {
                    identity: identity.clone(),
                    expected,
                    actual: actual.state,
                },
                None => ToolInvocationLedgerStoreError::NotFound {
                    identity: identity.clone(),
                },
            };
            rollback(tx, "compare-and-complete mismatch").await;
            return Err(mismatch);
        }
        let record = record.ok_or_else(|| ToolInvocationLedgerStoreError::NotFound {
            identity: identity.clone(),
        })?;
        tx.commit().await?;
        Ok(record)
    }

    /// Complete a prepared invocation from a semantic read observation. This
    /// CAS deliberately leaves dispatch certainty as `not_dispatched` and the
    /// provider attempt count at zero.
    pub async fn complete_from_semantic_read_cache(
        &self,
        identity: &ToolInvocationIdentity,
        result: &ToolInvocationResultPayload,
        completion_source: &ToolInvocationCompletionSource,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        result.validate()?;
        completion_source.validate()?;
        let outcome = ToolInvocationTerminalOutcome::Succeeded {
            result: result.clone(),
        };
        let outcome_json = serde_json::to_string(&outcome).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "outcome_json",
                source,
            }
        })?;
        let completion_source_json =
            serde_json::to_string(completion_source).map_err(|source| {
                ToolInvocationLedgerStoreError::Serialization {
                    field: "completion_source_json",
                    source,
                }
            })?;
        let mut tx = self.pool.get().begin().await?;
        let updated = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = 'succeeded', dispatch_certainty = 'not_dispatched',
                 outcome_json = ?, completion_source_json = ?,
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ?
               AND state = 'prepared' AND attempt_count = 0
               AND dispatch_owner IS NULL AND dispatch_lease_expires_at IS NULL",
        )
        .bind(outcome_json)
        .bind(completion_source_json)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let record = load_record_in_tx(&mut tx, identity).await?;
        if updated != 1 {
            let error = match record {
                Some(actual) => ToolInvocationLedgerStoreError::StateMismatch {
                    identity: identity.clone(),
                    expected: ToolInvocationState::Prepared,
                    actual: actual.state,
                },
                None => ToolInvocationLedgerStoreError::NotFound {
                    identity: identity.clone(),
                },
            };
            rollback(tx, "semantic-cache-completion mismatch").await;
            return Err(error);
        }
        let record = record.ok_or_else(|| ToolInvocationLedgerStoreError::NotFound {
            identity: identity.clone(),
        })?;
        tx.commit().await?;
        Ok(record)
    }
}

/// Serialize new invocation admission and dispatch with terminal run
/// transitions. The run row is the durable closure boundary: existing
/// identities remain readable before this guard, but after the run becomes
/// terminal no identity can create or cross a fresh provider boundary.
async fn lock_executable_run(
    tx: &mut Transaction<'_, MySql>,
    identity: &ToolInvocationIdentity,
) -> Result<(), ToolInvocationLedgerStoreError> {
    let row = sqlx::query(
        "SELECT session_id, status FROM agent_runs
         WHERE user_id = ? AND run_id = ? FOR UPDATE",
    )
    .bind(&identity.user_id)
    .bind(&identity.run_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(ToolInvocationLedgerStoreError::RunNotFound {
            user_id: identity.user_id.clone(),
            run_id: identity.run_id.clone(),
        });
    };
    let session_id: String = row.try_get("session_id")?;
    if session_id != identity.session_id {
        return Err(ToolInvocationLedgerStoreError::RunSessionMismatch {
            run_id: identity.run_id.clone(),
            expected_session_id: identity.session_id.clone(),
            actual_session_id: session_id,
        });
    }
    let status: String = row.try_get("status")?;
    if status != astra_core::STATUS_RUNNING {
        return Err(ToolInvocationLedgerStoreError::RunNotExecutable {
            run_id: identity.run_id.clone(),
            status,
        });
    }
    Ok(())
}

async fn lock_terminal_run(
    tx: &mut Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
) -> Result<String, ToolInvocationLedgerStoreError> {
    let row = sqlx::query(
        "SELECT session_id, status FROM agent_runs
         WHERE user_id = ? AND run_id = ? FOR UPDATE",
    )
    .bind(user_id)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(row) = row else {
        return Err(ToolInvocationLedgerStoreError::RunNotFound {
            user_id: user_id.to_string(),
            run_id: run_id.to_string(),
        });
    };
    let actual_session_id: String = row.try_get("session_id")?;
    if actual_session_id != session_id {
        return Err(ToolInvocationLedgerStoreError::RunSessionMismatch {
            run_id: run_id.to_string(),
            expected_session_id: session_id.to_string(),
            actual_session_id,
        });
    }
    let status: String = row.try_get("status")?;
    if !crate::runs::durable_run_status_is_terminal(&status) {
        return Err(ToolInvocationLedgerStoreError::RunNotTerminal {
            run_id: run_id.to_string(),
            status,
        });
    }
    Ok(status)
}

fn select_record_query(
    identity: &ToolInvocationIdentity,
) -> sqlx::query::Query<'_, MySql, sqlx::mysql::MySqlArguments> {
    sqlx::query(
        "SELECT CAST(fingerprint_json AS CHAR) AS fingerprint_json,
                CAST(decision_json AS CHAR) AS decision_json,
                CAST(outcome_json AS CHAR) AS outcome_json,
                CAST(completion_source_json AS CHAR) AS completion_source_json,
                state, dispatch_certainty, attempt_count, dispatch_owner,
                CAST(
                    UNIX_TIMESTAMP(dispatch_lease_expires_at) * 1000
                    AS UNSIGNED
                ) AS dispatch_lease_expires_at_epoch_ms
         FROM tool_invocation_ledger
         WHERE user_id = ? AND session_id = ? AND run_id = ?
           AND turn_chain_id = ? AND invocation_id = ?",
    )
    .bind(&identity.user_id)
    .bind(&identity.session_id)
    .bind(&identity.run_id)
    .bind(&identity.turn_chain_id)
    .bind(&identity.invocation_id)
}

async fn load_record_in_tx(
    tx: &mut Transaction<'_, MySql>,
    identity: &ToolInvocationIdentity,
) -> Result<Option<ToolInvocationRecord>, ToolInvocationLedgerStoreError> {
    let row = select_record_query(identity)
        .fetch_optional(&mut **tx)
        .await?;
    row.map(|row| decode_record(&row, identity)).transpose()
}

fn decode_record(
    row: &sqlx::mysql::MySqlRow,
    identity: &ToolInvocationIdentity,
) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
    let fingerprint_json: String = row.try_get("fingerprint_json")?;
    let fingerprint = serde_json::from_str(&fingerprint_json).map_err(|source| {
        ToolInvocationLedgerStoreError::InvalidStoredJson {
            field: "fingerprint_json",
            source,
        }
    })?;
    let decision_json: Option<String> = row.try_get("decision_json")?;
    let decision = serde_json::from_str(
        decision_json
            .as_deref()
            .ok_or(ToolInvocationLedgerStoreError::MissingDecision)?,
    )
    .map_err(|source| ToolInvocationLedgerStoreError::InvalidStoredJson {
        field: "decision_json",
        source,
    })?;
    let state_raw: String = row.try_get("state")?;
    let certainty_raw: String = row.try_get("dispatch_certainty")?;
    let attempt_count: u64 = row.try_get("attempt_count")?;
    let dispatch_owner: Option<String> = row.try_get("dispatch_owner")?;
    let dispatch_lease_expires_at_epoch_ms: Option<u64> =
        row.try_get("dispatch_lease_expires_at_epoch_ms")?;
    let outcome_json: Option<String> = row.try_get("outcome_json")?;
    let outcome = outcome_json
        .map(|value| {
            serde_json::from_str(&value).map_err(|source| {
                ToolInvocationLedgerStoreError::InvalidStoredJson {
                    field: "outcome_json",
                    source,
                }
            })
        })
        .transpose()?;
    let completion_source_json: Option<String> = row.try_get("completion_source_json")?;
    let completion_source = completion_source_json
        .map(|value| {
            serde_json::from_str(&value).map_err(|source| {
                ToolInvocationLedgerStoreError::InvalidStoredJson {
                    field: "completion_source_json",
                    source,
                }
            })
        })
        .transpose()?;
    if attempt_count > u64::from(u32::MAX) {
        return Err(ToolInvocationLedgerStoreError::InvalidAttemptCount(
            attempt_count,
        ));
    }
    let state = parse_state(&state_raw)?;
    let dispatch_certainty = parse_certainty(&certainty_raw)?;
    let dispatch_lease = match (dispatch_owner, dispatch_lease_expires_at_epoch_ms) {
        (Some(owner_id), Some(expires_at_epoch_ms)) => Some(ToolInvocationDispatchLease::new(
            owner_id,
            expires_at_epoch_ms,
        )?),
        (None, None) => None,
        _ => return Err(ToolInvocationLedgerStoreError::IncompleteDispatchLease),
    };
    let required = if completion_source.is_some() {
        DispatchCertainty::NotDispatched
    } else {
        state.required_dispatch_certainty()
    };
    if dispatch_certainty != required {
        return Err(ToolInvocationLedgerStoreError::CertaintyMismatch {
            state,
            expected: required,
            actual: dispatch_certainty,
        });
    }
    let record = ToolInvocationRecord {
        identity: identity.clone(),
        fingerprint,
        decision,
        state,
        dispatch_certainty,
        attempt_count: attempt_count as u32,
        dispatch_lease,
        outcome,
        completion_source,
    };
    record.validate()?;
    Ok(record)
}

async fn rollback(tx: Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "tool invocation ledger rollback failed");
    }
}

fn validate_lease_input(
    owner_id: &str,
    lease_duration_ms: u64,
) -> Result<(), ToolInvocationLedgerStoreError> {
    ToolInvocationDispatchLease::new(owner_id, 1)?;
    if lease_duration_ms == 0 {
        return Err(ToolInvocationLedgerStoreError::InvalidLeaseDuration);
    }
    Ok(())
}

fn lease_duration_us(lease_duration_ms: u64) -> Result<i64, ToolInvocationLedgerStoreError> {
    lease_duration_ms
        .checked_mul(1_000)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or(ToolInvocationLedgerStoreError::LeaseDurationOverflow)
}

fn ensure_dispatched_owner(
    identity: &ToolInvocationIdentity,
    record: &ToolInvocationRecord,
    owner_id: &str,
) -> Result<(), ToolInvocationLedgerStoreError> {
    if record.state != ToolInvocationState::Dispatched {
        return Err(ToolInvocationLedgerStoreError::StateMismatch {
            identity: identity.clone(),
            expected: ToolInvocationState::Dispatched,
            actual: record.state,
        });
    }
    let lease = record
        .dispatch_lease
        .as_ref()
        .ok_or(ToolInvocationLedgerStoreError::IncompleteDispatchLease)?;
    if lease.owner_id != owner_id {
        return Err(ToolInvocationLedgerStoreError::DispatchOwnerMismatch {
            identity: identity.clone(),
        });
    }
    Ok(())
}

fn completion_owner<'a>(
    expected: ToolInvocationState,
    owner_id: Option<&'a str>,
    identity: &ToolInvocationIdentity,
) -> Result<(&'static str, Option<&'a str>), ToolInvocationLedgerStoreError> {
    if expected != ToolInvocationState::Dispatched {
        return Ok(("", None));
    }
    let owner_id =
        owner_id.ok_or_else(|| ToolInvocationLedgerStoreError::DispatchOwnerRequired {
            identity: identity.clone(),
        })?;
    ToolInvocationDispatchLease::new(owner_id, 1)?;
    Ok((" AND dispatch_owner = ?", Some(owner_id)))
}

fn state_label(state: ToolInvocationState) -> &'static str {
    match state {
        ToolInvocationState::Prepared => "prepared",
        ToolInvocationState::Dispatched => "dispatched",
        ToolInvocationState::Succeeded => "succeeded",
        ToolInvocationState::Failed => "failed",
        ToolInvocationState::Rejected => "rejected",
        ToolInvocationState::OutcomeUnknown => "outcome_unknown",
    }
}

fn parse_state(value: &str) -> Result<ToolInvocationState, ToolInvocationLedgerStoreError> {
    match value {
        "prepared" => Ok(ToolInvocationState::Prepared),
        "dispatched" => Ok(ToolInvocationState::Dispatched),
        "succeeded" => Ok(ToolInvocationState::Succeeded),
        "failed" => Ok(ToolInvocationState::Failed),
        "rejected" => Ok(ToolInvocationState::Rejected),
        "outcome_unknown" => Ok(ToolInvocationState::OutcomeUnknown),
        other => Err(ToolInvocationLedgerStoreError::InvalidState(
            other.to_string(),
        )),
    }
}

fn parse_certainty(value: &str) -> Result<DispatchCertainty, ToolInvocationLedgerStoreError> {
    match value {
        "not_dispatched" => Ok(DispatchCertainty::NotDispatched),
        "dispatched" => Ok(DispatchCertainty::Dispatched),
        "unknown" => Ok(DispatchCertainty::Unknown),
        other => Err(ToolInvocationLedgerStoreError::InvalidDispatchCertainty(
            other.to_string(),
        )),
    }
}

#[cfg(test)]
fn certainty_label(certainty: DispatchCertainty) -> &'static str {
    match certainty {
        DispatchCertainty::NotDispatched => "not_dispatched",
        DispatchCertainty::Dispatched => "dispatched",
        DispatchCertainty::Unknown => "unknown",
    }
}

#[derive(Debug, Error)]
pub enum ToolInvocationLedgerStoreError {
    #[error("tool invocation ledger database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("serialize tool invocation ledger {field}: {source}")]
    Serialization {
        field: &'static str,
        source: serde_json::Error,
    },
    #[error("invalid stored tool invocation ledger {field}: {source}")]
    InvalidStoredJson {
        field: &'static str,
        source: serde_json::Error,
    },
    #[error("tool invocation identity conflicts with its durable fingerprint: {identity:?}")]
    IdentityConflict { identity: ToolInvocationIdentity },
    #[error("tool invocation run {run_id} was not found for user {user_id}")]
    RunNotFound { user_id: String, run_id: String },
    #[error(
        "tool invocation run {run_id} belongs to session {actual_session_id}, not {expected_session_id}"
    )]
    RunSessionMismatch {
        run_id: String,
        expected_session_id: String,
        actual_session_id: String,
    },
    #[error("tool invocation run {run_id} is not executable while status is {status}")]
    RunNotExecutable { run_id: String, status: String },
    #[error("tool invocation run {run_id} is not terminal while status is {status}")]
    RunNotTerminal { run_id: String, status: String },
    #[error(
        "tool invocation run {run_id} still has {non_terminal_count} prepared or dispatched rows"
    )]
    RunNotQuiescent {
        run_id: String,
        non_terminal_count: u64,
    },
    #[error(
        "tool invocation identity key mismatch for {identity:?}: expected {expected}, actual {actual}"
    )]
    IdentityKeyMismatch {
        identity: Box<ToolInvocationIdentity>,
        expected: String,
        actual: String,
    },
    #[error("tool invocation archive chunk is {actual_bytes} bytes; maximum is {max_bytes}")]
    ArchiveChunkTooLarge {
        actual_bytes: usize,
        max_bytes: usize,
    },
    #[error("tool invocation compaction returned invalid row count {0}")]
    InvalidCompactionCount(i64),
    #[error("invalid tool invocation archive artifact {artifact_id}: {source}")]
    InvalidArchive {
        artifact_id: String,
        source: serde_json::Error,
    },
    #[error("tool invocation archive artifact {artifact_id} has mismatched owner or run scope")]
    ArchiveScopeMismatch { artifact_id: String },
    #[error("tool invocation archive artifact {artifact_id} is unavailable with status {status:?}")]
    ArchiveUnavailable {
        artifact_id: String,
        status: Option<String>,
    },
    #[error("overlapping tool invocation archive ranges for {identity:?}")]
    OverlappingArchiveRanges { identity: ToolInvocationIdentity },
    #[error("tool invocation record is no longer retained for terminal run: {identity:?}")]
    TerminalRunRecordUnavailable { identity: ToolInvocationIdentity },
    #[error("tool invocation disappeared after prepare: {identity:?}")]
    MissingAfterPrepare { identity: ToolInvocationIdentity },
    #[error("stored tool invocation is missing its frozen decision")]
    MissingDecision,
    #[error("stored tool invocation has an incomplete dispatch lease")]
    IncompleteDispatchLease,
    #[error("tool invocation dispatch lease duration must be positive")]
    InvalidLeaseDuration,
    #[error("tool invocation dispatch lease duration exceeds database range")]
    LeaseDurationOverflow,
    #[error("dispatch owner does not own invocation: {identity:?}")]
    DispatchOwnerMismatch { identity: ToolInvocationIdentity },
    #[error("dispatch owner is required to complete invocation: {identity:?}")]
    DispatchOwnerRequired { identity: ToolInvocationIdentity },
    #[error("tool invocation not found: {identity:?}")]
    NotFound { identity: ToolInvocationIdentity },
    #[error(
        "tool invocation state compare-and-set failed for {identity:?}: expected {expected:?}, actual {actual:?}"
    )]
    StateMismatch {
        identity: ToolInvocationIdentity,
        expected: ToolInvocationState,
        actual: ToolInvocationState,
    },
    #[error("illegal tool invocation transition: {from:?} -> {to:?}")]
    IllegalTransition {
        from: ToolInvocationState,
        to: ToolInvocationState,
    },
    #[error(
        "dispatch certainty {actual:?} is inconsistent with state {state:?}; expected {expected:?}"
    )]
    CertaintyMismatch {
        state: ToolInvocationState,
        expected: DispatchCertainty,
        actual: DispatchCertainty,
    },
    #[error("invalid stored tool invocation state '{0}'")]
    InvalidState(String),
    #[error("invalid stored tool invocation dispatch certainty '{0}'")]
    InvalidDispatchCertainty(String),
    #[error("invalid stored tool invocation attempt_count {0}")]
    InvalidAttemptCount(u64),
    #[error(transparent)]
    Contract(#[from] astra_turn_types::ToolInvocationContractError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_and_certainty_wire_labels_round_trip_exhaustively() {
        for state in [
            ToolInvocationState::Prepared,
            ToolInvocationState::Dispatched,
            ToolInvocationState::Succeeded,
            ToolInvocationState::Failed,
            ToolInvocationState::Rejected,
            ToolInvocationState::OutcomeUnknown,
        ] {
            assert_eq!(parse_state(state_label(state)).unwrap(), state);
        }
        for certainty in [
            DispatchCertainty::NotDispatched,
            DispatchCertainty::Dispatched,
            DispatchCertainty::Unknown,
        ] {
            assert_eq!(
                parse_certainty(certainty_label(certainty)).unwrap(),
                certainty
            );
        }
    }

    #[test]
    fn unknown_wire_labels_fail_loudly() {
        assert!(matches!(
            parse_state("success"),
            Err(ToolInvocationLedgerStoreError::InvalidState(_))
        ));
        assert!(matches!(
            parse_certainty("maybe"),
            Err(ToolInvocationLedgerStoreError::InvalidDispatchCertainty(_))
        ));
    }
}
