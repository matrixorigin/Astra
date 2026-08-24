use crate::db_row::RowExt as PromptDeltaDbRow;
use astra_core::{
    SharedPool, matrixone_null_shape_comment, matrixone_statement_with_null_shape,
    push_matrixone_bound_string_set,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const PROMPT_DIAGNOSTIC_RETENTION_DAYS: u32 = 90;
const EXPIRED_PROMPT_REQUESTS_SQL: &str =
    "SELECT request.user_id, request.session_id, request.request_id
     FROM prompt_request_records AS request
     LEFT JOIN agent_session_lifecycle_fences AS fence
       ON fence.user_id = request.user_id
      AND fence.session_id = request.session_id
     WHERE request.created_at_unix_ms < UNIX_TIMESTAMP(DATE_SUB(NOW(6), INTERVAL ? DAY)) * 1000
       AND (
           fence.database_deleted_at IS NOT NULL
           OR (
               fence.delete_requested_at IS NULL
               AND NOT EXISTS (
                   SELECT 1
                   FROM prompt_request_records AS child
                   INNER JOIN prompt_deltas AS child_delta
                     ON child_delta.user_id = child.user_id
                    AND child_delta.session_id = child.session_id
                    AND child_delta.request_id = child.request_id
                    AND child_delta.op = 'reuse_prefix'
                   WHERE child.user_id = request.user_id
                     AND child.session_id = request.session_id
                     AND child.previous_request_id = request.request_id
               )
           )
       )
     ORDER BY request.created_at_unix_ms ASC, request.user_id ASC, request.request_id ASC
     LIMIT ?";

const VALIDATE_EXPIRED_PROMPT_REQUEST_SQL: &str = "SELECT 1
     FROM prompt_request_records AS request
     WHERE request.user_id = ?
       AND request.session_id = ?
       AND request.request_id = ?
       AND request.created_at_unix_ms < UNIX_TIMESTAMP(DATE_SUB(NOW(6), INTERVAL ? DAY)) * 1000
       AND NOT EXISTS (
           SELECT 1
           FROM prompt_request_records AS child
           INNER JOIN prompt_deltas AS child_delta
             ON child_delta.user_id = child.user_id
            AND child_delta.session_id = child.session_id
            AND child_delta.request_id = child.request_id
            AND child_delta.op = 'reuse_prefix'
           WHERE child.user_id = request.user_id
             AND child.session_id = request.session_id
             AND child.previous_request_id = request.request_id
       )
     LIMIT 1
     FOR UPDATE";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PromptDiagnosticExpiry {
    pub prompt_request_records: u64,
    pub prompt_deltas: u64,
}

fn push_prompt_request_key_predicates<'a>(
    query: &mut sqlx::QueryBuilder<'a, sqlx::MySql>,
    request_keys: &'a [(String, String, String)],
) {
    for (index, (user_id, session_id, request_id)) in request_keys.iter().enumerate() {
        if index > 0 {
            query.push(" OR ");
        }
        query
            .push("(user_id = ")
            .push_bind(user_id)
            .push(" AND session_id = ")
            .push_bind(session_id)
            .push(" AND request_id = ")
            .push_bind(request_id)
            .push(")");
    }
}

fn delete_prompt_deltas_query(
    request_keys: &[(String, String, String)],
) -> sqlx::QueryBuilder<'_, sqlx::MySql> {
    let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new("DELETE FROM prompt_deltas WHERE ");
    push_prompt_request_key_predicates(&mut query, request_keys);
    query.push(" ORDER BY user_id ASC, session_id ASC, request_id ASC, delta_seq ASC");
    query
}

fn delete_prompt_requests_query(
    request_keys: &[(String, String, String)],
) -> sqlx::QueryBuilder<'_, sqlx::MySql> {
    let mut query =
        sqlx::QueryBuilder::<sqlx::MySql>::new("DELETE FROM prompt_request_records WHERE ");
    push_prompt_request_key_predicates(&mut query, request_keys);
    query
}

/// Expire high-volume prompt-assembly diagnostics independently from durable
/// conversation history. Candidates are selected through the retention index,
/// then revalidated under the session fence. A request remains until no child
/// `reuse_prefix` record needs it for reconstruction.
pub(crate) async fn expire_prompt_diagnostics(
    pool: &SharedPool,
    batch_limit: u32,
) -> Result<PromptDiagnosticExpiry, String> {
    let expired_requests =
        sqlx::query_as::<_, (String, String, String)>(EXPIRED_PROMPT_REQUESTS_SQL)
            .bind(PROMPT_DIAGNOSTIC_RETENTION_DAYS)
            .bind(batch_limit)
            .fetch_all(pool.get())
            .await
            .map_err(|error| format!("select expired prompt diagnostics: {error}"))?;
    if expired_requests.is_empty() {
        return Ok(PromptDiagnosticExpiry::default());
    }

    let mut candidates = expired_requests;
    candidates.sort();
    candidates.dedup();

    let mut tx = pool
        .get()
        .begin()
        .await
        .map_err(|error| format!("begin prompt diagnostic expiry: {error}"))?;
    let mut expired_requests = Vec::with_capacity(candidates.len());
    for (user_id, session_id, request_id) in candidates {
        match crate::storage::lock_or_claim_orphaned_agent_session_write_fence(
            &mut tx,
            &session_id,
            &user_id,
        )
        .await
        .map_err(|error| format!("lock prompt diagnostic expiry session fence: {error}"))?
        {
            crate::storage::AgentSessionWriteFenceState::Writable => {
                let eligible: Option<i32> = sqlx::query_scalar(VALIDATE_EXPIRED_PROMPT_REQUEST_SQL)
                    .bind(&user_id)
                    .bind(&session_id)
                    .bind(&request_id)
                    .bind(PROMPT_DIAGNOSTIC_RETENTION_DAYS)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|error| format!("validate expired prompt diagnostic: {error}"))?;
                if eligible.is_some() {
                    expired_requests.push((user_id, session_id, request_id));
                }
            }
            crate::storage::AgentSessionWriteFenceState::CompletedDelete => {
                expired_requests.push((user_id, session_id, request_id));
            }
            crate::storage::AgentSessionWriteFenceState::PendingDelete
            | crate::storage::AgentSessionWriteFenceState::Missing => continue,
        }
    }
    if expired_requests.is_empty() {
        tx.commit()
            .await
            .map_err(|error| format!("commit empty prompt diagnostic expiry: {error}"))?;
        return Ok(PromptDiagnosticExpiry::default());
    }

    let mut delete_deltas = delete_prompt_deltas_query(&expired_requests);
    let mut delete_requests = delete_prompt_requests_query(&expired_requests);
    let prompt_deltas = delete_deltas
        .build()
        .execute(&mut *tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|error| format!("expire prompt_deltas: {error}"))?;
    let prompt_request_records = delete_requests
        .build()
        .execute(&mut *tx)
        .await
        .map(|result| result.rows_affected())
        .map_err(|error| format!("expire prompt_request_records: {error}"))?;
    tx.commit()
        .await
        .map_err(|error| format!("commit prompt diagnostic expiry: {error}"))?;
    Ok(PromptDiagnosticExpiry {
        prompt_request_records,
        prompt_deltas,
    })
}

#[cfg(test)]
mod retention_tests {
    use super::*;

    #[test]
    fn prompt_diagnostic_expiry_is_indexed_bounded_and_child_first() {
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.contains("WHERE request.created_at_unix_ms <"));
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.contains(
            "ORDER BY request.created_at_unix_ms ASC, request.user_id ASC, request.request_id ASC"
        ));
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.ends_with("LIMIT ?"));
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.contains("fence.database_deleted_at IS NOT NULL"));
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.contains("fence.delete_requested_at IS NULL"));
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.contains("NOT EXISTS"));
        assert!(EXPIRED_PROMPT_REQUESTS_SQL.contains("reuse_prefix"));
        assert!(VALIDATE_EXPIRED_PROMPT_REQUEST_SQL.contains("NOT EXISTS"));
        assert!(VALIDATE_EXPIRED_PROMPT_REQUEST_SQL.contains("reuse_prefix"));
        assert!(VALIDATE_EXPIRED_PROMPT_REQUEST_SQL.ends_with("FOR UPDATE"));

        let keys = vec![(
            "user-1".to_string(),
            "session-1".to_string(),
            "request-1".to_string(),
        )];
        let delete_deltas = delete_prompt_deltas_query(&keys);
        let delete_requests = delete_prompt_requests_query(&keys);
        assert!(delete_deltas.sql().starts_with("DELETE FROM prompt_deltas"));
        assert!(delete_deltas.sql().contains("delta_seq ASC"));
        assert!(
            delete_requests
                .sql()
                .starts_with("DELETE FROM prompt_request_records")
        );
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PromptDeltaCounts {
    pub reuse: u32,
    pub append: u32,
    pub replace: u32,
    pub drop: u32,
    /// Token-weighted evidence uses one versioned, tokenizer-independent
    /// estimator. Provider usage remains authoritative.
    #[serde(default)]
    pub token_weights: PromptDeltaTokenWeights,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PromptDeltaTokenWeights {
    /// Combined with `prompt_request_records.provider`, this identifies the
    /// provider/tokenizer namespace of every cached chunk weight.
    pub tokenizer_revision: String,
    /// False when a previous chunk had no persisted weight or came from a
    /// different provider/tokenizer namespace.
    pub complete: bool,
    pub reuse: u64,
    pub append: u64,
    pub replace_before: u64,
    pub replace_after: u64,
    pub drop: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRequestPlan {
    pub request_id: String,
    pub request_hash: String,
    pub message_count: u32,
    pub tool_count: u32,
    pub max_output_tokens: Option<u32>,
    pub summary_json: Value,
    chunks: Vec<PromptChunkPlan>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PromptChunkPlan {
    logical_key: String,
    chunk_kind: String,
    position: i32,
    chunk_id: String,
    chunk_hash: String,
    estimated_tokens: u64,
    serialized_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRequestPersistInput {
    pub session_id: String,
    pub user_id: String,
    pub run_id: Option<String>,
    pub turn: u32,
    pub round: u32,
    pub attempt: u32,
    pub source: String,
    pub model: String,
    pub provider: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptRequestPersistResult {
    pub request_id: String,
    pub request_hash: String,
    pub previous_request_id: Option<String>,
    pub message_count: u32,
    pub tool_count: u32,
    pub delta_counts: PromptDeltaCounts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequestObservability {
    pub request_id: String,
    pub request_hash: String,
    pub message_count: u32,
    pub tool_count: u32,
    pub delta_counts: PromptDeltaCounts,
}

async fn rollback_prompt_delta_tx(tx: sqlx::Transaction<'_, sqlx::MySql>, context: &'static str) {
    if let Err(error) = tx.rollback().await {
        tracing::warn!(target: "astra_services::prompt_delta", context, %error, "prompt delta transaction rollback failed");
    }
}

fn prompt_delta_row_string(row: &impl PromptDeltaDbRow, column: &str) -> Result<String, String> {
    row.string_column(column)
        .map_err(|error| format!("prompt delta row decode column `{column}`: {error}"))
}

fn prompt_delta_row_optional_string(
    row: &impl PromptDeltaDbRow,
    column: &str,
) -> Result<Option<String>, String> {
    row.optional_string_column(column)
        .map_err(|error| format!("prompt delta row decode column `{column}`: {error}"))
}

fn prompt_delta_row_i64(row: &impl PromptDeltaDbRow, column: &str) -> Result<i64, String> {
    row.i64_column(column)
        .map_err(|error| format!("prompt delta row decode column `{column}`: {error}"))
}

fn prompt_delta_row_u32(row: &impl PromptDeltaDbRow, column: &str) -> Result<u32, String> {
    let value = prompt_delta_row_i64(row, column)?;
    u32::try_from(value)
        .map_err(|_| format!("prompt delta row column `{column}` out of u32 range: {value}"))
}

fn prompt_delta_row_u64(row: &impl PromptDeltaDbRow, column: &str) -> Result<u64, String> {
    let value = prompt_delta_row_i64(row, column)?;
    u64::try_from(value)
        .map_err(|_| format!("prompt delta row column `{column}` out of u64 range: {value}"))
}

fn prompt_delta_summary_value(row: &impl PromptDeltaDbRow) -> Result<Value, String> {
    let summary_json = prompt_delta_row_string(row, "summary_json")?;
    serde_json::from_str(&summary_json)
        .map_err(|error| format!("prompt request summary_json decode failed: {error}"))
}

fn prompt_delta_counts_from_summary(summary_value: &Value) -> Result<PromptDeltaCounts, String> {
    let delta_counts = summary_value
        .get("delta_counts")
        .ok_or_else(|| "prompt request summary_json missing `delta_counts`".to_string())?;
    serde_json::from_value(delta_counts.clone())
        .map_err(|error| format!("prompt request delta_counts decode failed: {error}"))
}

fn decode_prompt_request_count(row: &impl PromptDeltaDbRow) -> Result<u32, String> {
    prompt_delta_row_u32(row, "total")
}

fn decode_prompt_observability(
    row: &impl PromptDeltaDbRow,
) -> Result<PromptRequestObservability, String> {
    let summary_value = prompt_delta_summary_value(row)?;
    Ok(PromptRequestObservability {
        request_id: prompt_delta_row_string(row, "request_id")?,
        request_hash: prompt_delta_row_string(row, "request_hash")?,
        message_count: prompt_delta_row_u32(row, "message_count")?,
        tool_count: prompt_delta_row_u32(row, "tool_count")?,
        delta_counts: prompt_delta_counts_from_summary(&summary_value)?,
    })
}

fn decode_prompt_persist_result(
    row: &impl PromptDeltaDbRow,
) -> Result<PromptRequestPersistResult, String> {
    let summary_value = prompt_delta_summary_value(row)?;
    Ok(PromptRequestPersistResult {
        request_id: prompt_delta_row_string(row, "request_id")?,
        request_hash: prompt_delta_row_string(row, "request_hash")?,
        previous_request_id: prompt_delta_row_optional_string(row, "previous_request_id")?,
        message_count: prompt_delta_row_u32(row, "message_count")?,
        tool_count: prompt_delta_row_u32(row, "tool_count")?,
        delta_counts: prompt_delta_counts_from_summary(&summary_value)?,
    })
}

fn decode_previous_request_id(row: &impl PromptDeltaDbRow) -> Result<String, String> {
    prompt_delta_row_string(row, "request_id")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviousPromptRequest {
    request_id: String,
    provider: String,
    tokenizer_revision: Option<String>,
}

const PROMPT_DELTA_CHECKPOINT_REQUESTS: usize = 64;
const MAX_PROMPT_DELTA_CHAIN_REQUESTS: usize = 4_096;

fn decode_previous_prompt_request(
    row: &impl PromptDeltaDbRow,
) -> Result<PreviousPromptRequest, String> {
    let summary = prompt_delta_summary_value(row)?;
    Ok(PreviousPromptRequest {
        request_id: decode_previous_request_id(row)?,
        provider: prompt_delta_row_string(row, "provider")?,
        tokenizer_revision: summary
            .pointer("/delta_counts/token_weights/tokenizer_revision")
            .and_then(Value::as_str)
            .map(String::from),
    })
}

fn common_prefix_len_for_storage(
    current: &[PromptChunkPlan],
    previous: &[ExistingPromptChunk],
    previous_chain_request_count: usize,
) -> usize {
    if previous_chain_request_count >= PROMPT_DELTA_CHECKPOINT_REQUESTS {
        return 0;
    }
    current
        .iter()
        .zip(previous)
        .take_while(|(current, previous)| {
            current.logical_key == previous.logical_key && current.chunk_hash == previous.chunk_hash
        })
        .count()
}

fn decode_existing_prompt_chunk(
    row: &impl PromptDeltaDbRow,
) -> Result<ExistingPromptChunk, String> {
    Ok(ExistingPromptChunk {
        logical_key: prompt_delta_row_string(row, "logical_key")?,
        chunk_hash: prompt_delta_row_string(row, "chunk_hash")?,
        estimated_tokens: prompt_delta_row_u64(row, "chunk_tokens")?,
        serialized_bytes: prompt_delta_row_u64(row, "chunk_bytes")?,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredPromptDelta {
    request_id: String,
    depth: u32,
    delta_seq: u32,
    op: String,
    position: u32,
    reuse_count: u32,
    chunk: Option<ExistingPromptChunk>,
}

fn decode_stored_prompt_delta(
    row: &impl PromptDeltaDbRow,
    depth: u32,
) -> Result<StoredPromptDelta, String> {
    let op = prompt_delta_row_string(row, "op")?;
    let chunk_hash = prompt_delta_row_string(row, "chunk_hash")?;
    let chunk = match op.as_str() {
        "reuse_prefix" | "drop" => None,
        _ => {
            if chunk_hash.is_empty() {
                return Err(format!(
                    "prompt delta row op `{op}` requires a non-empty chunk_hash"
                ));
            }
            Some(decode_existing_prompt_chunk(row)?)
        }
    };
    Ok(StoredPromptDelta {
        request_id: prompt_delta_row_string(row, "request_id")?,
        depth,
        delta_seq: prompt_delta_row_u32(row, "delta_seq")?,
        op,
        position: prompt_delta_row_u32(row, "position")?,
        reuse_count: prompt_delta_row_u32(row, "reuse_count")?,
        chunk,
    })
}

pub struct PromptRequestPlanInput<'a> {
    pub user_id: &'a str,
    pub session_id: &'a str,
    pub turn: u32,
    pub round: u32,
    pub attempt: u32,
    pub source: &'a str,
    pub messages: &'a [Value],
    pub tools: &'a [Value],
    pub max_output_tokens: Option<usize>,
}

pub fn plan_prompt_request(input: PromptRequestPlanInput<'_>) -> Result<PromptRequestPlan, String> {
    let mut chunks = Vec::with_capacity(input.messages.len() + input.tools.len());
    for (index, message) in input.messages.iter().enumerate() {
        let logical_key = format!(
            "message:{index}:{}",
            message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
        );
        chunks.push(build_chunk_plan(
            &logical_key,
            "message",
            index as i32,
            message,
        )?);
    }
    let message_count = chunks.len() as u32;
    for (index, tool) in input.tools.iter().enumerate() {
        let logical_key = format!("tool:{index}:{}", tool_identity(tool));
        chunks.push(build_chunk_plan(
            &logical_key,
            "tool",
            (message_count as i32) + index as i32,
            tool,
        )?);
    }
    let tool_count = input.tools.len() as u32;
    let max_output_tokens_u32 = input
        .max_output_tokens
        .map(|value| value.min(u32::MAX as usize) as u32);
    let summary_json = json!({
        "message_roles": input.messages.iter().map(message_role_summary).collect::<Vec<_>>(),
        "tool_names": input.tools.iter().map(tool_identity).collect::<Vec<_>>(),
        "max_output_tokens": max_output_tokens_u32,
        "message_count": message_count,
        "tool_count": tool_count,
    });
    let request_hash = hash_prompt_plan(&chunks, max_output_tokens_u32);
    Ok(PromptRequestPlan {
        request_id: prompt_request_id(
            input.user_id,
            input.session_id,
            input.turn,
            input.round,
            input.attempt,
            input.source,
        ),
        request_hash,
        message_count,
        tool_count,
        max_output_tokens: max_output_tokens_u32,
        summary_json,
        chunks,
    })
}

pub async fn persist_prompt_request(
    pool: &SharedPool,
    input: &PromptRequestPersistInput,
    plan: &PromptRequestPlan,
) -> Result<PromptRequestPersistResult, String> {
    let db = pool.get();
    let mut tx = db.begin().await.map_err(|error| error.to_string())?;
    crate::storage::lock_agent_session_write_fence(&mut tx, &input.session_id, &input.user_id)
        .await
        .map_err(|error| format!("lock prompt diagnostic session fence: {error}"))?;
    ensure_session_owner(&mut tx, &input.session_id, &input.user_id).await?;
    if let Some(existing) = load_existing_request(&mut tx, input, &plan.request_id).await? {
        return existing_prompt_request_or_conflict(input, plan, existing);
    }

    let previous_request = load_previous_request(&mut tx, input).await?;
    let previous_state = if let Some(previous_request) = previous_request.as_ref() {
        load_request_chunks(&mut tx, input, &previous_request.request_id).await?
    } else {
        LoadedPromptChunks::default()
    };
    let previous_chunks = previous_state.chunks;
    let previous_request_id = previous_request
        .as_ref()
        .map(|request| request.request_id.clone());

    // Materialize one full request periodically. Individual unchanged chunks
    // still count as reuse, but the checkpoint no longer depends on its
    // predecessor for reconstruction.
    let common_prefix_len = common_prefix_len_for_storage(
        &plan.chunks,
        &previous_chunks,
        previous_state.chain_request_count,
    );
    let mut previous_map = previous_chunks
        .into_iter()
        .skip(common_prefix_len)
        .map(|chunk| (chunk.logical_key.clone(), chunk))
        .collect::<std::collections::HashMap<_, _>>();
    let mut delta_counts = PromptDeltaCounts::default();
    delta_counts.token_weights.tokenizer_revision =
        astra_turn_types::token_estimate::CANONICAL_JSON_TOKENIZER_REVISION.to_string();
    delta_counts.token_weights.complete = previous_request.as_ref().is_none_or(|request| {
        request.provider == input.provider
            && request.tokenizer_revision.as_deref()
                == Some(astra_turn_types::token_estimate::CANONICAL_JSON_TOKENIZER_REVISION)
    });
    let mut delta_seq: i32 = 0;
    let mut delta_rows = Vec::with_capacity(
        plan.chunks
            .len()
            .saturating_sub(common_prefix_len)
            .saturating_add(previous_map.len())
            .saturating_add(usize::from(common_prefix_len > 0)),
    );
    if common_prefix_len > 0 {
        let reuse_count = u32::try_from(common_prefix_len)
            .map_err(|_| "prompt reuse prefix exceeds u32 range".to_string())?;
        let reused_tokens = plan.chunks[..common_prefix_len]
            .iter()
            .fold(0_u64, |total, chunk| {
                total.saturating_add(chunk.estimated_tokens)
            });
        let reused_bytes = plan.chunks[..common_prefix_len]
            .iter()
            .fold(0_u64, |total, chunk| {
                total.saturating_add(chunk.serialized_bytes)
            });
        delta_counts.reuse = reuse_count;
        delta_counts.token_weights.reuse = reused_tokens;
        delta_rows.push(PlannedPromptDelta {
            delta_seq,
            logical_key: format!("prefix:{reuse_count}"),
            chunk_kind: "prefix".to_string(),
            position: 0,
            op: "reuse_prefix",
            reuse_count: Some(reuse_count),
            chunk_id: None,
            chunk_hash: None,
            previous_chunk_hash: None,
            chunk_tokens: Some(reused_tokens),
            chunk_bytes: Some(reused_bytes),
            previous_chunk_tokens: Some(reused_tokens),
            previous_chunk_bytes: Some(reused_bytes),
        });
        delta_seq = delta_seq.saturating_add(1);
    }
    for chunk in &plan.chunks[common_prefix_len..] {
        let previous = previous_map.remove(&chunk.logical_key);
        let previous_hash = previous.as_ref().map(|chunk| chunk.chunk_hash.clone());
        let op = if previous_hash.as_deref() == Some(chunk.chunk_hash.as_str()) {
            delta_counts.reuse = delta_counts.reuse.saturating_add(1);
            delta_counts.token_weights.reuse = delta_counts
                .token_weights
                .reuse
                .saturating_add(chunk.estimated_tokens);
            "reuse"
        } else if previous_hash.is_some() {
            delta_counts.replace = delta_counts.replace.saturating_add(1);
            if previous
                .as_ref()
                .is_some_and(|chunk| chunk.serialized_bytes == 0)
            {
                delta_counts.token_weights.complete = false;
            }
            delta_counts.token_weights.replace_before = delta_counts
                .token_weights
                .replace_before
                .saturating_add(previous.as_ref().map_or(0, |chunk| chunk.estimated_tokens));
            delta_counts.token_weights.replace_after = delta_counts
                .token_weights
                .replace_after
                .saturating_add(chunk.estimated_tokens);
            "replace"
        } else {
            delta_counts.append = delta_counts.append.saturating_add(1);
            delta_counts.token_weights.append = delta_counts
                .token_weights
                .append
                .saturating_add(chunk.estimated_tokens);
            "append"
        };
        delta_rows.push(PlannedPromptDelta {
            delta_seq,
            logical_key: chunk.logical_key.clone(),
            chunk_kind: chunk.chunk_kind.clone(),
            position: chunk.position,
            op,
            reuse_count: None,
            chunk_id: Some(chunk.chunk_id.clone()),
            chunk_hash: Some(chunk.chunk_hash.clone()),
            previous_chunk_hash: previous_hash,
            chunk_tokens: Some(chunk.estimated_tokens),
            chunk_bytes: Some(chunk.serialized_bytes),
            previous_chunk_tokens: previous.as_ref().map(|chunk| chunk.estimated_tokens),
            previous_chunk_bytes: previous.as_ref().map(|chunk| chunk.serialized_bytes),
        });
        delta_seq = delta_seq.saturating_add(1);
    }
    for (logical_key, previous) in previous_map {
        delta_counts.drop = delta_counts.drop.saturating_add(1);
        if previous.serialized_bytes == 0 {
            delta_counts.token_weights.complete = false;
        }
        delta_counts.token_weights.drop = delta_counts
            .token_weights
            .drop
            .saturating_add(previous.estimated_tokens);
        delta_rows.push(PlannedPromptDelta {
            delta_seq,
            logical_key,
            chunk_kind: "drop".to_string(),
            position: delta_seq,
            op: "drop",
            reuse_count: None,
            chunk_id: None,
            chunk_hash: None,
            previous_chunk_hash: Some(previous.chunk_hash),
            chunk_tokens: None,
            chunk_bytes: None,
            previous_chunk_tokens: Some(previous.estimated_tokens),
            previous_chunk_bytes: Some(previous.serialized_bytes),
        });
        delta_seq = delta_seq.saturating_add(1);
    }

    let summary_json = json!({
        "summary": plan.summary_json.clone(),
        "delta_counts": delta_counts,
    });

    let write_result: Result<(), String> = async {
        let request_insert_sql = matrixone_statement_with_null_shape(
            "INSERT INTO prompt_request_records
             (request_id, session_id, user_id, run_id, turn, round, attempt, source,
              model, provider, max_output_tokens, message_count, tool_count,
              previous_request_id, request_hash, summary_json, created_at, created_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), UNIX_TIMESTAMP(NOW(6)) * 1000)",
            [
                input.run_id.is_some(),
                plan.max_output_tokens.is_some(),
                previous_request_id.is_some(),
            ],
        );
        sqlx::query(&request_insert_sql)
        .bind(&plan.request_id)
        .bind(&input.session_id)
        .bind(&input.user_id)
        .bind(&input.run_id)
        .bind(input.turn as i64)
        .bind(input.round as i64)
        .bind(input.attempt as i64)
        .bind(&input.source)
        .bind(&input.model)
        .bind(&input.provider)
        .bind(plan.max_output_tokens.map(i64::from))
        .bind(i64::from(plan.message_count))
        .bind(i64::from(plan.tool_count))
        .bind(&previous_request_id)
        .bind(&plan.request_hash)
        .bind(summary_json.to_string())
        .execute(&mut *tx)
        .await
        .map_err(|error| error.to_string())?;

        for batch in delta_rows.chunks(PROMPT_DELTA_INSERT_BATCH_ROWS) {
            insert_prompt_delta_batch(&mut tx, input, &plan.request_id, batch).await?;
        }
        Ok(())
    }
    .await;

    if let Err(error) = write_result {
        rollback_prompt_delta_tx(tx, "persist_prompt_request write failure").await;
        let mut recovery = db.acquire().await.map_err(|source| source.to_string())?;
        if let Some(existing) =
            load_existing_request(&mut recovery, input, &plan.request_id).await?
        {
            return existing_prompt_request_or_conflict(input, plan, existing);
        }
        return Err(error);
    }

    tx.commit().await.map_err(|error| error.to_string())?;
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_rows(
            astra_core::history_work::HistoryWorkSite::PromptDeltaRows,
            1_u64.saturating_add(delta_rows.len().try_into().unwrap_or(u64::MAX)),
        );
        let (unchanged_prefix_bytes, unchanged_prefix_rows) =
            unchanged_prefix_work(input, &plan.request_id, &delta_rows);
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::PromptDeltaUnchangedPrefix,
            unchanged_prefix_bytes,
            unchanged_prefix_rows,
            0,
        );
    }
    Ok(PromptRequestPersistResult {
        request_id: plan.request_id.clone(),
        request_hash: plan.request_hash.clone(),
        previous_request_id,
        message_count: plan.message_count,
        tool_count: plan.tool_count,
        delta_counts,
    })
}

pub async fn load_latest_prompt_observability_for_run(
    pool: &SharedPool,
    user_id: &str,
    run_id: &str,
) -> Result<Option<PromptRequestObservability>, String> {
    load_latest_prompt_observability(
        pool,
        "SELECT request_id, request_hash, message_count, tool_count, summary_json
         FROM prompt_request_records
         WHERE user_id = ? AND run_id = ?
         ORDER BY turn DESC, round DESC, attempt DESC, created_at DESC, request_id DESC
         LIMIT 1",
        user_id,
        run_id,
    )
    .await
}

pub async fn load_latest_prompt_observability_for_session(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> Result<Option<PromptRequestObservability>, String> {
    load_latest_prompt_observability(
        pool,
        "SELECT request_id, request_hash, message_count, tool_count, summary_json
         FROM prompt_request_records
         WHERE user_id = ? AND session_id = ?
         ORDER BY turn DESC, round DESC, attempt DESC, created_at DESC, request_id DESC
         LIMIT 1",
        user_id,
        session_id,
    )
    .await
}

pub async fn count_prompt_requests_for_run(
    pool: &SharedPool,
    user_id: &str,
    run_id: &str,
) -> Result<u32, String> {
    count_prompt_requests(
        pool,
        "SELECT COUNT(*) AS total FROM prompt_request_records WHERE user_id = ? AND run_id = ?",
        user_id,
        run_id,
    )
    .await
}

pub async fn count_prompt_requests_for_session(
    pool: &SharedPool,
    user_id: &str,
    session_id: &str,
) -> Result<u32, String> {
    count_prompt_requests(
        pool,
        "SELECT COUNT(*) AS total FROM prompt_request_records WHERE user_id = ? AND session_id = ?",
        user_id,
        session_id,
    )
    .await
}

async fn count_prompt_requests(
    pool: &SharedPool,
    sql: &str,
    user_id: &str,
    bind_value: &str,
) -> Result<u32, String> {
    let row = sqlx::query(sql)
        .bind(user_id)
        .bind(bind_value)
        .fetch_one(pool.get())
        .await
        .map_err(|error| error.to_string())?;
    decode_prompt_request_count(&row)
}

async fn load_latest_prompt_observability(
    pool: &SharedPool,
    sql: &str,
    user_id: &str,
    bind_value: &str,
) -> Result<Option<PromptRequestObservability>, String> {
    let row = sqlx::query(sql)
        .bind(user_id)
        .bind(bind_value)
        .fetch_optional(pool.get())
        .await
        .map_err(|error| error.to_string())?;
    row.map(|row| decode_prompt_observability(&row)).transpose()
}

fn existing_prompt_request_or_conflict(
    input: &PromptRequestPersistInput,
    plan: &PromptRequestPlan,
    existing: PromptRequestPersistResult,
) -> Result<PromptRequestPersistResult, String> {
    if existing.request_hash == plan.request_hash {
        return Ok(existing);
    }
    Err(format!(
        "prompt_request_records idempotency conflict for request_id={} user_id={} session_id={}: existing request_hash {} != planned {}",
        plan.request_id, input.user_id, input.session_id, existing.request_hash, plan.request_hash
    ))
}

async fn load_existing_request(
    connection: &mut sqlx::MySqlConnection,
    input: &PromptRequestPersistInput,
    request_id: &str,
) -> Result<Option<PromptRequestPersistResult>, String> {
    let row = sqlx::query(
        "SELECT request_id, request_hash, previous_request_id, message_count, tool_count, summary_json
         FROM prompt_request_records
         WHERE request_id = ? AND user_id = ? AND session_id = ?",
    )
    .bind(request_id)
    .bind(&input.user_id)
    .bind(&input.session_id)
    .fetch_optional(connection)
    .await
    .map_err(|error| error.to_string())?;
    row.map(|row| decode_prompt_persist_result(&row))
        .transpose()
}

async fn ensure_session_owner(
    connection: &mut sqlx::MySqlConnection,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let exists: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(connection)
    .await
    .map_err(|error| error.to_string())?;
    if exists.is_some() {
        Ok(())
    } else {
        Err(format!(
            "prompt_request_records owner mismatch for session_id={session_id} user_id={user_id}: agent_sessions owner root missing or belongs to another user"
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExistingPromptChunk {
    logical_key: String,
    chunk_hash: String,
    estimated_tokens: u64,
    serialized_bytes: u64,
}

#[derive(Default)]
struct LoadedPromptChunks {
    chunks: Vec<ExistingPromptChunk>,
    chain_request_count: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedPromptDelta {
    delta_seq: i32,
    logical_key: String,
    chunk_kind: String,
    position: i32,
    op: &'static str,
    reuse_count: Option<u32>,
    chunk_id: Option<String>,
    chunk_hash: Option<String>,
    previous_chunk_hash: Option<String>,
    chunk_tokens: Option<u64>,
    chunk_bytes: Option<u64>,
    previous_chunk_tokens: Option<u64>,
    previous_chunk_bytes: Option<u64>,
}

fn string_bytes(value: &str) -> u64 {
    value.len().try_into().unwrap_or(u64::MAX)
}

fn planned_prompt_delta_payload_bytes(
    input: &PromptRequestPersistInput,
    request_id: &str,
    delta: &PlannedPromptDelta,
) -> u64 {
    [
        string_bytes(&input.user_id),
        string_bytes(&input.session_id),
        string_bytes(request_id),
        string_bytes(&delta.logical_key),
        string_bytes(&delta.chunk_kind),
        string_bytes(delta.op),
        delta.chunk_id.as_deref().map_or(0, string_bytes),
        delta.chunk_hash.as_deref().map_or(0, string_bytes),
        delta.previous_chunk_hash.as_deref().map_or(0, string_bytes),
        u64::try_from(
            std::mem::size_of::<i32>() * 2
                + std::mem::size_of::<u32>()
                + std::mem::size_of::<u64>() * 4,
        )
        .unwrap_or(u64::MAX),
    ]
    .into_iter()
    .fold(0_u64, u64::saturating_add)
}

fn unchanged_prefix_work(
    input: &PromptRequestPersistInput,
    request_id: &str,
    delta_rows: &[PlannedPromptDelta],
) -> (u64, u64) {
    delta_rows
        .iter()
        .take_while(|delta| matches!(delta.op, "reuse_prefix" | "reuse"))
        .fold((0_u64, 0_u64), |(bytes, rows), delta| {
            (
                bytes.saturating_add(planned_prompt_delta_payload_bytes(input, request_id, delta)),
                rows.saturating_add(1),
            )
        })
}

async fn load_previous_request(
    connection: &mut sqlx::MySqlConnection,
    input: &PromptRequestPersistInput,
) -> Result<Option<PreviousPromptRequest>, String> {
    sqlx::query(
        "SELECT request_id, provider, CAST(summary_json AS CHAR) AS summary_json
         FROM prompt_request_records
         WHERE user_id = ? AND session_id = ? AND source = ?
           AND (
               turn < ?
               OR (turn = ? AND round < ?)
               OR (turn = ? AND round = ? AND attempt < ?)
           )
         ORDER BY turn DESC, round DESC, attempt DESC, created_at DESC, request_id DESC
         LIMIT 1",
    )
    .bind(&input.user_id)
    .bind(&input.session_id)
    .bind(&input.source)
    .bind(i64::from(input.turn))
    .bind(i64::from(input.turn))
    .bind(i64::from(input.round))
    .bind(i64::from(input.turn))
    .bind(i64::from(input.round))
    .bind(i64::from(input.attempt))
    .fetch_optional(connection)
    .await
    .map_err(|error| error.to_string())
    .and_then(|row| {
        row.map(|row| decode_previous_prompt_request(&row))
            .transpose()
    })
}

async fn load_request_chunks(
    connection: &mut sqlx::MySqlConnection,
    input: &PromptRequestPersistInput,
    request_id: &str,
) -> Result<LoadedPromptChunks, String> {
    let link_rows = sqlx::query(
        "SELECT request.request_id,
                request.previous_request_id,
                MAX(CASE WHEN delta.op = 'reuse_prefix' THEN 1 ELSE 0 END) AS has_reuse_prefix
         FROM prompt_request_records AS request
         LEFT JOIN prompt_deltas AS delta
           ON delta.user_id = request.user_id
          AND delta.session_id = request.session_id
          AND delta.request_id = request.request_id
          AND delta.op = 'reuse_prefix'
         WHERE request.user_id = ? AND request.session_id = ? AND request.source = ?
         GROUP BY request.request_id, request.previous_request_id,
                  request.created_at, request.turn, request.round, request.attempt
         ORDER BY request.turn DESC, request.round DESC,
                  request.attempt DESC, request.created_at DESC, request.request_id DESC
         LIMIT ?",
    )
    .bind(&input.user_id)
    .bind(&input.session_id)
    .bind(&input.source)
    .bind(
        i64::try_from(MAX_PROMPT_DELTA_CHAIN_REQUESTS + 1)
            .map_err(|_| "prompt delta chain limit exceeds BIGINT range".to_string())?,
    )
    .fetch_all(&mut *connection)
    .await
    .map_err(|error| error.to_string())?;
    let mut links = std::collections::HashMap::with_capacity(link_rows.len());
    for row in link_rows {
        let request_id = prompt_delta_row_string(&row, "request_id")?;
        let previous_request_id =
            row.optional_string_column("previous_request_id")
                .map_err(|error| {
                    format!(
                        "prompt delta chain decode column `previous_request_id` failed: {error}"
                    )
                })?;
        let has_reuse_prefix = prompt_delta_row_i64(&row, "has_reuse_prefix")? == 1;
        links.insert(request_id, (previous_request_id, has_reuse_prefix));
    }

    let mut chain = Vec::new();
    let mut current_request_id = request_id.to_string();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current_request_id.clone()) {
            return Err(format!(
                "prompt delta chain contains a cycle at request_id={current_request_id}"
            ));
        }
        let (previous_request_id, has_reuse_prefix) =
            links.get(&current_request_id).cloned().ok_or_else(|| {
                format!("prompt delta chain metadata is missing request_id={current_request_id}")
            })?;
        let depth = u32::try_from(chain.len())
            .map_err(|_| "prompt delta chain depth exceeds u32 range".to_string())?;
        chain.push((
            current_request_id.clone(),
            previous_request_id.clone(),
            depth,
        ));
        if !has_reuse_prefix {
            break;
        }
        if chain.len() >= MAX_PROMPT_DELTA_CHAIN_REQUESTS {
            return Err(format!(
                "prompt delta chain for request_id={request_id} exceeds {MAX_PROMPT_DELTA_CHAIN_REQUESTS} requests"
            ));
        }
        current_request_id = previous_request_id.ok_or_else(|| {
            format!(
                "prompt request {current_request_id} has a reuse_prefix without a previous request"
            )
        })?;
    }

    let depth_by_request = chain
        .iter()
        .map(|(request_id, _, depth)| (request_id.as_str(), *depth))
        .collect::<std::collections::HashMap<_, _>>();
    let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "SELECT delta.request_id,
                delta.delta_seq,
                delta.op,
                delta.position,
                COALESCE(delta.reuse_count, 0) AS reuse_count,
                delta.logical_key,
                COALESCE(delta.chunk_hash, '') AS chunk_hash,
                COALESCE(delta.chunk_tokens, 0) AS chunk_tokens,
                COALESCE(delta.chunk_bytes, 0) AS chunk_bytes
         FROM prompt_deltas AS delta
         INNER JOIN prompt_request_records AS request
           ON request.user_id = delta.user_id
          AND request.session_id = delta.session_id
          AND request.request_id = delta.request_id
         INNER JOIN ",
    );
    push_matrixone_bound_string_set(
        &mut query,
        chain.iter().map(|(request_id, _, _)| request_id.as_str()),
    );
    query
        .push(" AS selected_request ON selected_request.value = delta.request_id")
        .push(" WHERE delta.user_id = ")
        .push_bind(&input.user_id)
        .push(" AND delta.session_id = ")
        .push_bind(&input.session_id);
    let rows = query
        .build()
        .fetch_all(&mut *connection)
        .await
        .map_err(|error| error.to_string())?;
    let mut deltas =
        rows.into_iter()
            .map(|row| {
                let request_id = prompt_delta_row_string(&row, "request_id")?;
                let depth = depth_by_request.get(request_id.as_str()).copied().ok_or_else(|| {
                format!("prompt delta row references request outside selected chain: {request_id}")
            })?;
                decode_stored_prompt_delta(&row, depth)
            })
            .collect::<Result<Vec<_>, String>>()?;
    deltas.sort_by(|left, right| {
        right
            .depth
            .cmp(&left.depth)
            .then_with(|| left.delta_seq.cmp(&right.delta_seq))
    });
    let chunks = reconstruct_prompt_chunks(&deltas)?;
    if astra_core::history_work::instrumentation_enabled() {
        let bytes = chunks.iter().fold(0_u64, |total, chunk| {
            total
                .saturating_add(chunk.logical_key.len().try_into().unwrap_or(u64::MAX))
                .saturating_add(chunk.chunk_hash.len().try_into().unwrap_or(u64::MAX))
        });
        astra_core::history_work::record_operation(
            astra_core::history_work::HistoryWorkSite::PromptDeltaRead,
            bytes,
            chunks.len().try_into().unwrap_or(u64::MAX),
            0,
        );
    }
    Ok(LoadedPromptChunks {
        chunks,
        chain_request_count: chain.len(),
    })
}

fn reconstruct_prompt_chunks(
    deltas: &[StoredPromptDelta],
) -> Result<Vec<ExistingPromptChunk>, String> {
    let mut chunks = Vec::new();
    let mut seen_requests = std::collections::HashSet::new();
    let mut index = 0;
    while index < deltas.len() {
        let request_id = deltas[index].request_id.clone();
        if !seen_requests.insert(request_id.clone()) {
            return Err(format!(
                "prompt delta chain contains a cycle at request_id={request_id}"
            ));
        }
        let group_start = index;
        while index < deltas.len() && deltas[index].request_id == request_id {
            index += 1;
        }
        let group = &deltas[group_start..index];
        let mut reuse_prefix = group.iter().filter(|delta| delta.op == "reuse_prefix");
        if let Some(prefix) = reuse_prefix.next() {
            if reuse_prefix.next().is_some() {
                return Err(format!(
                    "prompt request {request_id} has more than one reuse_prefix row"
                ));
            }
            let prefix_len = usize::try_from(prefix.reuse_count)
                .map_err(|_| "prompt reuse prefix exceeds usize range".to_string())?;
            if prefix_len == 0 || prefix_len > chunks.len() {
                return Err(format!(
                    "prompt request {request_id} has invalid reuse prefix {prefix_len} over {} inherited chunks",
                    chunks.len()
                ));
            }
            chunks.truncate(prefix_len);
        } else {
            chunks.clear();
        }
        for delta in group {
            let Some(chunk) = delta.chunk.as_ref() else {
                continue;
            };
            let expected_position = u32::try_from(chunks.len())
                .map_err(|_| "prompt chunk position exceeds u32 range".to_string())?;
            if delta.position != expected_position {
                return Err(format!(
                    "prompt request {request_id} chunk position {} does not follow reconstructed position {expected_position}",
                    delta.position
                ));
            }
            chunks.push(chunk.clone());
        }
    }
    Ok(chunks)
}

const PROMPT_DELTA_INSERT_BATCH_ROWS: usize = 128;

async fn insert_prompt_delta_batch(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    input: &PromptRequestPersistInput,
    request_id: &str,
    deltas: &[PlannedPromptDelta],
) -> Result<(), String> {
    if deltas.is_empty() {
        return Ok(());
    }
    let mut query = sqlx::QueryBuilder::<sqlx::MySql>::new(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, reuse_count, chunk_id, chunk_hash, previous_chunk_hash,
          chunk_tokens, chunk_bytes, previous_chunk_tokens, previous_chunk_bytes) ",
    );
    query.push_values(deltas, |mut row, delta| {
        row.push_bind(&input.user_id)
            .push_bind(&input.session_id)
            .push_bind(request_id)
            .push_bind(delta.delta_seq)
            .push_bind(&delta.logical_key)
            .push_bind(&delta.chunk_kind)
            .push_bind(delta.position)
            .push_bind(delta.op)
            .push_bind(delta.reuse_count.map(i64::from))
            .push_bind(delta.chunk_id.as_deref())
            .push_bind(delta.chunk_hash.as_deref())
            .push_bind(delta.previous_chunk_hash.as_deref())
            .push_bind(
                delta
                    .chunk_tokens
                    .and_then(|value| i64::try_from(value).ok()),
            )
            .push_bind(
                delta
                    .chunk_bytes
                    .and_then(|value| i64::try_from(value).ok()),
            )
            .push_bind(
                delta
                    .previous_chunk_tokens
                    .and_then(|value| i64::try_from(value).ok()),
            )
            .push_bind(
                delta
                    .previous_chunk_bytes
                    .and_then(|value| i64::try_from(value).ok()),
            );
    });
    query.push(matrixone_null_shape_comment(deltas.iter().flat_map(
        |delta| {
            [
                delta.reuse_count.is_some(),
                delta.chunk_id.is_some(),
                delta.chunk_hash.is_some(),
                delta.previous_chunk_hash.is_some(),
                delta.chunk_tokens.is_some(),
                delta.chunk_bytes.is_some(),
                delta.previous_chunk_tokens.is_some(),
                delta.previous_chunk_bytes.is_some(),
            ]
        },
    )));
    query
        .build()
        .execute(&mut **tx)
        .await
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn build_chunk_plan(
    logical_key: &str,
    chunk_kind: &str,
    position: i32,
    payload: &Value,
) -> Result<PromptChunkPlan, String> {
    let payload_json = astra_core::canonical_json_string(payload);
    if astra_core::history_work::instrumentation_enabled() {
        astra_core::history_work::record_bytes(
            astra_core::history_work::HistoryWorkSite::PromptDeltaHash,
            payload_json.len().try_into().unwrap_or(u64::MAX),
        );
    }
    let chunk_hash = sha256_hex(payload_json.as_bytes());
    let estimated_tokens =
        astra_turn_types::token_estimate::estimate_canonical_json_tokens(&payload_json);
    Ok(PromptChunkPlan {
        logical_key: logical_key.to_string(),
        chunk_kind: chunk_kind.to_string(),
        position,
        chunk_id: format!("pchunk-{chunk_hash}"),
        chunk_hash,
        estimated_tokens,
        serialized_bytes: payload_json.len().try_into().unwrap_or(u64::MAX),
    })
}

fn hash_prompt_plan(chunks: &[PromptChunkPlan], max_output_tokens: Option<u32>) -> String {
    let mut digest = Sha256::new();
    digest.update(b"astra.prompt-plan.v2\0");
    match max_output_tokens {
        Some(tokens) => {
            digest.update([1]);
            digest.update(tokens.to_be_bytes());
        }
        None => digest.update([0]),
    }
    digest.update((chunks.len() as u64).to_be_bytes());
    for chunk in chunks {
        digest.update((chunk.logical_key.len() as u64).to_be_bytes());
        digest.update(chunk.logical_key.as_bytes());
        digest.update(chunk.position.to_be_bytes());
        digest.update(chunk.chunk_hash.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn prompt_request_id(
    user_id: &str,
    session_id: &str,
    turn: u32,
    round: u32,
    attempt: u32,
    source: &str,
) -> String {
    let digest =
        sha256_hex(format!("{user_id}|{session_id}|{turn}|{round}|{attempt}|{source}").as_bytes());
    format!("promptreq-{}", &digest[..24])
}

fn tool_identity(tool: &Value) -> String {
    tool.get("function")
        .and_then(|value| value.get("name"))
        .and_then(Value::as_str)
        .or_else(|| tool.get("name").and_then(Value::as_str))
        .unwrap_or("unknown")
        .to_string()
}

fn message_role_summary(message: &Value) -> Value {
    json!({
        "role": message.get("role").and_then(Value::as_str).unwrap_or("unknown"),
        "has_name": message.get("name").is_some(),
        "content_kind": content_kind(message.get("content")),
    })
}

fn content_kind(content: Option<&Value>) -> &'static str {
    match content {
        Some(Value::Array(_)) => "array",
        Some(Value::Object(_)) => "object",
        Some(Value::String(_)) => "string",
        Some(_) => "other",
        None => "missing",
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    format!("{digest:x}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Clone)]
    struct FakePromptDeltaRow {
        failed_column: Option<&'static str>,
        summary_json: String,
        previous_request_id: Option<String>,
        i64_overrides: Vec<(&'static str, i64)>,
    }

    impl FakePromptDeltaRow {
        fn complete() -> Self {
            Self {
                failed_column: None,
                summary_json: json!({
                    "summary": {"message_roles": []},
                    "delta_counts": {
                        "reuse": 1,
                        "append": 2,
                        "replace": 3,
                        "drop": 4
                    }
                })
                .to_string(),
                previous_request_id: Some("previous-request-1".to_string()),
                i64_overrides: Vec::new(),
            }
        }

        fn fail_on(column: &'static str) -> Self {
            Self {
                failed_column: Some(column),
                ..Self::complete()
            }
        }

        fn with_summary_json(summary_json: impl Into<String>) -> Self {
            Self {
                summary_json: summary_json.into(),
                ..Self::complete()
            }
        }

        fn with_i64(column: &'static str, value: i64) -> Self {
            Self {
                i64_overrides: vec![(column, value)],
                ..Self::complete()
            }
        }

        fn fail_if_needed(&self, column: &str) -> Result<(), sqlx::Error> {
            if self.failed_column == Some(column) {
                Err(sqlx::Error::ColumnNotFound(column.to_string()))
            } else {
                Ok(())
            }
        }
    }

    impl PromptDeltaDbRow for FakePromptDeltaRow {
        fn string_column(&self, column: &str) -> Result<String, sqlx::Error> {
            self.fail_if_needed(column)?;
            Ok(match column {
                "request_id" => "request-1".to_string(),
                "request_hash" => "hash-1".to_string(),
                "provider" => "provider-a".to_string(),
                "summary_json" => self.summary_json.clone(),
                "logical_key" => "message:0:user".to_string(),
                "chunk_hash" => "chunk-hash-1".to_string(),
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }

        fn optional_string_column(&self, column: &str) -> Result<Option<String>, sqlx::Error> {
            self.fail_if_needed(column)?;
            match column {
                "previous_request_id" => Ok(self.previous_request_id.clone()),
                _ => Err(sqlx::Error::ColumnNotFound(column.to_string())),
            }
        }

        fn i64_column(&self, column: &str) -> Result<i64, sqlx::Error> {
            self.fail_if_needed(column)?;
            if let Some((_, value)) = self
                .i64_overrides
                .iter()
                .find(|(candidate, _)| *candidate == column)
            {
                return Ok(*value);
            }
            Ok(match column {
                "total" => 7,
                "message_count" => 2,
                "tool_count" => 3,
                "chunk_tokens" => 11,
                "chunk_bytes" => 44,
                _ => return Err(sqlx::Error::ColumnNotFound(column.to_string())),
            })
        }
    }

    fn assert_decode_error_mentions(result: Result<impl std::fmt::Debug, String>, needle: &str) {
        let err = result.expect_err("decode should fail");
        assert!(
            err.contains(needle),
            "error should contain `{needle}`, got `{err}`"
        );
    }

    #[test]
    fn plan_prompt_request_hash_is_order_stable_for_object_keys() {
        let messages_a = [json!({"role": "system", "content": {"b": 2, "a": 1}})];
        let plan_a = plan_prompt_request(PromptRequestPlanInput {
            user_id: "user-1",
            session_id: "session-1",
            turn: 1,
            round: 0,
            attempt: 0,
            source: "server_loop_host",
            messages: &messages_a,
            tools: &[],
            max_output_tokens: Some(1024),
        })
        .expect("plan");
        let messages_b = [json!({"content": {"a": 1, "b": 2}, "role": "system"})];
        let plan_b = plan_prompt_request(PromptRequestPlanInput {
            user_id: "user-1",
            session_id: "session-1",
            turn: 1,
            round: 0,
            attempt: 0,
            source: "server_loop_host",
            messages: &messages_b,
            tools: &[],
            max_output_tokens: Some(1024),
        })
        .expect("plan");
        assert_eq!(plan_a.request_hash, plan_b.request_hash);
        assert_eq!(plan_a.chunks[0].chunk_hash, plan_b.chunks[0].chunk_hash);
    }

    #[test]
    fn prompt_hash_distinguishes_absent_and_zero_output_limits() {
        let messages = [json!({"role": "user", "content": "hello"})];
        let plan = |max_output_tokens| {
            plan_prompt_request(PromptRequestPlanInput {
                user_id: "owner-a",
                session_id: "session-a",
                turn: 1,
                round: 0,
                attempt: 0,
                source: "test",
                messages: &messages,
                tools: &[],
                max_output_tokens,
            })
            .expect("plan")
        };

        assert_ne!(plan(None).request_hash, plan(Some(0)).request_hash);
    }

    #[test]
    fn long_session_tail_change_preserves_prefix_chunk_identity_and_weights() {
        let mut before = (0..2_048)
            .map(|index| {
                json!({
                    "role": if index % 2 == 0 { "user" } else { "assistant" },
                    "content": format!("{index}:{}", "context".repeat(64)),
                })
            })
            .collect::<Vec<_>>();
        let first = plan_prompt_request(PromptRequestPlanInput {
            user_id: "owner-a",
            session_id: "long-session",
            turn: 1_024,
            round: 0,
            attempt: 0,
            source: "server_loop_host",
            messages: &before,
            tools: &[],
            max_output_tokens: Some(16_384),
        })
        .expect("first long-session plan");
        before.last_mut().expect("tail")["content"] = json!("volatile replacement");
        let second = plan_prompt_request(PromptRequestPlanInput {
            user_id: "owner-a",
            session_id: "long-session",
            turn: 1_025,
            round: 0,
            attempt: 0,
            source: "server_loop_host",
            messages: &before,
            tools: &[],
            max_output_tokens: Some(16_384),
        })
        .expect("second long-session plan");

        assert_ne!(first.request_hash, second.request_hash);
        assert_eq!(first.chunks.len(), 2_048);
        assert!(
            first.chunks[..2_047]
                .iter()
                .zip(&second.chunks[..2_047])
                .all(|(left, right)| left.chunk_hash == right.chunk_hash
                    && left.estimated_tokens == right.estimated_tokens
                    && left.serialized_bytes == right.serialized_bytes),
            "changing only the volatile tail must preserve every stable prefix identity"
        );
        assert_ne!(
            first.chunks[2_047].chunk_hash,
            second.chunks[2_047].chunk_hash
        );
    }

    #[test]
    fn long_session_delta_rows_use_bounded_batches() {
        let batch_count = |rows: usize| rows.div_ceil(PROMPT_DELTA_INSERT_BATCH_ROWS);
        assert_eq!(batch_count(0), 0);
        assert_eq!(batch_count(1), 1);
        assert_eq!(batch_count(PROMPT_DELTA_INSERT_BATCH_ROWS), 1);
        assert_eq!(batch_count(PROMPT_DELTA_INSERT_BATCH_ROWS + 1), 2);
        assert_eq!(batch_count(2_048), 16);
    }

    #[test]
    fn long_session_delta_chain_materializes_at_a_fixed_interval() {
        let plan = plan_prompt_request(PromptRequestPlanInput {
            user_id: "owner",
            session_id: "session",
            turn: 1,
            round: 0,
            attempt: 0,
            source: "server",
            messages: &[json!({"role": "user", "content": "stable"})],
            tools: &[],
            max_output_tokens: None,
        })
        .expect("plan");
        let previous = plan
            .chunks
            .iter()
            .map(|chunk| ExistingPromptChunk {
                logical_key: chunk.logical_key.clone(),
                chunk_hash: chunk.chunk_hash.clone(),
                estimated_tokens: chunk.estimated_tokens,
                serialized_bytes: chunk.serialized_bytes,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            common_prefix_len_for_storage(
                &plan.chunks,
                &previous,
                PROMPT_DELTA_CHECKPOINT_REQUESTS - 1
            ),
            1
        );
        assert_eq!(
            common_prefix_len_for_storage(
                &plan.chunks,
                &previous,
                PROMPT_DELTA_CHECKPOINT_REQUESTS
            ),
            0,
            "the checkpoint must break ancestry instead of extending a long-session chain"
        );
    }

    #[test]
    fn reuse_prefix_reconstructs_one_exact_current_chunk_sequence() {
        let chunk = |request_id: &str, depth, position, hash: &str| StoredPromptDelta {
            request_id: request_id.to_string(),
            depth,
            delta_seq: position,
            op: "append".to_string(),
            position,
            reuse_count: 0,
            chunk: Some(ExistingPromptChunk {
                logical_key: format!("message:{position}:user"),
                chunk_hash: hash.to_string(),
                estimated_tokens: 10,
                serialized_bytes: 40,
            }),
        };
        let deltas = vec![
            chunk("request-1", 1, 0, "old-0"),
            chunk("request-1", 1, 1, "old-1"),
            chunk("request-1", 1, 2, "old-2"),
            StoredPromptDelta {
                request_id: "request-2".to_string(),
                depth: 0,
                delta_seq: 0,
                op: "reuse_prefix".to_string(),
                position: 0,
                reuse_count: 2,
                chunk: None,
            },
            chunk("request-2", 0, 2, "new-2"),
        ];

        let reconstructed = reconstruct_prompt_chunks(&deltas).expect("reconstruct");
        assert_eq!(
            reconstructed
                .iter()
                .map(|chunk| chunk.chunk_hash.as_str())
                .collect::<Vec<_>>(),
            ["old-0", "old-1", "new-2"]
        );
    }

    #[test]
    fn reuse_prefix_rejects_missing_or_oversized_ancestry() {
        let prefix = StoredPromptDelta {
            request_id: "request-2".to_string(),
            depth: 0,
            delta_seq: 0,
            op: "reuse_prefix".to_string(),
            position: 0,
            reuse_count: 1,
            chunk: None,
        };
        let error = reconstruct_prompt_chunks(&[prefix]).expect_err("missing ancestry");
        assert!(error.contains("invalid reuse prefix"), "{error}");
    }

    #[test]
    fn unchanged_prefix_work_stops_at_first_changed_row() {
        let input = PromptRequestPersistInput {
            session_id: "session-1".to_string(),
            user_id: "user-1".to_string(),
            run_id: None,
            turn: 2,
            round: 0,
            attempt: 0,
            source: "test".to_string(),
            model: "model".to_string(),
            provider: "provider".to_string(),
        };
        let delta = |delta_seq, op| PlannedPromptDelta {
            delta_seq,
            logical_key: format!("message:{delta_seq}:user"),
            chunk_kind: "message".to_string(),
            position: delta_seq,
            op,
            reuse_count: None,
            chunk_id: Some(format!("chunk-{delta_seq}")),
            chunk_hash: Some(format!("hash-{delta_seq}")),
            previous_chunk_hash: Some(format!("previous-{delta_seq}")),
            chunk_tokens: Some(10),
            chunk_bytes: Some(40),
            previous_chunk_tokens: Some(10),
            previous_chunk_bytes: Some(40),
        };
        let rows = vec![
            delta(0, "reuse"),
            delta(1, "reuse"),
            delta(2, "replace"),
            delta(3, "reuse"),
        ];

        let (bytes, row_count) = unchanged_prefix_work(&input, "request-1", &rows);

        assert_eq!(row_count, 2);
        assert_eq!(
            bytes,
            planned_prompt_delta_payload_bytes(&input, "request-1", &rows[0]).saturating_add(
                planned_prompt_delta_payload_bytes(&input, "request-1", &rows[1],)
            ),
            "a later reuse row is not part of the unchanged prefix"
        );
    }

    #[test]
    fn plan_prompt_request_summarizes_roles_and_tools() {
        let messages = [
            json!({"role": "system", "content": "sys"}),
            json!({"role": "user", "content": [{"type": "text", "text": "hi"}]}),
        ];
        let tools = [json!({"function": {"name": "bash"}})];
        let plan = plan_prompt_request(PromptRequestPlanInput {
            user_id: "user-1",
            session_id: "session-1",
            turn: 2,
            round: 1,
            attempt: 0,
            source: "bridge_inprocess",
            messages: &messages,
            tools: &tools,
            max_output_tokens: Some(512),
        })
        .expect("plan");
        assert_eq!(plan.message_count, 2);
        assert_eq!(plan.tool_count, 1);
        assert_eq!(plan.summary_json["tool_names"][0], "bash");
        assert_eq!(
            plan.summary_json["message_roles"][1]["content_kind"],
            "array"
        );
    }

    #[test]
    fn plan_prompt_request_id_is_owner_session_attempt_bound() {
        let messages = [json!({"role": "user", "content": "same prompt"})];
        let owner_a = plan_prompt_request(PromptRequestPlanInput {
            user_id: "owner-a",
            session_id: "shared-session",
            turn: 2,
            round: 1,
            attempt: 0,
            source: "bridge_inprocess",
            messages: &messages,
            tools: &[],
            max_output_tokens: None,
        })
        .expect("owner a plan");
        let owner_b = plan_prompt_request(PromptRequestPlanInput {
            user_id: "owner-b",
            session_id: "shared-session",
            turn: 2,
            round: 1,
            attempt: 0,
            source: "bridge_inprocess",
            messages: &messages,
            tools: &[],
            max_output_tokens: None,
        })
        .expect("owner b plan");

        assert_ne!(
            owner_a.request_id, owner_b.request_id,
            "prompt request ids must include owner identity so two owners with the same external session id never collide"
        );
        assert_eq!(
            owner_a.request_hash, owner_b.request_hash,
            "request hash should describe wire content, not ownership"
        );
    }

    #[test]
    fn existing_prompt_request_accepts_only_matching_hash() {
        let messages = [json!({"role": "user", "content": "same prompt"})];
        let plan = plan_prompt_request(PromptRequestPlanInput {
            user_id: "owner-a",
            session_id: "session-a",
            turn: 2,
            round: 1,
            attempt: 0,
            source: "bridge_inprocess",
            messages: &messages,
            tools: &[],
            max_output_tokens: None,
        })
        .expect("plan");
        let input = PromptRequestPersistInput {
            session_id: "session-a".to_string(),
            user_id: "owner-a".to_string(),
            run_id: None,
            turn: 2,
            round: 1,
            attempt: 0,
            source: "bridge_inprocess".to_string(),
            model: "test-model".to_string(),
            provider: "test".to_string(),
        };
        let existing = PromptRequestPersistResult {
            request_id: plan.request_id.clone(),
            request_hash: plan.request_hash.clone(),
            previous_request_id: None,
            message_count: 1,
            tool_count: 0,
            delta_counts: PromptDeltaCounts::default(),
        };
        assert!(
            existing_prompt_request_or_conflict(&input, &plan, existing.clone()).is_ok(),
            "same idempotency key and same request hash should be a replay"
        );

        let conflicting = PromptRequestPersistResult {
            request_hash: "different-hash".to_string(),
            ..existing
        };
        let error = existing_prompt_request_or_conflict(&input, &plan, conflicting)
            .expect_err("same id with different payload hash must fail");
        assert!(error.contains("idempotency conflict"));
        assert!(error.contains(&plan.request_id));
    }

    #[test]
    fn prompt_request_count_decode_fails_loudly() {
        assert_eq!(
            decode_prompt_request_count(&FakePromptDeltaRow::complete()).expect("count decodes"),
            7
        );
        assert_decode_error_mentions(
            decode_prompt_request_count(&FakePromptDeltaRow::fail_on("total")),
            "decode column `total`",
        );
        assert_decode_error_mentions(
            decode_prompt_request_count(&FakePromptDeltaRow::with_i64("total", -1)),
            "out of u32 range",
        );
        assert_decode_error_mentions(
            decode_prompt_request_count(&FakePromptDeltaRow::with_i64(
                "total",
                i64::from(u32::MAX) + 1,
            )),
            "out of u32 range",
        );
    }

    #[test]
    fn prompt_observability_decode_preserves_values_and_fails_loudly() {
        let observability = decode_prompt_observability(&FakePromptDeltaRow::complete())
            .expect("observability decodes");
        assert_eq!(observability.request_id, "request-1");
        assert_eq!(observability.request_hash, "hash-1");
        assert_eq!(observability.message_count, 2);
        assert_eq!(observability.tool_count, 3);
        assert_eq!(
            observability.delta_counts,
            PromptDeltaCounts {
                reuse: 1,
                append: 2,
                replace: 3,
                drop: 4,
                token_weights: PromptDeltaTokenWeights::default(),
            }
        );

        for column in [
            "request_id",
            "request_hash",
            "message_count",
            "tool_count",
            "summary_json",
        ] {
            assert_decode_error_mentions(
                decode_prompt_observability(&FakePromptDeltaRow::fail_on(column)),
                &format!("`{column}`"),
            );
        }
        assert_decode_error_mentions(
            decode_prompt_observability(&FakePromptDeltaRow::with_summary_json("{not-json")),
            "summary_json decode failed",
        );
        assert_decode_error_mentions(
            decode_prompt_observability(&FakePromptDeltaRow::with_summary_json(
                json!({"summary": {}}).to_string(),
            )),
            "missing `delta_counts`",
        );
        assert_decode_error_mentions(
            decode_prompt_observability(&FakePromptDeltaRow::with_summary_json(
                json!({"summary": {}, "delta_counts": {"reuse": "bad"}}).to_string(),
            )),
            "delta_counts decode failed",
        );
    }

    #[test]
    fn prompt_persist_result_decode_preserves_values_and_fails_loudly() {
        let result =
            decode_prompt_persist_result(&FakePromptDeltaRow::complete()).expect("result decodes");
        assert_eq!(result.request_id, "request-1");
        assert_eq!(result.request_hash, "hash-1");
        assert_eq!(
            result.previous_request_id.as_deref(),
            Some("previous-request-1")
        );
        assert_eq!(result.message_count, 2);
        assert_eq!(result.tool_count, 3);
        assert_eq!(result.delta_counts.append, 2);

        for column in [
            "request_id",
            "request_hash",
            "previous_request_id",
            "message_count",
            "tool_count",
            "summary_json",
        ] {
            assert_decode_error_mentions(
                decode_prompt_persist_result(&FakePromptDeltaRow::fail_on(column)),
                &format!("`{column}`"),
            );
        }
    }

    #[test]
    fn previous_request_and_chunk_decode_fail_loudly() {
        assert_eq!(
            decode_previous_request_id(&FakePromptDeltaRow::complete())
                .expect("previous id decodes"),
            "request-1"
        );
        assert_decode_error_mentions(
            decode_previous_request_id(&FakePromptDeltaRow::fail_on("request_id")),
            "decode column `request_id`",
        );
        let previous =
            decode_previous_prompt_request(&FakePromptDeltaRow::complete()).expect("request");
        assert_eq!(previous.provider, "provider-a");
        assert_eq!(previous.tokenizer_revision, None);
        for column in ["request_id", "provider", "summary_json"] {
            assert_decode_error_mentions(
                decode_previous_prompt_request(&FakePromptDeltaRow::fail_on(column)),
                &format!("`{column}`"),
            );
        }

        let chunk =
            decode_existing_prompt_chunk(&FakePromptDeltaRow::complete()).expect("chunk decodes");
        assert_eq!(chunk.logical_key, "message:0:user");
        assert_eq!(chunk.chunk_hash, "chunk-hash-1");
        for column in ["logical_key", "chunk_hash"] {
            assert_decode_error_mentions(
                decode_existing_prompt_chunk(&FakePromptDeltaRow::fail_on(column)),
                &format!("decode column `{column}`"),
            );
        }
    }
}
