use super::*;
use futures_util::StreamExt;
use mo_agent_runtime::turn::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use mo_agent_runtime::turn::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
    edge_sse_thought_duration_line,
};
use mo_agent_runtime::turn::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, NoopSseStreamHost, STREAM_IDLE_TIMEOUT_MS,
    SseStreamHost, consume_sse_stream,
};
use mo_agent_runtime::turn::tool_result_semantics::cloud_tool_result_status_label;
use serde_json::Value;
use std::io::IsTerminal;
use std::ops::{Deref, DerefMut};

pub use mo_agent_runtime::turn::chat_turn_sse_dispatch::ChatTurnEdgePending;

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: &'a crate::edge_tools::ToolExecutor,
    pub quiet: bool,
    pub suppress_intermediate_output: bool,
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
    suppress_intermediate_output: bool,
    perm_manager: Option<&'a mut crate::permission_manager::PermissionManager>,
    render: StreamRenderState,
    /// Once this turn has emitted or requested tool work, hide any further prose
    /// so we don't flash an intermediate draft that will be invalidated.
    tool_work_detected: bool,
    /// Ordered tool executions from this SSE stream.
    pub edge_tool_round: Vec<EdgeToolExecResult>,
    // ── XML tag suppression ────────────────────────────────────────────
    /// Text accumulated while inside an open `<think>`/`<reflect>` tag.
    /// Flushed (after stripping the tags) once the closing tag arrives.
    /// Empty when not inside a tag — text goes directly to the renderer.
    xml_tag_buffer: String,
}

impl<'a> CliSseStreamHost<'a> {
    fn from_edge_ctx(ctx: EdgeSseContext<'a>, term_width: usize, render_md: bool) -> Self {
        Self {
            api: ctx.api,
            token: ctx.token,
            executor_id: ctx.executor_id,
            executor: ctx.executor,
            quiet: ctx.quiet,
            suppress_intermediate_output: ctx.suppress_intermediate_output,
            perm_manager: ctx.perm_manager,
            render: StreamRenderState::with_term_width(term_width, render_md),
            tool_work_detected: false,
            edge_tool_round: Vec::new(),
            xml_tag_buffer: String::new(),
        }
    }

    /// Push text to the active renderer (markdown or raw stdout).
    fn render_text(&mut self, s: &str) {
        if let Some(md) = &mut self.render.md {
            md.push(s);
        } else {
            print!("{s}");
            let _ = io::stdout().flush();
            self.render.track_output(s);
        }
    }

    /// Accept a text delta, suppressing content inside XML thinking tags.
    /// Text outside tags is rendered immediately (preserving streaming UX).
    /// Handles tags split across SSE chunks by holding back partial `<…` tails.
    fn push_text(&mut self, s: &str) {
        self.xml_tag_buffer.push_str(s);

        // Fast path: no tag markers at all.
        if !self.xml_tag_buffer.contains('<') {
            let buf = std::mem::take(&mut self.xml_tag_buffer);
            self.render_text(&buf);
            return;
        }

        // Check if there's an open thinking tag.
        if super::streaming_md::has_open_xml_tag(&self.xml_tag_buffer) {
            // Still inside a tag — keep buffering, don't render.
            return;
        }

        // Check for a potential incomplete thinking tag at the end of the buffer.
        // Only hold back if the tail could plausibly become one of our known tags.
        if let Some(last_lt) = self.xml_tag_buffer.rfind('<') {
            let tail = &self.xml_tag_buffer[last_lt..];
            if !tail.contains('>') && could_become_thinking_tag(tail) {
                // Potential partial tag — split: flush before, hold tail.
                let before = self.xml_tag_buffer[..last_lt].to_string();
                let held = self.xml_tag_buffer[last_lt..].to_string();
                self.xml_tag_buffer = held;
                if !before.is_empty() {
                    let mut buf = before;
                    super::streaming_md::strip_xml_tags_inplace(&mut buf);
                    if !buf.is_empty() {
                        self.render_text(&buf);
                    }
                }
                return;
            }
        }

        // Tag is closed (or there was never one).  Strip and flush.
        let mut buf = std::mem::take(&mut self.xml_tag_buffer);
        super::streaming_md::strip_xml_tags_inplace(&mut buf);
        if !buf.is_empty() {
            self.render_text(&buf);
        }
    }
}

/// Returns true if `partial` could become one of our known thinking tags.
/// E.g., "<", "<t", "<th", "<thi", "</", "</t", "</think" etc.
fn could_become_thinking_tag(partial: &str) -> bool {
    const PREFIXES: &[&str] = &[
        "<t",
        "<th",
        "<thi",
        "<thin",
        "<think",
        "<r",
        "<re",
        "<ref",
        "<refl",
        "<refle",
        "<reflec",
        "<reflect",
        "<i",
        "<in",
        "<inn",
        "<inne",
        "<inner",
        "<inner_",
        "</t",
        "</th",
        "</thi",
        "</thin",
        "</think",
        "</r",
        "</re",
        "</ref",
        "</refl",
        "</refle",
        "</reflec",
        "</reflect",
        "</i",
        "</in",
        "</inn",
        "</inne",
        "</inner",
        "</inner_",
    ];
    // Also match bare "<" or "</" which could become anything
    if partial == "<" || partial == "</" {
        return true;
    }
    PREFIXES
        .iter()
        .any(|p| p.starts_with(partial) || partial.starts_with(p))
}

#[async_trait::async_trait]
impl SseStreamHost for CliSseStreamHost<'_> {
    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        if self.quiet || self.suppress_intermediate_output {
            return;
        }
        for effect in effects {
            match effect {
                SseRenderEffect::StreamText(s) => {
                    // When tool_work_detected, buffer text instead of discarding.
                    // It will be rendered at stream completion if it's the final answer.
                    if self.tool_work_detected {
                        self.xml_tag_buffer.push_str(&s);
                        continue;
                    }
                    self.push_text(&s);
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
        // Clear text that was rendered or buffered BEFORE the first tool call
        // (intermediate draft). After first tool, keep buffering new text.
        if !self.tool_work_detected {
            self.tool_work_detected = true;
            // Discard any XML-tag-buffered text that was never rendered.
            self.xml_tag_buffer.clear();

            // Clear text that WAS already rendered (intermediate draft).
            if let Some(md) = &mut self.render.md {
                md.discard_and_reset();
            } else if self.render.lines_written > 0 && io::stdout().is_terminal() {
                execute!(
                    io::stdout(),
                    cursor::MoveUp(self.render.lines_written as u16),
                    cursor::MoveToColumn(0),
                    terminal::Clear(terminal::ClearType::FromCursorDown)
                )
                .ok();
                self.render.lines_written = 0;
                self.render.col = 0;
            }
        }
        // Show tool as running (in-place updatable via TerminalRegion).
        let tool_idx = if !self.quiet && !self.suppress_intermediate_output {
            Some(self.render.tool_start(tool, args))
        } else {
            None
        };
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
        // Update tool line to show completion.
        if let Some(idx) = tool_idx {
            self.render
                .tool_done(idx, tool, status, duration_ms, &output);
        }
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
            && !self.suppress_intermediate_output
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
            Some(pm) => {
                let d = pm.resolve_cloud_approval(tool, path, self.quiet);
                if !self.quiet {
                    // Cloud approval prompt emits 2-4 lines to stderr
                    // (header + optional path + prompt + optional confirmation).
                    self.render.lines_written += if path.is_some_and(|p| !p.is_empty()) {
                        4
                    } else {
                        3
                    };
                    self.render.col = 0;
                }
                d
            }
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
            && !self.suppress_intermediate_output
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

/// Don't show a spinner for very short pauses; they feel like visual noise.
const SPINNER_SHOW_DELAY_MS: u64 = 350;

/// Skip `● Thought for …` when thinking was shorter than this (reduces stderr churn).
const MIN_THOUGHT_DURATION_LOG_SECS: f64 = 1.5;

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
            std::thread::sleep(std::time::Duration::from_millis(SPINNER_SHOW_DELAY_MS));
            if stop2.load(Ordering::Relaxed) {
                return;
            }
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
        // Clear the spinner line (match real terminal width — hard-coded 80 left ghost chars on wide terms)
        let w = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(80)
            .clamp(20, 512);
        eprint!("\r{}\r", " ".repeat(w));
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
    /// Lines written to the terminal during streaming (stdout + stderr).
    /// Used by the re-render pass to clear all streamed output.
    pub(super) lines_written: usize,
    /// Current column position for wrap tracking.
    col: usize,
    /// Terminal width for wrap calculation.
    term_width: usize,
    /// Incremental markdown renderer — `None` when `render_md` is false.
    md: Option<super::streaming_md::StreamingMarkdown>,
    /// Stderr lines written between tool calls (thinking duration, tool notices).
    #[allow(dead_code)]
    stderr_lines: usize,
    /// Tool status region for in-place updates (⚡ running → ✓ done).
    tool_region: super::terminal_region::TerminalRegion,
    /// Tool status lines (one per tool).
    tool_lines: Vec<String>,
}

impl StreamRenderState {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_term_width(80, false)
    }

    fn with_term_width(tw: usize, render_md: bool) -> Self {
        let w = tw.max(1);
        Self {
            thinking_start: None,
            thinking_spinner: None,
            lines_written: 0,
            col: 0,
            term_width: w,
            md: if render_md {
                Some(super::streaming_md::StreamingMarkdown::new(w))
            } else {
                None
            },
            stderr_lines: 0,
            tool_region: super::terminal_region::TerminalRegion::new(),
            tool_lines: Vec::new(),
        }
    }

    /// Account for text written to the terminal (stdout or stderr).
    pub(super) fn track_output(&mut self, text: &str) {
        for ch in text.chars() {
            if ch == '\n' {
                self.lines_written += 1;
                self.col = 0;
            } else {
                self.col += 1;
                if self.col >= self.term_width {
                    self.lines_written += 1;
                    self.col = 0;
                }
            }
        }
    }

    /// Account for a full line written via eprintln! (adds 1 line).
    #[allow(dead_code)]
    pub(super) fn track_eprintln(&mut self) {
        self.stderr_lines += 1;
        if self.md.is_none() {
            self.lines_written += 1;
        }
        self.col = 0;
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
                if elapsed >= MIN_THOUGHT_DURATION_LOG_SECS && self.md.is_none() {
                    // In markdown streaming mode, injecting an unmanaged line here
                    // shifts the cursor below the tracked markdown regions, which
                    // causes partial clears/residual text on the next tool turn.
                    // Keep per-round thought timing out of the normal markdown UX.
                    let line = edge_sse_thought_duration_line(elapsed);
                    println!("{}", line.dim());
                    let _ = io::stdout().flush();
                    self.lines_written += 1;
                    self.col = 0;
                }
            }
        }
    }

    /// Show a tool as "running" with optional argument preview.
    fn tool_start(&mut self, tool: &str, args: &Value) -> usize {
        let idx = self.tool_lines.len();
        let arg_preview = self.format_tool_arg_preview(tool, args);
        if self.md.is_some() {
            eprintln!("  ⚡ {tool} …");
            if let Some(preview) = &arg_preview {
                eprintln!("  │ {preview}");
            }
            self.stderr_lines += if arg_preview.is_some() { 2 } else { 1 };
            return idx;
        }
        let mut lines_to_add = vec![format!("  ⚡ {tool} …")];
        if let Some(preview) = arg_preview {
            lines_to_add.push(format!("  │ {preview}"));
        }
        for line in lines_to_add {
            self.tool_lines.push(line);
        }
        self.tool_region.update(self.tool_lines.clone());
        idx
    }

    /// Format a preview of tool arguments (single line, max ~60 chars).
    fn format_tool_arg_preview(&self, tool: &str, args: &Value) -> Option<String> {
        match tool {
            "bash" | "shell" => args
                .get("command")
                .and_then(Value::as_str)
                .map(|cmd| truncate_line(cmd, 60)),
            "read_file" | "view_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                match (start, end) {
                    (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                    (Some(s), None) => Some(format!("{path}:{s}-")),
                    _ => Some(truncate_line(path, 60)),
                }
            }
            "write_file" | "create_file" | "edit_file" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "grep" | "search" => args
                .get("pattern")
                .and_then(Value::as_str)
                .map(|p| format!("/{}/", truncate_line(p, 50))),
            "git_log" | "git_show" | "git_diff" => {
                // Show ref/sha if present
                args.get("ref")
                    .or_else(|| args.get("sha"))
                    .or_else(|| args.get("commit"))
                    .and_then(Value::as_str)
                    .map(|s| truncate_line(s, 20))
            }
            _ => None,
        }
    }

    /// Update a tool line to show completion status with optional output summary.
    fn tool_done(&mut self, idx: usize, tool: &str, status: &str, duration_ms: u64, output: &str) {
        let (icon, suffix) = if status == "error" {
            ("✗", format!(" ({duration_ms}ms) error"))
        } else {
            ("✓", format!(" ({duration_ms}ms)"))
        };
        let output_summary = self.format_output_summary(tool, output, status);
        if self.md.is_some() {
            eprintln!("  {icon} {tool}{suffix}");
            if let Some(summary) = &output_summary {
                eprintln!("  └ {summary}");
            }
            self.stderr_lines += if output_summary.is_some() { 2 } else { 1 };
            return;
        }
        if idx < self.tool_lines.len() {
            self.tool_lines[idx] = format!("  {icon} {tool}{suffix}");
            if let Some(summary) = output_summary {
                // Insert summary line after the tool line
                let insert_pos = (idx + 1).min(self.tool_lines.len());
                // Check if there's already a preview line for this tool (from tool_start)
                // If so, replace it; otherwise insert
                if insert_pos < self.tool_lines.len()
                    && self.tool_lines[insert_pos].starts_with("  │")
                {
                    self.tool_lines[insert_pos] = format!("  └ {summary}");
                } else {
                    self.tool_lines.insert(insert_pos, format!("  └ {summary}"));
                }
            }
            self.tool_region.update(self.tool_lines.clone());
        }
    }

    /// Format a summary of tool output (single line, max ~50 chars).
    fn format_output_summary(&self, tool: &str, output: &str, status: &str) -> Option<String> {
        if status == "error" {
            let first_line = output.lines().next().unwrap_or("").trim();
            return Some(truncate_line(first_line, 50));
        }
        let line_count = output.lines().count();
        let byte_size = output.len();
        match tool {
            "bash" | "shell" => {
                if line_count <= 1 && byte_size < 50 {
                    let trimmed = output.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(truncate_line(trimmed, 50))
                    }
                } else {
                    Some(format!("{line_count} lines"))
                }
            }
            "read_file" | "view_file" => Some(format!("{line_count} lines")),
            "git_log" => {
                // Count commits (lines starting with a hash-like pattern)
                let commits = output.lines().filter(|l| !l.trim().is_empty()).count();
                Some(format!("{commits} commits"))
            }
            "git_show" | "git_diff" => {
                let additions = output.lines().filter(|l| l.starts_with('+')).count();
                let deletions = output.lines().filter(|l| l.starts_with('-')).count();
                if additions > 0 || deletions > 0 {
                    Some(format!("+{additions} -{deletions}"))
                } else {
                    Some(format!("{line_count} lines"))
                }
            }
            "grep" | "search" => {
                let matches = output.lines().count();
                Some(format!("{matches} matches"))
            }
            _ => {
                if line_count > 1 {
                    Some(format!("{line_count} lines"))
                } else {
                    None
                }
            }
        }
    }

    /// Clear tool status region (for intermediate turns before next SSE stream).
    #[allow(dead_code)]
    fn clear_tool_region(&mut self) {
        self.tool_region.clear();
        self.tool_lines.clear();
    }
}

/// Truncate a string to max_chars, adding "…" if truncated.
fn truncate_line(s: &str, max_chars: usize) -> String {
    // Take first line only
    let line = s.lines().next().unwrap_or(s);
    if line.chars().count() <= max_chars {
        line.to_string()
    } else {
        let truncated: String = line.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{truncated}…")
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
                if let Some(md) = &mut render.md {
                    md.push(&s);
                } else {
                    print!("{s}");
                    let _ = io::stdout().flush();
                    render.track_output(&s);
                }
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
    suppress_intermediate_output: bool,
    edge: Option<EdgeSseContext<'_>>,
    pre_clear_lines: usize,
) -> TurnResult {
    // Convert reqwest byte stream to runtime's generic chunk type
    let mut byte_stream = Box::pin(
        resp.bytes_stream()
            .map(|r| r.map(|b| b.to_vec()).map_err(|e| e.to_string())),
    );

    // Delegate to runtime's generic SSE consumer with the appropriate host
    let idle = std::time::Duration::from_millis(STREAM_IDLE_TIMEOUT_MS);
    let (sse_result, edge_tool_round, mut md_renderer, lines_written, pending_xml_buffer) =
        if let Some(ctx) = edge {
            let mut host = CliSseStreamHost::from_edge_ctx(
                ctx,
                term_width,
                render_md && !suppress_intermediate_output,
            );
            // pre_clear_lines only applies to non-md fallback path.
            if host.render.md.is_none() {
                host.render.lines_written = pre_clear_lines;
            }
            let (result, _abort) = consume_sse_stream(&mut byte_stream, &mut host, idle).await;
            let lw = host.render.lines_written;
            let md = host.render.md.take();
            let pending = std::mem::take(&mut host.xml_tag_buffer);
            (result, host.edge_tool_round, md, lw, pending)
        } else {
            let mut render = StreamRenderState::with_term_width(
                term_width,
                render_md && !suppress_intermediate_output,
            );
            if render.md.is_none() {
                render.lines_written = pre_clear_lines;
            }
            let mut host = NoopSseStreamHost;
            let (result, _abort) = consume_sse_stream(&mut byte_stream, &mut host, idle).await;
            let lw = render.lines_written;
            let md = render.md.take();
            (result, Vec::new(), md, lw, String::new())
        };

    let mut result = TurnResult {
        core: sse_result.accum,
        ttft_ms: sse_result.ttft_ms,
        edge_tool_round,
    };
    super::streaming_md::strip_xml_tags_inplace(&mut result.full_text);
    super::streaming_md::strip_leading_narration(&mut result.full_text);

    if quiet || suppress_intermediate_output {
        return result;
    }

    // ─── Finalize incremental markdown ───────────────────────────────────
    // Tool turns: discard ALL text (both rendered and buffered) — it's
    // intermediate thinking that will be superseded by subsequent turns.
    // Non-tool turns: the buffered text is the final answer — render it.
    if result.has_tool_calls {
        // Tool turn — discard everything
        if let Some(md) = &mut md_renderer {
            md.discard_and_reset();
        } else if lines_written > 0 && io::stdout().is_terminal() {
            execute!(
                io::stdout(),
                cursor::MoveUp(lines_written as u16),
                cursor::MoveToColumn(0),
                terminal::Clear(terminal::ClearType::FromCursorDown)
            )
            .ok();
        }
        // pending_xml_buffer is discarded implicitly (not rendered)
    } else {
        // Final turn — render any pending buffer, then finalize
        if !pending_xml_buffer.is_empty() {
            let mut buf = pending_xml_buffer;
            super::streaming_md::strip_xml_tags_inplace(&mut buf);
            super::streaming_md::strip_leading_narration(&mut buf);
            if !buf.is_empty() {
                if let Some(md) = &mut md_renderer {
                    md.push(&buf);
                } else {
                    print!("{buf}");
                    let _ = io::stdout().flush();
                }
            }
        }
        if let Some(md) = &mut md_renderer {
            md.finish();
        } else if !result.full_text.is_empty() && !result.full_text.ends_with('\n') {
            println!();
        }
    }

    result
}

/// Whether the re-render pass should print the accumulated text.
///
/// Returns `false` when tool calls are pending — the text is an intermediate
/// draft that would leak into the terminal and never be cleared by subsequent
/// agentic-loop turns.
#[allow(dead_code)]
fn should_rerender_text(has_tool_calls: bool) -> bool {
    !has_tool_calls
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

    // ── Regression: intermediate draft text must not leak ─────────────

    #[test]
    fn rerender_suppressed_when_tool_calls_present() {
        // When the LLM returns text + tool_calls (intermediate turn),
        // the re-render must NOT print the draft text.
        assert!(!should_rerender_text(true));
    }

    #[test]
    fn rerender_allowed_when_no_tool_calls() {
        // Final turn (no tool_calls) — text should be rendered.
        assert!(should_rerender_text(false));
    }

    #[test]
    fn dispatch_turn_complete_with_tool_calls_sets_flag() {
        // Verify that turn_complete with has_tool_calls=true flows through
        // to TurnResult so the re-render gate works end-to-end.
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"draft review text\""),
            sse("turn_complete", ",\"has_tool_calls\":true"),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, true, &mut vec![]);
        assert_eq!(r.full_text, "draft review text");
        assert!(r.has_tool_calls);
        // The gate must suppress re-render for this result.
        assert!(!should_rerender_text(r.has_tool_calls));
    }

    #[test]
    fn dispatch_turn_complete_without_tool_calls_allows_rerender() {
        let mut r = TurnResult::new();
        let mut s = StreamRenderState::new();
        let block = format!(
            "{}{}",
            sse("text_delta", ",\"content\":\"final answer\""),
            sse("turn_complete", ",\"has_tool_calls\":false"),
        );
        dispatch_turn_event_block(&block, &mut r, &mut s, true, &mut vec![]);
        assert_eq!(r.full_text, "final answer");
        assert!(!r.has_tool_calls);
        assert!(should_rerender_text(r.has_tool_calls));
    }

    #[test]
    fn final_text_cleanup_strips_reflect_tags() {
        let mut text = "before\n<reflect>hidden</reflect>\nafter".to_string();
        super::streaming_md::strip_xml_tags_inplace(&mut text);
        assert_eq!(text, "before\nafter");
    }

    #[test]
    fn final_text_cleanup_strips_think_tags() {
        let mut text = "before\n<think>\nlong thinking block\n</think>\nafter".to_string();
        super::streaming_md::strip_xml_tags_inplace(&mut text);
        assert_eq!(text, "before\nafter");
    }

    // ── Line tracking ────────────────────────────────────────────────

    #[test]
    fn track_output_counts_newlines() {
        let mut s = StreamRenderState::new();
        s.track_output("hello\nworld\n");
        assert_eq!(s.lines_written, 2);
    }

    #[test]
    fn track_output_counts_wraps() {
        let mut s = StreamRenderState::with_term_width(10, false);
        // 20 chars = 2 wraps on a 10-col terminal
        s.track_output("12345678901234567890");
        assert_eq!(s.lines_written, 2);
    }

    #[test]
    fn track_eprintln_increments_line() {
        let mut s = StreamRenderState::new();
        s.track_eprintln();
        s.track_eprintln();
        assert_eq!(s.lines_written, 2);
    }

    #[test]
    fn track_mixed_stdout_stderr_lines() {
        let mut s = StreamRenderState::with_term_width(80, false);
        // Simulate: thinking line (stderr) + streamed text (stdout) + tool_request (stderr)
        s.track_eprintln(); // ● Thought for 1.4s
        s.track_output("Let me review the code\n"); // text_delta
        s.track_eprintln(); // ⚡ tool_request: bash
        assert_eq!(s.lines_written, 3);
    }

    // ── Partial tag detection ────────────────────────────────────────

    #[test]
    fn could_become_thinking_tag_matches_known_prefixes() {
        assert!(could_become_thinking_tag("<"));
        assert!(could_become_thinking_tag("</"));
        assert!(could_become_thinking_tag("<t"));
        assert!(could_become_thinking_tag("<th"));
        assert!(could_become_thinking_tag("<thi"));
        assert!(could_become_thinking_tag("<thin"));
        assert!(could_become_thinking_tag("<think"));
        assert!(could_become_thinking_tag("</think"));
        assert!(could_become_thinking_tag("<r"));
        assert!(could_become_thinking_tag("<ref"));
        assert!(could_become_thinking_tag("</reflect"));
    }

    #[test]
    fn could_become_thinking_tag_rejects_other_tags() {
        // HTML tags that aren't thinking tags
        assert!(!could_become_thinking_tag("<co")); // <code>
        assert!(!could_become_thinking_tag("<p"));
        assert!(!could_become_thinking_tag("<div"));
        assert!(!could_become_thinking_tag("<span"));
        assert!(!could_become_thinking_tag("</code"));
        assert!(!could_become_thinking_tag("<a"));
        assert!(!could_become_thinking_tag("<b"));
    }
}
