//! Streaming / Speculative Tool Execution (D-9)
//!
//! This module allows
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
use std::time::Instant;

use tokio::sync::{Mutex, Semaphore};
use tokio::task::JoinHandle;

use super::parallel_tool_exec::{ToolExecResult, ToolExecutorFn, is_read_only_tool};
use super::permission_types::PermissionDecision;

/// Environment variable gating speculative streaming execution.
/// Set to `1` to enable; default (unset) keeps pre-speculation behavior.
pub const STREAMING_TOOL_EXEC_ENV: &str = "ASTRA_STREAMING_TOOL_EXEC";

/// Returns true iff the env-gated streaming tool executor is enabled.
pub fn streaming_tool_exec_enabled() -> bool {
    std::env::var(STREAMING_TOOL_EXEC_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Decide whether a tool call is eligible for speculative execution mid-stream.
///
/// Speculative execution is only safe when:
/// 1. The tool is classified read-only (no side-effects to undo), and
/// 2. Permission decision is either unknown or `Approve`. If the permission
///    layer has already pre-evaluated the call and returned `Deny` or
///    `Escalate`, we must not run it — the user hasn't consented.
///
/// The host is expected to re-check permission at the batch-phase merge point
/// so that observability/journal events fire exactly once regardless of
/// whether speculation happened.
pub fn should_speculate(tool_name: &str, perm_decision: Option<&PermissionDecision>) -> bool {
    if !is_read_only_tool(tool_name) {
        return false;
    }
    match perm_decision {
        None => true, // unknown → assume auto-allowed for read-only
        Some(PermissionDecision::Approve { .. }) => true,
        Some(PermissionDecision::Deny { .. }) | Some(PermissionDecision::Escalate) => false,
    }
}

/// Maximum speculative executions during streaming.
const MAX_SPECULATIVE: usize = 5;

/// Runtime counters describing how often speculative execution fired and
/// how much wall-clock was saved. Exposed via [`StreamingToolExecutor::snapshot`].
///
/// Consumers (CLI session-end logger, ObservabilityHub) can aggregate across
/// sessions or emit a single structured event per turn/session.
///
/// Fields are monotonic over the executor's lifetime unless [`StreamingToolExecutor::reset_metrics`]
/// is called. `wasted` is derived, not stored: `wasted = started - hit - discarded - inflight`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StreamingSpeculationMetrics {
    /// Number of `on_tool_block` invocations that actually spawned a speculative future.
    pub started: u64,
    /// Number of speculative results successfully merged back into a real tool_call batch.
    pub hit: u64,
    /// Number of speculative results dropped because Step 3 (skill/delegation) intercepted.
    pub discarded: u64,
    /// Number of speculative futures still in-flight at the time of this snapshot.
    pub inflight: u64,
    /// Sum of per-hit execution durations (ms). Approximates the wall-clock time
    /// the tool's I/O overlapped with LLM streaming — an upper bound on savings.
    pub total_saved_ms: u64,
}

impl StreamingSpeculationMetrics {
    /// Estimate of "wasted" speculations: started but neither hit nor discarded
    /// nor still in flight. Typically indicates a read-only tool whose call_id
    /// never appeared in the post-stream batch (e.g., Step 3 replaced it with
    /// a different tool call without calling `discard`).
    pub fn wasted(&self) -> u64 {
        self.started
            .saturating_sub(self.hit)
            .saturating_sub(self.discarded)
            .saturating_sub(self.inflight)
    }

    /// Hit rate (0.0–1.0): fraction of started speculations that translated
    /// into real wall-clock savings. Returns 0.0 if nothing has been started.
    pub fn hit_rate(&self) -> f64 {
        if self.started == 0 {
            0.0
        } else {
            self.hit as f64 / self.started as f64
        }
    }
}

#[derive(Default)]
struct InnerMetrics {
    started: u64,
    hit: u64,
    discarded: u64,
    total_saved_ms: u64,
    /// call_id → (start_instant, duration on completion)
    started_at: HashMap<String, Instant>,
    completed_ms: HashMap<String, u64>,
}

/// Tracks in-flight speculative tool executions during SSE streaming.
pub struct StreamingToolExecutor {
    executor: ToolExecutorFn,
    semaphore: Arc<Semaphore>,
    /// call_id → JoinHandle for speculative results
    inflight: Arc<Mutex<HashMap<String, JoinHandle<ToolExecResult>>>>,
    /// call_id → completed results (ready for harvest)
    completed: Arc<Mutex<HashMap<String, ToolExecResult>>>,
    metrics: Arc<Mutex<InnerMetrics>>,
}

impl StreamingToolExecutor {
    pub fn new(executor: ToolExecutorFn) -> Self {
        Self {
            executor,
            semaphore: Arc::new(Semaphore::new(MAX_SPECULATIVE)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
            completed: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(Mutex::new(InnerMetrics::default())),
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
        let metrics = self.metrics.clone();
        let cid = call_id.clone();

        // Insert a placeholder into inflight BEFORE spawning to avoid race
        // where the task completes before the handle is recorded.
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let started_at = Instant::now();
        let handle = tokio::spawn(async move {
            // Wait until handle is registered in inflight map
            let _ = rx.await;

            let _permit = match sem.try_acquire() {
                Ok(p) => p,
                Err(_) => {
                    // audit-#9: surface a descriptive sentinel instead of an
                    // empty string so callers (and tests) can distinguish
                    // "speculation skipped" from "tool returned empty output".
                    return ToolExecResult {
                        original_index,
                        call_id: cid,
                        tool_name,
                        content: "speculative execution skipped: capacity reached".to_string(),
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

            // Record duration for metrics before moving result.
            {
                let mut m = metrics.lock().await;
                let elapsed_ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
                m.completed_ms.insert(cid.clone(), elapsed_ms);
            }

            // Move to completed map
            completed.lock().await.insert(cid, result.clone());
            result
        });

        {
            let mut m = self.metrics.lock().await;
            m.started = m.started.saturating_add(1);
            m.started_at.insert(call_id.clone(), started_at);
        }

        self.inflight.lock().await.insert(call_id, handle);
        // Signal task to proceed now that handle is in the map
        let _ = tx.send(());
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
        let mut aborted = false;
        if let Some(handle) = self.inflight.lock().await.remove(call_id) {
            handle.abort();
            aborted = true;
        }
        if self.completed.lock().await.remove(call_id).is_some() {
            aborted = true;
        }
        if aborted {
            let mut m = self.metrics.lock().await;
            m.discarded = m.discarded.saturating_add(1);
            m.started_at.remove(call_id);
            m.completed_ms.remove(call_id);
        }
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
                // Account this call_id as a hit.
                let mut m = self.metrics.lock().await;
                m.hit = m.hit.saturating_add(1);
                if let Some(ms) = m.completed_ms.remove(cid) {
                    m.total_saved_ms = m.total_saved_ms.saturating_add(ms);
                }
                m.started_at.remove(cid);
            } else {
                needed.push(cid.clone());
            }
        }

        (done, needed)
    }
    /// Take a metrics snapshot. Does not reset counters.
    pub async fn snapshot(&self) -> StreamingSpeculationMetrics {
        let inflight = self.inflight.lock().await.len() as u64;
        let m = self.metrics.lock().await;
        StreamingSpeculationMetrics {
            started: m.started,
            hit: m.hit,
            discarded: m.discarded,
            inflight,
            total_saved_ms: m.total_saved_ms,
        }
    }

    /// Reset all counters to zero. Used at session boundaries when per-session
    /// aggregation is desired.
    pub async fn reset_metrics(&self) {
        let mut m = self.metrics.lock().await;
        *m = InnerMetrics::default();
    }

    /// Emit the current metrics snapshot as a structured `tracing::info!`
    /// event on target `astra::streaming_speculation::metrics`. Intended to
    /// be called once at session/turn end by the host.
    ///
    /// The event fields are stable so downstream log aggregators and the
    /// ObservabilityHub can pick them up without parsing prose.
    pub async fn emit_metrics_log(&self, session_id: Option<&str>) {
        let snap = self.snapshot().await;
        tracing::info!(
            target: "astra::streaming_speculation::metrics",
            session_id = session_id.unwrap_or(""),
            started = snap.started,
            hit = snap.hit,
            discarded = snap.discarded,
            inflight = snap.inflight,
            wasted = snap.wasted(),
            total_saved_ms = snap.total_saved_ms,
            hit_rate = snap.hit_rate(),
            "streaming speculation metrics"
        );
    }
}

/// audit-#12: speculative tool tasks must not outlive their executor. When a
/// turn aborts and the host drops the [`StreamingToolExecutor`], any
/// in-flight tasks would otherwise keep running, holding onto the cloned
/// executor closure (and its captured permissions/IO handles) indefinitely.
///
/// We can't `await` a lock from `Drop`, but `try_lock` succeeds whenever no
/// other task is currently mutating the map — which is the common case at
/// drop time because the host has stopped invoking [`Self::on_tool_block`]
/// and friends. If `try_lock` fails (a poll is racing with us), the tasks
/// are still bounded by the per-task semaphore + their own work; the leak
/// surface shrinks dramatically without the panic risk of blocking inside
/// `Drop`.
impl Drop for StreamingToolExecutor {
    fn drop(&mut self) {
        if let Ok(mut inflight) = self.inflight.try_lock() {
            for (_cid, handle) in inflight.drain() {
                handle.abort();
            }
        } else {
            tracing::debug!(
                target: "astra_turn_core::streaming_tool_exec",
                "StreamingToolExecutor dropped while inflight map was locked; speculative tasks may finish on their own"
            );
        }
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

        exec.on_tool_block("c1".into(), "grep".into(), tool_block("grep", "c1"), 0)
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

    #[test]
    fn should_speculate_gates() {
        // Read-only + unknown permission → speculate
        assert!(should_speculate("read_file", None));
        assert!(should_speculate("grep", None));
        // Read-only + Approve → speculate
        assert!(should_speculate(
            "read_file",
            Some(&PermissionDecision::approve())
        ));
        // Read-only + Deny → no speculation
        assert!(!should_speculate(
            "read_file",
            Some(&PermissionDecision::deny("policy"))
        ));
        // Read-only + Escalate → no speculation
        assert!(!should_speculate(
            "read_file",
            Some(&PermissionDecision::Escalate)
        ));
        // Non-read-only never speculates
        assert!(!should_speculate("bash", None));
        assert!(!should_speculate(
            "write_file",
            Some(&PermissionDecision::approve())
        ));
    }

    #[tokio::test]
    async fn metrics_counts_hit_and_saved_ms() {
        let exec = StreamingToolExecutor::new(make_slow_executor(80));

        // Two read-only speculations.
        exec.on_tool_block(
            "c1".into(),
            "read_file".into(),
            tool_block("read_file", "c1"),
            0,
        )
        .await;
        exec.on_tool_block("c2".into(), "grep".into(), tool_block("grep", "c2"), 1)
            .await;

        let snap_before = exec.snapshot().await;
        assert_eq!(snap_before.started, 2);
        assert_eq!(snap_before.hit, 0);

        // Merge: c1 becomes a hit, c2 is listed but we also claim c3 which has
        // no speculation.
        let (done, needed) = exec.merge_speculative(&["c1".into(), "c3".into()]).await;
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].call_id, "c1");
        assert_eq!(needed, vec!["c3"]);

        let snap = exec.snapshot().await;
        assert_eq!(snap.started, 2);
        assert_eq!(snap.hit, 1);
        assert_eq!(snap.discarded, 0);
        assert_eq!(snap.inflight, 0);
        // c2 was started but not merged and not discarded → wasted.
        assert_eq!(snap.wasted(), 1);
        // Saved ≥ tool duration (80ms) since the speculative future fully completed.
        assert!(
            snap.total_saved_ms >= 70,
            "saved_ms={}",
            snap.total_saved_ms
        );
        assert!((snap.hit_rate() - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn metrics_counts_discard() {
        let exec = StreamingToolExecutor::new(make_slow_executor(200));

        exec.on_tool_block(
            "c1".into(),
            "read_file".into(),
            tool_block("read_file", "c1"),
            0,
        )
        .await;
        exec.discard("c1").await;

        // Discarding a second time with no entry must not double-count.
        exec.discard("c1").await;

        let snap = exec.snapshot().await;
        assert_eq!(snap.started, 1);
        assert_eq!(snap.hit, 0);
        assert_eq!(snap.discarded, 1);
        assert_eq!(snap.wasted(), 0);
        assert_eq!(snap.total_saved_ms, 0);
    }

    #[tokio::test]
    async fn metrics_reset_clears_counters() {
        let exec = StreamingToolExecutor::new(make_fast_executor());

        exec.on_tool_block(
            "c1".into(),
            "read_file".into(),
            tool_block("read_file", "c1"),
            0,
        )
        .await;
        let (_done, _) = exec.merge_speculative(&["c1".into()]).await;

        let before = exec.snapshot().await;
        assert!(before.started >= 1 && before.hit >= 1);

        exec.reset_metrics().await;
        let after = exec.snapshot().await;
        assert_eq!(after, StreamingSpeculationMetrics::default());
    }

    /// audit-#12: dropping the executor must abort any speculative tasks it
    /// started so they don't outlive the turn.
    #[tokio::test]
    async fn drop_aborts_inflight_speculative_tasks() {
        // Use a sufficiently slow executor that the speculative task is still
        // running when we drop the host.
        let exec = StreamingToolExecutor::new(make_slow_executor(2_000));
        // read_only tool ("read_file") is eligible for speculation.
        let started = exec
            .on_tool_block(
                "c-drop".into(),
                "read_file".into(),
                tool_block("read_file", "c-drop"),
                0,
            )
            .await;
        assert!(started, "speculative task should have started");

        // Capture the underlying handle before dropping the executor.
        let handle: tokio::task::JoinHandle<ToolExecResult> = {
            let mut g = exec.inflight.lock().await;
            g.remove("c-drop").expect("inflight handle present")
        };
        // Re-insert so Drop has something to abort.
        exec.inflight.lock().await.insert("c-drop".into(), handle);

        // Get a clone of the Arc so we can observe the handle after drop.
        let inflight_arc = exec.inflight.clone();
        drop(exec);

        // Yield enough times for the abort to take effect on the runtime.
        for _ in 0..20 {
            tokio::task::yield_now().await;
            if inflight_arc.lock().await.is_empty() {
                break;
            }
        }

        let map = inflight_arc.lock().await;
        assert!(
            map.is_empty(),
            "Drop must drain in-flight speculative tasks"
        );
    }
}
