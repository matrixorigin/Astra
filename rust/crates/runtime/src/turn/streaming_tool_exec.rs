//! Streaming / Speculative Tool Execution (D-9)
//!
//! Inspired by Claude Code's `StreamingToolExecutor`, this module allows
//! read-only tools to begin executing **while the LLM response is still
//! streaming**. When the SSE stream emits a complete `tool_use` block for
//! a tool classified as read-only (see `parallel_tool_exec::is_read_only_tool`),
//! the executor can speculatively start it immediately.
//!
//! The host (stream_render.rs / cli_loop_host) feeds complete tool blocks
//! as they arrive. After the stream finishes, remaining (mutating) tools
//! run sequentially. Speculative results for read-only tools that were
//! not intercepted by Step 3 (skill/delegation) are merged into the final
//! results.
//!
//! Key design decisions:
//! - Only read-only tools are speculated (no side-effects to undo)
//! - Speculative results can be **discarded** if Step 3 intercepts the call
//! - Mutating tools wait until the full stream + Step 3 completes
//! - Semaphore limits concurrent speculative executions

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

use super::parallel_tool_exec::{is_read_only_tool, ToolExecutorFn, ToolExecResult};

/// Maximum speculative executions during streaming.
const MAX_SPECULATIVE: usize = 5;

/// Tracks in-flight speculative tool executions during SSE streaming.
pub struct StreamingToolExecutor {
    executor: ToolExecutorFn,
    semaphore: Arc<Semaphore>,
    /// call_id → JoinHandle for speculative results
    inflight: Arc<Mutex<HashMap<String, JoinHandle<ToolExecResult>>>>,
    /// call_id → completed results (ready for harvest)
    completed: Arc<Mutex<HashMap<String, ToolExecResult>>>,
}

impl StreamingToolExecutor {
    pub fn new(executor: ToolExecutorFn) -> Self {
        Self {
            executor,
            semaphore: Arc::new(Semaphore::new(MAX_SPECULATIVE)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Called when a complete tool_use block arrives from SSE stream.
    /// If the tool is read-only, begins speculative execution immediately.
    /// Returns true if speculative execution was started.
    pub async fn on_tool_block(
        &self,
        call_id: String,
        tool_name: String,
        tool_call: serde_json::Value,
        original_index: usize,
    ) -> bool {
        if !is_read_only_tool(&tool_name) {
            return false;
        }

        let sem = self.semaphore.clone();
        let exec = self.executor.clone();
        let completed = self.completed.clone();
        let cid = call_id.clone();

        let handle = tokio::spawn(async move {
            let _permit = match sem.try_acquire() {
                Ok(p) => p,
                Err(_) => {
                    // Semaphore full — don't speculate, will run later
                    return ToolExecResult {
                        original_index,
                        call_id: cid,
                        tool_name,
                        content: String::new(),
                        success: false,
                    };
                }
            };

            let (ret_id, ret_name, content, success) = exec(tool_call).await;
            let result = ToolExecResult {
                original_index,
                call_id: ret_id,
                tool_name: ret_name,
                content,
                success,
            };

            // Move to completed map
            completed.lock().await.insert(cid, result.clone());
            result
        });

        self.inflight.lock().await.insert(call_id, handle);
        true
    }

    /// Harvest all completed speculative results without blocking.
    /// Returns results that have finished so far.
    pub async fn harvest_completed(&self) -> Vec<ToolExecResult> {
        let mut completed = self.completed.lock().await;
        let results: Vec<ToolExecResult> = completed.drain().map(|(_, v)| v).collect();
        results
    }

    /// Discard a speculative result (e.g., when Step 3 intercepts the tool call).
    /// Cancels the in-flight task if still running.
    pub async fn discard(&self, call_id: &str) {
        if let Some(handle) = self.inflight.lock().await.remove(call_id) {
            handle.abort();
        }
        self.completed.lock().await.remove(call_id);
    }

    /// Wait for all in-flight speculative executions to complete.
    /// Returns all results keyed by call_id.
    pub async fn wait_all(&self) -> HashMap<String, ToolExecResult> {
        let handles: Vec<(String, JoinHandle<ToolExecResult>)> = {
            let mut inflight = self.inflight.lock().await;
            inflight.drain().collect()
        };

        let mut results = HashMap::new();
        for (cid, handle) in handles {
            if let Ok(result) = handle.await {
                // Only include successful speculations (non-empty content)
                if !result.content.is_empty() {
                    results.insert(cid, result);
                }
            }
        }

        // Also include already-completed results
        let mut completed = self.completed.lock().await;
        for (cid, result) in completed.drain() {
            results.entry(cid).or_insert(result);
        }

        results
    }

    /// Merge speculative results with the post-Step3 tool call list.
    /// Tool calls whose call_id has a speculative result use it directly;
    /// others need to be executed normally.
    ///
    /// Returns (already_done, still_needed) partitioned by call_id.
    pub async fn merge_speculative(
        &self,
        tool_call_ids: &[String],
    ) -> (Vec<ToolExecResult>, Vec<String>) {
        let speculative = self.wait_all().await;
        let mut done = Vec::new();
        let mut needed = Vec::new();

        for cid in tool_call_ids {
            if let Some(result) = speculative.get(cid) {
                done.push(result.clone());
            } else {
                needed.push(cid.clone());
            }
        }

        (done, needed)
    }

    /// Number of currently in-flight speculative executions.
    pub async fn inflight_count(&self) -> usize {
        self.inflight.lock().await.len()
    }
}

// ───────────────────────────── Tests ─────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn make_fast_executor() -> ToolExecutorFn {
        Arc::new(|tc: serde_json::Value| {
            Box::pin(async move {
                let call_id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                (call_id, name.clone(), format!("result:{}", name), true)
            })
        })
    }

    fn make_slow_executor(ms: u64) -> ToolExecutorFn {
        Arc::new(move |tc: serde_json::Value| {
            Box::pin(async move {
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                let call_id = tc["id"].as_str().unwrap_or("").to_string();
                let name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                (call_id, name.clone(), format!("result:{}", name), true)
            })
        })
    }

    fn tool_block(name: &str, id: &str) -> serde_json::Value {
        json!({
            "id": id,
            "function": { "name": name, "arguments": "{}" }
        })
    }

    #[tokio::test]
    async fn speculate_read_only() {
        let exec = StreamingToolExecutor::new(make_fast_executor());

        let started = exec
            .on_tool_block(
                "c1".into(),
                "read_file".into(),
                tool_block("read_file", "c1"),
                0,
            )
            .await;
        assert!(started);

        let results = exec.wait_all().await;
        assert_eq!(results.len(), 1);
        assert!(results["c1"].content.contains("read_file"));
    }

    #[tokio::test]
    async fn skip_mutating() {
        let exec = StreamingToolExecutor::new(make_fast_executor());

        let started = exec
            .on_tool_block("c1".into(), "bash".into(), tool_block("bash", "c1"), 0)
            .await;
        assert!(!started);
    }

    #[tokio::test]
    async fn discard_intercepted() {
        let exec = StreamingToolExecutor::new(make_slow_executor(200));

        exec.on_tool_block(
            "c1".into(),
            "grep".into(),
            tool_block("grep", "c1"),
            0,
        )
        .await;

        // Discard before completion
        exec.discard("c1").await;

        let results = exec.wait_all().await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn merge_speculative_results() {
        let exec = StreamingToolExecutor::new(make_fast_executor());

        // Speculate on read_file
        exec.on_tool_block(
            "c1".into(),
            "read_file".into(),
            tool_block("read_file", "c1"),
            0,
        )
        .await;

        // Give it time to complete
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // Merge: c1 was speculated, c2 was not
        let (done, needed) = exec.merge_speculative(&["c1".into(), "c2".into()]).await;

        assert_eq!(done.len(), 1);
        assert_eq!(done[0].call_id, "c1");
        assert_eq!(needed, vec!["c2"]);
    }

    #[tokio::test]
    async fn multiple_speculative() {
        let exec = StreamingToolExecutor::new(make_fast_executor());

        for i in 0..3 {
            let id = format!("c{}", i);
            exec.on_tool_block(id.clone(), "glob".into(), tool_block("glob", &id), i)
                .await;
        }

        let results = exec.wait_all().await;
        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn harvest_partial() {
        let exec = StreamingToolExecutor::new(make_fast_executor());

        exec.on_tool_block(
            "c1".into(),
            "read_file".into(),
            tool_block("read_file", "c1"),
            0,
        )
        .await;

        // Wait for completion
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let harvested = exec.harvest_completed().await;
        assert_eq!(harvested.len(), 1);

        // Second harvest should be empty
        let harvested2 = exec.harvest_completed().await;
        assert_eq!(harvested2.len(), 0);
    }
}
