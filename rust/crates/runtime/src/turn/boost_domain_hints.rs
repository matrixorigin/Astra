//! Derive [`DomainHint`] values from boost-term substrings (history + memory), for tool selection.

use crate::pipeline::routing::DomainHint;

/// Map boost-term keyword substrings → domain hints for gate softening / selector context.
/// Order: GitHub, Git, Code, Memory — multiple hints may apply.
pub fn domain_hints_from_boost_terms(boost_terms: &[String]) -> Vec<DomainHint> {
    let mut hints = Vec::new();
    let terms_lower: Vec<String> = boost_terms.iter().map(|t| t.to_lowercase()).collect();
    let has = |kw: &str| terms_lower.iter().any(|t| t.contains(kw));
    if has("github") || has("repo") || has("pr") || has("issue") || has("pull") {
        hints.push(DomainHint::GitHub);
    }
    if has("git") || has("commit") || has("branch") || has("diff") || has("log") {
        hints.push(DomainHint::Git);
    }
    if has("code") || has("file") || has("edit") || has("read") || has("write") {
        hints.push(DomainHint::Code);
    }
    if has("memory") || has("store") || has("remember") || has("preference") {
        hints.push(DomainHint::Memory);
    }
    hints
}

/// Debug strings for step recorder / telemetry (`format!("{hint:?}")` per hint).
#[must_use]
pub fn domain_hints_debug_strings(hints: &[DomainHint]) -> Vec<String> {
    hints.iter().map(|h| format!("{h:?}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_boost_terms_yields_no_hints() {
        assert!(domain_hints_from_boost_terms(&[]).is_empty());
    }

    #[test]
    fn github_keywords() {
        for terms in [
            vec!["github".into()],
            vec!["my-repo".into()],
            vec!["pull".into(), "request".into()],
        ] {
            let h = domain_hints_from_boost_terms(&terms);
            assert!(
                h.contains(&DomainHint::GitHub),
                "expected GitHub hint for {terms:?}"
            );
        }
    }

    #[test]
    fn git_branch_keywords() {
        let h = domain_hints_from_boost_terms(&["commit".into(), "main".into()]);
        assert!(h.contains(&DomainHint::Git));
    }

    #[test]
    fn code_and_memory_stack() {
        let h = domain_hints_from_boost_terms(&["read_file".into(), "memory_store".into()]);
        assert!(h.contains(&DomainHint::Code));
        assert!(h.contains(&DomainHint::Memory));
    }

    #[test]
    fn case_insensitive() {
        let h = domain_hints_from_boost_terms(&["GITHUB".into()]);
        assert!(h.contains(&DomainHint::GitHub));
    }

    #[test]
    fn domain_hints_debug_strings_match_debug_fmt() {
        let hints = vec![DomainHint::GitHub, DomainHint::Code];
        assert_eq!(
            domain_hints_debug_strings(&hints),
            vec!["GitHub".to_string(), "Code".to_string()]
        );
    }
}
