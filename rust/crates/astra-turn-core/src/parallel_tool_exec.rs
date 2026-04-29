//! Parallel Tool Execution (D-1)
//!
//! Classifies tool calls as read-only (safe for parallel execution) vs mutating
//! (must run sequentially).
//!
//! Read-only tools can be dispatched concurrently via `tokio::JoinSet` with
//! a semaphore limiting concurrency. Mutating tools run one at a time after
//! all read-only tools complete. If a bash tool errors, subsequent mutating
//! tools are aborted (sibling abort).
//!
//! This module provides the classification and orchestration logic. The actual
//! `execute_tool` function is supplied by the caller (CLI's stream_render.rs
//! or any other host).

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use tokio::sync::Semaphore;

/// Maximum number of read-only tools that can execute concurrently.
pub const MAX_CONCURRENT_READ_ONLY: usize = 10;

/// Alias used by the CLI-side batch path (`stream_render::execute_tools_batch`).
/// Kept equal to [`MAX_CONCURRENT_READ_ONLY`] so the in-turn cap matches
/// claude-code semantics (10) across both speculative and batched paths.
pub const MAX_CONCURRENT_TOOL_EXECUTIONS: usize = MAX_CONCURRENT_READ_ONLY;

/// Process-wide semaphore shared across every tool-execution batch.
///
/// Previously, each call to `execute_parallel_round` and each CLI batch in
/// `stream_render::execute_tools_batch` allocated its own `Semaphore::new(10)`.
/// That made the 10-concurrent cap **per batch**, not shared — N overlapping
/// batches allowed 10·N concurrent tools, contradicting the "shared semaphore"
/// contract stated in prompts and code comments. On large sessions or when
/// the runtime server multiplexes turns, this could saturate edge I/O or
/// exhaust file descriptors.
///
/// The single process-wide instance below enforces the true cap across all
/// batches, turns, and concurrent sessions sharing the same process.
///
/// Intended usage:
///
/// ```ignore
/// let permit = shared_tool_semaphore().acquire_owned().await?;
/// // run the tool, holding the permit
/// ```
pub fn shared_tool_semaphore() -> Arc<Semaphore> {
    static CELL: OnceLock<Arc<Semaphore>> = OnceLock::new();
    CELL.get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_TOOL_EXECUTIONS)))
        .clone()
}

// ───────────────────────────── Tool Classification ──────────────────────

/// Extract tool name from a tool call JSON value.
fn extract_tool_name(tc: &Value) -> &str {
    tc.get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
        .or_else(|| tc.get("name").and_then(|n| n.as_str()))
        .unwrap_or("")
}

/// Parse the `arguments` field from a tool call into a `serde_json::Value`.
/// Returns `None` if the field is missing or not parseable.
pub fn parse_tool_args(tc: &Value) -> Option<Value> {
    let raw = tc
        .get("function")
        .and_then(|f| f.get("arguments"))
        .or_else(|| tc.get("arguments"))?;

    match raw {
        Value::Object(_) => Some(raw.clone()),
        Value::String(s) => serde_json::from_str(s).ok(),
        _ => None,
    }
}

/// Classify a tool call as parallelizable using args-aware classification.
///
/// For shell tools (bash, BashTool), inspects the `command` argument to
/// determine if the command is read-only (e.g. `git status`, `ls`).
/// Falls back to the process-wide [`crate::concurrency_safety`] registry
/// for MCP / dynamic tools not in the static table.
pub fn is_read_only_tool(tool_name: &str) -> bool {
    if crate::tool_categories::classify_name(tool_name).parallelizable {
        return true;
    }
    crate::concurrency_safety::global_is_parallelizable(tool_name)
}

/// Args-aware variant: classify a tool call as parallelizable, inspecting
/// the command argument for shell tools.
pub fn is_read_only_tool_with_args(tool_name: &str, args: Option<&Value>) -> bool {
    if crate::tool_categories::classify(tool_name, args).parallelizable {
        return true;
    }
    crate::concurrency_safety::global_is_parallelizable(tool_name)
}

/// Iterate the canonical read-only tool names from the central registry.
pub fn read_only_tool_names() -> Vec<&'static str> {
    crate::tool_categories::registry().read_only_names()
}

/// Partition tool calls into (read_only, mutating) groups, preserving
/// original indices for result reassembly.
///
/// Uses args-aware classification: `bash "git status"` is partitioned as
/// read-only (parallelizable), while `bash "rm -rf /"` is mutating.
pub fn partition_tool_calls(tool_calls: &[Value]) -> (Vec<(usize, &Value)>, Vec<(usize, &Value)>) {
    let mut read_only = Vec::new();
    let mut mutating = Vec::new();

    for (i, tc) in tool_calls.iter().enumerate() {
        let name = extract_tool_name(tc);
        let args = parse_tool_args(tc);

        if is_read_only_tool_with_args(name, args.as_ref()) {
            read_only.push((i, tc));
        } else {
            mutating.push((i, tc));
        }
    }

    (read_only, mutating)
}

// ───────────────────────────── Execution Result ─────────────────────────

/// Result of a single tool execution.
#[derive(Debug, Clone)]
pub struct ToolExecResult {
    /// Original index in the tool_calls array.
    pub original_index: usize,
    /// Tool call ID (for matching with LLM response).
    pub call_id: String,
    /// Tool name.
    pub tool_name: String,
    /// The result content (success or error).
    pub content: String,
    /// Whether the tool execution succeeded.
    pub success: bool,
}

/// Outcome of a parallel tool round.
#[derive(Debug)]
pub struct ParallelRoundOutcome {
    /// All results, sorted by original index.
    pub results: Vec<ToolExecResult>,
    /// Number of tools that ran in parallel.
    pub parallel_count: usize,
    /// Number of tools that ran sequentially.
    pub sequential_count: usize,
    /// Whether a mutating tool (bash) error caused sibling abort.
    pub sibling_aborted: bool,
}

// ───────────────────────────── Orchestrator ──────────────────────────────

/// Type alias for the async tool executor function.
/// Takes a tool call Value and returns (call_id, tool_name, content, success).
pub type ToolExecutorFn = Arc<
    dyn Fn(Value) -> Pin<Box<dyn Future<Output = (String, String, String, bool)> + Send>>
        + Send
        + Sync,
>;

/// Execute a batch of tool calls with read-only parallelism.
///
/// 1. Read-only tools run concurrently (up to `MAX_CONCURRENT_READ_ONLY`).
/// 2. Mutating tools run sequentially after all read-only tools complete.
/// 3. If a bash/shell tool errors, remaining mutating tools are skipped.
/// 4. Results are returned in original order.
pub async fn execute_parallel_round(
    tool_calls: &[Value],
    executor: ToolExecutorFn,
) -> ParallelRoundOutcome {
    let (read_only, mutating) = partition_tool_calls(tool_calls);
    let total = tool_calls.len();
    let mut results: Vec<Option<ToolExecResult>> = vec![None; total];
    let mut sibling_aborted = false;

    // Phase 1: Execute read-only tools in parallel
    let parallel_count = read_only.len();
    if !read_only.is_empty() {
        let semaphore = shared_tool_semaphore();
        let mut join_set = tokio::task::JoinSet::new();

        for (idx, tc) in read_only {
            let tc_owned = tc.clone();
            let sem = semaphore.clone();
            let exec = executor.clone();
            join_set.spawn(async move {
                let _permit = match sem.acquire().await {
                    Ok(p) => p,
                    Err(_) => {
                        return ToolExecResult {
                            original_index: idx,
                            call_id: String::new(),
                            tool_name: String::new(),
                            content: "semaphore closed".into(),
                            success: false,
                        };
                    }
                };
                // Wrap execution in catch_unwind so panics produce an error
                // result with the correct original_index preserved.
                let res = tokio::task::spawn(async move {
                    let (call_id, tool_name, content, success) = exec(tc_owned).await;
                    ToolExecResult {
                        original_index: idx,
                        call_id,
                        tool_name,
                        content,
                        success,
                    }
                })
                .await;
                match res {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!("[parallel_tool_exec] tool task panicked: {e}");
                        ToolExecResult {
                            original_index: idx,
                            call_id: String::new(),
                            tool_name: String::new(),
                            content: format!("internal error: task panicked: {e}"),
                            success: false,
                        }
                    }
                }
            });
        }

        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(r) => {
                    let idx = r.original_index;
                    results[idx] = Some(r);
                }
                Err(e) => {
                    eprintln!("[parallel_tool_exec] outer task panicked: {e}");
                }
            }
        }
    }

    // Phase 2: Execute mutating tools sequentially.
    // Sibling-abort fires on ANY mutating-tool failure (not just bash): a
    // batched sequence of mutations is typically a coherent plan (write →
    // commit → push) where later steps become meaningless once an earlier
    // one fails, and continuing can partially apply destructive state.
    let sequential_count = mutating.len();
    let mut mutating_executed: usize = 0;
    let mut aborted_count: usize = 0;
    let mut trigger_tool: Option<String> = None;
    let mut trigger_position: Option<usize> = None;

    for (mut_pos, (idx, tc)) in mutating.into_iter().enumerate() {
        if sibling_aborted {
            let call_id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool_name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| tc.get("name").and_then(|n| n.as_str()))
                .unwrap_or("")
                .to_string();
            aborted_count += 1;
            results[idx] = Some(ToolExecResult {
                original_index: idx,
                call_id,
                tool_name,
                content: "Aborted: a prior tool in this batch failed.".to_string(),
                success: false,
            });
            continue;
        }

        let (call_id, tool_name, content, success) = executor(tc.clone()).await;
        mutating_executed += 1;

        // Sibling abort: any mutating-tool failure aborts remaining siblings.
        if !success {
            sibling_aborted = true;
            trigger_tool = Some(tool_name.clone());
            trigger_position = Some(mut_pos);
        }

        results[idx] = Some(ToolExecResult {
            original_index: idx,
            call_id,
            tool_name,
            content,
            success,
        });
    }

    // Structured signal for the "signals to watch" section of
    // docs/design/sibling-abort-policy.md. One event per round lets log
    // aggregation answer: how often are mutating batches ≥ 2? which tool
    // triggers aborts? at what position (early vs late) does the trigger
    // sit in the queue?
    tracing::info!(
        target: "astra::parallel_tool_exec::round",
        parallel_count,
        sequential_count,
        mutating_executed,
        aborted_count,
        sibling_aborted,
        trigger_tool = trigger_tool.as_deref().unwrap_or(""),
        trigger_position = trigger_position
            .map(|p| p as i64)
            .unwrap_or(-1),
        "parallel_tool_exec round completed"
    );

    // Fill any remaining None entries with synthetic error results.
    // This handles the case where an outer JoinSet task panicked and
    // the result was lost — the LLM must receive a result for every tool call.
    for (idx, slot) in results.iter_mut().enumerate() {
        if slot.is_none() {
            let tc = &tool_calls[idx];
            let call_id = tc
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let tool_name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .or_else(|| tc.get("name").and_then(|n| n.as_str()))
                .unwrap_or("")
                .to_string();
            *slot = Some(ToolExecResult {
                original_index: idx,
                call_id,
                tool_name,
                content: "internal error: tool task panicked and result was lost".into(),
                success: false,
            });
        }
    }

    ParallelRoundOutcome {
        results: results.into_iter().flatten().collect(),
        parallel_count,
        sequential_count,
        sibling_aborted,
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn make_tool_call(name: &str, id: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {
                "name": name,
                "arguments": "{}"
            }
        })
    }

    fn make_executor(delay_ms: u64) -> ToolExecutorFn {
        Arc::new(move |tc: Value| {
            Box::pin(async move {
                if delay_ms > 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                }
                let call_id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                (call_id, name.clone(), format!("result of {}", name), true)
            })
        })
    }

    fn make_failing_bash_executor() -> ToolExecutorFn {
        Arc::new(|tc: Value| {
            Box::pin(async move {
                let call_id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let success = name != "bash"; // bash fails, others succeed
                (
                    call_id,
                    name.clone(),
                    format!("result of {}", name),
                    success,
                )
            })
        })
    }

    #[test]
    fn read_only_classification() {
        assert!(is_read_only_tool("read_file"));
        assert!(is_read_only_tool("grep"));
        assert!(is_read_only_tool("glob"));
        assert!(is_read_only_tool("web_fetch"));
        assert!(!is_read_only_tool("bash"));
        assert!(!is_read_only_tool("write_file"));
        assert!(!is_read_only_tool("str_replace"));
        assert!(!is_read_only_tool("unknown_tool"));
    }

    #[test]
    fn partition_separates_correctly() {
        let calls = vec![
            make_tool_call("read_file", "1"),
            make_tool_call("bash", "2"),
            make_tool_call("grep", "3"),
            make_tool_call("write_file", "4"),
        ];
        let (ro, mut_) = partition_tool_calls(&calls);
        assert_eq!(ro.len(), 2); // read_file, grep
        assert_eq!(mut_.len(), 2); // bash, write_file
        assert_eq!(ro[0].0, 0); // read_file at index 0
        assert_eq!(ro[1].0, 2); // grep at index 2
        assert_eq!(mut_[0].0, 1); // bash at index 1
        assert_eq!(mut_[1].0, 3); // write_file at index 3
    }

    #[tokio::test]
    async fn parallel_round_all_read_only() {
        let calls = vec![
            make_tool_call("read_file", "1"),
            make_tool_call("grep", "2"),
            make_tool_call("glob", "3"),
        ];
        let outcome = execute_parallel_round(&calls, make_executor(10)).await;
        assert_eq!(outcome.results.len(), 3);
        assert_eq!(outcome.parallel_count, 3);
        assert_eq!(outcome.sequential_count, 0);
        assert!(!outcome.sibling_aborted);
        // Results should be in original order
        assert_eq!(outcome.results[0].original_index, 0);
        assert_eq!(outcome.results[1].original_index, 1);
        assert_eq!(outcome.results[2].original_index, 2);
    }

    #[tokio::test]
    async fn parallel_round_mixed() {
        let calls = vec![
            make_tool_call("read_file", "1"),
            make_tool_call("bash", "2"),
            make_tool_call("grep", "3"),
        ];
        let outcome = execute_parallel_round(&calls, make_executor(0)).await;
        assert_eq!(outcome.results.len(), 3);
        assert_eq!(outcome.parallel_count, 2);
        assert_eq!(outcome.sequential_count, 1);
    }

    #[tokio::test]
    async fn parallel_round_preserves_order() {
        let calls = vec![
            make_tool_call("bash", "1"),
            make_tool_call("read_file", "2"),
            make_tool_call("write_file", "3"),
            make_tool_call("grep", "4"),
        ];
        let outcome = execute_parallel_round(&calls, make_executor(0)).await;
        for (i, r) in outcome.results.iter().enumerate() {
            assert_eq!(r.original_index, i);
        }
    }

    #[tokio::test]
    async fn bash_error_aborts_siblings() {
        let calls = vec![
            make_tool_call("bash", "1"),       // fails
            make_tool_call("write_file", "2"), // should be aborted
        ];
        let outcome = execute_parallel_round(&calls, make_failing_bash_executor()).await;
        assert!(outcome.sibling_aborted);
        assert!(!outcome.results[0].success); // bash failed
        assert!(!outcome.results[1].success); // write_file aborted
        assert!(outcome.results[1].content.contains("Aborted"));
    }

    #[tokio::test]
    async fn read_only_tools_actually_parallel() {
        let counter = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));

        let c = counter.clone();
        let m = max_concurrent.clone();
        let executor: ToolExecutorFn = Arc::new(move |tc: Value| {
            let c = c.clone();
            let m = m.clone();
            Box::pin(async move {
                let current = c.fetch_add(1, Ordering::SeqCst) + 1;
                m.fetch_max(current, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                c.fetch_sub(1, Ordering::SeqCst);
                let call_id = tc["id"].as_str().unwrap_or("").to_string();
                (call_id, "read_file".into(), "ok".into(), true)
            })
        });

        let calls: Vec<Value> = (0..5)
            .map(|i| make_tool_call("read_file", &format!("{}", i)))
            .collect();

        let _outcome = execute_parallel_round(&calls, executor).await;

        // If truly parallel, max concurrent should be > 1
        assert!(
            max_concurrent.load(Ordering::SeqCst) > 1,
            "Expected parallel execution, but max concurrent was {}",
            max_concurrent.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn empty_round() {
        let calls: Vec<Value> = vec![];
        let outcome = execute_parallel_round(&calls, make_executor(0)).await;
        assert_eq!(outcome.results.len(), 0);
        assert_eq!(outcome.parallel_count, 0);
        assert_eq!(outcome.sequential_count, 0);
    }

    /// P1-E: Results count must always equal tool_calls count, even if a task panics.
    /// Verifies that panicked tasks produce synthetic error results instead of being dropped.
    #[tokio::test]
    async fn result_count_equals_tool_call_count() {
        let calls = vec![
            json!({"id": "c1", "function": {"name": "read_file", "arguments": "{}"}}),
            json!({"id": "c2", "function": {"name": "read_file", "arguments": "{}"}}),
            json!({"id": "c3", "function": {"name": "read_file", "arguments": "{}"}}),
        ];
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_clone = counter.clone();
        let exec: ToolExecutorFn = Arc::new(move |_tc| {
            let n = counter_clone.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if n == 1 {
                    (
                        "c2".into(),
                        "read_file".into(),
                        "error: file not found".into(),
                        false,
                    )
                } else {
                    ("ok".into(), "read_file".into(), "content".into(), true)
                }
            })
        });
        let outcome = execute_parallel_round(&calls, exec).await;
        assert_eq!(
            outcome.results.len(),
            calls.len(),
            "result count must equal tool_call count — no results may be silently dropped"
        );
    }

    // ── Args-aware classification tests ──────────────────────────────────

    fn make_bash_call(id: &str, command: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": json!({"command": command}).to_string()
            }
        })
    }

    fn make_bash_call_parsed_args(id: &str, command: &str) -> Value {
        json!({
            "id": id,
            "type": "function",
            "function": {
                "name": "bash",
                "arguments": {"command": command}
            }
        })
    }

    #[test]
    fn args_aware_bash_git_status_is_read_only() {
        let args = json!({"command": "git status"});
        assert!(is_read_only_tool_with_args("bash", Some(&args)));
    }

    #[test]
    fn args_aware_bash_rm_is_not_read_only() {
        let args = json!({"command": "rm -rf /"});
        assert!(!is_read_only_tool_with_args("bash", Some(&args)));
    }

    #[test]
    fn args_aware_bash_no_args_still_mutating() {
        assert!(!is_read_only_tool_with_args("bash", None));
    }

    #[test]
    fn args_aware_non_shell_ignores_args() {
        let args = json!({"command": "git status"});
        assert!(!is_read_only_tool_with_args("write_file", Some(&args)));
        assert!(is_read_only_tool_with_args("read_file", Some(&args)));
    }

    #[test]
    fn parse_tool_args_string_arguments() {
        let tc = json!({
            "function": {
                "name": "bash",
                "arguments": "{\"command\": \"git status\"}"
            }
        });
        let parsed = parse_tool_args(&tc).unwrap();
        assert_eq!(parsed["command"], "git status");
    }

    #[test]
    fn parse_tool_args_object_arguments() {
        let tc = json!({
            "function": {
                "name": "bash",
                "arguments": {"command": "ls -la"}
            }
        });
        let parsed = parse_tool_args(&tc).unwrap();
        assert_eq!(parsed["command"], "ls -la");
    }

    #[test]
    fn parse_tool_args_missing_returns_none() {
        let tc = json!({"function": {"name": "bash"}});
        assert!(parse_tool_args(&tc).is_none());
    }

    #[test]
    fn partition_bash_git_status_is_parallel() {
        let calls = vec![
            make_tool_call("read_file", "1"),
            make_bash_call("2", "git status"),
            make_tool_call("grep", "3"),
            make_bash_call("4", "rm -rf /"),
        ];
        let (ro, mut_) = partition_tool_calls(&calls);
        assert_eq!(ro.len(), 3, "read_file + bash(git status) + grep");
        assert_eq!(mut_.len(), 1, "bash(rm) only");
        assert_eq!(ro[0].0, 0); // read_file
        assert_eq!(ro[1].0, 1); // bash "git status"
        assert_eq!(ro[2].0, 2); // grep
        assert_eq!(mut_[0].0, 3); // bash "rm -rf /"
    }

    #[test]
    fn partition_bash_with_parsed_object_args() {
        let calls = vec![
            make_bash_call_parsed_args("1", "ls -la"),
            make_bash_call_parsed_args("2", "git push origin main"),
        ];
        let (ro, mut_) = partition_tool_calls(&calls);
        assert_eq!(ro.len(), 1);
        assert_eq!(mut_.len(), 1);
        assert_eq!(ro[0].0, 0); // ls
        assert_eq!(mut_[0].0, 1); // git push
    }

    #[test]
    fn partition_mixed_batch_bash_read_only_commands() {
        let calls = vec![
            make_tool_call("read_file", "1"),
            make_bash_call("2", "cargo check 2>&1 | head -50"),
            make_bash_call("3", "git diff HEAD"),
            make_tool_call("write_file", "4"),
            make_bash_call("5", "cargo build"),
            make_tool_call("grep", "6"),
        ];
        let (ro, mut_) = partition_tool_calls(&calls);
        // read_file + bash(cargo check|head) + bash(git diff) + grep = 4
        assert_eq!(ro.len(), 4);
        // write_file + bash(cargo build) = 2
        assert_eq!(mut_.len(), 2);
    }

    #[tokio::test]
    async fn parallel_round_bash_read_only_runs_in_parallel() {
        let calls = vec![
            make_tool_call("read_file", "1"),
            make_bash_call("2", "git status"),
            make_bash_call("3", "ls -la"),
            make_tool_call("grep", "4"),
        ];
        let outcome = execute_parallel_round(&calls, make_executor(0)).await;
        assert_eq!(outcome.parallel_count, 4, "all 4 should run in parallel");
        assert_eq!(outcome.sequential_count, 0);
    }

    #[tokio::test]
    async fn parallel_round_bash_mutating_runs_sequential() {
        let calls = vec![
            make_bash_call("1", "git status"),  // read-only → parallel
            make_bash_call("2", "cargo build"), // mutating → sequential
            make_bash_call("3", "ls"),          // read-only → parallel
        ];
        let outcome = execute_parallel_round(&calls, make_executor(0)).await;
        assert_eq!(outcome.parallel_count, 2);
        assert_eq!(outcome.sequential_count, 1);
    }

    #[tokio::test]
    async fn parallel_round_bash_no_args_is_sequential() {
        let calls = vec![
            make_tool_call("bash", "1"), // no args → mutating
            make_tool_call("read_file", "2"),
        ];
        let outcome = execute_parallel_round(&calls, make_executor(0)).await;
        assert_eq!(outcome.parallel_count, 1); // read_file only
        assert_eq!(outcome.sequential_count, 1); // bash without args
    }
}
