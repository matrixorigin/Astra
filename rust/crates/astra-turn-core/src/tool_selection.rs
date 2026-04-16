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

#[cfg(test)]
mod tests {
    use super::*;

    // --- plan_tool_subset_for_result_turn ---

    #[test]
    fn subset_empty_results_returns_none() {
        assert!(plan_tool_subset_for_result_turn(&[], "", &["bash"]).is_none());
    }

    #[test]
    fn subset_nonempty_user_query_returns_none() {
        let names = [Some("bash")];
        assert!(plan_tool_subset_for_result_turn(&names, "hello", &["bash"]).is_none());
    }

    #[test]
    fn subset_matching_tools() {
        let names = [Some("bash"), Some("read_file")];
        let result = plan_tool_subset_for_result_turn(&names, "", &["bash", "write_file"]);
        let subset = result.unwrap();
        assert_eq!(subset, vec!["bash"]);
    }

    #[test]
    fn subset_no_matching_tools_returns_none() {
        let names = [Some("bash")];
        assert!(plan_tool_subset_for_result_turn(&names, "", &["write_file"]).is_none());
    }

    #[test]
    fn subset_skips_none_names() {
        let names = [None, Some("bash"), None];
        let result = plan_tool_subset_for_result_turn(&names, "", &["bash"]);
        assert_eq!(result.unwrap(), vec!["bash"]);
    }

    #[test]
    fn subset_skips_empty_names() {
        let names = [Some(""), Some("bash")];
        let result = plan_tool_subset_for_result_turn(&names, "", &["bash"]);
        assert_eq!(result.unwrap(), vec!["bash"]);
    }

    // --- resolve_preferred_tool_status ---

    #[test]
    fn preferred_none_returns_none() {
        let final_set = BTreeSet::new();
        let avail = BTreeSet::new();
        assert!(resolve_preferred_tool_status(None, &final_set, &avail).is_none());
    }

    #[test]
    fn preferred_unavailable() {
        let final_set = BTreeSet::new();
        let avail = BTreeSet::new();
        assert_eq!(
            resolve_preferred_tool_status(Some("bash"), &final_set, &avail).unwrap(),
            "unavailable"
        );
    }

    #[test]
    fn preferred_selected() {
        let final_set = BTreeSet::from(["bash".to_string()]);
        let avail = BTreeSet::from(["bash".to_string()]);
        assert_eq!(
            resolve_preferred_tool_status(Some("bash"), &final_set, &avail).unwrap(),
            "selected"
        );
    }

    #[test]
    fn preferred_filtered() {
        let final_set = BTreeSet::new();
        let avail = BTreeSet::from(["bash".to_string()]);
        assert_eq!(
            resolve_preferred_tool_status(Some("bash"), &final_set, &avail).unwrap(),
            "filtered"
        );
    }
}
