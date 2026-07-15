//! MatrixOne-backed durable tool invocation ledger.
//!
//! This store owns persistence and atomic compare-and-set. Semantic result
//! caching is deliberately outside this module.

use astra_core::SharedPool;
use astra_turn_types::{
    DispatchCertainty, ToolInvocationDecision, ToolInvocationFingerprint, ToolInvocationIdentity,
    ToolInvocationPrepareOutcome, ToolInvocationRecord, ToolInvocationState,
    ToolInvocationTerminalOutcome,
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
        let inserted = sqlx::query(
            "INSERT IGNORE INTO tool_invocation_ledger (
                user_id, session_id, run_id, turn_chain_id, invocation_id,
                fingerprint_json, decision_json, state, dispatch_certainty, attempt_count,
                created_at, updated_at
             ) VALUES (?, ?, ?, ?, ?, ?, ?, 'prepared', 'not_dispatched', 0,
                       CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))",
        )
        .bind(&identity.user_id)
        .bind(&identity.session_id)
        .bind(&identity.run_id)
        .bind(&identity.turn_chain_id)
        .bind(&identity.invocation_id)
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
        if matches!(
            next,
            ToolInvocationState::Succeeded
                | ToolInvocationState::Failed
                | ToolInvocationState::Rejected
        ) {
            return Err(ToolInvocationLedgerStoreError::TerminalOutcomeRequired { state: next });
        }
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

    /// Atomically persist an acknowledged terminal outcome with its state.
    /// A replay can observe either the pre-terminal state or the complete
    /// typed outcome, never a terminal marker without replay material.
    pub async fn compare_and_complete(
        &self,
        identity: &ToolInvocationIdentity,
        expected: ToolInvocationState,
        outcome: &ToolInvocationTerminalOutcome,
    ) -> Result<ToolInvocationRecord, ToolInvocationLedgerStoreError> {
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
        let updated = sqlx::query(
            "UPDATE tool_invocation_ledger
             SET state = ?, dispatch_certainty = 'dispatched', outcome_json = ?,
                 updated_at = CURRENT_TIMESTAMP(6)
             WHERE user_id = ? AND session_id = ? AND run_id = ?
               AND turn_chain_id = ? AND invocation_id = ? AND state = ?",
        )
        .bind(state_label(next))
        .bind(outcome_json)
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
            rollback(tx, "compare-and-complete mismatch").await;
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
                CAST(decision_json AS CHAR) AS decision_json,
                CAST(outcome_json AS CHAR) AS outcome_json,
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
    let record = ToolInvocationRecord {
        identity: identity.clone(),
        fingerprint,
        decision,
        state,
        dispatch_certainty,
        attempt_count: attempt_count as u32,
        outcome,
    };
    record.validate()?;
    Ok(record)
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
    #[error("stored tool invocation is missing its frozen decision")]
    MissingDecision,
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
    #[error("terminal state {state:?} requires a typed invocation outcome")]
    TerminalOutcomeRequired { state: ToolInvocationState },
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
