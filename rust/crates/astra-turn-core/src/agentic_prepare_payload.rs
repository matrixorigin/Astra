//! Agentic `/chat` JSON payload steps after tool schemas are resolved (selector hints + `edge_tools`).

use std::collections::HashSet;

use astra_core::ConfidenceInterval;
use serde_json::Value;

use crate::tool_registry_report::SelectionReport;
use crate::tool_registry_selection_edge_hints as tool_registry;

use crate::chat_turn_payload::attach_filtered_edge_tools;

/// Apply first-selection hints to `edge_profile`, then attach restricted-filtered `edge_tools`.
///
/// `user_scenario` — user's declared `UserProfile.current_scenario`, if any. Used
/// alongside `learned_task_type` for scenario-tagged hint matching (see
/// `tool_registry_selection_edge_hints::hint_body_for_scenarios`).
#[allow(clippy::too_many_arguments)]
pub fn apply_selector_hints_then_attach_filtered_edge_tools(
    payload: &mut Value,
    turn_schemas: Vec<Value>,
    restricted_tools: &HashSet<String>,
    first_selection_report: Option<&SelectionReport>,
    selection_confidence: f64,
    learned_context_hint: &str,
    learned_task_type: Option<&str>,
    user_scenario: Option<&str>,
) {
    tool_registry::apply_selector_hints_to_edge_profile(
        &mut payload["edge_profile"],
        first_selection_report,
        ConfidenceInterval::exact(selection_confidence),
        learned_context_hint,
        learned_task_type,
        user_scenario,
    );
    attach_filtered_edge_tools(payload, turn_schemas, restricted_tools);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn attaches_tools_after_recommended_hints() {
        let mut payload = json!({
            "edge_profile": { "cwd": "/tmp" }
        });
        let report = SelectionReport {
            tools_selected: vec!["github_list_prs".into(), "grep".into()],
            selected_count: 2,
            budget_used: 10,
            budget_total: 100,
        };
        let schemas = vec![json!({"function": {"name": "grep"}})];
        let restricted = HashSet::new();
        apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            schemas,
            &restricted,
            Some(&report),
            0.9,
            "hint",
            Some("code"),
            None,
        );
        assert!(payload["edge_profile"]["recommended_tools"].is_array());
        assert_eq!(payload["edge_tools"][0]["function"]["name"], "grep");
    }
}
