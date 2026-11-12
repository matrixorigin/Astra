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

use crate::agent_live_event::{AgentLiveEvent, AgentLiveEventKind, AgentLiveGap, AgentLiveSignal};
use crate::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, ChatTurnSseFramer, DurableRunTerminal,
    DurableRunTerminalStatus, EdgeApprovalRequest, SseRenderEffect, dispatch_decoded_sse_event,
    received_unix_ms, tool_request_execution_deadline,
};
use crate::sse::data_lines::validated_sse_event;
pub use crate::tool::policy::is_tool_concurrency_safe;
use crate::tool::policy::tool_batch_coalesce_duration;
use astra_thin_client::ApprovalKind;
use async_trait::async_trait;
use serde_json::Value;
use std::collections::HashMap;

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

/// Bound later executable requests and controls while a batch is in flight.
const MAX_EDGE_PENDING_BYTES: usize = 1024 * 1024;
const MISSING_SSE_DONE: &str = "Error: SSE stream ended before the terminal [DONE] marker";

#[derive(Clone, Copy)]
struct SseEventReceivedAt {
    monotonic: tokio::time::Instant,
    unix_ms: u64,
}

impl SseEventReceivedAt {
    fn now() -> Self {
        Self {
            monotonic: tokio::time::Instant::now(),
            unix_ms: received_unix_ms(),
        }
    }
}

/// Convert a durable terminal authored by the root run into the exact local
/// abort kind. A valid failed terminal can carry a domain interruption without
/// an ErrorKind; preserve that missing classification rather than inventing a
/// protocol violation or provider overload.
fn durable_terminal_abort_kind(terminal: &DurableRunTerminal) -> Option<astra_core::ErrorKind> {
    match terminal.status {
        DurableRunTerminalStatus::Cancelled => Some(astra_core::ErrorKind::Cancelled),
        DurableRunTerminalStatus::Failed => Some(
            terminal
                .error_kind
                .unwrap_or(astra_core::ErrorKind::Unknown),
        ),
        DurableRunTerminalStatus::Completed
        | DurableRunTerminalStatus::Delegated
        | DurableRunTerminalStatus::Paused => None,
    }
}

/// Stream idle timeout (pre-progress), fixed at [`STREAM_IDLE_TIMEOUT_MS`].
pub fn stream_idle_timeout() -> std::time::Duration {
    std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS)
}

/// Stream idle timeout (post-progress), fixed at [`STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS`].
pub fn stream_idle_timeout_after_progress() -> std::time::Duration {
    std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_AFTER_PROGRESS_MS)
}

// ─── Data types ──────────────────────────────────────────────────────────────

/// Result of executing an edge tool request via the host.
#[derive(Debug, Clone)]
pub struct EdgeToolExecResult {
    pub work_attribution: Option<astra_services::runs::WorkInvocationAttribution>,
    /// Server-owned evidence provenance, never read from tool-result metadata.
    pub execution_completion: Option<astra_turn_types::task_resolution::ToolExecutionEvidenceRef>,
    pub request_id: String,
    pub tool: String,
    pub args: Value,
    pub output: String,
    pub tool_result_fields: Option<serde_json::Map<String, Value>>,
    /// Machine-owned terminal status such as `"completed"`, `"failed"`, or
    /// `"skipped"`.  It is authoritative transport evidence, not prose for
    /// the model to interpret.
    pub status: String,
    pub duration_ms: u64,
}

impl crate::headless_tool_assembly::EdgeToolRoundRow for EdgeToolExecResult {
    fn work_attribution(&self) -> Option<&astra_services::runs::WorkInvocationAttribution> {
        self.work_attribution.as_ref()
    }
    fn execution_completion(
        &self,
    ) -> Option<&astra_turn_types::task_resolution::ToolExecutionEvidenceRef> {
        self.execution_completion.as_ref()
    }
    fn tool_name(&self) -> &str {
        &self.tool
    }
    fn tool_args(&self) -> &Value {
        &self.args
    }
    fn tool_output(&self) -> &str {
        &self.output
    }
    fn tool_execution_status(&self) -> Option<&str> {
        Some(&self.status)
    }
    fn tool_result_fields(&self) -> Option<&serde_json::Map<String, Value>> {
        self.tool_result_fields.as_ref()
    }
    fn tool_duration_ms(&self) -> u64 {
        self.duration_ms
    }
    fn tool_call_id(&self) -> Option<&str> {
        (!self.request_id.is_empty()).then_some(self.request_id.as_str())
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
    pub work_attribution: Option<astra_services::runs::WorkInvocationAttribution>,
    /// Canonical skill directory admitted for this exact invocation.
    pub admitted_skill_names: Option<Vec<String>>,
    pub session_id: String,
    pub run_id: String,
    pub turn_chain_id: String,
    pub request_id: String,
    /// Immutable server-issued read-only execution ceiling for this request.
    /// Keep it on the request so batching cannot lose the authority when the
    /// host advances to another SSE exchange.
    pub read_only_execution: bool,
    /// Immutable execution budget issued by the server for this invocation.
    pub execution_timeout_ms: u64,
    /// Effective local monotonic deadline, frozen when the request arrived.
    pub execution_deadline_at: tokio::time::Instant,
    /// Bash command ceiling. The delivery deadline also includes settlement
    /// grace and must not replace this executor-owned budget.
    pub command_timeout_cap_ms: Option<u64>,
    /// Immutable absolute server deadline retained until this exact invocation starts.
    pub execution_deadline_unix_ms: u64,
    pub tool: String,
    pub args: Value,
}

impl ToolBatchRequest {
    /// Rejection before execution, retaining the effective invocation arguments.
    pub fn rejected_result(&self, args: &Value, error: String) -> EdgeToolExecResult {
        let mut fields = serde_json::Map::new();
        astra_tools::execution_outcome::insert_not_executed_fact(&mut fields);
        EdgeToolExecResult {
            work_attribution: self.work_attribution.clone(),
            execution_completion: None,
            request_id: self.request_id.clone(),
            tool: self.tool.clone(),
            args: args.clone(),
            output: astra_text_utils::credential_redaction::redact_credentials_for_display(&error)
                .0,
            tool_result_fields: Some(fields),
            status: "failed".into(),
            duration_ms: 0,
        }
    }
}

// Keep tool requests inline as in the original pending-work lane.
#[allow(clippy::large_enum_variant)]
enum SsePending<C> {
    Work(ChatTurnEdgePending),
    Control(C),
}

fn pending_is_coalescible_tool_batch<C>(pending: &[SsePending<C>]) -> bool {
    let mut saw_tool = false;
    for item in pending {
        match item {
            SsePending::Control(_) if !saw_tool => {}
            SsePending::Work(ChatTurnEdgePending::ToolRequest { tool, args, .. })
                if is_tool_concurrency_safe(tool, Some(args)) =>
            {
                saw_tool = true
            }
            _ => return false,
        }
    }
    saw_tool
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
/// Observer and executor have disjoint mutable ownership so the stream can
/// continue accepting events while already-admitted Edge work is settling.
pub trait SseStreamHost: Send {
    type Observer: SseStreamObserver;
    type Edge: SseEdgeWork<Control = <Self::Observer as SseStreamObserver>::Control>;

    fn split(&mut self) -> (&mut Self::Observer, &mut Self::Edge);
}

#[async_trait]
pub trait SseStreamObserver: Send {
    type Control: Send;

    /// A cancelling attached client may still need the first Server run identity
    /// to cancel that exact execution. Opt in to a bounded identity-only drain;
    /// no tool or approval may execute during it. Other hosts stop immediately.
    fn cancelled_admission_grace(&self) -> std::time::Duration {
        std::time::Duration::ZERO
    }

    /// Whether a clean stream EOF must follow the `[DONE]` marker.
    fn requires_sse_done(&self) -> bool {
        false
    }

    /// Called for each syntactically valid JSON `data:` event after the shared
    /// typed dispatcher has accepted the containing SSE block.
    async fn on_accepted_sse_event(
        &mut self,
        _event: &Value,
    ) -> Result<Option<Self::Control>, String> {
        Ok(None)
    }

    /// Called only after the shared dispatcher observes the transport's
    /// terminal `[DONE]` marker.
    async fn on_sse_done(&mut self, _accum: &ChatTurnSseAccum) -> Result<(), String> {
        Ok(())
    }

    /// Called for each batch of render effects parsed from an SSE event block.
    /// CLI: prints text deltas, starts/stops thinking spinner.
    /// Headless: no-op.
    async fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>);

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
    fn on_session_id(&mut self, _session_id: &str) -> Option<Self::Control> {
        None
    }

    /// Observe a raw session identity before the accumulator rejects a conflict.
    fn on_session_identity(&mut self, _session_id: &str, _run_id: Option<&str>) {}

    /// Called after each event block updates the shared SSE accumulator.
    ///
    /// Hosts can use this to mirror session metadata, streamed assistant text,
    /// and usage counters into their own incremental recovery state.
    fn on_accum_update(&mut self, _accum: &ChatTurnSseAccum) {}

    /// Stop admitting new Edge work after the canonical dispatcher observes
    /// a final root failure or terminal. Already-started work remains owned and settles.
    fn on_edge_admission_closed(&mut self) {}

    /// Called when the server emits bounded, typed inter-agent communication
    /// evidence. This observation lane is intentionally separate from prompt
    /// messages and from rendering effects.
    fn on_agent_communication(&mut self, _event: astra_turn_types::AgentCommunicationEvent) {}

    /// Called when a delegated agent emits its typed live transcript event.
    /// This lane carries the exact agent identity and content boundaries across
    /// CLI, Server Only, and Edge+Server execution; hosts must not reconstruct
    /// it from parent tool-card text.
    async fn on_agent_live_event(&mut self, _event: AgentLiveEvent) {}

    /// Called when the transport had to drop coalescible agent live activity.
    /// Hosts must treat the corresponding transcript/projection as incomplete
    /// until it has been reconciled from durable state.
    async fn on_agent_live_gap(&mut self, _gap: AgentLiveGap) {}

    /// A local stream observer can fail independently of tool callbacks.
    /// The caller checks this only after already-owned work has settled.
    fn stream_observer_error(&self) -> Option<&'static str> {
        None
    }
}

#[async_trait]
pub trait SseEdgeWork: Send {
    type Control: Send;

    /// Apply an already-validated control at its original work boundary.
    fn apply_control(&mut self, control: Self::Control);

    /// Freeze the root identity for the next batch.
    fn prepare_stream(&mut self, _accum: &ChatTurnSseAccum) {}

    /// Reconcile the Server's exact wire-schema admission before an Edge
    /// executor applies its local binding, argument, permission and sandbox
    /// gates. This reconciliation must not wait on callback delivery or I/O.
    /// Hosts without a second local tool surface need no action.
    fn on_server_tool_surface_admission(
        &mut self,
        _request: &ToolBatchRequest,
    ) -> Result<(), String> {
        Ok(())
    }

    /// Whether a terminal host-side control-plane failure has made additional
    /// edge work unsafe. Implementations set this after an approval/tool-result
    /// callback cannot be acknowledged; the protocol loop then stops before a
    /// coalesced mutating tool can execute on stale local approval state.
    fn should_abort_edge_work(&self) -> bool {
        false
    }

    /// A local stream observer can fail independently of tool callbacks.
    /// The caller checks this only after already-owned work has settled.
    fn stream_observer_error(&self) -> Option<&'static str> {
        None
    }

    /// Execute a tool request that arrived via `tool_request` SSE event.
    /// Returns the execution result (output, status, duration).
    async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult;

    /// Resolve an approval request that arrived via `approval_required` SSE event.
    /// CLI: interactive prompt. Headless: auto-deny or ledger-based.
    ///
    /// `detail` is the raw command/path (for classifier matching);
    /// `display_label` is the rich preview string suitable for UI.
    /// Implementations should prefer `display_label` for output and
    /// fall back to `detail` when absent.
    #[allow(clippy::too_many_arguments)]
    async fn resolve_approval(
        &mut self,
        request_id: &str,
        tool: &str,
        approval_kind: ApprovalKind,
        session_id: Option<&str>,
        run_id: Option<&str>,
        detail: Option<&str>,
        display_label: Option<&str>,
    ) -> EdgeApprovalResult;

    /// Resolve a batch of approval requests in one interactive step when supported.
    async fn resolve_approvals_batch(
        &mut self,
        requests: &[EdgeApprovalRequest],
        session_id: Option<&str>,
        run_id: Option<&str>,
    ) -> Vec<EdgeApprovalResult> {
        let mut results = Vec::with_capacity(requests.len());
        for request in requests {
            results.push(
                self.resolve_approval(
                    &request.request_id,
                    &request.tool,
                    request.approval_kind,
                    session_id,
                    run_id,
                    request.detail.as_deref(),
                    request.display_label.as_deref(),
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
            let r = if self.should_abort_edge_work() {
                req.rejected_result(&req.args, "Execution stopped before tool dispatch".into())
            } else {
                self.execute_tool(&req).await
            };
            results.push(r);
        }
        results
    }

    /// Called after a tool execution result has been produced by the host.
    ///
    /// Hosts can mirror tool audit data into an interruption-safe snapshot
    /// before the wider turn finalization hooks run.
    fn on_tool_result(&mut self, _result: &EdgeToolExecResult) {}
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

    let (host, edge) = host.split();

    // EOF can arrive while Edge work is settling; replaying buffered events
    // afterwards must not poll an already-terminated source again.
    let mut chunks = chunks.fuse();
    let mut accum = ChatTurnSseAccum::default();
    let mut framer = ChatTurnSseFramer::new();
    let mut pending = Vec::<SsePending<<H::Observer as SseStreamObserver>::Control>>::new();
    let mut tool_results: Vec<EdgeToolExecResult> = Vec::new();
    let mut approval_results: Vec<EdgeApprovalResult> = Vec::new();
    let mut abort: Option<SseAbortReason> = None;
    let mut abort_message: Option<String> = None;
    let mut first_sse_frame_seen = false;
    let mut reported_session_id: Option<String> = None;
    let mut last_received_at = None;
    let admission_grace = host.cancelled_admission_grace();
    let mut cancelled_admission_deadline: Option<tokio::time::Instant> = None;

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
                let mut remaining = idle.saturating_sub(elapsed);
                if let Some(deadline) = cancelled_admission_deadline {
                    let grace_remaining =
                        deadline.saturating_duration_since(tokio::time::Instant::now());
                    if grace_remaining.is_zero() {
                        abort = Some(astra_core::ErrorKind::Cancelled);
                        break 'wait None;
                    }
                    remaining = remaining.min(grace_remaining);
                }
                if remaining.is_zero() {
                    break 'wait None; // idle timeout
                }
                let wait = remaining.min(tick);
                let r = if let Some(token) = cancel_token {
                    tokio::select! {
                        biased;
                        _ = token.cancelled(), if cancelled_admission_deadline.is_none() => {
                            if accum.run_id.is_none() && !admission_grace.is_zero() {
                                cancelled_admission_deadline = Some(tokio::time::Instant::now() + admission_grace);
                                continue;
                            }
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
                        if let Some(error) = host.stream_observer_error() {
                            abort = Some(astra_core::ErrorKind::StreamTransport);
                            abort_message = Some(error.to_string());
                            break 'wait None;
                        }
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
        let received_at = SseEventReceivedAt::now();
        last_received_at = Some(received_at);
        let mut input = bytes.as_slice();
        let mut terminal_marker_seen = false;
        while !input.is_empty() {
            let event_str = match framer.next_frame(&mut input) {
                Ok(Some(block)) => block,
                Ok(None) => break,
                Err(error) => {
                    abort = Some(astra_core::ErrorKind::StreamTransport);
                    abort_message = Some(format!("Error: invalid SSE response: {error}"));
                    break;
                }
            };
            let processed = process_sse_event_block(
                &event_str,
                host,
                &mut accum,
                &mut pending,
                &mut first_sse_frame_seen,
                &mut reported_session_id,
                Some(received_at),
                false,
                cancel_token,
            )
            .await;
            if let Err(error) = processed {
                abort = Some(error.kind);
                abort_message = Some(format!("Error: invalid SSE protocol: {error}"));
                break;
            }
            if let Some(kind) = root_terminal_abort(&accum) {
                abort = Some(kind);
                break;
            }
            // Capture only the accepted identity needed by explicit-cancel
            // settlement, then stop before accepting executable work.
            if !admission_grace.is_zero()
                && cancel_token.is_some_and(|token| token.is_cancelled())
                && (accum.run_id.is_some() || !pending.is_empty() || accum.stream_complete)
            {
                abort.get_or_insert(astra_core::ErrorKind::Cancelled);
                break;
            }
            if host.stream_observer_error().is_some() {
                break;
            }
            if accum.stream_complete {
                terminal_marker_seen = true;
                break;
            }
        }
        if abort.is_some() {
            break;
        }

        // Parallel tool calls often arrive as several adjacent SSE
        // `tool_request` frames. If we flush immediately after the first
        // frame, the first long-running agent spawn action blocks the socket reader
        // and the later spawn frames cannot join the same batch. Coalesce only
        // requests that are already classified as concurrency-safe, and only
        // for a tiny window; side-effectful tools still execute inline to avoid
        // the bridge/result deadlock guarded by `tool_request_executes_inline_not_deferred`.
        while !terminal_marker_seen
            && host.stream_observer_error().is_none()
            && pending_is_coalescible_tool_batch(&pending)
        {
            if cancel_token.is_some_and(|token| token.is_cancelled()) {
                abort = Some(astra_core::ErrorKind::Cancelled);
                break;
            }
            let (next, reached_eof) = if let Some(token) = cancel_token {
                tokio::select! {
                    biased;
                    _ = token.cancelled() => {
                        abort = Some(astra_core::ErrorKind::Cancelled);
                        (None, false)
                    }
                    r = tokio::time::timeout(
                        tool_batch_coalesce_duration(),
                        chunks.next(),
                    ) => match r {
                        Ok(Some(item)) => (Some(item), false),
                        Ok(None) => (None, true),
                        Err(_) => (None, false),
                    },
                }
            } else {
                match tokio::time::timeout(tool_batch_coalesce_duration(), chunks.next()).await {
                    Ok(Some(item)) => (Some(item), false),
                    Ok(None) => (None, true),
                    Err(_) => (None, false),
                }
            };

            let Some(next) = next else {
                if reached_eof {
                    if abort_message.is_none() {
                        match framer.take_trailing_dispatch_blob() {
                            Ok(tail) if !tail.trim().is_empty() => {
                                match process_sse_event_block(
                                    &tail,
                                    host,
                                    &mut accum,
                                    &mut pending,
                                    &mut first_sse_frame_seen,
                                    &mut reported_session_id,
                                    last_received_at,
                                    false,
                                    cancel_token,
                                )
                                .await
                                {
                                    Ok(_) => {
                                        terminal_marker_seen |= accum.stream_complete;
                                        abort = root_terminal_abort(&accum);
                                    }
                                    Err(error) => {
                                        abort = Some(error.kind);
                                        abort_message =
                                            Some(format!("Error: invalid SSE protocol: {error}"));
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                abort = Some(astra_core::ErrorKind::StreamTransport);
                                abort_message = Some(format!(
                                    "Error: invalid UTF-8 in model SSE response: {error}"
                                ));
                            }
                        }
                    }
                }
                break;
            };
            match next {
                Ok(bytes) => {
                    let received_at = SseEventReceivedAt::now();
                    last_received_at = Some(received_at);
                    let mut saw_event = false;
                    let mut all_events_extended_batch = true;
                    let mut input = bytes.as_slice();
                    while !input.is_empty() {
                        let event_str = match framer.next_frame(&mut input) {
                            Ok(Some(block)) => block,
                            Ok(None) => break,
                            Err(error) => {
                                abort = Some(astra_core::ErrorKind::StreamTransport);
                                abort_message =
                                    Some(format!("Error: invalid SSE response: {error}"));
                                break;
                            }
                        };
                        saw_event = true;
                        match process_sse_event_block(
                            &event_str,
                            host,
                            &mut accum,
                            &mut pending,
                            &mut first_sse_frame_seen,
                            &mut reported_session_id,
                            Some(received_at),
                            false,
                            cancel_token,
                        )
                        .await
                        {
                            Ok(extends_batch) => {
                                all_events_extended_batch &= extends_batch;
                            }
                            Err(error) => {
                                abort = Some(error.kind);
                                abort_message =
                                    Some(format!("Error: invalid SSE protocol: {error}"));
                                break;
                            }
                        }
                        if let Some(kind) = root_terminal_abort(&accum) {
                            abort = Some(kind);
                            break;
                        }
                        if host.stream_observer_error().is_some() {
                            break;
                        }
                        if accum.stream_complete {
                            terminal_marker_seen = true;
                            break;
                        }
                    }
                    if terminal_marker_seen
                        || host.stream_observer_error().is_some()
                        || (saw_event && !all_events_extended_batch)
                    {
                        break;
                    }
                }
                Err(_) => {
                    abort = Some(astra_core::ErrorKind::StreamTransport);
                    break;
                }
            }
        }
        if abort.is_some() {
            break;
        }
        // The observer and executor have disjoint owners. Process accepted
        // events while the current work batch settles, retaining only later
        // executable work and controls on the bounded pending lane.
        let mut sse_done_deferred = false;
        while !pending.is_empty() {
            let mut pending_bytes = 0usize;
            prioritize_skill_tools(&mut pending);
            edge.prepare_stream(&accum);
            let session_id = accum.session_id.clone();
            let run_id = accum.run_id.clone();
            let mut current = std::mem::take(&mut pending);
            let mut finished_reading = terminal_marker_seen;
            let mut done_deferred = false;
            let mut flush_done = false;
            {
                let flush = flush_pending_via_host(
                    &mut current,
                    edge,
                    session_id.as_deref(),
                    run_id.as_deref(),
                    &mut tool_results,
                    &mut approval_results,
                );
                tokio::pin!(flush);
                let cancel_wait = async {
                    match cancel_token {
                        Some(token) => token.cancelled().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::pin!(cancel_wait);
                while !flush_done && !finished_reading {
                    let next = tokio::select! {
                        biased;
                        _ = &mut flush => {
                            flush_done = true;
                            continue;
                        },
                        _ = &mut cancel_wait => {
                            abort.get_or_insert(astra_core::ErrorKind::Cancelled);
                            finished_reading = true;
                            continue;
                        },
                        next = chunks.next() => next,
                    };
                    let Some(next) = next else {
                        match framer.take_trailing_dispatch_blob() {
                            Ok(tail) if !tail.trim().is_empty() && abort_message.is_none() => {
                                let before = pending.len();
                                let processed = {
                                    let dispatch = process_sse_event_block(
                                        &tail,
                                        host,
                                        &mut accum,
                                        &mut pending,
                                        &mut first_sse_frame_seen,
                                        &mut reported_session_id,
                                        last_received_at,
                                        true,
                                        cancel_token,
                                    );
                                    tokio::pin!(dispatch);
                                    tokio::select! {
                                        result = &mut dispatch => result,
                                        _ = &mut flush => {
                                            flush_done = true;
                                            dispatch.await
                                        }
                                    }
                                };
                                if let Err(error) = processed {
                                    abort = Some(error.kind);
                                    abort_message =
                                        Some(format!("Error: invalid SSE protocol: {error}"));
                                    if let Some(token) = cancel_token {
                                        token.cancel();
                                    }
                                } else if pending.len() > before {
                                    pending_bytes = pending_bytes.saturating_add(tail.len());
                                    if pending_bytes > MAX_EDGE_PENDING_BYTES {
                                        abort = Some(astra_core::ErrorKind::StreamTransport);
                                        abort_message = Some(format!(
                                            "Error: pending Edge work exceeded {} bytes while another batch was running",
                                            MAX_EDGE_PENDING_BYTES
                                        ));
                                        if let Some(token) = cancel_token {
                                            token.cancel();
                                        }
                                    }
                                }
                            }
                            Ok(_) => {}
                            Err(error) => {
                                abort = Some(astra_core::ErrorKind::StreamTransport);
                                abort_message =
                                    Some(format!("Error: invalid SSE response: {error}"));
                                if let Some(token) = cancel_token {
                                    token.cancel();
                                }
                            }
                        }
                        if abort.is_none() {
                            abort = root_terminal_abort(&accum);
                        }
                        if accum.stream_complete {
                            terminal_marker_seen = true;
                            done_deferred = true;
                        }
                        if host.requires_sse_done() && !accum.stream_complete && abort.is_none() {
                            abort = Some(astra_core::ErrorKind::StreamTransport);
                            abort_message = Some(MISSING_SSE_DONE.to_string());
                            if let Some(token) = cancel_token {
                                token.cancel();
                            }
                        }
                        finished_reading = true;
                        continue;
                    };
                    let bytes = match next {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            abort = Some(astra_core::ErrorKind::StreamTransport);
                            if let Some(token) = cancel_token {
                                token.cancel();
                            }
                            finished_reading = true;
                            continue;
                        }
                    };
                    let received_at = SseEventReceivedAt::now();
                    last_received_at = Some(received_at);
                    let mut input = bytes.as_slice();
                    while !input.is_empty() {
                        let block = match framer.next_frame(&mut input) {
                            Ok(Some(block)) => block,
                            Ok(None) => break,
                            Err(error) => {
                                abort = Some(astra_core::ErrorKind::StreamTransport);
                                abort_message =
                                    Some(format!("Error: invalid SSE response: {error}"));
                                if let Some(token) = cancel_token {
                                    token.cancel();
                                }
                                finished_reading = true;
                                break;
                            }
                        };
                        let before = pending.len();
                        let processed = {
                            let dispatch = process_sse_event_block(
                                &block,
                                host,
                                &mut accum,
                                &mut pending,
                                &mut first_sse_frame_seen,
                                &mut reported_session_id,
                                Some(received_at),
                                true,
                                cancel_token,
                            );
                            tokio::pin!(dispatch);
                            if flush_done {
                                dispatch.await
                            } else {
                                tokio::select! {
                                    result = &mut dispatch => result,
                                    _ = &mut flush => {
                                        flush_done = true;
                                        dispatch.await
                                    }
                                }
                            }
                        };
                        if let Err(error) = processed {
                            abort = Some(error.kind);
                            abort_message = Some(format!("Error: invalid SSE protocol: {error}"));
                            if let Some(token) = cancel_token {
                                token.cancel();
                            }
                            finished_reading = true;
                            break;
                        }
                        if pending.len() > before {
                            pending_bytes = pending_bytes.saturating_add(block.len());
                            if pending_bytes > MAX_EDGE_PENDING_BYTES {
                                abort = Some(astra_core::ErrorKind::StreamTransport);
                                abort_message = Some(format!(
                                    "Error: pending Edge work exceeded {} bytes while another batch was running",
                                    MAX_EDGE_PENDING_BYTES
                                ));
                                if let Some(token) = cancel_token {
                                    token.cancel();
                                }
                                finished_reading = true;
                                break;
                            }
                        }
                        if let Some(kind) = root_terminal_abort(&accum) {
                            abort = Some(kind);
                            finished_reading = true;
                            break;
                        }
                        if accum.stream_complete {
                            terminal_marker_seen = true;
                            done_deferred = true;
                            finished_reading = true;
                            break;
                        }
                        if let Some(error) = host.stream_observer_error() {
                            abort = Some(astra_core::ErrorKind::StreamTransport);
                            abort_message = Some(error.to_string());
                            if let Some(token) = cancel_token {
                                token.cancel();
                            }
                            finished_reading = true;
                            break;
                        }
                    }
                }
                if !flush_done {
                    flush.await;
                }
            }
            sse_done_deferred |= done_deferred;
            if abort.is_some() {
                apply_pending_controls(&mut pending, edge);
                break;
            }
            if terminal_marker_seen && pending.is_empty() {
                break;
            }
        }
        if sse_done_deferred
            && abort.is_none()
            && let Err(error) = host.on_sse_done(&accum).await
        {
            abort = Some(astra_core::ErrorKind::StreamTransport);
            abort_message = Some(format!("Error: invalid SSE protocol: {error}"));
        }
        if abort.is_none()
            && let Some(error) = host.stream_observer_error()
        {
            abort = Some(astra_core::ErrorKind::StreamTransport);
            abort_message = Some(error.to_string());
        }
        if abort.is_some() {
            break;
        }
        if terminal_marker_seen {
            break;
        }
    }

    // EOF makes a final unterminated event dispatchable. Apply it before
    // deciding whether any already-accepted work may execute.
    if abort.is_none() && !accum.stream_complete {
        match framer.take_trailing_dispatch_blob() {
            Ok(tail) if !tail.trim().is_empty() => {
                match process_sse_event_block(
                    &tail,
                    host,
                    &mut accum,
                    &mut pending,
                    &mut first_sse_frame_seen,
                    &mut reported_session_id,
                    last_received_at,
                    false,
                    cancel_token,
                )
                .await
                {
                    Ok(_) => {
                        abort = root_terminal_abort(&accum);
                        if abort.is_none() {
                            prioritize_skill_tools(&mut pending);
                            edge.prepare_stream(&accum);
                            flush_pending_via_host(
                                &mut pending,
                                edge,
                                accum.session_id.as_deref(),
                                accum.run_id.as_deref(),
                                &mut tool_results,
                                &mut approval_results,
                            )
                            .await;
                            if let Some(error) = host.stream_observer_error() {
                                abort = Some(astra_core::ErrorKind::StreamTransport);
                                abort_message = Some(error.to_string());
                            }
                        }
                    }
                    Err(error) => {
                        abort = Some(error.kind);
                        abort_message = Some(format!("Error: invalid SSE protocol: {error}"));
                    }
                }
            }
            Ok(_) => {}
            Err(error) => {
                abort = Some(astra_core::ErrorKind::StreamTransport);
                abort_message = Some(format!(
                    "Error: invalid UTF-8 in model SSE response: {error}"
                ));
            }
        }
    }
    let ttft_ms = framer.ttft_ms;
    if host.requires_sse_done() && abort.is_none() && !accum.stream_complete {
        abort = Some(astra_core::ErrorKind::StreamTransport);
        abort_message = Some(MISSING_SSE_DONE.to_string());
    }

    // A validated root failure retains observed output. Local transport and
    // protocol failures still tombstone it; all aborts discard executable work.
    if abort.is_some() {
        let root_terminal_matches_abort = accum
            .run_terminal
            .as_ref()
            .and_then(durable_terminal_abort_kind)
            == abort;
        let interrupted_terminal = abort_message.is_none()
            && root_terminal_matches_abort
            && accum.has_authoritative_interrupted_terminal();
        let preserve_failed_output = interrupted_terminal
            || (abort_message.is_none()
                && accum.has_authoritative_failed_terminal()
                && root_terminal_matches_abort);
        accum.stream_complete = false;
        let prior_contract_message = (accum.error_kind
            == Some(astra_core::ErrorKind::ContractViolation))
        .then(|| accum.error_message.clone())
        .flatten();
        if accum.error_kind == Some(astra_core::ErrorKind::ContractViolation) {
            abort = accum.error_kind;
        }
        if !interrupted_terminal {
            accum.error_kind = abort;
        }
        // A canonical save failure invalidates completion, not text already
        // received. Keep its partial display while discarding executable work.
        if !preserve_failed_output
            && accum.error_code.as_deref() != Some("canonical_persistence_failed")
        {
            accum.full_text.clear();
            accum.reasoning_content.clear();
        }
        accum.tool_calls.clear();
        apply_pending_controls(&mut pending, edge);
        let msg = match abort {
            Some(astra_core::ErrorKind::ContractViolation) => prior_contract_message
                .or(abort_message)
                .unwrap_or_else(|| "Invalid durable root terminal".to_string()),
            Some(astra_core::ErrorKind::StreamIdle) if !preserve_failed_output => {
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
            Some(astra_core::ErrorKind::Cancelled) => accum
                .run_terminal
                .as_ref()
                .map(|terminal| format!("Server run {} was cancelled", terminal.run_id))
                .unwrap_or_else(|| "Cancelled by user".to_string()),
            Some(astra_core::ErrorKind::ServerError) => accum
                .run_terminal
                .as_ref()
                .map(|terminal| {
                    terminal
                        .error
                        .clone()
                        .unwrap_or_else(|| format!("Server run {} failed", terminal.run_id))
                })
                .unwrap_or_else(|| "Server run failed".to_string()),
            Some(astra_core::ErrorKind::StreamTransport) => abort_message
                .or_else(|| {
                    accum
                        .run_terminal
                        .as_ref()
                        .and_then(|terminal| terminal.error.clone())
                })
                .unwrap_or_else(|| {
                    "Error: stream transport ended while reading model response".to_string()
                }),
            Some(kind) => accum
                .run_terminal
                .as_ref()
                .and_then(|terminal| terminal.error.clone())
                .unwrap_or_else(|| format!("Server run failed [{}]", kind.as_str())),
            None => unreachable!("abort tombstone requires an abort kind"),
        };
        accum.error_message = Some(msg);
        host.on_render_effects(vec![SseRenderEffect::StopThinkingSpinner])
            .await;
    }

    // Provider text is display data. Only typed Server tool requests can
    // authorize local execution; provider-level degraded calls are resolved
    // by the Server before this transport boundary.

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

fn stream_protocol_error(message: impl Into<String>) -> astra_core::ClassifiedError {
    astra_core::ClassifiedError::new(astra_core::ErrorKind::StreamTransport, message)
}

fn root_terminal_abort(accum: &ChatTurnSseAccum) -> Option<astra_core::ErrorKind> {
    accum
        .run_terminal
        .as_ref()
        .and_then(durable_terminal_abort_kind)
}

#[allow(clippy::too_many_arguments)]
async fn process_sse_event_block<H: SseStreamObserver>(
    event_str: &str,
    host: &mut H,
    accum: &mut ChatTurnSseAccum,
    pending: &mut Vec<SsePending<H::Control>>,
    first_sse_frame_seen: &mut bool,
    reported_session_id: &mut Option<String>,
    received_at: Option<SseEventReceivedAt>,
    defer_sse_done: bool,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<bool, astra_core::ClassifiedError> {
    // `[DONE]` establishes the only terminal transport boundary.  The
    // consumer normally stops before this function can be called again, but
    // retain the guard here so a future caller cannot forward late typed agent
    // events before the dispatcher gets a chance to ignore them.
    if accum.stream_complete {
        return Ok(false);
    }
    let parsed = validated_sse_event(event_str).map_err(stream_protocol_error)?;
    let lifecycle = parsed
        .events
        .first()
        .and_then(|event| event.get("type"))
        .and_then(Value::as_str);
    if lifecycle == Some("run_finished")
        && parsed.events[0]
            .get("run_id")
            .and_then(Value::as_str)
            .is_none_or(|id| id.trim().is_empty())
    {
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            "invalid durable run lifecycle: run_finished event omitted its durable run_id",
        ));
    }
    let prior_contract_failure = accum.error_kind == Some(astra_core::ErrorKind::ContractViolation);
    if matches!(lifecycle, Some("session_info" | "error"))
        && let Some(event) = parsed.events.first()
        && let Some(session_id) = event
            .get("session_id")
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
    {
        host.on_session_identity(session_id, event.get("run_id").and_then(Value::as_str));
    }
    if !*first_sse_frame_seen {
        *first_sse_frame_seen = true;
        host.on_first_sse_frame();
    }
    let mut decoded_work = Vec::new();
    for event in &parsed.events {
        match event.get("type").and_then(Value::as_str) {
            Some("agent_communication") => {
                match serde_json::from_value::<astra_turn_types::AgentCommunicationEvent>(
                    event.clone(),
                ) {
                    Ok(event) => host.on_agent_communication(event),
                    Err(error) => tracing::warn!(
                        target: "astra_turn_core::sse",
                        %error,
                        "ignored malformed agent communication SSE evidence"
                    ),
                }
            }
            Some("agent_live_event") => match agent_live_event_from_sse(event) {
                Ok(event) => host.on_agent_live_event(event).await,
                Err(error) => tracing::warn!(
                    target: "astra_turn_core::sse",
                    %error,
                    "ignored malformed agent live SSE evidence"
                ),
            },
            Some("agent_live_gap") => match agent_live_gap_from_sse(event) {
                Ok(gap) => host.on_agent_live_gap(gap).await,
                Err(error) => tracing::warn!(
                    target: "astra_turn_core::sse",
                    %error,
                    "ignored malformed agent live gap SSE evidence"
                ),
            },
            _ => {}
        }
    }
    let (effects, pending_cleared) = dispatch_decoded_sse_event(&parsed, accum, &mut decoded_work);
    if matches!(
        lifecycle,
        Some("session_info" | "run_started" | "run_finished")
    ) && !prior_contract_failure
        && accum.error_kind == Some(astra_core::ErrorKind::ContractViolation)
    {
        let message = format!(
            "invalid durable run lifecycle: {}",
            accum
                .error_message
                .as_deref()
                .unwrap_or("invalid root identity or terminal"),
        );
        accum.error_message = Some(message.clone());
        return Err(astra_core::ClassifiedError::new(
            astra_core::ErrorKind::ContractViolation,
            message,
        ));
    }
    if lifecycle == Some("run_finished")
        && accum
            .run_terminal
            .as_ref()
            .and_then(durable_terminal_abort_kind)
            .is_some()
        && let Some(token) = cancel_token
    {
        token.cancel();
    }
    if pending_cleared {
        // A root terminal revokes executable work, not already accepted
        // control acknowledgements. Apply those at their original boundary
        // after any earlier work has settled.
        pending.retain(|item| matches!(item, SsePending::Control(_)));
        host.on_edge_admission_closed();
    }
    if let Some(received_at) = received_at {
        for item in &mut decoded_work {
            if let ChatTurnEdgePending::ToolRequest {
                execution_timeout_ms,
                execution_deadline_unix_ms,
                execution_deadline_at,
                ..
            } = item
            {
                *execution_deadline_at = tool_request_execution_deadline(
                    *execution_timeout_ms,
                    *execution_deadline_unix_ms,
                    received_at.monotonic,
                    received_at.unix_ms,
                );
            }
        }
    }
    let extends_coalescible_batch = {
        !decoded_work.is_empty()
            && decoded_work.iter().all(|item| match item {
                ChatTurnEdgePending::ToolRequest { tool, args, .. } => {
                    is_tool_concurrency_safe(tool, Some(args))
                }
                _ => false,
            })
    };
    if accum.session_id.as_deref() != reported_session_id.as_deref()
        && let Some(session_id) = accum.session_id.as_deref()
    {
        if let Some(control) = host.on_session_id(session_id) {
            pending.push(SsePending::Control(control));
        }
        *reported_session_id = Some(session_id.to_string());
    }
    pending.extend(decoded_work.into_iter().map(SsePending::Work));
    host.on_accum_update(accum);
    host.on_render_effects(effects).await;
    for event in &parsed.events {
        if let Some(control) = host
            .on_accepted_sse_event(event)
            .await
            .map_err(stream_protocol_error)?
        {
            pending.push(SsePending::Control(control));
        }
    }
    if parsed.stream_finished && !defer_sse_done {
        host.on_sse_done(accum)
            .await
            .map_err(stream_protocol_error)?;
    }
    Ok(extends_coalescible_batch)
}

fn agent_live_event_from_sse(event: &Value) -> Result<AgentLiveEvent, String> {
    let run_id = required_sse_string(event, "run_id")?;
    let agent_id = required_sse_string(event, "agent_id")?;
    let kind = match required_sse_string(event, "event_kind")?.as_str() {
        "output_delta" => AgentLiveEventKind::OutputDelta {
            model_item_id: event
                .get("model_item_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            text: required_sse_string(event, "content")?,
        },
        "thinking_delta" => AgentLiveEventKind::ThinkingDelta {
            model_item_id: event
                .get("model_item_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            text: required_sse_string(event, "content")?,
        },
        "status" => AgentLiveEventKind::Status {
            text: required_sse_string(event, "content")?,
        },
        "signal" => AgentLiveEventKind::Signal(
            serde_json::from_value::<AgentLiveSignal>(
                event
                    .get("signal")
                    .cloned()
                    .ok_or_else(|| "agent_live_event missing signal".to_string())?,
            )
            .map_err(|error| format!("invalid agent live signal: {error}"))?,
        ),
        "tool_started" => AgentLiveEventKind::ToolStarted {
            name: required_sse_string(event, "name")?,
            description: required_sse_string(event, "description")?,
            tool_use_id: required_sse_string(event, "tool_use_id")?,
        },
        "tool_completed" => AgentLiveEventKind::ToolCompleted {
            name: required_sse_string(event, "name")?,
            description: required_sse_string(event, "description")?,
            status: required_sse_string(event, "status")?,
            duration_ms: required_sse_u64(event, "duration_ms")?,
            output_summary: optional_sse_string(event, "output_summary"),
            output: optional_sse_string(event, "output"),
            tool_use_id: required_sse_string(event, "tool_use_id")?,
        },
        "agent_terminated" => AgentLiveEventKind::AgentTerminated {
            termination: serde_json::from_value(
                event
                    .get("termination")
                    .cloned()
                    .ok_or_else(|| "agent_live_event missing termination".to_string())?,
            )
            .map_err(|error| format!("invalid agent terminal status: {error}"))?,
            duration_ms: required_sse_u64(event, "duration_ms")?,
            reason: optional_sse_string(event, "reason"),
        },
        other => return Err(format!("unknown agent live event kind: {other}")),
    };
    Ok(AgentLiveEvent {
        run_id,
        agent_id,
        kind,
    })
}

fn agent_live_gap_from_sse(event: &Value) -> Result<AgentLiveGap, String> {
    let dropped_event_count = event
        .get("dropped_event_count")
        .and_then(Value::as_u64)
        .filter(|count| *count > 0)
        .ok_or_else(|| "agent live gap requires a positive dropped_event_count".to_string())?;
    Ok(AgentLiveGap {
        run_id: required_sse_string(event, "run_id")?,
        agent_id: required_sse_string(event, "agent_id")?,
        dropped_event_count,
    })
}

fn required_sse_string(event: &Value, field: &str) -> Result<String, String> {
    event
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| format!("agent_live_event missing {field}"))
}

fn optional_sse_string(event: &Value, field: &str) -> Option<String> {
    event
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToString::to_string)
}

fn required_sse_u64(event: &Value, field: &str) -> Result<u64, String> {
    event
        .get(field)
        .and_then(Value::as_u64)
        .ok_or_else(|| format!("agent_live_event missing {field}"))
}

/// Prioritize skills within each contiguous work batch, never across a
/// validated execution-control boundary.
fn prioritize_skill_tools<C>(items: &mut Vec<SsePending<C>>) {
    let mut skills = Vec::new();
    let mut rest = Vec::new();
    for item in std::mem::take(items) {
        match item {
            SsePending::Work(work) => {
                if matches!(&work, ChatTurnEdgePending::ToolRequest { tool, .. } if tool == "skill")
                {
                    skills.push(SsePending::Work(work));
                } else {
                    rest.push(SsePending::Work(work));
                }
            }
            SsePending::Control(control) => {
                items.append(&mut skills);
                items.append(&mut rest);
                items.push(SsePending::Control(control));
            }
        }
    }
    items.append(&mut skills);
    items.append(&mut rest);
}

fn apply_pending_controls<H: SseEdgeWork>(pending: &mut Vec<SsePending<H::Control>>, host: &mut H) {
    for item in std::mem::take(pending) {
        if let SsePending::Control(control) = item {
            host.apply_control(control);
        }
    }
}

async fn flush_pending_via_host<H: SseEdgeWork>(
    pending: &mut Vec<SsePending<H::Control>>,
    host: &mut H,
    fallback_session_id: Option<&str>,
    fallback_run_id: Option<&str>,
    tool_results: &mut Vec<EdgeToolExecResult>,
    approval_results: &mut Vec<EdgeApprovalResult>,
) {
    let mut work = Vec::new();
    for item in std::mem::take(pending) {
        match item {
            SsePending::Work(item) => work.push(item),
            SsePending::Control(control) => {
                flush_work_batch_via_host(
                    &mut work,
                    host,
                    fallback_session_id,
                    fallback_run_id,
                    tool_results,
                    approval_results,
                )
                .await;
                host.apply_control(control);
            }
        }
    }
    flush_work_batch_via_host(
        &mut work,
        host,
        fallback_session_id,
        fallback_run_id,
        tool_results,
        approval_results,
    )
    .await;
}

async fn flush_work_batch_via_host<H: SseEdgeWork>(
    pending: &mut Vec<ChatTurnEdgePending>,
    host: &mut H,
    fallback_session_id: Option<&str>,
    fallback_run_id: Option<&str>,
    tool_results: &mut Vec<EdgeToolExecResult>,
    approval_results: &mut Vec<EdgeApprovalResult>,
) {
    let items = std::mem::take(pending);
    // Retain the server clock authority until the concrete edge executor can
    // both enforce it and post an authenticated terminal callback.
    let mut tool_batch: Vec<ToolBatchRequest> = Vec::new();
    let mut approval_requests: Vec<EdgeApprovalRequest> = Vec::new();

    for item in items {
        match item {
            ChatTurnEdgePending::ToolRequest {
                work_attribution,
                admitted_skill_names,
                session_id,
                run_id,
                turn_chain_id,
                request_id,
                schema_admitted_by_server,
                read_only_execution,
                execution_deadline_unix_ms,
                execution_timeout_ms,
                execution_deadline_at,
                command_timeout_cap_ms,
                tool,
                args,
            } => {
                if request_id.is_empty() || tool.is_empty() {
                    continue;
                }
                let session_id = if session_id.is_empty() {
                    fallback_session_id.unwrap_or("").to_string()
                } else {
                    session_id
                };
                let run_id = if run_id.is_empty() {
                    fallback_run_id.unwrap_or("").to_string()
                } else {
                    run_id
                };
                if !schema_admitted_by_server {
                    continue;
                }
                let request = ToolBatchRequest {
                    work_attribution,
                    admitted_skill_names,
                    session_id,
                    run_id,
                    turn_chain_id,
                    request_id,
                    execution_timeout_ms,
                    execution_deadline_at,
                    command_timeout_cap_ms,
                    execution_deadline_unix_ms,
                    read_only_execution,
                    tool,
                    args,
                };
                if host.should_abort_edge_work() && host.stream_observer_error().is_none() {
                    let result = request.rejected_result(
                        &request.args,
                        "Execution stopped before tool dispatch".into(),
                    );
                    host.on_tool_result(&result);
                    tool_results.push(result);
                    continue;
                }
                if let Err(error) = host.on_server_tool_surface_admission(&request) {
                    let result = request.rejected_result(&request.args, error);
                    host.on_tool_result(&result);
                    tool_results.push(result);
                    continue;
                }
                tool_batch.push(request);
            }
            ChatTurnEdgePending::ApprovalRequired {
                session_id,
                run_id,
                request_id,
                tool,
                approval_kind,
                detail,
                display_label,
            } => approval_requests.push(EdgeApprovalRequest {
                session_id,
                run_id,
                request_id,
                tool,
                approval_kind,
                detail,
                display_label,
            }),
            ChatTurnEdgePending::ApprovalBatchRequired {
                session_id,
                run_id,
                mut requests,
            } => {
                for request in &mut requests {
                    if request.session_id.is_none() {
                        request.session_id.clone_from(&session_id);
                    }
                    if request.run_id.is_none() {
                        request.run_id.clone_from(&run_id);
                    }
                }
                approval_requests.extend(requests);
            }
        }
    }

    // Approvals MUST resolve before tools execute. Pre-coalescing the
    // event-at-a-time flush naturally enforced this (each
    // approval_required event was processed and resolved before the
    // next event arrived). Now that we coalesce concurrency-safe tool
    // requests, both approvals and tools may sit in `pending`
    // together — so we explicitly run approvals first to preserve
    // the invariant pinned by
    // `approval_then_tool_request_same_id_both_processed`.
    //
    // Practical impact: a `str_replace` whose `approval_required`
    // event arrived just before its `tool_request` would otherwise
    // execute the edit BEFORE the user / ledger granted permission.
    // Preserve the producer-authored durable identity. A live stream can
    // legitimately contain projected interactions for concurrent descendants;
    // grouping only by the consuming stream would post an approval to the
    // wrong run and strand the actual owner until its long timeout.
    let scoped_approval_groups =
        group_approval_requests(approval_requests, fallback_session_id, fallback_run_id);
    for (session_id, run_id, requests) in scoped_approval_groups {
        // A callback failure makes the durable owner unknown. Do not prompt or
        // post approvals for unrelated descendants after that point: besides
        // being unsafe, each callback has its own bounded retry and would turn
        // one failure into O(number of runs) user-visible latency.
        if host.should_abort_edge_work() {
            approval_results.extend(requests.into_iter().map(|request| EdgeApprovalResult {
                request_id: request.request_id,
                decision: "deny".into(),
                reason: Some("Approval interrupted before a decision was submitted".into()),
            }));
            continue;
        }
        if requests.len() > 1 {
            approval_results.extend(
                host.resolve_approvals_batch(&requests, session_id.as_deref(), run_id.as_deref())
                    .await,
            );
        } else if let Some(request) = requests.into_iter().next() {
            approval_results.push(
                host.resolve_approval(
                    &request.request_id,
                    &request.tool,
                    request.approval_kind,
                    session_id.as_deref(),
                    run_id.as_deref(),
                    request.detail.as_deref(),
                    request.display_label.as_deref(),
                )
                .await,
            );
        }
    }

    if host.should_abort_edge_work() && host.stream_observer_error().is_none() {
        for request in tool_batch {
            let result = request.rejected_result(
                &request.args,
                "Execution stopped before tool dispatch".into(),
            );
            host.on_tool_result(&result);
            tool_results.push(result);
        }
        return;
    }

    if !tool_batch.is_empty() {
        let results = host.execute_tools_batch(tool_batch).await;
        for result in results {
            host.on_tool_result(&result);
            tool_results.push(result);
        }
    }
}

type ScopedApprovalGroup = (Option<String>, Option<String>, Vec<EdgeApprovalRequest>);

/// Group approvals by their durable owner in first-seen order.
///
/// A hash index keeps this O(n) for concurrent multi-session streams while the
/// separate vector preserves deterministic prompt/result ordering. Producer
/// identity always wins; the consuming stream is only a legacy fallback.
fn group_approval_requests(
    requests: Vec<EdgeApprovalRequest>,
    fallback_session_id: Option<&str>,
    fallback_run_id: Option<&str>,
) -> Vec<ScopedApprovalGroup> {
    let mut groups = Vec::<ScopedApprovalGroup>::new();
    let mut group_indexes = HashMap::<(Option<String>, Option<String>), usize>::new();

    for request in requests {
        let session_id = request
            .session_id
            .clone()
            .or_else(|| fallback_session_id.map(ToString::to_string));
        let run_id = request
            .run_id
            .clone()
            .or_else(|| fallback_run_id.map(ToString::to_string));
        let key = (session_id.clone(), run_id.clone());
        if let Some(index) = group_indexes.get(&key).copied() {
            groups[index].2.push(request);
        } else {
            let index = groups.len();
            group_indexes.insert(key, index);
            groups.push((session_id, run_id, vec![request]));
        }
    }

    groups
}

// ─── Headless (no-op) host ───────────────────────────────────────────────────

/// A no-op host for headless/testing contexts.
///
/// - Render effects are silently discarded.
/// - Tool requests are denied with `"headless: not supported"`.
/// - Approval requests are auto-denied.
#[derive(Default)]
pub struct NoopSseStreamHost {
    observer: NoopSseObserver,
    edge: NoopSseEdge,
}

#[doc(hidden)]
#[derive(Default)]
pub struct NoopSseObserver;
#[doc(hidden)]
#[derive(Default)]
pub struct NoopSseEdge;

impl SseStreamHost for NoopSseStreamHost {
    type Observer = NoopSseObserver;
    type Edge = NoopSseEdge;

    fn split(&mut self) -> (&mut Self::Observer, &mut Self::Edge) {
        (&mut self.observer, &mut self.edge)
    }
}

#[async_trait]
impl SseStreamObserver for NoopSseObserver {
    type Control = ();

    async fn on_render_effects(&mut self, _effects: Vec<SseRenderEffect>) {}

    fn on_stream_complete(&mut self) {}
}

#[async_trait]
impl SseEdgeWork for NoopSseEdge {
    type Control = ();

    fn apply_control(&mut self, (): ()) {}

    async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
        let request_id = request.request_id.as_str();
        let tool = request.tool.as_str();
        let args = &request.args;
        EdgeToolExecResult {
            work_attribution: None,
            execution_completion: None,
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: "headless: tool execution not supported".to_string(),
            tool_result_fields: None,
            status: "failed".to_string(),
            duration_ms: 0,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
        _approval_kind: ApprovalKind,
        _session_id: Option<&str>,
        _run_id: Option<&str>,
        _detail: Option<&str>,
        _display_label: Option<&str>,
    ) -> EdgeApprovalResult {
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: "deny".to_string(),
            reason: Some("headless: auto-deny".to_string()),
        }
    }
}

// ─── Recording host (for tests) ─────────────────────────────────────────────

/// A recording host with separate observation and tool owners.
#[cfg(test)]
struct RecordingSseStreamHost {
    observer: RecordingObserver,
    edge: RecordingEdge,
}
#[cfg(test)]
struct RecordingObserver {
    cancelled_admission_grace: std::time::Duration,
    cancel_after_accepted_event: Option<tokio_util::sync::CancellationToken>,
    render_effects: Vec<SseRenderEffect>,
    agent_communications: Vec<astra_turn_types::AgentCommunicationEvent>,
    agent_live_events: Vec<AgentLiveEvent>,
    agent_live_gaps: Vec<AgentLiveGap>,
    require_sse_done: bool,
    accepted_sse_events: Vec<Value>,
    rejected_sse_event_type: Option<&'static str>,
    done_count: usize,
    stream_completed: bool,
    fail_observer_on_tick: bool,
    fail_observer_on_render: bool,
    admission_closures: usize,
    observer_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(test)]
struct RecordingEdge {
    tool_outputs: std::collections::HashMap<String, String>,
    admitted_deadlines: Vec<(String, tokio::time::Instant, tokio::time::Instant)>,
    approval_kinds: Vec<ApprovalKind>,
    approval_session_ids: Vec<Option<String>>,
    approval_run_ids: Vec<Option<String>>,
    tool_delay: Option<std::time::Duration>,
    tool_gate: Option<(
        std::sync::Arc<tokio::sync::Notify>,
        tokio_util::sync::CancellationToken,
    )>,
    abort_edge_work_after_approval: bool,
    edge_work_aborted: bool,
    fail_observer_on_tool: bool,
    observer_failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
}
#[cfg(test)]
impl RecordingSseStreamHost {
    fn new() -> Self {
        let observer_failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        Self {
            observer: RecordingObserver {
                cancelled_admission_grace: std::time::Duration::ZERO,
                cancel_after_accepted_event: None,
                render_effects: Vec::new(),
                agent_communications: Vec::new(),
                agent_live_events: Vec::new(),
                agent_live_gaps: Vec::new(),
                require_sse_done: false,
                accepted_sse_events: Vec::new(),
                rejected_sse_event_type: None,
                done_count: 0,
                stream_completed: false,
                fail_observer_on_tick: false,
                fail_observer_on_render: false,
                admission_closures: 0,
                observer_failed: observer_failed.clone(),
            },
            edge: RecordingEdge {
                tool_outputs: std::collections::HashMap::new(),
                admitted_deadlines: Vec::new(),
                approval_kinds: Vec::new(),
                approval_session_ids: Vec::new(),
                approval_run_ids: Vec::new(),
                tool_delay: None,
                tool_gate: None,
                abort_edge_work_after_approval: false,
                edge_work_aborted: false,
                fail_observer_on_tool: false,
                observer_failed,
            },
        }
    }
    fn with_tool_output(mut self, tool: &str, output: &str) -> Self {
        self.edge
            .tool_outputs
            .insert(tool.to_string(), output.to_string());
        self
    }
    fn with_tool_delay(mut self, delay: std::time::Duration) -> Self {
        self.edge.tool_delay = Some(delay);
        self
    }
    fn strict(mut self) -> Self {
        self.observer.require_sse_done = true;
        self
    }
    fn abort_edge_work_after_approval(mut self) -> Self {
        self.edge.abort_edge_work_after_approval = true;
        self
    }
}
#[cfg(test)]
impl SseStreamHost for RecordingSseStreamHost {
    type Observer = RecordingObserver;
    type Edge = RecordingEdge;
    fn split(&mut self) -> (&mut Self::Observer, &mut Self::Edge) {
        (&mut self.observer, &mut self.edge)
    }
}
#[cfg(test)]
#[async_trait]
impl SseStreamObserver for RecordingObserver {
    type Control = ();
    fn stream_observer_error(&self) -> Option<&'static str> {
        self.observer_failed
            .load(std::sync::atomic::Ordering::SeqCst)
            .then_some("stream_observer_closed")
    }

    fn cancelled_admission_grace(&self) -> std::time::Duration {
        self.cancelled_admission_grace
    }

    fn requires_sse_done(&self) -> bool {
        self.require_sse_done
    }

    fn on_edge_admission_closed(&mut self) {
        self.admission_closures += 1;
    }

    fn on_idle_tick(&mut self) {
        if self.fail_observer_on_tick {
            self.observer_failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    async fn on_accepted_sse_event(&mut self, event: &Value) -> Result<Option<()>, String> {
        if event.get("type").and_then(Value::as_str) == self.rejected_sse_event_type
            && self.rejected_sse_event_type.is_some()
        {
            return Err("injected observation callback failure".into());
        }
        self.accepted_sse_events.push(event.clone());
        if let Some(cancel) = &self.cancel_after_accepted_event {
            cancel.cancel();
        }
        Ok(None)
    }

    async fn on_sse_done(&mut self, accum: &ChatTurnSseAccum) -> Result<(), String> {
        assert!(accum.stream_complete);
        self.done_count += 1;
        Ok(())
    }

    async fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        if self.fail_observer_on_render {
            self.observer_failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.render_effects.extend(effects);
    }

    fn on_stream_complete(&mut self) {
        self.stream_completed = true;
    }

    fn on_agent_communication(&mut self, event: astra_turn_types::AgentCommunicationEvent) {
        self.agent_communications.push(event);
    }

    async fn on_agent_live_event(&mut self, event: AgentLiveEvent) {
        self.agent_live_events.push(event);
    }

    async fn on_agent_live_gap(&mut self, gap: AgentLiveGap) {
        self.agent_live_gaps.push(gap);
    }
}
#[cfg(test)]
#[async_trait]
impl SseEdgeWork for RecordingEdge {
    type Control = ();
    fn apply_control(&mut self, (): ()) {}
    fn stream_observer_error(&self) -> Option<&'static str> {
        self.observer_failed
            .load(std::sync::atomic::Ordering::SeqCst)
            .then_some("stream_observer_closed")
    }

    fn should_abort_edge_work(&self) -> bool {
        self.edge_work_aborted
            || self
                .observer_failed
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn on_server_tool_surface_admission(
        &mut self,
        request: &ToolBatchRequest,
    ) -> Result<(), String> {
        self.admitted_deadlines.push((
            request.request_id.clone(),
            request.execution_deadline_at,
            tokio::time::Instant::now(),
        ));
        Ok(())
    }

    async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
        let request_id = request.request_id.as_str();
        let tool = request.tool.as_str();
        let args = &request.args;
        if let Some((started, release)) = &self.tool_gate {
            started.notify_one();
            release.cancelled().await;
        }
        if let Some(delay) = self.tool_delay {
            tokio::time::sleep(delay).await;
        }
        if self.fail_observer_on_tool {
            self.observer_failed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        let output = self
            .tool_outputs
            .get(tool)
            .cloned()
            .unwrap_or_else(|| format!("mock output for {tool}"));
        EdgeToolExecResult {
            work_attribution: None,
            execution_completion: None,
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output,
            tool_result_fields: None,
            status: "completed".to_string(),
            duration_ms: 1,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
        approval_kind: ApprovalKind,
        session_id: Option<&str>,
        run_id: Option<&str>,
        _detail: Option<&str>,
        _display_label: Option<&str>,
    ) -> EdgeApprovalResult {
        self.approval_kinds.push(approval_kind);
        self.approval_session_ids
            .push(session_id.map(std::string::ToString::to_string));
        self.approval_run_ids
            .push(run_id.map(std::string::ToString::to_string));
        self.edge_work_aborted |= self.abort_edge_work_after_approval;
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
    struct TestEdgeOnly<T> {
        observer: super::NoopSseObserver,
        edge: T,
    }
    impl<T> TestEdgeOnly<T> {
        fn new(edge: T) -> Self {
            Self {
                observer: super::NoopSseObserver,
                edge,
            }
        }
    }
    impl<T: super::SseEdgeWork<Control = ()>> super::SseStreamHost for TestEdgeOnly<T> {
        type Observer = super::NoopSseObserver;
        type Edge = T;
        fn split(&mut self) -> (&mut Self::Observer, &mut Self::Edge) {
            (&mut self.observer, &mut self.edge)
        }
    }
    impl<T> std::ops::Deref for TestEdgeOnly<T> {
        type Target = T;
        fn deref(&self) -> &T {
            &self.edge
        }
    }
    impl<T> std::ops::DerefMut for TestEdgeOnly<T> {
        fn deref_mut(&mut self) -> &mut T {
            &mut self.edge
        }
    }
    struct TestDuplex<T> {
        observer: T,
        edge: T,
    }
    impl<T: Clone> TestDuplex<T> {
        fn new(edge: T) -> Self {
            Self {
                observer: edge.clone(),
                edge,
            }
        }
    }
    impl<T> super::SseStreamHost for TestDuplex<T>
    where
        T: super::SseStreamObserver<Control = ()> + super::SseEdgeWork<Control = ()>,
    {
        type Observer = T;
        type Edge = T;
        fn split(&mut self) -> (&mut Self::Observer, &mut Self::Edge) {
            (&mut self.observer, &mut self.edge)
        }
    }

    use super::*;
    use futures_util::StreamExt;
    use futures_util::stream;

    fn sse_event(typ: &str, extra: &str) -> String {
        let admission = if typ == "tool_request" {
            ",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"command_timeout_cap_ms\":30000,\"execution_deadline_unix_ms\":4102444800000"
        } else {
            ""
        };
        format!("data: {{\"type\":\"{typ}\"{admission}{extra}}}\n\n")
    }

    fn chunks_from_sse(events: &str) -> Vec<Result<Vec<u8>, String>> {
        vec![Ok(events.as_bytes().to_vec())]
    }

    #[tokio::test]
    async fn terminal_contract_failure_differs_from_truncated_stream() {
        for (events, expected) in [
            (
                format!(
                    "{}{}",
                    sse_event("session_info", ",\"run_id\":\"root\""),
                    sse_event(
                        "run_finished",
                        ",\"run_id\":\"root\",\"status\":\"invalid\""
                    )
                ),
                astra_core::ErrorKind::ContractViolation,
            ),
            (
                "data: {broken".to_string(),
                astra_core::ErrorKind::StreamTransport,
            ),
        ] {
            let mut source = stream::iter(vec![Ok(events.into_bytes())]);
            let mut host = NoopSseStreamHost::default();
            let (_, abort) =
                consume_sse_stream(&mut source, &mut host, std::time::Duration::from_secs(1)).await;
            assert_eq!(abort, Some(expected));
        }
    }

    #[tokio::test]
    async fn late_session_info_owns_root_terminal_scope() {
        let events = format!(
            "{}{}{}{}",
            sse_event("run_started", ",\"run_id\":\"legacy-root\""),
            sse_event("session_info", ",\"run_id\":\"canonical-root\""),
            sse_event(
                "run_finished",
                ",\"run_id\":\"legacy-root\",\"status\":\"cancelled\""
            ),
            sse_event(
                "run_finished",
                ",\"run_id\":\"canonical-root\",\"status\":\"cancelled\""
            )
        );
        let mut source = stream::iter(vec![Ok(events.into_bytes())]);
        let mut host = NoopSseStreamHost::default();
        let (result, abort) =
            consume_sse_stream(&mut source, &mut host, std::time::Duration::from_secs(1)).await;
        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert_eq!(result.accum.run_terminal.unwrap().run_id, "canonical-root");
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
        let mut host = NoopSseStreamHost::default();
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
    async fn multiline_event_reaches_host_and_done_precedes_later_bad_bytes() {
        let bytes = b"data:{\"type\":\"text_delta\",\rdata: \"content\":\"hello\"}\n\r\ndata:[DONE]\r\rdata: \xff\n\n";
        for chunks in [
            vec![bytes.to_vec()],
            bytes.chunks(7).map(<[u8]>::to_vec).collect(),
        ] {
            let chunks = chunks.into_iter().map(Ok::<_, String>);
            let mut stream = stream::iter(chunks);
            let mut host = NoopSseStreamHost::default();
            let (result, abort) = consume_sse_stream(
                &mut stream,
                &mut host,
                std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
            )
            .await;
            assert!(abort.is_none(), "{abort:?}");
            assert_eq!(result.accum.full_text, "hello");
            assert!(result.accum.stream_complete);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn closed_observer_detaches_while_server_stream_is_idle() {
        let bytes = sse_event("text_delta", ",\"content\":\"partial\"").into_bytes();
        let mut stream = stream::iter(vec![Ok(bytes)]).chain(stream::pending());
        let mut host = RecordingSseStreamHost::new();
        host.observer.fail_observer_on_tick = true;
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, std::time::Duration::from_secs(60)).await;
        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert_eq!(
            result.accum.error_message.as_deref(),
            Some("stream_observer_closed")
        );
        assert!(result.accum.full_text.is_empty());
    }

    #[tokio::test]
    async fn closed_observer_settles_accepted_tool_before_detaching() {
        let event = sse_event(
            "tool_request",
            ",\"request_id\":\"t1\",\"tool\":\"bash\",\"args\":{}",
        );
        let mut stream = stream::iter(chunks_from_sse(&event));
        let mut host = RecordingSseStreamHost::new();
        host.observer.fail_observer_on_render = true;
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, std::time::Duration::from_secs(5)).await;
        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert_eq!(
            result.accum.error_message.as_deref(),
            Some("stream_observer_closed")
        );
        assert_eq!(result.tool_results.len(), 1);
        assert_ne!(result.tool_results[0].status, "completed");
    }

    #[tokio::test]
    async fn observer_failure_after_tail_tool_settles_and_clears_partial_output() {
        let text = sse_event("text_delta", ",\"content\":\"partial\"");
        let tool = sse_event(
            "tool_request",
            ",\"request_id\":\"t1\",\"tool\":\"bash\",\"args\":{}",
        );
        let bytes = format!("{}{}", text, tool.trim_end()).into_bytes();
        let mut stream = stream::iter(vec![Ok(bytes)]);
        let mut host = RecordingSseStreamHost::new();
        host.edge.fail_observer_on_tool = true;
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, std::time::Duration::from_secs(5)).await;
        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert_eq!(
            result.accum.error_message.as_deref(),
            Some("stream_observer_closed")
        );
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].status, "completed");
        assert!(result.accum.full_text.is_empty());
    }

    #[tokio::test]
    async fn durable_cancel_is_observed_while_edge_tool_is_running() {
        struct CancelAwareToolHost {
            token: tokio_util::sync::CancellationToken,
            observed_cancel: bool,
            live_gaps: u64,
        }

        #[async_trait]
        impl SseEdgeWork for CancelAwareToolHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}

            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.token.cancelled().await;
                self.observed_cancel = true;
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "cancelled locally".to_string(),
                    tool_result_fields: None,
                    status: "failed".to_string(),
                    duration_ms: 1,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let first = format!(
            "{}{}",
            // The live POST response binds its root identity in the transport
            // bootstrap `session_info`; a separate run_started frame is not
            // guaranteed. Cancellation probing must follow that real wire
            // contract rather than a replay-only fixture shape.
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event(
                "tool_request",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\",\"turn_chain_id\":\"chain-1\",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{}"
            )
        );
        for status in ["cancelled", "failed"] {
            let kind = if status == "cancelled" {
                "cancelled"
            } else {
                "budget_exhausted"
            };
            let second = format!(
                "{}{}{}{}{}{}{}{}",
                // Root streams are aggregate observability surfaces: fanout child
                // lifecycle is legal here and must not steal terminal authority.
                sse_event("run_started", ",\"run_id\":\"child-run\""),
                sse_event(
                    "run_finished",
                    ",\"run_id\":\"child-run\",\"status\":\"future_child_state\"",
                ),
                sse_event("usage", ",\"input_tokens\":10,\"output_tokens\":5"),
                sse_event(
                    "error",
                    &format!(
                        ",\"message\":\"received terminal reason\",\"error_code\":\"{kind}\",\"metadata\":{{\"boundary\":\"received\"}}"
                    )
                ),
                sse_event(
                    "tool_request",
                    ",\"session_id\":\"session-1\",\"run_id\":\"root-run\",\"request_id\":\"must-not-run\",\"tool\":\"bash\",\"args\":{}"
                ),
                sse_event(
                    "run_finished",
                    &format!(
                        ",\"run_id\":\"root-run\",\"status\":\"{status}\",\"error_kind\":\"{kind}\""
                    )
                ),
                sse_event("usage", ",\"input_tokens\":999,\"output_tokens\":999"),
                sse_event(
                    "agent_live_gap",
                    ",\"run_id\":\"root-run\",\"agent_id\":\"root\",\"dropped_event_count\":99"
                )
            );
            let source =
                stream::iter(vec![Ok(first.as_bytes().to_vec())]).chain(stream::once(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                    Ok(second.into_bytes())
                }));
            let mut source = Box::pin(source);
            let token = tokio_util::sync::CancellationToken::new();
            let mut host = TestEdgeOnly::new(CancelAwareToolHost {
                token: token.clone(),
                observed_cancel: false,
                live_gaps: 0,
            });

            let (result, abort) = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                consume_sse_stream_cancellable(
                    &mut source,
                    &mut host,
                    stream_idle_timeout(),
                    Some(&token),
                    None,
                ),
            )
            .await
            .expect("durable cancellation must not wait for the Edge tool deadline");

            assert_eq!(abort, astra_core::ErrorKind::parse_tag(kind));
            assert!(host.observed_cancel);
            assert_eq!(
                host.live_gaps, 0,
                "terminal tail must not leak aggregated live facts"
            );
            let terminal = result.accum.run_terminal.expect("typed terminal retained");
            assert_eq!(terminal.run_id, "root-run");
            assert_eq!(
                terminal.status,
                DurableRunTerminalStatus::from_wire(status).unwrap()
            );
            assert!(result.accum.tool_calls.is_empty());
            assert_eq!(result.accum.prompt_tokens, 10);
            assert_eq!(result.accum.completion_tokens, 5);
            assert_eq!(result.accum.error_code.as_deref(), Some(kind));
            assert_eq!(
                result.accum.error_metadata,
                Some(serde_json::json!({"boundary": "received"}))
            );
            assert_eq!(result.tool_results.len(), 1);
            assert_eq!(result.tool_results[0].request_id, "tool-1");
        }
    }

    #[tokio::test]
    async fn unterminated_eof_cancel_interrupts_running_edge_tool() {
        struct WaitingToolHost {
            token: tokio_util::sync::CancellationToken,
            settled: bool,
        }

        #[async_trait]
        impl SseEdgeWork for WaitingToolHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}

            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.token.cancelled().await;
                self.settled = true;
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "cancelled locally".to_string(),
                    tool_result_fields: None,
                    status: "failed".to_string(),
                    duration_ms: 0,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let first = format!(
            "{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event(
                "tool_request",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{}"
            )
        );
        let terminal =
            "data: {\"type\":\"run_finished\",\"run_id\":\"root-run\",\"status\":\"cancelled\"}";
        let source = stream::iter(vec![Ok(first.into_bytes())]).chain(stream::once(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(terminal.as_bytes().to_vec())
        }));
        let mut source = Box::pin(source);
        let token = tokio_util::sync::CancellationToken::new();
        let mut host = TestEdgeOnly::new(WaitingToolHost {
            token: token.clone(),
            settled: false,
        });

        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            consume_sse_stream_cancellable(
                &mut source,
                &mut host,
                stream_idle_timeout(),
                Some(&token),
                None,
            ),
        )
        .await
        .expect("EOF-tail cancellation must interrupt an already-running Edge tool");

        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert!(host.settled);
        assert_eq!(
            result.accum.run_terminal.map(|terminal| terminal.status),
            Some(DurableRunTerminalStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn durable_cancel_inside_coalesce_window_prevents_tool_execution() {
        struct CountingToolHost(std::sync::Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait]
        impl SseEdgeWork for CountingToolHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}

            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "must not execute".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 0,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let first = format!(
            "{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event(
                "tool_request",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\",\"request_id\":\"tool-1\",\"tool\":\"read_file\",\"args\":{\"path\":\"README.md\"}"
            )
        );
        let terminal = sse_event(
            "run_finished",
            ",\"run_id\":\"root-run\",\"status\":\"cancelled\"",
        );
        let source = stream::iter(vec![Ok(first.into_bytes())]).chain(stream::once(async move {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Ok(terminal.into_bytes())
        }));
        let mut source = Box::pin(source);
        let executions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut host = TestEdgeOnly::new(CountingToolHost(executions.clone()));
        let token = tokio_util::sync::CancellationToken::new();

        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_millis(250),
            consume_sse_stream_cancellable(
                &mut source,
                &mut host,
                stream_idle_timeout(),
                Some(&token),
                None,
            ),
        )
        .await
        .expect("coalesce-window cancellation must settle promptly");

        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(result.tool_results.is_empty());
        assert_eq!(
            result.accum.run_terminal.map(|terminal| terminal.status),
            Some(DurableRunTerminalStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn unterminated_eof_cancel_prevents_pending_tool_execution() {
        struct CountingToolHost(std::sync::Arc<std::sync::atomic::AtomicUsize>);

        #[async_trait]
        impl SseEdgeWork for CountingToolHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}

            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "must not execute".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 0,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let bytes = format!(
            "{}{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event(
                "tool_request",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\",\"request_id\":\"tool-1\",\"tool\":\"read_file\",\"args\":{\"path\":\"README.md\"}"
            ),
            // Deliberately no final blank-line boundary.
            "data: {\"type\":\"run_finished\",\"run_id\":\"root-run\",\"status\":\"cancelled\"}"
        );
        let mut source = stream::iter(vec![Ok(bytes.into_bytes())]);
        let executions = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut host = TestEdgeOnly::new(CountingToolHost(executions.clone()));
        let token = tokio_util::sync::CancellationToken::new();

        let (result, abort) = consume_sse_stream_cancellable(
            &mut source,
            &mut host,
            stream_idle_timeout(),
            Some(&token),
            None,
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert_eq!(executions.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(result.tool_results.is_empty());
        assert_eq!(
            result.accum.run_terminal.map(|terminal| terminal.status),
            Some(DurableRunTerminalStatus::Cancelled)
        );
    }

    #[tokio::test]
    async fn typed_failed_terminal_preserves_budget_and_prevents_pending_tool_execution() {
        let first = format!(
            "{}{}{}{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event("text_delta", ",\"content\":\"partial answer\""),
            sse_event("usage", ",\"input_tokens\":10,\"output_tokens\":5"),
            sse_event(
                "tool_request",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\",\"request_id\":\"tool-1\",\"tool\":\"read_file\",\"args\":{\"path\":\"README.md\"}"
            ),
            sse_event(
                "error",
                ",\"message\":\"LLM time budget exhausted\",\"error_code\":\"budget_exhausted\",\"error_kind\":\"budget_exhausted\""
            )
        );
        let terminal = sse_event(
            "run_finished",
            ",\"run_id\":\"root-run\",\"status\":\"failed\",\"error\":\"LLM time budget exhausted\",\"error_kind\":\"budget_exhausted\"",
        );
        for chunks in [
            vec![Ok(format!(
                "{first}{terminal}{}",
                sse_event("usage", ",\"input_tokens\":999,\"output_tokens\":999")
            )
            .into_bytes())],
            vec![
                Ok(first.as_bytes().to_vec()),
                Ok(terminal.as_bytes().to_vec()),
            ],
        ] {
            let mut source = stream::iter(chunks);
            let mut host = NoopSseStreamHost::default();

            let (result, abort) = consume_sse_stream(
                &mut source,
                &mut host,
                std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
            )
            .await;

            assert_eq!(abort, Some(astra_core::ErrorKind::BudgetExhausted));
            assert_eq!(
                result.accum.error_kind,
                Some(astra_core::ErrorKind::BudgetExhausted)
            );
            assert_eq!(
                result.accum.error_message.as_deref(),
                Some("LLM time budget exhausted")
            );
            assert_eq!(result.accum.session_id.as_deref(), Some("session-1"));
            assert_eq!(result.accum.run_id.as_deref(), Some("root-run"));
            assert_eq!(result.accum.prompt_tokens, 10);
            assert_eq!(result.accum.completion_tokens, 5);
            assert_eq!(result.accum.error_code.as_deref(), Some("budget_exhausted"));
            assert!(result.accum.full_text.is_empty());
            assert!(result.accum.tool_calls.is_empty());
            assert!(result.tool_results.is_empty());
            let terminal = result.accum.run_terminal.expect("typed root terminal");
            assert_eq!(
                terminal.error_kind,
                Some(astra_core::ErrorKind::BudgetExhausted)
            );
        }
    }

    #[tokio::test]
    async fn classified_root_failure_retains_observed_output_across_framing_boundaries() {
        for kind in [
            astra_core::ErrorKind::StreamTransport,
            astra_core::ErrorKind::StreamIdle,
        ] {
            let frames = [
                sse_event(
                    "session_info",
                    ",\"session_id\":\"session-1\",\"run_id\":\"root-run\"",
                ),
                sse_event(
                    "text_done",
                    ",\"full_text\":\"  actual partial bytes\\n\",\"model_item_id\":\"answer-1\",\"partial\":true",
                ),
                sse_event(
                    "run_error",
                    ",\"run_id\":\"root-run\",\"error\":\"provider stream stopped\",\"error_code\":\"stream_transport\"",
                ),
                // Even a producer that queues work after final failure cannot execute it.
                sse_event(
                    "tool_request",
                    ",\"request_id\":\"late-tool\",\"tool\":\"read_file\",\"args\":{}",
                ),
                sse_event(
                    "approval_required",
                    ",\"request_id\":\"late-approval\",\"tool\":\"bash\"",
                ),
                sse_event(
                    "usage",
                    ",\"input_tokens\":17,\"output_tokens\":3,\"usage_scope\":\"run_total\",\"qualified_usage\":{\"input_tokens\":17,\"output_tokens\":3}",
                ),
                sse_event(
                    "run_finished",
                    ",\"run_id\":\"root-run\",\"owner_generation\":4,\"status\":\"failed\",\"error\":\"provider stream stopped\",\"error_code\":\"stream_transport\"",
                ),
            ];
            let frames = frames.map(|frame| frame.replace("stream_transport", kind.as_str()));
            let all = frames.concat();
            let cases = [
                frames
                    .iter()
                    .map(|frame| Ok(frame.as_bytes().to_vec()))
                    .collect::<Vec<_>>(),
                vec![Ok(all.as_bytes().to_vec())],
                all.as_bytes()
                    .chunks(7)
                    .map(|part| Ok(part.to_vec()))
                    .collect(),
                vec![Ok(all.trim_end().as_bytes().to_vec())],
            ];
            for chunks in cases {
                let mut source = stream::iter(chunks);
                let mut host = RecordingSseStreamHost::new();
                let (result, abort) =
                    consume_sse_stream(&mut source, &mut host, std::time::Duration::from_secs(1))
                        .await;
                assert_eq!(abort, Some(kind));
                let accum = result.accum;
                assert!(accum.has_authoritative_failed_terminal());
                assert!(!accum.stream_complete);
                assert!(!accum.server_loop_terminal);
                assert!(accum.server_execution_summary.is_none());
                assert_eq!(accum.full_text, "  actual partial bytes\n");
                assert_eq!(accum.model_item_id.as_deref(), Some("answer-1"));
                assert_eq!(accum.accounted_usage().unwrap().input_tokens(), Some(17));
                assert_eq!(accum.accounted_usage().unwrap().output_tokens(), Some(3));
                assert_eq!(
                    accum.error_message.as_deref(),
                    Some("provider stream stopped")
                );
                assert!(result.tool_results.is_empty());
                assert!(host.edge.approval_kinds.is_empty());
                assert_eq!(host.observer.done_count, 0);
            }
        }
    }

    #[tokio::test]
    async fn failed_output_requires_valid_physical_root_evidence() {
        let prefix = format!(
            "{}{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event("text_delta", ",\"content\":\"must be discarded\""),
            sse_event(
                "run_error",
                ",\"run_id\":\"root-run\",\"error\":\"stream failed\",\"error_code\":\"stream_transport\""
            ),
        );
        for terminal in [
            serde_json::json!({"type":"run_finished","run_id":"foreign","owner_generation":4,"status":"failed","error_code":"stream_transport"}),
            serde_json::json!({"type":"run_finished","run_id":"root-run","status":"failed","error_code":"stream_transport"}),
            serde_json::json!({"type":"run_finished","run_id":"root-run","owner_generation":4,"status":"completed"}),
            serde_json::json!({"type":"run_finished","run_id":"root-run","owner_generation":4,"status":"failed","error_kind":"not_a_kind","error_code":"stream_transport"}),
            serde_json::json!({"type":"run_finished","run_id":"root-run","owner_generation":4,"status":"failed","error_code":"stream_transport","turn_evaluation":false}),
            serde_json::json!({"type":"run_finished","run_id":"root-run","owner_generation":4,"status":"failed","error_code":"stream_idle"}),
        ] {
            let mut source = stream::iter(vec![Ok(
                format!("{prefix}data: {terminal}\n\n").into_bytes()
            )]);
            let mut host = RecordingSseStreamHost::new();
            host.observer.require_sse_done = true;
            let (result, abort) =
                consume_sse_stream(&mut source, &mut host, std::time::Duration::from_secs(1)).await;
            assert!(abort.is_some());
            assert!(
                !result.accum.has_authoritative_failed_terminal(),
                "{terminal}"
            );
            assert!(result.accum.full_text.is_empty(), "{terminal}");
            assert!(!result.accum.stream_complete);
        }
    }

    #[tokio::test]
    async fn large_observation_before_failed_terminal_is_accepted_and_cancels_work() {
        let (tx, mut source) = test_channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut host = RecordingSseStreamHost::new();
        host.edge.tool_gate = Some((started.clone(), cancel.clone()));
        let bridge = tokio::spawn(async move {
            tx.send(Ok(format!(
                "{}{}",
                sse_event(
                    "session_info",
                    ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
                ),
                sse_event(
                    "tool_request",
                    ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{}"
                ),
            )
            .into_bytes()))
                .await
                .unwrap();
            started.notified().await;
            let output = "x".repeat(MAX_EDGE_PENDING_BYTES + 1);
            let overflow = sse_event("context_meta", &format!(",\"large\":{output:?}"));
            let failed = sse_event(
                "run_finished",
                ",\"run_id\":\"root-run\",\"owner_generation\":4,\"status\":\"failed\",\"error_code\":\"stream_transport\"",
            );
            tx.send(Ok(format!("{overflow}{failed}").into_bytes()))
                .await
                .unwrap();
        });
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream_cancellable(
                &mut source,
                &mut host,
                stream_idle_timeout(),
                Some(&cancel),
                None,
            ),
        )
        .await
        .expect("the failed terminal must settle the active callback");
        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert!(
            host.observer
                .accepted_sse_events
                .iter()
                .any(|event| { event.get("type").and_then(Value::as_str) == Some("context_meta") })
        );
        assert!(result.accum.has_authoritative_failed_terminal());
        bridge.await.unwrap();
    }

    #[tokio::test]
    async fn observer_failure_stops_before_later_unterminated_terminal() {
        let (tx, mut source) = test_channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut host = RecordingSseStreamHost::new();
        host.edge.tool_gate = Some((started.clone(), cancel.clone()));
        host.observer.rejected_sse_event_type = Some("text_delta");
        let bridge = tokio::spawn(async move {
            tx.send(Ok(format!(
                "{}{}",
                sse_event(
                    "session_info",
                    ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
                ),
                sse_event(
                    "tool_request",
                    ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{}"
                ),
            )
            .into_bytes()))
                .await
                .unwrap();
            started.notified().await;
            let prefix = sse_event(
                "text_delta",
                ",\"content\":\"observed but callback failed\"",
            );
            let terminal = sse_event(
                "run_finished",
                ",\"run_id\":\"root-run\",\"owner_generation\":4,\"status\":\"failed\",\"error_code\":\"stream_transport\"",
            );
            tx.send(Ok(format!("{prefix}{}", terminal.trim_end()).into_bytes()))
                .await
                .unwrap();
        });
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream_cancellable(
                &mut source,
                &mut host,
                stream_idle_timeout(),
                Some(&cancel),
                None,
            ),
        )
        .await
        .expect("terminal observation must settle the active callback");
        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert!(
            result
                .accum
                .error_message
                .as_deref()
                .unwrap()
                .contains("injected observation callback failure")
        );
        assert!(result.accum.run_terminal.is_none());
        assert!(!result.accum.has_authoritative_failed_terminal());
        assert!(result.accum.full_text.is_empty());
        bridge.await.unwrap();
    }

    #[tokio::test]
    async fn persistence_failure_preserves_partial_output_without_executing_queued_work() {
        let frames = [
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\"",
            ),
            sse_event("text_delta", ",\"content\":\"partial answer\""),
            sse_event(
                "tool_request",
                ",\"request_id\":\"queued\",\"tool\":\"read_file\",\"args\":{\"path\":\"fixture\"}",
            ),
            sse_event(
                "run_error",
                ",\"run_id\":\"root-run\",\"error\":\"Completion could not be saved.\",\"error_code\":\"canonical_persistence_failed\"",
            ),
            sse_event(
                "run_finished",
                ",\"run_id\":\"root-run\",\"status\":\"failed\",\"error\":\"Completion could not be saved.\"",
            ),
            "data: [DONE]\n\n".to_owned(),
        ];
        for chunks in [
            frames
                .iter()
                .map(|frame| Ok(frame.as_bytes().to_vec()))
                .collect::<Vec<_>>(),
            vec![Ok(frames.concat().into_bytes())],
        ] {
            let mut stream = stream::iter(chunks);
            let mut host = RecordingSseStreamHost::new();
            let (result, abort) =
                consume_sse_stream(&mut stream, &mut host, std::time::Duration::from_secs(1)).await;
            assert_eq!(abort, Some(astra_core::ErrorKind::ContractViolation));
            assert_eq!(result.accum.full_text, "partial answer");
            assert_eq!(
                result.accum.error_code.as_deref(),
                Some("canonical_persistence_failed")
            );
            assert!(
                result
                    .accum
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("Completion could not be saved")
            );
            assert!(result.tool_results.is_empty());
            assert!(host.observer.admission_closures >= 1);
        }
    }

    #[tokio::test]
    async fn completed_root_terminal_closes_unstarted_tool_admission() {
        let events = format!(
            "{}{}{}{}{}",
            sse_event("session_info", ",\"session_id\":\"s\",\"run_id\":\"root\""),
            sse_event(
                "tool_request",
                ",\"request_id\":\"queued\",\"tool\":\"read_file\",\"args\":{\"path\":\"before\"}"
            ),
            sse_event(
                "run_finished",
                ",\"run_id\":\"root\",\"status\":\"completed\",\"owner_generation\":1"
            ),
            sse_event(
                "tool_request",
                ",\"request_id\":\"late\",\"tool\":\"read_file\",\"args\":{\"path\":\"after\"}"
            ),
            "data: [DONE]\n\n"
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new();
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()).await;
        assert_eq!(abort, None);
        assert!(result.tool_results.is_empty());
        assert!(host.edge.admitted_deadlines.is_empty());
        assert_eq!(host.observer.admission_closures, 1);
    }

    #[tokio::test]
    async fn completed_root_terminal_discards_queued_work_while_started_tool_settles() {
        let (tx, rx) = test_channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = tokio_util::sync::CancellationToken::new();
        let mut stream = rx.inspect(|chunk| {
            if chunk
                .as_ref()
                .is_ok_and(|bytes| String::from_utf8_lossy(bytes).contains("run_finished"))
            {
                release.cancel();
            }
        });
        let mut host = RecordingSseStreamHost::new();
        host.edge.tool_gate = Some((started.clone(), release.clone()));
        let bridge = tokio::spawn(async move {
            tx.send(Ok(format!(
                "{}{}",
                sse_event("session_info", ",\"session_id\":\"s\",\"run_id\":\"root\""),
                sse_event(
                    "tool_request",
                    ",\"request_id\":\"started\",\"tool\":\"bash\",\"args\":{}"
                )
            )
            .into_bytes()))
                .await
                .unwrap();
            started.notified().await;
            tx.send(Ok(format!("{}{}{}{}",
                sse_event("tool_request", ",\"request_id\":\"queued\",\"tool\":\"read_file\",\"args\":{\"path\":\"before\"}"),
                sse_event("run_finished", ",\"run_id\":\"root\",\"status\":\"completed\",\"owner_generation\":1"),
                sse_event("tool_request", ",\"request_id\":\"late\",\"tool\":\"read_file\",\"args\":{\"path\":\"after\"}"),
                "data: [DONE]\n\n"
            ).into_bytes())).await.unwrap();
        });
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()),
        )
        .await
        .unwrap();
        bridge.await.unwrap();
        assert_eq!(abort, None, "{:?}", result.accum.error_message);
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].request_id, "started");
        assert_eq!(host.edge.admitted_deadlines.len(), 1);
        assert_eq!(host.observer.admission_closures, 1);
    }

    #[tokio::test]
    async fn descendant_run_error_does_not_close_root_admission() {
        let events = format!(
            "{}{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root\""
            ),
            sse_event(
                "run_error",
                ",\"run_id\":\"child\",\"error\":\"child failed\""
            ),
            "data: [DONE]\n\n"
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new();
        let (_, abort) = consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()).await;
        assert_eq!(abort, None);
        assert_eq!(host.observer.admission_closures, 0);
    }

    #[tokio::test]
    async fn unterminated_eof_failed_terminal_preserves_exact_error_kind() {
        let bytes = format!(
            "{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            // Deliberately no final blank-line boundary.
            "data: {\"type\":\"run_finished\",\"run_id\":\"root-run\",\"status\":\"failed\",\"error_kind\":\"budget_exhausted\"}"
        );
        let mut source = stream::iter(vec![Ok(bytes.into_bytes())]);
        let mut host = NoopSseStreamHost::default();

        let (result, abort) = consume_sse_stream(
            &mut source,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::BudgetExhausted));
        assert_eq!(
            result.accum.error_kind,
            Some(astra_core::ErrorKind::BudgetExhausted)
        );
        assert_eq!(
            result
                .accum
                .run_terminal
                .as_ref()
                .and_then(|terminal| terminal.error_kind),
            Some(astra_core::ErrorKind::BudgetExhausted)
        );
    }

    #[tokio::test]
    async fn failed_terminal_without_error_kind_is_not_guessed_as_server_overload() {
        let events = format!(
            "{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
            ),
            sse_event(
                "run_finished",
                ",\"run_id\":\"root-run\",\"status\":\"failed\",\"error\":\"legacy unclassified failure\""
            )
        );
        let mut source = stream::iter(chunks_from_sse(&events));
        let mut host = NoopSseStreamHost::default();

        let (result, abort) = consume_sse_stream(
            &mut source,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::Unknown));
        assert_eq!(
            result.accum.error_kind,
            Some(astra_core::ErrorKind::Unknown)
        );
        assert_ne!(abort, Some(astra_core::ErrorKind::ServerError));
    }

    #[tokio::test]
    async fn explicit_server_error_terminal_retains_overload_classification() {
        let events = format!(
            "{}{}",
            sse_event("session_info", ",\"run_id\":\"root-run\""),
            sse_event(
                "run_finished",
                ",\"run_id\":\"root-run\",\"status\":\"failed\",\"error_kind\":\"server_error\""
            )
        );
        let mut source = stream::iter(chunks_from_sse(&events));
        let mut host = NoopSseStreamHost::default();

        let (result, abort) = consume_sse_stream(
            &mut source,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::ServerError));
        assert_eq!(
            result.accum.error_kind,
            Some(astra_core::ErrorKind::ServerError)
        );
    }

    #[tokio::test]
    async fn noop_host_captures_usage() {
        let events = sse_event(
            "usage",
            ",\"input_tokens\":100,\"output_tokens\":50,\"total_tokens\":150",
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost::default();
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
    async fn strict_host_observes_accepted_events_and_done_in_protocol_order() {
        let events = format!(
            "{}{}data: [DONE]\n\n",
            sse_event(
                "session_info",
                ",\"session_id\":\"session-1\",\"run_id\":\"run-1\""
            ),
            sse_event("usage", ",\"input_tokens\":7,\"output_tokens\":2"),
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new().strict();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert!(result.accum.stream_complete);
        assert_eq!(host.observer.accepted_sse_events.len(), 2);
        assert_eq!(host.observer.accepted_sse_events[0]["type"], "session_info");
        assert_eq!(host.observer.accepted_sse_events[1]["type"], "usage");
        assert_eq!(host.observer.done_count, 1);
    }

    #[tokio::test]
    async fn hosts_reject_malformed_json_without_done_observation() {
        let events = "data: {not-json}\n\ndata: [DONE]\n\n";
        for strict in [false, true] {
            let mut stream = stream::iter(chunks_from_sse(events));
            let mut host = RecordingSseStreamHost::new();
            host.observer.require_sse_done = strict;

            let (result, abort) = consume_sse_stream(
                &mut stream,
                &mut host,
                std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
            )
            .await;

            assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
            assert!(!result.accum.stream_complete);
            assert!(result.accum.error_message.is_some());
            assert!(host.observer.accepted_sse_events.is_empty());
            assert_eq!(host.observer.done_count, 0);
        }
    }

    #[tokio::test]
    async fn strict_host_rejects_eof_without_done_observation() {
        let events = sse_event("text_delta", ",\"content\":\"partial\"");
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new().strict();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert!(!result.accum.stream_complete);
        assert!(result.accum.full_text.is_empty());
        assert_eq!(host.observer.accepted_sse_events.len(), 1);
        assert_eq!(host.observer.done_count, 0);
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

        assert!(host.observer.stream_completed);
        assert_eq!(result.accum.full_text, "answer done");
        assert_eq!(result.accum.reasoning_content, "think");

        // Should have received StreamText effects
        let text_effects: Vec<&str> = host
            .observer
            .render_effects
            .iter()
            .filter_map(|e| match e {
                SseRenderEffect::StreamText { text: s, .. } => Some(s.as_str()),
                _ => None,
            })
            .collect();
        assert!(text_effects.contains(&"answer"));
        assert!(text_effects.contains(&" done"));
    }

    #[tokio::test]
    async fn done_marker_prevents_later_chunks_from_executing_edge_tools() {
        let chunks: Vec<Result<Vec<u8>, String>> = vec![
            Ok(b"data: [DONE]\n\n".to_vec()),
            Ok(b"data: {\"type\":\"tool_request\",\"request_id\":\"after-done\",\"tool\":\"bash\",\"args\":{}}\n\n".to_vec()),
        ];
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert!(result.accum.stream_complete);
        assert!(result.tool_results.is_empty());
        assert!(result.approval_results.is_empty());
    }

    #[tokio::test]
    async fn done_marker_terminates_consumption_without_waiting_for_source_eof() {
        let source = stream::iter(vec![Ok(b"data: [DONE]\n\n".to_vec())])
            .chain(stream::pending::<Result<Vec<u8>, String>>());
        let mut stream = source;
        let mut host = RecordingSseStreamHost::new();

        let completed = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            consume_sse_stream(
                &mut stream,
                &mut host,
                std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
            ),
        )
        .await
        .expect("[DONE] is a terminal transport signal, not a request to await EOF");

        assert!(completed.1.is_none());
        assert!(completed.0.accum.stream_complete);
        assert!(host.observer.stream_completed);
    }

    #[tokio::test]
    async fn done_marker_flushes_preceding_edge_work_before_returning() {
        let events = format!(
            "{}data: [DONE]\n\n",
            sse_event(
                "tool_request",
                ",\"request_id\":\"before-done\",\"tool\":\"bash\",\"args\":{\"command\":\"git status --short\"}",
            )
        );
        let source = stream::iter(vec![Ok(events.into_bytes())])
            .chain(stream::pending::<Result<Vec<u8>, String>>());
        let mut stream = source;
        let mut host = RecordingSseStreamHost::new().with_tool_output("bash", "clean");

        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            consume_sse_stream(
                &mut stream,
                &mut host,
                std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
            ),
        )
        .await
        .expect("[DONE] must flush already-accepted edge work without waiting for EOF");

        assert!(abort.is_none());
        assert!(result.accum.stream_complete);
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].request_id, "before-done");
    }

    #[tokio::test]
    async fn done_marker_does_not_forward_later_agent_live_evidence() {
        let chunks: Vec<Result<Vec<u8>, String>> = vec![
            Ok(b"data: [DONE]\n\n".to_vec()),
            Ok(b"data: {\"type\":\"agent_live_gap\",\"run_id\":\"late-run\",\"agent_id\":\"late-agent\",\"dropped_event_count\":1}\n\n".to_vec()),
        ];
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert!(result.accum.stream_complete);
        assert!(host.observer.agent_live_gaps.is_empty());
    }

    #[tokio::test]
    async fn recording_host_receives_typed_agent_communication() {
        let events = sse_event(
            "agent_communication",
            ",\"schema_version\":\"astra.agent_communication.v1\",\"observed_by\":{\"run_id\":\"run-review\",\"agent_id\":\"reviewer\"},\"direction\":\"received\",\"message_id\":\"msg-1\",\"from\":{\"run_id\":\"run-code\",\"agent_id\":\"coder\"},\"to\":{\"kind\":\"direct\",\"address\":{\"run_id\":\"run-review\",\"agent_id\":\"reviewer\"}},\"payload_kind\":\"text\",\"summary\":\"review this\",\"timestamp_ms\":42",
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new();

        let (_result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(host.observer.agent_communications.len(), 1);
        assert_eq!(
            host.observer.agent_communications[0].observed_by.run_id,
            "run-review"
        );
        assert_eq!(host.observer.agent_communications[0].from.agent_id, "coder");
    }

    #[tokio::test]
    async fn recording_host_receives_typed_agent_live_gap() {
        let events = sse_event(
            "agent_live_gap",
            ",\"run_id\":\"run-reviewer-1\",\"agent_id\":\"reviewer\",\"dropped_event_count\":3,\"repair\":\"refresh_run_snapshot\"",
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new();

        let (_result, abort) = consume_sse_stream_cancellable(
            &mut stream,
            &mut host,
            stream_idle_timeout(),
            None,
            Some(stream_idle_timeout_after_progress()),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(
            host.observer.agent_live_gaps,
            vec![AgentLiveGap {
                run_id: "run-reviewer-1".into(),
                agent_id: "reviewer".into(),
                dropped_event_count: 3,
            }]
        );
    }

    #[tokio::test]
    async fn recording_host_receives_typed_agent_live_events() {
        let events = format!(
            "{}{}{}",
            sse_event(
                "agent_live_event",
                ",\"run_id\":\"run-reviewer-1\",\"agent_id\":\"reviewer\",\"event_kind\":\"thinking_delta\",\"content\":\"inspect ownership\"",
            ),
            sse_event(
                "agent_live_event",
                ",\"run_id\":\"run-reviewer-1\",\"agent_id\":\"reviewer\",\"event_kind\":\"tool_completed\",\"name\":\"bash\",\"description\":\"cargo test\",\"status\":\"success\",\"duration_ms\":12,\"output_summary\":\"ok\",\"output\":\"all passed\",\"tool_use_id\":\"call-1\"",
            ),
            sse_event(
                "agent_live_event",
                ",\"run_id\":\"run-reviewer-1\",\"agent_id\":\"reviewer\",\"event_kind\":\"signal\",\"signal\":{\"signal\":\"approval_required\",\"request_id\":\"approval-1\",\"tool\":\"bash\",\"approval_kind\":\"explicit\",\"path\":null,\"detail\":\"git status\",\"display_label\":\"$ git status\"}",
            ),
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new();

        let (_result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert!(matches!(
            host.observer.agent_live_events.as_slice(),
            [
                AgentLiveEvent {
                    run_id,
                    agent_id,
                    kind: AgentLiveEventKind::ThinkingDelta { text, .. },
                },
                AgentLiveEvent {
                    run_id: tool_run_id,
                    kind: AgentLiveEventKind::ToolCompleted {
                        tool_use_id,
                        output: Some(output),
                        ..
                    },
                    ..
                },
                AgentLiveEvent {
                    run_id: approval_run_id,
                    kind: AgentLiveEventKind::Signal(AgentLiveSignal::ApprovalRequired {
                        request_id,
                        ..
                    }),
                    ..
                },
            ] if run_id == "run-reviewer-1"
                && agent_id == "reviewer"
                && text == "inspect ownership"
                && tool_run_id == "run-reviewer-1"
                && tool_use_id == "call-1"
                && output == "all passed"
                && approval_run_id == "run-reviewer-1"
                && request_id == "approval-1"
        ));
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
        assert_eq!(result.tool_results[0].status, "completed");
    }

    #[tokio::test]
    async fn recording_host_approval_resolved() {
        let events = format!(
            "{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"sess-approval\",\"run_id\":\"run-approval\""
            ),
            sse_event(
                "approval_required",
                ",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"approval_kind\":\"standard\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"",
            )
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
        assert_eq!(host.edge.approval_kinds, vec![ApprovalKind::Standard]);
        assert_eq!(
            host.edge.approval_run_ids,
            vec![Some("run-approval".to_string())]
        );
    }

    #[tokio::test]
    async fn approval_uses_event_owner_instead_of_consuming_stream_fallback() {
        let events = format!(
            "{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"shared-session\",\"run_id\":\"consumer-run\""
            ),
            sse_event(
                "approval_required",
                ",\"session_id\":\"shared-session\",\"run_id\":\"approval-owner-run\",\"request_id\":\"ap-cross-run\",\"tool\":\"bash\",\"approval_kind\":\"explicit\",\"detail\":\"echo safe\"",
            )
        );
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = RecordingSseStreamHost::new();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(
            host.edge.approval_session_ids,
            vec![Some("shared-session".to_string())]
        );
        assert_eq!(
            host.edge.approval_run_ids,
            vec![Some("approval-owner-run".to_string())],
            "concurrent child approval identity must not be replaced by the consuming stream"
        );
    }

    #[tokio::test]
    async fn approval_callback_failure_stops_coalesced_tool_before_execution() {
        let events = format!(
            "{}{}",
            sse_event(
                "approval_required",
                ",\"session_id\":\"session-safe\",\"run_id\":\"run-safe\",\"request_id\":\"same-id\",\"tool\":\"bash\",\"approval_kind\":\"explicit\"",
            ),
            sse_event(
                "tool_request",
                ",\"session_id\":\"session-safe\",\"run_id\":\"run-safe\",\"request_id\":\"same-id\",\"tool\":\"bash\",\"args\":{\"command\":\"touch must-not-run\"}",
            )
        );
        let mut stream = stream::iter(vec![Ok(events.into_bytes())]);
        let mut host = RecordingSseStreamHost::new().abort_edge_work_after_approval();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].status, "failed");
        assert_eq!(
            result.tool_results[0]
                .tool_result_fields
                .as_ref()
                .and_then(|fields| fields.get("execution_started")),
            Some(&serde_json::Value::Bool(false)),
            "the suppressed tool retains an explicit not-started fact"
        );
    }

    #[tokio::test]
    async fn approval_callback_failure_stops_remaining_owner_groups() {
        let events = format!(
            "{}{}",
            sse_event(
                "approval_required",
                ",\"session_id\":\"session-a\",\"run_id\":\"run-a\",\"request_id\":\"approval-a\",\"tool\":\"bash\",\"approval_kind\":\"explicit\"",
            ),
            sse_event(
                "approval_required",
                ",\"session_id\":\"session-b\",\"run_id\":\"run-b\",\"request_id\":\"approval-b\",\"tool\":\"bash\",\"approval_kind\":\"explicit\"",
            )
        );
        let mut stream = stream::iter(vec![Ok(events.into_bytes())]);
        let mut host = RecordingSseStreamHost::new().abort_edge_work_after_approval();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(
            result.approval_results.len(),
            2,
            "both the submitted and interrupted requests remain observable"
        );
        assert!(
            result.approval_results[1]
                .reason
                .as_deref()
                .unwrap()
                .contains("interrupted")
        );
        assert_eq!(host.edge.approval_run_ids, vec![Some("run-a".to_string())]);
    }

    #[test]
    fn approval_grouping_is_session_isolated_ordered_and_linear_indexed() {
        const SESSION_COUNT: usize = 128;
        const REQUEST_COUNT: usize = 4_096;
        let requests = (0..REQUEST_COUNT)
            .map(|index| {
                let session = index % SESSION_COUNT;
                EdgeApprovalRequest {
                    session_id: Some(format!("session-{session}")),
                    run_id: Some(format!("run-{session}")),
                    request_id: format!("approval-{index}"),
                    tool: "bash".to_string(),
                    approval_kind: ApprovalKind::Explicit,
                    detail: Some("echo safe".to_string()),
                    display_label: None,
                }
            })
            .collect();

        let groups =
            group_approval_requests(requests, Some("fallback-session"), Some("fallback-run"));

        assert_eq!(groups.len(), SESSION_COUNT);
        for (expected, (session_id, run_id, approvals)) in groups.iter().enumerate() {
            assert_eq!(
                session_id.as_deref(),
                Some(format!("session-{expected}").as_str())
            );
            assert_eq!(run_id.as_deref(), Some(format!("run-{expected}").as_str()));
            assert_eq!(approvals.len(), REQUEST_COUNT / SESSION_COUNT);
            assert!(approvals.iter().all(|approval| {
                approval.session_id.as_deref() == session_id.as_deref()
                    && approval.run_id.as_deref() == run_id.as_deref()
            }));
        }
    }

    #[test]
    fn approval_grouping_uses_fallback_only_for_missing_identity() {
        let request = |request_id: &str, session_id: Option<&str>, run_id: Option<&str>| {
            EdgeApprovalRequest {
                session_id: session_id.map(ToString::to_string),
                run_id: run_id.map(ToString::to_string),
                request_id: request_id.to_string(),
                tool: "bash".to_string(),
                approval_kind: ApprovalKind::Explicit,
                detail: None,
                display_label: None,
            }
        };
        let groups = group_approval_requests(
            vec![
                request("legacy", None, None),
                request("owned", Some("producer-session"), Some("producer-run")),
            ],
            Some("consumer-session"),
            Some("consumer-run"),
        );

        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0.as_deref(), Some("consumer-session"));
        assert_eq!(groups[0].1.as_deref(), Some("consumer-run"));
        assert_eq!(groups[1].0.as_deref(), Some("producer-session"));
        assert_eq!(groups[1].1.as_deref(), Some("producer-run"));
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
        let mut host = NoopSseStreamHost::default();
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
        #[derive(Clone)]
        struct SessionAwareHost(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl SseStreamObserver for SessionAwareHost {
            type Control = ();
            async fn on_render_effects(&mut self, _: Vec<SseRenderEffect>) {}
            fn on_stream_complete(&mut self) {}
            fn on_session_id(&mut self, session_id: &str) -> Option<()> {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("session:{session_id}"));
                None
            }
        }

        #[async_trait]
        impl SseEdgeWork for SessionAwareHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}

            fn on_server_tool_surface_admission(
                &mut self,
                request: &ToolBatchRequest,
            ) -> Result<(), String> {
                let tool = request.tool.as_str();
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("admission:{tool}"));
                if tool == "blocked" {
                    Err("tool not admitted".into())
                } else {
                    Ok(())
                }
            }

            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("tool:{request_id}"));
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "ok".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 1,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let mut host = TestDuplex::new(SessionAwareHost(order.clone()));
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.accum.session_id.as_deref(), Some("sess-hook"));
        assert_eq!(
            astra_core::sync_poison::recover_mutex_lock(&order).as_slice(),
            &[
                "session:sess-hook".to_string(),
                "admission:bash".to_string(),
                "tool:tr-1".to_string(),
            ]
        );
        let denied = sse_event(
            "tool_request",
            ",\"request_id\":\"tr-2\",\"tool\":\"blocked\",\"args\":{}",
        );
        let mut denied_stream = stream::iter(chunks_from_sse(&denied));
        let (result, abort) = consume_sse_stream(
            &mut denied_stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert_eq!(result.tool_results.len(), 1);
        assert!(
            !crate::headless_tool_journal::journal_record_edge_tool_result(&result.tool_results[0])
                .was_executed()
        );
        assert_eq!(order.lock().unwrap().last().unwrap(), "admission:blocked");
    }

    #[tokio::test]
    async fn accum_and_tool_result_hooks_receive_live_updates() {
        let events = format!(
            "{}{}{}{}",
            sse_event(
                "session_info",
                ",\"session_id\":\"sess-live\",\"run_id\":\"run-live\""
            ),
            sse_event("usage", ",\"input_tokens\":21,\"output_tokens\":13"),
            sse_event("text_delta", ",\"content\":\"partial answer\""),
            sse_event(
                "tool_request",
                ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo hi\"}",
            ),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);

        #[derive(Default)]
        struct LiveHookHost {
            observer: LiveHookObserver,
            edge: LiveHookEdge,
        }
        #[derive(Default)]
        struct LiveHookObserver {
            snapshots: Vec<(Option<String>, Option<String>, String, u64, u64)>,
        }
        #[derive(Default)]
        struct LiveHookEdge {
            tool_results: Vec<(String, String, String)>,
        }

        #[async_trait]
        impl SseStreamObserver for LiveHookObserver {
            type Control = ();
            async fn on_render_effects(&mut self, _: Vec<SseRenderEffect>) {}
            fn on_stream_complete(&mut self) {}
            fn on_accum_update(&mut self, accum: &ChatTurnSseAccum) {
                self.snapshots.push((
                    accum.session_id.clone(),
                    accum.run_id.clone(),
                    accum.full_text.clone(),
                    accum.prompt_tokens,
                    accum.completion_tokens,
                ));
            }
        }
        impl SseStreamHost for LiveHookHost {
            type Observer = LiveHookObserver;
            type Edge = LiveHookEdge;
            fn split(&mut self) -> (&mut Self::Observer, &mut Self::Edge) {
                (&mut self.observer, &mut self.edge)
            }
        }

        #[async_trait]
        impl SseEdgeWork for LiveHookEdge {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}

            fn on_tool_result(&mut self, result: &EdgeToolExecResult) {
                self.tool_results.push((
                    result.request_id.clone(),
                    result.tool.clone(),
                    result.output.clone(),
                ));
            }

            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "hi".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 1,
                }
            }

            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        let mut host = LiveHookHost::default();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.accum.session_id.as_deref(), Some("sess-live"));
        assert_eq!(result.accum.run_id.as_deref(), Some("run-live"));
        assert_eq!(result.accum.full_text, "partial answer");
        assert_eq!(result.accum.prompt_tokens, 21);
        assert_eq!(result.accum.completion_tokens, 13);
        assert!(
            host.observer.snapshots.iter().any(|snapshot| {
                snapshot.0.as_deref() == Some("sess-live")
                    && snapshot.1.as_deref() == Some("run-live")
                    && snapshot.2 == "partial answer"
                    && snapshot.3 == 21
                    && snapshot.4 == 13
            }),
            "on_accum_update should see the live session/run/text/usage state"
        );
        assert_eq!(
            host.edge.tool_results,
            vec![("tool-1".to_string(), "bash".to_string(), "hi".to_string())]
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
        let mut host = NoopSseStreamHost::default();
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
        let mut host = NoopSseStreamHost::default();
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
        let mut host = NoopSseStreamHost::default();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        assert_eq!(result.tool_results.len(), 1);
        assert_eq!(result.tool_results[0].status, "failed");
        assert!(result.tool_results[0].output.contains("not supported"));

        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(result.approval_results[0].decision, "deny");
    }

    #[tokio::test]
    async fn error_event_captured() {
        let events = sse_event("error", ",\"message\":\"rate limited\"");
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost::default();
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
        assert!(host.observer.stream_completed);
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
        assert!(host.observer.stream_completed);
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

    #[tokio::test]
    async fn cancelled_admission_per_event_stop_requires_host_opt_in() {
        for opted_in in [false, true] {
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut chunks = stream::iter(vec![Ok(concat!(
                "data: {\"type\":\"session_info\",\"session_id\":\"session\",\"run_id\":\"run\"}\n\n",
                "data: {\"type\":\"text_delta\",\"content\":\"same chunk\"}\n\n",
                "data: [DONE]\n\n"
            ).as_bytes().to_vec())]);
            let mut host = RecordingSseStreamHost::new();
            host.observer.cancel_after_accepted_event = Some(cancel.clone());
            if opted_in {
                host.observer.cancelled_admission_grace = std::time::Duration::from_secs(1);
            }
            let (result, _) = consume_sse_stream_cancellable(
                &mut chunks,
                &mut host,
                std::time::Duration::from_secs(60),
                Some(&cancel),
                None,
            )
            .await;
            assert_eq!(
                result.accum.full_text,
                if opted_in { "" } else { "same chunk" }
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_admission_captures_delayed_run_without_executing_same_chunk_tools() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let mut chunks = Box::pin(stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            Ok(concat!(
                "data: {\"type\":\"session_info\",\"session_id\":\"session-late\",\"run_id\":\"run-late\"}\n\n",
                "data: {\"type\":\"approval_required\",\"request_id\":\"approval-late\",\"tool\":\"bash\"}\n\n",
                "data: {\"type\":\"tool_request\",\"request_id\":\"call-late\",\"tool\":\"bash\",\"args\":{\"command\":\"must-not-run\"},\"schema_admitted_by_server\":true}\n\n"
            ).as_bytes().to_vec())
        }));
        let mut host = RecordingSseStreamHost::new();
        host.observer.cancelled_admission_grace = std::time::Duration::from_millis(100);

        let (result, abort) = consume_sse_stream_cancellable(
            &mut chunks,
            &mut host,
            std::time::Duration::from_secs(60),
            Some(&cancel),
            None,
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert_eq!(result.accum.session_id.as_deref(), Some("session-late"));
        assert_eq!(result.accum.run_id.as_deref(), Some("run-late"));
        assert!(result.tool_results.is_empty());
        assert!(result.approval_results.is_empty());
        assert!(host.edge.approval_kinds.is_empty());
        assert_eq!(
            host.observer.accepted_sse_events.len(),
            1,
            "stop at the identity boundary"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cancelled_admission_grace_is_absolute_even_with_keepalives() {
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let mut chunks = Box::pin(stream::unfold((), |()| async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            Some((Ok(b": keepalive\n\n".to_vec()), ()))
        }));
        let mut host = RecordingSseStreamHost::new();
        host.observer.cancelled_admission_grace = std::time::Duration::from_millis(40);
        let started = tokio::time::Instant::now();
        let (result, abort) = consume_sse_stream_cancellable(
            &mut chunks,
            &mut host,
            std::time::Duration::from_secs(60),
            Some(&cancel),
            None,
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
        assert!(result.accum.run_id.is_none());
        assert_eq!(started.elapsed(), std::time::Duration::from_millis(40));
        assert!(result.tool_results.is_empty());
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
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert!(
            result.accum.full_text.is_empty(),
            "transport abort tombstones partial text so pending frames cannot be mistaken for a complete turn"
        );
        assert!(result.accum.error_message.is_some());
    }

    #[tokio::test]
    async fn invalid_utf8_aborts_before_tool_execution() {
        let chunks: Vec<Result<Vec<u8>, String>> = vec![
            Ok(sse_event("text_delta", ",\"content\":\"partial\"").into_bytes()),
            Ok(b"data: {\"type\":\"tool_request\",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{\"x\":\"\xff\"}}\n\n".to_vec()),
        ];
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
        assert!(
            result.accum.full_text.is_empty(),
            "partial text must be tombstoned"
        );
        assert!(
            result.tool_results.is_empty(),
            "invalid bytes must not reach tools"
        );
        assert!(
            result
                .accum
                .error_message
                .as_deref()
                .unwrap_or("")
                .contains("invalid UTF-8")
        );
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
        let mut host = NoopSseStreamHost::default();
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
        let mut host = NoopSseStreamHost::default();
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
                ",\"session_id\":\"s1\",\"run_id\":\"r1\",\"turn_chain_id\":\"c1\",\"request_id\":\"t1\",\"tool\":\"read_file\",\"args\":{\"path\":\"x\"}"
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

    fn make_tool_pending(tool: &str) -> SsePending<()> {
        SsePending::Work(ChatTurnEdgePending::ToolRequest {
            work_attribution: None,
            admitted_skill_names: None,
            session_id: "s1".to_string(),
            run_id: "r1".to_string(),
            turn_chain_id: "c1".to_string(),
            request_id: format!("req-{tool}"),
            schema_admitted_by_server: true,
            read_only_execution: false,
            execution_deadline_unix_ms: 4_102_444_800_000,
            execution_timeout_ms: 300_000,
            execution_deadline_at: tokio::time::Instant::now()
                + std::time::Duration::from_secs(300),
            command_timeout_cap_ms: Some(300_000),
            tool: tool.to_string(),
            args: serde_json::json!({}),
        })
    }

    fn tool_name(item: &SsePending<()>) -> &str {
        match item {
            SsePending::Work(ChatTurnEdgePending::ToolRequest { tool, .. }) => tool,
            _ => "approval",
        }
    }

    /// REGRESSION (session 8ca96f0f): when the model emits N
    /// agent spawn actions in one round, all N must run in parallel.
    /// Pre-fix the classifier omitted `agent`, so the dispatcher
    /// took the sequential path and processed them one by one — the
    /// strip showed "1 parallel agents" because only one ToolStarted
    /// reached the chat_widget at a time.
    ///
    /// The agent spawn action is safe to parallelize: each spawn creates an
    /// isolated sub-process with its own working dir / mailbox /
    /// permission ctx. The only "side effect" is mailbox registration
    /// keyed by unique run_id — no shared mutable state between
    /// concurrent spawns.
    ///
    /// `agent.send_message` is the explicit exception: it mutates a
    /// recipient mailbox; concurrent sends to the same target risk
    /// reordering. Keep that one sequential.
    #[test]
    fn agent_spawn_is_concurrency_safe() {
        use serde_json::json;
        let args = json!({
            "action": "spawn",
            "agent_type": "code-review",
            "name": "reviewer",
            "description": "review",
            "prompt": "x"
        });
        assert!(
            super::is_tool_concurrency_safe("agent", Some(&args)),
            "agent spawn actions must be concurrency-safe so N parallel spawns \
             actually run in parallel — pre-fix sequential dispatch \
             was the smoking gun in session 8ca96f0f"
        );
    }

    #[test]
    fn agent_get_result_is_concurrency_safe() {
        use serde_json::json;
        let args = json!({"action": "get_result", "agent_id": "a@1"});
        assert!(
            super::is_tool_concurrency_safe("agent", Some(&args)),
            "get_result is a pure read of the agent registry — safe \
             to parallelize across N agent_ids"
        );
    }

    #[test]
    fn agent_send_message_stays_sequential() {
        use serde_json::json;
        let args = json!({
            "action": "send_message",
            "to": "agent-X",
            "message": {"content": "hi"}
        });
        assert!(
            !super::is_tool_concurrency_safe("agent", Some(&args)),
            "send_message mutates the recipient mailbox — concurrent \
             sends to the same target could reorder; keep sequential"
        );
    }

    #[test]
    fn agent_without_args_stays_sequential_defensively() {
        // No args = unknown action; default to the safe-but-slow
        // sequential path rather than parallelizing something that
        // might mutate state.
        assert!(
            !super::is_tool_concurrency_safe("agent", None),
            "missing args: default to sequential (defensive)"
        );
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

    #[tokio::test]
    async fn pending_work_is_bounded_while_prior_tool_settles() {
        for unterminated in [false, true] {
            let (tx, mut stream) = test_channel();
            let started = std::sync::Arc::new(tokio::sync::Notify::new());
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut host = RecordingSseStreamHost::new();
            host.edge.tool_gate = Some((started.clone(), cancel.clone()));
            let bridge = tokio::spawn(async move {
                tx.send(Ok(sse_event(
                    "tool_request",
                    ",\"request_id\":\"first\",\"tool\":\"bash\",\"args\":{}",
                )
                .into_bytes()))
                    .await
                    .unwrap();
                started.notified().await;
                let long_path = "x".repeat(MAX_EDGE_PENDING_BYTES + 1);
                let frame = sse_event(
                    "tool_request",
                    &format!(
                        ",\"request_id\":\"later\",\"tool\":\"read_file\",\"args\":{{\"path\":{long_path:?}}}"
                    ),
                );
                let frame = if unterminated {
                    frame.trim_end().to_string()
                } else {
                    frame
                };
                tx.send(Ok(frame.into_bytes())).await.unwrap();
            });
            let (result, abort) = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                consume_sse_stream_cancellable(
                    &mut stream,
                    &mut host,
                    stream_idle_timeout(),
                    Some(&cancel),
                    None,
                ),
            )
            .await
            .expect("bounded pending work must settle the active tool");
            bridge.await.unwrap();
            assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
            assert!(
                result
                    .accum
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains("pending Edge work exceeded")
            );
            assert_eq!(result.tool_results.len(), 1);
            assert_eq!(result.tool_results[0].request_id, "first");
            assert_eq!(host.edge.admitted_deadlines.len(), 1);
        }
    }

    #[tokio::test]
    async fn controls_and_done_follow_already_accepted_work_in_order() {
        use std::sync::{Arc, Mutex};
        struct Observer {
            order: Arc<Mutex<Vec<String>>>,
        }
        struct Edge {
            order: Arc<Mutex<Vec<String>>>,
            started: Arc<tokio::sync::Notify>,
            release: tokio_util::sync::CancellationToken,
        }
        struct Host {
            observer: Observer,
            edge: Edge,
        }
        impl SseStreamHost for Host {
            type Observer = Observer;
            type Edge = Edge;
            fn split(&mut self) -> (&mut Observer, &mut Edge) {
                (&mut self.observer, &mut self.edge)
            }
        }
        #[async_trait]
        impl SseStreamObserver for Observer {
            type Control = String;
            async fn on_render_effects(&mut self, _: Vec<SseRenderEffect>) {}
            fn on_stream_complete(&mut self) {}
            async fn on_accepted_sse_event(
                &mut self,
                event: &Value,
            ) -> Result<Option<String>, String> {
                Ok(
                    (event.get("type").and_then(Value::as_str) == Some("test_control"))
                        .then(|| "policy".to_string()),
                )
            }
            async fn on_sse_done(&mut self, _: &ChatTurnSseAccum) -> Result<(), String> {
                self.order.lock().unwrap().push("done".into());
                Ok(())
            }
        }
        #[async_trait]
        impl SseEdgeWork for Edge {
            type Control = String;
            fn apply_control(&mut self, control: String) {
                self.order
                    .lock()
                    .unwrap()
                    .push(format!("control:{control}"));
            }
            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                self.order
                    .lock()
                    .unwrap()
                    .push(format!("tool:{}", request.request_id));
                if request.request_id == "first" {
                    self.started.notify_one();
                    self.release.cancelled().await;
                }
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request.request_id.clone(),
                    tool: request.tool.clone(),
                    args: request.args.clone(),
                    output: "ok".into(),
                    tool_result_fields: None,
                    status: "completed".into(),
                    duration_ms: 0,
                }
            }
            async fn resolve_approval(
                &mut self,
                request_id: &str,
                _: &str,
                _: ApprovalKind,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> EdgeApprovalResult {
                EdgeApprovalResult {
                    request_id: request_id.into(),
                    decision: "deny".into(),
                    reason: None,
                }
            }
        }
        let order = Arc::new(Mutex::new(Vec::new()));
        let started = Arc::new(tokio::sync::Notify::new());
        let release = tokio_util::sync::CancellationToken::new();
        let mut host = Host {
            observer: Observer {
                order: order.clone(),
            },
            edge: Edge {
                order: order.clone(),
                started: started.clone(),
                release: release.clone(),
            },
        };
        let (tx, rx) = test_channel();
        let mut stream = rx.inspect(|chunk| {
            if chunk
                .as_ref()
                .is_ok_and(|bytes| bytes == b"data: [DONE]\n\n")
            {
                release.cancel();
            }
        });
        let bridge = tokio::spawn(async move {
            tx.send(Ok(sse_event(
                "tool_request",
                ",\"request_id\":\"first\",\"tool\":\"bash\",\"args\":{}",
            )
            .into_bytes()))
                .await
                .unwrap();
            started.notified().await;
            tx.send(Ok(format!(
                "{}{}",
                sse_event("test_control", ""),
                sse_event(
                    "tool_request",
                    ",\"request_id\":\"second\",\"tool\":\"bash\",\"args\":{}"
                )
            )
            .into_bytes()))
                .await
                .unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()),
        )
        .await
        .unwrap();
        bridge.await.unwrap();
        assert_eq!(abort, None, "{:?}", result.accum.error_message);
        assert_eq!(
            *order.lock().unwrap(),
            vec!["tool:first", "control:policy", "tool:second", "done"]
        );
        assert_eq!(
            result
                .tool_results
                .iter()
                .map(|r| r.request_id.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
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

    #[tokio::test]
    async fn edge_execution_observes_large_live_burst_without_losing_events() {
        let (tx, rx) = test_channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = tokio_util::sync::CancellationToken::new();
        let mut stream = rx.inspect(|chunk| {
            if chunk
                .as_ref()
                .is_ok_and(|bytes| bytes == b"data: [DONE]\n\n")
            {
                release.cancel();
            }
        });
        let ordered = [
            serde_json::json!({"event_kind": "signal", "signal": {
                "signal": "run_started", "parent_run_id": "root-run", "depth": 1,
                "transcript_location": "durable_server"
            }}),
            serde_json::json!({"event_kind": "tool_started", "name": "bash",
                "description": "child tool", "tool_use_id": "child-tool"}),
            serde_json::json!({"event_kind": "tool_completed", "name": "bash",
                "description": "child tool", "tool_use_id": "child-tool",
                "status": "completed", "duration_ms": 1}),
            serde_json::json!({"event_kind": "signal", "signal": {"signal": "output_settled"}}),
            serde_json::json!({"event_kind": "agent_terminated", "termination": "completed",
                "duration_ms": 2}),
        ]
        .map(|mut event| {
            event["type"] = "agent_live_event".into();
            event["run_id"] = "child-run".into();
            event["agent_id"] = "child-agent".into();
            event
        });
        let expected: Vec<_> = ordered
            .iter()
            .map(|event| agent_live_event_from_sse(event).unwrap())
            .collect();
        let lifecycle = ordered
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<Vec<_>>();
        let bridge_started = started.clone();
        let bridge = tokio::spawn(async move {
            let first = format!(
                "{}{}",
                sse_event(
                    "session_info",
                    ",\"session_id\":\"session-1\",\"run_id\":\"root-run\""
                ),
                sse_event(
                    "tool_request",
                    ",\"request_id\":\"tool-1\",\"tool\":\"agent_fanout\",\"args\":{}"
                )
            );
            tx.send(Ok(first.into_bytes())).await.unwrap();
            bridge_started.notified().await;

            // More than 1 MiB of live events must remain observable while
            // an already-admitted tool is still running.
            let content = "x".repeat(4096);
            let mut live_burst = lifecycle[0].clone();
            for index in 0..300 {
                if index == 100 || index == 200 {
                    live_burst.push_str(&lifecycle[index / 100]);
                }
                live_burst.push_str(&sse_event(
                    "agent_live_event",
                    &format!(
                        ",\"run_id\":\"child-run\",\"agent_id\":\"child-agent\",\
                         \"event_kind\":\"output_delta\",\"content\":{content:?}"
                    ),
                ));
            }
            live_burst.extend(lifecycle[3..].iter().map(String::as_str));
            tx.send(Ok(live_burst.into_bytes())).await.unwrap();
            tx.send(Ok(
                sse_event("text_delta", ",\"content\":\"done\"").into_bytes()
            ))
            .await
            .unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });

        let mut host = RecordingSseStreamHost::new();
        host.edge.tool_gate = Some((started, release.clone()));
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()),
        )
        .await
        .expect("bounded live read-ahead must settle");

        assert_eq!(abort, None, "live observation pressure must not abort SSE");
        assert_eq!(result.accum.full_text, "done");
        assert_eq!(result.tool_results.len(), 1);
        let lifecycle: Vec<_> = host
            .observer
            .agent_live_events
            .iter()
            .filter(|event| !matches!(event.kind, AgentLiveEventKind::OutputDelta { .. }))
            .cloned()
            .collect();
        assert_eq!(
            serde_json::to_value(lifecycle).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
        assert_eq!(host.observer.agent_live_events.len(), 305);
        assert!(host.observer.agent_live_gaps.is_empty());
        bridge.await.unwrap();
    }

    #[tokio::test]
    async fn ordered_live_events_over_one_mib_do_not_abort_running_tool() {
        let (tx, rx) = test_channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let cancel = tokio_util::sync::CancellationToken::new();
        let release = tokio_util::sync::CancellationToken::new();
        let mut stream = rx.inspect(|chunk| {
            if chunk
                .as_ref()
                .is_ok_and(|bytes| bytes == b"data: [DONE]\n\n")
            {
                release.cancel();
            }
        });
        let mut host = RecordingSseStreamHost::new();
        host.edge.tool_gate = Some((started.clone(), release.clone()));
        let bridge = tokio::spawn(async move {
            tx.send(Ok(sse_event(
                "tool_request",
                ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{}",
            )
            .into_bytes()))
                .await
                .unwrap();
            started.notified().await;
            let output = "x".repeat(4096);
            let control = sse_event(
                "agent_live_event",
                &format!(
                    ",\"run_id\":\"child-run\",\"agent_id\":\"child-agent\",\
                \"event_kind\":\"tool_completed\",\"name\":\"bash\",\
                \"description\":\"child tool\",\"tool_use_id\":\"child-tool\",\
                \"status\":\"completed\",\"duration_ms\":1,\"output\":{output:?}"
                ),
            );
            tx.send(Ok(control.repeat(300).into_bytes())).await.unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream_cancellable(
                &mut stream,
                &mut host,
                stream_idle_timeout(),
                Some(&cancel),
                None,
            ),
        )
        .await
        .expect("ordered observations must not block Edge settlement");
        assert_eq!(abort, None, "{:?}", result.accum.error_message);
        assert!(!cancel.is_cancelled());
        assert_eq!(host.observer.agent_live_events.len(), 300);
        assert!(host.observer.agent_live_gaps.is_empty());
        bridge.await.unwrap();
    }

    #[tokio::test]
    async fn framer_joins_utf8_split_across_tool_execution() {
        let (tx, mut stream) = test_channel();
        let bridge = tokio::spawn(async move {
            let tool = sse_event(
                "tool_request",
                ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo ok\"}",
            );
            let text = sse_event("text_delta", ",\"content\":\"中文\"");
            let split = text.find('中').unwrap() + 1;
            let mut first = tool.into_bytes();
            first.extend_from_slice(&text.as_bytes()[..split]);
            tx.send(Ok(first)).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            tx.send(Ok(text.as_bytes()[split..].to_vec()))
                .await
                .unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });
        let mut host =
            RecordingSseStreamHost::new().with_tool_delay(std::time::Duration::from_millis(200));
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()).await;
        bridge.await.unwrap();
        assert_eq!(
            abort, None,
            "valid UTF-8 split across execution boundary was rejected: {:?}",
            result.accum.error_message
        );
        assert_eq!(result.accum.full_text, "中文");
        assert_eq!(result.tool_results.len(), 1);
    }

    #[test]
    fn one_framer_preserves_crlf_boundaries_across_chunks() {
        let mut framer = ChatTurnSseFramer::new();
        let mut initial = b"data: {\"type\":\"tool_request\"}\r\n\r".as_slice();
        assert!(framer.next_frame(&mut initial).unwrap().is_some());
        let mut next = b"\ndata: {\"type\":\"ping\"}\r\n\r".as_slice();
        assert_eq!(
            framer.next_frame(&mut next).unwrap().as_deref(),
            Some("data: {\"type\":\"ping\"}")
        );
        let mut rest = b"\ndata: {\"type\":\"done\"}\n\n".as_slice();
        assert_eq!(
            framer.next_frame(&mut rest).unwrap().as_deref(),
            Some("data: {\"type\":\"done\"}")
        );
    }

    #[tokio::test]
    async fn invalid_or_incomplete_stream_cancels_running_edge_tool() {
        let cases: [(bool, Option<&'static [u8]>, bool, &'static str); 7] = [
            (false, Some(b"data: not-json"), false, "invalid JSON"),
            (true, Some(b"data: not-json"), false, "invalid JSON"),
            (false, Some(b"data: {broken}\n\n"), false, "invalid JSON"),
            (false, Some(b"data: {broken}\n\n"), true, "invalid JSON"),
            (true, Some(b"data: {broken}\n\n"), false, "invalid JSON"),
            (true, Some(b"data: {broken}\n\n"), true, "invalid JSON"),
            (true, None, false, MISSING_SSE_DONE),
        ];
        for (require_done, payload, keep_open, expected) in cases {
            let (tx, mut source) = test_channel();
            let started = std::sync::Arc::new(tokio::sync::Notify::new());
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut host = RecordingSseStreamHost::new();
            host.edge.tool_gate = Some((started.clone(), cancel.clone()));
            host.observer.require_sse_done = require_done;
            let bridge = tokio::spawn(async move {
                tx.send(Ok(format!(
                    "{}{}",
                    sse_event("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
                    sse_event(
                        "tool_request",
                        ",\"request_id\":\"t\",\"tool\":\"bash\",\"args\":{}"
                    )
                )
                .into_bytes()))
                    .await
                    .unwrap();
                started.notified().await;
                if let Some(payload) = payload {
                    tx.send(Ok(payload.to_vec())).await.unwrap();
                }
                if keep_open {
                    std::future::pending::<()>().await;
                }
            });
            let (result, abort) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                consume_sse_stream_cancellable(
                    &mut source,
                    &mut host,
                    stream_idle_timeout(),
                    Some(&cancel),
                    None,
                ),
            )
            .await
            .expect("invalid stream must settle the running tool");
            bridge.abort();
            assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
            assert!(cancel.is_cancelled());
            assert!(
                result
                    .accum
                    .error_message
                    .as_deref()
                    .unwrap()
                    .contains(expected)
            );
            assert!(!result.accum.stream_complete);
        }
    }

    #[tokio::test]
    async fn done_marker_is_accepted_at_chunk_boundary_or_eof() {
        for (strict, unterminated, completed_first) in [
            (false, false, false),
            (true, false, false),
            (true, true, false),
            (true, true, true),
        ] {
            let (tx, source) = test_channel();
            let started = std::sync::Arc::new(tokio::sync::Notify::new());
            let release = tokio_util::sync::CancellationToken::new();
            let mut source = Box::pin(stream::unfold(
                (source.0, release.clone(), 0usize),
                move |(mut rx, release, count)| async move {
                    let chunk = rx.recv().await;
                    if (count == 1 && !unterminated) || chunk.is_none() {
                        release.cancel();
                    }
                    chunk.map(|chunk| (chunk, (rx, release, count + 1)))
                },
            ));
            let mut host = RecordingSseStreamHost::new();
            host.edge.tool_gate = Some((started.clone(), release.clone()));
            host.observer.require_sse_done = strict;
            let bridge = tokio::spawn(async move {
                tx.send(Ok(format!(
                    "{}{}",
                    sse_event("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
                    sse_event(
                        "tool_request",
                        ",\"request_id\":\"t\",\"tool\":\"bash\",\"args\":{}"
                    )
                )
                .into_bytes()))
                    .await
                    .unwrap();
                started.notified().await;
                let mut chunk = if completed_first {
                    sse_event(
                        "run_finished",
                        ",\"run_id\":\"r\",\"owner_generation\":4,\"status\":\"completed\"",
                    )
                    .into_bytes()
                } else {
                    Vec::new()
                };
                chunk.extend_from_slice(b"data: [DONE]");
                if !unterminated {
                    chunk.extend_from_slice(b"\n\n");
                    chunk.resize(64 * 1024, b'x');
                    chunk.extend_from_slice(b"data: \xff\n\n");
                }
                tx.send(Ok(chunk)).await.unwrap();
                drop(tx);
            });
            let (result, abort) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                consume_sse_stream(&mut source, &mut host, stream_idle_timeout()),
            )
            .await
            .expect("tool and stream must complete");
            bridge.await.unwrap();
            assert_eq!(abort, None);
            assert!(result.accum.stream_complete);
            assert_eq!(host.observer.done_count, 1);
            assert_eq!(result.tool_results.len(), 1);
        }
    }

    #[tokio::test]
    async fn failed_root_terminal_stops_later_frames_in_same_chunk() {
        for (split, malformed_first) in [(false, false), (true, false), (false, true)] {
            let (tx, mut source) = test_channel();
            let started = std::sync::Arc::new(tokio::sync::Notify::new());
            let cancel = tokio_util::sync::CancellationToken::new();
            let mut host = RecordingSseStreamHost::new().strict();
            host.edge.tool_gate = Some((started.clone(), cancel.clone()));
            let bridge = tokio::spawn(async move {
                tx.send(Ok(format!(
                    "{}{}",
                    sse_event("session_info", ",\"session_id\":\"s\",\"run_id\":\"r\""),
                    sse_event(
                        "tool_request",
                        ",\"request_id\":\"t\",\"tool\":\"bash\",\"args\":{}"
                    )
                )
                .into_bytes()))
                    .await
                    .unwrap();
                started.notified().await;
                let terminal = sse_event(
                    "run_finished",
                    ",\"run_id\":\"r\",\"owner_generation\":4,\"status\":\"cancelled\"",
                );
                let cut = if split { terminal.len() / 2 } else { 0 };
                if cut != 0 {
                    tx.send(Ok(terminal.as_bytes()[..cut].to_vec()))
                        .await
                        .unwrap();
                }
                let mut chunk = Vec::new();
                if malformed_first {
                    chunk.extend_from_slice(b"data: {broken}\n\n");
                }
                chunk.extend_from_slice(&terminal.as_bytes()[cut..]);
                if !malformed_first {
                    chunk.extend_from_slice(b"data: \xff\n\n");
                }
                tx.send(Ok(chunk)).await.unwrap();
            });
            let (result, abort) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                consume_sse_stream_cancellable(
                    &mut source,
                    &mut host,
                    stream_idle_timeout(),
                    Some(&cancel),
                    None,
                ),
            )
            .await
            .expect("terminal must settle the running tool");
            bridge.await.unwrap();
            if malformed_first {
                assert_eq!(abort, Some(astra_core::ErrorKind::StreamTransport));
                assert!(result.accum.run_terminal.is_none());
            } else {
                assert_eq!(abort, Some(astra_core::ErrorKind::Cancelled));
                assert_eq!(
                    result.accum.run_terminal.unwrap().status,
                    DurableRunTerminalStatus::Cancelled
                );
            }
            assert!(cancel.is_cancelled());
        }
    }

    #[tokio::test]
    async fn framer_preserves_partial_event_across_two_tool_windows() {
        let (tx, mut stream) = test_channel();
        let bridge = tokio::spawn(async move {
            tx.send(Ok(sse_event(
                "tool_request",
                ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo one\"}",
            )
            .into_bytes()))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let tool = sse_event(
                "tool_request",
                ",\"request_id\":\"tool-2\",\"tool\":\"bash\",\"args\":{\"command\":\"echo two\"}",
            );
            let text = sse_event("text_delta", ",\"content\":\"中文\"");
            let split = text.find('中').unwrap() + 1;
            let mut second = tool.into_bytes();
            second.extend_from_slice(&text.as_bytes()[..split]);
            tx.send(Ok(second)).await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(590)).await;
            tx.send(Ok(text.as_bytes()[split..].to_vec()))
                .await
                .unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });
        let mut host =
            RecordingSseStreamHost::new().with_tool_delay(std::time::Duration::from_millis(500));
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()).await;
        bridge.await.unwrap();
        assert_eq!(
            abort, None,
            "successive Edge windows rejected valid UTF-8: {:?}",
            result.accum.error_message
        );
        assert_eq!(result.accum.full_text, "中文");
        assert_eq!(result.tool_results.len(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn queued_tool_request_keeps_its_receive_deadline() {
        let (tx, mut stream) = test_channel();
        let bridge = tokio::spawn(async move {
            tx.send(Ok(sse_event(
                "tool_request",
                ",\"request_id\":\"first\",\"tool\":\"bash\",\"args\":{\"command\":\"true\"}",
            )
            .into_bytes()))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let request = serde_json::json!({
                "type": "tool_request", "request_id": "queued", "tool": "read_file",
                "args": {"path": "note.txt"}, "schema_admitted_by_server": true,
                "execution_timeout_ms": 500,
                "execution_deadline_unix_ms": received_unix_ms() + 60_000,
            });
            tx.send(Ok(format!("data: {request}\n\n").into_bytes()))
                .await
                .unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });
        let mut host =
            RecordingSseStreamHost::new().with_tool_delay(std::time::Duration::from_secs(2));
        let (_, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()),
        )
        .await
        .expect("stream must settle");
        bridge.await.unwrap();
        assert_eq!(abort, None);
        let (_, deadline, admitted) = host
            .edge
            .admitted_deadlines
            .iter()
            .find(|(id, _, _)| id == "queued")
            .expect("queued request reaches admission");
        assert!(
            deadline <= admitted,
            "replay must not restart the 500 ms budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn unterminated_tool_request_keeps_its_last_receive_deadline() {
        let (tx, mut stream) = test_channel();
        let started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release = tokio_util::sync::CancellationToken::new();
        let bridge_started = started.clone();
        let bridge_release = release.clone();
        let bridge = tokio::spawn(async move {
            tx.send(Ok(sse_event(
                "tool_request",
                ",\"request_id\":\"first\",\"tool\":\"bash\",\"args\":{}",
            )
            .into_bytes()))
                .await
                .unwrap();
            bridge_started.notified().await;
            let request = serde_json::json!({
                "type": "tool_request", "request_id": "tail", "tool": "read_file",
                "args": {"path": "fixture"}, "schema_admitted_by_server": true,
                "execution_timeout_ms": 500,
                "execution_deadline_unix_ms": received_unix_ms() + 60_000,
            });
            // EOF is intentionally later than the last byte of the valid JSON event.
            tx.send(Ok(format!("data: {request}").into_bytes()))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            bridge_release.cancel();
        });
        let mut host = RecordingSseStreamHost::new();
        host.edge.tool_gate = Some((started, release));
        let (_, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(6),
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()),
        )
        .await
        .unwrap();
        bridge.await.unwrap();
        assert_eq!(abort, None);
        let (_, deadline, admitted) = host
            .edge
            .admitted_deadlines
            .iter()
            .find(|(id, _, _)| id == "tail")
            .expect("tail request is admitted");
        assert!(
            deadline <= admitted,
            "EOF must not restart the execution budget"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn coalescing_keeps_partial_event_before_new_network_bytes() {
        let (tx, mut stream) = test_channel();
        let bridge = tokio::spawn(async move {
            tx.send(Ok(sse_event(
                "tool_request",
                ",\"request_id\":\"tool-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo one\"}",
            )
            .into_bytes()))
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let tool = sse_event(
                "tool_request",
                ",\"request_id\":\"tool-2\",\"tool\":\"read_file\",\"args\":{\"path\":\"note.txt\"}",
            );
            let text = sse_event("text_delta", ",\"content\":\"中文\"");
            let split = text.find('中').unwrap() + 1;
            let mut second = tool.into_bytes();
            second.extend_from_slice(&text.as_bytes()[..split]);
            tx.send(Ok(second)).await.unwrap();
            // Tool 1 settles at 500 ms. The suffix arrives inside tool 2's
            // 25 ms coalescing window, after its queued partial prefix.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            tx.send(Ok(text.as_bytes()[split..].to_vec()))
                .await
                .unwrap();
            tx.send(Ok(b"data: [DONE]\n\n".to_vec())).await.unwrap();
        });
        let mut host =
            RecordingSseStreamHost::new().with_tool_delay(std::time::Duration::from_millis(500));
        let (result, abort) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            consume_sse_stream(&mut stream, &mut host, stream_idle_timeout()),
        )
        .await
        .expect("coalescing must settle");
        bridge.await.unwrap();
        assert_eq!(
            abort, None,
            "coalescing crossed a UTF-8 event boundary: {:?}",
            result.accum.error_message
        );
        assert_eq!(result.accum.full_text, "中文");
        assert_eq!(result.tool_results.len(), 2);
    }

    /// Adjacent concurrency-safe tool requests may arrive as separate SSE
    /// chunks. They must still execute as one batch; otherwise the first
    /// long-running agent spawn action blocks the socket reader and the UI only
    /// ever sees "1 parallel agent".
    #[tokio::test]
    async fn concurrent_agent_spawn_requests_coalesce_across_adjacent_chunks() {
        let (tx, rx) = test_channel();
        let mut stream = rx;

        let batch_sizes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        struct BatchHost(std::sync::Arc<std::sync::Mutex<Vec<usize>>>);
        #[async_trait]
        impl SseEdgeWork for BatchHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}
            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let rid = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: rid.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "ok".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 1,
                }
            }
            async fn execute_tools_batch(
                &mut self,
                requests: Vec<ToolBatchRequest>,
            ) -> Vec<EdgeToolExecResult> {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(requests.len());
                requests
                    .into_iter()
                    .map(|req| EdgeToolExecResult {
                        work_attribution: None,
                        execution_completion: None,
                        request_id: req.request_id,
                        tool: req.tool,
                        args: req.args,
                        output: "ok".to_string(),
                        tool_result_fields: None,
                        status: "completed".to_string(),
                        duration_ms: 1,
                    })
                    .collect()
            }
            async fn resolve_approval(
                &mut self,
                rid: &str,
                _: &str,
                _: ApprovalKind,
                _: Option<&str>,
                _: Option<&str>,
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

        let bridge = tokio::spawn(async move {
            let first = sse_event(
                "tool_request",
                ",\"request_id\":\"a1\",\"tool\":\"agent\",\"args\":{\"action\":\"spawn\",\"description\":\"review one\",\"prompt\":\"p1\",\"run_in_background\":true}",
            );
            let second = sse_event(
                "tool_request",
                ",\"request_id\":\"a2\",\"tool\":\"agent\",\"args\":{\"action\":\"spawn\",\"description\":\"review two\",\"prompt\":\"p2\",\"run_in_background\":true}",
            );
            let third = sse_event(
                "tool_request",
                ",\"request_id\":\"a3\",\"tool\":\"agent\",\"args\":{\"action\":\"spawn\",\"description\":\"review three\",\"prompt\":\"p3\",\"run_in_background\":true}",
            );
            tx.send(Ok(first.into_bytes())).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            tx.send(Ok(second.into_bytes())).await.unwrap();
            tokio::time::sleep(tokio::time::Duration::from_millis(5)).await;
            tx.send(Ok(third.into_bytes())).await.unwrap();
            drop(tx);
        });

        let mut host = TestEdgeOnly::new(BatchHost(batch_sizes.clone()));
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(500),
        )
        .await;

        assert!(abort.is_none(), "unexpected abort: {abort:?}");
        assert_eq!(result.tool_results.len(), 3);
        assert_eq!(
            *astra_core::sync_poison::recover_mutex_lock(&*batch_sizes),
            vec![3],
            "agent spawn requests should execute as one parallel batch"
        );
        bridge.await.unwrap();
    }

    /// Skill and non-skill tool_request in the same SSE block during
    /// streaming: skill must execute first.
    #[tokio::test]
    async fn skill_prioritized_over_regular_tool_in_same_chunk() {
        // Both independently framed requests arrive in one transport chunk.
        let block = format!(
            "data: {{\"type\":\"tool_request\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"command_timeout_cap_ms\":30000,\"execution_deadline_unix_ms\":4102444800000,\"request_id\":\"t-bash\",\"tool\":\"bash\",\"args\":{{}}}}\n\ndata: {{\"type\":\"tool_request\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"command_timeout_cap_ms\":30000,\"execution_deadline_unix_ms\":4102444800000,\"admitted_skill_names\":[\"test-skill\"],\"request_id\":\"t-skill\",\"tool\":\"{}\",\"args\":{{}}}}\n\n",
            "skill"
        );
        let chunks: Vec<Result<Vec<u8>, String>> = vec![Ok(block.into_bytes())];
        let mut stream = stream::iter(chunks);

        let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct OrderTrackingHost(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl SseEdgeWork for OrderTrackingHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}
            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let rid = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(tool.to_string());
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: rid.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: format!("ok-{tool}"),
                    tool_result_fields: None,
                    status: "completed".to_string(),
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

        let mut host = TestEdgeOnly::new(OrderTrackingHost(order.clone()));
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert_eq!(result.tool_results.len(), 2);
        let exec_order = astra_core::sync_poison::recover_mutex_lock(&order);
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
        let partial_tool = "data: {\"type\":\"tool_request\",\"schema_admitted_by_server\":true,\"execution_timeout_ms\":300000,\"command_timeout_cap_ms\":30000,\"execution_deadline_unix_ms\":4102444800000,\"request_id\":\"t1\",\"tool\":\"bash\",\"args\":{}}";
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
        impl SseEdgeWork for TrackingHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}
            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let request_id = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("exec:{request_id}:{tool}"));
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: request_id.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "ok".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 0,
                }
            }
            async fn resolve_approval(
                &mut self,
                request_id: &str,
                tool: &str,
                _approval_kind: ApprovalKind,
                _session_id: Option<&str>,
                _run_id: Option<&str>,
                _detail: Option<&str>,
                _display_label: Option<&str>,
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

        let mut host = TestEdgeOnly::new(TrackingHost(ops.clone()));
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());

        // Both approval and tool execution should be recorded.
        let recorded = astra_core::sync_poison::recover_mutex_lock(&ops);
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

    // ── Text is not execution authority ───────────────────────────────────────────────────

    #[tokio::test]
    async fn tool_syntax_in_text_never_creates_client_tool_calls() {
        // Even executable-looking syntax is not a typed Server tool request.
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
        let mut host = NoopSseStreamHost::default();
        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none());
        assert!(result.accum.tool_calls.is_empty());
        assert!(!result.accum.has_tool_calls);
        assert!(result.tool_results.is_empty());
        assert_eq!(result.accum.full_text, xml_text);
    }

    #[tokio::test]
    async fn server_owned_terminal_text_never_reopens_client_tool_execution() {
        let receipt = crate::tool_ledger_receipt::ToolLedgerReceipt::empty("run-1", 0);
        let assistant_text = "The remote run is complete. Example: <invoke name=\"bash\"></invoke>";
        let terminal = serde_json::json!({
            "type": "turn_complete",
            "continuation_owner": "server",
            "assistant_text": assistant_text,
            "tool_calls_count": 0,
            "observation_tool_calls_count": 0,
            "tools_used": [],
            "llm_rounds": 1,
            "tool_ledger_receipt": receipt,
        });
        let events = format!("data: {terminal}\n\n");
        let mut stream = stream::iter(chunks_from_sse(&events));
        let mut host = NoopSseStreamHost::default();

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;

        assert!(abort.is_none());
        assert!(result.accum.server_loop_terminal);
        assert!(result.accum.tool_calls.is_empty());
        assert!(!result.accum.has_tool_calls);
        assert_eq!(result.accum.full_text, assistant_text);
    }

    /// REGRESSION (reviewer L2-4): approval-before-tool ordering MUST
    /// hold across coalescing windows, not only within one. The
    /// invariant: when a tool requires approval, the approval
    /// request is resolved BEFORE the tool executes — even if the
    /// approval_required event lands in window N and the
    /// tool_request lands in window N+1 (e.g., adjacent SSE chunks
    /// straddle the 25ms coalesce boundary).
    ///
    /// Mechanism check:
    ///   - approval_required is NOT a `ChatTurnEdgePending::ToolRequest`,
    ///     so `pending_is_coalescible_tool_batch` returns false the
    ///     moment it lands.
    ///   - That makes the inner `while pending_is_coalescible_tool_batch
    ///     (&pending)` loop exit early.
    ///   - `flush_pending_via_host` runs approvals first, then tools.
    ///   - The next outer-loop iteration starts a fresh window for
    ///     the tool.
    ///
    /// Test by injecting a delay between the approval chunk and the
    /// tool chunk that EXCEEDS the coalescing window so they
    /// definitely fall into separate windows.
    #[tokio::test]
    async fn approval_resolves_before_tool_even_across_coalesce_windows() {
        let (tx, rx) = test_channel();
        let mut stream = rx;

        let ops = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        struct OrderHost(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        #[async_trait]
        impl SseEdgeWork for OrderHost {
            type Control = ();
            fn apply_control(&mut self, (): ()) {}
            async fn execute_tool(&mut self, request: &ToolBatchRequest) -> EdgeToolExecResult {
                let rid = request.request_id.as_str();
                let tool = request.tool.as_str();
                let args = &request.args;
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("exec:{rid}:{tool}"));
                EdgeToolExecResult {
                    work_attribution: None,
                    execution_completion: None,
                    request_id: rid.to_string(),
                    tool: tool.to_string(),
                    args: args.clone(),
                    output: "ok".to_string(),
                    tool_result_fields: None,
                    status: "completed".to_string(),
                    duration_ms: 0,
                }
            }
            async fn resolve_approval(
                &mut self,
                rid: &str,
                tool: &str,
                _: ApprovalKind,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
                _: Option<&str>,
            ) -> EdgeApprovalResult {
                self.0
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .push(format!("approve:{rid}:{tool}"));
                EdgeApprovalResult {
                    request_id: rid.to_string(),
                    decision: "allow".to_string(),
                    reason: None,
                }
            }
        }

        // Bridge: send approval, sleep WAY longer than the coalesce
        // window (25ms default), then send tool_request. They land in
        // different outer-loop iterations.
        let bridge = tokio::spawn(async move {
            let approval = sse_event(
                "approval_required",
                ",\"request_id\":\"shared-1\",\"tool\":\"str_replace\",\"approval_kind\":\"standard\",\"detail\":\"src/x.rs\"",
            );
            let tool_req = sse_event(
                "tool_request",
                ",\"request_id\":\"shared-1\",\"tool\":\"str_replace\",\"args\":{\"path\":\"src/x.rs\",\"old_str\":\"a\",\"new_str\":\"b\"}",
            );
            tx.send(Ok(approval.into_bytes())).await.unwrap();
            // 100ms ≫ 25ms coalesce window — guarantees the approval
            // chunk is fully processed and flushed before the tool
            // chunk arrives, so the tool lands in window N+1.
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            tx.send(Ok(tool_req.into_bytes())).await.unwrap();
            drop(tx);
        });

        let mut host = TestEdgeOnly::new(OrderHost(ops.clone()));
        let (_result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS),
        )
        .await;
        assert!(abort.is_none(), "unexpected abort: {abort:?}");

        let recorded = astra_core::sync_poison::recover_mutex_lock(&ops).clone();
        assert_eq!(
            recorded.len(),
            2,
            "expected approve + exec, got: {recorded:?}"
        );
        assert_eq!(
            recorded[0], "approve:shared-1:str_replace",
            "approval MUST resolve before the tool executes — even when \
             the two events arrive in different coalescing windows. \
             Got: {recorded:?}"
        );
        assert_eq!(recorded[1], "exec:shared-1:str_replace");

        bridge.await.unwrap();
    }
}
