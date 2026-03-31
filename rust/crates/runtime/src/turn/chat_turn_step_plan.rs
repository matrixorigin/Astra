//! Step recorder `Plan` phase after `/chat` payload assembly.

use crate::pipeline::step_recorder::StepRecorder;
use crate::tool_registry::SelectionReport;

/// Mirrors the `prepare_chat_turn_payload` tail that calls [`StepRecorder::record_plan`].
pub fn record_agentic_step_plan_after_payload_prep(
    step_recorder: &mut StepRecorder,
    first_selection_report: Option<&SelectionReport>,
    first_budget_pressure: f64,
    selection_confidence: f64,
) {
    let selected_tool_names: Vec<String> = first_selection_report
        .map(|r| r.tools_selected.clone())
        .unwrap_or_default();
    let budget_tokens = first_selection_report
        .map(|r| r.budget_used as u64)
        .unwrap_or(0);
    step_recorder.record_plan(
        &selected_tool_names,
        selection_confidence,
        first_budget_pressure,
        budget_tokens,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_plan_with_report() {
        let mut rec = StepRecorder::new("sid", "tid");
        let rep = SelectionReport {
            tools_selected: vec!["bash".into()],
            selected_count: 1,
            budget_used: 12,
            budget_total: 100,
        };
        record_agentic_step_plan_after_payload_prep(&mut rec, Some(&rep), 0.3, 0.9);
        // Smoke: recorder accepted without panic; phase advanced internally.
        assert!(!rec.events().is_empty());
    }
}
