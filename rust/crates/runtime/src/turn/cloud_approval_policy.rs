//! Which edge tool names require an `approval_required` round-trip before `tool_request`
//! in cloud-orchestrated delivery ([`super::cloud_tool_delivery`]).
//!
//! CLI permission prompts use [`cloud_gated_tool_kind`] so icons (Execute vs Write) and cloud gating
//! cannot drift.

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

/// Subset of [`CLOUD_APPROVAL_REQUIRED_TOOLS`] that take a shell `command` argument (CLI ▶).
pub const CLOUD_APPROVAL_EXECUTE_TOOLS: &[&str] = &["bash", "exec", "run_command", "shell"];

/// Kind of side effect for tools gated before edge execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudGatedToolKind {
    Write,
    Execute,
}

/// Returns [`None`] when the tool is not cloud-gated (treated as read-only for approval purposes).
#[inline]
pub fn cloud_gated_tool_kind(name: &str) -> Option<CloudGatedToolKind> {
    if !CLOUD_APPROVAL_REQUIRED_TOOLS.contains(&name) {
        return None;
    }
    if CLOUD_APPROVAL_EXECUTE_TOOLS.contains(&name) {
        Some(CloudGatedToolKind::Execute)
    } else {
        Some(CloudGatedToolKind::Write)
    }
}

/// Returns true if `name` is in [`CLOUD_APPROVAL_REQUIRED_TOOLS`].
#[inline]
pub fn edge_tool_requires_cloud_approval(name: &str) -> bool {
    cloud_gated_tool_kind(name).is_some()
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

    #[test]
    fn execute_tools_sorted_and_subset_of_required() {
        let mut sorted = CLOUD_APPROVAL_EXECUTE_TOOLS.to_vec();
        sorted.sort_unstable();
        assert_eq!(
            CLOUD_APPROVAL_EXECUTE_TOOLS,
            sorted.as_slice(),
            "CLOUD_APPROVAL_EXECUTE_TOOLS should stay sorted"
        );
        for &name in CLOUD_APPROVAL_EXECUTE_TOOLS {
            assert!(
                CLOUD_APPROVAL_REQUIRED_TOOLS.contains(&name),
                "{name} must appear in CLOUD_APPROVAL_REQUIRED_TOOLS"
            );
        }
    }

    #[test]
    fn required_tools_partition_into_execute_and_write() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS {
            let kind = cloud_gated_tool_kind(name).expect("required tools must classify");
            match kind {
                CloudGatedToolKind::Execute => {
                    assert!(CLOUD_APPROVAL_EXECUTE_TOOLS.contains(&name));
                }
                CloudGatedToolKind::Write => {
                    assert!(!CLOUD_APPROVAL_EXECUTE_TOOLS.contains(&name));
                }
            }
        }
    }
}
