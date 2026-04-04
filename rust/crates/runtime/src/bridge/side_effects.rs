use super::*;

use std::sync::atomic::{AtomicU64, Ordering};

/// Global counters for fire-and-forget persistence failures.
/// Exposed via `/health` and observable without a DB connection.
pub static PERSIST_FAIL_COUNT: AtomicU64 = AtomicU64::new(0);
pub static PERSIST_OK_COUNT: AtomicU64 = AtomicU64::new(0);

/// Increment the failure counter and log the error.
fn record_persist_failure(context: &str, error: &impl std::fmt::Display) {
    PERSIST_FAIL_COUNT.fetch_add(1, Ordering::Relaxed);
    astra_core::agent_error!("persist", "{context}: {error}");
}

/// Increment the success counter.
fn record_persist_ok() {
    PERSIST_OK_COUNT.fetch_add(1, Ordering::Relaxed);
}

pub fn take_bridge_prompt_fingerprints(
    bridge_state: &mut serde_json::Map<String, serde_json::Value>,
) -> Vec<String> {
    bridge_state
        .remove("prompt_fingerprints")
        .and_then(|value| value.as_array().cloned())
        .map(|items| {
            items
                .into_iter()
                .filter_map(|item| item.as_str().map(ToString::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn take_bridge_side_effect_inputs(
    bridge_state: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if let Some(side_effect_inputs) = bridge_state
        .remove("side_effect_inputs")
        .and_then(|value| value.as_object().cloned())
    {
        return Some(side_effect_inputs);
    }

    let mut side_effect_inputs = serde_json::Map::new();

    for (bridge_key, payload_key) in [
        ("side_effect_messages", "messages"),
        ("side_effect_tool_results", "tool_results"),
        ("side_effect_full_text", "full_text"),
        ("side_effect_cloud_tool_calls", "cloud_tool_calls"),
        ("side_effect_edge_tool_calls", "edge_tool_calls"),
        ("side_effect_reasoning_content", "reasoning_content"),
        ("side_effect_cloud_tool_results", "cloud_tool_results"),
        ("side_effect_context_capture_id", "context_capture_id"),
        ("side_effect_model_used", "model_used"),
        ("side_effect_token_usage", "token_usage"),
        ("side_effect_llm_params", "llm_params"),
        ("side_effect_agent_id", "agent_id"),
        ("side_effect_routing_meta", "routing_meta"),
        (
            "side_effect_tool_quality_assessments",
            "tool_quality_assessments",
        ),
        ("side_effect_sections", "sections"),
        ("side_effect_session_start", "session_start"),
    ] {
        if let Some(value) = bridge_state.remove(bridge_key) {
            side_effect_inputs.insert(payload_key.to_string(), value);
        }
    }

    (!side_effect_inputs.is_empty()).then_some(side_effect_inputs)
}

pub(super) fn take_bridge_tail_update_args(
    bridge_state: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if let Some(tail_update_args) = bridge_state
        .remove("tail_update_args")
        .and_then(|value| value.as_object().cloned())
    {
        return Some(tail_update_args);
    }

    let full_text = bridge_state.remove("tail_full_text")?;
    let mut tail_update_args = serde_json::Map::new();
    tail_update_args.insert("full_text".to_string(), full_text);

    for (bridge_key, payload_key) in [
        ("tail_tool_calls", "tool_calls"),
        ("tail_reasoning_content", "reasoning_content"),
        ("tail_cloud_loop_history", "cloud_loop_history"),
    ] {
        if let Some(value) = bridge_state.remove(bridge_key) {
            tail_update_args.insert(payload_key.to_string(), value);
        }
    }

    Some(tail_update_args)
}

pub fn run_bridge_hook_side_effects(
    payload: Option<serde_json::Value>,
    turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    turn_observer_worker: Arc<dyn TurnObserverWorker>,
    turn_learning_writer: Option<Arc<dyn TurnLearningWriter>>,
) {
    let Some(payload) = payload else {
        return;
    };
    tokio::spawn(async move {
        if let Some((plan, writer)) =
            build_hook_db_persist_from_payload(&payload, turn_hook_db_writer)
            && let Err(error) = writer.persist(plan).await
        {
            record_persist_failure("hook_db_persist", &error);
            return;
        }
        let reflection_actions = build_reflection_actions_from_payload(&payload);
        let reflection_transfer =
            resolve_reflection_transfer(turn_reflection_state_store.clone(), reflection_actions)
                .await;
        if let Some(mark) = reflection_transfer.mark
            && let Err(error) = turn_reflection_state_store.mark_reflecting(mark).await
        {
            record_persist_failure("reflection_state_mark", &error);
        }
        if let Some(lesson) = reflection_transfer.lesson
            && let Err(error) = turn_reflection_lesson_writer.persist_lesson(lesson).await
        {
            record_persist_failure("reflection_lesson_persist", &error);
        }
        if let Some(observer_request) = build_observer_request_from_payload(&payload)
            && let Err(error) = turn_observer_worker.run(observer_request).await
        {
            record_persist_failure("observer_run", &error);
        }
        // Pipeline learning: extract turn outcome and update EntityGraph/PatternLibrary/Calibrator
        if let Some(writer) = turn_learning_writer
            && let Some(outcome) =
                crate::pipeline::learning::build_learning_outcome_from_payload(&payload)
            && let Err(error) = writer.record_outcome(outcome).await
        {
            record_persist_failure("pipeline_learning", &error);
        }
        record_persist_ok();
    });
}

pub(super) fn dispatch_bridge_side_effect_request(
    payload: Option<serde_json::Value>,
    turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
    turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
    turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
    turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
) {
    let Some(payload) = payload else {
        return;
    };
    tokio::spawn(async move {
        let auxiliary_event_persist = build_auxiliary_event_persist_from_payload(
            &payload,
            turn_auxiliary_event_writer.clone(),
        );
        let core_event_persist =
            build_core_event_persist_from_payload(&payload, turn_core_event_writer.clone());
        let tool_event_persist =
            build_tool_event_persist_from_payload(&payload, turn_tool_event_writer.clone());
        let mut persisted_llm_response_event_id: Option<String> = None;
        if let Some((plan, writer)) = core_event_persist {
            match writer.persist(plan).await {
                Ok(outcome) => {
                    persisted_llm_response_event_id = outcome.llm_response_event_id;
                }
                Err(error) => {
                    record_persist_failure("core_event_persist", &error);
                    return;
                }
            }
        }
        if let Some((plan, writer)) = tool_event_persist
            && let Err(error) = writer.persist(plan).await
        {
            record_persist_failure("tool_event_persist", &error);
            return;
        }
        if let Some((events, writer)) = auxiliary_event_persist
            && let Err(error) = writer.persist_events(events).await
        {
            record_persist_failure("auxiliary_event_persist", &error);
        }
        let session_activity_update = build_session_activity_update_from_persist_payload(
            &payload,
            turn_session_activity_writer.clone(),
            persisted_llm_response_event_id.as_deref(),
        );
        if let Some((session_id, plan, writer)) = session_activity_update
            && let Err(error) = writer.update_session_activity(&session_id, plan).await
        {
            record_persist_failure("session_activity_update", &error);
        } else {
            record_persist_ok();
        }
    });
}

const BRIDGE_SNAPSHOT_TURN_INTERVAL: usize = 3;

fn build_auxiliary_event_persist_from_payload(
    payload: &serde_json::Value,
    turn_auxiliary_event_writer: Arc<dyn TurnAuxiliaryEventWriter>,
) -> Option<(
    Vec<TurnAuxiliaryEventRecord>,
    Arc<dyn TurnAuxiliaryEventWriter>,
)> {
    let persist_payload = payload.as_object()?;
    if persist_payload
        .get("run_auxiliary_event_persist")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return None;
    }
    let user_id = optional_object_str(persist_payload, "user_id")?.to_string();
    let session_id = optional_object_str(persist_payload, "session_id")?.to_string();
    let parent_event_id =
        optional_object_str(persist_payload, "user_query_event_id").map(ToString::to_string);
    let causal_chain_id = optional_object_str(persist_payload, "turn_chain_id")
        .map(ToString::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let messages = object_array(persist_payload, "messages");
    let history = object_array_maps(persist_payload, "history");
    let turn_count = persist_payload
        .get("turn_count")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0)
        .max(0) as usize;
    let user_content = first_user_content(&messages);
    let mut events = Vec::new();

    if let Some(routing_meta) = persist_payload
        .get("routing_meta")
        .and_then(serde_json::Value::as_object)
        .filter(|routing_meta| {
            !routing_meta
                .get("skipped")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
    {
        let payload = build_routing_decision_event_payload(routing_meta);
        events.push(build_auxiliary_event_record(
            &user_id,
            &session_id,
            "routing_decision",
            payload,
            parent_event_id.clone(),
            &causal_chain_id,
        ));
    }

    for assessment in object_array_maps(persist_payload, "tool_quality_assessments")
        .into_iter()
        .filter(|assessment| {
            assessment.get("grade").and_then(serde_json::Value::as_str) != Some("complete")
        })
    {
        let payload = build_tool_result_quality_event_payload(&assessment);
        events.push(build_auxiliary_event_record(
            &user_id,
            &session_id,
            "tool_result_quality",
            payload,
            parent_event_id.clone(),
            &causal_chain_id,
        ));
    }

    if should_persist_session_history_snapshot(
        !history.is_empty(),
        user_content.is_some(),
        turn_count,
        BRIDGE_SNAPSHOT_TURN_INTERVAL,
    ) {
        let content = serde_json::to_value(build_session_history_snapshot(&history, 500)).ok()?;
        let payload = serde_json::Map::from_iter([
            ("content".to_string(), content),
            (
                "metadata".to_string(),
                serde_json::json!({"turn_count": turn_count}),
            ),
        ]);
        events.push(build_auxiliary_event_record(
            &user_id,
            &session_id,
            "session_history_snapshot",
            payload,
            parent_event_id,
            &causal_chain_id,
        ));
    }

    Some((events, turn_auxiliary_event_writer))
}

fn build_core_event_persist_from_payload(
    payload: &serde_json::Value,
    turn_core_event_writer: Arc<dyn TurnCoreEventWriter>,
) -> Option<(TurnCorePersistPlan, Arc<dyn TurnCoreEventWriter>)> {
    let persist_payload = payload.as_object()?;
    let run_request_response_persist = persist_payload
        .get("run_request_response_persist")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let run_snapshot_link_update = persist_payload
        .get("run_snapshot_link_update")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if run_request_response_persist && run_snapshot_link_update {
        return None;
    }
    let user_id = optional_object_str(persist_payload, "user_id")?.to_string();
    let session_id = optional_object_str(persist_payload, "session_id")?.to_string();
    let messages = object_array(persist_payload, "messages");
    let turn_chain_id = optional_object_str(persist_payload, "turn_chain_id")
        .map(ToString::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let parent_event_id =
        optional_object_str(persist_payload, "user_query_event_id").map(ToString::to_string);
    let user_query_event = if !run_request_response_persist {
        first_user_content(&messages).map(|user_content| TurnCoreEventRecord {
            event_id: parent_event_id
                .clone()
                .unwrap_or_else(|| Uuid::now_v7().to_string()),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            event_type: "user_query".to_string(),
            content: user_content.to_string(),
            parent_event_id: None,
            causal_chain_id: turn_chain_id.clone(),
            llm_model_used: None,
            token_usage: None,
            llm_params: None,
            reasoning_content: None,
        })
    } else {
        None
    };

    let tool_calls = object_array(persist_payload, "tool_calls");
    let llm_plan = build_llm_response_persist_plan(
        optional_object_str(persist_payload, "full_text").unwrap_or_default(),
        !tool_calls.is_empty(),
        optional_object_str(persist_payload, "reasoning_content").unwrap_or_default(),
    );
    let llm_response_event = if !run_request_response_persist && llm_plan.should_persist {
        Some(TurnCoreEventRecord {
            event_id: Uuid::now_v7().to_string(),
            user_id: user_id.clone(),
            session_id: session_id.clone(),
            event_type: "llm_response".to_string(),
            content: llm_plan.content,
            parent_event_id: parent_event_id.clone(),
            causal_chain_id: turn_chain_id.clone(),
            llm_model_used: optional_object_str(persist_payload, "model_used")
                .map(ToString::to_string),
            token_usage: optional_object_value(persist_payload, "token_usage"),
            llm_params: optional_object_value(persist_payload, "llm_params"),
            reasoning_content: llm_plan.reasoning_content,
        })
    } else {
        None
    };

    let snapshot_link_plan = if !run_snapshot_link_update {
        build_snapshot_link_plan(
            optional_object_str(persist_payload, "context_capture_id"),
            parent_event_id.as_deref(),
            llm_response_event
                .as_ref()
                .map(|event| event.event_id.as_str()),
        )
    } else {
        None
    };

    Some((
        TurnCorePersistPlan {
            user_query_event,
            llm_response_event,
            snapshot_link_plan,
        },
        turn_core_event_writer,
    ))
}

fn build_tool_event_persist_from_payload(
    payload: &serde_json::Value,
    turn_tool_event_writer: Arc<dyn TurnToolEventWriter>,
) -> Option<(TurnToolEventPersistPlan, Arc<dyn TurnToolEventWriter>)> {
    let persist_payload = payload.as_object()?;
    if persist_payload
        .get("run_tool_event_persist")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return None;
    }
    let user_id = optional_object_str(persist_payload, "user_id")?.to_string();
    let session_id = optional_object_str(persist_payload, "session_id")?.to_string();
    let parent_event_id =
        optional_object_str(persist_payload, "user_query_event_id").map(ToString::to_string);
    let causal_chain_id = optional_object_str(persist_payload, "turn_chain_id")
        .map(ToString::to_string)
        .unwrap_or_else(|| Uuid::now_v7().to_string());
    let tool_results = object_array_maps(persist_payload, "tool_results");
    let tool_calls = object_array_maps(persist_payload, "tool_calls");
    let cloud_tool_results = object_array_maps(persist_payload, "cloud_tool_results");
    let reasoning_content =
        optional_object_str(persist_payload, "reasoning_content").unwrap_or_default();
    let mut events = Vec::new();

    for tool_result in &tool_results {
        let payload = build_tool_result_event_payload(tool_result, "edge", TOOL_RESULT_AUDIT_CHARS);
        events.push(build_tool_event_record(
            &user_id,
            &session_id,
            "tool_result",
            payload,
            parent_event_id.clone(),
            &causal_chain_id,
        ));
    }
    for (index, tool_call) in tool_calls.iter().enumerate() {
        let payload = build_tool_call_event_payload(tool_call, index, reasoning_content);
        events.push(build_tool_event_record(
            &user_id,
            &session_id,
            "tool_call",
            payload,
            parent_event_id.clone(),
            &causal_chain_id,
        ));
    }
    for tool_result in &cloud_tool_results {
        let payload =
            build_tool_result_event_payload(tool_result, "cloud", TOOL_RESULT_AUDIT_CHARS);
        events.push(build_tool_event_record(
            &user_id,
            &session_id,
            "tool_result",
            payload,
            parent_event_id.clone(),
            &causal_chain_id,
        ));
    }

    if events.is_empty() {
        None
    } else {
        Some((TurnToolEventPersistPlan { events }, turn_tool_event_writer))
    }
}

fn build_hook_db_persist_from_payload(
    payload: &serde_json::Value,
    turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
) -> Option<(TurnHookDbPersistPlan, Arc<dyn TurnHookDbWriter>)> {
    let hook_payload = payload.as_object()?;
    if hook_payload
        .get("run_hook_db_writes")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return None;
    }
    let session_id = optional_object_str(hook_payload, "session_id")?.to_string();
    let user_id = optional_object_str(hook_payload, "user_id")?.to_string();
    let parent_event_id = optional_object_str(hook_payload, "parent_event_id")?.to_string();
    let messages = object_array(hook_payload, "messages");
    let run_implicit_feedback = hook_payload
        .get("run_implicit_feedback")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    let tool_calls = object_array_maps(hook_payload, "tool_calls");
    let user_content = first_user_content(&messages).unwrap_or_default();
    let tool_call_names = tool_calls
        .iter()
        .filter_map(|tool_call| {
            tool_call
                .get("function")
                .and_then(serde_json::Value::as_object)
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();
    let decision_audit = Some(TurnDecisionAuditRecord {
        decision_id: Uuid::now_v7().to_string(),
        session_id: session_id.clone(),
        event_id: parent_event_id.clone(),
        decision_type: if tool_call_names.is_empty() {
            "response_generation".to_string()
        } else {
            "tool_selection".to_string()
        },
        decision_output: serde_json::json!({
            "text": truncate_text(optional_object_str(hook_payload, "full_text").unwrap_or_default(), 500),
            "tool_calls": tool_call_names,
            "model_used": optional_object_str(hook_payload, "model_used"),
        }),
        model_used: optional_object_str(hook_payload, "model_used").map(ToString::to_string),
        context_capture_id: optional_object_str(hook_payload, "context_capture_id")
            .map(ToString::to_string),
    });
    let skill_selection = tool_calls
        .first()
        .and_then(|tool_call| {
            tool_call
                .get("function")
                .and_then(serde_json::Value::as_object)
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
        })
        .map(|first_tool_name| TurnSkillSelectionRecord {
            event_id: Uuid::now_v7().to_string(),
            session_id,
            user_id: user_id.clone(),
            agent_id: optional_object_str(hook_payload, "agent_id").map(ToString::to_string),
            user_query: truncate_text(user_content, 2000),
            selected_skills: tool_calls
                .iter()
                .filter_map(|tool_call| {
                    tool_call
                        .get("function")
                        .and_then(serde_json::Value::as_object)
                        .and_then(|function| function.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
                .collect(),
            skill_name: first_tool_name.to_string(),
            skill_version: None,
            selection_method: "llm_tool_choice".to_string(),
            execution_success: None,
            execution_time_ms: None,
        });
    let implicit_feedback = if !run_implicit_feedback {
        first_user_content(&messages).and_then(|user_content| {
            let signal =
                detect_implicit_feedback_signal(user_content, latest_assistant_content(&messages));
            if signal.signal_type == "neutral" {
                None
            } else {
                Some(TurnImplicitFeedbackRecord {
                    feedback_id: Uuid::now_v7().to_string(),
                    prompt_template_id: "chat_turn".to_string(),
                    prompt_version: "auto".to_string(),
                    llm_request_id: parent_event_id.clone(),
                    rating: implicit_feedback_rating(&signal.signal_type),
                    comment: Some(format!(
                        "[implicit:{}] {}",
                        signal.signal_type, signal.evidence
                    )),
                    metadata: Some(serde_json::json!({
                        "source": "implicit_heuristic",
                        "confidence": signal.confidence.to_string(),
                    })),
                })
            }
        })
    } else {
        None
    };
    Some((
        TurnHookDbPersistPlan {
            decision_audit,
            skill_selection,
            implicit_feedback,
            reflection_mark: None,
            reflection_lesson: None,
        },
        turn_hook_db_writer,
    ))
}

fn build_reflection_actions_from_payload(
    payload: &serde_json::Value,
) -> Option<(
    Option<TurnReflectionMark>,
    Option<TurnReflectionLessonRequest>,
)> {
    let hook_payload = payload.as_object()?;
    let run_reflection_learning = hook_payload
        .get("run_reflection_learning")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if run_reflection_learning {
        return None;
    }
    let session_id = optional_object_str(hook_payload, "session_id")?.to_string();
    let tool_calls = object_array_maps(hook_payload, "tool_calls");
    let tool_results = object_array_maps(hook_payload, "tool_results");
    let tc_names = tool_calls
        .iter()
        .filter_map(|tool_call| {
            tool_call
                .get("function")
                .and_then(serde_json::Value::as_object)
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string)
        })
        .collect::<Vec<_>>();

    if tc_names.iter().any(|name| name == "reflect") {
        let reflect_output = tool_results
            .iter()
            .find(|result| {
                result
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    == "reflect"
            })
            .and_then(|result| {
                result
                    .get("result")
                    .and_then(serde_json::Value::as_str)
                    .map(ToString::to_string)
            })
            .unwrap_or_default();
        return Some((
            Some(TurnReflectionMark {
                session_id,
                reflect_output: reflect_output.chars().take(500).collect(),
            }),
            None,
        ));
    }

    let retry_names = tc_names
        .into_iter()
        .filter(|name| !name.is_empty() && name != "reflect")
        .collect::<Vec<_>>();
    if retry_names.is_empty() {
        return Some((None, None));
    }
    let user_id = optional_object_str(hook_payload, "user_id")?.to_string();
    Some((
        None,
        Some(TurnReflectionLessonRequest {
            user_id,
            session_id,
            retry_names,
        }),
    ))
}

fn build_observer_request_from_payload(payload: &serde_json::Value) -> Option<TurnObserverRequest> {
    let hook_payload = payload.as_object()?;
    let run_observer = hook_payload
        .get("run_observer")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(true);
    if run_observer {
        return None;
    }
    let full_text = optional_object_str(hook_payload, "full_text").unwrap_or_default();
    let tool_calls = object_array(hook_payload, "tool_calls");
    if !should_run_observer(full_text, !tool_calls.is_empty()) {
        return None;
    }
    let messages = object_array(hook_payload, "messages");
    let user_id = optional_object_str(hook_payload, "user_id")?.to_string();
    let session_id = optional_object_str(hook_payload, "session_id")?.to_string();
    let observer_messages = build_observer_messages(first_user_content(&messages), full_text);
    let turn_count = hook_payload
        .get("turn_count")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let session_start = optional_object_value(hook_payload, "session_start");
    Some(TurnObserverRequest {
        user_id,
        session_id,
        messages: observer_messages,
        turn_count,
        session_start,
    })
}

#[derive(Clone, Debug, Default)]
struct ReflectionTransfer {
    mark: Option<TurnReflectionMark>,
    lesson: Option<TurnReflectionLessonRecord>,
}

async fn resolve_reflection_transfer(
    state_store: Arc<dyn TurnReflectionStateStore>,
    actions: Option<(
        Option<TurnReflectionMark>,
        Option<TurnReflectionLessonRequest>,
    )>,
) -> ReflectionTransfer {
    let Some((mark, lesson_request)) = actions else {
        return ReflectionTransfer::default();
    };

    if let Some(mark) = mark {
        return ReflectionTransfer {
            mark: Some(mark),
            lesson: None,
        };
    }

    let Some(lesson_request) = lesson_request else {
        return ReflectionTransfer::default();
    };
    let reflect_output = match state_store.pop_reflecting(&lesson_request.session_id).await {
        Ok(Some(mark)) => mark.reflect_output,
        Ok(None) => String::new(),
        Err(error) => {
            astra_core::agent_error!("bridge", "reflection state pop failed: {error}");
            String::new()
        }
    };
    if reflect_output.is_empty() {
        return ReflectionTransfer::default();
    }
    ReflectionTransfer {
        mark: None,
        lesson: Some(TurnReflectionLessonRecord {
            user_id: lesson_request.user_id,
            session_id: lesson_request.session_id,
            content: format!(
                "Reflection-driven fix: after reviewing decision history, retried with {}. Context: {}",
                lesson_request.retry_names.join(", "),
                reflect_output.chars().take(200).collect::<String>(),
            ),
        }),
    }
}

fn build_auxiliary_event_record(
    user_id: &str,
    session_id: &str,
    event_type: &str,
    payload: serde_json::Map<String, serde_json::Value>,
    parent_event_id: Option<String>,
    causal_chain_id: &str,
) -> TurnAuxiliaryEventRecord {
    TurnAuxiliaryEventRecord {
        event_id: Uuid::now_v7().to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        event_type: event_type.to_string(),
        content: payload
            .get("content")
            .cloned()
            .map(|content| match content {
                serde_json::Value::String(content) => content,
                value => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
            })
            .unwrap_or_else(|| "null".to_string()),
        parent_event_id,
        causal_chain_id: causal_chain_id.to_string(),
        metadata: payload
            .get("metadata")
            .cloned()
            .filter(|metadata| !metadata.is_null()),
        reasoning_content: payload
            .get("reasoning_content")
            .and_then(serde_json::Value::as_str)
            .map(ToString::to_string),
    }
}

fn build_tool_event_record(
    user_id: &str,
    session_id: &str,
    event_type: &str,
    payload: PersistEventPayload,
    parent_event_id: Option<String>,
    causal_chain_id: &str,
) -> TurnToolEventRecord {
    TurnToolEventRecord {
        event_id: Uuid::now_v7().to_string(),
        user_id: user_id.to_string(),
        session_id: session_id.to_string(),
        event_type: event_type.to_string(),
        content: match payload.content {
            serde_json::Value::String(content) => content,
            value => serde_json::to_string(&value).unwrap_or_else(|_| "null".to_string()),
        },
        parent_event_id,
        causal_chain_id: causal_chain_id.to_string(),
        metadata: (!payload.metadata.is_empty())
            .then_some(serde_json::Value::Object(payload.metadata)),
        skill_name: (!payload.skill_name.is_empty()).then_some(payload.skill_name),
        skill_version: None,
        reasoning_content: payload.reasoning_content,
    }
}

fn truncate_text(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn build_session_activity_update_from_persist_payload(
    payload: &serde_json::Value,
    turn_session_activity_writer: Arc<dyn TurnSessionActivityWriter>,
    llm_response_event_id_override: Option<&str>,
) -> Option<(
    String,
    SessionActivityUpdatePlan,
    Arc<dyn TurnSessionActivityWriter>,
)> {
    let persist_payload = payload.as_object()?;
    if persist_payload
        .get("run_session_activity")
        .and_then(serde_json::Value::as_bool)
        != Some(false)
    {
        return None;
    }
    let session_id = optional_object_str(persist_payload, "session_id")?.to_string();
    let messages = object_array(persist_payload, "messages");
    let tool_results = object_array(persist_payload, "tool_results");
    let tool_calls = object_array(persist_payload, "tool_calls");
    let cloud_tool_results = object_array(persist_payload, "cloud_tool_results");
    let user_content = messages.iter().find_map(|message| {
        let message = message.as_object()?;
        if message.get("role").and_then(serde_json::Value::as_str) == Some("user") {
            return optional_object_str(message, "content");
        }
        None
    });
    let full_text = optional_object_str(persist_payload, "full_text").unwrap_or_default();
    let plan = build_session_activity_update_plan(
        user_content.is_some_and(|content| !content.is_empty()),
        tool_results.len(),
        tool_calls.len(),
        cloud_tool_results.len(),
        !full_text.trim().is_empty() || !tool_calls.is_empty(),
        optional_object_str(persist_payload, "user_query_event_id"),
        llm_response_event_id_override,
    );
    Some((session_id, plan, turn_session_activity_writer))
}

fn optional_object_str<'a>(
    object: &'a serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<&'a str> {
    object.get(key).and_then(serde_json::Value::as_str)
}

fn optional_object_value(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Option<serde_json::Value> {
    object.get(key).cloned().filter(|value| !value.is_null())
}

fn object_array(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<serde_json::Value> {
    object
        .get(key)
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn object_array_maps(
    object: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Vec<serde_json::Map<String, serde_json::Value>> {
    object
        .get(key)
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_object().cloned())
                .collect()
        })
        .unwrap_or_default()
}

fn first_user_content(messages: &[serde_json::Value]) -> Option<&str> {
    messages.iter().find_map(|message| {
        let message = message.as_object()?;
        if message.get("role").and_then(serde_json::Value::as_str) == Some("user") {
            return optional_object_str(message, "content").filter(|content| !content.is_empty());
        }
        None
    })
}

fn latest_assistant_content(messages: &[serde_json::Value]) -> Option<&str> {
    messages.iter().rev().find_map(|message| {
        let message = message.as_object()?;
        if message.get("role").and_then(serde_json::Value::as_str) == Some("assistant") {
            return optional_object_str(message, "content").filter(|content| !content.is_empty());
        }
        None
    })
}

pub(super) fn build_bridge_response_guard_error_event(
    tail_update_args: &serde_json::Map<String, serde_json::Value>,
    prompt_fingerprints: &[String],
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let tool_calls = object_array(tail_update_args, "tool_calls");
    if !tool_calls.is_empty() {
        return None;
    }

    let full_text = optional_object_str(tail_update_args, "full_text").unwrap_or_default();
    if is_prompt_leaked(full_text, prompt_fingerprints) {
        return Some(build_stream_error_event(
            "Model returned invalid response (prompt leakage). Please retry.",
            "PROMPT_LEAK",
            true,
        ));
    }
    if is_repetition_loop(full_text) {
        return Some(build_stream_error_event(
            "Model returned invalid response (repetition loop). Please retry or switch models.",
            "MODEL_DEGRADED",
            false,
        ));
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(super) fn build_bridge_side_effect_payloads(
    side_effect_user_id: Option<&str>,
    session_id: &str,
    bridge_state: &serde_json::Map<String, serde_json::Value>,
    side_effect_inputs: &serde_json::Map<String, serde_json::Value>,
    tail_update_args: Option<&serde_json::Map<String, serde_json::Value>>,
    streamed_token_usage: Option<&serde_json::Value>,
    trusted_routing_meta: Option<&serde_json::Value>,
    side_effect_request_context: Option<&BridgeSideEffectRequestContext>,
) -> Option<(serde_json::Value, serde_json::Value)> {
    let user_id = side_effect_user_id?;
    let messages = if side_effect_inputs.contains_key("messages") {
        object_array(side_effect_inputs, "messages")
    } else {
        side_effect_request_context
            .map(|context| context.messages.clone())
            .unwrap_or_default()
    };
    let tool_results = if side_effect_inputs.contains_key("tool_results") {
        object_array(side_effect_inputs, "tool_results")
    } else {
        side_effect_request_context
            .map(|context| context.tool_results.clone())
            .unwrap_or_default()
    };
    let cloud_tool_calls = object_array(side_effect_inputs, "cloud_tool_calls");
    let edge_tool_calls = tail_update_args
        .map(|tail| object_array(tail, "tool_calls"))
        .unwrap_or_else(|| object_array(side_effect_inputs, "edge_tool_calls"));
    let all_tool_calls = cloud_tool_calls
        .iter()
        .chain(edge_tool_calls.iter())
        .cloned()
        .collect::<Vec<_>>();
    let cloud_tool_results = object_array(side_effect_inputs, "cloud_tool_results");
    let history = object_array(bridge_state, "history");
    let sections = optional_object_value(side_effect_inputs, "sections")
        .or_else(|| optional_object_value(bridge_state, "sections"));
    let turn_count = bridge_state
        .get("turn_count")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    let session_start = optional_object_value(side_effect_inputs, "session_start")
        .or_else(|| optional_object_value(bridge_state, "created_at"));
    let tool_quality_assessments =
        optional_object_value(side_effect_inputs, "tool_quality_assessments")
            .or_else(|| optional_object_value(bridge_state, "tool_quality_assessments"))
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new()));
    let full_text = tail_update_args
        .and_then(|tail| optional_object_str(tail, "full_text"))
        .or_else(|| optional_object_str(side_effect_inputs, "full_text"))
        .unwrap_or_default();
    let reasoning_content = tail_update_args
        .and_then(|tail| optional_object_str(tail, "reasoning_content"))
        .or_else(|| optional_object_str(side_effect_inputs, "reasoning_content"))
        .unwrap_or_default();
    let mut persist_args = build_persist_thread_args(
        user_id,
        session_id,
        &messages,
        &tool_results,
        full_text,
        &cloud_tool_calls,
        &edge_tool_calls,
        reasoning_content,
        &cloud_tool_results,
        optional_object_str(side_effect_inputs, "context_capture_id"),
        optional_object_str(side_effect_inputs, "model_used"),
        streamed_token_usage
            .cloned()
            .or_else(|| optional_object_value(side_effect_inputs, "token_usage")),
        optional_object_value(side_effect_inputs, "llm_params"),
        &history,
        turn_count,
        optional_object_str(side_effect_inputs, "agent_id").or_else(|| {
            side_effect_request_context.and_then(|context| context.agent_id.as_deref())
        }),
        optional_object_str(bridge_state, "turn_chain_id"),
        optional_object_str(bridge_state, "user_query_event_id"),
        session_start.clone(),
        Some(tool_quality_assessments),
        trusted_routing_meta
            .cloned()
            .or_else(|| optional_object_value(side_effect_inputs, "routing_meta")),
        false,
        false,
        false,
        false,
        false,
        false,
    );
    if let Some(sections) = sections {
        persist_args.insert("sections".to_string(), sections);
    }
    let hook_args = build_turn_hook_args(
        user_id,
        session_id,
        &messages,
        &tool_results,
        full_text,
        &all_tool_calls,
        optional_object_str(side_effect_inputs, "context_capture_id"),
        optional_object_str(side_effect_inputs, "model_used"),
        optional_object_str(side_effect_inputs, "agent_id"),
        optional_object_str(bridge_state, "user_query_event_id"),
        turn_count,
        session_start,
        false,
        false,
        false,
        false,
    );
    Some((
        serde_json::Value::Object(persist_args),
        serde_json::Value::Object(hook_args),
    ))
}

pub(super) fn take_bridge_warning_event(
    bridge_state: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let claims_failed = bridge_state
        .remove("firewall_warning_claims_failed")?
        .as_i64()?;
    Some(build_firewall_warning_event(claims_failed))
}

fn build_explain_event_from_bridge_inputs(
    explain_inputs: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let total_ms = explain_inputs
        .get("total_ms")
        .and_then(serde_json::Value::as_i64)?;
    let prompt_tokens = explain_inputs
        .get("prompt_tokens")
        .and_then(serde_json::Value::as_i64);
    let completion_tokens = explain_inputs
        .get("completion_tokens")
        .and_then(serde_json::Value::as_i64);
    let tools_selected = explain_inputs
        .get("tools_selected")
        .and_then(serde_json::Value::as_u64)? as usize;
    let tools_available = explain_inputs
        .get("tools_available")
        .and_then(serde_json::Value::as_u64)? as usize;
    let tool_selection = optional_object_value(explain_inputs, "tool_selection");
    let steps = object_array(explain_inputs, "steps");
    let memory = optional_object_value(explain_inputs, "memory");
    let routing = optional_object_value(explain_inputs, "routing");
    let auxiliary_llm_calls = explain_inputs
        .get("auxiliary_llm_calls")
        .and_then(serde_json::Value::as_array)
        .cloned();
    Some(build_explain_event(
        total_ms,
        prompt_tokens,
        completion_tokens,
        tools_selected,
        tools_available,
        tool_selection,
        steps,
        memory,
        routing,
        auxiliary_llm_calls,
    ))
}

pub(super) fn take_bridge_explain_event(
    bridge_state: &mut serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    if let Some(explain_inputs) = bridge_state
        .remove("explain_inputs")
        .and_then(|value| value.as_object().cloned())
    {
        return build_explain_event_from_bridge_inputs(&explain_inputs);
    }

    let total_ms = bridge_state.remove("explain_total_ms")?;
    let prompt_tokens = bridge_state.remove("explain_prompt_tokens");
    let completion_tokens = bridge_state.remove("explain_completion_tokens");
    let tools_selected = bridge_state.remove("explain_tools_selected");
    let tools_available = bridge_state.remove("explain_tools_available");
    let tool_selection = bridge_state.remove("explain_tool_selection");
    let steps = bridge_state.remove("explain_steps");
    let memory = bridge_state.remove("explain_memory");
    let routing = bridge_state.remove("explain_routing");
    let auxiliary_llm_calls = bridge_state.remove("explain_auxiliary_llm_calls");

    let mut explain_inputs = serde_json::Map::new();
    explain_inputs.insert("total_ms".to_string(), total_ms);
    if let Some(value) = prompt_tokens {
        explain_inputs.insert("prompt_tokens".to_string(), value);
    }
    if let Some(value) = completion_tokens {
        explain_inputs.insert("completion_tokens".to_string(), value);
    }
    if let Some(value) = tools_selected {
        explain_inputs.insert("tools_selected".to_string(), value);
    }
    if let Some(value) = tools_available {
        explain_inputs.insert("tools_available".to_string(), value);
    }
    if let Some(value) = tool_selection {
        explain_inputs.insert("tool_selection".to_string(), value);
    }
    if let Some(value) = steps {
        explain_inputs.insert("steps".to_string(), value);
    }
    if let Some(value) = memory {
        explain_inputs.insert("memory".to_string(), value);
    }
    if let Some(value) = routing {
        explain_inputs.insert("routing".to_string(), value);
    }
    if let Some(value) = auxiliary_llm_calls {
        explain_inputs.insert("auxiliary_llm_calls".to_string(), value);
    }

    build_explain_event_from_bridge_inputs(&explain_inputs)
}

fn strip_bridge_cache_derived_fields(
    bridge_state: &mut serde_json::Map<String, serde_json::Value>,
) {
    for key in [
        "history",
        "turn_count",
        "tool_sigs",
        "turn_chain_id",
        "user_query_event_id",
        "has_tool_calls",
        "stall_detected",
    ] {
        bridge_state.remove(key);
    }
}

fn serialize_tool_signatures(tool_sigs: &[BTreeSet<String>]) -> serde_json::Value {
    serde_json::Value::Array(
        tool_sigs
            .iter()
            .map(|sig| {
                serde_json::Value::Array(
                    sig.iter().cloned().map(serde_json::Value::String).collect(),
                )
            })
            .collect(),
    )
}

fn apply_bridge_tail_update(
    entry: &serde_json::Map<String, serde_json::Value>,
    tail_update: &serde_json::Map<String, serde_json::Value>,
    trusted_turn_chain_id: Option<&str>,
    trusted_user_query_event_id: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let full_text = tail_update
        .get("full_text")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let tool_calls = tail_update
        .get("tool_calls")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let reasoning_content = tail_update
        .get("reasoning_content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let cloud_loop_history = tail_update
        .get("cloud_loop_history")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .unwrap_or_default();
    let turn_chain_id = trusted_turn_chain_id;
    let user_query_event_id = trusted_user_query_event_id;
    let mut updated_entry = apply_turn_to_session_entry(
        entry,
        full_text,
        &tool_calls,
        reasoning_content,
        &cloud_loop_history,
        turn_chain_id,
        user_query_event_id,
    );
    let mut tool_sigs = bridge_state_tool_signatures(&updated_entry).unwrap_or_default();
    record_server_tool_signatures(&mut tool_sigs, &tool_calls, SERVER_STALL_WINDOW);
    updated_entry.insert(
        "tool_sigs".to_string(),
        serialize_tool_signatures(&tool_sigs),
    );
    updated_entry
}

pub(super) async fn sync_bridge_state_event(
    cache: Arc<tokio::sync::Mutex<SessionCache>>,
    session_id: &str,
    mut bridge_state: serde_json::Map<String, serde_json::Value>,
    tail_update_args: Option<&serde_json::Map<String, serde_json::Value>>,
    trusted_turn_chain_id: Option<&str>,
    trusted_user_query_event_id: Option<&str>,
) -> serde_json::Map<String, serde_json::Value> {
    let now = current_unix_seconds();
    let mut cache = cache.lock().await;
    let mut entry = cache.get(session_id, now).unwrap_or_default();
    if tail_update_args.is_some() {
        strip_bridge_cache_derived_fields(&mut bridge_state);
    }
    for key in [
        "firewall_warning_claims_failed",
        "explain_inputs",
        "explain_total_ms",
        "explain_prompt_tokens",
        "explain_completion_tokens",
        "explain_tools_selected",
        "explain_tools_available",
        "explain_tool_selection",
        "explain_steps",
        "explain_memory",
        "explain_routing",
        "explain_auxiliary_llm_calls",
        "firewall_warning",
        "explain_event",
    ] {
        entry.remove(key);
    }
    entry.extend(bridge_state);
    if let Some(tail_update_args) = tail_update_args {
        entry = apply_bridge_tail_update(
            &entry,
            tail_update_args,
            trusted_turn_chain_id,
            trusted_user_query_event_id,
        );
    }
    cache.insert(session_id.to_string(), entry.clone(), now);
    entry
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper: convert a `serde_json::Value::Object` into its inner `Map`.
    fn to_map(value: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        match value {
            serde_json::Value::Object(m) => m,
            _ => panic!("expected an object"),
        }
    }

    // ──────────────────────────────────────────────────────────
    // take_bridge_prompt_fingerprints
    // ──────────────────────────────────────────────────────────

    #[test]
    fn fingerprints_empty_map() {
        let mut map = serde_json::Map::new();
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert!(result.is_empty());
    }

    #[test]
    fn fingerprints_missing_key() {
        let mut map = to_map(json!({"unrelated": "value"}));
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert!(result.is_empty());
        // key was not removed because it didn't exist; other keys untouched
        assert!(map.contains_key("unrelated"));
    }

    #[test]
    fn fingerprints_non_array_value() {
        let mut map = to_map(json!({"prompt_fingerprints": "not-an-array"}));
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert!(result.is_empty());
        // the key is still consumed (removed) even though it wasn't an array
        assert!(!map.contains_key("prompt_fingerprints"));
    }

    #[test]
    fn fingerprints_null_value() {
        let mut map = to_map(json!({"prompt_fingerprints": null}));
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert!(result.is_empty());
        assert!(!map.contains_key("prompt_fingerprints"));
    }

    #[test]
    fn fingerprints_empty_array() {
        let mut map = to_map(json!({"prompt_fingerprints": []}));
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert!(result.is_empty());
    }

    #[test]
    fn fingerprints_valid_strings() {
        let mut map = to_map(json!({"prompt_fingerprints": ["abc", "def", "ghi"]}));
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert_eq!(result, vec!["abc", "def", "ghi"]);
        assert!(!map.contains_key("prompt_fingerprints"));
    }

    #[test]
    fn fingerprints_skips_non_string_items() {
        let mut map = to_map(json!({"prompt_fingerprints": ["a", 42, null, "b", true]}));
        let result = take_bridge_prompt_fingerprints(&mut map);
        assert_eq!(result, vec!["a", "b"]);
    }

    // ──────────────────────────────────────────────────────────
    // take_bridge_side_effect_inputs
    // ──────────────────────────────────────────────────────────

    #[test]
    fn side_effect_inputs_empty_map() {
        let mut map = serde_json::Map::new();
        let result = take_bridge_side_effect_inputs(&mut map);
        assert!(result.is_none());
    }

    #[test]
    fn side_effect_inputs_direct_object() {
        let inner = json!({"messages": [1,2,3], "full_text": "hello"});
        let mut map = to_map(json!({"side_effect_inputs": inner}));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.get("messages").unwrap(), &json!([1, 2, 3]));
        assert_eq!(result.get("full_text").unwrap(), &json!("hello"));
        assert!(!map.contains_key("side_effect_inputs"));
    }

    #[test]
    fn side_effect_inputs_direct_non_object_returns_none_then_falls_through() {
        // If "side_effect_inputs" is a string, the .as_object() fails,
        // so it falls through to individual-key extraction which also finds nothing.
        let mut map = to_map(json!({"side_effect_inputs": "not-an-object"}));
        let result = take_bridge_side_effect_inputs(&mut map);
        assert!(result.is_none());
    }

    #[test]
    fn side_effect_inputs_individual_keys_single() {
        let mut map = to_map(json!({"side_effect_full_text": "hello world"}));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.get("full_text").unwrap(), &json!("hello world"));
        assert_eq!(result.len(), 1);
        assert!(!map.contains_key("side_effect_full_text"));
    }

    #[test]
    fn side_effect_inputs_individual_keys_multiple() {
        let mut map = to_map(json!({
            "side_effect_messages": [{"role": "user"}],
            "side_effect_model_used": "gpt-4",
            "side_effect_agent_id": "agent-1",
            "side_effect_session_start": true,
        }));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.len(), 4);
        assert_eq!(result.get("messages").unwrap(), &json!([{"role": "user"}]));
        assert_eq!(result.get("model_used").unwrap(), &json!("gpt-4"));
        assert_eq!(result.get("agent_id").unwrap(), &json!("agent-1"));
        assert_eq!(result.get("session_start").unwrap(), &json!(true));
        assert!(map.is_empty());
    }

    #[test]
    fn side_effect_inputs_all_known_keys() {
        let mut map = to_map(json!({
            "side_effect_messages": [],
            "side_effect_tool_results": [],
            "side_effect_full_text": "",
            "side_effect_cloud_tool_calls": [],
            "side_effect_edge_tool_calls": [],
            "side_effect_reasoning_content": "",
            "side_effect_cloud_tool_results": [],
            "side_effect_context_capture_id": "cap-1",
            "side_effect_model_used": "m",
            "side_effect_token_usage": {},
            "side_effect_llm_params": {},
            "side_effect_agent_id": "a",
            "side_effect_routing_meta": {},
            "side_effect_tool_quality_assessments": [],
            "side_effect_sections": [],
            "side_effect_session_start": false,
        }));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.len(), 16);
        assert!(map.is_empty());
    }

    #[test]
    fn side_effect_inputs_ignores_unknown_keys() {
        let mut map = to_map(json!({
            "side_effect_full_text": "hi",
            "some_other_key": "ignored",
        }));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.len(), 1);
        // "some_other_key" remains in bridge_state
        assert!(map.contains_key("some_other_key"));
    }

    #[test]
    fn side_effect_inputs_direct_object_takes_priority_over_individual() {
        let mut map = to_map(json!({
            "side_effect_inputs": {"messages": "from-direct"},
            "side_effect_messages": "from-individual",
        }));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.get("messages").unwrap(), &json!("from-direct"));
        // individual key is NOT consumed when direct object is found
        assert!(map.contains_key("side_effect_messages"));
    }

    #[test]
    fn side_effect_inputs_null_values_are_collected() {
        let mut map = to_map(json!({
            "side_effect_full_text": null,
        }));
        let result = take_bridge_side_effect_inputs(&mut map).unwrap();
        assert_eq!(result.get("full_text").unwrap(), &json!(null));
    }

    // ──────────────────────────────────────────────────────────
    // take_bridge_tail_update_args
    // ──────────────────────────────────────────────────────────

    #[test]
    fn tail_update_args_empty_map() {
        let mut map = serde_json::Map::new();
        let result = take_bridge_tail_update_args(&mut map);
        assert!(result.is_none());
    }

    #[test]
    fn tail_update_args_direct_object() {
        let inner = json!({"full_text": "txt", "tool_calls": []});
        let mut map = to_map(json!({"tail_update_args": inner}));
        let result = take_bridge_tail_update_args(&mut map).unwrap();
        assert_eq!(result.get("full_text").unwrap(), &json!("txt"));
        assert_eq!(result.get("tool_calls").unwrap(), &json!([]));
        assert!(!map.contains_key("tail_update_args"));
    }

    #[test]
    fn tail_update_args_direct_non_object_falls_through() {
        // "tail_update_args" is a string → .as_object() fails, falls through.
        // No "tail_full_text" either → returns None.
        let mut map = to_map(json!({"tail_update_args": "bad"}));
        let result = take_bridge_tail_update_args(&mut map);
        assert!(result.is_none());
    }

    #[test]
    fn tail_update_args_minimal_full_text_only() {
        let mut map = to_map(json!({"tail_full_text": "response text"}));
        let result = take_bridge_tail_update_args(&mut map).unwrap();
        assert_eq!(result.get("full_text").unwrap(), &json!("response text"));
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn tail_update_args_full_text_with_optional_keys() {
        let mut map = to_map(json!({
            "tail_full_text": "text",
            "tail_tool_calls": [{"id": "tc1"}],
            "tail_reasoning_content": "thought",
            "tail_cloud_loop_history": [1, 2],
        }));
        let result = take_bridge_tail_update_args(&mut map).unwrap();
        assert_eq!(result.len(), 4);
        assert_eq!(result.get("full_text").unwrap(), &json!("text"));
        assert_eq!(result.get("tool_calls").unwrap(), &json!([{"id": "tc1"}]));
        assert_eq!(result.get("reasoning_content").unwrap(), &json!("thought"));
        assert_eq!(result.get("cloud_loop_history").unwrap(), &json!([1, 2]));
        assert!(map.is_empty());
    }

    #[test]
    fn tail_update_args_missing_full_text_returns_none() {
        // Has optional keys but no required "tail_full_text"
        let mut map = to_map(json!({
            "tail_tool_calls": [{"id": "tc1"}],
            "tail_reasoning_content": "thought",
        }));
        let result = take_bridge_tail_update_args(&mut map);
        assert!(result.is_none());
    }

    #[test]
    fn tail_update_args_direct_takes_priority() {
        let mut map = to_map(json!({
            "tail_update_args": {"full_text": "direct"},
            "tail_full_text": "individual",
        }));
        let result = take_bridge_tail_update_args(&mut map).unwrap();
        assert_eq!(result.get("full_text").unwrap(), &json!("direct"));
        // individual key is NOT consumed
        assert!(map.contains_key("tail_full_text"));
    }

    #[test]
    fn tail_update_args_null_full_text_is_accepted() {
        let mut map = to_map(json!({"tail_full_text": null}));
        let result = take_bridge_tail_update_args(&mut map).unwrap();
        assert_eq!(result.get("full_text").unwrap(), &json!(null));
    }
}
