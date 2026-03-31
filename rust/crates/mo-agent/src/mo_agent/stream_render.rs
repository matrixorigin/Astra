use super::*;
use futures_util::StreamExt;
use mo_agent_runtime::turn::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, ChatTurnSseFramer, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use mo_agent_runtime::turn::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
    edge_sse_thought_duration_line, edge_sse_tool_request_notice_line,
};
use mo_agent_runtime::turn::tool_result_semantics::cloud_tool_result_status_label;
use std::ops::{Deref, DerefMut};

pub use mo_agent_runtime::turn::chat_turn_sse_dispatch::ChatTurnEdgePending;

/// One tool executed from SSE `tool_request` (ordering preserved for synthetic `tool_calls`).
#[derive(Debug, Clone)]
pub(super) struct EdgeToolRoundEntry {
    pub request_id: String,
    pub tool: String,
    pub args: serde_json::Value,
    pub output: String,
}

impl mo_agent_runtime::turn::headless_tool_assembly::EdgeToolRoundRow for EdgeToolRoundEntry {
    fn tool_name(&self) -> &str {
        &self.tool
    }
    fn tool_args(&self) -> &serde_json::Value {
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

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
///
/// `perm_manager` must point at the same [`crate::permission_manager::PermissionManager`] used for
/// local tool checks; it must not be used elsewhere until [`consume_turn_sse`] returns.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: &'a crate::edge_tools::ToolExecutor,
    pub quiet: bool,
    pub perm_manager: Option<std::ptr::NonNull<crate::permission_manager::PermissionManager>>,
    pub _pm: std::marker::PhantomData<&'a mut crate::permission_manager::PermissionManager>,
}

async fn flush_pending_edge_work(
    pending: &mut Vec<ChatTurnEdgePending>,
    edge: Option<&EdgeSseContext<'_>>,
    result: &mut TurnResult,
) {
    if pending.is_empty() {
        return;
    }
    let Some(ctx) = edge else {
        pending.clear();
        return;
    };
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
                if !ctx.quiet {
                    eprintln!(
                        "{}",
                        edge_sse_tool_request_notice_line(&tool, &request_id).dim()
                    );
                }
                let allowed = match ctx.perm_manager {
                    Some(mut ptr) => unsafe { ptr.as_mut().check(&tool, &args) },
                    None => true,
                };
                let start = std::time::Instant::now();
                let output = if allowed {
                    ctx.executor.execute(&tool, &args).await
                } else {
                    "Permission denied".to_string()
                };
                let sig = mo_agent_runtime::turn::tool_result_semantics::tool_dedup_signature(
                    &tool, &args,
                );
                result.edge_callback_outputs.insert(sig, output.clone());
                result.edge_tool_round.push(EdgeToolRoundEntry {
                    request_id: request_id.clone(),
                    tool: tool.clone(),
                    args: args.clone(),
                    output: output.clone(),
                });
                let status = if !allowed {
                    "error"
                } else {
                    cloud_tool_result_status_label(&output)
                };
                let body = mo_thin_client::ToolResultRequest {
                    request_id,
                    status: status.to_string(),
                    output: Some(output),
                    duration_ms: Some(start.elapsed().as_millis() as u64),
                };
                if let Err(e) = ctx
                    .api
                    .post_tool_result(Some(ctx.token), Some(ctx.executor_id), &body)
                    .await
                    && !ctx.quiet
                {
                    eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
                }
            }
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                path,
            } => {
                if request_id.is_empty() {
                    continue;
                }
                let decision = match ctx.perm_manager {
                    Some(mut ptr) => unsafe {
                        ptr.as_mut()
                            .resolve_cloud_approval(&tool, path.as_deref(), ctx.quiet)
                    },
                    None => mo_thin_client::ApprovalDecision::Deny,
                };
                let body = mo_thin_client::ApprovalRespondRequest {
                    request_id,
                    decision,
                    reason: None,
                };
                if let Err(e) = ctx.api.post_approval(Some(ctx.token), &body).await
                    && !ctx.quiet
                {
                    eprintln!("{}", edge_sse_post_approval_fail_line(e).yellow());
                }
            }
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
    /// Outputs from SSE `tool_request` (same key as [`mo_agent_runtime::turn::tool_result_semantics::tool_dedup_signature`]).
    pub(super) edge_callback_outputs: std::collections::HashMap<String, String>,
    /// Ordered executions from this SSE stream (for rounds without legacy `tool_call` events).
    pub(super) edge_tool_round: Vec<EdgeToolRoundEntry>,
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
    pub(super) fn new() -> Self {
        Self {
            core: ChatTurnSseAccum::default(),
            ttft_ms: None,
            edge_callback_outputs: std::collections::HashMap::new(),
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
/// When `quiet` is true, all terminal output is suppressed but result.full_text is still captured.
pub(super) async fn consume_turn_sse(
    resp: mo_thin_client::HttpResponse,
    render_md: bool,
    term_width: usize,
    quiet: bool,
    edge: Option<EdgeSseContext<'_>>,
) -> TurnResult {
    let mut result = TurnResult::new();
    let mut render = StreamRenderState::new();
    let mut stream = resp.bytes_stream();
    let mut framer = ChatTurnSseFramer::new();
    let mut pending_edge: Vec<ChatTurnEdgePending> = Vec::new();

    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else { break };
        for event_str in framer.push_lossy_bytes(chunk.as_ref()) {
            let effects =
                dispatch_chat_turn_sse_event_block(&event_str, &mut result.core, &mut pending_edge);
            apply_sse_render_effects(effects, &mut render, quiet);
            flush_pending_edge_work(&mut pending_edge, edge.as_ref(), &mut result).await;
        }
    }
    let tail = framer.take_trailing_dispatch_blob();
    result.ttft_ms = framer.ttft_ms;
    if !tail.trim().is_empty() {
        let effects =
            dispatch_chat_turn_sse_event_block(&tail, &mut result.core, &mut pending_edge);
        apply_sse_render_effects(effects, &mut render, quiet);
        flush_pending_edge_work(&mut pending_edge, edge.as_ref(), &mut result).await;
    }

    // Ensure thinking spinner is cleaned up
    render.stop_thinking();

    if quiet {
        // In quiet mode: text is captured in result.full_text, no terminal output
        return result;
    }

    // Clear raw streamed text and re-render cleanly
    if !result.full_text.is_empty() {
        // Calculate how many visual lines the raw streamed text occupies
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
            // Single-line text: just clear the current line
            execute!(
                io::stdout(),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::CurrentLine)
            )
            .ok();
        }

        if result.has_tool_calls {
            // Before tool calls: print trimmed text with exactly one trailing newline
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
    // If no text at all (pure tool calls on subsequent turns), print nothing

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
