use std::sync::RwLock;

use serde_json::Value;

#[cfg(test)]
pub(crate) fn handle_introspect(
    args: &Value,
    session_id: &str,
    snapshot: &RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>,
    current_session_turn: u32,
) -> String {
    let snapshot = current_introspect_snapshot(session_id, snapshot, current_session_turn);
    render_introspect_snapshot(args, &snapshot)
}

pub(crate) fn current_introspect_snapshot(
    session_id: &str,
    snapshot: &RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>,
    current_session_turn: u32,
) -> astra_turn_core::introspect::IntrospectSnapshot {
    let snapshot = snapshot
        .read()
        .unwrap_or_else(|poison| {
            tracing::warn!(
                session_id = %session_id,
                "introspect_snapshot lock poisoned (writer panicked), recovering with inner data"
            );
            poison.into_inner()
        })
        .clone();

    match snapshot {
        Some(mut snapshot) => {
            astra_turn_core::introspect::mark_snapshot_age(&mut snapshot, current_session_turn);
            snapshot
        }
        None => astra_turn_core::introspect::IntrospectSnapshot::default(),
    }
}

pub(crate) fn render_introspect_snapshot(
    args: &Value,
    snapshot: &astra_turn_core::introspect::IntrospectSnapshot,
) -> String {
    let request = astra_turn_core::introspect::IntrospectRequest::from_args(args);
    let mut output = astra_turn_core::introspect::render_introspect_request(snapshot, &request);
    let live_max_bytes_ignored = args.get("max_bytes").is_some_and(|value| !value.is_null())
        && args.get("artifact").is_none();
    if live_max_bytes_ignored && request.format.is_json() {
        // Keep the machine boundary valid. The warning belongs in the typed
        // coverage object; appending Markdown here would make the JSON
        // report unparseable by the model projection and evaluation layer.
        if let Ok(mut value) = serde_json::from_str::<Value>(&output) {
            if let Some(data_coverage) = value
                .get_mut("data_coverage")
                .and_then(Value::as_object_mut)
            {
                data_coverage
                    .entry("warnings")
                    .or_insert_with(|| Value::Array(Vec::new()));
                if let Some(warnings) = data_coverage
                    .get_mut("warnings")
                    .and_then(Value::as_array_mut)
                {
                    warnings.push(Value::String(
                        "max_bytes applies only to introspect(artifact=...) windows; live observations are already bounded and this value was ignored"
                            .to_string(),
                    ));
                }
            }
            output = serde_json::to_string(&value).unwrap_or(output);
        } else {
            tracing::warn!("introspect JSON renderer returned an invalid JSON boundary");
        }
    } else if live_max_bytes_ignored {
        output.push_str(
            "\n\n## Observation Parameter Boundary\n`max_bytes` applies only to `introspect(artifact=...)` windows; live observations are already bounded and this value was ignored.",
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feedback() -> astra_turn_core::context_feedback::RuntimeFeedbackFrame {
        use astra_turn_core::context_feedback::{
            RuntimeContextFeedback, RuntimeFeedbackFrame, RuntimeFeedbackIdentity,
            RuntimeFeedbackProgress,
        };
        RuntimeFeedbackFrame {
            schema_version: RuntimeFeedbackFrame::SCHEMA_VERSION,
            identity: RuntimeFeedbackIdentity {
                session_id: "session-1".into(),
                run_id: "run-1".into(),
                agent_id: "agent-1".into(),
                model_id: "deepseek-v4-flash".into(),
                topology: astra_services::ModelRequestTopology::ServerOnly,
                request: None,
            },
            progress: RuntimeFeedbackProgress {
                session_turn: 2,
                agentic_round_index: 1,
                llm_rounds_completed: 2,
                slice_round_limit: 2,
                slice_rounds_remaining: 0,
                absolute_round_ceiling: None,
            },
            context: RuntimeContextFeedback {
                prompt_cache_identity: None,
                model_context_window_tokens: None,
                effective_input_limit_tokens: None,
                estimated_input_tokens: None,
                estimated_cache_eligible_tokens: None,
                token_pressure: Some(0.0),
                compaction_tier: astra_turn_core::compaction_types::CompactionTier::Normal,
            },
            request_usage: Some(Default::default()),
            run_usage: Some(Default::default()),
            was_truncated: false,
            cache_break_detected: None,
            policy_feedback: Default::default(),
        }
    }

    #[test]
    fn diagnostic_depth_includes_step_latency() {
        let snapshot = RwLock::new(Some(astra_turn_core::introspect::IntrospectSnapshot {
            step_latency: vec![astra_turn_core::introspect::StepLatencySnapshotEntry {
                step_id: "turn-1-step-3".into(),
                total_ms: Some(8_978),
                pre_tool_wait_ms: Some(8_000),
                first_tool_name: Some("bash".into()),
                tool_execution_ms: 8,
                max_tool_execution_ms: 8,
                tool_call_count: 1,
                dominant_phase: "model_wait".into(),
                terminal_event_kind: Some("StepIncomplete".into()),
                ..Default::default()
            }],
            ..Default::default()
        }));

        let out = handle_introspect(
            &serde_json::json!({"depth": "diagnostic"}),
            "session-1",
            &snapshot,
            1,
        );

        assert!(out.contains("## Step Latency"), "got: {out}");
        assert!(out.contains("model_wait"), "got: {out}");
        assert!(out.contains("8000"), "got: {out}");
    }

    #[test]
    fn summary_marks_stale_snapshot_from_current_turn() {
        let snapshot = RwLock::new(Some(astra_turn_core::introspect::IntrospectSnapshot {
            runtime_feedback: Some(feedback()),
            ..Default::default()
        }));

        let out = handle_introspect(
            &serde_json::json!({"depth": "summary"}),
            "session-1",
            &snapshot,
            5,
        );

        assert!(out.contains("remaining=0"), "got: {out}");
        assert!(!out.contains('∞'), "got: {out}");
        assert!(out.contains("Snapshot age: 3 turn(s)"), "got: {out}");
    }

    #[test]
    fn live_max_bytes_is_explicitly_ignored_instead_of_looking_like_expansion() {
        let out = handle_introspect(
            &serde_json::json!({"facet": "overview", "max_bytes": 16384}),
            "session-1",
            &RwLock::new(Some(
                astra_turn_core::introspect::IntrospectSnapshot::default(),
            )),
            1,
        );

        assert!(out.contains("max_bytes` applies only to `introspect(artifact=...)`"));
        assert!(out.contains("this value was ignored"));
    }

    #[test]
    fn live_max_bytes_json_keeps_structured_boundary_valid() {
        let out = handle_introspect(
            &serde_json::json!({
                "facet": "overview",
                "format": "json",
                "max_bytes": 16384
            }),
            "session-1",
            &RwLock::new(Some(
                astra_turn_core::introspect::IntrospectSnapshot::default(),
            )),
            1,
        );

        let value: Value = serde_json::from_str(&out).expect("live JSON must remain parseable");
        assert!(!out.contains("## Observation Parameter Boundary"));
        assert!(
            value["data_coverage"]["warnings"]
                .as_array()
                .expect("typed coverage warnings")
                .iter()
                .any(|warning| warning
                    .as_str()
                    .is_some_and(|warning| warning.contains("max_bytes applies only")))
        );
    }
}
