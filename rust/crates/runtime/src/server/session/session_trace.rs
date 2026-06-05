use crate::orchestration::{
    agent_trace_requires_result_collection, agent_trace_status_from_event,
    is_agent_trace_settled_event,
};
use crate::server::*;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct TraceApiEvent {
    pub event_id: String,
    pub event_type: String,
    pub trace_kind: Option<String>,
    pub session_id: String,
    pub turn_id: Option<String>,
    pub turn_seq: Option<i64>,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    pub round_index: Option<i64>,
    pub tool_call_id: Option<String>,
    pub meta_tool_name: Option<String>,
    pub content: Option<String>,
    pub reasoning_content: Option<String>,
    pub token_usage: Option<Value>,
    pub llm_model_used: Option<String>,
    pub meta_duration_ms: Option<i32>,
    pub parent_event_id: Option<String>,
    pub causal_chain_id: Option<String>,
    pub metadata: Value,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct TraceToolCallNode {
    pub tool_call_id: Option<String>,
    pub meta_tool_name: Option<String>,
    pub child_run_id: Option<String>,
    pub events: Vec<TraceApiEvent>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct TraceRoundNode {
    pub round_index: i64,
    pub events: Vec<TraceApiEvent>,
    pub tool_calls: Vec<TraceToolCallNode>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct TraceChildRunNode {
    pub run_id: String,
    pub parent_run_id: Option<String>,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub events: Vec<TraceApiEvent>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct TraceTreeNode {
    #[serde(rename = "type")]
    pub node_type: &'static str,
    pub run_id: Option<String>,
    pub agent_id: Option<String>,
    pub rounds: Vec<TraceRoundNode>,
    pub children: Vec<TraceChildRunNode>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
pub(crate) struct TraceResponse {
    pub session_id: String,
    pub turn_id: String,
    pub turn_seq: Option<i64>,
    pub source: &'static str,
    pub complete: bool,
    pub warnings: Vec<String>,
    pub missing: Vec<String>,
    pub events: Vec<TraceApiEvent>,
    pub tree: TraceTreeNode,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TraceRunLiveness {
    pub active_run_ids: HashSet<String>,
}

impl TraceRunLiveness {
    fn turn_is_running(&self, events: &[TraceApiEvent]) -> bool {
        events
            .iter()
            .filter_map(|event| event.run_id.as_deref())
            .any(|run_id| self.active_run_ids.contains(run_id))
    }
}

pub(crate) async fn get_session_turn_trace_handler(
    State(state): State<AppState>,
    Path((session_id, turn_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<TraceResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user = state.auth_service.current_user(&headers).await?;
    let session = state
        .session_service
        .get_session(session_id.clone(), user.user_id.clone())
        .await?;
    let pool = state
        .shared_pool
        .as_ref()
        .ok_or_else(|| internal_error("shared MatrixOne pool is not configured"))?;

    let events = load_trace_events(pool, &session.session_id, &session.user_id, &turn_id).await?;
    let liveness =
        load_trace_run_liveness(pool, &session.session_id, &session.user_id, &events).await?;

    Ok(Json(build_trace_response(
        session.session_id,
        turn_id,
        events,
        liveness,
    )))
}

async fn load_trace_events(
    pool: &SharedPool,
    session_id: &str,
    user_id: &str,
    turn_id: &str,
) -> Result<Vec<TraceApiEvent>, (StatusCode, Json<ErrorResponse>)> {
    let rows = sqlx::query(
        "SELECT event_id, session_id, user_id, agent_id, event_type, content,
                parent_event_id, causal_chain_id, run_id, parent_run_id, turn_id,
                turn_seq, round_index, tool_call_id, parent_agent_id, trace_kind,
                token_usage, llm_model_used, metadata, reasoning_content,
                meta_tool_name, meta_duration_ms, created_at
         FROM agent_events
         WHERE session_id = ? AND user_id = ? AND turn_id = ?
         ORDER BY created_at ASC, event_id ASC",
    )
    .bind(session_id)
    .bind(user_id)
    .bind(turn_id)
    .fetch_all(pool.get())
    .await
    .map_err(internal_error)?;

    Ok(rows.into_iter().map(trace_event_from_row).collect())
}

async fn load_trace_run_liveness(
    pool: &SharedPool,
    session_id: &str,
    user_id: &str,
    events: &[TraceApiEvent],
) -> Result<TraceRunLiveness, (StatusCode, Json<ErrorResponse>)> {
    let run_ids = events
        .iter()
        .filter_map(|event| event.run_id.as_deref())
        .collect::<HashSet<_>>();
    if run_ids.is_empty() {
        return Ok(TraceRunLiveness::default());
    }

    let mut builder =
        sqlx::QueryBuilder::<sqlx::MySql>::new("SELECT run_id FROM agent_runs WHERE session_id = ");
    builder
        .push_bind(session_id)
        .push(" AND user_id = ")
        .push_bind(user_id)
        .push(" AND status IN ('running', 'waiting', 'paused') AND run_id IN (");
    let mut separated = builder.separated(", ");
    for run_id in run_ids {
        separated.push_bind(run_id);
    }
    separated.push_unseparated(")");

    let rows = builder
        .build()
        .fetch_all(pool.get())
        .await
        .map_err(internal_error)?;
    let active_run_ids = rows
        .into_iter()
        .filter_map(|row| row.try_get::<String, _>("run_id").ok())
        .collect();
    Ok(TraceRunLiveness { active_run_ids })
}

fn trace_event_from_row(row: sqlx::mysql::MySqlRow) -> TraceApiEvent {
    TraceApiEvent {
        event_id: row.try_get("event_id").unwrap_or_default(),
        event_type: row.try_get("event_type").unwrap_or_default(),
        trace_kind: row
            .try_get::<Option<String>, _>("trace_kind")
            .ok()
            .flatten(),
        session_id: row.try_get("session_id").unwrap_or_default(),
        turn_id: row.try_get::<Option<String>, _>("turn_id").ok().flatten(),
        turn_seq: row.try_get::<Option<i64>, _>("turn_seq").ok().flatten(),
        run_id: row.try_get::<Option<String>, _>("run_id").ok().flatten(),
        parent_run_id: row
            .try_get::<Option<String>, _>("parent_run_id")
            .ok()
            .flatten(),
        agent_id: row.try_get::<Option<String>, _>("agent_id").ok().flatten(),
        parent_agent_id: row
            .try_get::<Option<String>, _>("parent_agent_id")
            .ok()
            .flatten(),
        round_index: row.try_get::<Option<i64>, _>("round_index").ok().flatten(),
        tool_call_id: row
            .try_get::<Option<String>, _>("tool_call_id")
            .ok()
            .flatten(),
        meta_tool_name: row
            .try_get::<Option<String>, _>("meta_tool_name")
            .ok()
            .flatten(),
        content: row.try_get::<Option<String>, _>("content").ok().flatten(),
        reasoning_content: row
            .try_get::<Option<String>, _>("reasoning_content")
            .ok()
            .flatten(),
        token_usage: parse_json_column(&row, "token_usage"),
        llm_model_used: row
            .try_get::<Option<String>, _>("llm_model_used")
            .ok()
            .flatten(),
        meta_duration_ms: row
            .try_get::<Option<i32>, _>("meta_duration_ms")
            .ok()
            .flatten(),
        parent_event_id: row
            .try_get::<Option<String>, _>("parent_event_id")
            .ok()
            .flatten(),
        causal_chain_id: row
            .try_get::<Option<String>, _>("causal_chain_id")
            .ok()
            .flatten(),
        metadata: parse_json_column(&row, "metadata").unwrap_or_else(|| serde_json::json!({})),
        created_at: row.try_get("created_at").unwrap_or_default(),
    }
}

fn parse_json_column(row: &sqlx::mysql::MySqlRow, column: &str) -> Option<Value> {
    row.try_get::<Option<String>, _>(column)
        .ok()
        .flatten()
        .and_then(|raw| serde_json::from_str(&raw).ok())
}

pub(crate) fn build_trace_response(
    session_id: String,
    turn_id: String,
    events: Vec<TraceApiEvent>,
    liveness: TraceRunLiveness,
) -> TraceResponse {
    let turn_seq = events.iter().find_map(|event| event.turn_seq);
    let tree = build_trace_tree(&events);
    let (complete, warnings, missing) = evaluate_trace_completeness(&events, &tree, &liveness);
    TraceResponse {
        session_id,
        turn_id,
        turn_seq,
        source: "database",
        complete,
        warnings,
        missing,
        events,
        tree,
    }
}

fn build_trace_tree(events: &[TraceApiEvent]) -> TraceTreeNode {
    let root_run_id = root_run_id(events);
    let root_agent_id = root_run_id
        .as_deref()
        .and_then(|run_id| {
            events
                .iter()
                .find(|event| event.run_id.as_deref() == Some(run_id) && event.agent_id.is_some())
        })
        .and_then(|event| event.agent_id.clone())
        .or_else(|| events.iter().find_map(|event| event.agent_id.clone()));

    let mut rounds_by_index: BTreeMap<i64, Vec<TraceApiEvent>> = BTreeMap::new();
    for event in events.iter().filter(|event| {
        event.run_id == root_run_id
            && matches!(
                event.trace_kind.as_deref(),
                Some("llm_round") | Some("tool_call")
            )
    }) {
        if let Some(round_index) = event.round_index {
            rounds_by_index
                .entry(round_index)
                .or_default()
                .push(event.clone());
        }
    }

    let rounds = rounds_by_index
        .into_iter()
        .map(|(round_index, round_events)| TraceRoundNode {
            round_index,
            tool_calls: build_tool_call_nodes(&round_events),
            events: round_events
                .iter()
                .filter(|event| event.trace_kind.as_deref() != Some("tool_call"))
                .cloned()
                .collect(),
        })
        .collect();

    let mut children_by_run: BTreeMap<String, Vec<TraceApiEvent>> = BTreeMap::new();
    for event in events {
        if is_child_run_event(event, root_run_id.as_deref()) {
            if let Some(run_id) = event.run_id.as_ref() {
                children_by_run
                    .entry(run_id.clone())
                    .or_default()
                    .push(event.clone());
            }
        }
    }

    let children = children_by_run
        .into_iter()
        .map(|(run_id, child_events)| {
            let status = child_events
                .iter()
                .rev()
                .find_map(|event| lifecycle_status(event).map(ToString::to_string));
            TraceChildRunNode {
                run_id,
                parent_run_id: child_events
                    .iter()
                    .find_map(|event| event.parent_run_id.clone()),
                agent_id: child_events.iter().find_map(|event| event.agent_id.clone()),
                status,
                events: child_events,
            }
        })
        .collect();

    TraceTreeNode {
        node_type: "turn",
        run_id: root_run_id,
        agent_id: root_agent_id,
        rounds,
        children,
    }
}

fn build_tool_call_nodes(events: &[TraceApiEvent]) -> Vec<TraceToolCallNode> {
    let mut by_call: BTreeMap<String, Vec<TraceApiEvent>> = BTreeMap::new();
    for event in events
        .iter()
        .filter(|event| event.trace_kind.as_deref() == Some("tool_call"))
    {
        let key = event
            .tool_call_id
            .clone()
            .unwrap_or_else(|| event.event_id.clone());
        by_call.entry(key).or_default().push(event.clone());
    }

    by_call
        .into_values()
        .map(|call_events| TraceToolCallNode {
            tool_call_id: call_events
                .iter()
                .find_map(|event| event.tool_call_id.clone()),
            meta_tool_name: call_events
                .iter()
                .find_map(|event| event.meta_tool_name.clone()),
            child_run_id: call_events
                .iter()
                .find_map(|event| metadata_string(&event.metadata, "child_run_id")),
            events: call_events,
        })
        .collect()
}

fn is_child_run_event(event: &TraceApiEvent, root_run_id: Option<&str>) -> bool {
    let Some(run_id) = event.run_id.as_deref() else {
        return false;
    };
    if Some(run_id) == root_run_id {
        return false;
    }
    event.parent_run_id.is_some()
}

fn root_run_id(events: &[TraceApiEvent]) -> Option<String> {
    events
        .iter()
        .find(|event| event.event_type == "user_query")
        .and_then(|event| event.run_id.clone())
        .or_else(|| {
            events
                .iter()
                .find(|event| event.parent_run_id.is_none() && event.run_id.is_some())
                .and_then(|event| event.run_id.clone())
        })
        .or_else(|| events.iter().find_map(|event| event.run_id.clone()))
}

fn evaluate_trace_completeness(
    events: &[TraceApiEvent],
    tree: &TraceTreeNode,
    liveness: &TraceRunLiveness,
) -> (bool, Vec<String>, Vec<String>) {
    let mut warnings = Vec::new();
    let mut missing = Vec::new();
    let turn_running = liveness.turn_is_running(events);

    if !events.iter().any(|event| event.event_type == "user_query") {
        missing.push("user_query".to_string());
    }

    if !turn_running {
        let final_response_exists = match tree.run_id.as_deref() {
            Some(root_run_id) => events.iter().any(|event| {
                event.event_type == "llm_response" && event.run_id.as_deref() == Some(root_run_id)
            }),
            None => events
                .iter()
                .any(|event| event.event_type == "llm_response"),
        };
        if !final_response_exists {
            missing.push("llm_response".to_string());
        }
    }

    for event in events
        .iter()
        .filter(|event| event.event_type == "trace_persistence_degraded")
    {
        warnings.push(format!("trace_persistence_degraded:{}", event.event_id));
        missing.push("trace_persistence_degraded".to_string());
    }

    let spawned_by_run = events
        .iter()
        .filter(|event| event.event_type == "agent_spawned")
        .filter_map(|event| event.run_id.as_deref().map(|run_id| (run_id, event)))
        .collect::<HashMap<_, _>>();
    let terminal_by_run = events
        .iter()
        .filter(|event| is_agent_trace_settled_event(event.event_type.as_str()))
        .filter_map(|event| event.run_id.as_deref().map(|run_id| (run_id, event)))
        .collect::<HashMap<_, _>>();
    let collected_child_runs = events
        .iter()
        .filter(|event| event.event_type == "agent_result_collected")
        .filter_map(|event| metadata_string(&event.metadata, "child_run_id"))
        .collect::<HashSet<_>>();

    for event in events.iter().filter(|event| {
        event.event_type == "tool_call_completed"
            && metadata_string(&event.metadata, "action").as_deref() == Some("spawn")
            && metadata_string(&event.metadata, "child_run_id").is_some()
    }) {
        let child_run_id = metadata_string(&event.metadata, "child_run_id").unwrap();
        if !spawned_by_run.contains_key(child_run_id.as_str()) {
            missing.push(format!("agent_spawned:{child_run_id}"));
        }
    }

    for (run_id, spawn_event) in &spawned_by_run {
        if !terminal_by_run.contains_key(run_id) && !liveness.active_run_ids.contains(*run_id) {
            missing.push(format!("agent_terminal:{run_id}"));
        }

        if spawn_event
            .metadata
            .get("run_in_background")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && terminal_by_run.get(run_id).is_some_and(|event| {
                agent_trace_requires_result_collection(
                    event.event_type.as_str(),
                    event.metadata.get("status").and_then(Value::as_str),
                )
            })
            && !collected_child_runs.contains(*run_id)
            && !turn_running
        {
            missing.push(format!("agent_result_collected:{run_id}"));
        }
    }

    (missing.is_empty(), warnings, missing)
}

fn lifecycle_status(event: &TraceApiEvent) -> Option<&'static str> {
    agent_trace_status_from_event(
        event.event_type.as_str(),
        event.metadata.get("status").and_then(Value::as_str),
    )
}

fn metadata_string(metadata: &Value, key: &str) -> Option<String> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn event(event_type: &str, run_id: &str) -> TraceApiEvent {
        TraceApiEvent {
            event_id: format!("{event_type}:{run_id}"),
            event_type: event_type.to_string(),
            trace_kind: Some("test".to_string()),
            session_id: "session-1".to_string(),
            turn_id: Some("turn-1".to_string()),
            turn_seq: Some(1),
            run_id: Some(run_id.to_string()),
            parent_run_id: None,
            agent_id: Some("root-agent".to_string()),
            parent_agent_id: None,
            round_index: None,
            tool_call_id: None,
            meta_tool_name: None,
            content: None,
            reasoning_content: None,
            token_usage: None,
            llm_model_used: None,
            meta_duration_ms: None,
            parent_event_id: None,
            causal_chain_id: Some("chain-1".to_string()),
            metadata: serde_json::json!({}),
            created_at: "2026-01-01 00:00:00.000000".to_string(),
        }
    }

    #[test]
    fn complete_trace_accepts_background_child_chain() {
        let mut events = vec![
            event("user_query", "root-run"),
            event("llm_round_completed", "root-run"),
            event("llm_response", "root-run"),
        ];
        let mut tool = event("tool_call_completed", "root-run");
        tool.trace_kind = Some("tool_call".to_string());
        tool.tool_call_id = Some("tool-1".to_string());
        tool.metadata = serde_json::json!({
            "action": "spawn",
            "child_run_id": "child-run"
        });
        events.push(tool);

        let mut spawned = event("agent_spawned", "child-run");
        spawned.parent_run_id = Some("root-run".to_string());
        spawned.metadata = serde_json::json!({"run_in_background": true});
        events.push(spawned);

        let mut completed = event("agent_completed", "child-run");
        completed.parent_run_id = Some("root-run".to_string());
        events.push(completed);

        let mut collected = event("agent_result_collected", "root-run");
        collected.metadata = serde_json::json!({"child_run_id": "child-run"});
        events.push(collected);

        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness::default(),
        );
        assert!(response.complete, "{:?}", response.missing);
        assert_eq!(response.source, "database");
    }

    #[test]
    fn child_trace_status_prefers_latest_terminal_status_over_spawned() {
        let mut events = vec![
            event("user_query", "root-run"),
            event("llm_response", "root-run"),
        ];

        let mut spawned = event("agent_spawned", "child-run");
        spawned.parent_run_id = Some("root-run".to_string());
        spawned.metadata = serde_json::json!({"status": "spawned"});
        events.push(spawned);

        let mut completed = event("agent_completed", "child-run");
        completed.parent_run_id = Some("root-run".to_string());
        completed.metadata = serde_json::json!({"status": "completed"});
        events.push(completed);

        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness::default(),
        );
        assert_eq!(
            response.tree.children[0].status.as_deref(),
            Some("completed")
        );
    }

    #[test]
    fn child_trace_status_preserves_interrupted_terminal_status() {
        let mut events = vec![
            event("user_query", "root-run"),
            event("llm_response", "root-run"),
        ];

        let mut spawned = event("agent_spawned", "child-run");
        spawned.parent_run_id = Some("root-run".to_string());
        events.push(spawned);

        let mut interrupted = event("agent_interrupted", "child-run");
        interrupted.parent_run_id = Some("root-run".to_string());
        interrupted.metadata = serde_json::json!({"status": "interrupted"});
        events.push(interrupted);

        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness::default(),
        );
        assert_eq!(
            response.tree.children[0].status.as_deref(),
            Some("interrupted")
        );
    }

    #[test]
    fn background_waiting_child_does_not_require_result_collected() {
        let mut events = vec![
            event("user_query", "root-run"),
            event("llm_round_completed", "root-run"),
            event("llm_response", "root-run"),
        ];

        let mut tool = event("tool_call_completed", "root-run");
        tool.trace_kind = Some("tool_call".to_string());
        tool.tool_call_id = Some("tool-1".to_string());
        tool.metadata = serde_json::json!({
            "action": "spawn",
            "child_run_id": "child-run"
        });
        events.push(tool);

        let mut spawned = event("agent_spawned", "child-run");
        spawned.parent_run_id = Some("root-run".to_string());
        spawned.metadata = serde_json::json!({"run_in_background": true});
        events.push(spawned);

        let mut waiting = event("agent_waiting", "child-run");
        waiting.parent_run_id = Some("root-run".to_string());
        waiting.metadata = serde_json::json!({"status": "waiting"});
        events.push(waiting);

        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness::default(),
        );
        assert!(response.complete, "{:?}", response.missing);
        assert_eq!(response.tree.children[0].status.as_deref(), Some("waiting"));
    }

    #[test]
    fn completeness_reports_missing_terminal_event() {
        let mut events = vec![
            event("user_query", "root-run"),
            event("llm_response", "root-run"),
        ];
        let mut spawned = event("agent_spawned", "child-run");
        spawned.parent_run_id = Some("root-run".to_string());
        events.push(spawned);

        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness::default(),
        );
        assert!(!response.complete);
        assert!(
            response
                .missing
                .contains(&"agent_terminal:child-run".to_string())
        );
    }

    #[test]
    fn completeness_reports_degraded_trace() {
        let events = vec![
            event("user_query", "root-run"),
            event("llm_response", "root-run"),
            event("trace_persistence_degraded", "root-run"),
        ];
        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness::default(),
        );
        assert!(!response.complete);
        assert!(
            response
                .missing
                .contains(&"trace_persistence_degraded".to_string())
        );
    }

    #[test]
    fn active_turn_does_not_require_final_response_yet() {
        let events = vec![event("user_query", "root-run")];
        let response = build_trace_response(
            "session-1".to_string(),
            "turn-1".to_string(),
            events,
            TraceRunLiveness {
                active_run_ids: HashSet::from(["root-run".to_string()]),
            },
        );
        assert!(response.complete, "{:?}", response.missing);
    }
}
