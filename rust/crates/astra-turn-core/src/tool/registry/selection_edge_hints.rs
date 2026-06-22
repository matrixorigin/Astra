//! Edge `edge_profile` fields derived from tool selection (recommended dynamic tools, confidence).

use astra_core::ConfidenceInterval;
use serde_json::{Value, json};

/// First `max` tool names from the slice, preserving order.
pub fn top_dynamic_tool_names(names: &[String], max: usize) -> Vec<String> {
    names.iter().take(max).cloned().collect()
}

/// Merge selector guidance into an existing `edge_profile` JSON object (mutates in place).
///
/// `dynamic_tool_names` is the ordered list of dynamically selected tool names
/// for this turn. Only the names are needed — budget/cost fields are intentionally
/// NOT accepted here so the hint path cannot carry stale budget state from a
/// pre-filtering `SelectionReport`.
///
/// `learned_task_type` is the selector's inferred task archetype for
/// this turn (e.g. `"code_search"`, `"new_feature"`).
pub fn apply_selector_hints_to_edge_profile(
    edge_profile: &mut Value,
    dynamic_tool_names: Option<&[String]>,
    selection_confidence: ConfidenceInterval,
    learned_task_type: Option<&str>,
) {
    let dynamic_tools = dynamic_tool_names
        .map(|names| top_dynamic_tool_names(names, 3))
        .unwrap_or_default();
    if selection_confidence.conservatively_exceeds(0.4)
        && !dynamic_tools.is_empty()
        && let Some(obj) = edge_profile.as_object_mut()
    {
        obj.insert("recommended_tools".to_string(), json!(dynamic_tools));
        obj.insert(
            "selection_confidence".to_string(),
            json!(selection_confidence),
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
    fn top_dynamic_takes_first_n_preserving_order() {
        let names: Vec<String> = vec!["a".into(), "b".into(), "c".into()];
        let out = top_dynamic_tool_names(&names, 2);
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn apply_hints_respects_confidence_threshold() {
        let names: Vec<String> = vec!["github_list_prs".into()];
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&names),
            ConfidenceInterval::exact(0.39),
            None,
        );
        assert!(ep.get("recommended_tools").is_none());

        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&names),
            ConfidenceInterval::exact(0.4),
            None,
        );
        assert!(ep.get("recommended_tools").is_some());
        assert_eq!(ep["selection_confidence"]["point"], 0.4);
    }

    #[test]
    fn apply_hints_with_empty_names_is_noop_even_at_high_confidence() {
        let names: Vec<String> = vec![];
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&names),
            ConfidenceInterval::exact(0.9),
            None,
        );
        assert!(ep.get("recommended_tools").is_none());
    }

    #[test]
    fn apply_hints_with_none_names_is_noop() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(&mut ep, None, ConfidenceInterval::exact(0.9), None);
        assert!(ep.get("recommended_tools").is_none());
    }

    #[test]
    fn apply_task_type() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            Some("fetch"),
        );
        assert_eq!(ep["selection_task_type"], "fetch");
    }

    #[test]
    fn apply_hints_requires_confident_lower_bound() {
        let names: Vec<String> = vec!["github_list_prs".into()];
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&names),
            ConfidenceInterval::new(0.6, 0.39, 0.9),
            None,
        );
        assert!(ep.get("recommended_tools").is_none());
    }
}
