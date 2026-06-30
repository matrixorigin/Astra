use std::sync::RwLock;

use serde_json::Value;

pub(crate) fn handle_introspect(
    args: &Value,
    session_id: &str,
    snapshot: &RwLock<Option<astra_turn_core::introspect::IntrospectSnapshot>>,
) -> String {
    let request = astra_turn_core::introspect::IntrospectRequest::from_args(args);
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

    let snapshot = snapshot.unwrap_or_default();
    astra_turn_core::introspect::render_introspect_request(&snapshot, &request)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        );

        assert!(out.contains("## Step Latency"), "got: {out}");
        assert!(out.contains("model_wait"), "got: {out}");
        assert!(out.contains("8000"), "got: {out}");
    }
}
