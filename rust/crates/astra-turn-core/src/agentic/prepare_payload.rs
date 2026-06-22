//! Agentic `/chat` JSON payload steps after tool schemas are resolved (selector hints + `edge_tools`).

use std::collections::HashSet;

use astra_core::ConfidenceInterval;
use serde_json::Value;

use crate::tool::registry::report::SelectionReport;
use crate::tool::registry::selection_edge_hints as tool_registry;

use crate::chat_turn_payload::attach_filtered_edge_tools;

/// Apply first-selection hints to `edge_profile`, then attach restricted-filtered `edge_tools`.
///
/// Only the *names* of dynamically selected tools are forwarded to the hint
/// path. Budget fields from `first_selection_report` are deliberately NOT
/// propagated: they reflect pre-restriction state and would be stale once
/// `restricted_tools` filters the dynamic set. Passing names only makes the
/// hint path budget-free by construction.
pub fn apply_selector_hints_then_attach_filtered_edge_tools(
    payload: &mut Value,
    turn_schemas: Vec<Value>,
    restricted_tools: &HashSet<String>,
    first_selection_report: Option<&SelectionReport>,
    selection_confidence: f64,
    learned_task_type: Option<&str>,
) {
    // Build the restricted dynamic-name list without cloning the whole report.
    // We only need the names; budget_used / budget_total are intentionally
    // dropped here so they cannot leak into edge_profile telemetry.
    let filtered_names: Vec<String> = match first_selection_report {
        Some(report) => report
            .dynamic_tools_selected
            .iter()
            .filter(|name| !restricted_tools.contains(name.as_str()))
            .cloned()
            .collect(),
        None => Vec::new(),
    };
    let dynamic_names_arg = if filtered_names.is_empty() {
        None
    } else {
        Some(filtered_names.as_slice())
    };
    tool_registry::apply_selector_hints_to_edge_profile(
        &mut payload["edge_profile"],
        dynamic_names_arg,
        ConfidenceInterval::exact(selection_confidence),
        learned_task_type,
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
            dynamic_tools_selected: vec!["github_list_prs".into(), "grep".into()],
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
            Some("code"),
        );
        assert!(payload["edge_profile"]["recommended_tools"].is_array());
        assert_eq!(payload["edge_tools"][0]["function"]["name"], "grep");
    }

    #[test]
    fn restricted_dynamic_tools_are_not_recommended() {
        let mut payload = json!({
            "edge_profile": { "cwd": "/tmp" }
        });
        let report = SelectionReport {
            tools_selected: vec!["grep".into()],
            dynamic_tools_selected: vec!["grep".into()],
            selected_count: 1,
            budget_used: 10,
            budget_total: 100,
        };
        let schemas = vec![json!({"function": {"name": "grep"}})];
        let restricted = HashSet::from(["grep".to_string()]);

        apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            schemas,
            &restricted,
            Some(&report),
            0.9,
            Some("code"),
        );

        assert!(payload["edge_profile"].get("recommended_tools").is_none());
        assert_eq!(payload["edge_tools"].as_array().unwrap().len(), 0);
    }

    /// Regression: the hint path must not carry `budget_used` from the report.
    /// After this refactor, `edge_profile` should contain only `recommended_tools`,
    /// `selection_confidence`, and `selection_task_type` — never a budget field.
    /// This guards against re-introducing stale budget propagation.
    #[test]
    fn hint_path_never_emits_budget_fields() {
        let mut payload = json!({ "edge_profile": { "cwd": "/tmp" } });
        let report = SelectionReport {
            tools_selected: vec!["grep".into()],
            dynamic_tools_selected: vec!["grep".into()],
            selected_count: 1,
            budget_used: 999,
            budget_total: 1000,
        };
        let schemas = vec![json!({"function": {"name": "grep"}})];
        let restricted = HashSet::new();
        apply_selector_hints_then_attach_filtered_edge_tools(
            &mut payload,
            schemas,
            &restricted,
            Some(&report),
            0.9,
            Some("code"),
        );
        let ep = &payload["edge_profile"];
        assert!(
            ep.get("budget_used").is_none(),
            "budget_used leaked into hints"
        );
        assert!(
            ep.get("budget_total").is_none(),
            "budget_total leaked into hints"
        );
        assert!(ep.get("recommended_tools").is_some());
    }
}
