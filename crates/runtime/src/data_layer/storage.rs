pub use astra_services::storage::*;

use std::time::Duration;

use serde_json::Value;
use sqlx::{MySql, query};

use astra_core::canonical_names::{
    metadata_duration_ms, metadata_tool_call_id, metadata_tool_name,
};
use astra_turn_core::contracts::{
    TurnCoreEventRecord, TurnDecisionAuditRecord, TurnSkillSelectionRecord, TurnToolEventRecord,
};
use astra_turn_core::hook_plans::SnapshotLinkPlan;
use astra_turn_core::trace_event::TraceEvent;

fn metadata_string(metadata: Option<&serde_json::Value>, key: &str) -> Option<String> {
    metadata
        .and_then(|value| value.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

fn mysql_datetime(dt: chrono::DateTime<chrono::Utc>) -> String {
    dt.format("%Y-%m-%d %H:%M:%S%.6f").to_string()
}

const INSERT_CORE_TURN_EVENT_SQL: &str = "INSERT IGNORE INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, run_id, turn_seq, token_usage, llm_model_used, llm_params, reasoning_content, \
          token_input, token_output, token_total, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())";

#[derive(Debug, PartialEq)]
struct CoreTurnEventInsertValues {
    turn_seq: Option<i64>,
    token_usage_json: Option<String>,
    llm_params_json: Option<String>,
    token_input: Option<i64>,
    token_output: Option<i64>,
    token_total: Option<i64>,
}

#[derive(Debug, PartialEq)]
struct TraceEventInsertValues {
    token_usage_json: Option<String>,
    token_input: Option<i64>,
    token_output: Option<i64>,
    token_total: Option<i64>,
    metadata_json: String,
    created_at: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CanonicalTokenUsageColumns {
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_creation_tokens: i64,
    token_input: i64,
    token_output: i64,
    token_total: i64,
}

fn token_usage_protocol_error(message: impl Into<String>) -> sqlx::Error {
    sqlx::Error::Protocol(message.into())
}

fn canonical_token_count(value: &Value, field: &str) -> Result<i64, sqlx::Error> {
    let raw = value.get(field).ok_or_else(|| {
        token_usage_protocol_error(format!("token_usage missing canonical field `{field}`"))
    })?;
    let Some(raw) = raw.as_i64() else {
        return Err(token_usage_protocol_error(format!(
            "token_usage field `{field}` must be a non-negative integer, got {raw}"
        )));
    };
    if raw < 0 {
        return Err(token_usage_protocol_error(format!(
            "token_usage field `{field}` must be non-negative, got {raw}"
        )));
    }
    Ok(raw)
}

fn canonical_token_usage_columns(
    token_usage: Option<&Value>,
) -> Result<Option<CanonicalTokenUsageColumns>, sqlx::Error> {
    let Some(value) = token_usage else {
        return Ok(None);
    };
    if !value.is_object() {
        return Err(token_usage_protocol_error(format!(
            "token_usage must be a canonical JSON object, got {value}"
        )));
    }
    let input = canonical_token_count(value, "input_tokens")?;
    let cached = canonical_token_count(value, "cached_input_tokens")?;
    let creation = canonical_token_count(value, "cache_creation_tokens")?;
    let output = canonical_token_count(value, "output_tokens")?;
    let total = canonical_token_count(value, "total_tokens")?;
    let normalized_input = astra_turn_types::NormalizedPromptCacheUsage::new(
        input as u64,
        cached as u64,
        creation as u64,
    );
    let token_input = normalized_input
        .checked_total_input_tokens()
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| token_usage_protocol_error("token_usage input column overflow"))?;
    let expected_total = normalized_input
        .checked_total_tokens_with_output(output as u64)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| token_usage_protocol_error("token_usage total column overflow"))?;
    if total != expected_total {
        return Err(token_usage_protocol_error(format!(
            "token_usage total_tokens mismatch: expected {expected_total}, got {total}"
        )));
    }
    Ok(Some(CanonicalTokenUsageColumns {
        input_tokens: input,
        cached_input_tokens: cached,
        cache_creation_tokens: creation,
        token_input,
        token_output: output,
        token_total: total,
    }))
}

fn persisted_token_usage_json(
    token_usage: Option<&Value>,
    usage: Option<CanonicalTokenUsageColumns>,
) -> Option<String> {
    let token_usage = token_usage?;
    let usage = usage?;
    let mut token_usage = token_usage.clone();
    if let Value::Object(ref mut obj) = token_usage {
        obj.insert("input_tokens".into(), Value::from(usage.input_tokens));
        obj.insert(
            "cached_input_tokens".into(),
            Value::from(usage.cached_input_tokens),
        );
        obj.insert(
            "cache_creation_tokens".into(),
            Value::from(usage.cache_creation_tokens),
        );
        obj.insert("output_tokens".into(), Value::from(usage.token_output));
        obj.insert("total_tokens".into(), Value::from(usage.token_total));
        obj.insert("prompt".into(), Value::from(usage.token_input));
        obj.insert("completion".into(), Value::from(usage.token_output));
        obj.insert("cache_read".into(), Value::from(usage.cached_input_tokens));
        obj.insert(
            "cache_write".into(),
            Value::from(usage.cache_creation_tokens),
        );
        obj.insert("total".into(), Value::from(usage.token_total));
    }
    Some(token_usage.to_string())
}

fn core_turn_event_insert_values(
    event: &TurnCoreEventRecord,
) -> Result<CoreTurnEventInsertValues, sqlx::Error> {
    let usage = canonical_token_usage_columns(event.token_usage.as_ref())?;
    Ok(CoreTurnEventInsertValues {
        turn_seq: event.turn_seq,
        token_usage_json: persisted_token_usage_json(event.token_usage.as_ref(), usage),
        llm_params_json: event.llm_params.as_ref().map(serde_json::Value::to_string),
        token_input: usage.map(|usage| usage.token_input),
        token_output: usage.map(|usage| usage.token_output),
        token_total: usage.map(|usage| usage.token_total),
    })
}

fn trace_event_insert_values(event: &TraceEvent) -> Result<TraceEventInsertValues, sqlx::Error> {
    let usage = canonical_token_usage_columns(event.token_usage.as_ref())?;
    Ok(TraceEventInsertValues {
        token_usage_json: persisted_token_usage_json(event.token_usage.as_ref(), usage),
        token_input: usage.map(|usage| usage.token_input),
        token_output: usage.map(|usage| usage.token_output),
        token_total: usage.map(|usage| usage.token_total),
        metadata_json: event.metadata.to_string(),
        created_at: mysql_datetime(event.created_at),
    })
}

pub(crate) async fn insert_trace_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TraceEvent,
) -> Result<bool, sqlx::Error> {
    let values = trace_event_insert_values(event)?;
    let result = query(
        "INSERT IGNORE INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, run_id, parent_run_id, turn_id, turn_seq, \
          round_index, tool_call_id, parent_agent_id, trace_kind, token_usage, \
          llm_model_used, reasoning_content, token_input, token_output, token_total, \
          meta_tool_name, meta_duration_ms, metadata, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&event.event_id)
    .bind(&event.session_id)
    .bind(&event.user_id)
    .bind(event.agent_id.as_deref().unwrap_or("astra-server"))
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(&event.event_type)
    .bind(&event.content)
    .bind(&event.parent_event_id)
    .bind(&event.causal_chain_id)
    .bind(&event.run_id)
    .bind(&event.parent_run_id)
    .bind(&event.turn_id)
    .bind(event.turn_seq)
    .bind(event.round_index)
    .bind(&event.tool_call_id)
    .bind(&event.parent_agent_id)
    .bind(&event.trace_kind)
    .bind(values.token_usage_json)
    .bind(&event.llm_model_used)
    .bind(&event.reasoning_content)
    .bind(values.token_input)
    .bind(values.token_output)
    .bind(values.token_total)
    .bind(&event.meta_tool_name)
    .bind(event.meta_duration_ms)
    .bind(values.metadata_json)
    .bind(values.created_at)
    .execute(&mut **tx)
    .await?;
    let inserted = result.rows_affected() > 0;
    if inserted {
        insert_agent_event_edges(
            &mut **tx,
            &event.user_id,
            &event.session_id,
            &event.event_id,
            event.parent_event_id.as_deref(),
            event
                .parent_event_id
                .as_ref()
                .map(|id| std::slice::from_ref(id))
                .unwrap_or(&[]),
        )
        .await?;
    }
    Ok(inserted)
}

pub(crate) async fn insert_core_turn_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TurnCoreEventRecord,
) -> Result<bool, sqlx::Error> {
    let values = core_turn_event_insert_values(event)?;
    let result = query(INSERT_CORE_TURN_EVENT_SQL)
        .bind(&event.event_id)
        .bind(&event.session_id)
        .bind(&event.user_id)
        .bind(event.agent_id.as_deref().unwrap_or("astra-cli"))
        .bind(env!("CARGO_PKG_VERSION"))
        .bind(&event.event_type)
        .bind(&event.content)
        .bind(&event.parent_event_id)
        .bind(&event.causal_chain_id)
        .bind(&event.run_id)
        .bind(values.turn_seq)
        .bind(values.token_usage_json)
        .bind(&event.llm_model_used)
        .bind(values.llm_params_json)
        .bind(&event.reasoning_content)
        .bind(values.token_input)
        .bind(values.token_output)
        .bind(values.token_total)
        .execute(&mut **tx)
        .await?;
    let inserted = result.rows_affected() > 0;
    if inserted {
        insert_agent_event_edges(
            &mut **tx,
            &event.user_id,
            &event.session_id,
            &event.event_id,
            event.parent_event_id.as_deref(),
            &event.parent_event_ids,
        )
        .await?;
    }
    Ok(inserted)
}

pub(crate) async fn insert_tool_turn_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TurnToolEventRecord,
    skill_version: Option<&String>,
) -> Result<bool, sqlx::Error> {
    let result = query(
        "INSERT IGNORE INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, run_id, tool_call_id, metadata, skill_name, skill_version, reasoning_content, \
          meta_tool_name, meta_duration_ms, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&event.event_id)
    .bind(&event.session_id)
    .bind(&event.user_id)
    .bind(event.agent_id.as_deref().unwrap_or("astra-cli"))
    .bind(env!("CARGO_PKG_VERSION"))
    .bind(&event.event_type)
    .bind(&event.content)
    .bind(&event.parent_event_id)
    .bind(&event.causal_chain_id)
    .bind(
        event
            .run_id
            .clone()
            .or_else(|| metadata_string(event.metadata.as_ref(), "run_id")),
    )
    .bind(
        event
            .tool_call_id
            .clone()
            .or_else(|| metadata_tool_call_id(event.metadata.as_ref())),
    )
    .bind(event.metadata.as_ref().map(serde_json::Value::to_string))
    .bind(&event.skill_name)
    .bind(skill_version.cloned().or_else(|| event.skill_version.clone()))
    .bind(&event.reasoning_content)
    .bind(metadata_tool_name(event.metadata.as_ref()))
    .bind(metadata_duration_ms(event.metadata.as_ref()))
    .execute(&mut **tx)
    .await?;
    let inserted = result.rows_affected() > 0;
    if inserted {
        insert_agent_event_edges(
            &mut **tx,
            &event.user_id,
            &event.session_id,
            &event.event_id,
            event.parent_event_id.as_deref(),
            &event.parent_event_ids,
        )
        .await?;
    }
    Ok(inserted)
}

pub(crate) async fn insert_turn_decision_audit(
    tx: &mut sqlx::Transaction<'_, MySql>,
    record: &TurnDecisionAuditRecord,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO ctx_decision_audits \
         (decision_id, user_id, session_id, event_id, decision_type, decision_output, model_used, context_capture_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&record.decision_id)
    .bind(&record.user_id)
    .bind(&record.session_id)
    .bind(&record.event_id)
    .bind(&record.decision_type)
    .bind(record.decision_output.to_string())
    .bind(&record.model_used)
    .bind(&record.context_capture_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        INSERT_CORE_TURN_EVENT_SQL, core_turn_event_insert_values, metadata_string,
        metadata_tool_name, trace_event_insert_values,
    };
    use astra_turn_core::contracts::TurnCoreEventRecord;
    use astra_turn_core::trace_event::TraceEvent;

    #[test]
    fn core_turn_event_insert_persists_turn_seq() {
        assert!(
            INSERT_CORE_TURN_EVENT_SQL.contains(
                "parent_event_id, causal_chain_id, run_id, turn_seq, token_usage, llm_model_used"
            ),
            "core turn events must persist run_id and turn_seq so session traces have durable anchors"
        );
        assert_eq!(
            INSERT_CORE_TURN_EVENT_SQL.matches('?').count(),
            18,
            "core turn event insert SQL placeholder count must match its bound values"
        );
    }

    #[test]
    fn metadata_string_trims_empty_values() {
        assert_eq!(
            metadata_string(Some(&serde_json::json!({"run_id": " run-1 "})), "run_id").as_deref(),
            Some("run-1")
        );
        assert_eq!(
            metadata_string(
                Some(&serde_json::json!({"tool_call_id": "  "})),
                "tool_call_id"
            ),
            None
        );
        assert_eq!(
            metadata_string(
                Some(&serde_json::json!({"tool_call_id": 7})),
                "tool_call_id"
            ),
            None
        );
    }

    fn core_event_with_token_usage(token_usage: Option<serde_json::Value>) -> TurnCoreEventRecord {
        TurnCoreEventRecord {
            event_id: "evt-1".to_string(),
            user_id: "user-1".to_string(),
            session_id: "session-1".to_string(),
            run_id: Some("run-1".to_string()),
            agent_id: None,
            event_type: "llm_response".to_string(),
            content: "done".to_string(),
            parent_event_id: None,
            parent_event_ids: Vec::new(),
            causal_chain_id: "chain-1".to_string(),
            turn_seq: Some(42),
            llm_model_used: Some("model-1".to_string()),
            token_usage,
            llm_params: Some(serde_json::json!({"temperature": 0.2})),
            reasoning_content: None,
        }
    }

    fn canonical_token_usage() -> serde_json::Value {
        serde_json::json!({
            "input_tokens": 10,
            "cached_input_tokens": 4,
            "cache_creation_tokens": 3,
            "output_tokens": 5,
            "total_tokens": 22
        })
    }

    #[test]
    fn core_turn_event_insert_values_preserve_turn_seq_and_token_columns() {
        let event = core_event_with_token_usage(Some(canonical_token_usage()));

        let values = core_turn_event_insert_values(&event).expect("canonical token usage");

        assert_eq!(values.turn_seq, Some(42));
        assert_eq!(values.token_input, Some(17));
        assert_eq!(values.token_output, Some(5));
        assert_eq!(values.token_total, Some(22));
        assert!(
            values
                .token_usage_json
                .as_deref()
                .is_some_and(|json| json.contains("\"input_tokens\":10"))
        );
        let persisted: serde_json::Value =
            serde_json::from_str(values.token_usage_json.as_deref().unwrap()).unwrap();
        assert_eq!(persisted["prompt"], 17);
        assert_eq!(persisted["completion"], 5);
        assert_eq!(persisted["cache_read"], 4);
        assert_eq!(persisted["cache_write"], 3);
        assert_eq!(persisted["total"], 22);
        assert_eq!(
            values.llm_params_json.as_deref(),
            Some("{\"temperature\":0.2}")
        );
    }

    #[test]
    fn trace_event_insert_values_preserve_canonical_token_columns() {
        let mut event = TraceEvent::new(
            "trace-1",
            "session-1",
            "user-1",
            "llm_round_completed",
            "llm_round",
        );
        event.token_usage = Some(canonical_token_usage());

        let values = trace_event_insert_values(&event).expect("canonical token usage");

        assert_eq!(values.token_input, Some(17));
        assert_eq!(values.token_output, Some(5));
        assert_eq!(values.token_total, Some(22));
        assert!(
            values
                .token_usage_json
                .as_deref()
                .is_some_and(|json| json.contains("\"input_tokens\":10"))
        );
        let persisted: serde_json::Value =
            serde_json::from_str(values.token_usage_json.as_deref().unwrap()).unwrap();
        assert_eq!(persisted["prompt"], 17);
        assert_eq!(persisted["completion"], 5);
        assert_eq!(persisted["cache_read"], 4);
        assert_eq!(persisted["cache_write"], 3);
        assert_eq!(persisted["total"], 22);
    }

    #[test]
    fn metadata_tool_name_requires_explicit_tool_name() {
        assert_eq!(
            metadata_tool_name(Some(&serde_json::json!({"tool_name": " bash "}))).as_deref(),
            Some("bash")
        );
        assert_eq!(
            metadata_tool_name(Some(
                &serde_json::json!({"tool_name": "preferred", "name": "read_file"})
            ))
            .as_deref(),
            Some("preferred")
        );
        assert!(metadata_tool_name(Some(&serde_json::json!({"name": "read_file"}))).is_none());
    }

    #[test]
    fn token_usage_columns_fail_loudly_on_noncanonical_usage() {
        let missing_field = core_event_with_token_usage(Some(serde_json::json!({
            "cached_input_tokens": 0,
            "cache_creation_tokens": 0,
            "output_tokens": 5,
            "total_tokens": 5,
        })));
        let err = core_turn_event_insert_values(&missing_field)
            .expect_err("missing canonical field must fail");
        assert!(
            err.to_string().contains("input_tokens"),
            "error should identify missing canonical field: {err}"
        );

        let mismatched_total = core_event_with_token_usage(Some(serde_json::json!({
            "input_tokens": 10,
            "cached_input_tokens": 4,
            "cache_creation_tokens": 3,
            "output_tokens": 5,
            "total_tokens": 21,
        })));
        let err = core_turn_event_insert_values(&mismatched_total)
            .expect_err("mismatched total must fail");
        assert!(
            err.to_string().contains("total_tokens mismatch"),
            "error should identify total mismatch: {err}"
        );

        let mut trace = TraceEvent::new(
            "trace-bad",
            "session-1",
            "user-1",
            "llm_round_completed",
            "llm_round",
        );
        trace.token_usage = Some(serde_json::json!({
            "prompt": 10,
            "completion": 5,
            "total": 15,
        }));
        let err = trace_event_insert_values(&trace).expect_err("alternate token dialect must fail");
        assert!(
            err.to_string().contains("input_tokens"),
            "trace token usage must be canonical-only: {err}"
        );
    }
}

pub(crate) async fn insert_turn_skill_selection(
    tx: &mut sqlx::Transaction<'_, MySql>,
    record: &TurnSkillSelectionRecord,
) -> Result<(), sqlx::Error> {
    query(
        "INSERT INTO skill_selection_events \
         (event_id, session_id, user_id, agent_id, user_query, selected_skills, skill_name, skill_version, selection_method, execution_success, execution_time_ms, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
    )
    .bind(&record.event_id)
    .bind(&record.session_id)
    .bind(&record.user_id)
    .bind(&record.agent_id)
    .bind(&record.user_query)
    .bind(serde_json::json!(record.selected_skills).to_string())
    .bind(&record.skill_name)
    .bind(&record.skill_version)
    .bind(&record.selection_method)
    .bind(record.execution_success)
    .bind(record.execution_time_ms)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

pub(crate) async fn update_snapshot_llm_ids(
    pool: &sqlx::Pool<MySql>,
    plan: &SnapshotLinkPlan,
) -> Result<(), sqlx::Error> {
    for _ in 0..5 {
        let rows_affected = query(
            "UPDATE ctx_snapshots \
             SET llm_request_id = ?, llm_response_id = COALESCE(?, llm_response_id) \
             WHERE context_capture_id = ? AND user_id = ?",
        )
        .bind(&plan.llm_request_id)
        .bind(&plan.llm_response_id)
        .bind(&plan.context_capture_id)
        .bind(&plan.user_id)
        .execute(pool)
        .await?
        .rows_affected();
        if rows_affected > 0 {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Ok(())
}
