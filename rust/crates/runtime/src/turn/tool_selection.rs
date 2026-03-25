use std::collections::BTreeSet;

pub fn plan_tool_subset_for_result_turn(
    tool_result_names: &[Option<&str>],
    user_query: &str,
    available_tool_names: &[&str],
) -> Option<Vec<String>> {
    if tool_result_names.is_empty() || !user_query.is_empty() {
        return None;
    }

    let used_names = tool_result_names
        .iter()
        .flatten()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string())
        .collect::<BTreeSet<_>>();
    let subset_names = available_tool_names
        .iter()
        .filter(|name| used_names.contains(**name))
        .map(|name| name.to_string())
        .collect::<Vec<_>>();
    (!subset_names.is_empty()).then_some(subset_names)
}

pub fn resolve_preferred_tool_status(
    preferred_tool_name: Option<&str>,
    final_tool_names: &BTreeSet<String>,
    available_turn_tools: &BTreeSet<String>,
) -> Option<String> {
    let preferred = preferred_tool_name?;
    if !available_turn_tools.contains(preferred) {
        return Some("unavailable".to_string());
    }
    if final_tool_names.contains(preferred) {
        return Some("selected".to_string());
    }
    Some("filtered".to_string())
}
