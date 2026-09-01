use astra_turn_core::cloud_approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind, cloud_gated_tool_kind_with_args,
};
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
    "rollback_git_worktrees",
];

/// Tools that can directly change files or VCS state in the bound workspace.
///
/// This is intentionally narrower than [`tool_call_invalidates_read_cache`].
/// A memory write, Work graph update, message, or GitHub write changes state and
/// must invalidate affected caches, but it is not evidence that the user's
/// workspace was modified. Keeping the two predicates separate prevents a
/// read-only workspace turn from disabling unrelated Astra capabilities and
/// prevents an unrelated state write from satisfying a required code change.
const DIRECT_WORKSPACE_MUTATION_TOOLS: &[&str] = &[
    "write_file",
    "str_replace",
    "multi_edit",
    "edit_file",
    "apply_patch",
    "create_file",
    "delete_file",
    "notebook_edit",
    "rollback_file_edits",
    "rollback_git_worktrees",
    "rename_symbol",
];

/// True when this invocation may directly mutate the bound workspace.
///
/// Shell commands fail closed unless the shared permission classifier proves
/// them read-only. Consolidated git actions use their argument-aware category.
/// Control-plane and external-state writes remain outside this boundary.
pub fn tool_call_may_mutate_workspace(name: &str, args: Option<&Value>) -> bool {
    if DIRECT_WORKSPACE_MUTATION_TOOLS.contains(&name) {
        return true;
    }
    if name == "git" {
        return astra_turn_core::tool::categories::classify(name, args)
            .category
            .is_mutating();
    }
    if astra_turn_core::cloud_approval_policy::is_cloud_execute_tool(name) {
        return args
            .and_then(astra_turn_core::tool_argument_hints::command_hint_from_args)
            .map(|command| {
                !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command)
            })
            // A malformed/missing command is not proof of read-only behavior.
            .unwrap_or(true);
    }
    false
}

/// Positive, post-execution evidence that a call actually changed the bound
/// workspace.  This is deliberately different from
/// [`tool_call_may_mutate_workspace`]: the latter is an admission/cache safety
/// predicate and must treat an unknown shell command as potentially mutating;
/// an execution ledger cannot treat that possibility as a mutation receipt.
pub fn tool_call_records_workspace_mutation(name: &str, args: Option<&Value>) -> bool {
    if DIRECT_WORKSPACE_MUTATION_TOOLS.contains(&name) {
        return true;
    }
    if name == "git" {
        return astra_turn_core::tool::categories::classify(name, args)
            .category
            .is_mutating();
    }
    if astra_turn_core::cloud_approval_policy::is_cloud_execute_tool(name) {
        return args
            .and_then(astra_turn_core::tool_argument_hints::command_hint_from_args)
            .is_some_and(crate::bash_intent::bash_command_looks_mutating);
    }
    false
}

/// True only for a tool whose successful result can be evidence about the
/// bound workspace after a mutation.  This is intentionally a positive
/// capability predicate: an external write, message, memory update, or
/// unknown tool is not an observation merely because it did not mutate a
/// local file.
pub fn tool_call_may_observe_workspace(name: &str, args: Option<&Value>) -> bool {
    if name == "git" {
        return astra_turn_core::tool::categories::classify(name, args)
            .category
            .is_read_only();
    }
    if astra_turn_core::cloud_approval_policy::is_cloud_execute_tool(name) {
        let Some(command) =
            args.and_then(astra_turn_core::tool_argument_hints::command_hint_from_args)
        else {
            return false;
        };
        // A single shell invocation may both change state and then validate
        // the resulting state.  Check the ordered receipt before treating the
        // invocation as a mutation-only record.
        if astra_turn_core::evaluation::bash_command_has_post_mutation_validation(command) {
            return true;
        }
        // This function is consumed by the completion ledger after execution,
        // so use the positive mutation predicate above rather than the
        // conservative permission/cache predicate.  Unknown bash remains
        // unknown evidence.
        if tool_call_records_workspace_mutation(name, args) {
            return false;
        }
        if astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command) {
            return true;
        }
        // Compound verification commands often perform setup/deployment and
        // then read the resulting workspace or endpoint in the same shell
        // invocation.  The permission classifier correctly rejects the
        // whole compound command as non-read-only, but the completion ledger
        // can still use a later positive receipt without trusting prose.
        if astra_turn_core::evaluation::bash_command_has_post_mutation_validation(command) {
            return true;
        }
        // Build/test commands are useful workspace receipts even when the
        // permission classifier intentionally requires approval for their
        // unknown/side-effect-capable shell form (notably Python entrypoints).
        let raw_args = serde_json::json!({"command": command}).to_string();
        return astra_turn_core::evaluation::normalize_validation_prefix(name, &raw_args).is_some();
    }
    matches!(
        name,
        "read_file"
            | "read_metadata"
            | "list_dir"
            | "glob"
            | "grep"
            | "find_definition"
            | "find_references"
            | "lsp"
            | "git_diff"
            | "git_status"
            | "inspect_file"
    )
}

/// True for tools whose mutation status depends on arguments rather than the
/// tool name alone, e.g. `bash` command text or consolidated `git` action.
pub(crate) fn tool_classified_from_arguments(name: &str) -> bool {
    matches!(
        cloud_gated_tool_kind(name),
        Some(CloudGatedToolKind::Execute)
    ) || matches!(name, "git" | "github")
}

/// True when a successful call with this tool name is known to invalidate
/// cached read-only results without inspecting arguments.
pub(crate) fn tool_name_invalidates_read_cache(name: &str) -> bool {
    if tool_classified_from_arguments(name) {
        return false;
    }
    matches!(cloud_gated_tool_kind(name), Some(CloudGatedToolKind::Write))
        || NON_CLOUD_MUTATION_TOOLS.contains(&name)
}

/// True when a successful tool call should evict read-only idempotency cache
/// entries. Argument-classified tools require their mutating argument form.
pub fn tool_call_invalidates_read_cache(name: &str, args: Option<&Value>) -> bool {
    if tool_name_invalidates_read_cache(name) {
        return true;
    }
    if !tool_classified_from_arguments(name) {
        return false;
    }
    if name == "git" && git_action_is(args, "worktree") {
        return git_worktree_action_invalidates_read_cache(args);
    }
    match cloud_gated_tool_kind_with_args(name, args) {
        Some(CloudGatedToolKind::Write) => return true,
        Some(CloudGatedToolKind::Execute) => {}
        None => return false,
    }
    if matches!(
        cloud_gated_tool_kind(name),
        Some(CloudGatedToolKind::Execute)
    ) {
        return args
            .and_then(astra_turn_core::tool_argument_hints::command_hint_from_args)
            // The permission classifier is a conservative allowlist of
            // commands proven read-only. Cache/effect safety needs the same
            // boundary: an unknown executable may mutate even when a small
            // mutation-prefix list does not recognize it.
            .is_some_and(|command| {
                !astra_turn_core::cloud_approval_policy::bash_command_is_read_only(command)
            });
    }
    false
}

/// Conservative execution-boundary predicate for any state mutation, not just
/// files in the bound workspace. A started cancellation of one of these calls
/// can leave local, external, control-plane, or lifecycle state uncertain and
/// therefore must participate in terminal settlement.
pub fn tool_call_may_mutate_any_state(name: &str, args: Option<&Value>) -> bool {
    astra_turn_core::tool::categories::classify(name, args)
        .category
        .is_mutating()
        || tool_call_invalidates_read_cache(name, args)
}

fn git_action_is(args: Option<&Value>, expected: &str) -> bool {
    args.and_then(|args| args.get("action"))
        .and_then(Value::as_str)
        == Some(expected)
}

fn git_worktree_action_invalidates_read_cache(args: Option<&Value>) -> bool {
    let Some(sub_action) = args
        .and_then(|args| args.get("action"))
        .and_then(Value::as_str)
    else {
        // Conservative by design: missing `action` still evicts so we prefer
        // extra cache misses over serving stale read-only results.
        return true;
    };
    if sub_action != "worktree" {
        return false;
    }
    let Some(sub_action) = args
        .and_then(|args| args.get("sub_action"))
        .and_then(Value::as_str)
    else {
        return true;
    };
    !matches!(sub_action, "list" | "ls")
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
        for &name in CLOUD_APPROVAL_REQUIRED_TOOLS.iter() {
            match cloud_gated_tool_kind(name) {
                Some(CloudGatedToolKind::Write) if tool_classified_from_arguments(name) => {
                    assert!(
                        !tool_name_invalidates_read_cache(name),
                        "args-aware write tool must not invalidate by name alone: {name}"
                    );
                }
                Some(CloudGatedToolKind::Write) => {
                    assert!(
                        tool_name_invalidates_read_cache(name),
                        "write-gated tool must invalidate read cache: {name}"
                    );
                }
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
    fn consolidated_git_and_github_invalidate_read_cache_by_action() {
        assert!(!tool_name_invalidates_read_cache("git"));
        assert!(!tool_name_invalidates_read_cache("github"));

        assert!(!tool_call_invalidates_read_cache(
            "git",
            Some(&serde_json::json!({"action": "diff"}))
        ));
        assert!(tool_call_invalidates_read_cache(
            "git",
            Some(&serde_json::json!({"action": "commit", "message": "ship"}))
        ));
        assert!(!tool_call_invalidates_read_cache(
            "github",
            Some(&serde_json::json!({"action": "list_prs"}))
        ));
        assert!(tool_call_invalidates_read_cache(
            "github",
            Some(&serde_json::json!({"action": "create_issue", "title": "bug"}))
        ));
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
        for &name in astra_turn_core::headless_tool_assembly::READ_ONLY_TOOLS.iter() {
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

    /// Guards the shared conservative boundary between permission and cache
    /// invalidation. Commands not proven read-only must evict read evidence;
    /// extra cache misses are preferable to stale evidence or effect leaks.
    ///
    /// Invariant: if the permission gate approves a command as read-only, the
    /// cache-eviction gate must **not** classify it as mutating. Otherwise a
    /// command could be approved silently yet still evict the idempotency
    /// cache (or, worse, approved silently AND skip eviction for a real
    /// mutation). A shared corpus makes future regressions visible.
    #[test]
    fn read_only_permission_implies_non_mutating_cache_classification() {
        use astra_turn_core::cloud_approval_policy::bash_command_is_read_only;

        // Corpus spans positive and negative cases across both gates.
        let corpus = [
            // read-only shapes
            "ls",
            "cat foo.rs",
            "sed -n '1,20p' a.rs",
            "grep -r pattern .",
            "git status",
            "git log --oneline",
            "rg pattern",
            "rg pattern 2>&1 | head -50",
            "cd /tmp && cat file.txt",
            "echo '2>/dev/null'",
            "echo '>'",
            "echo 'apply_patch'",
            "echo 'rm file; git commit is prose'",
            // fd-redirect variants — every benign form the normalizer strips
            // should remain read-only on the permission gate AND non-mutating
            // on the cache gate. Regression corpus for `strip_benign_fd_redirects`.
            "rg pattern 1>&2",
            "rg pattern 2>/dev/null",
            "rg pattern 1>/dev/null",
            "rg pattern >/dev/null",
            "rg pattern &>/dev/null",
            // mutating shapes (both gates should agree these are NOT read-only,
            // and the cache gate SHOULD flag them as mutating)
            "rm file.txt",
            "sed -i 's/a/b/' foo.rs",
            "echo hi > foo.txt",
            "cd /tmp && mv x y",
            "npm install react",
            // Left-boundary regression: `a2` is an echo arg, `>` is a real
            // stdout redirect — must be classified as mutating on both gates.
            "echo a2>/tmp/x",
            "echo a2>>/tmp/x",
        ];

        // Commands that genuinely mutate the workspace (redirect, in-place
        // edit, VCS state changes, rm/mv/install). Permission and cache/effect
        // boundaries MUST agree on this corpus.
        let both_gates_mutation_corpus = [
            "rm file.txt",
            "sed -i 's/a/b/' foo.rs",
            "echo hi > foo.txt",
            "cd /tmp && mv x y",
            "npm install react",
            "git add .",
            "git checkout review-ref -- .",
            "git reset --hard HEAD~1",
            "cargo check &>> /tmp/unused_log",
            "cargo check 2> /tmp/git_commit_trace.log",
            "cargo check 2>> /tmp/rm_me.log",
            // Left-boundary regression — `a2` is echo's arg, `>` is a real
            // stdout redirect. Must be flagged mutating on BOTH gates.
            "echo a2>/tmp/x",
            "echo a2>>/tmp/x",
        ];

        for cmd in corpus {
            if bash_command_is_read_only(cmd) {
                assert!(
                    !tool_call_invalidates_read_cache(
                        "bash",
                        Some(&serde_json::json!({"command": cmd}))
                    ),
                    "permission-proven read-only command must preserve read cache: {cmd:?}"
                );
            }
        }

        for cmd in both_gates_mutation_corpus {
            assert!(
                !bash_command_is_read_only(cmd),
                "dual-gate mutation corpus regressed: permission gate now allows: {cmd:?}"
            );
            assert!(
                tool_call_invalidates_read_cache(
                    "bash",
                    Some(&serde_json::json!({"command": cmd}))
                ),
                "dual-gate mutation corpus regressed: effect/cache gate no longer flags: {cmd:?}"
            );
        }
    }

    #[test]
    fn historical_review_read_commands_do_not_look_like_mutations() {
        for command in [
            "cd \"$(git rev-parse --show-toplevel 2>/dev/null || echo .)\" && git show 449b13b95f56f57619094fbb8afbc496d31dd7a8:crates/services/src/storage.rs | sed -n '8580,8620p'",
            "cd \"$(git rev-parse --show-toplevel 2>/dev/null || echo .)\" && git show 449b13b95f56f57619094fbb8afbc496d31dd7a8:crates/runtime/src/turn/agentic_loop/host.rs | grep -n foo | head -60",
            "cd \"$(git rev-parse --show-toplevel)\"; awk 'NR>=5380 && NR<=5470' crates/runtime/src/turn/agentic_loop/execution_phase.rs | grep -n foo",
        ] {
            let args = serde_json::json!({"command": command});
            assert!(
                !tool_call_records_workspace_mutation("bash", Some(&args)),
                "read-only review command was classified as mutation: {command}"
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
        assert!(tool_classified_from_arguments("git"));
        assert!(tool_call_invalidates_read_cache(
            "git",
            Some(
                &serde_json::json!({"action": "worktree", "sub_action": "add", "branch": "feature"})
            )
        ));
        assert!(!tool_call_invalidates_read_cache(
            "git",
            Some(&serde_json::json!({"action": "worktree", "sub_action": "list"}))
        ));
    }

    #[test]
    fn workspace_mutation_is_distinct_from_other_state_effects() {
        for (name, args) in [
            ("memory", serde_json::json!({"action": "remember"})),
            ("github", serde_json::json!({"action": "create_issue"})),
            ("propose_work_plan", serde_json::json!({"additions": []})),
            ("agent", serde_json::json!({"action": "start"})),
        ] {
            assert!(
                !tool_call_may_mutate_workspace(name, Some(&args)),
                "non-workspace state write must not count as a workspace mutation: {name}"
            );
        }

        for (name, args) in [
            ("write_file", serde_json::json!({"path": "src/lib.rs"})),
            ("rename_symbol", serde_json::json!({"symbol": "old"})),
            ("git", serde_json::json!({"action": "checkout_file"})),
            (
                "bash",
                serde_json::json!({"command": "git checkout -- src"}),
            ),
        ] {
            assert!(
                tool_call_may_mutate_workspace(name, Some(&args)),
                "direct workspace mutation must be recognized: {name}"
            );
        }

        for (name, args) in [
            ("read_file", serde_json::json!({"path": "src/lib.rs"})),
            ("git", serde_json::json!({"action": "diff"})),
            (
                "git",
                serde_json::json!({"action": "worktree", "sub_action": "list"}),
            ),
            (
                "bash",
                serde_json::json!({"command": "git show HEAD:src/lib.rs"}),
            ),
        ] {
            assert!(
                !tool_call_may_mutate_workspace(name, Some(&args)),
                "read-only evidence must remain admissible: {name}"
            );
        }

        // Admission remains conservative for an opaque shell command, while
        // the completion ledger must not turn that possibility into a
        // mutation receipt.
        let opaque = serde_json::json!({
            "command": "python3 -c 'from pathlib import Path; Path(\"src/lib.rs\").write_text(\"x\")'"
        });
        assert!(tool_call_may_mutate_workspace("bash", Some(&opaque)));
        assert!(!tool_call_records_workspace_mutation("bash", Some(&opaque)));
        assert!(!tool_call_may_observe_workspace("bash", Some(&opaque)));

        // Python test entrypoints are a positive validation receipt even
        // though the permission classifier does not bless unknown prefixes.
        let pytest = serde_json::json!({"command": "python3 -m pytest tests/test_knot.py"});
        assert!(tool_call_may_mutate_workspace("bash", Some(&pytest)));
        assert!(!tool_call_records_workspace_mutation("bash", Some(&pytest)));
        assert!(tool_call_may_observe_workspace("bash", Some(&pytest)));

        let cython_build = serde_json::json!({
            "command": "python setup.py build_ext --inplace 2>&1 | tail -20"
        });
        assert!(tool_call_may_mutate_workspace("bash", Some(&cython_build)));
        assert!(!tool_call_records_workspace_mutation(
            "bash",
            Some(&cython_build)
        ));
        assert!(tool_call_may_observe_workspace("bash", Some(&cython_build)));

        // A known shell mutation is a receipt, not an observation.
        let edit = serde_json::json!({"command": "sed -i 's/old/new/' src/lib.rs"});
        assert!(tool_call_records_workspace_mutation("bash", Some(&edit)));
        assert!(!tool_call_may_observe_workspace("bash", Some(&edit)));

        // A compound shell call can mutate first and then provide a concrete
        // read/endpoint receipt.  The receipt must be order-sensitive: a
        // read before a later mutation is not post-mutation evidence.
        let compound = serde_json::json!({
            "command": "rm -rf build && git push origin main && cat dist/index.html && curl -sk https://localhost/"
        });
        assert!(tool_call_may_observe_workspace("bash", Some(&compound)));
        let stale = serde_json::json!({
            "command": "cat dist/index.html && rm -rf build"
        });
        assert!(!tool_call_may_observe_workspace("bash", Some(&stale)));
    }
}
