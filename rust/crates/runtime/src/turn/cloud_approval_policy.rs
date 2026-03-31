//! Which edge tool names require an `approval_required` round-trip before `tool_request`
//! in cloud-orchestrated delivery ([`super::cloud_tool_delivery`]).
//!
//! Keep this list aligned with CLI permission-manager side-effect classification so local prompts
//! and cloud gates stay consistent.

/// Canonical tool names that must pass user approval (thin-client ledger) before edge execution.
pub const CLOUD_APPROVAL_REQUIRED_TOOLS: &[&str] = &[
    "bash",
    "create_file",
    "edit_file",
    "exec",
    "run_command",
    "shell",
    "str_replace",
    "write_file",
];

/// Returns true if `name` is in [`CLOUD_APPROVAL_REQUIRED_TOOLS`].
#[inline]
pub fn edge_tool_requires_cloud_approval(name: &str) -> bool {
    CLOUD_APPROVAL_REQUIRED_TOOLS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_listed_tool_requires_approval() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS {
            assert!(
                edge_tool_requires_cloud_approval(name),
                "list entry must satisfy predicate: {name}"
            );
        }
    }

    #[test]
    fn read_only_tools_skip_approval_gate() {
        for name in ["read_file", "list_dir", "grep", "glob", "git_status"] {
            assert!(
                !edge_tool_requires_cloud_approval(name),
                "{name} should not require cloud approval"
            );
        }
    }

    #[test]
    fn unknown_tool_not_gated() {
        assert!(!edge_tool_requires_cloud_approval("made_up_tool"));
        assert!(!edge_tool_requires_cloud_approval(""));
    }

    #[test]
    fn list_is_sorted_for_stable_diffs() {
        let mut sorted = CLOUD_APPROVAL_REQUIRED_TOOLS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            CLOUD_APPROVAL_REQUIRED_TOOLS,
            sorted.as_slice(),
            "CLOUD_APPROVAL_REQUIRED_TOOLS should stay sorted"
        );
    }
}
