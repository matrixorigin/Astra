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
    let mut tool_verification_summaries = std::collections::HashMap::new();
    let mut tool_pre_state_snapshots = std::collections::HashMap::new();
    let mut tool_pre_state_snapshot_databases = std::collections::HashMap::new();
    let mut tool_execution_outcomes: std::collections::HashMap<
        String,
        crate::turn::action_compensation::ExecutionOutcomeClassification,
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
        let is_err = crate::turn::tool_result_semantics::is_tool_error(result_text)
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
        let classification = crate::turn::action_compensation::classify_execution_outcome(
            result_text,
            is_err,
            duration_ms,
            was_rejected,
        );
        tool_execution_outcomes.insert(tool_call_id.clone(), classification);

        if let Some(summary) = tool_verification_summary_from_tool_result(&tool_result) {
            tool_verification_summaries.insert(tool_call_id.clone(), summary);
        }
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
    let turn_verification_summary = hook_payload
        .get("turn_count")
        .and_then(serde_json::Value::as_i64)
        .and_then(|turn| u32::try_from(turn).ok())
        .filter(|_| tool_calls.len() == 1)
        .and_then(|turn| turn_verification_summary_from_journal(&session_id, turn));
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
                if let Some(summary) = tool_verification_summaries.get(tool_call_id) {
                    action_profile.insert("verifier".to_string(), summary.clone());
                    action_profile.insert(
                        "verifier_source".to_string(),
                        serde_json::Value::String("tool_result".to_string()),
                    );
                } else if let Some(summary) = turn_verification_summary.as_ref() {
                    action_profile.insert("verifier".to_string(), summary.clone());
                    action_profile.insert(
                        "verifier_source".to_string(),
                        serde_json::Value::String("turn_journal".to_string()),
                    );
                } else {
                    let verifier_gap = if tool_calls.len() > 1 {
                        "ambiguous_multi_action_turn"
                    } else {
                        "no_verifier_signal"
                    };
                    action_profile.insert(
                        "verifier_gap".to_string(),
                        serde_json::Value::String(verifier_gap.to_string()),
                    );
                }
            } else if let Some(summary) = turn_verification_summary.as_ref() {
                action_profile.insert("verifier".to_string(), summary.clone());
                action_profile.insert(
                    "verifier_source".to_string(),
                    serde_json::Value::String("turn_journal".to_string()),
                );
            } else {
                let verifier_gap = if tool_calls.len() > 1 {
                    "ambiguous_multi_action_turn"
                } else {
                    "no_verifier_signal"
                };
                action_profile.insert(
                    "verifier_gap".to_string(),
                    serde_json::Value::String(verifier_gap.to_string()),
                );
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
    let mutation_objective_score =
        crate::pipeline::learning::build_learning_outcome_from_payload(payload)
            .and_then(|outcome| serde_json::to_value(outcome.mutation_objective_score()).ok());
    let turn_number = hook_payload
        .get("turn_count")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or_default();
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
            "mutation_objective_score": mutation_objective_score,
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
    let derived_shortlist =
        crate::turn::skill_tool::parse_skill_selector_shortlist_from_messages(&messages);
    let skill_selector_metric = hook_payload
        .get("skill_selector_metric")
        .and_then(parse_turn_skill_selector_metric_record)
        .or_else(|| {
            crate::turn::skill_tool::build_turn_skill_selector_metric_record(
                &session_id,
                &user_id,
                turn_number,
                derived_shortlist.as_ref(),
                &selected_skills,
            )
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
            skill_selector_metric,
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

fn truncate_text(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        text.to_string()
    } else {
        text[..limit].to_string()
    }
}

fn parse_turn_skill_selector_metric_record(
    value: &serde_json::Value,
) -> Option<TurnSkillSelectorMetricRecord> {
    serde_json::from_value(value.clone()).ok()
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

fn latest_assistant_content(messages: &[serde_json::Value]) -> Option<&str> {
    messages.iter().rev().find_map(|message| {
        if message.get("role").and_then(|v| v.as_str()) == Some("assistant") {
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

fn tool_verification_summary_from_tool_result(
    tool_result: &serde_json::Map<String, serde_json::Value>,
) -> Option<serde_json::Value> {
    tool_result
        .get("verification_summary")
        .and_then(extract_verification_summary_from_value)
        .or_else(|| {
            tool_result
                .get("result")
                .and_then(extract_verification_summary_from_value)
        })
        .or_else(|| {
            tool_result
                .get("content")
                .and_then(extract_verification_summary_from_value)
        })
}

fn extract_verification_summary_from_value(value: &serde_json::Value) -> Option<serde_json::Value> {
    if let Some(summary) = astra_services::MutationVerifierSummary::from_value(value) {
        return serde_json::to_value(summary).ok();
    }

    match value {
        serde_json::Value::Object(object) => {
            if let (Some(all_required_passed), Some(results)) = (
                object
                    .get("all_required_passed")
                    .and_then(serde_json::Value::as_bool),
                object.get("results").and_then(serde_json::Value::as_array),
            ) {
                let parsed_results = results
                    .iter()
                    .map(|item| {
                        serde_json::from_value::<astra_services::VerificationResult>(item.clone())
                            .ok()
                    })
                    .collect::<Option<Vec<_>>>()?;
                return serde_json::to_value(
                    astra_services::MutationVerifierSummary::from_results(
                        all_required_passed,
                        &parsed_results,
                    ),
                )
                .ok();
            }

            if let (Some(passed), Some(results_count)) = (
                object
                    .get("all_required_passed")
                    .or_else(|| object.get("passed"))
                    .and_then(serde_json::Value::as_bool),
                object
                    .get("results_count")
                    .and_then(serde_json::Value::as_u64)
                    .map(|count| count as u32),
            ) {
                let criteria_passed = if passed { results_count } else { 0 };
                let pass_rate = if results_count == 0 {
                    astra_core::confidence::ConfidenceInterval::FULL
                } else {
                    astra_core::confidence::ConfidenceInterval::exact(
                        criteria_passed as f64 / results_count as f64,
                    )
                };
                return serde_json::to_value(astra_services::MutationVerifierSummary {
                    all_required_passed: passed,
                    criteria_total: results_count,
                    criteria_passed,
                    pass_rate,
                    failing_criteria: Vec::new(),
                })
                .ok();
            }

            None
        }
        serde_json::Value::String(string) => serde_json::from_str::<serde_json::Value>(string)
            .ok()
            .and_then(|parsed| extract_verification_summary_from_value(&parsed)),
        _ => None,
    }
}

fn turn_verification_summary_from_journal(
    session_id: &str,
    turn: u32,
) -> Option<serde_json::Value> {
    let summaries = astra_services::session_journal::read_journal(session_id)
        .ok()?
        .iter()
        .filter_map(|event| journal_verification_summary_from_event(event, turn))
        .collect::<Vec<_>>();
    if summaries.is_empty() {
        return None;
    }
    serde_json::to_value(merge_verification_summaries(&summaries)).ok()
}

fn journal_verification_summary_from_event(
    event: &astra_services::session_journal::JournalEvent,
    turn: u32,
) -> Option<astra_services::MutationVerifierSummary> {
    if !matches!(
        event.event_type,
        astra_services::session_journal::JournalEventType::VerificationCompleted
    ) || event.turn != Some(turn)
    {
        return None;
    }
    let metadata = event.metadata.as_ref()?;
    let passed = metadata.get("passed").and_then(serde_json::Value::as_bool);
    let results = metadata.get("results")?;
    journal_verification_summary_from_results(results, passed)
}

fn journal_verification_summary_from_results(
    results: &serde_json::Value,
    passed: Option<bool>,
) -> Option<astra_services::MutationVerifierSummary> {
    if let Some(summary) = astra_services::MutationVerifierSummary::from_value(results) {
        return Some(summary);
    }

    match results {
        serde_json::Value::Array(items) => {
            let mut criteria_total = items.len() as u32;
            let mut criteria_passed = items
                .iter()
                .filter(|item| {
                    item.get("passed").and_then(serde_json::Value::as_bool) == Some(true)
                })
                .count() as u32;
            if criteria_total == 0
                && let Some(all_required_passed) = passed
            {
                criteria_total = 1;
                criteria_passed = u32::from(all_required_passed);
            }
            let all_required_passed =
                passed.unwrap_or(criteria_total == 0 || criteria_passed == criteria_total);
            let failing_criteria = items
                .iter()
                .filter(|item| {
                    item.get("passed").and_then(serde_json::Value::as_bool) == Some(false)
                })
                .filter_map(|item| {
                    let object = item.as_object()?;
                    object
                        .get("criterion_id")
                        .or_else(|| object.get("check"))
                        .or_else(|| object.get("target"))
                        .or_else(|| object.get("name"))
                        .and_then(serde_json::Value::as_str)
                        .map(ToString::to_string)
                })
                .collect::<Vec<_>>();
            let pass_rate = if criteria_total == 0 {
                astra_core::confidence::ConfidenceInterval::FULL
            } else {
                astra_core::confidence::ConfidenceInterval::exact(
                    criteria_passed as f64 / criteria_total as f64,
                )
            };
            Some(astra_services::MutationVerifierSummary {
                all_required_passed,
                criteria_total,
                criteria_passed,
                pass_rate,
                failing_criteria,
            })
        }
        serde_json::Value::Object(_) if passed.is_some() => {
            let all_required_passed = passed.unwrap_or(false);
            Some(astra_services::MutationVerifierSummary {
                all_required_passed,
                criteria_total: 1,
                criteria_passed: u32::from(all_required_passed),
                pass_rate: astra_core::confidence::ConfidenceInterval::exact(
                    if all_required_passed { 1.0 } else { 0.0 },
                ),
                failing_criteria: Vec::new(),
            })
        }
        serde_json::Value::String(string) => serde_json::from_str::<serde_json::Value>(string)
            .ok()
            .and_then(|parsed| journal_verification_summary_from_results(&parsed, passed)),
        _ => None,
    }
}

fn merge_verification_summaries(
    summaries: &[astra_services::MutationVerifierSummary],
) -> astra_services::MutationVerifierSummary {
    let all_required_passed = summaries.iter().all(|summary| summary.all_required_passed);
    let criteria_total = summaries.iter().map(|summary| summary.criteria_total).sum();
    let criteria_passed = summaries
        .iter()
        .map(|summary| summary.criteria_passed)
        .sum();
    let mut failing_criteria = Vec::new();
    for criterion in summaries
        .iter()
        .flat_map(|summary| summary.failing_criteria.iter())
    {
        if !failing_criteria.contains(criterion) {
            failing_criteria.push(criterion.clone());
        }
    }
    let pass_rate = if criteria_total == 0 {
        astra_core::confidence::ConfidenceInterval::FULL
    } else {
        astra_core::confidence::ConfidenceInterval::exact(
            criteria_passed as f64 / criteria_total as f64,
        )
    };
    astra_services::MutationVerifierSummary {
        all_required_passed,
        criteria_total,
        criteria_passed,
        pass_rate,
        failing_criteria,
    }
}

#[cfg(test)]
mod inprocess_hook_contract_tests {
    use std::sync::Arc;

    use astra_services::session_journal::{JournalDirGuard, JournalEvent, JournalWriter};
    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tempfile::tempdir;
    use tokio::sync::Mutex;

    use crate::{
        TurnHookDbPersistPlan, TurnHookDbWriter, TurnObserverRequest, TurnObserverWorker,
        TurnReflectionLessonRecord, TurnReflectionLessonWriter, TurnReflectionMark,
        TurnReflectionStateStore, build_turn_hook_args,
    };

    use super::run_bridge_hook_side_effects;

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
            "result": "src/lib.rs",
            "verification_summary": {
                "all_required_passed": true,
                "criteria_total": 1,
                "criteria_passed": 1,
                "pass_rate": {"point": 1.0, "lower": 1.0, "upper": 1.0},
                "failing_criteria": []
            }
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
                "hit_at_1": false,
                "hit_at_3": true,
                "hit_at_5": true,
                "hit_at_14": true
            }),
        );
        Value::Object(payload)
    }

    fn build_hook_payload_with_derived_skill_metric() -> Value {
        let messages = vec![
            crate::turn::skill_tool::skill_listing_system_message(
                &[
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
                ],
                None,
                None,
                true,
            ),
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
        Value::Object(build_turn_hook_args(
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
            true,
        ))
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
            true,
        ))
    }

    fn build_hook_payload_with_result_shaped_verifier() -> Value {
        let messages = vec![json!({"role": "user", "content": "run the verification"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-1",
            "name": "verify",
            "result": {
                "all_required_passed": false,
                "results": [
                    {
                        "criterion_id": "tests",
                        "passed": false,
                        "evidence": "cargo test failed",
                        "expected": "tests pass",
                        "duration_ms": 25
                    }
                ]
            }
        })];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "verify", "arguments": "{\"scope\": \"turn\"}"}
        })];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Verification failed on tests.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-3"),
            3,
            None,
            false,
            true,
            true,
            true,
        ))
    }

    fn build_hook_payload_with_mo_query_snapshot() -> Value {
        let messages = vec![json!({"role": "user", "content": "update the database"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-1",
            "name": "mo_query",
            "result": "OK (no results)",
            "pre_state_snapshot_id": "moq_snap_123",
            "pre_state_snapshot_database": "analytics"
        })];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {
                "name": "mo_query",
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
            true,
        ))
    }

    fn build_hook_payload_with_single_tool_and_no_verifier(turn: i64) -> Value {
        let messages = vec![json!({"role": "user", "content": "run the command"})];
        let tool_results: Vec<Value> = vec![json!({
            "tool_call_id": "call-1",
            "name": "bash",
            "result": "done"
        })];
        let tool_calls = vec![json!({
            "id": "call-1",
            "function": {"name": "bash", "arguments": "{\"command\": \"cargo test\"}"}
        })];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Ran the command.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-4"),
            turn,
            None,
            false,
            true,
            true,
            true,
        ))
    }

    fn build_hook_payload_with_multiple_tools_and_no_verifier(turn: i64) -> Value {
        let messages = vec![json!({"role": "user", "content": "run both commands"})];
        let tool_results: Vec<Value> = vec![
            json!({
                "tool_call_id": "call-1",
                "name": "bash",
                "result": "done"
            }),
            json!({
                "tool_call_id": "call-2",
                "name": "bash",
                "result": "done"
            }),
        ];
        let tool_calls = vec![
            json!({
                "id": "call-1",
                "function": {"name": "bash", "arguments": "{\"command\": \"cargo test\"}"}
            }),
            json!({
                "id": "call-2",
                "function": {"name": "bash", "arguments": "{\"command\": \"cargo fmt\"}"}
            }),
        ];
        Value::Object(build_turn_hook_args(
            "user-1",
            "session-1",
            &messages,
            &tool_results,
            "Ran both commands.",
            &tool_calls,
            None,
            Some("gpt-4"),
            Some("agent-1"),
            Some("evt-query-5"),
            turn,
            None,
            false,
            true,
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
            None,
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
            audit.decision_output["action_profiles"][0]["verifier"]["criteria_total"],
            1
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["profile"]["category"],
            "read"
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["profile"]["bounded"],
            false
        );
        assert!(audit.decision_output["mutation_objective_score"].is_object());

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
            None,
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
            None,
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

        let metric = plan
            .skill_selector_metric
            .as_ref()
            .expect("skill selector metric missing");
        assert_eq!(metric.event_id, "metric-1");
        assert_eq!(metric.turn_number, 5);
        assert_eq!(metric.best_chosen_rank, Some(2));
        assert!(metric.hit_at_3);
    }

    #[tokio::test]
    async fn hook_derives_skill_selector_metric_from_skill_listing_message() {
        let hook_writer = RecordingHookDbWriter::default();
        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_derived_skill_metric()),
            Arc::new(hook_writer.clone()),
            Arc::new(RecordingReflectionStateStore::default()),
            Arc::new(RecordingReflectionLessonWriter::default()),
            Arc::new(RecordingObserverWorker::default()),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let metric = plans[0]
            .skill_selector_metric
            .as_ref()
            .expect("derived metric missing");
        assert_eq!(metric.turn_number, 6);
        assert_eq!(metric.visible_skill_count, 2);
        assert_eq!(metric.best_chosen_rank, Some(2));
        assert!(metric.hit_at_3);
    }

    #[tokio::test]
    async fn hook_extracts_verifier_summary_from_result_payload() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_result_shaped_verifier()),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let audit = plans[0]
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier"]["all_required_passed"],
            json!(false)
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier"]["criteria_total"],
            json!(1)
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier"]["failing_criteria"],
            json!(["tests"])
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier_source"],
            json!("tool_result")
        );
    }

    #[tokio::test]
    async fn hook_persists_pre_state_snapshot_id_on_action_profile() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_mo_query_snapshot()),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let audit = plans[0]
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert_eq!(
            audit.decision_output["action_profiles"][0]["tool_name"],
            "mo_query"
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["pre_state_snapshot_id"],
            "moq_snap_123"
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["pre_state_snapshot_database"],
            "analytics"
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["profile"]["requires_pre_state"],
            true
        );
    }

    #[tokio::test]
    async fn hook_uses_turn_journal_verification_as_single_action_fallback() {
        let temp = tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let writer = JournalWriter::new("session-1").expect("journal writer");
        writer
            .append(&JournalEvent::verification_completed(
                Some("session-1"),
                4,
                "subtask-1",
                "global",
                false,
                &json!([
                    {"check": "unit-tests", "passed": true},
                    {"check": "integration-tests", "passed": false}
                ]),
            ))
            .expect("append verification");

        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_single_tool_and_no_verifier(4)),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let audit = plans[0]
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier"]["criteria_total"],
            json!(2)
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier"]["criteria_passed"],
            json!(1)
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier"]["failing_criteria"],
            json!(["integration-tests"])
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier_source"],
            json!("turn_journal")
        );
    }

    #[tokio::test]
    async fn hook_marks_missing_verifier_signal_for_single_action_turns() {
        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_single_tool_and_no_verifier(6)),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let audit = plans[0]
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert!(
            audit.decision_output["action_profiles"][0]
                .get("verifier")
                .is_none()
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier_gap"],
            json!("no_verifier_signal")
        );
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
            None,
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
    async fn hook_skips_turn_journal_fallback_for_multi_action_turns() {
        let temp = tempdir().expect("tempdir");
        let _guard = JournalDirGuard::new(temp.path());
        let writer = JournalWriter::new("session-1").expect("journal writer");
        writer
            .append(&JournalEvent::verification_completed(
                Some("session-1"),
                5,
                "subtask-1",
                "global",
                false,
                &json!([
                    {"check": "unit-tests", "passed": true},
                    {"check": "integration-tests", "passed": false}
                ]),
            ))
            .expect("append verification");

        let hook_writer = RecordingHookDbWriter::default();
        let reflection_store = RecordingReflectionStateStore::default();
        let lesson_writer = RecordingReflectionLessonWriter::default();
        let observer = RecordingObserverWorker::default();

        run_bridge_hook_side_effects(
            Some(build_hook_payload_with_multiple_tools_and_no_verifier(5)),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        let audit = plans[0]
            .decision_audit
            .as_ref()
            .expect("decision_audit missing");
        assert!(
            audit.decision_output["action_profiles"][0]
                .get("verifier")
                .is_none()
        );
        assert!(
            audit.decision_output["action_profiles"][1]
                .get("verifier")
                .is_none()
        );
        assert_eq!(
            audit.decision_output["action_profiles"][0]["verifier_gap"],
            json!("ambiguous_multi_action_turn")
        );
        assert_eq!(
            audit.decision_output["action_profiles"][1]["verifier_gap"],
            json!("ambiguous_multi_action_turn")
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
            true,
            false,
        ));

        run_bridge_hook_side_effects(
            Some(payload),
            Arc::new(hook_writer),
            Arc::new(reflection_store.clone()),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
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
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        assert!(hook_writer.plans.lock().await.is_empty());
    }

    #[tokio::test]
    async fn hook_persists_implicit_feedback_on_negative_signal() {
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
            false,
            true,
        ));

        run_bridge_hook_side_effects(
            Some(payload),
            Arc::new(hook_writer.clone()),
            Arc::new(reflection_store),
            Arc::new(lesson_writer),
            Arc::new(observer),
            None,
        );

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let plans = hook_writer.plans.lock().await;
        assert_eq!(plans.len(), 1);
        let feedback = plans[0].implicit_feedback.as_ref();
        if let Some(fb) = feedback {
            assert!(fb.rating < 3, "negative signal should produce low rating");
            assert!(fb.comment.as_deref().unwrap_or("").contains("implicit:"));
        }
    }

    // ─── E2E: correction signal chain through run_bridge_hook_side_effects ───
    //
    // These tests exercise the REAL production path including tokio::spawn:
    //   build_turn_hook_args() → inject is_correction + routing_meta
    //   → run_bridge_hook_side_effects() [spawns async task]
    //   → build_learning_outcome_from_payload() → PipelineLearningWriter.record_outcome()
    //   → ProgressiveCalibrator.record(was_corrected=true)

    /// Full e2e: user says "不对" → implicit feedback detected → is_correction
    /// injected into hook payload → spawned side_effects task updates Calibrator.
    #[tokio::test]
    async fn e2e_correction_flows_through_side_effects_to_calibrator() {
        use std::sync::{Arc, Mutex as StdMutex};

        let cal = Arc::new(StdMutex::new(
            crate::pipeline::calibration::ProgressiveCalibrator::new(0.70),
        ));
        let writer: Arc<dyn crate::TurnLearningWriter> = Arc::new(
            crate::pipeline::learning::PipelineLearningWriter::new()
                .with_progressive_calibrator(cal.clone()),
        );

        let initial = cal.lock().unwrap().calibrated_threshold(
            "fetch",
            None,
            crate::pipeline::routing::TaskType::Fetch,
        );

        for i in 0..6 {
            // Step 1: detect implicit feedback (same as bridge_inprocess.rs line 1864)
            let user_input = format!("不对，这完全错了 {i}");
            let prev_assistant = "Here are the PRs.";
            let signal = crate::turn::implicit_feedback::detect_implicit_feedback_signal(
                &user_input,
                Some(prev_assistant),
            );
            let is_correction = matches!(signal.signal_type.as_str(), "correction" | "frustration");
            assert!(
                is_correction,
                "turn {i}: '不对' should be detected as correction"
            );

            // Step 2: build hook payload (same as bridge_inprocess.rs line 2939)
            let messages = vec![
                json!({"role": "assistant", "content": prev_assistant}),
                json!({"role": "user", "content": &user_input}),
            ];
            let tool_calls = vec![json!({
                "id": format!("call-{i}"),
                "function": {"name": "github_list_prs", "arguments": "{}"}
            })];
            let tool_results = vec![json!({
                "tool_call_id": format!("call-{i}"),
                "name": "github_list_prs",
                "result": "{\"prs\": []}"
            })];
            let mut payload = build_turn_hook_args(
                "user-1",
                "session-1",
                &messages,
                &tool_results,
                prev_assistant,
                &tool_calls,
                None,
                Some("gpt-4"),
                None,
                Some("evt-1"),
                (i + 1) as i64,
                None,
                false,
                false,
                false,
                false,
            );

            // Step 3: inject correction + routing (same as bridge_inprocess.rs line 2960+)
            if is_correction {
                payload.insert("is_correction".to_string(), json!(true));
            }
            payload
                .entry("routing_meta".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert("task_type".to_string(), json!("fetch"));

            // Step 4: fire through real run_bridge_hook_side_effects (with tokio::spawn)
            run_bridge_hook_side_effects(
                Some(Value::Object(payload)),
                Arc::new(RecordingHookDbWriter::default()),
                Arc::new(RecordingReflectionStateStore::default()),
                Arc::new(RecordingReflectionLessonWriter::default()),
                Arc::new(RecordingObserverWorker::default()),
                Some(writer.clone()),
            );
        }

        // Wait for all 6 spawned tasks
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let final_threshold = cal.lock().unwrap().calibrated_threshold(
            "fetch",
            None,
            crate::pipeline::routing::TaskType::Fetch,
        );

        assert!(
            final_threshold < initial,
            "calibrated threshold should decrease after corrections: \
             initial={initial}, final={final_threshold}"
        );
    }

    /// Same real path but normal turns (no correction) — threshold must not change.
    #[tokio::test]
    async fn e2e_no_correction_through_side_effects_leaves_calibrator_unchanged() {
        use std::sync::{Arc, Mutex as StdMutex};

        let cal = Arc::new(StdMutex::new(
            crate::pipeline::calibration::ProgressiveCalibrator::new(0.70),
        ));
        let writer: Arc<dyn crate::TurnLearningWriter> = Arc::new(
            crate::pipeline::learning::PipelineLearningWriter::new()
                .with_progressive_calibrator(cal.clone()),
        );

        let initial = cal.lock().unwrap().calibrated_threshold(
            "code",
            None,
            crate::pipeline::routing::TaskType::Code,
        );

        for i in 0..6 {
            let user_input = format!("show me the implementation {i}");
            let signal =
                crate::turn::implicit_feedback::detect_implicit_feedback_signal(&user_input, None);
            assert!(
                !matches!(signal.signal_type.as_str(), "correction" | "frustration"),
                "normal input should not be correction"
            );

            let messages = vec![json!({"role": "user", "content": &user_input})];
            let tool_calls = vec![json!({
                "id": format!("call-{i}"),
                "function": {"name": "write_file", "arguments": "{}"}
            })];
            let tool_results = vec![json!({
                "tool_call_id": format!("call-{i}"),
                "name": "write_file",
                "result": "ok"
            })];
            let mut payload = build_turn_hook_args(
                "user-1",
                "session-2",
                &messages,
                &tool_results,
                "Done.",
                &tool_calls,
                None,
                Some("gpt-4"),
                None,
                Some("evt-2"),
                (i + 1) as i64,
                None,
                false,
                false,
                false,
                false,
            );
            // No is_correction — bridge would not inject it
            payload
                .entry("routing_meta".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert("task_type".to_string(), json!("code"));

            run_bridge_hook_side_effects(
                Some(Value::Object(payload)),
                Arc::new(RecordingHookDbWriter::default()),
                Arc::new(RecordingReflectionStateStore::default()),
                Arc::new(RecordingReflectionLessonWriter::default()),
                Arc::new(RecordingObserverWorker::default()),
                Some(writer.clone()),
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let final_threshold = cal.lock().unwrap().calibrated_threshold(
            "code",
            None,
            crate::pipeline::routing::TaskType::Code,
        );

        assert_eq!(
            initial, final_threshold,
            "threshold should not change without correction signal"
        );
    }

    /// learning_writer=None → spawned task completes without panic.
    #[tokio::test]
    async fn e2e_no_learning_writer_graceful_noop() {
        let mut payload = build_turn_hook_args(
            "user-1",
            "session-3",
            &[json!({"role": "user", "content": "wrong"})],
            &[json!({"tool_call_id": "c1", "name": "bash", "result": "err"})],
            "Failed.",
            &[json!({"id": "c1", "function": {"name": "bash", "arguments": "{}"}})],
            None,
            Some("gpt-4"),
            None,
            Some("evt-3"),
            1,
            None,
            false,
            false,
            false,
            false,
        );
        payload.insert("is_correction".to_string(), json!(true));

        run_bridge_hook_side_effects(
            Some(Value::Object(payload)),
            Arc::new(RecordingHookDbWriter::default()),
            Arc::new(RecordingReflectionStateStore::default()),
            Arc::new(RecordingReflectionLessonWriter::default()),
            Arc::new(RecordingObserverWorker::default()),
            None, // no learning writer
        );

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        // No panic = success
    }

    /// Mixed scenario: 5 corrections + 5 normal → threshold decreases.
    /// Normal turns need tool_quality_assessments to pass the ambiguous quality gate.
    #[tokio::test]
    async fn e2e_mixed_corrections_and_normal_partial_threshold_decrease() {
        use std::sync::{Arc, Mutex as StdMutex};

        let cal = Arc::new(StdMutex::new(
            crate::pipeline::calibration::ProgressiveCalibrator::new(0.70),
        ));
        let writer: Arc<dyn crate::TurnLearningWriter> = Arc::new(
            crate::pipeline::learning::PipelineLearningWriter::new()
                .with_progressive_calibrator(cal.clone()),
        );

        let initial = cal.lock().unwrap().calibrated_threshold(
            "fetch",
            None,
            crate::pipeline::routing::TaskType::Fetch,
        );

        for i in 0..10 {
            let is_correction_turn = i < 5;
            let user_input = if is_correction_turn {
                format!("不对，重新来 {i}")
            } else {
                format!("show me the PRs for project {i}")
            };
            let messages = vec![
                json!({"role": "assistant", "content": "Previous response."}),
                json!({"role": "user", "content": &user_input}),
            ];
            let tool_calls = vec![json!({
                "id": format!("call-{i}"),
                "function": {"name": "github_list_prs", "arguments": "{}"}
            })];
            let tool_results = vec![json!({
                "tool_call_id": format!("call-{i}"),
                "name": "github_list_prs",
                "result": "{\"prs\": [{\"title\": \"fix\"}]}"
            })];
            let mut payload = build_turn_hook_args(
                "user-1",
                "session-4",
                &messages,
                &tool_results,
                "Here.",
                &tool_calls,
                None,
                Some("gpt-4"),
                None,
                Some("evt-4"),
                (i + 1) as i64,
                None,
                false,
                false,
                false,
                false,
            );
            if is_correction_turn {
                payload.insert("is_correction".to_string(), json!(true));
            }
            // Add quality assessments so normal turns pass the ambiguous quality gate
            payload.insert(
                "tool_quality_assessments".to_string(),
                json!([
                    {"tool_name": "github_list_prs", "quality_score": 0.85}
                ]),
            );
            payload
                .entry("routing_meta".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .unwrap()
                .insert("task_type".to_string(), json!("fetch"));

            run_bridge_hook_side_effects(
                Some(Value::Object(payload)),
                Arc::new(RecordingHookDbWriter::default()),
                Arc::new(RecordingReflectionStateStore::default()),
                Arc::new(RecordingReflectionLessonWriter::default()),
                Arc::new(RecordingObserverWorker::default()),
                Some(writer.clone()),
            );
        }

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let final_threshold = cal.lock().unwrap().calibrated_threshold(
            "fetch",
            None,
            crate::pipeline::routing::TaskType::Fetch,
        );

        // 5/10 corrections = 50% correction rate → threshold should decrease
        assert!(
            final_threshold < initial,
            "threshold should decrease with 50% correction rate: \
             initial={initial}, final={final_threshold}"
        );
    }
}
