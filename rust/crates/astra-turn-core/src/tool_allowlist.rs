use std::collections::HashSet;

pub fn normalize_tool_name(tool: &str) -> Option<String> {
    astra_text_utils::tool_name::normalize_ascii_tool_name(tool)
}

pub fn normalize_tool_names<I>(tools: I) -> HashSet<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    tools
        .into_iter()
        .filter_map(|tool| normalize_tool_name(tool.as_ref()))
        .collect()
}

/// Compose request- and skill-scoped allowlists into the effective set the
/// runtime should enforce.
///
/// Semantics: **AND-intersection**. When both lanes are present, only tools
/// appearing in both pass. Either lane being `None` is treated as
/// "no opinion" and the other lane wins; both `None` means unrestricted
/// (return `None`). An *empty* `Some` set is honoured as an explicit deny-all
/// — that distinction is preserved through the intersection.
///
/// Different policies (e.g. union semantics) require a different function;
/// callers should not change behaviour here without auditing every caller of
/// `compute_effective_allowlist`.
pub fn compute_effective_allowlist(
    request_allowed: Option<&HashSet<String>>,
    skill_allowed: Option<&HashSet<String>>,
) -> Option<HashSet<String>> {
    match (request_allowed, skill_allowed) {
        (None, None) => None,
        (Some(request), None) => Some(normalize_tool_names(request)),
        (None, Some(skill)) => Some(normalize_tool_names(skill)),
        (Some(request), Some(skill)) => {
            let request = normalize_tool_names(request);
            let skill = normalize_tool_names(skill);
            Some(request.intersection(&skill).cloned().collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effective_allowlist_normalizes_before_intersection() {
        let request = HashSet::from([" Bash ".to_string(), "READ_FILE".to_string()]);
        let skill = HashSet::from(["bash".to_string()]);

        assert_eq!(
            compute_effective_allowlist(Some(&request), Some(&skill)),
            Some(HashSet::from(["bash".to_string()]))
        );
    }

    #[test]
    fn intersection_strips_members_not_in_both_sets() {
        let request = HashSet::from(["Bash".to_string(), "Python".to_string()]);
        let skill = HashSet::from([" python ".to_string(), "nodejs".to_string()]);

        assert_eq!(
            compute_effective_allowlist(Some(&request), Some(&skill)),
            Some(HashSet::from(["python".to_string()]))
        );
    }

    #[test]
    fn explicit_empty_allowlist_is_preserved() {
        let request = HashSet::new();

        assert_eq!(
            compute_effective_allowlist(Some(&request), None),
            Some(HashSet::new())
        );
    }

    #[test]
    fn empty_allowlist_beats_skill_scoped_set() {
        let empty = HashSet::new();
        let skill = HashSet::from(["python".to_string()]);

        assert_eq!(
            compute_effective_allowlist(Some(&empty), Some(&skill)),
            Some(HashSet::new())
        );
    }

    #[test]
    fn both_lanes_none_means_no_restriction() {
        assert_eq!(compute_effective_allowlist(None, None), None);
    }

    #[test]
    fn normalize_tool_name_rejects_blank_entries() {
        assert_eq!(normalize_tool_name(" bash "), Some("bash".to_string()));
        assert_eq!(normalize_tool_name(" \t "), None);
    }
}
