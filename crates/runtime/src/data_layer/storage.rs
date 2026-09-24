pub use astra_services::storage::*;

use std::time::Duration;

use serde_json::Value;
use sqlx::{MySql, QueryBuilder, Row, query};

use astra_core::canonical_names::{
    metadata_duration_ms, metadata_tool_call_id, metadata_tool_name,
};
use astra_core::{matrixone_null_shape_comment, matrixone_statement_with_null_shape};
use astra_services::observation_capture::{
    DurableCaptureOutcome, ObservationCollisionReceipt, ObservationPayloadDomain,
    canonical_observation_payload_hash, classify_capture, record_observation_collisions,
};
use astra_turn_core::contracts::{
    TurnAuxiliaryEventRecord, TurnCoreEventRecord, TurnSkillSelectionRecord, TurnToolEventRecord,
};
use astra_turn_core::hook_plans::SnapshotLinkPlan;
use astra_turn_core::trace_event::TraceEvent;
use uuid::Uuid;

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
          token_input, token_output, token_total, payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())";

const CORE_TURN_EVENT_COLLISION_SOURCE: &str = "runtime_core_turn_event";
const TOOL_TURN_EVENT_COLLISION_SOURCE: &str = "runtime_tool_turn_event";
const TRACE_EVENT_COLLISION_SOURCE: &str = "runtime_trace_event";
pub(crate) const AUXILIARY_EVENT_COLLISION_SOURCE: &str = "runtime_auxiliary_event";

#[derive(Clone, Copy, Debug)]
pub(crate) struct AgentEventCaptureAttempt<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) session_id: &'a str,
    pub(crate) event_id: &'a str,
    pub(crate) payload_hash: &'a str,
}

#[derive(Clone, Debug)]
struct AgentEventCaptureReadback {
    session_id: String,
    payload_hash: String,
    ingestion_write_id: String,
}

#[derive(Debug, PartialEq)]
struct CoreTurnEventInsertValues {
    payload_hash: String,
    turn_seq: Option<i64>,
    token_usage_json: Option<String>,
    llm_params_json: Option<String>,
    token_input: Option<i64>,
    token_output: Option<i64>,
    token_total: Option<i64>,
}

#[derive(Debug, PartialEq)]
struct TraceEventInsertValues {
    payload_hash: String,
    token_usage_json: Option<String>,
    token_input: Option<i64>,
    token_output: Option<i64>,
    token_total: Option<i64>,
    metadata_json: String,
    created_at: String,
}

#[derive(Debug, Default)]
pub(crate) struct AgentEventInsertBatchOutcome {
    pub(crate) inserted: u64,
    pub(crate) last_inserted_event_id: Option<String>,
    pub(crate) event_outcomes: std::collections::BTreeMap<String, DurableCaptureOutcome>,
}

fn merge_capture_outcome(current: &mut DurableCaptureOutcome, incoming: DurableCaptureOutcome) {
    if matches!(current, DurableCaptureOutcome::Collision { .. }) {
        return;
    }
    if matches!(incoming, DurableCaptureOutcome::Collision { .. })
        || (*current == DurableCaptureOutcome::Replayed
            && incoming == DurableCaptureOutcome::Inserted)
    {
        *current = incoming;
    }
}

impl TraceEventInsertValues {
    fn nullable_shape(&self, event: &TraceEvent) -> [bool; 18] {
        [
            event.content.is_some(),
            event.parent_event_id.is_some(),
            event.causal_chain_id.is_some(),
            event.run_id.is_some(),
            event.parent_run_id.is_some(),
            event.turn_id.is_some(),
            event.turn_seq.is_some(),
            event.round_index.is_some(),
            event.tool_call_id.is_some(),
            event.parent_agent_id.is_some(),
            self.token_usage_json.is_some(),
            event.llm_model_used.is_some(),
            event.reasoning_content.is_some(),
            self.token_input.is_some(),
            self.token_output.is_some(),
            self.token_total.is_some(),
            event.meta_tool_name.is_some(),
            event.meta_duration_ms.is_some(),
        ]
    }
}

pub(crate) async fn classify_agent_event_capture_attempts(
    tx: &mut sqlx::Transaction<'_, MySql>,
    attempts: &[AgentEventCaptureAttempt<'_>],
    attempted_write_id: &str,
    collision_source: &'static str,
) -> Result<Vec<DurableCaptureOutcome>, sqlx::Error> {
    let Some(first) = attempts.first() else {
        return Ok(Vec::new());
    };
    if attempts
        .iter()
        .any(|attempt| attempt.user_id != first.user_id)
    {
        return Err(sqlx::Error::Protocol(
            "agent event capture readback must belong to one owner".to_string(),
        ));
    }

    let event_ids = attempts
        .iter()
        .map(|attempt| attempt.event_id)
        .collect::<std::collections::BTreeSet<_>>();
    let mut query = QueryBuilder::<MySql>::new(
        "SELECT event_id, session_id, payload_hash, ingestion_write_id \
         FROM agent_events WHERE user_id = ",
    );
    query.push_bind(first.user_id).push(" AND event_id IN (");
    {
        let mut separated = query.separated(", ");
        for event_id in event_ids {
            separated.push_bind(event_id);
        }
        separated.push_unseparated(")");
    }
    let rows = query.build().fetch_all(&mut **tx).await?;
    let mut readbacks = std::collections::BTreeMap::new();
    for row in rows {
        let event_id = row.try_get::<String, _>("event_id")?;
        let readback = AgentEventCaptureReadback {
            session_id: row.try_get("session_id")?,
            payload_hash: row.try_get("payload_hash")?,
            ingestion_write_id: row.try_get("ingestion_write_id")?,
        };
        if readbacks.insert(event_id.clone(), readback).is_some() {
            return Err(sqlx::Error::Protocol(format!(
                "duplicate owner-scoped agent event readback: event_id={event_id}"
            )));
        }
    }

    let mut inserted_effects = std::collections::BTreeSet::new();
    let mut outcomes = Vec::with_capacity(attempts.len());
    for attempt in attempts {
        let stored = readbacks.get(attempt.event_id).ok_or_else(|| {
            sqlx::Error::Protocol(format!(
                "missing owner-scoped agent event readback: event_id={}",
                attempt.event_id
            ))
        })?;
        let mut outcome = classify_capture(
            &stored.payload_hash,
            &stored.ingestion_write_id,
            attempt.payload_hash,
            attempted_write_id,
        );
        if stored.session_id != attempt.session_id
            && !matches!(outcome, DurableCaptureOutcome::Collision { .. })
        {
            return Err(sqlx::Error::Protocol(format!(
                "agent event identity crossed session boundary without a payload collision: event_id={}, stored_session={}, attempted_session={}",
                attempt.event_id, stored.session_id, attempt.session_id
            )));
        }
        if outcome == DurableCaptureOutcome::Inserted && !inserted_effects.insert(attempt.event_id)
        {
            outcome = DurableCaptureOutcome::Replayed;
        }
        if let DurableCaptureOutcome::Collision {
            stored_payload_hash,
            attempted_payload_hash,
        } = &outcome
        {
            tracing::warn!(
                target: "astra_runtime::agent_event_capture",
                user_id = %attempt.user_id,
                session_id = %attempt.session_id,
                event_id = %attempt.event_id,
                stored_session_id = %stored.session_id,
                stored_payload_hash = %stored_payload_hash,
                attempted_payload_hash = %attempted_payload_hash,
                source = collision_source,
                "agent event identity collision recorded without applying derived effects"
            );
        }
        outcomes.push(outcome);
    }
    let receipts = attempts
        .iter()
        .zip(&outcomes)
        .filter_map(|(attempt, outcome)| match outcome {
            DurableCaptureOutcome::Collision {
                stored_payload_hash,
                attempted_payload_hash,
            } => Some(ObservationCollisionReceipt {
                user_id: attempt.user_id,
                domain: ObservationPayloadDomain::AgentEvent,
                identity_id: attempt.event_id,
                session_id: attempt.session_id,
                stored_payload_hash,
                attempted_payload_hash,
                source: collision_source,
            }),
            _ => None,
        })
        .collect::<Vec<_>>();
    record_observation_collisions(tx, &receipts).await?;
    Ok(outcomes)
}

fn canonical_token_usage_columns(
    token_usage: Option<&Value>,
) -> Result<Option<astra_turn_types::CanonicalTokenUsage>, sqlx::Error> {
    token_usage
        .map(astra_turn_types::CanonicalTokenUsage::from_json)
        .transpose()
        .map_err(sqlx::Error::Protocol)
}

fn persisted_token_usage_json(
    source: Option<&Value>,
    usage: Option<astra_turn_types::CanonicalTokenUsage>,
) -> Option<String> {
    let usage = usage?;
    let mut object = source?.as_object()?.clone();
    // Keep existing accounting scope/metadata, but never stale numeric aliases
    // or a ratio derived from a now-partial input partition.
    for key in [
        "input_tokens",
        "cached_input_tokens",
        "cache_creation_tokens",
        "output_tokens",
        "total_tokens",
        "prompt",
        "completion",
        "cache_read",
        "cache_write",
        "total",
        "raw_prompt_tokens",
        "uncached_input_tokens",
        "effective_input_tokens",
        "prompt_cache_hit_ratio",
    ] {
        object.remove(key);
    }
    object.extend(usage.to_json().as_object()?.clone());
    // Metadata alone is not a canonical usage object. Keep an explicit unknown
    // field so an observed unavailable sample survives a validated replay.
    if !object.is_empty()
        && [
            "input_tokens",
            "cached_input_tokens",
            "cache_creation_tokens",
            "output_tokens",
            "total_tokens",
        ]
        .iter()
        .all(|key| !object.contains_key(*key))
    {
        object.insert("total_tokens".into(), Value::Null);
    }
    Some(Value::Object(object).to_string())
}

fn hash_agent_event_payload(payload: serde_json::Value) -> String {
    canonical_observation_payload_hash(ObservationPayloadDomain::AgentEvent, &payload)
}

/// Runtime trace identities carry the complete producer envelope. Journal
/// ingestion uses a separate content-addressed identity namespace.
pub(crate) fn trace_event_payload_hash(event: &TraceEvent) -> Result<String, sqlx::Error> {
    let payload = serde_json::to_value(event).map_err(|error| {
        sqlx::Error::Protocol(format!("serialize trace event capture: {error}"))
    })?;
    Ok(hash_agent_event_payload(payload))
}

fn core_turn_event_payload_hash(event: &TurnCoreEventRecord) -> String {
    hash_agent_event_payload(serde_json::json!({
        "event_id": event.event_id,
        "session_id": event.session_id,
        "user_id": event.user_id,
        "agent_id": event.agent_id.as_deref().unwrap_or("astra-cli"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "event_type": event.event_type,
        "content": event.content,
        "parent_event_id": event.parent_event_id,
        "parent_event_ids": event.parent_event_ids,
        "causal_chain_id": event.causal_chain_id,
        "run_id": event.run_id,
        "turn_seq": event.turn_seq,
        "token_usage": event.token_usage,
        "llm_model_used": event.llm_model_used,
        "llm_params": event.llm_params,
        "reasoning_content": event.reasoning_content,
    }))
}

fn tool_turn_event_payload_hash(
    event: &TurnToolEventRecord,
    run_id: Option<&str>,
    tool_call_id: Option<&str>,
    skill_version: Option<&str>,
    meta_tool_name: Option<&str>,
    meta_duration_ms: Option<i32>,
) -> String {
    hash_agent_event_payload(serde_json::json!({
        "event_id": event.event_id,
        "session_id": event.session_id,
        "user_id": event.user_id,
        "agent_id": event.agent_id.as_deref().unwrap_or("astra-cli"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "event_type": event.event_type,
        "content": event.content,
        "parent_event_id": event.parent_event_id,
        "parent_event_ids": event.parent_event_ids,
        "causal_chain_id": event.causal_chain_id,
        "run_id": run_id,
        "tool_call_id": tool_call_id,
        "metadata": event.metadata,
        "skill_name": event.skill_name,
        "skill_version": skill_version,
        "reasoning_content": event.reasoning_content,
        "meta_tool_name": meta_tool_name,
        "meta_duration_ms": meta_duration_ms,
    }))
}

pub(crate) fn auxiliary_turn_event_payload_hash(
    event: &TurnAuxiliaryEventRecord,
    meta_tool_name: Option<&str>,
    meta_duration_ms: Option<i32>,
) -> String {
    hash_agent_event_payload(serde_json::json!({
        "event_id": event.event_id,
        "session_id": event.session_id,
        "user_id": event.user_id,
        "agent_id": event.agent_id.as_deref().unwrap_or("astra-cli"),
        "agent_version": env!("CARGO_PKG_VERSION"),
        "event_type": event.event_type,
        "content": event.content,
        "parent_event_id": event.parent_event_id,
        "parent_event_ids": event.parent_event_ids,
        "causal_chain_id": event.causal_chain_id,
        "metadata": event.metadata,
        "reasoning_content": event.reasoning_content,
        "meta_tool_name": meta_tool_name,
        "meta_duration_ms": meta_duration_ms,
    }))
}

fn core_turn_event_insert_values(
    event: &TurnCoreEventRecord,
) -> Result<CoreTurnEventInsertValues, sqlx::Error> {
    let usage = canonical_token_usage_columns(event.token_usage.as_ref())?;
    Ok(CoreTurnEventInsertValues {
        payload_hash: core_turn_event_payload_hash(event),
        turn_seq: event.turn_seq,
        token_usage_json: persisted_token_usage_json(event.token_usage.as_ref(), usage),
        llm_params_json: event.llm_params.as_ref().map(serde_json::Value::to_string),
        token_input: usage.and_then(|usage| usage.input_column()),
        token_output: usage.and_then(|usage| usage.output_column()),
        token_total: usage.and_then(|usage| usage.total_column()),
    })
}

fn trace_event_insert_values(event: &TraceEvent) -> Result<TraceEventInsertValues, sqlx::Error> {
    let usage = canonical_token_usage_columns(event.token_usage.as_ref())?;
    Ok(TraceEventInsertValues {
        payload_hash: trace_event_payload_hash(event)?,
        token_usage_json: persisted_token_usage_json(event.token_usage.as_ref(), usage),
        token_input: usage.and_then(|usage| usage.input_column()),
        token_output: usage.and_then(|usage| usage.output_column()),
        token_total: usage.and_then(|usage| usage.total_column()),
        metadata_json: event.metadata.to_string(),
        created_at: mysql_datetime(event.created_at),
    })
}

fn unique_trace_event_indices(events: &[TraceEvent]) -> Vec<usize> {
    let mut event_ids = std::collections::BTreeSet::new();
    events
        .iter()
        .enumerate()
        .filter_map(|(index, event)| event_ids.insert(&event.event_id).then_some(index))
        .collect()
}

pub(crate) async fn insert_trace_events(
    tx: &mut sqlx::Transaction<'_, MySql>,
    events: &[TraceEvent],
) -> Result<AgentEventInsertBatchOutcome, sqlx::Error> {
    let Some(first) = events.first() else {
        return Ok(AgentEventInsertBatchOutcome::default());
    };
    if events
        .iter()
        .any(|event| event.user_id != first.user_id || event.session_id != first.session_id)
    {
        return Err(sqlx::Error::Protocol(
            "trace event batch must belong to one user session".to_string(),
        ));
    }

    let values = events
        .iter()
        .map(trace_event_insert_values)
        .collect::<Result<Vec<_>, _>>()?;
    let unique_indices = unique_trace_event_indices(events);
    let ingestion_write_id = Uuid::new_v4().to_string();
    let mut insert = QueryBuilder::<MySql>::new(
        "INSERT IGNORE INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, run_id, parent_run_id, turn_id, turn_seq, \
          round_index, tool_call_id, parent_agent_id, trace_kind, token_usage, \
          llm_model_used, reasoning_content, token_input, token_output, token_total, \
          meta_tool_name, meta_duration_ms, metadata, payload_hash, ingestion_write_id, created_at) ",
    );
    insert.push_values(unique_indices.iter(), |mut row, index| {
        let event = &events[*index];
        let values = &values[*index];
        row.push_bind(&event.event_id)
            .push_bind(&event.session_id)
            .push_bind(&event.user_id)
            .push_bind(event.agent_id.as_deref().unwrap_or("astra-server"))
            .push_bind(env!("CARGO_PKG_VERSION"))
            .push_bind(&event.event_type)
            .push_bind(&event.content)
            .push_bind(&event.parent_event_id)
            .push_bind(&event.causal_chain_id)
            .push_bind(&event.run_id)
            .push_bind(&event.parent_run_id)
            .push_bind(&event.turn_id)
            .push_bind(event.turn_seq)
            .push_bind(event.round_index)
            .push_bind(&event.tool_call_id)
            .push_bind(&event.parent_agent_id)
            .push_bind(&event.trace_kind)
            .push_bind(&values.token_usage_json)
            .push_bind(&event.llm_model_used)
            .push_bind(&event.reasoning_content)
            .push_bind(values.token_input)
            .push_bind(values.token_output)
            .push_bind(values.token_total)
            .push_bind(&event.meta_tool_name)
            .push_bind(event.meta_duration_ms)
            .push_bind(&values.metadata_json)
            .push_bind(&values.payload_hash)
            .push_bind(&ingestion_write_id)
            .push_bind(&values.created_at);
    });
    insert.push(matrixone_null_shape_comment(
        unique_indices
            .iter()
            .flat_map(|index| values[*index].nullable_shape(&events[*index])),
    ));
    insert.build().execute(&mut **tx).await?;

    let attempts = events
        .iter()
        .zip(&values)
        .map(|(event, values)| AgentEventCaptureAttempt {
            user_id: &event.user_id,
            session_id: &event.session_id,
            event_id: &event.event_id,
            payload_hash: &values.payload_hash,
        })
        .collect::<Vec<_>>();
    let outcomes = classify_agent_event_capture_attempts(
        tx,
        &attempts,
        &ingestion_write_id,
        TRACE_EVENT_COLLISION_SOURCE,
    )
    .await?;
    let inserted_indices = outcomes
        .iter()
        .enumerate()
        .filter_map(|(index, outcome)| {
            (*outcome == DurableCaptureOutcome::Inserted).then_some(index)
        })
        .collect::<Vec<_>>();
    let inserted = u64::try_from(inserted_indices.len()).map_err(|_| {
        sqlx::Error::Protocol("trace event inserted row count exceeds u64::MAX".to_string())
    })?;
    let last_inserted_event_id = inserted_indices
        .last()
        .map(|index| events[*index].event_id.clone());
    let mut event_outcomes = std::collections::BTreeMap::new();
    for (event, outcome) in events.iter().zip(outcomes) {
        event_outcomes
            .entry(event.event_id.clone())
            .and_modify(|current| merge_capture_outcome(current, outcome.clone()))
            .or_insert(outcome);
    }

    let edge_inputs = inserted_indices
        .iter()
        .map(|index| &events[*index])
        .map(|event| astra_services::storage::AgentEventEdgeInsert {
            user_id: &event.user_id,
            session_id: &event.session_id,
            child_event_id: &event.event_id,
            primary_parent_event_id: event.parent_event_id.as_deref(),
            parent_event_ids: event.parent_event_id.as_slice(),
        })
        .collect::<Vec<_>>();
    astra_services::storage::insert_agent_event_edges_batch(&mut **tx, &edge_inputs).await?;

    Ok(AgentEventInsertBatchOutcome {
        inserted,
        last_inserted_event_id,
        event_outcomes,
    })
}

pub(crate) async fn insert_core_turn_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TurnCoreEventRecord,
) -> Result<DurableCaptureOutcome, sqlx::Error> {
    let values = core_turn_event_insert_values(event)?;
    let ingestion_write_id = Uuid::new_v4().to_string();
    let insert_sql = matrixone_statement_with_null_shape(
        INSERT_CORE_TURN_EVENT_SQL,
        [
            event.parent_event_id.is_some(),
            event.run_id.is_some(),
            values.turn_seq.is_some(),
            values.token_usage_json.is_some(),
            event.llm_model_used.is_some(),
            values.llm_params_json.is_some(),
            event.reasoning_content.is_some(),
            values.token_input.is_some(),
            values.token_output.is_some(),
            values.token_total.is_some(),
        ],
    );
    let result = query(&insert_sql)
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
        .bind(&values.token_usage_json)
        .bind(&event.llm_model_used)
        .bind(&values.llm_params_json)
        .bind(&event.reasoning_content)
        .bind(values.token_input)
        .bind(values.token_output)
        .bind(values.token_total)
        .bind(&values.payload_hash)
        .bind(&ingestion_write_id)
        .execute(&mut **tx)
        .await?;
    let _reported_rows_affected = result.rows_affected();
    let outcome = classify_agent_event_capture_attempts(
        tx,
        &[AgentEventCaptureAttempt {
            user_id: &event.user_id,
            session_id: &event.session_id,
            event_id: &event.event_id,
            payload_hash: &values.payload_hash,
        }],
        &ingestion_write_id,
        CORE_TURN_EVENT_COLLISION_SOURCE,
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| sqlx::Error::Protocol("missing core event capture outcome".to_string()))?;
    let inserted = outcome == DurableCaptureOutcome::Inserted;
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
    Ok(outcome)
}

pub(crate) async fn insert_tool_turn_event(
    tx: &mut sqlx::Transaction<'_, MySql>,
    event: &TurnToolEventRecord,
    skill_version: Option<&String>,
) -> Result<bool, sqlx::Error> {
    let run_id = event
        .run_id
        .clone()
        .or_else(|| metadata_string(event.metadata.as_ref(), "run_id"));
    let tool_call_id = event
        .tool_call_id
        .clone()
        .or_else(|| metadata_tool_call_id(event.metadata.as_ref()));
    let metadata_json = event.metadata.as_ref().map(serde_json::Value::to_string);
    let skill_version = skill_version
        .cloned()
        .or_else(|| event.skill_version.clone());
    let meta_tool_name = metadata_tool_name(event.metadata.as_ref());
    let meta_duration_ms = metadata_duration_ms(event.metadata.as_ref());
    let payload_hash = tool_turn_event_payload_hash(
        event,
        run_id.as_deref(),
        tool_call_id.as_deref(),
        skill_version.as_deref(),
        meta_tool_name.as_deref(),
        meta_duration_ms,
    );
    let ingestion_write_id = Uuid::new_v4().to_string();
    let insert_sql = matrixone_statement_with_null_shape(
        "INSERT IGNORE INTO agent_events \
         (event_id, session_id, user_id, agent_id, agent_version, event_type, content, \
          parent_event_id, causal_chain_id, run_id, tool_call_id, metadata, skill_name, skill_version, reasoning_content, \
          meta_tool_name, meta_duration_ms, payload_hash, ingestion_write_id, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, NOW())",
        [
            event.parent_event_id.is_some(),
            run_id.is_some(),
            tool_call_id.is_some(),
            metadata_json.is_some(),
            event.skill_name.is_some(),
            skill_version.is_some(),
            event.reasoning_content.is_some(),
            meta_tool_name.is_some(),
            meta_duration_ms.is_some(),
        ],
    );
    let result = query(&insert_sql)
        .bind(&event.event_id)
        .bind(&event.session_id)
        .bind(&event.user_id)
        .bind(event.agent_id.as_deref().unwrap_or("astra-cli"))
        .bind(env!("CARGO_PKG_VERSION"))
        .bind(&event.event_type)
        .bind(&event.content)
        .bind(&event.parent_event_id)
        .bind(&event.causal_chain_id)
        .bind(&run_id)
        .bind(&tool_call_id)
        .bind(&metadata_json)
        .bind(&event.skill_name)
        .bind(&skill_version)
        .bind(&event.reasoning_content)
        .bind(&meta_tool_name)
        .bind(meta_duration_ms)
        .bind(&payload_hash)
        .bind(&ingestion_write_id)
        .execute(&mut **tx)
        .await?;
    let _reported_rows_affected = result.rows_affected();
    let outcome = classify_agent_event_capture_attempts(
        tx,
        &[AgentEventCaptureAttempt {
            user_id: &event.user_id,
            session_id: &event.session_id,
            event_id: &event.event_id,
            payload_hash: &payload_hash,
        }],
        &ingestion_write_id,
        TOOL_TURN_EVENT_COLLISION_SOURCE,
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| sqlx::Error::Protocol("missing tool event capture outcome".to_string()))?;
    let inserted = outcome == DurableCaptureOutcome::Inserted;
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

#[cfg(test)]
mod tests {
    use super::{
        INSERT_CORE_TURN_EVENT_SQL, core_turn_event_insert_values, metadata_string,
        metadata_tool_name, trace_event_insert_values, trace_event_payload_hash,
        unique_trace_event_indices,
    };
    use astra_core::matrixone_statement_with_null_shape;
    use astra_services::observation_capture::{
        ObservationPayloadDomain, canonical_observation_payload_hash,
    };
    use astra_turn_core::contracts::TurnCoreEventRecord;
    use astra_turn_core::trace_event::TraceEvent;

    #[tokio::test]
    #[ignore = "requires ASTRA_TEST_DB_IT=1 and MatrixOne"]
    async fn trace_capture_batch_preserves_mixed_outcomes_and_collision_order() {
        use super::{DurableCaptureOutcome, insert_trace_events};
        use sqlx::Row;

        assert_eq!(std::env::var("ASTRA_TEST_DB_IT").as_deref(), Ok("1"));
        let settings = astra_core::MatrixOneSettings::from_env();
        let catalog =
            std::env::var("ASTRA_DATABASE_BOOTSTRAP_CATALOG").unwrap_or_else(|_| "mysql".into());
        astra_services::storage::ensure_core_schema(&settings, &catalog)
            .await
            .unwrap();
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(1)
            .connect(&settings.database_url_with_password())
            .await
            .unwrap();
        let owner = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        let first = TraceEvent::new("first", &session, &owner, "trace_span", "runtime");
        let mut tx = pool.begin().await.unwrap();
        let seeded = insert_trace_events(&mut tx, std::slice::from_ref(&first))
            .await
            .unwrap();
        assert_eq!(seeded.inserted, 1);
        let sibling = TraceEvent::new("sibling", &session, &owner, "trace_span", "runtime");
        let mut events = vec![first.clone(), sibling.clone(), sibling];
        for index in 0..130 {
            let mut collision = first.clone();
            collision.content = Some(format!("conflict-{index}"));
            collision.parent_event_id = Some("rejected-parent".into());
            events.push(collision);
        }
        let outcome = insert_trace_events(&mut tx, &events).await.unwrap();
        assert_eq!(outcome.inserted, 1);
        assert_eq!(outcome.last_inserted_event_id.as_deref(), Some("sibling"));
        assert_eq!(
            outcome.event_outcomes["sibling"],
            DurableCaptureOutcome::Inserted
        );
        assert!(matches!(
            outcome.event_outcomes["first"],
            DurableCaptureOutcome::Collision { .. }
        ));
        let receipt = sqlx::query("SELECT collision_count, stored_payload_hash, attempted_payload_hash, source FROM observation_identity_collisions WHERE user_id = ?")
            .bind(&owner).fetch_one(&mut *tx).await.unwrap();
        assert_eq!(receipt.get::<u64, _>("collision_count"), 130);
        assert_eq!(
            receipt.get::<String, _>("stored_payload_hash"),
            trace_event_payload_hash(&first).unwrap()
        );
        assert_eq!(
            receipt.get::<String, _>("attempted_payload_hash"),
            trace_event_payload_hash(events.last().unwrap()).unwrap()
        );
        assert_eq!(receipt.get::<String, _>("source"), "runtime_trace_event");
        let stored: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM agent_events WHERE user_id = ?")
            .bind(&owner)
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(stored, 2);
        let edges: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM agent_event_edges WHERE user_id = ?")
                .bind(&owner)
                .fetch_one(&mut *tx)
                .await
                .unwrap();
        assert_eq!(edges, 0, "rejected parents must not create causal edges");
        let replay = insert_trace_events(&mut tx, &events[..3]).await.unwrap();
        assert_eq!(replay.inserted, 0);
        assert!(
            replay
                .event_outcomes
                .values()
                .all(|outcome| *outcome == DurableCaptureOutcome::Replayed)
        );
        tx.rollback().await.unwrap();
        let remaining: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM observation_identity_collisions WHERE user_id = ?",
        )
        .bind(&owner)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(remaining, 0);
        pool.close().await;
    }

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
            20,
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
    fn token_usage_writers_preserve_partial_and_unavailable_samples() {
        for raw in [
            serde_json::json!({}),
            serde_json::json!({"output_tokens":7}),
            serde_json::json!({"input_tokens":null,"output_tokens":7,"total_tokens":null}),
        ] {
            let event = core_event_with_token_usage(Some(raw.clone()));
            let core = core_turn_event_insert_values(&event).unwrap();
            let mut trace = TraceEvent::new(
                "partial",
                "session-1",
                "user-1",
                "llm_round_completed",
                "llm_round",
            );
            trace.token_usage = Some(raw.clone());
            let trace = trace_event_insert_values(&trace).unwrap();
            assert_eq!(core.token_usage_json, trace.token_usage_json);
            assert_eq!(core.token_input, None);
            assert_eq!(trace.token_input, None);
            assert_eq!(core.token_total, None);
            assert_eq!(trace.token_total, None);
            assert_eq!(
                core.token_output,
                raw.get("output_tokens").and_then(serde_json::Value::as_i64)
            );
            assert_eq!(trace.token_output, core.token_output);
            let stored: serde_json::Value =
                serde_json::from_str(core.token_usage_json.as_deref().unwrap()).unwrap();
            assert_eq!(stored.get("output_tokens"), raw.get("output_tokens"));
            assert!(stored.get("input_tokens").is_none());
            assert!(stored.get("total_tokens").is_none());
        }
    }

    #[test]
    fn unavailable_usage_with_metadata_round_trips_without_stale_counters() {
        let raw = serde_json::json!({
            "input_tokens":null, "scope":"runtime_accounted_usage", "source":"provider",
            "prompt":99, "cache_read":98, "total":100, "prompt_cache_hit_ratio":0.98,
        });
        let event = core_event_with_token_usage(Some(raw));
        let values = core_turn_event_insert_values(&event).unwrap();
        assert_eq!(
            (values.token_input, values.token_output, values.token_total),
            (None, None, None)
        );
        let persisted: serde_json::Value =
            serde_json::from_str(values.token_usage_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            persisted,
            serde_json::json!({"scope":"runtime_accounted_usage","source":"provider","total_tokens":null})
        );
        let usage = astra_turn_types::CanonicalTokenUsage::from_json(&persisted).unwrap();
        assert_eq!(usage.to_json(), serde_json::json!({}));
        let replay = core_event_with_token_usage(Some(persisted));
        let replay = core_turn_event_insert_values(&replay).unwrap();
        assert_eq!(values.token_usage_json, replay.token_usage_json);
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
        assert!(
            persisted.get("prompt").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("completion").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("cache_read").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("cache_write").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("total").is_none(),
            "canonical serialization omits redundant aliases"
        );
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
        assert!(
            persisted.get("prompt").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("completion").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("cache_read").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("cache_write").is_none(),
            "canonical serialization omits redundant aliases"
        );
        assert!(
            persisted.get("total").is_none(),
            "canonical serialization omits redundant aliases"
        );
    }

    #[test]
    fn trace_batch_retains_only_the_first_occurrence_of_each_identity() {
        let event = |id| TraceEvent::new(id, "session-1", "user-1", "trace", "runtime");

        let existing_then_new = vec![event("existing"), event("new")];
        assert_eq!(unique_trace_event_indices(&existing_then_new), vec![0, 1]);

        let new_then_existing = vec![event("new"), event("existing")];
        assert_eq!(unique_trace_event_indices(&new_then_existing), vec![0, 1]);

        let repeated_new = vec![event("first"), event("last"), event("first")];
        assert_eq!(
            unique_trace_event_indices(&repeated_new),
            vec![0, 1],
            "the repeated tail id must not replace the final row actually inserted"
        );
    }

    #[test]
    fn trace_hash_detects_changed_persisted_correlation_fields() {
        let event = astra_turn_core::trace_event::TraceEvent::new(
            "stable-id",
            "session",
            "owner",
            "trace_span",
            "original-kind",
        );
        let original = trace_event_payload_hash(&event).unwrap();
        let mut changed = event.clone();
        changed.trace_kind = "different-kind".into();
        assert_ne!(original, trace_event_payload_hash(&changed).unwrap());
        let mut changed = event.clone();
        changed.turn_seq = Some(2);
        assert_ne!(original, trace_event_payload_hash(&changed).unwrap());
        let mut changed = event;
        changed.run_id = Some("different-run".into());
        assert_ne!(original, trace_event_payload_hash(&changed).unwrap());
    }

    #[test]
    fn trace_hash_covers_the_complete_producer_envelope() {
        let mut event = TraceEvent::new(
            "trace-shared",
            "session-1",
            "user-1",
            "trace_span",
            "producer-only-kind",
        );
        event.created_at = chrono::DateTime::parse_from_rfc3339("2026-09-20T01:02:03+00:00")
            .unwrap()
            .with_timezone(&chrono::Utc);
        event.content = Some("captured".to_string());
        event.parent_event_id = Some("parent-1".to_string());
        event.causal_chain_id = Some("chain-1".to_string());
        event.metadata = serde_json::json!({"run_id": "run-1", "span_id": "span-1"});

        assert_eq!(
            trace_event_payload_hash(&event).unwrap(),
            canonical_observation_payload_hash(
                ObservationPayloadDomain::AgentEvent,
                &serde_json::to_value(&event).unwrap(),
            )
        );
    }

    #[test]
    fn trace_event_statement_identity_includes_causal_chain_nullness() {
        let mut event = TraceEvent::new(
            "trace-shape",
            "session-1",
            "user-1",
            "platform_event",
            "platform",
        );
        let without_chain = trace_event_insert_values(&event).unwrap();
        let without_chain_sql = matrixone_statement_with_null_shape(
            "INSERT INTO agent_events VALUES (?)",
            without_chain.nullable_shape(&event),
        );

        event.causal_chain_id = Some("chain-1".to_string());
        let with_chain = trace_event_insert_values(&event).unwrap();
        let with_chain_sql = matrixone_statement_with_null_shape(
            "INSERT INTO agent_events VALUES (?)",
            with_chain.nullable_shape(&event),
        );

        assert_ne!(without_chain_sql, with_chain_sql);
        assert!(with_chain_sql.contains("astra-null-shape:001"));
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
