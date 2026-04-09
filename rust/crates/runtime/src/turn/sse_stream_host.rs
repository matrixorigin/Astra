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

use crate::turn::chat_turn_sse_dispatch::{
    ChatTurnEdgePending, ChatTurnSseAccum, ChatTurnSseFramer, SseRenderEffect,
    dispatch_chat_turn_sse_event_block,
};
use async_trait::async_trait;
use serde_json::Value;

/// Stream idle watchdog: abort SSE consumption if no chunk arrives within this time.
pub const STREAM_IDLE_TIMEOUT_MS: u64 = 90_000;

// ─── Data types ──────────────────────────────────────────────────────────────

/// Result of executing an edge tool request via the host.
#[derive(Debug, Clone)]
pub struct EdgeToolExecResult {
    pub request_id: String,
    pub tool: String,
    pub args: Value,
    pub output: String,
    /// Semantic label: `"ok"`, `"error"`, etc.
    pub status: String,
    pub duration_ms: u64,
}

impl crate::turn::headless_tool_assembly::EdgeToolRoundRow for EdgeToolExecResult {
    fn tool_name(&self) -> &str {
        &self.tool
    }
    fn tool_args(&self) -> &Value {
        &self.args
    }
    fn tool_output(&self) -> &str {
        &self.output
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
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseAbortReason {
    IdleTimeout,
    TransportError,
    /// User-initiated cancellation via [`CancellationToken`].
    Cancelled,
}

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
        detail: Option<&str>,
    ) -> EdgeApprovalResult;
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
    consume_sse_stream_cancellable(chunks, host, idle_timeout, None).await
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
) -> (SseConsumeResult, Option<SseAbortReason>) {
    use futures_util::StreamExt;

    let mut accum = ChatTurnSseAccum::default();
    let mut framer = ChatTurnSseFramer::new();
    let mut pending: Vec<ChatTurnEdgePending> = Vec::new();
    let mut tool_results: Vec<EdgeToolExecResult> = Vec::new();
    let mut approval_results: Vec<EdgeApprovalResult> = Vec::new();
    let mut abort: Option<SseAbortReason> = None;
    let mut first_sse_frame_seen = false;

    host.on_before_sse_read_loop();

    let idle = idle_timeout;
    // Short tick for UI heartbeat (thinking pane elapsed timer refresh).
    let tick = std::time::Duration::from_secs(1);
    loop {
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
                            abort = Some(SseAbortReason::Cancelled);
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
                abort = Some(SseAbortReason::IdleTimeout);
                break;
            }
        };
        let Some(chunk) = chunk else { break };
        let Ok(bytes) = chunk else {
            abort = Some(SseAbortReason::TransportError);
            break;
        };
        for event_str in framer.push_lossy_bytes(&bytes) {
            if !first_sse_frame_seen {
                first_sse_frame_seen = true;
                host.on_first_sse_frame();
            }
            let effects = dispatch_chat_turn_sse_event_block(&event_str, &mut accum, &mut pending);
            host.on_render_effects(effects);
            // Skill-exclusivity: reorder so skill calls execute before
            // non-skill calls within the same batch.
            prioritize_skill_tools(&mut pending);
            flush_pending_via_host(&mut pending, host, &mut tool_results, &mut approval_results)
                .await;
        }
    }

    // Tombstone on abort (timeout or cancellation).
    if matches!(
        abort,
        Some(SseAbortReason::IdleTimeout) | Some(SseAbortReason::Cancelled)
    ) {
        accum.full_text.clear();
        accum.reasoning_content.clear();
        accum.tool_calls.clear();
        pending.clear();
        let msg = match abort {
            Some(SseAbortReason::IdleTimeout) => {
                format!("Error: stream idle timeout after {}ms", idle.as_millis())
            }
            Some(SseAbortReason::Cancelled) => "Cancelled by user".to_string(),
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
        let effects = dispatch_chat_turn_sse_event_block(&tail, &mut accum, &mut pending);
        host.on_render_effects(effects);
        prioritize_skill_tools(&mut pending);
        flush_pending_via_host(&mut pending, host, &mut tool_results, &mut approval_results).await;
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
            ChatTurnEdgePending::ToolRequest { tool, .. }
                if tool == crate::turn::skill_tool::SKILL_TOOL_NAME =>
            {
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
    tool_results: &mut Vec<EdgeToolExecResult>,
    approval_results: &mut Vec<EdgeApprovalResult>,
) {
    for item in std::mem::take(pending) {
        match item {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                tool,
                args,
            } => {
                if request_id.is_empty() || tool.is_empty() {
                    continue;
                }
                let result = host.execute_tool(&request_id, &tool, &args).await;
                tool_results.push(result);
            }
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                detail,
            } => {
                if request_id.is_empty() {
                    continue;
                }
                let result = host
                    .resolve_approval(&request_id, &tool, detail.as_deref())
                    .await;
                approval_results.push(result);
            }
        }
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
            status: "error".to_string(),
            duration_ms: 0,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
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
    stream_completed: bool,
}

#[cfg(test)]
impl RecordingSseStreamHost {
    fn new() -> Self {
        Self {
            render_effects: Vec::new(),
            tool_outputs: std::collections::HashMap::new(),
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
            status: "ok".to_string(),
            duration_ms: 1,
        }
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        _tool: &str,
        _detail: Option<&str>,
    ) -> EdgeApprovalResult {
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
        let events = sse_event("usage", ",\"prompt_tokens\":100,\"completion_tokens\":50");
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
            ",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"",
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
                "tool_call",
                ",\"id\":\"tc-1\",\"name\":\"bash\",\"args\":\"{\\\"command\\\":\\\"ls\\\"}\""
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
        let (result, abort) =
            consume_sse_stream(&mut stream, &mut host, std::time::Duration::from_millis(5)).await;

        assert_eq!(abort, Some(SseAbortReason::IdleTimeout));
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
        )
        .await;

        assert_eq!(abort, Some(SseAbortReason::Cancelled));
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

        assert_eq!(abort, Some(SseAbortReason::TransportError));
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
            make_tool_pending(crate::turn::skill_tool::SKILL_TOOL_NAME),
            make_tool_pending("grep"),
        ];
        super::prioritize_skill_tools(&mut items);

        assert_eq!(
            tool_name(&items[0]),
            crate::turn::skill_tool::SKILL_TOOL_NAME
        );
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
            make_tool_pending(crate::turn::skill_tool::SKILL_TOOL_NAME),
            make_tool_pending("write_file"),
            make_tool_pending(crate::turn::skill_tool::SKILL_TOOL_NAME),
        ];
        super::prioritize_skill_tools(&mut items);

        assert_eq!(
            tool_name(&items[0]),
            crate::turn::skill_tool::SKILL_TOOL_NAME
        );
        assert_eq!(
            tool_name(&items[1]),
            crate::turn::skill_tool::SKILL_TOOL_NAME
        );
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
            crate::turn::skill_tool::SKILL_TOOL_NAME
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
                    status: "ok".to_string(),
                    duration_ms: 1,
                }
            }
            async fn resolve_approval(
                &mut self,
                rid: &str,
                _: &str,
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
            exec_order[0],
            crate::turn::skill_tool::SKILL_TOOL_NAME,
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

        let (result, abort) = consume_sse_stream(
            &mut stream,
            &mut host,
            std::time::Duration::from_millis(100),
        )
        .await;

        assert_eq!(abort, Some(SseAbortReason::IdleTimeout));
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
}
