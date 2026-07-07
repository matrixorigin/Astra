//! Step recorder `Plan` phase after `/chat` payload assembly.

use crate::tool::registry::report::ToolSelectionReport;
use astra_pipeline::step_recorder::StepRecorder;

/// Mirrors the `prepare_chat_turn_payload` tail that calls [`StepRecorder::record_plan`].
pub fn record_agentic_step_plan_after_payload_prep(
    step_recorder: &mut StepRecorder,
    first_surface_report: Option<&ToolSelectionReport>,
    first_budget_pressure: f64,
) {
    let selected_tool_names: Vec<String> = first_surface_report
        .map(|r| r.visible_tools.clone())
        .unwrap_or_default();
    let schema_budget_tokens = first_surface_report
        .map(|r| r.schema_budget_used as u64)
        .unwrap_or(0);
    step_recorder.record_plan(
        &selected_tool_names,
        first_budget_pressure,
        schema_budget_tokens,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_plan_with_report() {
        let mut rec = StepRecorder::new("test-user", "sid", "tid");
        let rep = ToolSelectionReport {
            visible_tools: vec!["bash".into()],
            visible_count: 1,
            schema_budget_used: 12,
            schema_budget_total: 100,
        };
        record_agentic_step_plan_after_payload_prep(&mut rec, Some(&rep), 0.3);
        // Smoke: recorder accepted without panic; phase advanced internally.
        assert!(!rec.events().is_empty());
    }
}
