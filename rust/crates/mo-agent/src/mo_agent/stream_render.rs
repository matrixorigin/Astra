use super::*;
use futures_util::StreamExt;
use mo_agent_runtime::turn::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use mo_agent_runtime::turn::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
    edge_sse_thought_duration_line, edge_sse_tool_request_notice_line,
};
use mo_agent_runtime::turn::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, NoopSseStreamHost, SseStreamHost, consume_sse_stream,
    STREAM_IDLE_TIMEOUT_MS,
};
use mo_agent_runtime::turn::tool_result_semantics::cloud_tool_result_status_label;
use std::ops::{Deref, DerefMut};

pub use mo_agent_runtime::turn::chat_turn_sse_dispatch::ChatTurnEdgePending;

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: &'a crate::edge_tools::ToolExecutor,
    pub quiet: bool,
    pub perm_manager: Option<&'a mut crate::permission_manager::PermissionManager>,
}

// ─── CLI SSE stream host ─────────────────────────────────────────────────────
//
// Implements the runtime's `SseStreamHost` trait, wiring terminal rendering,
// local tool execution, and permission prompts into the generic SSE consumer.

/// CLI host for SSE stream consumption.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream`] while handling:
/// - Terminal rendering (spinners, text deltas) via [`StreamRenderState`]
/// - Edge tool execution via [`crate::edge_tools::ToolExecutor`]
/// - Approval prompts via [`crate::permission_manager::PermissionManager`]
/// - Cloud API posting (tool results, approvals) via [`mo_thin_client::ThinClient`]
struct CliSseStreamHost<'a> {
    api: &'a mo_thin_client::ThinClient,
    token: &'a str,
    executor_id: &'a str,
    executor: &'a crate::edge_tools::ToolExecutor,
    quiet: bool,
    perm_manager: Option<&'a mut crate::permission_manager::PermissionManager>,
    render: StreamRenderState,
    /// Ordered tool executions from this SSE stream.
    pub edge_tool_round: Vec<EdgeToolExecResult>,
}

impl<'a> CliSseStreamHost<'a> {
    fn from_edge_ctx(ctx: EdgeSseContext<'a>) -> Self {
        Self {
            api: ctx.api,
            token: ctx.token,
            executor_id: ctx.executor_id,
            executor: ctx.executor,
            quiet: ctx.quiet,
            perm_manager: ctx.perm_manager,
            render: StreamRenderState::new(),
            edge_tool_round: Vec::new(),
        }
    }
}

#[async_trait::async_trait]
impl SseStreamHost for CliSseStreamHost<'_> {
    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        if self.quiet {
            return;
        }
        for effect in effects {
            match effect {
                SseRenderEffect::StreamText(s) => {
                    print!("{s}");
                    let _ = io::stdout().flush();
                }
                SseRenderEffect::StopThinkingSpinner => self.render.stop_thinking(),
                SseRenderEffect::StartThinkingSpinner => self.render.start_thinking(),
            }
        }
    }

    fn on_stream_complete(&mut self) {
        self.render.stop_thinking();
    }

    async fn execute_tool(
        &mut self,
        request_id: &str,
        tool: &str,
        args: &serde_json::Value,
    ) -> EdgeToolExecResult {
        if !self.quiet {
            eprintln!(
                "{}",
                edge_sse_tool_request_notice_line(tool, request_id).dim()
            );
        }
        let allowed = match &mut self.perm_manager {
            Some(pm) => pm.check(tool, args),
            None => true,
        };
        let start = std::time::Instant::now();
        let output = if allowed {
            self.executor.execute(tool, args).await
        } else {
            "Permission denied".to_string()
        };
        let status = if !allowed {
            "error"
        } else {
            cloud_tool_result_status_label(&output)
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        self.edge_tool_round.push(EdgeToolExecResult {
            request_id: request_id.to_string(),
            tool: tool.to_string(),
            args: args.clone(),
            output: output.clone(),
            status: status.to_string(),
            duration_ms,
        });
        let body = mo_thin_client::ToolResultRequest {
            request_id: request_id.to_string(),
            status: status.to_string(),
            output: Some(output),
            duration_ms: Some(duration_ms),
        };
        if let Err(e) = self
            .api
            .post_tool_result(Some(self.token), Some(self.executor_id), &body)
            .await
            && !self.quiet
        {
            eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
        }
        self.edge_tool_round.last().unwrap().clone()
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        tool: &str,
        path: Option<&str>,
    ) -> EdgeApprovalResult {
        let decision = match &mut self.perm_manager {
            Some(pm) => pm.resolve_cloud_approval(tool, path, self.quiet),
            None => mo_thin_client::ApprovalDecision::Deny,
        };
        let decision_str = match &decision {
            mo_thin_client::ApprovalDecision::Allow => "allow",
            _ => "deny",
        };
        let body = mo_thin_client::ApprovalRespondRequest {
            request_id: request_id.to_string(),
            decision,
            reason: None,
        };
        if let Err(e) = self.api.post_approval(Some(self.token), &body).await
            && !self.quiet
        {
            eprintln!("{}", edge_sse_post_approval_fail_line(e).yellow());
        }
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: decision_str.to_string(),
            reason: None,
        }
    }
}

// ═══════════════════════════════════════════════════════════════ Spinner ══

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A spinner that runs in a background thread.
pub(super) struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner with the given prefix text (e.g., "  🔧 read_file").
    pub(super) fn start(prefix: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let mut idx = 0usize;
            while !stop2.load(Ordering::Relaxed) {
                let frame = SPINNER_FRAMES[idx % SPINNER_FRAMES.len()];
                eprint!(
                    "\r  {} {}",
                    prefix.as_str().cyan(),
                    format!("{frame}").yellow()
                );
                let _ = io::stderr().flush();
                idx += 1;
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and clear its line. Returns nothing.
    pub(super) fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Clear the spinner line
        eprint!("\r{}\r", " ".repeat(80));
        let _ = io::stderr().flush();
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ─── Turn result from one /chat/turn SSE stream ───────────────────────────────

/// One turn: core fields from [`ChatTurnSseAccum`] plus CLI-only edge bookkeeping and TTFT.
pub(super) struct TurnResult {
    pub(super) core: ChatTurnSseAccum,
    /// Time to first token in milliseconds (streaming latency).
    pub(super) ttft_ms: Option<u64>,
    /// Ordered executions from this SSE stream (for rounds without legacy `tool_call` events).
    pub(super) edge_tool_round: Vec<EdgeToolExecResult>,
}

impl Deref for TurnResult {
    type Target = ChatTurnSseAccum;

    fn deref(&self) -> &Self::Target {
        &self.core
    }
}

impl DerefMut for TurnResult {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.core
    }
}

impl TurnResult {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self {
            core: ChatTurnSseAccum::default(),
            ttft_ms: None,
            edge_tool_round: Vec::new(),
        }
    }
}

/// Live rendering state tracked across SSE chunks within one turn.
pub(super) struct StreamRenderState {
    thinking_start: Option<Instant>,
    thinking_spinner: Option<Spinner>,
}

impl StreamRenderState {
    pub(super) fn new() -> Self {
        Self {
            thinking_start: None,
            thinking_spinner: None,
        }
    }

    fn start_thinking(&mut self) {
        if self.thinking_spinner.is_none() {
            self.thinking_start = Some(Instant::now());
            self.thinking_spinner = Some(Spinner::start("  ● Thinking".to_string()));
        }
    }

    fn stop_thinking(&mut self) {
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
            if let Some(start) = self.thinking_start.take() {
                let elapsed = start.elapsed().as_secs_f64();
                eprintln!("{}", edge_sse_thought_duration_line(elapsed).dim());
            }
        }
    }
}

fn apply_sse_render_effects(
    effects: Vec<SseRenderEffect>,
    render: &mut StreamRenderState,
    quiet: bool,
) {
    if quiet {
        return;
    }
    for effect in effects {
        match effect {
            SseRenderEffect::StreamText(s) => {
                print!("{s}");
                let _ = io::stdout().flush();
            }
            SseRenderEffect::StopThinkingSpinner => render.stop_thinking(),
            SseRenderEffect::StartThinkingSpinner => render.start_thinking(),
        }
    }
}

/// Consume one /chat/turn SSE stream, render text deltas, collect tool_calls.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream`]; CLI-specific
/// rendering, tool execution, and approval prompts are handled by [`CliSseStreamHost`].
///
/// When `quiet` is true, all terminal output is suppressed but result.full_text is still captured.
pub(super) async fn consume_turn_sse(
    resp: mo_thin_client::HttpResponse,
    render_md: bool,
    term_width: usize,
    quiet: bool,
    edge: Option<EdgeSseContext<'_>>,
) -> TurnResult {
    // Convert reqwest byte stream to runtime's generic chunk type
    let mut byte_stream = Box::pin(
        resp.bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(|e| e.to_string())),
    );

    // Delegate to runtime's generic SSE consumer with the appropriate host
    let idle = std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS);
    let (sse_result, edge_tool_round) = if let Some(ctx) = edge {
        let mut host = CliSseStreamHost::from_edge_ctx(ctx);
        let (result, _abort) = consume_sse_stream(&mut byte_stream, &mut host, idle).await;
        (result, host.edge_tool_round)
    } else {
        let mut host = NoopSseStreamHost;
        let (result, _abort) = consume_sse_stream(&mut byte_stream, &mut host, idle).await;
        (result, Vec::new())
    };

    let result = TurnResult {
        core: sse_result.accum,
        ttft_ms: sse_result.ttft_ms,
        edge_tool_round,
    };

    if quiet {
        return result;
    }

    // ─── Terminal re-render (CLI-specific) ────────────────────────────────
    // Clear raw streamed text and re-render cleanly with markdown support
    if !result.full_text.is_empty() {
        let tw = term_width.max(1);
        let mut visual_lines = 0usize;
        let mut col = 0usize;
        for ch in result.full_text.chars() {
            if ch == '\n' {
                visual_lines += 1;
                col = 0;
            } else {
                col += 1;
                if col >= tw {
                    visual_lines += 1;
                    col = 0;
                }
            }
        }
        if visual_lines > 0 {
            execute!(
                io::stdout(),
                cursor::MoveUp(visual_lines as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
        } else {
            execute!(
                io::stdout(),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::CurrentLine)
            )
            .ok();
        }

        if result.has_tool_calls {
            let trimmed = result.full_text.trim();
            if !trimmed.is_empty() {
                if render_md {
                    print_markdown(trimmed);
                } else {
                    println!("{trimmed}");
                }
            }
        } else if render_md {
            print_markdown(&result.full_text);
        } else {
            println!("{}", result.full_text.trim_end());
        }
    }

    result
}

/// Used by `main` test module and stream_render unit tests; production path is [`consume_turn_sse`].
#[allow(dead_code)]
pub(super) fn dispatch_turn_event_block(
    block: &str,
    result: &mut TurnResult,
    render: &mut StreamRenderState,
    quiet: bool,
    pending_edge: &mut Vec<ChatTurnEdgePending>,
) {
    let effects = dispatch_chat_turn_sse_event_block(block, &mut result.core, pending_edge);
    apply_sse_render_effects(effects, render, quiet);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sse(event_type: &str, extra: &str) -> String {
        format!("data: {{\"type\":\"{event_type}\"{extra}}}\n\n")
    }

    /// `dispatch_turn_event_block` with `quiet` must still fill the shared runtime accumulator.
    #[test]
    fn dispatch_quiet_wires_runtime_accumulator() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"hello \""),
            sse("text_delta", ",\"content\":\"world\""),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, true, &mut vec![]);
        assert_eq!(r.full_text, "hello world");
    }

    #[test]
    fn tool_request_enqueues_pending() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"tool_request\",\"request_id\":\"tr-1\",\"tool\":\"bash\",\"args\":{\"command\":\"echo x\"}}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, true, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ToolRequest {
                request_id,
                tool,
                args,
            } => {
                assert_eq!(request_id, "tr-1");
                assert_eq!(tool, "bash");
                assert_eq!(args["command"], "echo x");
            }
            _ => panic!("expected ToolRequest"),
        }
    }

    #[test]
    fn approval_required_enqueues_pending() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let mut pending = Vec::new();
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"path\":\"src/x.rs\"}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, true, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                path,
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(path.as_deref(), Some("src/x.rs"));
            }
            _ => panic!("expected ApprovalRequired"),
        }
    }
}
