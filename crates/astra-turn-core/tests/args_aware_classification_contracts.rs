//! Cross-system contract tests for args-aware tool classification.
//!
//! These tests verify that the classification → approval pipeline stays
//! consistent when bash commands carry read-only vs mutating arguments.

use astra_turn_core::cloud_approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind_with_args, edge_tool_requires_cloud_approval,
    edge_tool_requires_cloud_approval_with_args,
};
use astra_turn_core::parallel_tool_exec::{is_read_only_tool, is_read_only_tool_with_args};
use astra_turn_core::tool_categories::{ToolCategory, classify, classify_name};
use serde_json::json;

// ── Scenario 1: Full pipeline consistency for read-only bash ────────────

/// The classification and approval pipeline treat `bash "git status"` as read-only.
#[test]
fn pipeline_consistency_bash_git_status() {
    let args = json!({"command": "git status"});

    // 1. classify says ReadOnly + parallelizable + no approval
    let c = classify("bash", Some(&args));
    assert_eq!(c.category, ToolCategory::ReadOnly);
    assert!(c.parallelizable);
    assert!(!c.approval_required);
    assert!(c.compactable);
    assert!(c.exploration);

    // 2. parallel_tool_exec says parallelizable
    assert!(is_read_only_tool_with_args("bash", Some(&args)));

    // 3. cloud approval says no approval needed
    assert!(!edge_tool_requires_cloud_approval_with_args(
        "bash",
        Some(&args)
    ));
    assert_eq!(cloud_gated_tool_kind_with_args("bash", Some(&args)), None);
}

/// The entire pipeline must agree: `bash "rm -rf /"` is mutating.
#[test]
fn pipeline_consistency_bash_rm() {
    let args = json!({"command": "rm -rf /"});

    let c = classify("bash", Some(&args));
    assert_eq!(c.category, ToolCategory::Shell);
    assert!(!c.parallelizable);
    assert!(c.approval_required);
    assert!(!c.compactable);

    assert!(!is_read_only_tool_with_args("bash", Some(&args)));
    assert!(edge_tool_requires_cloud_approval_with_args(
        "bash",
        Some(&args)
    ));
    assert_eq!(
        cloud_gated_tool_kind_with_args("bash", Some(&args)),
        Some(CloudGatedToolKind::Execute)
    );
}

/// bash without args: fail-closed across the entire pipeline.
#[test]
fn pipeline_consistency_bash_no_args() {
    let c = classify("bash", None);
    assert_eq!(c.category, ToolCategory::Shell);
    assert!(!c.parallelizable);
    assert!(c.approval_required);

    assert!(!is_read_only_tool("bash"));
    assert!(edge_tool_requires_cloud_approval("bash"));
}

// ── Scenario 5: Cloud approval bypass savings ───────────────────────────

/// Quantify the approval gate savings: for a batch of 5 bash commands,
/// 3 are read-only (skip approval) and 2 require it.
#[test]
fn cloud_approval_bypass_counts() {
    let commands = vec![
        ("git status", false),
        ("ls -la", false),
        ("grep -r TODO .", false),
        ("cargo build", true),
        ("git push origin main", true),
    ];

    let mut bypassed = 0;
    let mut required = 0;

    for (cmd, expected_required) in &commands {
        let args = json!({"command": cmd});
        let needs_approval = edge_tool_requires_cloud_approval_with_args("bash", Some(&args));
        assert_eq!(
            needs_approval, *expected_required,
            "bash {cmd}: expected approval_required={expected_required}, got {needs_approval}"
        );
        if needs_approval {
            required += 1;
        } else {
            bypassed += 1;
        }
    }

    assert_eq!(bypassed, 3, "3 read-only commands bypass approval");
    assert_eq!(required, 2, "2 mutating commands require approval");
}

// ── Scenario 6: classify_name vs classify consistency ───────────────────

/// classify_name(name) must produce identical results to classify(name, None)
/// for every tool in the registry.
#[test]
fn classify_name_equals_classify_none_for_all_tools() {
    let names = astra_turn_core::tool_categories::registry().canonical_names();
    for name in names {
        let cn = classify_name(name);
        let c = classify(name, None);
        assert_eq!(cn, c, "classify_name vs classify(None) mismatch for {name}");
    }
}

// ── Scenario 7: Edge cases ─────────────────────────────────────────────

/// cd-prefixed bash commands: `cd project && ls` is read-only.
#[test]
fn cd_prefixed_bash_pipeline() {
    let args = json!({"command": "cd project && ls -la"});
    let c = classify("bash", Some(&args));
    assert!(c.parallelizable, "cd && ls should be parallelizable");
    assert!(!c.approval_required, "cd && ls should skip approval");
}

/// Piped read-only commands remain parallelizable when every stage is
/// observational.
#[test]
fn piped_read_only_bash_pipeline() {
    let args = json!({"command": "rg TODO . 2>&1 | head -50"});
    let c = classify("bash", Some(&args));
    assert!(c.parallelizable);
    assert!(!c.approval_required);
    assert!(c.exploration);
}

#[test]
fn build_command_piped_to_reader_still_requires_approval() {
    let args = json!({"command": "cargo check 2>&1 | head -50"});
    let c = classify("bash", Some(&args));
    assert_eq!(c.category, ToolCategory::Shell);
    assert!(!c.parallelizable);
    assert!(c.approval_required);
}

/// Dangerous pipe: `ls | xargs rm` is mutating despite starting with ls.
#[test]
fn dangerous_pipe_detected() {
    let args = json!({"command": "ls | xargs rm"});
    let c = classify("bash", Some(&args));
    assert!(!c.parallelizable);
    assert!(c.approval_required);
}

/// Output redirection makes an otherwise read-only command mutating.
#[test]
fn output_redirect_detected() {
    let args = json!({"command": "ls > output.txt"});
    let c = classify("bash", Some(&args));
    assert!(!c.parallelizable);
    assert!(c.approval_required);
}

/// Empty command: fail-closed.
#[test]
fn empty_bash_command_fail_closed() {
    let args = json!({"command": ""});
    let c = classify("bash", Some(&args));
    assert!(!c.parallelizable);
    assert!(c.approval_required);
}

/// Non-bash tools ignore args: write_file with any args is still mutating.
#[test]
fn non_bash_ignores_command_arg() {
    let args = json!({"command": "git status", "file_path": "/tmp/x"});
    let c = classify("write_file", Some(&args));
    assert_eq!(c.category, ToolCategory::Mutating);
    assert!(!c.parallelizable);
    assert!(c.approval_required);
}

// ── Scenario 8: removed shell names do not inherit bash classification ───

#[test]
fn removed_bashtool_name_is_unknown_mutating() {
    let commands = ["git status", "ls -la", "cargo build", "rm -rf /", ""];
    for cmd in commands {
        let args = json!({"command": cmd});
        let c = classify("BashTool", Some(&args));
        assert_eq!(c.category, ToolCategory::Mutating, "{cmd:?}");
        assert!(!c.parallelizable, "{cmd:?}");
        assert!(!c.approval_required, "{cmd:?}");
        assert!(!c.compactable, "{cmd:?}");
    }
}

// ── Scenario 9: MCP tool always gated ──────────────────────────────────

/// MCP tools must always require approval, even with read-only-looking args.
#[test]
fn mcp_tool_always_gated_regardless_of_args() {
    let args = json!({"command": "ls"});
    assert!(edge_tool_requires_cloud_approval_with_args(
        "mcp_fs_read",
        Some(&args)
    ));
    assert_eq!(
        cloud_gated_tool_kind_with_args("mcp_fs_read", Some(&args)),
        Some(CloudGatedToolKind::Execute)
    );
    assert!(!is_read_only_tool_with_args("mcp_fs_read", Some(&args)));
}

// ── Scenario 10: Consultative tools pipeline ────────────────────────────

/// Consultative tools (skill, discover_skills) should be parallelizable
/// and explorable but not approval-required.
#[test]
fn consultative_tools_pipeline_consistency() {
    for name in ["skill", "discover_skills"] {
        let c = classify_name(name);
        assert_eq!(c.category, ToolCategory::Consultative, "{name}");
        assert!(c.parallelizable, "{name} should be parallelizable");
        assert!(!c.approval_required, "{name} should skip approval");
        assert!(
            !c.never_restrict,
            "{name} should be restrictable (stall avoidance)"
        );
        assert!(c.exploration, "{name} should count as exploration");
        assert!(!c.compactable, "{name} should not be compactable");
    }
}
