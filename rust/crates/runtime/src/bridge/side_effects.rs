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

pub fn run_bridge_hook_side_effects(
    payload: Option<serde_json::Value>,
    turn_hook_db_writer: Arc<dyn TurnHookDbWriter>,
    turn_reflection_state_store: Arc<dyn TurnReflectionStateStore>,
    turn_reflection_lesson_writer: Arc<dyn TurnReflectionLessonWriter>,
    turn_observer_worker: Arc<dyn TurnObserverWorker>,
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
        record_persist_ok();
    });
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
    let tool_calls = object_array_maps(hook_payload, "tool_calls");
    let selected_skills = crate::turn::skill_tool::selected_skill_names_from_tool_calls(
        &tool_calls
            .iter()
            .cloned()
            .map(serde_json::Value::Object)
            .collect::<Vec<_>>(),
    );
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
    let tool_results = object_array_maps(hook_payload, "tool_results");
    let mut tool_pre_state_snapshots = std::collections::HashMap::new();
    let mut tool_pre_state_snapshot_databases = std::collections::HashMap::new();
    let mut tool_execution_outcomes: std::collections::HashMap<
        String,
        astra_turn_core::action_compensation::ExecutionOutcomeClassification,
    > = std::collections::HashMap::new();
    for tool_result in tool_results {
        let Some(tool_call_id) = tool_result
            .get("tool_call_id")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.is_empty())
            .map(ToString::to_string)
        else {
            continue;
        };

        // Classify execution outcome from result content
        let result_text = tool_result
            .get("result")
            .or_else(|| tool_result.get("content"))
            .or_else(|| tool_result.get("error"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let is_err = astra_turn_core::tool_result_semantics::is_tool_error(result_text)
            || tool_result.get("ok").and_then(serde_json::Value::as_bool) == Some(false);
        let error_text = tool_result
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let duration_ms = tool_result
            .get("duration_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let was_rejected = tool_result
            .get("status")
            .and_then(serde_json::Value::as_str)
            == Some("rejected")
            || error_text.starts_with("blocked_tool:")
            || result_text.starts_with("blocked_tool:");
        let classification = astra_turn_core::action_compensation::classify_execution_outcome(
            result_text,
            is_err,
            duration_ms,
            was_rejected,
        );
        tool_execution_outcomes.insert(tool_call_id.clone(), classification);

        if let Some(snapshot_id) = tool_result
            .get("pre_state_snapshot_id")
            .and_then(serde_json::Value::as_str)
            .filter(|snapshot_id| !snapshot_id.is_empty())
        {
            tool_pre_state_snapshots.insert(tool_call_id.clone(), snapshot_id.to_string());
        }
        if let Some(database) = tool_result
            .get("pre_state_snapshot_database")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|database| !database.is_empty())
        {
            tool_pre_state_snapshot_databases.insert(tool_call_id, database.to_string());
        }
    }
    let tool_action_profiles = tool_calls
        .iter()
        .map(|tool_call| {
            let function = tool_call
                .get("function")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            let tool_name = function
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let arguments = function
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
            let profile = crate::tool_action_profile_value(tool_name, &arguments);
            let tool_call_id = tool_call
                .get("id")
                .cloned()
                .unwrap_or(serde_json::Value::String(String::new()));
            let mut action_profile = serde_json::Map::from_iter([
                ("tool_call_id".to_string(), tool_call_id.clone()),
                (
                    "tool_name".to_string(),
                    serde_json::Value::String(tool_name.to_string()),
                ),
                ("arguments".to_string(), arguments),
                ("profile".to_string(), profile),
            ]);
            if let Some(tool_call_id) = tool_call_id.as_str() {
                if let Some(snapshot_id) = tool_pre_state_snapshots.get(tool_call_id) {
                    action_profile.insert(
                        "pre_state_snapshot_id".to_string(),
                        serde_json::Value::String(snapshot_id.clone()),
                    );
                    if let Some(database) = tool_pre_state_snapshot_databases.get(tool_call_id) {
                        action_profile.insert(
                            "pre_state_snapshot_database".to_string(),
                            serde_json::Value::String(database.clone()),
                        );
                    }
                }
            }
            // Attach execution outcome classification when available.
            if let Some(tool_call_id_str) = tool_call_id.as_str() {
                if let Some(outcome) = tool_execution_outcomes.get(tool_call_id_str) {
                    if let Ok(val) = serde_json::to_value(outcome) {
                        action_profile.insert("execution_outcome".to_string(), val);
                    }
                }
            }
            serde_json::Value::Object(action_profile)
        })
        .collect::<Vec<_>>();
    let _turn_number = valid_turn_number(hook_payload.get("turn_count"));
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
            "turn": hook_payload.get("turn_count").cloned(),
            "tool_calls": tool_call_names,
            "action_profiles": tool_action_profiles,
            "model_used": optional_object_str(hook_payload, "model_used"),
        }),
        model_used: optional_object_str(hook_payload, "model_used").map(ToString::to_string),
        context_capture_id: optional_object_str(hook_payload, "context_capture_id")
            .map(ToString::to_string),
    });
    let skill_selection = if let Some(first_skill_name) = selected_skills.first() {
        Some(TurnSkillSelectionRecord {
            event_id: Uuid::now_v7().to_string(),
            session_id: session_id.clone(),
            user_id: user_id.clone(),
            agent_id: optional_object_str(hook_payload, "agent_id").map(ToString::to_string),
            user_query: truncate_text(user_content, 2000),
            selected_skills: selected_skills.clone(),
            skill_name: first_skill_name.to_string(),
            skill_version: None,
            selection_method: "llm_skill_choice".to_string(),
            execution_success: None,
            execution_time_ms: None,
        })
    } else {
        tool_calls
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
                session_id: session_id.clone(),
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
            })
    };
    Some((
        TurnHookDbPersistPlan {
            decision_audit,
            skill_selection,
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

fn valid_turn_number(value: Option<&serde_json::Value>) -> Option<i64> {
    const MAX_TURN_NUMBER: i64 = 1_i64 << 31;
    value
        .and_then(serde_json::Value::as_i64)
        .filter(|turn| (0..MAX_TURN_NUMBER).contains(turn))
}

fn truncate_text(text: &str, limit: usize) -> String {
    let mut chars = text.chars();
    let truncated = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        text.to_string()
    }
}

fn first_user_content(messages: &[serde_json::Value]) -> Option<&str> {
    messages.iter().find_map(|message| {
        if message.get("role").and_then(|v| v.as_str()) == Some("user") {
            message
                .get("content")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        } else {
            None
        }
    })
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

#[cfg(test)]
#[allow(dead_code)]
mod inprocess_hook_contract_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::Mutex;

    use crate::{
        TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
        TurnReflectionLessonRecord, TurnReflectionLessonWriter, TurnReflectionMark,
        TurnReflectionStateStore, build_turn_hook_args,
    };

    use super::{run_bridge_hook_side_effects, truncate_text};

    #[derive(Clone, Default)]
    struct RecordingHookDbWriter {
        plans: Arc<Mutex<Vec<TurnHookDbPersistPlan>>>,
    }

    #[async_trait]
    impl TurnHookDbWriter for RecordingHookDbWriter {
        async fn persist(&self, plan: TurnHookDbPersistPlan) -> Result<(), String> {
            self.plans.lock().await.push(plan);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingReflectionStateStore {
        marks: Arc<Mutex<Vec<TurnReflectionMark>>>,
    }

    #[async_trait]
    impl TurnReflectionStateStore for RecordingReflectionStateStore {
        async fn mark_reflecting(&self, mark: TurnReflectionMark) -> Result<(), String> {
            self.marks.lock().await.push(mark);
            Ok(())
        }
        async fn pop_reflecting(
            &self,
            session_id: &str,
        ) -> Result<Option<TurnReflectionMark>, String> {
            let mut marks = self.marks.lock().await;
            if let Some(i) = marks.iter().position(|m| m.session_id == session_id) {
                Ok(Some(marks.remove(i)))
            } else {
                Ok(None)
            }
        }
    }

    #[derive(Clone, Default)]
    struct RecordingReflectionLessonWriter {
        lessons: Arc<Mutex<Vec<TurnReflectionLessonRecord>>>,
    }

    #[async_trait]
    impl TurnReflectionLessonWriter for RecordingReflectionLessonWriter {
        async fn persist_lesson(&self, lesson: TurnReflectionLessonRecord) -> Result<(), String> {
            self.lessons.lock().await.push(lesson);
            Ok(())
        }
    }

    #[derive(Clone, Default)]
    struct RecordingObserverWorker {
        requests: Arc<Mutex<Vec<TurnObserverRequest>>>,
    }

    #[async_trait]
    impl TurnObserverWorker for RecordingObserverWorker {
        async fn run(&self, request: TurnObserverRequest) -> Result<(), String> {
            self.requests.lock().await.push(request);
            Ok(())
        }
    }

    fn build_hook_payload_with_tool_call() -> Value {
        let messages = vec![json!({"role": "user", "content": "list files in src/"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-1",
            "name": "bash",
            "result": "src/lib.rs"
        })];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{\"command\": \"ls src/\"}"}
        })];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Let me list the files for you.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-1"),
            1,
            None,
            false,
            true,
            true,
        ))
    }

    fn build_hook_payload_with_skill_metric() -> Value {
        let messages = vec![json!({"role": "user", "content": "deploy the service"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-skill-1",
            "name": "skill",
            "result": "Loaded deploy skill."
        })];
        let tool_calls = vec![json!({
            "id": "call-skill-1",
            "function": {"name": "skill", "arguments": "{\"skill_name\": \"deploy-beta\"}"}
        })];
        let mut payload = build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Using the deployment skill.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-skill"),
            5,
            None,
            false,
            true,
            true,
        );
        payload.insert(
            "skill_selector_metric".to_string(),
            json!({
                "event_id": "metric-1",
                "session_id": "session-1",
                "user_id": "user-1",
                "turn_number": 5,
                "visible_skill_count": 4,
                "chosen_skill_count": 1,
                "shortlisted_chosen_count": 1,
                "missed_chosen_count": 0,
                "best_chosen_rank": 2,
                "selector_tier": "embedding",
                "elapsed_ms": 12,
                "total_catalog_size": 4
            }),
        );
        Value::Object(payload)
    }

    fn build_hook_payload_with_derived_skill_metric() -> Value {
        let section = crate::prompts::build_skill_listing_section(&[
            crate::turn::skill_tool::SkillToolInfo {
                name: "inspect".into(),
                description: "inspect cluster".into(),
                ..Default::default()
            },
            crate::turn::skill_tool::SkillToolInfo {
                name: "deploy".into(),
                description: "deploy service".into(),
                aliases: vec!["ship-it".into()],
                ..Default::default()
            },
        ])
        .expect("skill listing section");
        let messages = vec![
            json!({"role": "system", "content": section.text}),
            json!({"role": "user", "content": "deploy the service"}),
        ];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-skill-2",
            "name": "skill",
            "result": "Loaded deploy skill."
        })];
        let tool_calls = vec![json!({
            "id": "call-skill-2",
            "function": {"name": "skill", "arguments": "{\"skill_name\": \"ship-it\"}"}
        })];
        let mut payload = build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Using the deployment skill.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-skill-derived"),
            6,
            None,
            false,
            true,
            true,
        );
        payload.insert(
            "skill_selector_shortlist".to_string(),
            json!({
                "open_catalog": true,
                "visible_skill_count": 2,
                "skills": [
                    {
                        "rank": 1,
                        "skill_name": "inspect",
                        "aliases": [],
                        "description": "inspect cluster",
                        "source": "test",
                        "category": null
                    },
                    {
                        "rank": 2,
                        "skill_name": "deploy",
                        "aliases": ["ship-it"],
                        "description": "deploy service",
                        "source": "test",
                        "category": null
                    }
                ],
                "telemetry": {
                    "selector_tier": "lexical",
                    "elapsed_ms": 1,
                    "total_catalog_size": 2
                }
            }),
        );
        Value::Object(payload)
    }

    fn build_hook_payload_text_only() -> Value {
        let messages = vec![json!({"role": "user", "content": "what is Rust?"})];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &[],
            "Rust is a systems programming language.",
            &[],
            None,
            Some("gpt-4"),
            None,
            Some("evt-query-2"),
            2,
            None,
            false,
            true,
            true,
        ))
    }

    fn build_hook_payload_with_mo_query_snapshot() -> Value {
        let messages = vec![json!({"role": "user", "content": "update the database"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-1",
            "name": "mo",
            "result": "OK (no results)",
            "pre_state_snapshot_id": "moq_snap_123",
            "pre_state_snapshot_database": "analytics"
        })];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {
                "name": "mo",
                "arguments": "{\"sql\": \"UPDATE metrics SET value = 1\", \"database\": \"analytics\"}"
            }
        })];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Updated the database row.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-snapshot"),
            4,
            None,
            false,
            true,
            true,
        ))
    }

    fn build_hook_payload_with_blocked_tool_result(turn: i64) -> Value {
        let messages = vec![json!({"role": "user", "content": "run the blocked command"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-1",
            "name": "bash",
            "ok": false,
            "error": "blocked_tool: Explicit approval required: action scope is unbounded."
        })];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{\"command\": \"rm -rf tmp\"}"}
        })];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "The command was blocked.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-blocked"),
            turn,
            None,
            false,
            true,
            true,
        ))
    }

    #[tokio::test]
    async fn hook_persists_decision_audit_and_skill_selection_for_tool_calls() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_tool_call()),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        assert_eq!(plans.len(), 1, "should persist exactly one hook plan");

        let plan = &plans[0];
        let audit = plan
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert_eq!(audit.session_id, "session-1");
        assert_eq!(audit.event_id, "evt-query-1");
        assert_eq!(audit.decision_type, "tool_selection");
        assert_eq!(audit.model_used.as_deref(), Some("gpt-4"));
        assert_eq!(
            audit.decision_output["action_profiles"][0]["tool_name"],
            "bash"
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["tool_call_id"],
            "call-1"
        );
        assert_eq!(audit.decision_output["turn"], 1);
        assert_eq!(
            audit.decision_output["action_profiles"][0]["arguments"],
            json!("{\"command\": \"ls src/\"}")
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["profile"]["category"],
            "read"
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["profile"]["bounded"],
            false
        );

        let selection = plan
            .skill_selection
            .as_ref()
            .expect("skill_selection missing");
        assert_eq!(selection.session_id, "session-1");
        assert_eq!(selection.skill_name, "bash");
        assert_eq!(selection.selected_skills, vec!["bash"]);
        assert_eq!(selection.selection_method, "llm_tool_choice");
    }

    #[tokio::test]
    async fn hook_persists_response_generation_audit_without_skill_selection() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_text_only()),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        assert_eq!(plans.len(), 1);

        let plan = &plans[0];
        let audit = plan
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert_eq!(audit.decision_type, "response_generation");
        assert!(
            plan.skill_selection.is_none(),
            "text-only turn should not produce skill_selection"
        );
    }

    #[tokio::test]
    async fn hook_persists_skill_selector_metric_and_skill_names() {
        let hook_writer = RecordingHookDbWriter::default();
        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_skill_metric()),
            Arc::new(hook_writer.clone()),
            Arc::new(RecordingReflectionStateStore::default()),
            Arc::new(RecordingReflectionLessonWriter::default()),
            Arc::new(RecordingObserverWorker::default()),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let plan = &plans[0];
        let selection = plan
            .skill_selection
            .as_ref()
            .expect("skill selection missing");
        assert_eq!(selection.skill_name, "deploy-beta");
        assert_eq!(selection.selected_skills, vec!["deploy-beta"]);
        assert_eq!(selection.selection_method, "llm_skill_choice");
    }

    #[test]
    fn truncate_text_preserves_utf8_and_marks_truncation() {
        let input = "你好🚀".repeat(1000);
        let truncated = truncate_text(&input, 5);
        assert_eq!(truncated, "你好🚀你好…");
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    #[tokio::test]
    async fn hook_marks_blocked_tool_results_as_rejected_execution_outcomes() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_blocked_tool_result(7)),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let audit = plans[0]
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert_eq!(
            audit.decision_output["action_profiles"][0]["execution_outcome"]["outcome"],
            json!("rejected")
        );
    }

    #[tokio::test]
    async fn hook_marks_reflection_state_when_reflect_tool_called() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        let messages = vec![json!({"role": "user", "content": "reflect on our session"})];
        let tool_calls = vec![json!({
            "function": {"name": "reflect", "arguments": "{}"}
        })];
        let tool_results = vec![json!({
            "name": "reflect",
            "result": "Session analysis: good progress on refactoring."
        })];
        let payload = Value::Object(build_turn_hook_args(
            "user-1",
            "session-reflect",
            &messages,
            &tool_results,
            "",
            &tool_calls,
            None,
            Some("gpt-4"),
            None,
            Some("evt-reflect"),
            5,
            None,
            false,
            true,
            false,
        ));

        run_bridge_hook_side_effects(
            Some(payload),
            Arc::new(hook_writer),
            Arc::new(reflection_store.clone()),
            Arc::new(lesson_writer),
            Arc::new(observer),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let marks = reflection_store.marks.lock().await;
        assert_eq!(marks.len(), 1, "should mark reflection state");
        assert_eq!(marks[0].session_id, "session-reflect");
        assert!(
            marks[0]
                .reflect_output
                .contains("good progress on refactoring")
        );
    }

    #[tokio::test]
    async fn hook_noop_on_none_payload() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            None,
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(hook_writer.plans.lock().await.is_empty());
    }

    #[tokio::test]
    async fn hook_records_response_audit_without_skill_selection_on_retry_signal() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        let messages = vec![
            json!({"role": "assistant", "content": "The answer is 42."}),
            json!({"role": "user", "content": "that's wrong, try again"}),
        ];
        let payload = Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &[],
            "Let me reconsider...",
            &[],
            None,
            Some("gpt-4"),
            None,
            Some("evt-retry"),
            3,
            None,
            false,
            true,
            true,
        ));

        run_bridge_hook_side_effects(
            Some(payload),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        assert_eq!(plans.len(), 1);
        assert!(plans[0].decision_audit.is_some());
        assert!(plans[0].skill_selection.is_none());
    }
}
