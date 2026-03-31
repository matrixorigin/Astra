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
        path: Option<&str>,
    ) -> EdgeApprovalResult;
}

// ─── Generic SSE consumer ────────────────────────────────────────────────────

/// Consume an SSE byte stream using the provided host for rendering and edge work.
///
/// This is the **runtime-generic** equivalent of CLI's `consume_turn_sse`.
/// The protocol parsing (framing, event dispatch, accumulation) runs identically
/// in all contexts; only the host callbacks differ.
pub async fn consume_sse_stream<H: SseStreamHost>(
    chunks: &mut (dyn futures_util::Stream<Item = Result<Vec<u8>, String>> + Unpin + Send),
    host: &mut H,
) -> SseConsumeResult {
    use futures_util::StreamExt;

    let mut accum = ChatTurnSseAccum::default();
    let mut framer = ChatTurnSseFramer::new();
    let mut pending: Vec<ChatTurnEdgePending> = Vec::new();
    let mut tool_results: Vec<EdgeToolExecResult> = Vec::new();
    let mut approval_results: Vec<EdgeApprovalResult> = Vec::new();

    while let Some(chunk) = chunks.next().await {
        let Ok(bytes) = chunk else { break };
        for event_str in framer.push_lossy_bytes(&bytes) {
            let effects =
                dispatch_chat_turn_sse_event_block(&event_str, &mut accum, &mut pending);
            host.on_render_effects(effects);
            flush_pending_via_host(&mut pending, host, &mut tool_results, &mut approval_results)
                .await;
        }
    }

    let tail = framer.take_trailing_dispatch_blob();
    let ttft_ms = framer.ttft_ms;
    if !tail.trim().is_empty() {
        let effects = dispatch_chat_turn_sse_event_block(&tail, &mut accum, &mut pending);
        host.on_render_effects(effects);
        flush_pending_via_host(&mut pending, host, &mut tool_results, &mut approval_results)
            .await;
    }

    host.on_stream_complete();

    SseConsumeResult {
        accum,
        ttft_ms,
        tool_results,
        approval_results,
    }
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
                path,
            } => {
                if request_id.is_empty() {
                    continue;
                }
                let result = host
                    .resolve_approval(&request_id, &tool, path.as_deref())
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
        _path: Option<&str>,
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
        self.tool_outputs.insert(tool.to_string(), output.to_string());
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
        _path: Option<&str>,
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
        let result = consume_sse_stream(&mut stream, &mut host).await;
        assert_eq!(result.accum.full_text, "hello world");
        assert!(result.tool_results.is_empty());
        assert!(result.approval_results.is_empty());
    }

    #[tokio::test]
    async fn noop_host_captures_usage() {
        let events = sse_event(
            "usage",
            ",\"prompt_tokens\":100,\"completion_tokens\":50",
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let result = consume_sse_stream(&mut stream, &mut host).await;
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
        let result = consume_sse_stream(&mut stream, &mut host).await;

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
        let result = consume_sse_stream(&mut stream, &mut host).await;

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
            ",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"path\":\"src/x.rs\"",
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let result = consume_sse_stream(&mut stream, &mut host).await;

        assert_eq!(result.approval_results.len(), 1);
        assert_eq!(result.approval_results[0].request_id, "ap-1");
        assert_eq!(result.approval_results[0].decision, "allow");
    }

    #[tokio::test]
    async fn empty_request_ids_are_skipped() {
        let events = format!(
            "{}{}",
            sse_event("tool_request", ",\"request_id\":\"\",\"tool\":\"bash\",\"args\":{}"),
            sse_event("approval_required", ",\"request_id\":\"\",\"tool\":\"x\""),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = RecordingSseStreamHost::new();
        let result = consume_sse_stream(&mut stream, &mut host).await;

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
        let result = consume_sse_stream(&mut stream, &mut host).await;
        assert_eq!(result.accum.session_id.as_deref(), Some("sess-42"));
        assert_eq!(result.accum.run_id.as_deref(), Some("run-7"));
    }

    #[tokio::test]
    async fn multi_chunk_framing() {
        // Split one event across two chunks
        let part1 = "data: {\"type\":\"text_delta\",\"content\":\"he";
        let part2 = "llo\"}\n\n";
        let chunks: Vec<Result<Vec<u8>, String>> = vec![
            Ok(part1.as_bytes().to_vec()),
            Ok(part2.as_bytes().to_vec()),
        ];
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let result = consume_sse_stream(&mut stream, &mut host).await;
        assert_eq!(result.accum.full_text, "hello");
    }

    #[tokio::test]
    async fn tool_call_events_captured() {
        // `has_tool_calls` is set by the `turn_complete` event
        let events = format!(
            "{}{}",
            sse_event("tool_call", ",\"id\":\"tc-1\",\"name\":\"bash\",\"args\":\"{\\\"command\\\":\\\"ls\\\"}\""),
            sse_event("turn_complete", ",\"has_tool_calls\":true"),
        );
        let chunks = chunks_from_sse(&events);
        let mut stream = stream::iter(chunks);
        let mut host = NoopSseStreamHost;
        let result = consume_sse_stream(&mut stream, &mut host).await;
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
        let result = consume_sse_stream(&mut stream, &mut host).await;

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
        let result = consume_sse_stream(&mut stream, &mut host).await;
        // dispatch prepends "Error: " to the message
        assert_eq!(
            result.accum.error_message.as_deref(),
            Some("Error: rate limited")
        );
    }
}
