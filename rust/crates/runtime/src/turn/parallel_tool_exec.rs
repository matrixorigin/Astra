//! Parallel Tool Execution (D-1)
//!
//! Classifies tool calls as read-only (safe for parallel execution) vs mutating
//! (must run sequentially), inspired by Claude Code's `StreamingToolExecutor`.
//!
//! Read-only tools can be dispatched concurrently via `tokio::JoinSet` with
//! a semaphore limiting concurrency. Mutating tools run one at a time after
//! all read-only tools complete. If a bash tool errors, subsequent mutating
//! tools are aborted (sibling abort).
//!
//! This module provides the classification and orchestration logic. The actual
//! `execute_tool` function is supplied by the caller (CLI's stream_render.rs
//! or any other host).

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::Semaphore;

/// Maximum number of read-only tools that can execute concurrently.
pub const MAX_CONCURRENT_READ_ONLY: usize = 10;

// ───────────────────────────── Tool Classification ──────────────────────

/// Read-only tool names that are safe for parallel execution.
/// These tools do not modify the filesystem or have side-effects.
static READ_ONLY_TOOLS: &[&str] = &[
    "read_file",
    "file_read",
    "ReadFileTool",
    "grep",
    "GrepTool",
    "glob",
    "GlobTool",
    "list_dir",
    "ListDirTool",
    "web_fetch",
    "WebFetchTool",
    "web_search",
    "WebSearchTool",
    "memory_search",
    "memory_retrieve",
    "get_file_contents",
    "search_code",
    "list_files",
    "find_files",
    "view_file",
];

/// Classify a tool call as read-only or mutating.
pub fn is_read_only_tool(tool_name: &str) -> bool {
    READ_ONLY_TOOLS.contains(&tool_name)
}

/// Partition tool calls into (read_only, mutating) groups, preserving
/// original indices for result reassembly.
pub fn partition_tool_calls(tool_calls: &[Value]) -> (Vec<(usize, &Value)>, Vec<(usize, &Value)>) {
    let mut read_only = Vec::new();
    let mut mutating = Vec::new();

    for (i, tc) in tool_calls.iter().enumerate() {
        let name = tc
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|n| n.as_str())
            .or_else(|| tc.get("name").and_then(|n| n.as_str()))
            .unwrap_or("");

        if is_read_only_tool(name) {
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
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_READ_ONLY));
        let mut join_set = tokio::task::JoinSet::new();

        for (idx, tc) in read_only {
            let tc_owned = tc.clone();
            let sem = semaphore.clone();
            let exec = executor.clone();
            join_set.spawn(async move {
                let _permit = sem.acquire().await.expect("semaphore closed");
                let (call_id, tool_name, content, success) = exec(tc_owned).await;
                ToolExecResult {
                    original_index: idx,
                    call_id,
                    tool_name,
                    content,
                    success,
                }
            });
        }

        while let Some(result) = join_set.join_next().await {
            if let Ok(r) = result {
                let idx = r.original_index;
                results[idx] = Some(r);
            }
        }
    }

    // Phase 2: Execute mutating tools sequentially
    let sequential_count = mutating.len();
    let bash_names: HashSet<&str> = ["bash", "BashTool", "shell", "execute_command"]
        .iter()
        .copied()
        .collect();

    for (idx, tc) in mutating {
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

        // Sibling abort: if a bash tool fails, skip remaining mutating tools
        if !success && bash_names.contains(tool_name.as_str()) {
            sibling_aborted = true;
        }

        results[idx] = Some(ToolExecResult {
            original_index: idx,
            call_id,
            tool_name,
            content,
            success,
        });
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

    fn make_executor(
        delay_ms: u64,
    ) -> ToolExecutorFn {
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
}
