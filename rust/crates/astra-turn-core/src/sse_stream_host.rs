//! Trait-based SSE stream consumption for the agentic loop.
//!
//! [`SseStreamHost`] abstracts the CLI-specific parts of SSE consumption
//! (terminal rendering, edge tool execution, approval prompts) so the
//! protocol loop can run identically in CLI and headless cloud contexts.
//!
//! ```text
//! SSE byte stream
//!   ↓ ChatTurnSseFramer   (protocol — runtime)
//!   ↓ dispatch_event_block (protocol — runtime)
//!   ↓ SseStreamHost        (host — CLI or headless)
//!       ├─ on_render_effects   → terminal spinners / text deltas
//!       ├─ execute_tool        → local tool execution + permission
//!       ├─ resolve_approval    → interactive prompt or ledger
//!       └─ on_stream_complete  → cleanup
//! ```

use crate::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, ChatTurnSseFramer, EdgeApprovalRequest, SseRenderEffect,
    dispatch_chat_turn_sse_event_block,
};
use astra_thin_client::ApprovalKind;
use async_trait::async_trait;
use serde_json::Value;

/// Stream idle watchdog default: abort SSE consumption if no chunk arrives within this time.
///
/// This timeout applies before the first SSE frame is received (the TTFT gap).
/// Because thinking/reasoning models (o1, o3, DeepSeek-R1, claude with extended
/// thinking) can spend minutes in the reasoning phase **before** emitting any SSE
/// chunk, it is impossible to distinguish a genuinely dead connection from a slow
/// model at this stage. The 5-minute default matches
/// [`STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS`].
pub const STREAM_IDLE_TIMEOUT_MS: u64 = 300_000;

/// Idle timeout after at least one SSE chunk has been received.
///
/// Thinking/reasoning models (o1, o3, DeepSeek-R1, claude with extended thinking)
/// can spend minutes in the reasoning phase where the SSE stream produces no
/// `text_delta` events. A 5-minute post-progress window avoids false aborts while
/// still catching genuinely stalled connections.
pub const STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS: u64 = 300_000;

/// Stream idle timeout (pre-progress).
///
/// Returns the value of `ASTRA_STREAM_IDLE_TIMEOUT_MS` env var when set,
/// otherwise falls back to [`STREAM_IDLE_TIMEOUT_MS`].
pub fn stream_idle_timeout() -> std::time::Duration {
    let ms = std::env::var("ASTRA_STREAM_IDLE_TIMEOUT_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(STREAM_IDLE_TIMEOUT_MS);
    std::time::Duration::from_millis(ms)
}

/// Stream idle timeout (post-progress).
///
/// Returns the value of `ASTRA_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS` env var when set,
/// otherwise falls back to [`STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS`].
pub fn stream_idle_timeout_after_progress() -> std::time::Duration {
    let ms = std::env::var("ASTRA_STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS);
    std::time::Duration::from_millis(ms)
}

// ─── Data types ──────────────────────────────────────────────────────────────

/// Result of executing an edge tool request via the host.
#[derive(Debug, Clone)]
pub struct EdgeToolExecResult {
    pub request_id: String,
    pub tool: String,
    pub args: Value,
    pub output: String,
    pub tool_result_fields: Option<serde_json::Map<String, Value>>,
    /// Semantic label: `"ok"`, `"error"`, etc.
    pub status: String,
    pub duration_ms: u64,
}

impl crate::headless_tool_assembly::EdgeToolRoundRow for EdgeToolExecResult {
    fn tool_name(&self) -> &str {
        &self.tool
    }
    fn tool_args(&self) -> &Value {
        &self.args
    }
    fn tool_output(&self) -> &str {
        &self.output
    }
    fn tool_result_fields(&self) -> Option<&serde_json::Map<String, Value>> {
        self.tool_result_fields.as_ref()
    }
    fn tool_duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn assistant_tool_call_id(&self, index: usize) -> String {
        if self.request_id.is_empty() {
            format!("edge-{index}")
        } else {
            self.request_id.clone()
        }
    }
}

/// Result of resolving an approval request via the host.
#[derive(Debug, Clone)]
pub struct EdgeApprovalResult {
    pub request_id: String,
    /// `"allow"` or `"deny"`.
    pub decision: String,
    pub reason: Option<String>,
}

/// A tool request bundled for batch execution.
#[derive(Debug, Clone)]
pub struct ToolBatchRequest {
    pub request_id: String,
    pub tool: String,
    pub args: Value,
}

/// Returns `true` if the named tool is safe for concurrent execution.
///
/// Concurrent-safe tools are read-only operations without observable side effects,
/// making them safe to execute in parallel via `futures::future::join_all`.
/// This includes both sync tools (fast local I/O) and async tools (network I/O
/// that benefits most from parallel execution).
pub fn is_tool_concurrency_safe(tool: &str) -> bool {
    matches!(
        tool,
        // ── Local read-only (sync) ───────────────────────────────────
        "read_file"
            | "list_dir"
            | "grep"
            | "glob"
            | "git_status"
            | "git_diff"
            | "git_log"
            | "git_show"
            | "git_blame"
            | "git_file_history"
            | "git_contributors"
            | "git_log_search"
            | "find_definition"
            | "find_references"
            | "call_graph"
            | "extract_members"
            | "type_hierarchy"
            | "hover_info"
            | "symbol_search"
            | "dead_code"
            | "symbols"
            | "lsp"
            | "env"
            | "brief"
            | "tool_search"
            | "get_agent_info"
            | "reflect"
            | "web_fetch"
            | "web_search"
            | "share_context"
            | "query_context"
            // ── GitHub read-only (async — benefits from join_all) ─────
            | "github_list_prs"
            | "github_get_pr"
            | "github_ci_status"
            | "github_list_issues"
            | "github_get_issue"
            | "github_repo_stats"
            // ── Memoria read-only (async) ────────────────────────────
            | "memory_retrieve"
            | "memory_search"
            | "memory_profile"
    )
}

/// Aggregated result from consuming one SSE stream.
#[derive(Debug)]
pub struct SseConsumeResult {
    /// Protocol-level accumulator (session_id, text, tool_calls, usage, etc.).
    pub accum: ChatTurnSseAccum,
    /// Time to first token (ms), measured by the framer.
    pub ttft_ms: Option<u64>,
    /// Tool executions performed during the stream.
    pub tool_results: Vec<EdgeToolExecResult>,
    /// Approval resolutions performed during the stream.
    pub approval_results: Vec<EdgeApprovalResult>,
}

/// Why SSE consumption aborted unexpectedly.
///
/// Uses [`astra_core::ErrorKind`] directly — no separate enum needed.
pub type SseAbortReason = astra_core::ErrorKind;

// ─── Host trait ──────────────────────────────────────────────────────────────

/// Abstraction over CLI-specific SSE consumption behavior.
///
/// The runtime calls these methods during [`consume_sse_stream`]; the host
/// decides *how* to render, execute tools, and resolve approvals.
///
/// **CLI host**: renders spinners / text to terminal, executes tools locally
/// via `ToolExecutor`, prompts user for approval via `PermissionManager`.
///
/// **Headless host**: no-op rendering, auto-deny approvals (or ledger-based),
/// tool execution via edge callback ledger.
#[async_trait]
pub trait SseStreamHost: Send {
    /// Called for each batch of render effects parsed from an SSE event block.
    /// CLI: prints text deltas, starts/stops thinking spinner.
    /// Headless: no-op.
    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>);

    /// Called when a complete tool_call entry has been accumulated from the
    /// SSE stream. Default: no-op.
    ///
    /// CLI host uses this to kick speculative read-only tool execution
    /// (see [`crate::streaming_tool_exec::StreamingToolExecutor`]), overlapping
    /// tool I/O with the remaining LLM stream. Gated behind
    /// `ASTRA_STREAMING_TOOL_EXEC=1` for rollout safety.
    ///
    /// `index` is the position in `accum.tool_calls`; `tool_call` is the
    /// normalized OpenAI-shaped call object (`{id, type, function: {name, arguments}}`).
    async fn on_tool_call_complete(&mut self, _index: usize, _tool_call: &Value) {}

    /// Called when the SSE stream ends. Host should clean up any active UI state.
    fn on_stream_complete(&mut self);

    /// Called once before the consumer blocks waiting for the first SSE chunk (TTFT gap).
    /// CLI: may show a short “waiting for model” spinner on stderr.
    fn on_before_sse_read_loop(&mut self) {}

    /// Called when the first decodable SSE event block is produced from the byte stream.
    /// CLI: dismiss the TTFT “waiting” spinner before handling render effects.
    fn on_first_sse_frame(&mut self) {}

    /// Periodic heartbeat while waiting for the next SSE chunk.
    /// CLI: refreshes the thinking pane elapsed timer so the UI never looks frozen.
    fn on_idle_tick(&mut self) {}

    /// Called when a `session_info` SSE event yields a session ID.
    ///
    /// Hosts that need the server-issued session identity during the same turn
    /// (for example, before flushing pending tool requests) can capture it here.
    fn on_session_id(&mut self, _session_id: &str) {}

    /// Execute a tool request that arrived via `tool_request` SSE event.
    /// Returns the execution result (output, status, duration).
    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &Value,
    ) -> EdgeToolExecResult;

    /// Resolve an approval request that arrived via `approval_required` SSE event.
    /// CLI: interactive prompt. Headless: auto-deny or ledger-based.
    async fn resolve_approval(
        &mut self,
        request_id: &str,
        tool: &str,
        approval_kind: ApprovalKind,
        session_id: Option<&str>,
        detail: Option<&str>,
    ) -> EdgeApprovalResult;

    /// Resolve a batch of approval requests in one interactive step when supported.
    async fn resolve_approvals_batch(
        &mut self,
        requests: &[EdgeApprovalRequest],
        session_id: Option<&str>,
    ) -> Vec<EdgeApprovalResult> {
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            results.push(
                self.resolve_approval(
                    &request.request_id,
                    &request.tool,
                    request.approval_kind,
                    session_id,
                    request.detail.as_deref(),
                )
                .await,
            );
        }
        results
    }

    /// Execute a batch of tool requests, potentially in parallel.
    ///
    /// The default implementation calls [`execute_tool`](Self::execute_tool)
    /// sequentially.  CLI hosts override this to run concurrent-safe tools
    /// via `futures::future::join_all`, overlapping network I/O for async
    /// tools (GitHub, Memoria, MCP).
    ///
    /// Results are returned in the same order as the input `requests`.
    async fn execute_tools_batch(
        &mut self,
        requests: Vec<ToolBatchRequest>,
    ) -> Vec<EdgeToolExecResult> {
        let mut results = Vec::with_capacity(requests.len());
        for req in requests {
            let r = self
                .execute_tool(&req.request_id, &req.tool, &req.args)
                .await;
            results.push(r);
        }
        results
    }
}

// ─── Generic SSE consumer ────────────────────────────────────────────────────

/// Consume an SSE byte stream using the provided host for rendering and edge work.
///
/// **This is the only supported entrypoint.** It always enforces an idle watchdog and returns
/// an abort reason when the stream ends unexpectedly.
///
/// When idle timeout triggers, the consumer tombstones partial state:
/// - clears partial `full_text` / `reasoning` / `tool_calls`
/// - drops pending edge work to prevent dirty state leaks
/// - sets `accum.error_message` to a synthetic error for visibility
pub async fn consume_sse_stream<H: SseStreamHost>(
    chunks: &mut (dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin + Send),
    host: &mut H,
    idle_timeout: std::time::Duration,
) -> (SseConsumeResult, Option<SseAbortReason>) {
    consume_sse_stream_cancellable(chunks, host, idle_timeout, None, None).await
}

/// Consume an SSE byte stream with optional cancellation support.
///
/// Like [`consume_sse_stream`] but accepts an optional [`tokio_util::sync::CancellationToken`]
/// that can interrupt the stream consumption mid-flight. When cancelled:
/// - Returns `SseAbortReason::Cancelled`
/// - Tombstones partial state (same as idle timeout)
/// - Stops the thinking spinner
pub async fn consume_sse_stream_cancellable<H: SseStreamHost>(
    chunks: &mut (dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin + Send),
    host: &mut H,
    idle_timeout: std::time::Duration,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    idle_timeout_after_progress: Option<std::time::Duration>,
) -> (SseConsumeResult, Option<SseAbortReason>) {
    use futures_util::StreamExt;

    let mut accum = ChatTurnSseAccum::default();
    let mut framer = ChatTurnSseFramer::new();
    let mut pending: Vec<ChatTurnEdgePending> = Vec::new();
    let mut tool_results: Vec<EdgeToolExecResult> = Vec::new();
    let mut approval_results: Vec<EdgeApprovalResult> = Vec::new();
    let mut abort: Option<SseAbortReason> = None;
    let mut first_sse_frame_seen = false;
    let mut reported_session_id: Option<String> = None;

    host.on_before_sse_read_loop();

    let idle_pre = idle_timeout;
    let idle_post = idle_timeout_after_progress.unwrap_or_else(stream_idle_timeout_after_progress);
    // Short tick for UI heartbeat (thinking pane elapsed timer refresh).
    let tick = std::time::Duration::from_secs(1);
    loop {
        // After first progress, use the more generous post-progress timeout
        // to avoid false aborts during thinking/reasoning model pauses.
        let idle = if first_sse_frame_seen {
            idle_post
        } else {
            idle_pre
        };
        // Inner loop: retry with short ticks so on_idle_tick can refresh the UI,
        // but accumulate elapsed time toward the full idle_timeout.
        let chunk_result = 'wait: {
            let mut elapsed = std::time::Duration::ZERO;
            loop {
                let remaining = idle.saturating_sub(elapsed);
                if remaining.is_zero() {
                    break 'wait None; // idle timeout
                }
                let wait = remaining.min(tick);
                let r = if let Some(token) = cancel_token {
                    tokio::select! {
                        biased;
                        _ = token.cancelled() => {
                            abort = Some(astra_core::ErrorKind::Cancelled);
                            break 'wait None;
                        }
                        next = tokio::time::timeout(wait, chunks.next()) => next,
                    }
                } else {
                    tokio::time::timeout(wait, chunks.next()).await
                };
                match r {
                    Ok(v) => break 'wait Some(v),
                    Err(_) => {
                        elapsed += wait;
                        host.on_idle_tick();
                    }
                }
            }
        };
        if abort.is_some() {
            break;
        }

        let chunk = match chunk_result {
            Some(v) => v,
            None => {
                abort = Some(astra_core::ErrorKind::StreamIdle);
                break;
            }
        };
        let Some(chunk) = chunk else { break };
        let Ok(bytes) = chunk else {
            abort = Some(astra_core::ErrorKind::StreamTransport);
            break;
        };
        for event_str in framer.push_lossy_bytes(&bytes) {
            if !first_sse_frame_seen {
                first_sse_frame_seen = true;
                host.on_first_sse_frame();
            }
            let tc_len_before = accum.tool_calls.len();
            let effects = dispatch_chat_turn_sse_event_block(&event_str, &mut accum, &mut pending);
            if accum.session_id.as_deref() != reported_session_id.as_deref()
                && let Some(session_id) = accum.session_id.as_deref()
            {
                host.on_session_id(session_id);
                reported_session_id = Some(session_id.to_string());
            }
            host.on_render_effects(effects);
            // Notify host of newly-complete tool_call entries (D-9 speculative
            // streaming hook). Snapshot the values to avoid holding a borrow
            // across the `await`.
            if accum.tool_calls.len() > tc_len_before {
                let new_calls: Vec<(usize, Value)> = accum.tool_calls[tc_len_before..]
                    .iter()
                    .enumerate()
                    .map(|(off, v)| (tc_len_before + off, v.clone()))
                    .collect();
                for (idx, tc) in new_calls {
                    host.on_tool_call_complete(idx, &tc).await;
                }
            }
            // Skill-exclusivity: reorder so skill calls execute before
            // non-skill calls within the same batch.
            prioritize_skill_tools(&mut pending);
            flush_pending_via_host(
                &mut pending,
                host,
                accum.session_id.as_deref(),
                &mut tool_results,
                &mut approval_results,
            )
            .await;
        }
    }

    // Tombstone on abort (timeout or cancellation).
    if matches!(
        abort,
        Some(astra_core::ErrorKind::StreamIdle) | Some(astra_core::ErrorKind::Cancelled)
    ) {
        accum.full_text.clear();
        accum.reasoning_content.clear();
        accum.tool_calls.clear();
        pending.clear();
        let msg = match abort {
            Some(astra_core::ErrorKind::StreamIdle) => {
                let timeout_used = if first_sse_frame_seen {
                    idle_post
                } else {
                    idle_pre
                };
                format!(
                    "Error: stream idle timeout after {}ms",
                    timeout_used.as_millis()
                )
            }
            Some(astra_core::ErrorKind::Cancelled) => "Cancelled by user".to_string(),
            _ => "Unknown abort".to_string(),
        };
        accum.error_message = Some(msg);
        host.on_render_effects(vec![SseRenderEffect::StopThinkingSpinner]);
    }

    let tail = framer.take_trailing_dispatch_blob();
    let ttft_ms = framer.ttft_ms;
    if !tail.trim().is_empty() {
        if !first_sse_frame_seen {
            host.on_first_sse_frame();
        }
        let tc_len_before = accum.tool_calls.len();
        let effects = dispatch_chat_turn_sse_event_block(&tail, &mut accum, &mut pending);
        if accum.session_id.as_deref() != reported_session_id.as_deref()
            && let Some(session_id) = accum.session_id.as_deref()
        {
            host.on_session_id(session_id);
        }
        host.on_render_effects(effects);
        if accum.tool_calls.len() > tc_len_before {
            let new_calls: Vec<(usize, Value)> = accum.tool_calls[tc_len_before..]
                .iter()
                .enumerate()
                .map(|(off, v)| (tc_len_before + off, v.clone()))
                .collect();
            for (idx, tc) in new_calls {
                host.on_tool_call_complete(idx, &tc).await;
            }
        }
        prioritize_skill_tools(&mut pending);
        flush_pending_via_host(
            &mut pending,
            host,
            accum.session_id.as_deref(),
            &mut tool_results,
            &mut approval_results,
        )
        .await;
    }

    // Degraded tool-call fallback: if the model emitted <invoke> or <tool_call>
    // XML in text instead of native tool_call events, recover them here.
    //
    // NOTE: keep in sync with bridge_llm_stream.rs (server-side equivalent).
    //
    // This only fires when tool_calls is empty (pure XML output). When the
    // model emits *both* native tool_call events and degraded XML text, the
    // native calls are already in accum.tool_calls and the XML stays in
    // full_text. The CLI strips that residual XML in consume_turn_sse
    // (stream_render.rs) when has_tool_calls is true.
    if accum.tool_calls.is_empty() {
        if let Some(parsed) =
            crate::xml_tool_call_fallback::parse_degraded_tool_calls(&accum.full_text)
        {
            accum.full_text =
                crate::xml_tool_call_fallback::strip_degraded_tool_calls(&accum.full_text);
            accum.tool_calls = parsed;
        }
    }

    host.on_stream_complete();

    (
        SseConsumeResult {
            accum,
            ttft_ms,
            tool_results,
            approval_results,
        },
        abort,
    )
}

/// Reorder so that skill tool requests come before all non-skill requests.
/// Preserves relative order within each group.
fn prioritize_skill_tools(items: &mut Vec<ChatTurnEdgePending>) {
    if items.len() < 2 {
        return;
    }
    // stable partition: skills first, rest after
    let mut skills = Vec::new();
    let mut rest = Vec::new();
    for item in std::mem::take(items) {
        match &item {
            ChatTurnEdgePending::ToolRequest { tool, .. } if tool == "skill" => {
                skills.push(item);
            }
            _ => rest.push(item),
        }
    }
    skills.extend(rest);
    *items = skills;
}

async fn flush_pending_via_host<H: SseStreamHost>(
    pending: &mut Vec<ChatTurnEdgePending>,
    host: &mut H,
    session_id: Option<&str>,
    tool_results: &mut Vec<EdgeToolExecResult>,
    approval_results: &mut Vec<EdgeApprovalResult>,
) {
    let items = std::mem::take(pending);
    let mut tool_batch: Vec<ToolBatchRequest> = Vec::new();
    let mut approval_requests: Vec<EdgeApprovalRequest> = Vec::new();

    for item in items {
        match item {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                tool,
                args,
            } => {
                if request_id.is_empty() || tool.is_empty() {
                    continue;
                }
                tool_batch.push(ToolBatchRequest {
                    request_id,
                    tool,
                    args,
                });
            }
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                approval_kind,
                detail,
            } => approval_requests.push(EdgeApprovalRequest {
                request_id,
                tool,
                approval_kind,
                detail,
            }),
            ChatTurnEdgePending::ApprovalBatchRequired { requests } => {
                approval_requests.extend(requests);
            }
        }
    }

    // Execute tools — the host decides whether to parallelize.
    if !tool_batch.is_empty() {
        let results = host.execute_tools_batch(tool_batch).await;
        tool_results.extend(results);
    }

    if approval_requests.len() > 1 {
        approval_results.extend(
            host.resolve_approvals_batch(&approval_requests, session_id)
                .await,
        );
    } else if let Some(request) = approval_requests.into_iter().next() {
        approval_results.push(
            host.resolve_approval(
                &request.request_id,
                &request.tool,
                request.approval_kind,
                session_id,
                request.detail.as_deref(),
            )
            .await,
        );
    }
}

// ─── Headless (no-op) host ───────────────────────────────────────────────────

/// A no-op host for headless/testing contexts.
///
/// - Render effects are silently discarded.
/// - Tool requests are denied with `"headless: not supported"`.
/// - Approval requests are auto-denied.
pub struct NoopSseStreamHost;

#[async_trait]
impl SseStreamHost for NoopSseStreamHost {
    fn on_render_effects(&mut self, _effects: Vec<SseRenderEffect>) {}

    fn on_stream_complete(&mut self) {}

    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &Value,
    ) -> EdgeToolExecResult {
        EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: "headless: tool execution not supported".to_string(),
            tool_result_fields: None,
            status: "error".to_string(),
            duration_ms: 0,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
        _approval_kind: ApprovalKind,
        _session_id: Option<&str>,
        _detail: Option<&str>,
    ) -> EdgeApprovalResult {
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: "deny".to_string(),
            reason: Some("headless: auto-deny".to_string()),
        }
    }
}

// ─── Recording host (for tests) ─────────────────────────────────────────────

/// A test host that records all render effects and returns canned results.
#[cfg(test)]
struct RecordingSseStreamHost {
    render_effects: Vec<SseRenderEffect>,
    tool_outputs: std::collections::HashMap<String, String>,
    approval_kinds: Vec<ApprovalKind>,
    stream_completed: bool,
}

#[cfg(test)]
impl RecordingSseStreamHost {
    fn new() -> Self {
        Self {
            render_effects: Vec::new(),
            tool_outputs: std::collections::HashMap::new(),
            approval_kinds: Vec::new(),
            stream_completed: false,
        }
    }

    fn with_tool_output(mut self, tool: &str, output: &str) -> Self {
        self.tool_outputs
            .insert(tool.to_string(), output.to_string());
        self
    }
}

#[cfg(test)]
#[async_trait]
impl SseStreamHost for RecordingSseStreamHost {
    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        self.render_effects.extend(effects);
    }

    fn on_stream_complete(&mut self) {
        self.stream_completed = true;
    }

    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &Value,
    ) -> EdgeToolExecResult {
        let output = self
            .tool_outputs
            .get(tool)
            .cloned()
            .unwrap_or_else(|| format!("mock output for {tool}"));
        EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output,
            tool_result_fields: None,
            status: "ok".to_string(),
            duration_ms: 1,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
        approval_kind: ApprovalKind,
        _session_id: Option<&str>,
        _detail: Option<&str>,
    ) -> EdgeApprovalResult {
        self.approval_kinds.push(approval_kind);
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: "allow".to_string(),
            reason: None,
        }
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use futures_util::stream;

    fn sse_event(typ: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{typ}\"{extra}}}\n\n")
    }

    fn chunks_from_sse(events: &str) -> Vec<Result<Vec<u8>, String>> {
        vec![Ok(events.as_bytes().to_vec())]
    }

    #[tokio::test]
    async fn noop_host_captures_text() {
        let events = format!(
            "{}{}",
            sse_event("text_delta", ",\"content\":\"hello \""),
            sse_event("text_delta", ",\"content\":\"world\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert_eq!(result.accum.full_text, "hello world");
        assert!(result.tool_results.is_empty());
        assert!(result.approval_results.is_empty());
    }

    #[tokio::test]
    async fn noop_host_captures_usage() {
        let events = sse_event(
            "usage",
            ",\"input_tokens\":100,\"output_tokens\":50,\"total_tokens\":150",
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert_eq!(result.accum.prompt_tokens, 100);
        assert_eq!(result.accum.completion_tokens, 50);
        assert!(result.accum.has_usage);
    }

    #[tokio::test]
    async fn recording_host_receives_render_effects() {
        let events = format!(
            "{}{}{}",
            sse_event("reasoning_delta", ",\"content\":\"think\""),
            sse_event("text_delta", ",\"content\":\"answer\""),
            sse_event("text_delta", ",\"content\":\" done\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        assert!(host.stream_completed);
        assert_eq!(result.accum.full_text, "answer done");
        assert_eq!(result.accum.reasoning_content, "think");

        // Should have received StreamText effects
        let text_effects: Vec<&str> = host
            .render_effects
            .iter()
            .filter_map(|e| match e {
                SseRenderEffect::StreamText(s) => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(text_effects.contains(&"answer"));
        assert!(text_effects.contains(&" done"));
    }

    #[tokio::test]
    async fn recording_host_tool_request_executed() {
        let events = sse_event(
            "tool_request",
            ",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo hi\"}",
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new().with_tool_output("bash", "hi\n");
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].request_id, "tr-1");
        assert_eq!(result.tool_results[0].tool, "bash");
        assert_eq!(result.tool_results[0].output, "hi\n");
        assert_eq!(result.tool_results[0].status, "ok");
    }

    #[tokio::test]
    async fn recording_host_approval_resolved() {
        let events = sse_event(
            "approval_required",
            ",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"",
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(result.approval_results[0].request_id, "ap-1");
        assert_eq!(result.approval_results[0].decision, "allow");
        assert_eq!(host.approval_kinds, vec![ApprovalKind::Standard]);
    }

    #[tokio::test]
    async fn empty_request_ids_are_skipped() {
        let events = format!(
            "{}{}",
            sse_event(
                "tool_request",
                ",\"request_id\":\"\",\"tool\":\"bash\",\"args\":{}"
            ),
            sse_event("approval_required", ",\"request_id\":\"\",\"tool\":\"x\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        assert!(result.tool_results.is_empty());
        assert!(result.approval_results.is_empty());
    }

    #[tokio::test]
    async fn session_id_captured() {
        let events = format!(
            "{}{}",
            sse_event("session_info", ",\"session_id\":\"sess-42\""),
            sse_event("run_started", ",\"run_id\":\"run-7\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert_eq!(result.accum.session_id.as_deref(), Some("sess-42"));
        assert_eq!(result.accum.run_id.as_deref(), Some("run-7"));
    }

    #[tokio::test]
    async fn session_id_hook_runs_before_tool_request_flush() {
        let events = format!(
            "{}{}",
            sse_event("session_info", ",\"session_id\":\"sess-hook\""),
            sse_event(
                "tool_request",
                ",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo hi\"}",
            ),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);

        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct SessionAwareHost(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl SseStreamHost for SessionAwareHost {
            fn on_render_effects(&mut self, _effects: Vec<SseRenderEffect>) {}

            fn on_stream_complete(&mut self) {}

            fn on_session_id(&mut self, session_id: &str) {
                self.0.lock().unwrap().push(format!("session:{session_id}"));
            }

            async fn execute_tool(
                &mut self,
                request_id: &str,
                tool: &str,
                args: &Value,
            ) -> EdgeToolExecResult {
                self.0.lock().unwrap().push(format!("tool:{request_id}"));
                EdgeToolExecResult {
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "ok".to_string(),
                    tool_result_fields: None,
                    status: "ok".to_string(),
                    duration_ms: 1,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _detail: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let mut host = SessionAwareHost(order.clone());
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.accum.session_id.as_deref(), Some("sess-hook"));
        assert_eq!(
            order.lock().unwrap().as_slice(),
            &["session:sess-hook".to_string(), "tool:tr-1".to_string()]
        );
    }

    #[tokio::test]
    async fn multi_chunk_framing() {
        // Split one event across two chunks
        let part1 = "data: {\"type\":\"text_delta\",\"content\":\"he";
        let part2 = "llo\"}\n\n";
        let chunks: Vec<Result<Vec<u8>, String>> =
            vec![Ok(part1.as_bytes().to_vec()), Ok(part2.as_bytes().to_vec())];
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert_eq!(result.accum.full_text, "hello");
    }

    #[tokio::test]
    async fn tool_call_events_captured() {
        // `has_tool_calls` is set by the `turn_complete` event
        let events = format!(
            "{}{}",
            sse_event(
                "tool_call_start",
                ",\"call_id\":\"tc-1\",\"tool\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"ls\\\"}\""
            ),
            sse_event("turn_complete", ",\"has_tool_calls\":true"),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert!(result.accum.has_tool_calls);
        assert_eq!(result.accum.tool_calls.len(), 1);
        assert_eq!(
            result.accum.tool_calls[0]["function"]["name"].as_str(),
            Some("bash")
        );
    }

    #[tokio::test]
    async fn noop_host_denies_tools_and_approvals() {
        let events = format!(
            "{}{}",
            sse_event(
                "tool_request",
                ",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{}"
            ),
            sse_event(
                "approval_required",
                ",\"request_id\":\"ap-1\",\"tool\":\"write_file\""
            ),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].status, "error");
        assert!(result.tool_results[0].output.contains("not supported"));

        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(result.approval_results[0].decision, "deny");
    }

    #[tokio::test]
    async fn error_event_captured() {
        let events = sse_event("error", ",\"message\":\"rate limited\"");
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        // dispatch prepends "Error: " to the message
        assert_eq!(
            result.accum.error_message.as_deref(),
            Some("Error: rate limited")
        );
    }

    #[tokio::test]
    async fn idle_watchdog_aborts_and_tombstones_state() {
        let events = sse_event("text_delta", ",\"content\":\"partial\"");
        // First chunk yields partial text, then the stream never yields again → idle timeout.
        let mut stream = stream::iter(chunks_from_sse(&events))
            .chain(stream::pending::<Result<Vec<u8>, String>>());

        let mut host = RecordingSseStreamHost::new();
        let timeout = std::time::Duration::from_millis(5);
        let (result, abort) =
            consume_sse_stream_cancellable(&mut stream, &mut host, timeout, None, Some(timeout))
                .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::StreamIdle));
        assert!(host.stream_completed);
        assert!(result.accum.full_text.is_empty());
        assert!(result.accum.reasoning_content.is_empty());
        assert!(result.accum.tool_calls.is_empty());
        assert!(
            result
                .accum
                .error_message
                .as_deref()
                .unwrap_or("")
                .contains("idle timeout")
        );
    }

    #[tokio::test]
    async fn cancellation_aborts_and_tombstones_state() {
        let events = sse_event("text_delta", ",\"content\":\"partial\"");
        // First chunk yields partial text, then the stream never yields again.
        let mut stream = stream::iter(chunks_from_sse(&events))
            .chain(stream::pending::<Result<Vec<u8>, String>>());

        let cancel_token = tokio_util::sync::CancellationToken::new();
        let token_for_cancel = cancel_token.clone();

        // Spawn a task that cancels after 5ms.
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            token_for_cancel.cancel();
        });

        let mut host = RecordingSseStreamHost::new();
        let (result, abort) = consume_sse_stream_cancellable(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(60_000), // Long timeout - won't fire
            Some(&cancel_token),
            None,
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert!(host.stream_completed);
        assert!(result.accum.full_text.is_empty());
        assert!(result.accum.reasoning_content.is_empty());
        assert!(result.accum.tool_calls.is_empty());
        assert!(
            result
                .accum
                .error_message
                .as_deref()
                .unwrap_or("")
                .contains("Cancelled")
        );
    }

    // ── Edge SSE resilience: transport error, malformed frames, pings ────────

    #[tokio::test]
    async fn transport_error_aborts_cleanly() {
        // Stream yields one good chunk then an error.
        let chunks: Vec<Result<Vec<u8>, String>> = vec![
            Ok(sse_event("text_delta", ",\"content\":\"partial\"").into_bytes()),
            Err("connection reset by peer".to_string()),
        ];
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let (_result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        // Unlike idle timeout, transport error does NOT tombstone — partial
        // state is preserved so the caller can decide what to do.
        // (The consumer breaks out of the loop immediately.)
    }

    #[tokio::test]
    async fn malformed_sse_frame_skipped_gracefully() {
        // Mix of garbage and valid events — valid events should still be captured.
        let raw = format!(
            "garbage line without data prefix\n\n{}not-json-data\n\n{}",
            sse_event("text_delta", ",\"content\":\"good\""),
            sse_event("text_delta", ",\"content\":\" stuff\""),
        );
        let chunks = chunks_from_sse(&raw);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert!(
            result.accum.full_text.contains("good"),
            "valid events should be captured despite garbage: {}",
            result.accum.full_text
        );
    }

    #[tokio::test]
    async fn ping_events_interleaved_with_content() {
        let events = format!(
            "{}{}{}{}{}",
            sse_event("session_info", ",\"session_id\":\"s1\""),
            sse_event("ping", ",\"ts\":1234567890"),
            sse_event("text_delta", ",\"content\":\"hello\""),
            sse_event("ping", ",\"ts\":1234567891"),
            sse_event("text_delta", ",\"content\":\" world\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.accum.full_text, "hello world");
        assert_eq!(result.accum.session_id.as_deref(), Some("s1"));
    }

    #[tokio::test]
    async fn multiple_tool_requests_executed_sequentially() {
        let events = format!(
            "{}{}",
            sse_event(
                "tool_request",
                ",\"request_id\":\"t1\",\"tool\":\"read_file\",\"args\":{\"path\":\"a.rs\"}"
            ),
            sse_event(
                "tool_request",
                ",\"request_id\":\"t2\",\"tool\":\"grep\",\"args\":{\"pattern\":\"TODO\"}"
            ),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new()
            .with_tool_output("read_file", "fn main() {}")
            .with_tool_output("grep", "line 10: TODO");
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.tool_results.len(), 2);
        assert_eq!(result.tool_results[0].request_id, "t1");
        assert_eq!(result.tool_results[0].output, "fn main() {}");
        assert_eq!(result.tool_results[1].request_id, "t2");
        assert_eq!(result.tool_results[1].output, "line 10: TODO");
    }

    #[tokio::test]
    async fn tool_request_then_text_delta_in_same_stream() {
        // Realistic: tool_request in round 1, then text_delta in round 2
        // (all within the same SSE stream from a single /chat/turn call)
        let events = format!(
            "{}{}{}",
            sse_event(
                "tool_request",
                ",\"request_id\":\"t1\",\"tool\":\"read_file\",\"args\":{\"path\":\"x\"}"
            ),
            sse_event("text_delta", ",\"content\":\"Based on the file, \""),
            sse_event("text_delta", ",\"content\":\"here is my analysis.\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new().with_tool_output("read_file", "file content");
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(
            result.accum.full_text,
            "Based on the file, here is my analysis."
        );
    }

    // ── prioritize_skill_tools unit tests ──────────────────────────────────

    fn make_tool_pending(tool: &str) -> ChatTurnEdgePending {
        ChatTurnEdgePending::ToolRequest {
            request_id: format!("req-{tool}"),
            tool: tool.to_string(),
            args: serde_json::json!({}),
        }
    }

    fn tool_name(item: &ChatTurnEdgePending) -> &str {
        match item {
            ChatTurnEdgePending::ToolRequest { tool, .. } => tool,
            _ => "approval",
        }
    }

    #[test]
    fn prioritize_puts_skill_before_others() {
        let mut items = vec![
            make_tool_pending("write_file"),
            make_tool_pending("bash"),
            make_tool_pending("skill"),
            make_tool_pending("grep"),
        ];
        super::prioritize_skill_tools(&mut items);

        assert_eq!(tool_name(&items[0]), "skill");
        assert_eq!(tool_name(&items[1]), "write_file");
        assert_eq!(tool_name(&items[2]), "bash");
        assert_eq!(tool_name(&items[3]), "grep");
    }

    #[test]
    fn prioritize_no_skill_preserves_order() {
        let mut items = vec![make_tool_pending("write_file"), make_tool_pending("bash")];
        super::prioritize_skill_tools(&mut items);

        assert_eq!(tool_name(&items[0]), "write_file");
        assert_eq!(tool_name(&items[1]), "bash");
    }

    #[test]
    fn prioritize_multiple_skills() {
        let mut items = vec![
            make_tool_pending("bash"),
            make_tool_pending("skill"),
            make_tool_pending("write_file"),
            make_tool_pending("skill"),
        ];
        super::prioritize_skill_tools(&mut items);

        assert_eq!(tool_name(&items[0]), "skill");
        assert_eq!(tool_name(&items[1]), "skill");
        assert_eq!(tool_name(&items[2]), "bash");
        assert_eq!(tool_name(&items[3]), "write_file");
    }

    // Helper: create a channel-backed stream for async tests.
    // Wrap tokio mpsc Receiver as a futures Stream for test use.
    struct RxStream(tokio::sync::mpsc::Receiver<Result<Vec<u8>, String>>);
    impl futures_util::Stream for RxStream {
        type Item = Result<Vec<u8>, String>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.0.poll_recv(cx)
        }
    }
    impl Unpin for RxStream {}

    fn test_channel() -> (tokio::sync::mpsc::Sender<Result<Vec<u8>, String>>, RxStream) {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        (tx, RxStream(rx))
    }

    // ── Deadlock regression: tool_request must execute inline during streaming ──

    /// Simulates bridge behavior: yields a tool_request, then waits for the
    /// host to execute it before yielding more data. If tool execution is
    /// deferred until after the stream ends, this will deadlock (timeout).
    ///
    /// Regression test for the bug where `split_and_defer_tools` during
    /// streaming caused CLI to wait for stream end while bridge waited for
    /// tool result via edge callback. Fix: removed defer mechanism entirely;
    /// skill exclusivity is handled server-side in agentic_loop_host.rs.
    #[tokio::test]
    async fn tool_request_executes_inline_not_deferred() {
        let (tx, rx) = test_channel();
        let mut stream = rx;

        let mut host = RecordingSseStreamHost::new().with_tool_output("bash", "commit abc123");

        // "Bridge" task: send tool_request, pause (simulating ledger wait),
        // then send final text once the tool has had time to execute inline.
        let bridge = tokio::spawn(async move {
            let tool_req = sse_event(
                "tool_request",
                ",\"request_id\":\"t1\",\"tool\":\"bash\",\"args\":{\"command\":\"git log -1\"}",
            );
            tx.send(Ok(tool_req.into_bytes())).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            let text = sse_event("text_delta", ",\"content\":\"Latest commit: abc123\"");
            tx.send(Ok(text.into_bytes())).await.unwrap();
            drop(tx);
        });

        // Must complete within 2s — if tools are deferred, bridge holds the
        // stream open waiting for tool result → idle timeout here.
        let result = tokio::time::timeout(
            tokio::time::Duration::from_secs(2),
            consume_sse_stream(
                &mut stream,
                &mut host,
                std::time::Duration::from_millis(500),
            ),
        )
        .await;

        assert!(
            result.is_ok(),
            "timed out — tool execution was likely deferred, causing deadlock"
        );
        let (result, abort) = result.unwrap();
        assert!(abort.is_none(), "unexpected abort: {abort:?}");
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].tool, "bash");
        assert_eq!(result.tool_results[0].output, "commit abc123");
        assert_eq!(result.accum.full_text, "Latest commit: abc123");
        bridge.await.unwrap();
    }

    /// Skill and non-skill tool_request in the same SSE block during
    /// streaming: skill must execute first.
    #[tokio::test]
    async fn skill_prioritized_over_regular_tool_in_same_block() {
        // Both tool requests in a SINGLE SSE block (no \n\n between them,
        // only one \n\n at the end). This simulates them arriving in the
        // same TCP chunk as a single framed event.
        let block = format!(
            "data: {{\"type\":\"tool_request\",\"request_id\":\"t-bash\",\"tool\":\"bash\",\"args\":{{}}}}\ndata: {{\"type\":\"tool_request\",\"request_id\":\"t-skill\",\"tool\":\"{}\",\"args\":{{}}}}\n\n",
            "skill"
        );
        let chunks: Vec<Result<Vec<u8>, String>> = vec![Ok(block.into_bytes())];
        let mut stream = stream::iter(chunks);

        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct OrderTrackingHost(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl SseStreamHost for OrderTrackingHost {
            fn on_render_effects(&mut self, _: Vec<SseRenderEffect>) {}
            fn on_stream_complete(&mut self) {}
            async fn execute_tool(
                &mut self,
                rid: &str,
                tool: &str,
                args: &Value,
            ) -> EdgeToolExecResult {
                self.0.lock().unwrap().push(tool.to_string());
                EdgeToolExecResult {
                    request_id: rid.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: format!("ok-{tool}"),
                    tool_result_fields: None,
                    status: "ok".to_string(),
                    duration_ms: 1,
                }
            }
            async fn resolve_approval(
                &mut self,
                rid: &str,
                _: &str,
                _: ApprovalKind,
                _: Option<&str>,
                _: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: rid.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let mut host = OrderTrackingHost(order.clone());
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.tool_results.len(), 2);
        let exec_order = order.lock().unwrap();
        assert_eq!(
            exec_order[0], "skill",
            "skill should execute before bash, got: {:?}",
            *exec_order
        );
        assert_eq!(exec_order[1], "bash");
    }

    /// Tool request in trailing bytes (incomplete SSE block that only
    /// completes via the framer's tail flush after stream EOF) must still
    /// be executed.
    #[tokio::test]
    async fn tail_flush_executes_tool_request() {
        // Send a complete text event, then a tool_request WITHOUT the
        // trailing \n\n — it stays in the framer buffer and gets flushed
        // as the tail blob after the stream ends.
        let complete = sse_event("text_delta", ",\"content\":\"hi\"");
        let partial_tool =
            "data: {\"type\":\"tool_request\",\"request_id\":\"t1\",\"tool\":\"bash\",\"args\":{}}";
        let chunks: Vec<Result<Vec<u8>, String>> =
            vec![Ok(format!("{complete}{partial_tool}").into_bytes())];
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new().with_tool_output("bash", "tail result");

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.accum.full_text, "hi");
        assert_eq!(
            result.tool_results.len(),
            1,
            "tail tool_request should be executed"
        );
        assert_eq!(result.tool_results[0].output, "tail result");
    }

    /// Unhappy: idle timeout with pending deferred tools — tombstoned, no
    /// tools executed.
    #[tokio::test]
    async fn idle_timeout_clears_deferred_tools() {
        let (tx, rx) = test_channel();
        let mut stream = rx;
        let mut host = RecordingSseStreamHost::new();

        let _hold = tx.clone();
        tx.send(Ok(sse_event(
            "reasoning_delta",
            ",\"content\":\"thinking...\"",
        )
        .into_bytes()))
            .await
            .unwrap();

        let timeout = std::time::Duration::from_millis(100);
        let (result, abort) =
            consume_sse_stream_cancellable(&mut stream, &mut host, timeout, None, Some(timeout))
                .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::StreamIdle));
        assert!(
            result.accum.full_text.is_empty(),
            "text should be tombstoned"
        );
        assert!(
            result.accum.reasoning_content.is_empty(),
            "reasoning should be tombstoned"
        );
        assert!(
            result.tool_results.is_empty(),
            "no tools should have executed"
        );
        assert!(
            result
                .accum
                .error_message
                .as_ref()
                .unwrap()
                .contains("idle timeout")
        );
    }

    /// Unhappy: cancellation with pending tool requests — tombstoned.
    #[tokio::test]
    async fn cancellation_clears_pending_and_deferred() {
        let (tx, rx) = test_channel();
        let mut stream = rx;
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut host = RecordingSseStreamHost::new();

        tx.send(Ok(
            sse_event("text_delta", ",\"content\":\"hello\"").into_bytes()
        ))
        .await
        .unwrap();

        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            cancel_clone.cancel();
        });

        let (result, abort) = consume_sse_stream_cancellable(
            &mut stream,
            &mut host,
            std::time::Duration::from_secs(10),
            Some(&cancel),
            None,
        )
        .await;

        assert_eq!(abort, Some(SseAbortReason::Cancelled));
        assert!(
            result
                .accum
                .error_message
                .as_ref()
                .unwrap()
                .contains("Cancelled")
        );
        assert!(
            result.accum.full_text.is_empty(),
            "text should be tombstoned on cancel"
        );
    }

    /// When `approval_required` and `tool_request` share the same `request_id`,
    /// verify both are processed: the approval is resolved first, then the
    /// tool executes. This mirrors the production flow where the cloud sends
    /// approval_required, waits for approval, then sends tool_request.
    #[tokio::test]
    async fn approval_then_tool_request_same_id_both_processed() {
        let events = format!(
            "{}{}",
            sse_event(
                "approval_required",
                ",\"request_id\":\"shared-1\",\"tool\":\"str_replace\",\"approval_kind\":\"standard\",\"detail\":\"src/x.rs\"",
            ),
            sse_event(
                "tool_request",
                ",\"request_id\":\"shared-1\",\"tool\":\"str_replace\",\"args\":{\"path\":\"src/x.rs\",\"old_str\":\"a\",\"new_str\":\"b\"}",
            ),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct TrackingHost(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl SseStreamHost for TrackingHost {
            fn on_render_effects(&mut self, _: Vec<SseRenderEffect>) {}
            fn on_stream_complete(&mut self) {}
            async fn execute_tool(
                &mut self,
                request_id: &str,
                tool: &str,
                args: &Value,
            ) -> EdgeToolExecResult {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("exec:{request_id}:{tool}"));
                EdgeToolExecResult {
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "ok".to_string(),
                    tool_result_fields: None,
                    status: "ok".to_string(),
                    duration_ms: 0,
                }
            }
            async fn resolve_approval(
                &mut self,
                request_id: &str,
                tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _detail: Option<&str>,
            ) -> EdgeApprovalResult {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("approve:{request_id}:{tool}"));
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let mut host = TrackingHost(ops.clone());
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        // Both approval and tool execution should be recorded.
        let recorded = ops.lock().unwrap();
        assert_eq!(
            recorded.len(),
            2,
            "expected approve + exec, got: {recorded:?}"
        );
        assert_eq!(recorded[0], "approve:shared-1:str_replace");
        assert_eq!(recorded[1], "exec:shared-1:str_replace");

        // Both results should be in the output.
        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(result.approval_results[0].decision, "allow");
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].request_id, "shared-1");
    }

    // ── XML invoke fallback ───────────────────────────────────────────────────

    #[tokio::test]
    async fn xml_invoke_in_text_is_recovered_as_tool_calls() {
        // Model degrades to <invoke> XML instead of native tool_call events.
        // consume_sse_stream must parse these and move them into accum.tool_calls.
        let xml_text = concat!(
            "I'll create the file now.\n",
            "<invoke name=\"write_file\">\n",
            "<parameter name=\"path\">server.js</parameter>\n",
            "<parameter name=\"content\">console.log('hi');</parameter>\n",
            "</invoke>",
        );
        let events = format!(
            "{}{}",
            sse_event(
                "text_delta",
                &format!(",\"content\":{}", serde_json::json!(xml_text))
            ),
            sse_event("usage", ",\"input_tokens\":100,\"output_tokens\":50"),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert_eq!(
            result.accum.tool_calls.len(),
            1,
            "expected 1 recovered tool call, got: {:?}",
            result.accum.tool_calls
        );
        assert_eq!(result.accum.tool_calls[0]["function"]["name"], "write_file");
        assert!(
            !result.accum.full_text.contains("<invoke"),
            "XML should be stripped from full_text, got: {}",
            result.accum.full_text
        );
        assert!(result.accum.full_text.contains("create the file"));
    }
}
