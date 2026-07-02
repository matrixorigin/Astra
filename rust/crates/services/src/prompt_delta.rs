use crate::db_row::RowExt as PromptDeltaDbRow;
use astra_core::SharedPool;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PromptDeltaCounts {
    pub reuse: u32,
    pub append: u32,
    pub replace: u32,
    pub drop: u32,
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

fn decode_existing_prompt_chunk(
    row: &impl PromptDeltaDbRow,
) -> Result<ExistingPromptChunk, String> {
    Ok(ExistingPromptChunk {
        logical_key: prompt_delta_row_string(row, "logical_key")?,
        chunk_hash: prompt_delta_row_string(row, "chunk_hash")?,
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
    let request_hash = hash_json_value(&json!({
        "messages": input.messages,
        "tools": input.tools,
        "max_output_tokens": max_output_tokens_u32,
    }))?;
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
    ensure_session_owner(db, &input.session_id, &input.user_id).await?;
    if let Some(existing) = load_existing_request(db, input, &plan.request_id).await? {
        return existing_prompt_request_or_conflict(input, plan, existing);
    }

    let previous_request_id = load_previous_request_id(db, input).await?;
    let previous_chunks = if let Some(previous_request_id) = previous_request_id.as_deref() {
        load_request_chunks(db, input, previous_request_id).await?
    } else {
        Vec::new()
    };

    let mut previous_map = previous_chunks
        .into_iter()
        .map(|chunk| (chunk.logical_key, chunk.chunk_hash))
        .collect::<std::collections::HashMap<_, _>>();
    let mut delta_counts = PromptDeltaCounts::default();
    let mut delta_seq: i32 = 0;
    let mut delta_rows = Vec::with_capacity(plan.chunks.len().saturating_add(previous_map.len()));
    for chunk in &plan.chunks {
        let previous_hash = previous_map.remove(&chunk.logical_key);
        let op = if previous_hash.as_deref() == Some(chunk.chunk_hash.as_str()) {
            delta_counts.reuse = delta_counts.reuse.saturating_add(1);
            "reuse"
        } else if previous_hash.is_some() {
            delta_counts.replace = delta_counts.replace.saturating_add(1);
            "replace"
        } else {
            delta_counts.append = delta_counts.append.saturating_add(1);
            "append"
        };
        delta_rows.push(PlannedPromptDelta {
            delta_seq,
            logical_key: chunk.logical_key.clone(),
            chunk_kind: chunk.chunk_kind.clone(),
            position: chunk.position,
            op,
            chunk_id: Some(chunk.chunk_id.clone()),
            chunk_hash: Some(chunk.chunk_hash.clone()),
            previous_chunk_hash: previous_hash,
        });
        delta_seq = delta_seq.saturating_add(1);
    }
    for (logical_key, previous_hash) in previous_map {
        delta_counts.drop = delta_counts.drop.saturating_add(1);
        delta_rows.push(PlannedPromptDelta {
            delta_seq,
            logical_key,
            chunk_kind: "drop".to_string(),
            position: delta_seq,
            op: "drop",
            chunk_id: None,
            chunk_hash: None,
            previous_chunk_hash: Some(previous_hash),
        });
        delta_seq = delta_seq.saturating_add(1);
    }

    let summary_json = json!({
        "summary": plan.summary_json.clone(),
        "delta_counts": delta_counts,
    });

    let mut tx = db.begin().await.map_err(|error| error.to_string())?;
    let write_result: Result<(), String> = async {
        sqlx::query(
            "INSERT INTO prompt_request_records
             (request_id, session_id, user_id, run_id, turn, round, attempt, source,
              model, provider, max_output_tokens, message_count, tool_count,
              previous_request_id, request_hash, summary_json, created_at, created_at_unix_ms)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6), UNIX_TIMESTAMP(NOW(6)) * 1000)",
        )
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

        for delta in &delta_rows {
            insert_prompt_delta(
                &mut tx,
                PromptDeltaInsert {
                    user_id: &input.user_id,
                    session_id: &input.session_id,
                    request_id: &plan.request_id,
                    delta_seq: delta.delta_seq,
                    logical_key: &delta.logical_key,
                    chunk_kind: &delta.chunk_kind,
                    position: delta.position,
                    op: delta.op,
                    chunk_id: delta.chunk_id.as_deref(),
                    chunk_hash: delta.chunk_hash.as_deref(),
                    previous_chunk_hash: delta.previous_chunk_hash.as_deref(),
                },
            )
            .await?;
        }
        Ok(())
    }
    .await;

    if let Err(error) = write_result {
        rollback_prompt_delta_tx(tx, "persist_prompt_request write failure").await;
        if let Some(existing) = load_existing_request(db, input, &plan.request_id).await? {
            return existing_prompt_request_or_conflict(input, plan, existing);
        }
        return Err(error);
    }

    tx.commit().await.map_err(|error| error.to_string())?;
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
         ORDER BY created_at DESC, turn DESC, round DESC, attempt DESC
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
         ORDER BY created_at DESC, turn DESC, round DESC, attempt DESC
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
    pool: &sqlx::Pool<sqlx::MySql>,
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
    .fetch_optional(pool)
    .await
    .map_err(|error| error.to_string())?;
    row.map(|row| decode_prompt_persist_result(&row))
        .transpose()
}

async fn ensure_session_owner(
    pool: &sqlx::Pool<sqlx::MySql>,
    session_id: &str,
    user_id: &str,
) -> Result<(), String> {
    let exists: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM agent_sessions WHERE session_id = ? AND user_id = ? LIMIT 1",
    )
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(pool)
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
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlannedPromptDelta {
    delta_seq: i32,
    logical_key: String,
    chunk_kind: String,
    position: i32,
    op: &'static str,
    chunk_id: Option<String>,
    chunk_hash: Option<String>,
    previous_chunk_hash: Option<String>,
}

async fn load_previous_request_id(
    pool: &sqlx::Pool<sqlx::MySql>,
    input: &PromptRequestPersistInput,
) -> Result<Option<String>, String> {
    sqlx::query(
        "SELECT request_id
         FROM prompt_request_records
         WHERE user_id = ? AND session_id = ? AND source = ?
         ORDER BY created_at DESC, turn DESC, round DESC, attempt DESC
         LIMIT 1",
    )
    .bind(&input.user_id)
    .bind(&input.session_id)
    .bind(&input.source)
    .fetch_optional(pool)
    .await
    .map_err(|error| error.to_string())
    .and_then(|row| row.map(|row| decode_previous_request_id(&row)).transpose())
}

async fn load_request_chunks(
    pool: &sqlx::Pool<sqlx::MySql>,
    input: &PromptRequestPersistInput,
    request_id: &str,
) -> Result<Vec<ExistingPromptChunk>, String> {
    let rows = sqlx::query(
        "SELECT logical_key, chunk_hash
         FROM prompt_deltas
         WHERE user_id = ? AND session_id = ? AND request_id = ?
           AND op != 'drop' AND chunk_hash IS NOT NULL
         ORDER BY position ASC, delta_seq ASC",
    )
    .bind(&input.user_id)
    .bind(&input.session_id)
    .bind(request_id)
    .fetch_all(pool)
    .await
    .map_err(|error| error.to_string())?;
    rows.into_iter()
        .map(|row| decode_existing_prompt_chunk(&row))
        .collect()
}

struct PromptDeltaInsert<'a> {
    user_id: &'a str,
    session_id: &'a str,
    request_id: &'a str,
    delta_seq: i32,
    logical_key: &'a str,
    chunk_kind: &'a str,
    position: i32,
    op: &'a str,
    chunk_id: Option<&'a str>,
    chunk_hash: Option<&'a str>,
    previous_chunk_hash: Option<&'a str>,
}

async fn insert_prompt_delta(
    tx: &mut sqlx::Transaction<'_, sqlx::MySql>,
    delta: PromptDeltaInsert<'_>,
) -> Result<(), String> {
    sqlx::query(
        "INSERT INTO prompt_deltas
         (user_id, session_id, request_id, delta_seq, logical_key, chunk_kind, position,
          op, chunk_id, chunk_hash, previous_chunk_hash, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW(6))",
    )
    .bind(delta.user_id)
    .bind(delta.session_id)
    .bind(delta.request_id)
    .bind(delta.delta_seq)
    .bind(delta.logical_key)
    .bind(delta.chunk_kind)
    .bind(delta.position)
    .bind(delta.op)
    .bind(delta.chunk_id)
    .bind(delta.chunk_hash)
    .bind(delta.previous_chunk_hash)
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
    let payload_json = canonical_json_string(payload);
    let chunk_hash = sha256_hex(payload_json.as_bytes());
    Ok(PromptChunkPlan {
        logical_key: logical_key.to_string(),
        chunk_kind: chunk_kind.to_string(),
        position,
        chunk_id: format!("pchunk-{chunk_hash}"),
        chunk_hash,
    })
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

fn hash_json_value(value: &Value) -> Result<String, String> {
    Ok(sha256_hex(canonical_json_string(value).as_bytes()))
}

fn canonical_json_string(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(value) => value.to_string(),
        Value::Number(value) => value.to_string(),
        Value::String(value) => serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string()),
        Value::Array(values) => {
            let inner = values
                .iter()
                .map(canonical_json_string)
                .collect::<Vec<_>>()
                .join(",");
            format!("[{inner}]")
        }
        Value::Object(values) => canonical_json_object(values),
    }
}

fn canonical_json_object(values: &Map<String, Value>) -> String {
    let mut entries = values.iter().collect::<Vec<_>>();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    let inner = entries
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}:{}",
                serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                canonical_json_string(value)
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    format!("{{{inner}}}")
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
