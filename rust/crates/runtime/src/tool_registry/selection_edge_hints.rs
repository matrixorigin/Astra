//! Edge `edge_profile` fields derived from tool selection (recommended dynamic tools, confidence).

use serde_json::{Value, json};

use super::SelectionReport;
use super::meta::TOOL_CATALOG;

/// First `max` selected tool names that are not catalog-pinned (dynamic tools), preserving order.
pub fn top_unpinned_tool_names_from_report(report: &SelectionReport, max: usize) -> Vec<String> {
    report
        .tools_selected
        .iter()
        .filter(|n| {
            !TOOL_CATALOG
                .iter()
                .any(|t| t.pinned && t.name == n.as_str())
        })
        .take(max)
        .cloned()
        .collect()
}

/// Merge selector guidance into an existing `edge_profile` JSON object (mutates in place).
pub fn apply_selector_hints_to_edge_profile(
    edge_profile: &mut Value,
    first_selection_report: Option<&SelectionReport>,
    selection_confidence: f64,
    learned_context_hint: &str,
    learned_task_type: Option<&str>,
) {
    let dynamic_tools = first_selection_report
        .map(|r| top_unpinned_tool_names_from_report(r, 3))
        .unwrap_or_default();
    if selection_confidence >= 0.4
        && !dynamic_tools.is_empty()
        && let Some(obj) = edge_profile.as_object_mut()
    {
        obj.insert("recommended_tools".to_string(), json!(dynamic_tools));
        obj.insert(
            "selection_confidence".to_string(),
            json!(selection_confidence),
        );
    }
    if !learned_context_hint.is_empty()
        && let Some(obj) = edge_profile.as_object_mut()
    {
        obj.insert(
            "learned_context_hint".to_string(),
            json!(learned_context_hint),
        );
    }
    if let Some(tt) = learned_task_type
        && let Some(obj) = edge_profile.as_object_mut()
    {
        obj.insert("selection_task_type".to_string(), json!(tt));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_unpinned_skips_pinned() {
        let report = SelectionReport {
            tools_selected: vec!["read_file".into(), "github_list_prs".into(), "grep".into()],
            selected_count: 3,
            budget_used: 0,
            budget_total: 0,
        };
        let names = top_unpinned_tool_names_from_report(&report, 3);
        assert!(!names.contains(&"read_file".to_string()));
        assert!(names.contains(&"github_list_prs".to_string()));
    }

    #[test]
    fn apply_hints_respects_confidence_threshold() {
        let mut ep = json!({});
        let report = SelectionReport {
            tools_selected: vec!["github_list_prs".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 0,
        };
        apply_selector_hints_to_edge_profile(&mut ep, Some(&report), 0.39, "", None);
        assert!(ep.get("recommended_tools").is_none());

        apply_selector_hints_to_edge_profile(&mut ep, Some(&report), 0.4, "", None);
        assert!(ep.get("recommended_tools").is_some());
    }

    #[test]
    fn apply_learned_hint_and_task_type() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(&mut ep, None, 1.0, "hint", Some("fetch"));
        assert_eq!(ep["learned_context_hint"], "hint");
        assert_eq!(ep["selection_task_type"], "fetch");
    }
}
