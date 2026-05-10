//! Edge `edge_profile` fields derived from tool selection (recommended dynamic tools, confidence).

use astra_core::ConfidenceInterval;
use serde_json::{Value, json};

use crate::tool_registry_meta::TOOL_CATALOG;
use crate::tool_registry_report::SelectionReport;

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

/// Parse scenario tags from a hint prefix like `[code_review,coding] rest of hint`.
///
/// Returns `(Some(tags), remaining_hint)` when the hint starts with `[tag1,tag2]`
/// (comma-separated, whitespace-trimmed). Returns `(None, whole_hint)` when there
/// is no recognized tag prefix — those hints are treated as unconditional.
///
/// This is used by [`apply_selector_hints_to_edge_profile`] to suppress hints that
/// were learned under a different scenario than the current one (e.g. a
/// `[code_review]` hint should not leak into a `coding` turn where it is noise).
fn parse_hint_scenarios(hint: &str) -> (Option<Vec<&str>>, &str) {
    let hint = hint.trim_start();
    if !hint.starts_with('[') {
        return (None, hint);
    }
    let Some(close) = hint.find(']') else {
        return (None, hint);
    };
    let inside = &hint[1..close];
    // Tags must be ident-ish (letters, digits, `_`, `-`, `,`, whitespace).
    // Anything else → this isn't a scenario tag, just a regular bracketed hint.
    if !inside
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | ',' | ' ' | '\t'))
    {
        return (None, hint);
    }
    let tags: Vec<&str> = inside
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tags.is_empty() {
        return (None, hint);
    }
    let remaining = hint[close + 1..].trim_start();
    (Some(tags), remaining)
}

/// Decide whether a (possibly scenario-tagged) hint should be injected for the
/// current scenario. Untagged hints are always kept (backward-compat). Tagged
/// hints are kept iff `current_scenario` matches one of the declared tags
/// (case-insensitive). When `current_scenario` is `None`, tagged hints are
/// dropped — we have no way to know they apply.
///
/// Returns the hint body to inject (without the tag prefix), or `None` when
/// the hint should be suppressed.
pub fn hint_body_for_scenario<'a>(
    hint: &'a str,
    current_scenario: Option<&str>,
) -> Option<&'a str> {
    if hint.is_empty() {
        return None;
    }
    let (tags, body) = parse_hint_scenarios(hint);
    match tags {
        None => Some(body),
        Some(tags) => match current_scenario {
            None => None,
            Some(sc) => {
                let sc_lower = sc.to_ascii_lowercase();
                if tags
                    .iter()
                    .any(|t| t.eq_ignore_ascii_case(&sc_lower) || t.eq_ignore_ascii_case(sc))
                {
                    Some(body)
                } else {
                    None
                }
            }
        },
    }
}

/// Merge selector guidance into an existing `edge_profile` JSON object (mutates in place).
pub fn apply_selector_hints_to_edge_profile(
    edge_profile: &mut Value,
    first_selection_report: Option<&SelectionReport>,
    selection_confidence: ConfidenceInterval,
    learned_context_hint: &str,
    learned_task_type: Option<&str>,
) {
    let dynamic_tools = first_selection_report
        .map(|r| top_unpinned_tool_names_from_report(r, 3))
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
    if !learned_context_hint.is_empty()
        && let Some(obj) = edge_profile.as_object_mut()
    {
        let scenario = learned_task_type;
        if let Some(body) = hint_body_for_scenario(learned_context_hint, scenario) {
            obj.insert(
                "learned_context_hint".to_string(),
                json!(body),
            );
        }
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
        let report = SelectionReport {
            tools_selected: vec!["github_list_prs".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 0,
        };
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&report),
            ConfidenceInterval::exact(0.39),
            "",
            None,
        );
        assert!(ep.get("recommended_tools").is_none());

        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&report),
            ConfidenceInterval::exact(0.4),
            "",
            None,
        );
        assert!(ep.get("recommended_tools").is_some());
        assert_eq!(ep["selection_confidence"]["point"], 0.4);
    }

    #[test]
    fn apply_learned_hint_and_task_type() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "hint",
            Some("fetch"),
        );
        assert_eq!(ep["learned_context_hint"], "hint");
        assert_eq!(ep["selection_task_type"], "fetch");
    }

    #[test]
    fn untagged_hint_is_always_kept_for_backward_compat() {
        // Plain hints with no `[scenario]` prefix must still inject regardless
        // of the current scenario — otherwise existing learned hints would
        // suddenly disappear after this change.
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "plain hint with no tag",
            Some("code_review"),
        );
        assert_eq!(ep["learned_context_hint"], "plain hint with no tag");
    }

    #[test]
    fn tagged_hint_kept_when_scenario_matches() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "[code_review] prefer grep over broad read_file",
            Some("code_review"),
        );
        assert_eq!(
            ep["learned_context_hint"],
            "prefer grep over broad read_file",
            "tag prefix must be stripped on injection"
        );
    }

    #[test]
    fn tagged_hint_dropped_when_scenario_mismatches() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "[code_review] prefer grep over broad read_file",
            Some("coding"),
        );
        assert!(
            ep.get("learned_context_hint").is_none(),
            "hint learned under code_review must not leak into coding turn"
        );
    }

    #[test]
    fn tagged_hint_dropped_when_scenario_missing() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "[code_review] hint body",
            None,
        );
        assert!(
            ep.get("learned_context_hint").is_none(),
            "no current scenario → tagged hint has no way to verify applicability → drop"
        );
    }

    #[test]
    fn tagged_hint_with_multiple_tags_matches_any() {
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "[code_review, coding] hint body",
            Some("coding"),
        );
        assert_eq!(ep["learned_context_hint"], "hint body");
    }

    #[test]
    fn non_scenario_bracket_prefix_is_treated_as_body() {
        // `[P1]` is not ident-only but contains punctuation we don't allow — treat
        // as regular prose, not a scenario tag.
        let mut ep = json!({});
        apply_selector_hints_to_edge_profile(
            &mut ep,
            None,
            ConfidenceInterval::exact(1.0),
            "[P1!] urgent hint",
            Some("coding"),
        );
        // The `!` makes it non-ident → parse_hint_scenarios returns None → keep whole body.
        assert_eq!(ep["learned_context_hint"], "[P1!] urgent hint");
    }

    #[test]
    fn apply_hints_requires_confident_lower_bound() {
        let mut ep = json!({});
        let report = SelectionReport {
            tools_selected: vec!["github_list_prs".into()],
            selected_count: 1,
            budget_used: 0,
            budget_total: 0,
        };

        apply_selector_hints_to_edge_profile(
            &mut ep,
            Some(&report),
            ConfidenceInterval::new(0.6, 0.39, 0.9),
            "",
            None,
        );
        assert!(ep.get("recommended_tools").is_none());
    }
}
