//! Complete action evidence from the canonical Run journal, not its UI projection.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifiedRunActionGrant {
    pub action_id: String,
    pub owner_generation: u64,
    pub event_index: i64,
    pub event_id: String,
    pub event_hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedRunActionHistory {
    pub last_event_index: i64,
    pub run_generation: u64,
    pub status: String,
    pub grants: Vec<VerifiedRunActionGrant>,
}

#[derive(Debug, Error)]
pub enum RunActionHistoryError {
    #[error("Run action history database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Run action history unavailable for the exact owner, Session and Run")]
    Unavailable,
    #[error("Run action history integrity error: {0}")]
    Integrity(String),
}

/// Read every retained event in the caller's transaction and verify the entire
/// sequence against the Run watermark before exposing any admission grants.
/// The Run row lock fences journal appends and invocation compaction. Keep
/// dependent ledger reads in this transaction so that fence stays held.
pub async fn load_verified_run_action_history_in_transaction(
    tx: &mut sqlx::Transaction<'_, MySql>,
    user_id: &str,
    session_id: &str,
    run_id: &str,
) -> Result<VerifiedRunActionHistory, RunActionHistoryError> {
    let row = sqlx::query(
        "SELECT last_event_idx, run_generation, status FROM agent_runs
         WHERE user_id = ? AND session_id = ? AND run_id = ? FOR UPDATE",
    )
    .bind(user_id)
    .bind(session_id)
    .bind(run_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(RunActionHistoryError::Unavailable)?;
    let last_event_index: i64 = row.try_get("last_event_idx")?;
    let generation: i64 = row.try_get("run_generation")?;
    let run_generation = u64::try_from(generation)
        .map_err(|_| RunActionHistoryError::Integrity("negative Run generation".into()))?;
    if last_event_index < -1 {
        return Err(RunActionHistoryError::Integrity(
            "invalid Run event watermark".into(),
        ));
    }
    let mut history = VerifiedRunActionHistory {
        last_event_index,
        run_generation,
        status: row.try_get("status")?,
        grants: Vec::new(),
    };
    let mut next_index = 0_i64;
    let mut action_ids = HashSet::new();
    // Page without a total-result limit. Query beyond the watermark too: extra
    // journal rows are contradictory evidence, not an ignorable suffix.
    loop {
        let rows = sqlx::query(
            "SELECT session_id, event_idx, event_type, event_id, idempotency_key,
                    event_hash, request_id, payload_json FROM agent_run_events
             WHERE user_id = ? AND run_id = ?
             ORDER BY event_idx LIMIT 256 OFFSET ?",
        )
        .bind(user_id)
        .bind(run_id)
        .bind(next_index)
        .fetch_all(&mut **tx)
        .await?;
        if rows.is_empty() {
            break;
        }
        for row in rows {
            if row.try_get::<String, _>("session_id")? != session_id {
                return Err(RunActionHistoryError::Integrity(
                    "event Session mismatch".into(),
                ));
            }
            let payload_json: String = row.try_get("payload_json")?;
            let (receipt, payload) = decode_atomic_terminal_event_row(row, run_id)
                .map_err(RunActionHistoryError::Integrity)?;
            validate_history_event(
                &receipt,
                &payload,
                &payload_json,
                next_index,
                last_event_index,
            )?;
            if receipt.event_type == ACTION_ADMISSION_GRANTED_EVENT_TYPE {
                let grant = decode_grant(
                    &receipt,
                    &payload,
                    user_id,
                    session_id,
                    run_id,
                    run_generation,
                )?;
                if !action_ids.insert(grant.action_id.clone()) {
                    return Err(RunActionHistoryError::Integrity(
                        "duplicate action admission".into(),
                    ));
                }
                history.grants.push(grant);
            }
            next_index = next_index
                .checked_add(1)
                .ok_or_else(|| RunActionHistoryError::Integrity("event index overflow".into()))?;
        }
    }
    if next_index.checked_sub(1) != Some(last_event_index) {
        return Err(RunActionHistoryError::Integrity(
            "Run journal ends before its watermark".into(),
        ));
    }
    Ok(history)
}

fn validate_history_event(
    receipt: &AtomicRunTerminalEventReceipt,
    payload: &serde_json::Value,
    payload_json: &str,
    expected_index: i64,
    watermark: i64,
) -> Result<(), RunActionHistoryError> {
    if receipt.event_idx != expected_index || receipt.event_idx > watermark {
        return Err(RunActionHistoryError::Integrity(
            "Run journal is not contiguous through its watermark".into(),
        ));
    }
    if receipt.event_id.is_empty()
        || extract_event_type(payload) != receipt.event_type
        || extract_optional_string(payload, "idempotency_key") != receipt.idempotency_key
        || receipt.event_hash != sha256_hex(payload_json.as_bytes())
    {
        return Err(RunActionHistoryError::Integrity(
            "event identity, type or hash mismatch".into(),
        ));
    }
    if let Some(id) = extract_optional_string(payload, "event_id")
        .or_else(|| extract_optional_string(payload, "id"))
        && id != receipt.event_id
    {
        return Err(RunActionHistoryError::Integrity(
            "event payload identity mismatch".into(),
        ));
    }
    Ok(())
}

fn decode_grant(
    receipt: &AtomicRunTerminalEventReceipt,
    payload: &serde_json::Value,
    user_id: &str,
    session_id: &str,
    run_id: &str,
    run_generation: u64,
) -> Result<VerifiedRunActionGrant, RunActionHistoryError> {
    let data = &payload["data"];
    let action_id = data["action_id"]
        .as_str()
        .ok_or_else(|| RunActionHistoryError::Integrity("missing action identity".into()))?;
    let owner_generation = data["owner_generation"]
        .as_u64()
        .ok_or_else(|| RunActionHistoryError::Integrity("invalid admission generation".into()))?;
    let expected_control_epoch = data["expected_control_epoch"].as_i64().ok_or_else(|| {
        RunActionHistoryError::Integrity("invalid admission control epoch".into())
    })?;
    if owner_generation > run_generation || expected_control_epoch >= receipt.event_idx {
        return Err(RunActionHistoryError::Integrity(
            "admission is ahead of its Run history".into(),
        ));
    }
    let request = AtomicRunActionAdmissionRequest {
        user_id,
        run_id,
        expected_session_id: session_id,
        action_id,
        expected_control_epoch,
        expected_owner_generation: owner_generation,
    };
    validate_action_admission_request(request).map_err(RunActionHistoryError::Integrity)?;
    if *payload != action_admission_granted_event(request) {
        return Err(RunActionHistoryError::Integrity(
            "admission payload does not match its canonical identity".into(),
        ));
    }
    Ok(VerifiedRunActionGrant {
        action_id: action_id.into(),
        owner_generation,
        event_index: receipt.event_idx,
        event_id: receipt.event_id.clone(),
        event_hash: receipt.event_hash.clone(),
    })
}
