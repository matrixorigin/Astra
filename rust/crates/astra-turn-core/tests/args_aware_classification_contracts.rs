//! Cross-system contract tests for args-aware tool classification.
//!
//! These tests verify that the classification → partition → approval →
//! speculation pipeline stays consistent when bash commands carry
//! read-only vs mutating arguments. This is the cloud-edge advantage
//! over Claude Code: `bash "git status"` runs in parallel without
//! approval while `bash "rm -rf"` is serialized and gated.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use astra_turn_core::cloud_approval_policy::{
    CloudGatedToolKind, cloud_gated_tool_kind_with_args, edge_tool_requires_cloud_approval,
    edge_tool_requires_cloud_approval_with_args,
};
use astra_turn_core::parallel_tool_exec::{
    ToolExecutorFn, execute_parallel_round, is_read_only_tool, is_read_only_tool_with_args,
    partition_tool_calls,
};
use astra_turn_core::streaming_tool_exec::should_speculate;
use astra_turn_core::tool_categories::{ToolCategory, classify, classify_name};
use serde_json::{Value, json};

// ── Helpers ─────────────────────────────────────────────────────────────

fn tc(name: &str, id: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": { "name": name, "arguments": "{}" }
    })
}

fn tc_bash(id: &str, command: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": "bash",
            "arguments": json!({"command": command}).to_string()
        }
    })
}

fn tc_bash_obj_args(id: &str, command: &str) -> Value {
    json!({
        "id": id,
        "type": "function",
        "function": {
            "name": "bash",
            "arguments": {"command": command}
        }
    })
}

// ── Scenario 1: Full pipeline consistency for read-only bash ────────────

/// The entire pipeline must agree: classify, partition, approval, and
/// speculation all treat `bash "git status"` as read-only.
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

    // 4. speculation says eligible
    assert!(should_speculate("bash", Some(&args), None));
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
    assert!(!should_speculate("bash", Some(&args), None));
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
    assert!(!should_speculate("bash", None, None));
}

// ── Scenario 2: Mixed bash batch — real-world agentic turn ──────────────

/// Simulate an agentic investigation turn: the LLM emits 6 tool calls
/// in one batch — 3 read-only bash, 1 read_file, 1 mutating bash, 1 edit.
/// The partition must be 4 parallel + 2 sequential.
#[test]
fn mixed_agentic_batch_partition() {
    let calls = vec![
        tc_bash("1", "git status"),
        tc("read_file", "2"),
        tc_bash("3", "cargo check 2>&1 | head -50"),
        tc_bash("4", "git diff HEAD"),
        tc_bash("5", "cargo build --release"),
        tc("str_replace", "6"),
    ];

    let (ro, mut_) = partition_tool_calls(&calls);

    assert_eq!(
        ro.len(),
        4,
        "git status + read_file + cargo check + git diff"
    );
    assert_eq!(mut_.len(), 2, "cargo build + str_replace");

    // Verify indices
    assert_eq!(ro[0].0, 0); // bash git status
    assert_eq!(ro[1].0, 1); // read_file
    assert_eq!(ro[2].0, 2); // bash cargo check
    assert_eq!(ro[3].0, 3); // bash git diff
    assert_eq!(mut_[0].0, 4); // bash cargo build
    assert_eq!(mut_[1].0, 5); // str_replace
}

/// Same batch but with object-style arguments (not JSON strings).
#[test]
fn mixed_batch_with_object_args() {
    let calls = vec![
        tc_bash_obj_args("1", "ls -la"),
        tc_bash_obj_args("2", "git push origin main"),
        tc("grep", "3"),
    ];

    let (ro, mut_) = partition_tool_calls(&calls);
    assert_eq!(ro.len(), 2, "ls + grep");
    assert_eq!(mut_.len(), 1, "git push");
}

// ── Scenario 3: Wall-clock parallelism for bash read-only ───────────────

/// 4 bash read-only commands (each 200ms) must complete in < 500ms when
/// dispatched through execute_parallel_round — proving args-aware
/// classification enables real parallel execution.
#[tokio::test]
async fn bash_read_only_commands_run_in_parallel() {
    let counter = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    let m = max_concurrent.clone();

    let executor: ToolExecutorFn = Arc::new(move |tc_value: Value| {
        let c = c.clone();
        let m = m.clone();
        Box::pin(async move {
            let cur = c.fetch_add(1, Ordering::SeqCst) + 1;
            m.fetch_max(cur, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(200)).await;
            c.fetch_sub(1, Ordering::SeqCst);
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            (call_id, name, "ok".into(), true)
        })
    });

    let calls = vec![
        tc_bash("1", "git status"),
        tc_bash("2", "ls -la"),
        tc_bash("3", "cargo check 2>&1"),
        tc_bash("4", "grep -r TODO ."),
    ];

    let started = Instant::now();
    let outcome = execute_parallel_round(&calls, executor).await;
    let elapsed = started.elapsed();

    assert_eq!(outcome.parallel_count, 4);
    assert_eq!(outcome.sequential_count, 0);
    assert!(
        elapsed < Duration::from_millis(500),
        "4 parallel bash commands should finish in < 500ms, took {elapsed:?}"
    );
    assert!(
        max_concurrent.load(Ordering::SeqCst) > 1,
        "expected parallel execution, max concurrent = {}",
        max_concurrent.load(Ordering::SeqCst)
    );
}

/// Mix of read-only bash + mutating bash: read-only run in parallel
/// (phase 1), mutating run after (phase 2).
#[tokio::test]
async fn mixed_bash_parallel_then_sequential() {
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_c = ran.clone();

    let executor: ToolExecutorFn = Arc::new(move |tc_value: Value| {
        let ran = ran_c.clone();
        Box::pin(async move {
            ran.fetch_add(1, Ordering::SeqCst);
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            (call_id, name, "ok".into(), true)
        })
    });

    let calls = vec![
        tc_bash("1", "git status"),  // read-only → parallel
        tc_bash("2", "ls"),          // read-only → parallel
        tc_bash("3", "cargo build"), // mutating → sequential
        tc_bash("4", "git push"),    // mutating → sequential
    ];

    let outcome = execute_parallel_round(&calls, executor).await;
    assert_eq!(outcome.parallel_count, 2);
    assert_eq!(outcome.sequential_count, 2);
    assert_eq!(outcome.results.len(), 4);
    // All ran
    assert_eq!(ran.load(Ordering::SeqCst), 4);
}

// ── Scenario 4: Sibling abort with args-aware classification ────────────

/// A mutating bash error aborts subsequent mutations, but read-only bash
/// commands (which ran in Phase 1) are already complete.
#[tokio::test]
async fn sibling_abort_respects_args_aware_partition() {
    let ran = Arc::new(AtomicUsize::new(0));
    let ran_c = ran.clone();

    let executor: ToolExecutorFn = Arc::new(move |tc_value: Value| {
        let ran = ran_c.clone();
        Box::pin(async move {
            ran.fetch_add(1, Ordering::SeqCst);
            let call_id = tc_value["id"].as_str().unwrap_or("").to_string();
            let name = tc_value["function"]["name"]
                .as_str()
                .unwrap_or("")
                .to_string();
            // Parse the bash command to check if it's the failing one
            let args_str = tc_value["function"]["arguments"].as_str().unwrap_or("{}");
            let args: Value = serde_json::from_str(args_str).unwrap_or_default();
            let cmd = args["command"].as_str().unwrap_or("");
            let success = cmd != "cargo build"; // cargo build fails
            (call_id, name, format!("cmd={cmd}"), success)
        })
    });

    let calls = vec![
        tc_bash("1", "git status"),  // read-only → Phase 1 (succeeds)
        tc_bash("2", "ls"),          // read-only → Phase 1 (succeeds)
        tc_bash("3", "cargo build"), // mutating → Phase 2 (FAILS)
        tc_bash("4", "git push"),    // mutating → Phase 2 (ABORTED)
    ];

    let outcome = execute_parallel_round(&calls, executor).await;

    // Phase 1 read-only tools succeeded
    assert!(outcome.results[0].success, "git status should succeed");
    assert!(outcome.results[1].success, "ls should succeed");

    // Phase 2: cargo build failed, git push aborted
    assert!(!outcome.results[2].success, "cargo build should fail");
    assert!(!outcome.results[3].success, "git push should be aborted");
    assert!(outcome.results[3].content.contains("Aborted"));
    assert!(outcome.sibling_aborted);

    // Only 3 tools actually executed (2 read-only + 1 mutating before abort)
    assert_eq!(ran.load(Ordering::SeqCst), 3);
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

/// Piped read-only commands: `cargo check 2>&1 | head -50`
#[test]
fn piped_read_only_bash_pipeline() {
    let args = json!({"command": "cargo check 2>&1 | head -50"});
    let c = classify("bash", Some(&args));
    assert!(c.parallelizable);
    assert!(!c.approval_required);
    assert!(c.exploration);
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

// ── Scenario 8: BashTool alias consistency ─────────────────────────────

/// BashTool is an alias for bash — must behave identically.
#[test]
fn bashtool_alias_consistent_with_bash() {
    let commands = ["git status", "ls -la", "cargo build", "rm -rf /", ""];
    for cmd in commands {
        let args = json!({"command": cmd});
        let bash_c = classify("bash", Some(&args));
        let bashtool_c = classify("BashTool", Some(&args));
        assert_eq!(
            bash_c.parallelizable, bashtool_c.parallelizable,
            "BashTool vs bash parallelizable mismatch for {cmd:?}"
        );
        assert_eq!(
            bash_c.approval_required, bashtool_c.approval_required,
            "BashTool vs bash approval mismatch for {cmd:?}"
        );
        assert_eq!(
            bash_c.category, bashtool_c.category,
            "BashTool vs bash category mismatch for {cmd:?}"
        );
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
        assert!(c.never_restrict, "{name} should be never-restrict");
        assert!(c.exploration, "{name} should count as exploration");
        assert!(!c.compactable, "{name} should not be compactable");
    }
}
