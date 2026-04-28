use astra_turn_core::cloud_approval_policy::{CloudGatedToolKind, cloud_gated_tool_kind};
use serde_json::Value;

/// Local/legacy mutating tools that are not covered by cloud approval policy.
///
/// INVARIANT: prefer adding new mutating tools to `CLOUD_APPROVAL_REQUIRED_TOOLS`
/// with `CloudGatedToolKind::Write`. Keep this supplemental list limited to
/// tools cross-checked against edge schemas or internal rollback hooks that can
/// change local files/git state but are not cloud-gated.
const NON_CLOUD_MUTATION_TOOLS: &[&str] = &[
    "apply_patch",
    // Conservative: even a no-op checkout can refresh file contents.
    "git_checkout_file",
    "rollback_git_worktrees",
];

/// True for tools whose mutation status depends on arguments rather than the
/// tool name alone, e.g. `bash` command text or `git_worktree` action.
pub(crate) fn tool_classified_from_arguments(name: &str) -> bool {
    matches!(
        cloud_gated_tool_kind(name),
        Some(CloudGatedToolKind::Execute)
    ) || name == "git_worktree"
}

/// True when a successful call with this tool name is known to invalidate
/// cached read-only results without inspecting arguments.
pub(crate) fn tool_name_invalidates_read_cache(name: &str) -> bool {
    matches!(cloud_gated_tool_kind(name), Some(CloudGatedToolKind::Write))
        || NON_CLOUD_MUTATION_TOOLS.contains(&name)
}

/// True when a successful tool call should evict read-only idempotency cache
/// entries. Argument-classified tools require their mutating argument form.
pub(crate) fn tool_call_invalidates_read_cache(name: &str, args: Option<&Value>) -> bool {
    if tool_name_invalidates_read_cache(name) {
        return true;
    }
    if !tool_classified_from_arguments(name) {
        return false;
    }
    if matches!(
        cloud_gated_tool_kind(name),
        Some(CloudGatedToolKind::Execute)
    ) {
        return args
            .and_then(crate::turn::tool_argument_hints::command_hint_from_args)
            .is_some_and(crate::bash_intent::bash_command_looks_mutating);
    }
    if name == "git_worktree" {
        return git_worktree_invalidates_read_cache(args);
    }
    false
}

fn git_worktree_invalidates_read_cache(args: Option<&Value>) -> bool {
    let Some(action) = args
        .and_then(|args| args.get("action"))
        .and_then(Value::as_str)
    else {
        // Conservative by design: missing `action` still evicts so we prefer
        // extra cache misses over serving stale read-only results.
        return true;
    };
    !matches!(action, "list" | "ls")
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use astra_turn_core::cloud_approval_policy::{
        CLOUD_APPROVAL_REQUIRED_TOOLS, CloudGatedToolKind, cloud_gated_tool_kind,
    };

    use super::*;

    #[test]
    fn all_cloud_write_tools_invalidate_read_cache() {
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS {
            match cloud_gated_tool_kind(name) {
                Some(CloudGatedToolKind::Write) => assert!(
                    tool_name_invalidates_read_cache(name),
                    "write-gated tool must invalidate read cache: {name}"
                ),
                Some(CloudGatedToolKind::Execute) => assert!(
                    !tool_name_invalidates_read_cache(name),
                    "execute tools are classified from arguments: {name}"
                ),
                None => panic!("required tool must classify: {name}"),
            }
        }
    }

    #[test]
    fn server_approval_required_tools_share_read_cache_classification() {
        for &name in astra_tools::APPROVAL_REQUIRED_TOOLS {
            if matches!(
                cloud_gated_tool_kind(name),
                Some(CloudGatedToolKind::Execute)
            ) {
                assert!(
                    tool_classified_from_arguments(name),
                    "execute tool must be argument-classified: {name}"
                );
            } else {
                assert!(
                    tool_name_invalidates_read_cache(name),
                    "server approval-required write tool must invalidate read cache: {name}"
                );
            }
        }
    }

    #[test]
    fn non_cloud_mutation_list_is_grounded_in_known_tool_surfaces() {
        let schema_names: HashSet<String> = astra_tools::schemas::all_tool_schemas()
            .into_iter()
            .filter_map(|schema| {
                schema
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        let internal_hooks = ["apply_patch", "rollback_git_worktrees"];

        for &name in NON_CLOUD_MUTATION_TOOLS {
            assert!(
                schema_names.contains(name) || internal_hooks.contains(&name),
                "non-cloud mutation tool must be present in schemas or documented internal hooks: {name}"
            );
            assert!(
                tool_name_invalidates_read_cache(name),
                "non-cloud mutation tool must invalidate read cache: {name}"
            );
        }
    }

    #[test]
    fn read_only_tools_do_not_overlap_mutation_classification() {
        for &name in crate::turn::headless_tool_assembly::READ_ONLY_TOOLS {
            assert!(
                !tool_name_invalidates_read_cache(name),
                "read-only tool must not also be name-classified as mutation: {name}"
            );
            assert!(
                !tool_classified_from_arguments(name),
                "read-only tool must not require argument mutation classification: {name}"
            );
        }
    }

    #[test]
    fn argument_classified_tools_invalidate_only_for_mutating_args() {
        assert!(tool_classified_from_arguments("bash"));
        assert!(tool_call_invalidates_read_cache(
            "bash",
            Some(&serde_json::json!({"command": "printf new > a.txt"}))
        ));
        assert!(!tool_call_invalidates_read_cache(
            "bash",
            Some(&serde_json::json!({"command": "sed -n '1,20p' a.txt"}))
        ));
        assert!(tool_classified_from_arguments("git_worktree"));
        assert!(tool_call_invalidates_read_cache(
            "git_worktree",
            Some(&serde_json::json!({"action": "add", "branch": "feature"}))
        ));
        assert!(!tool_call_invalidates_read_cache(
            "git_worktree",
            Some(&serde_json::json!({"action": "list"}))
        ));
    }
}
