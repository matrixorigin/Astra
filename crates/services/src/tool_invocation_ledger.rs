//! MatrixOne-backed durable tool invocation ledger.
//!
//! This store owns persistence and atomic compare-and-set. Semantic result
//! caching is deliberately outside this module.

use astra_core::SharedPool;
use astra_turn_types::{
    DispatchCertainty, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationState,
};
use sqlx::{MySql, Row, Transaction};
use thiserror::Error;

#[derive(Clone)]
pub struct DatabaseToolInvocationLedger {
    pool: SharedPool,
}

impl DatabaseToolInvocationLedger {
    pub fn new(pool: SharedPool) -> Self {
        Self { pool }
    }

    /// Insert `Prepared` idempotently and return the authoritative row. An
    /// existing identity with a different fingerprint is a hard conflict.
    pub async fn prepare(
        &self,
        identity: &ToolInvocationIdentity,
        fingerprint: &ToolInvocationFingerprint,
    ) -> Result<ToolInvocationPrepareOutcome, ToolInvocationLedgerStoreError> {
        let fingerprint_json = serde_json::to_string(fingerprint).map_err(|source| {
            ToolInvocationLedgerStoreError::Serialization {
                field: "fingerprint_json",
                source,
            }
        })?;
        let mut tx = self.pool.get().begin().await?;
        let inserted = sqlx::query(
            "INSERT IGNORE INTO tool_invocation_ledger (
                user_id, session_id, run_id, turn_chain_id, invocation_id,
                fingerprint_json, state, dispatch_certainty, attempt_count,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, 'prepared', 'not_dispatched', 0,
                       CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .bind(&fingerprint_json)
        .execute(&mut *tx)
        .await?
        .rows_affected()
            == 1;

        let record = load_record_in_tx(&mut tx, identity).await?.ok_or_else(|| {
            ToolInvocationLedgerStoreError::MissingAfterPrepare {
                identity: identity.clone(),
            }
        })?;
        if record.fingerprint != *fingerprint {
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
        row.map(|row| decode_record(&row, identity)).transpose()
    }

    /// Atomic state transition. Only one worker can claim
    /// `Prepared -> Dispatched`; a zero-row update is decoded as missing or a
    /// compare-and-set conflict instead of silently succeeding.
    pub async fn compare_and_transition(
        &self,
        identity: &ToolInvocationIdentity,
        expected: ToolInvocationState,
        next: ToolInvocationState,
        dispatch_certainty: DispatchCertainty,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
        if !expected.can_transition_to(next) {
            return Err(ToolInvocationLedgerStoreError::IllegalTransition {
                from: expected,
                to: next,
            });
        }
        let required = next.required_dispatch_certainty();
        if dispatch_certainty != required {
            return Err(ToolInvocationLedgerStoreError::CertaintyMismatch {
                state: next,
                expected: required,
                actual: dispatch_certainty,
            });
        }
        let attempt_increment = u64::from(
            expected == ToolInvocationState::Prepared && next == ToolInvocationState::Dispatched,
        );
        let mut tx = self.pool.get().begin().await?;
        let updated = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = ?, dispatch_certainty = ?,
                 attempt_count = attempt_count + ?, updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ? AND state = ?",
        )
        .bind(state_label(next))
        .bind(certainty_label(dispatch_certainty))
        .bind(attempt_increment)
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
        .bind(state_label(expected))
        .execute(&mut *tx)
        .await?
        .rows_affected();

        let record = load_record_in_tx(&mut tx, identity).await?;
        if updated != 1 {
            rollback(tx, "compare-and-transition mismatch").await;
            return match record {
                Some(actual) => Err(ToolInvocationLedgerStoreError::StateMismatch {
                    identity: identity.clone(),
                    expected,
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
}

fn select_record_query(
    identity: &ToolInvocationIdentity,
) -> sqlx::query::Query<'_, MySql, sqlx::mysql::MySqlArguments> {
    sqlx::query(
        "SELECT CAST(fingerprint_json AS CHAR) AS fingerprint_json,
                state, dispatch_certainty, attempt_count
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
    let state_raw: String = row.try_get("state")?;
    let certainty_raw: String = row.try_get("dispatch_certainty")?;
    let attempt_count: u64 = row.try_get("attempt_count")?;
    if attempt_count > u64::from(u32::MAX) {
        return Err(ToolInvocationLedgerStoreError::InvalidAttemptCount(
            attempt_count,
        ));
    }
    let state = parse_state(&state_raw)?;
    let dispatch_certainty = parse_certainty(&certainty_raw)?;
    let required = state.required_dispatch_certainty();
    if dispatch_certainty != required {
        return Err(ToolInvocationLedgerStoreError::CertaintyMismatch {
            state,
            expected: required,
            actual: dispatch_certainty,
        });
    }
    Ok(ToolInvocationRecord {
        identity: identity.clone(),
        fingerprint,
        state,
        dispatch_certainty,
        attempt_count: attempt_count as u32,
    })
}

async fn rollback(tx: Transaction<'_, MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(context, %error, "tool invocation ledger rollback failed");
    }
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

fn certainty_label(certainty: DispatchCertainty) -> &'static str {
    match certainty {
        DispatchCertainty::NotDispatched => "not_dispatched",
        DispatchCertainty::Dispatched => "dispatched",
        DispatchCertainty::Unknown => "unknown",
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
    #[error("tool invocation disappeared after prepare: {identity:?}")]
    MissingAfterPrepare { identity: ToolInvocationIdentity },
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
