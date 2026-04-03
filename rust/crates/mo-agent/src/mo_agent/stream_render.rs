use super::*;
use crossterm::style::Stylize;
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
    SseStreamHost, consume_sse_stream_cancellable,
};
use mo_agent_runtime::turn::tool_result_semantics::cloud_tool_result_status_label;
use serde_json::Value;
use std::borrow::Cow;
use std::io::{IsTerminal, Write};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

pub use mo_agent_runtime::turn::chat_turn_sse_dispatch::ChatTurnEdgePending;

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a mo_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: &'a crate::edge_tools::ToolExecutor,
    pub quiet: bool,
    pub suppress_intermediate_output: bool,
    /// Skip `StreamText` effects only (reasoning preview / spinners still run).
    pub hide_streaming_assistant_text: bool,
    /// When hiding assistant text (plan-only), still show `reasoning_delta` in the thinking viewport.
    pub show_reasoning_preview: bool,
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
    hide_streaming_assistant_text: bool,
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
            hide_streaming_assistant_text: ctx.hide_streaming_assistant_text,
            perm_manager: ctx.perm_manager,
            render: StreamRenderState::with_term_width(
                term_width,
                render_md,
                ctx.hide_streaming_assistant_text && !ctx.show_reasoning_preview,
            ),
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
    fn on_before_sse_read_loop(&mut self) {
        if self.quiet || self.suppress_intermediate_output {
            return;
        }
        self.render.start_waiting_for_model();
    }

    fn on_first_sse_frame(&mut self) {
        if self.quiet || self.suppress_intermediate_output {
            return;
        }
        self.render.stop_thinking();
    }

    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        if self.quiet || self.suppress_intermediate_output {
            return;
        }
        let mut i = 0usize;
        while i < effects.len() {
            match &effects[i] {
                SseRenderEffect::StopThinkingSpinner => {
                    // `text_delta` emits Stop then StreamText; in plan-only mode we stream the
                    // assistant body into the reasoning viewport — skipping Stop avoids clearing
                    // the pane on every token.
                    let skip = self.hide_streaming_assistant_text
                        && i + 1 < effects.len()
                        && matches!(&effects[i + 1], SseRenderEffect::StreamText(_));
                    if !skip {
                        self.render.stop_thinking();
                    }
                    i += 1;
                }
                SseRenderEffect::StreamText(s) => {
                    if self.hide_streaming_assistant_text {
                        if !s.is_empty() {
                            self.render.push_thinking_preview_chunk(s);
                        }
                        i += 1;
                        continue;
                    }
                    // When tool_work_detected, buffer text instead of discarding.
                    // It will be rendered at stream completion if it's the final answer.
                    if self.tool_work_detected {
                        self.xml_tag_buffer.push_str(s);
                        i += 1;
                        continue;
                    }
                    self.push_text(s);
                    i += 1;
                }
                SseRenderEffect::StartThinkingSpinner => {
                    self.render.start_thinking();
                    i += 1;
                }
                SseRenderEffect::ThinkingPreviewChunk(s) => {
                    self.render.push_thinking_preview_chunk(s);
                    i += 1;
                }
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
        // `tool_request` does not emit StopThinking; clear the thinking stderr line so it does
        // not fight the running-tool spinner (`\r` on the same fd).
        self.render.stop_thinking();
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
                .tool_done(idx, tool, args, status, duration_ms, &output);
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
        // `resolve_cloud_approval` writes to stderr only. Never bump `lines_written` here:
        // that counter drives stdout `MoveUp` when clearing streamed text before the first
        // tool line; mixing in stderr line counts caused a large blank gap after prompts.
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

/// Poll interval for interruptible delays (allows early exit on stop signal).
const INTERRUPTIBLE_POLL_MS: u64 = 20;

/// Skip `● Thought for …` when thinking was shorter than this (reduces stderr churn).
const MIN_THOUGHT_DURATION_LOG_SECS: f64 = 1.5;

/// Minimum terminal width for clear-line operations.
const MIN_TERM_WIDTH: usize = 20;
/// Maximum terminal width for clear-line operations (avoid huge allocations).
const MAX_TERM_WIDTH: usize = 512;

/// Clear the current stderr line (carriage return + spaces + carriage return).
pub(super) fn clear_stderr_line() {
    let w = crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .clamp(MIN_TERM_WIDTH, MAX_TERM_WIDTH);
    eprint!("\r{}\r", " ".repeat(w));
    let _ = io::stderr().flush();
}

/// Return terminal width, clamped to reasonable bounds.
fn term_width() -> usize {
    crossterm::terminal::size()
        .map(|(c, _)| c as usize)
        .unwrap_or(80)
        .clamp(MIN_TERM_WIDTH, MAX_TERM_WIDTH)
}

/// Sleep for the given duration, but wake early if `stop` becomes true.
/// Returns true if sleep completed normally, false if interrupted early.
fn interruptible_sleep(duration: std::time::Duration, stop: &AtomicBool) -> bool {
    let poll = std::time::Duration::from_millis(INTERRUPTIBLE_POLL_MS);
    let deadline = std::time::Instant::now() + duration;
    while std::time::Instant::now() < deadline {
        if stop.load(Ordering::Relaxed) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        std::thread::sleep(remaining.min(poll));
    }
    !stop.load(Ordering::Relaxed)
}

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
            // Interruptible delay — can wake early if stop is set
            if !interruptible_sleep(
                std::time::Duration::from_millis(SPINNER_SHOW_DELAY_MS),
                &stop2,
            ) {
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
        clear_stderr_line();
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

/// Pre-TTFT wait on stderr: same rhythm as [`PlanAssembleLineSpinner`] chat prep (`Ns` + label + trailing braille).
pub(super) struct TtftWaitLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl TtftWaitLineSpinner {
    pub(super) fn start() -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let t0 = std::time::Instant::now();
        let w = term_width();
        let label = "Waiting for stream";
        // Paint immediately: prep line just cleared and the first SSE byte may take seconds.
        {
            let time_part = format!("{:>3}s", 0u64);
            let frame = SPINNER_FRAMES[0];
            let visible = 2 + time_part.chars().count() + 1 + label.chars().count() + 1 + 1;
            eprint!("\r  ");
            eprint!("{}", time_part.dim());
            eprint!(" {}", label.dim());
            eprint!(" {}", format!("{frame}").yellow());
            if visible < w {
                eprint!("{}", " ".repeat(w - visible));
            }
            let _ = io::stderr().flush();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let tick = std::time::Duration::from_millis(50);
            let mut spin_idx = 1usize;
            while !stop2.load(Ordering::Relaxed) {
                if !interruptible_sleep(tick, &stop2) {
                    return;
                }
                let sec = t0.elapsed().as_secs();
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                let time_part = format!("{:>3}s", sec);
                let visible = 2 + time_part.chars().count() + 1 + label.chars().count() + 1 + 1;
                eprint!("\r  ");
                eprint!("{}", time_part.dim());
                eprint!(" {}", label.dim());
                eprint!(" {}", format!("{frame}").yellow());
                if visible < w {
                    eprint!("{}", " ".repeat(w - visible));
                }
                let _ = io::stderr().flush();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub(super) fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

impl Drop for TtftWaitLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// stderr `\r` status while a tool runs (markdown mode — same layout as TTFT wait line).
struct ToolRunningLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ToolRunningLineSpinner {
    fn start(description: String) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let detail = truncate_cli_status_detail(&description, 48);
        let t0 = Instant::now();
        let w = term_width();
        let label = "Running…";
        {
            let time_part = format!("{:>3}s", 0u64);
            let frame = SPINNER_FRAMES[0];
            let visible = 2
                + time_part.chars().count()
                + 1
                + label.chars().count()
                + 1
                + detail.chars().count()
                + 1
                + 1;
            eprint!("\r  ");
            eprint!("{}", time_part.dim());
            eprint!(" {}", label.dim());
            eprint!(" {}", detail.as_str().dim());
            eprint!(" {}", format!("{frame}").yellow());
            if visible < w {
                eprint!("{}", " ".repeat(w - visible));
            }
            let _ = io::stderr().flush();
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let detail_for_thread = detail.clone();
        let handle = std::thread::spawn(move || {
            let tick = std::time::Duration::from_millis(50);
            let mut spin_idx = 1usize;
            while !stop2.load(Ordering::Relaxed) {
                if !interruptible_sleep(tick, &stop2) {
                    return;
                }
                let sec = t0.elapsed().as_secs();
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                let time_part = format!("{:>3}s", sec);
                let visible = 2
                    + time_part.chars().count()
                    + 1
                    + label.chars().count()
                    + 1
                    + detail_for_thread.chars().count()
                    + 1
                    + 1;
                eprint!("\r  ");
                eprint!("{}", time_part.dim());
                eprint!(" {}", label.dim());
                eprint!(" {}", detail_for_thread.as_str().dim());
                eprint!(" {}", format!("{frame}").yellow());
                if visible < w {
                    eprint!("{}", " ".repeat(w - visible));
                }
                let _ = io::stderr().flush();
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        clear_stderr_line();
    }
}

impl Drop for ToolRunningLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Tool lines drawn with [`TerminalRegion`] (non-markdown CLI).
struct ToolRegionState {
    region: super::terminal_region::TerminalRegion,
    lines: Vec<String>,
}

/// Animates the trailing braille on the current running tool line (stdout).
struct ToolStdoutLineAnim {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ToolStdoutLineAnim {
    fn start(ui: Arc<Mutex<ToolRegionState>>, idx: usize, description: String) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            {
                let mut g = ui.lock().unwrap();
                if idx < g.lines.len() {
                    let frame = SPINNER_FRAMES[0];
                    g.lines[idx] = format!(
                        "  {} {} {}",
                        "⬢".cyan(),
                        description,
                        format!("{frame}").yellow()
                    );
                    let lines = g.lines.clone();
                    g.region.update(lines);
                }
            }
            let mut spin_idx = 1usize;
            while !stop2.load(Ordering::Relaxed) {
                if !interruptible_sleep(std::time::Duration::from_millis(50), &stop2) {
                    return;
                }
                let mut g = ui.lock().unwrap();
                if idx >= g.lines.len() {
                    return;
                }
                let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                spin_idx += 1;
                g.lines[idx] = format!(
                    "  {} {} {}",
                    "⬢".cyan(),
                    description,
                    format!("{frame}").yellow()
                );
                let lines = g.lines.clone();
                g.region.update(lines);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    fn stop_join(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ToolStdoutLineAnim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn truncate_cli_status_detail(s: &str, max_chars: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max_chars {
        return t.to_string();
    }
    format!(
        "{}…",
        t.chars()
            .take(max_chars.saturating_sub(1))
            .collect::<String>()
    )
}

/// Which kind of spinner is shown in the single "thinking" stderr slot.
enum ThinkingSpinnerKind {
    /// Classic prefix+braille spinner (e.g., "  🔧 read_file ⣾").
    Classic(Spinner),
    /// TTFT elapsed line spinner (e.g., "  3s Waiting for stream ⣾").
    TtftWait(TtftWaitLineSpinner),
}

impl ThinkingSpinnerKind {
    fn stop_clear(self) {
        match self {
            Self::Classic(s) => s.stop_clear(),
            Self::TtftWait(s) => s.stop_clear(),
        }
    }
}

/// Shared label for [`PlanAssembleLineSpinner`] while building a normal-chat `/chat/turn` payload.
pub(crate) type ChatPrepPhaseLabel = Arc<RwLock<String>>;

/// Which copy to show on the single-line stderr “seconds” status (plan vs normal chat prep).
#[derive(Clone, Copy)]
pub(crate) enum SecStatusLineKind {
    PlanAssemble,
    /// Normal `/chat/turn`: payload assembly + tool schemas + POST until response headers.
    ChatRequestPrep,
}

/// One stderr line, updated in place with elapsed whole seconds.
/// Used for plan-only assemble and for normal-chat request prep (before SSE read loop).
pub(crate) struct PlanAssembleLineSpinner {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

/// RAII: clears [`PlanAssembleLineSpinner`] when dropped (covers prepare/HTTP errors).
pub(crate) struct ChatTurnPrepLineGuard(Option<PlanAssembleLineSpinner>);

impl ChatTurnPrepLineGuard {
    pub(crate) fn maybe_start(show: bool, phase: Option<ChatPrepPhaseLabel>) -> Self {
        Self(if show {
            Some(PlanAssembleLineSpinner::start_chat_request_prep_line(
                phase.expect("prep phase label when show_prep_line"),
            ))
        } else {
            None
        })
    }
}

impl Drop for ChatTurnPrepLineGuard {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() {
            s.stop_clear();
        }
    }
}

impl PlanAssembleLineSpinner {
    #[allow(dead_code)]
    pub(crate) fn start() -> Self {
        Self::start_with_origin(std::time::Instant::now())
    }

    /// Same as [`Self::start`], but elapsed seconds are measured from `origin` (e.g. plan> session start).
    #[allow(dead_code)]
    pub(crate) fn start_with_origin(origin: std::time::Instant) -> Self {
        Self::start_with_origin_release(origin, None)
    }

    /// Like [`Self::start_with_origin`], but when `line_release` is set and becomes `true`, the thread
    /// clears the status line and exits (so SSE can use `Waiting for model` / reasoning preview).
    pub(crate) fn start_with_origin_release(
        origin: std::time::Instant,
        line_release: Option<Arc<AtomicBool>>,
    ) -> Self {
        Self::start_with_origin_release_kind(
            origin,
            line_release,
            SecStatusLineKind::PlanAssemble,
            None,
        )
    }

    /// Normal chat: payload + HTTP until response object (cleared before SSE / “Waiting for model”).
    /// `phase` is updated by the payload builder so the line shows *what* is running, not only elapsed time.
    pub(crate) fn start_chat_request_prep_line(phase: ChatPrepPhaseLabel) -> Self {
        Self::start_with_origin_release_kind(
            std::time::Instant::now(),
            None,
            SecStatusLineKind::ChatRequestPrep,
            Some(phase),
        )
    }

    fn start_with_origin_release_kind(
        origin: std::time::Instant,
        line_release: Option<Arc<AtomicBool>>,
        kind: SecStatusLineKind,
        chat_prep_phase: Option<ChatPrepPhaseLabel>,
    ) -> Self {
        if !io::stderr().is_terminal() {
            return Self {
                stop: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }
        let stop = Arc::new(AtomicBool::new(false));
        let stop2 = stop.clone();
        let handle = std::thread::spawn(move || {
            let t0 = origin;
            std::thread::sleep(std::time::Duration::from_millis(SPINNER_SHOW_DELAY_MS));
            if stop2.load(Ordering::Relaxed) {
                return;
            }
            let poll_phase =
                matches!(kind, SecStatusLineKind::ChatRequestPrep) && chat_prep_phase.is_some();
            let tick = if line_release.is_some() || poll_phase {
                std::time::Duration::from_millis(50)
            } else {
                std::time::Duration::from_millis(200)
            };
            let w = crossterm::terminal::size()
                .map(|(c, _)| c as usize)
                .unwrap_or(80)
                .clamp(20, 512);
            let mut last_shown_sec: Option<u64> = None;
            let mut spin_idx = 0usize;
            while !stop2.load(Ordering::Relaxed) {
                // `Release` store in `fetch_chat_turn_sse` after successful POST headers.
                if line_release
                    .as_ref()
                    .is_some_and(|r| r.load(Ordering::Acquire))
                {
                    eprint!("\r{}\r", " ".repeat(w));
                    let _ = io::stderr().flush();
                    return;
                }
                let sec = t0.elapsed().as_secs();
                match kind {
                    SecStatusLineKind::PlanAssemble => {
                        if last_shown_sec != Some(sec) {
                            last_shown_sec = Some(sec);
                            let line = format!(
                                "  ⋯ Assembling plan · {:>3}s  (build · network · server · first token)",
                                sec
                            );
                            let visible = line.chars().count();
                            eprint!("\r{}", line);
                            if visible < w {
                                eprint!("{}", " ".repeat(w - visible));
                            }
                            let _ = io::stderr().flush();
                        }
                    }
                    SecStatusLineKind::ChatRequestPrep => {
                        spin_idx += 1;
                        let frame = SPINNER_FRAMES[spin_idx % SPINNER_FRAMES.len()];
                        let phase_raw: String = chat_prep_phase
                            .as_ref()
                            .and_then(|p| p.read().ok())
                            .map(|g| {
                                let t = g.trim();
                                if t.is_empty() {
                                    return "Working…".to_string();
                                }
                                let max = 42usize;
                                if t.chars().count() > max {
                                    format!(
                                        "{}…",
                                        t.chars().take(max.saturating_sub(1)).collect::<String>()
                                    )
                                } else {
                                    t.to_string()
                                }
                            })
                            .unwrap_or_else(|| "Working…".to_string());
                        let time_part = format!("{:>3}s", sec);
                        // Phase + elapsed first; braille animation trails at the end (like "Thinking ⠹").
                        let visible =
                            2 + time_part.chars().count() + 1 + phase_raw.chars().count() + 1 + 1;
                        eprint!("\r  ");
                        eprint!("{}", time_part.dim());
                        eprint!(" {}", phase_raw.dim());
                        eprint!(" {}", format!("{frame}").yellow());
                        if visible < w {
                            eprint!("{}", " ".repeat(w - visible));
                        }
                        let _ = io::stderr().flush();
                        last_shown_sec = Some(sec);
                    }
                }
                std::thread::sleep(tick);
            }
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    pub(crate) fn stop_clear(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let w = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(80)
            .clamp(20, 512);
        eprint!("\r{}\r", " ".repeat(w));
        let _ = io::stderr().flush();
    }
}

impl Drop for PlanAssembleLineSpinner {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

// ─── Reasoning preview (stderr viewport, Cursor-style grow-then-tail) ─────────

/// Max **content** rows for `thinking_delta` / `reasoning_delta` (`0` = spinner only).
/// While under this cap the pane **grows downward** (no blank padding). Past the cap, the top
/// folds away and a `... (N lines hidden above)` header appears above the tail.
fn thinking_viewport_rows() -> usize {
    std::env::var("MO_AGENT_THINKING_VIEWPORT_LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(|n: usize| n.min(24))
        .unwrap_or(6)
}

/// Split one logical line into fixed-width visual rows (UTF-8 safe).
fn wrap_line_to_width(line: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![line.to_string()];
    }
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        let end = (i + w).min(chars.len());
        out.push(chars[i..end].iter().collect());
        i = end;
    }
    out
}

/// Expand buffer into visual rows (preserve newlines; wrap long lines). Works for pretty-printed JSON.
fn buffer_to_visual_lines(buffer: &str, w: usize) -> Vec<String> {
    let mut out = Vec::new();
    if buffer.is_empty() {
        return out;
    }
    // Normalize line endings: \r\n -> \n, standalone \r -> \n
    let normalized = buffer.replace("\r\n", "\n").replace('\r', "\n");
    for raw_line in normalized.split('\n') {
        let line = raw_line.trim_end_matches([' ', '\t']);
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        out.extend(wrap_line_to_width(line, w));
    }
    out
}

/// Redraw a stderr window: grows with content until `body_rows`, then tail + hidden count.
/// Thinking preview pane using TerminalRegion for flicker-free stdout rendering.
///
/// Unlike the old stderr-based implementation, this uses stdout + TerminalRegion
/// so it shares the same cursor coordinate space as StreamingMarkdown. This
/// prevents the cursor desync issues that caused duplicate lines.
struct ThinkingPreviewPane {
    body_rows: usize,
    width: usize,
    buffer: String,
    /// Region for diff-based updates (stdout).
    region: super::terminal_region::TerminalRegion,
}

impl ThinkingPreviewPane {
    fn new(body_rows: usize, width: usize) -> Self {
        Self {
            body_rows: body_rows.max(1),
            width: width.max(20),
            buffer: String::new(),
            region: super::terminal_region::TerminalRegion::new(),
        }
    }

    fn push_chunk(&mut self, chunk: &str) {
        self.buffer.push_str(chunk);
        const CAP: usize = 48 * 1024;
        if self.buffer.len() > CAP {
            let overflow = self.buffer.len() - CAP / 2;
            // Find the next valid UTF-8 char boundary after the overflow point
            // to avoid splitting multi-byte characters.
            let drain_end = self
                .buffer
                .char_indices()
                .map(|(i, _)| i)
                .find(|&i| i >= overflow)
                .unwrap_or(overflow);
            self.buffer.drain(..drain_end);
        }
        self.redraw();
    }

    fn build_frame(&self) -> (String, Vec<String>) {
        let w = self.width.saturating_sub(6).max(12);
        let visual = buffer_to_visual_lines(&self.buffer, w);
        let cap = self.body_rows;
        if visual.is_empty() {
            return (String::new(), Vec::new());
        }
        let hidden = visual.len().saturating_sub(cap);
        let body: Vec<String> = if hidden == 0 {
            visual
        } else {
            visual[visual.len() - cap..].to_vec()
        };
        let header = if hidden > 0 {
            format!("... ({hidden} lines hidden above)")
        } else {
            String::new()
        };
        (header, body)
    }

    /// Redraw using TerminalRegion (stdout) for flicker-free diff-based updates.
    fn redraw(&mut self) {
        use crossterm::style::Stylize;
        let (header, body) = self.build_frame();
        if header.is_empty() && body.is_empty() {
            self.region.update(Vec::new());
            return;
        }
        let mut lines = Vec::with_capacity(body.len() + 1);
        if !header.is_empty() {
            lines.push(format!("  {}", header.dim()));
        }
        for line in body {
            if line.is_empty() {
                lines.push(String::new());
            } else {
                lines.push(format!("  {} {}", "◇".dim(), line.dim()));
            }
        }
        self.region.update(lines);
    }

    fn clear(&mut self) {
        self.region.clear();
        self.buffer.clear();
    }

    /// Return the number of lines currently displayed.
    #[allow(dead_code)]
    fn height(&self) -> usize {
        self.region.height()
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
    /// True while showing the pre-TTFT “waiting for model” spinner (skip thought-duration log).
    waiting_for_first_sse: bool,
    thinking_start: Option<Instant>,
    thinking_spinner: Option<ThinkingSpinnerKind>,
    /// stderr preview for `reasoning_delta`: grows until viewport cap, then tail + hidden count (see `MO_AGENT_THINKING_VIEWPORT_LINES`).
    thinking_pane: Option<ThinkingPreviewPane>,
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
    /// Tool status region + lines (non-markdown); mutex so a worker thread can animate the running line.
    tool_ui: Arc<Mutex<ToolRegionState>>,
    /// stderr `\r` line while a tool runs (markdown streaming UX).
    tool_stderr_running: Option<ToolRunningLineSpinner>,
    /// Braille animation on the current running tool row (non-markdown).
    tool_stdout_anim: Option<ToolStdoutLineAnim>,
    /// When true, do not paint the stderr reasoning viewport (plan-only / hidden assistant text).
    /// Avoids broken in-place redraw when other stderr lines (e.g. project context) were printed first,
    /// and keeps plan decomposition output readable. Reasoning is still accumulated for the API.
    suppress_reasoning_viewport: bool,
}

impl StreamRenderState {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self::with_term_width(80, false, false)
    }

    fn with_term_width(tw: usize, render_md: bool, suppress_reasoning_viewport: bool) -> Self {
        let w = tw.max(1);
        Self {
            waiting_for_first_sse: false,
            thinking_start: None,
            thinking_spinner: None,
            thinking_pane: None,
            lines_written: 0,
            col: 0,
            term_width: w,
            md: if render_md {
                Some(super::streaming_md::StreamingMarkdown::new(w))
            } else {
                None
            },
            stderr_lines: 0,
            tool_ui: Arc::new(Mutex::new(ToolRegionState {
                region: super::terminal_region::TerminalRegion::new(),
                lines: Vec::new(),
            })),
            tool_stderr_running: None,
            tool_stdout_anim: None,
            suppress_reasoning_viewport,
        }
    }

    fn stop_tool_stderr_running(&mut self) {
        if let Some(s) = self.tool_stderr_running.take() {
            s.stop_clear();
        }
    }

    fn stop_tool_stdout_anim(&mut self) {
        if let Some(mut a) = self.tool_stdout_anim.take() {
            a.stop_join();
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

    /// Spinner during HTTP/TTFB before any SSE event is decoded (reuses stderr spinner slot).
    fn start_waiting_for_model(&mut self) {
        if self.thinking_spinner.is_some() || self.thinking_pane.is_some() {
            return;
        }
        if !io::stderr().is_terminal() {
            return;
        }
        self.waiting_for_first_sse = true;
        self.thinking_start.get_or_insert_with(Instant::now);
        self.thinking_spinner = Some(ThinkingSpinnerKind::TtftWait(TtftWaitLineSpinner::start()));
    }

    fn start_thinking(&mut self) {
        if self.thinking_pane.is_some() {
            return;
        }
        self.waiting_for_first_sse = false;
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane now uses stdout (via TerminalRegion), so it works
        // in both markdown and non-markdown modes without cursor conflicts.
        let use_pane = rows > 0 && io::stdout().is_terminal() && !self.suppress_reasoning_viewport;
        if !use_pane && io::stderr().is_terminal() {
            self.thinking_spinner = Some(ThinkingSpinnerKind::Classic(Spinner::start(
                "  Thinking".to_string(),
            )));
        }
    }

    fn push_thinking_preview_chunk(&mut self, chunk: &str) {
        if chunk.is_empty() || self.suppress_reasoning_viewport {
            return;
        }
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane now uses stdout (via TerminalRegion), sharing the
        // same cursor coordinate space as StreamingMarkdown. No more conflicts.
        if rows > 0 && io::stdout().is_terminal() {
            if self.thinking_pane.is_none() {
                self.thinking_pane = Some(ThinkingPreviewPane::new(rows, self.term_width));
            }
            if let Some(pane) = &mut self.thinking_pane {
                pane.push_chunk(chunk);
            }
        }
    }

    fn stop_thinking(&mut self) {
        if let Some(mut pane) = self.thinking_pane.take() {
            pane.clear();
        }
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        let skip_thought_duration_log = self.waiting_for_first_sse;
        self.waiting_for_first_sse = false;
        if let Some(start) = self.thinking_start.take() {
            let elapsed = start.elapsed().as_secs_f64();
            if !skip_thought_duration_log
                && elapsed >= MIN_THOUGHT_DURATION_LOG_SECS
                && self.md.is_none()
            {
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

    /// Show a tool as "running" with Cursor-style description (single line).
    fn tool_start(&mut self, tool: &str, args: &Value) -> usize {
        let description = self.format_tool_description(tool, args);
        // Always clear thinking pane before tool output, regardless of mode.
        // ThinkingPreviewPane uses stdout (TerminalRegion), so any output to
        // stdout or stderr will desync its cursor tracking.
        if let Some(mut pane) = self.thinking_pane.take() {
            pane.clear();
        }
        self.suppress_reasoning_viewport = true;
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            if io::stderr().is_terminal() {
                self.tool_stderr_running = Some(ToolRunningLineSpinner::start(description));
            } else {
                let line = format!("  {} {} …", "⬢".cyan(), description);
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
            return 0;
        }
        self.stop_tool_stdout_anim();
        let idx = {
            let mut g = self.tool_ui.lock().unwrap();
            let idx = g.lines.len();
            let line = format!("  {} {} …", "⬢".cyan(), description);
            g.lines.push(line);
            let lines = g.lines.clone();
            g.region.update(lines);
            idx
        };
        self.tool_stdout_anim = Some(ToolStdoutLineAnim::start(
            self.tool_ui.clone(),
            idx,
            description,
        ));
        idx
    }

    /// Format a Cursor-style tool description: "Grepped pattern in path", "Read file lines X-Y"
    fn format_tool_description(&self, tool: &str, args: &Value) -> String {
        match tool {
            "bash" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, 55))
            }
            "read_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                let short_path = shorten_path(path, 40);
                match (start, end) {
                    (Some(s), Some(e)) => format!("Read {short_path} lines {s}-{e}"),
                    (Some(s), None) => format!("Read {short_path} from line {s}"),
                    _ => format!("Read {short_path}"),
                }
            }
            "write_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Write {}", shorten_path(path, 50))
            }
            "str_replace" | "multi_edit" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Edit {}", shorten_path(path, 50))
            }
            "delete_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Delete {}", shorten_path(path, 50))
            }
            "list_dir" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                format!("List {}", shorten_path(path, 50))
            }
            "grep" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                let glob_filter = args.get("glob").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                let short_pattern = truncate_line(pattern, 25);
                match (glob_filter, path) {
                    (Some(g), _) => format!("Grep \"{short_pattern}\" in {g}"),
                    (None, Some(p)) => {
                        format!("Grep \"{short_pattern}\" in {}", shorten_path(p, 25))
                    }
                    _ => format!("Grep \"{short_pattern}\""),
                }
            }
            "glob" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                format!("Glob {}", truncate_line(pattern, 50))
            }
            "git_status" => "Git status".to_string(),
            "git_log" => {
                let n = args.get("n").and_then(Value::as_u64);
                let branch = args.get("branch").and_then(Value::as_str);
                match (n, branch) {
                    (Some(n), Some(b)) => format!("Git log -{n} {b}"),
                    (Some(n), None) => format!("Git log -{n}"),
                    (None, Some(b)) => format!("Git log {b}"),
                    _ => "Git log".to_string(),
                }
            }
            "git_show" => {
                let commit = args
                    .get("commit")
                    .or_else(|| args.get("ref"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                format!("Git show {}", truncate_line(commit, 12))
            }
            "git_diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                let path = args.get("path").and_then(Value::as_str);
                match (staged, path) {
                    (true, Some(p)) => format!("Git diff --staged {}", shorten_path(p, 35)),
                    (true, None) => "Git diff --staged".to_string(),
                    (false, Some(p)) => format!("Git diff {}", shorten_path(p, 40)),
                    _ => "Git diff".to_string(),
                }
            }
            "git_blame" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Git blame {}", shorten_path(path, 45))
            }
            "git_commit" => {
                let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
                format!("Git commit \"{}\"", truncate_line(msg, 40))
            }
            "find_definition" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Find definition of {}", truncate_line(symbol, 35))
            }
            "find_references" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Find references to {}", truncate_line(symbol, 35))
            }
            "symbol_search" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Search symbol {}", truncate_line(symbol, 40))
            }
            "symbols" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Get symbols in {}", shorten_path(path, 40))
            }
            "call_graph" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Call graph for {}", truncate_line(symbol, 40))
            }
            "run_build_test" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, 55))
            }
            "web_fetch" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                format!("Fetch {}", truncate_line(url, 50))
            }
            "github_get_pr" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                let num = args.get("pr_number").and_then(Value::as_u64).unwrap_or(0);
                format!("Get PR {owner}/{repo}#{num}")
            }
            "github_list_prs" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                format!("List PRs in {owner}/{repo}")
            }
            // Memory tools with natural verbs
            "memory_retrieve" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!("Recall \"{}\"", truncate_line(query, 45))
            }
            "memory_store" => {
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                format!("Remember \"{}\"", truncate_line(content, 40))
            }
            "memory_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!("Search memory \"{}\"", truncate_line(query, 40))
            }
            "memory_purge" => "Forget memory".to_string(),
            "memory_correct" => "Update memory".to_string(),
            "memory_profile" => "Check profile".to_string(),
            _ => tool.to_string(),
        }
    }

    /// Shorten a path by keeping the last N chars with leading "..."
    fn _format_tool_arg_preview_unused(&self, tool: &str, args: &Value) -> Option<String> {
        match tool {
            "bash" => args
                .get("command")
                .and_then(Value::as_str)
                .map(|cmd| truncate_line(cmd, 60)),
            "read_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                match (start, end) {
                    (Some(s), Some(e)) => Some(format!("{path}:{s}-{e}")),
                    (Some(s), None) => Some(format!("{path}:{s}-")),
                    _ => Some(truncate_line(path, 60)),
                }
            }
            "write_file" | "delete_file" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "str_replace" | "multi_edit" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "list_dir" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                let depth = args.get("depth").and_then(Value::as_u64);
                match depth {
                    Some(d) => Some(format!("{} (depth {})", truncate_line(path, 50), d)),
                    None => Some(truncate_line(path, 60)),
                }
            }
            "grep" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                let glob_filter = args.get("glob").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                let mut preview = format!("/{}/", truncate_line(pattern, 30));
                if let Some(g) = glob_filter {
                    preview.push_str(&format!(" {}", truncate_line(g, 20)));
                } else if let Some(p) = path {
                    preview.push_str(&format!(" in {}", truncate_line(p, 20)));
                }
                Some(preview)
            }
            "glob" => args
                .get("pattern")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "git_log" => {
                let n = args.get("n").and_then(Value::as_u64);
                let branch = args.get("branch").and_then(Value::as_str);
                match (n, branch) {
                    (Some(n), Some(b)) => Some(format!("-{n} {b}")),
                    (Some(n), None) => Some(format!("-{n}")),
                    (None, Some(b)) => Some(truncate_line(b, 20)),
                    _ => None,
                }
            }
            "git_show" | "git_blame" | "git_file_history" => args
                .get("commit")
                .or_else(|| args.get("ref"))
                .or_else(|| args.get("path"))
                .and_then(Value::as_str)
                .map(|s| truncate_line(s, 20)),
            "git_diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                let path = args.get("path").and_then(Value::as_str);
                match (staged, path) {
                    (true, Some(p)) => Some(format!("--staged {}", truncate_line(p, 45))),
                    (true, None) => Some("--staged".to_string()),
                    (false, Some(p)) => Some(truncate_line(p, 60)),
                    _ => None,
                }
            }
            "git_commit" => args
                .get("message")
                .and_then(Value::as_str)
                .map(|m| format!("-m \"{}\"", truncate_line(m, 50))),
            "git_stash" => args
                .get("action")
                .and_then(Value::as_str)
                .map(|a| a.to_string()),
            "find_definition" | "find_references" | "symbol_search" | "hover_info" => args
                .get("symbol")
                .and_then(Value::as_str)
                .map(|s| truncate_line(s, 40)),
            "call_graph" | "type_hierarchy" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                let depth = args.get("depth").and_then(Value::as_u64);
                match depth {
                    Some(d) => Some(format!("{} (depth {})", truncate_line(symbol, 40), d)),
                    None => Some(truncate_line(symbol, 50)),
                }
            }
            "symbols" => args
                .get("path")
                .and_then(Value::as_str)
                .map(|p| truncate_line(p, 60)),
            "run_build_test" => args
                .get("command")
                .and_then(Value::as_str)
                .map(|c| truncate_line(c, 60)),
            "web_fetch" => args
                .get("url")
                .and_then(Value::as_str)
                .map(|u| truncate_line(u, 60)),
            "github_get_pr" | "github_get_issue" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                let number = args
                    .get("number")
                    .or_else(|| args.get("pr_number"))
                    .or_else(|| args.get("issue_number"))
                    .and_then(Value::as_u64);
                match number {
                    Some(n) => Some(format!("{owner}/{repo}#{n}")),
                    None => Some(format!("{owner}/{repo}")),
                }
            }
            "github_list_prs" | "github_list_issues" | "github_repo_stats" | "github_ci_status" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                Some(format!("{owner}/{repo}"))
            }
            "mo_query" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|q| truncate_line(q, 60)),
            "mo_snapshot" | "mo_branch" => args
                .get("action")
                .and_then(Value::as_str)
                .map(|a| a.to_string()),
            "memory_retrieve" | "memory_search" => args
                .get("query")
                .and_then(Value::as_str)
                .map(|q| truncate_line(q, 50)),
            "memory_store" => args
                .get("content")
                .and_then(Value::as_str)
                .map(|c| truncate_line(c, 50)),
            _ => None,
        }
    }

    /// Update a tool line to show completion status with Cursor-style summary.
    fn tool_done(
        &mut self,
        idx: usize,
        tool: &str,
        args: &Value,
        status: &str,
        duration_ms: u64,
        output: &str,
    ) {
        let output_summary = self.format_output_summary(tool, output, status);
        let duration_suffix = format_duration_suffix(duration_ms);
        // Cursor-style format: original description with result appended
        let (icon, line) = if status == "error" {
            let err_msg = output_summary.unwrap_or_else(|| "failed".to_string());
            (format!("{}", "✗".red()), format!("    {}", err_msg.red()))
        } else {
            let summary_line = match output_summary {
                Some(summary) => format!("    {}", summary.dim()),
                None => String::new(),
            };
            (format!("{}", "⬢".green()), summary_line)
        };
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            // Keep the same tool label as the in-flight spinner (`Git show …`, `Git diff …`) so
            // stderr is not only anonymous +/- counts and paths.
            let description = self.format_tool_description(tool, args);
            let dur_display = if duration_suffix.is_empty() {
                String::new()
            } else {
                format!("{}", duration_suffix.dim())
            };
            let mut out_lines = 1usize;
            if status == "error" {
                eprintln!("  {} {}{}", "✗".red(), description, dur_display);
            } else {
                eprintln!("  {} {}{}", "⬢".green(), description, dur_display);
            }
            if !line.is_empty() {
                eprintln!("{line}");
                out_lines = out_lines.saturating_add(line.matches('\n').count() + 1);
            }
            self.stderr_lines = self.stderr_lines.saturating_add(out_lines);
            return;
        }
        self.stop_tool_stdout_anim();
        let description = self.format_tool_description(tool, args);
        let mut g = self.tool_ui.lock().unwrap();
        if idx < g.lines.len() {
            if status != "error" {
                // Rebuild the row from tool+args so we drop any trailing braille from the animator.
                let base_row = format!("  {} {}", "⬢".cyan(), description);
                let base = base_row.replacen(&format!("{}", "⬢".cyan()), &icon, 1);
                let dur = if duration_suffix.is_empty() {
                    String::new()
                } else {
                    format!("{}", duration_suffix.dim())
                };
                g.lines[idx] = format!("{base}{dur}");
                if !line.is_empty() {
                    let insert_pos = idx + 1;
                    if insert_pos <= g.lines.len() {
                        g.lines.insert(insert_pos, line);
                    }
                }
            } else {
                // Error: replace icon with ✗, keep description
                let base_row = format!("  {} {}", "⬢".cyan(), description);
                let base = base_row.replacen(&format!("{}", "⬢".cyan()), &icon, 1);
                g.lines[idx] = base;
                if !line.is_empty() {
                    let insert_pos = idx + 1;
                    if insert_pos <= g.lines.len() {
                        g.lines.insert(insert_pos, line);
                    }
                }
            }
            let lines = g.lines.clone();
            g.region.update(lines);
        }
    }

    /// Format a detailed summary of tool output.
    /// Returns multiple lines for richer context — Cursor-style.
    fn format_output_summary(&self, tool: &str, output: &str, status: &str) -> Option<String> {
        if status == "error" {
            let first_line = output.lines().next().unwrap_or("").trim();
            return Some(truncate_line(first_line, 60));
        }
        let line_count = output.lines().count();
        let byte_size = output.len();
        match tool {
            "bash" | "shell" | "run_build_test" => {
                if output.trim().is_empty() {
                    return None;
                }
                // Show first few meaningful lines of output
                let meaningful: Vec<&str> = output
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .take(3)
                    .collect();
                if meaningful.is_empty() {
                    return None;
                }
                let mut parts: Vec<String> =
                    meaningful.iter().map(|l| truncate_line(l, 60)).collect();
                let remaining = line_count.saturating_sub(3);
                if remaining > 0 {
                    parts.push(format!("… +{remaining} more lines"));
                }
                Some(parts.join("\n    "))
            }
            "read_file" | "view_file" => Some(format!(
                "{line_count} lines, {}",
                format_byte_size(byte_size)
            )),
            "git_log" => {
                // Show first few commit summaries
                let commits: Vec<&str> = output
                    .lines()
                    .filter(|l| !l.trim().is_empty())
                    .take(3)
                    .collect();
                let total = output.lines().filter(|l| !l.trim().is_empty()).count();
                let mut parts: Vec<String> = commits.iter().map(|l| truncate_line(l, 60)).collect();
                let remaining = total.saturating_sub(3);
                if remaining > 0 {
                    parts.push(format!("… +{remaining} more"));
                }
                Some(parts.join("\n    "))
            }
            "git_show" | "git_diff" => {
                // Ignore diff file headers (`+++ b/…`, `--- a/…`) so counts match real hunks.
                let additions = output
                    .lines()
                    .filter(|l| l.starts_with('+') && !l.starts_with("+++"))
                    .count();
                let deletions = output
                    .lines()
                    .filter(|l| l.starts_with('-') && !l.starts_with("---"))
                    .count();
                // Extract changed file names only from +++ b/ lines (not diff --git headers)
                let files: Vec<&str> = output
                    .lines()
                    .filter_map(|l| l.strip_prefix("+++ b/"))
                    .filter(|f| !f.is_empty() && *f != "/dev/null")
                    .take(5)
                    .collect();
                let total_files = output
                    .lines()
                    .filter(|l| l.starts_with("+++ b/") && !l.contains("/dev/null"))
                    .count();
                let stat = if additions > 0 || deletions > 0 {
                    format!(
                        "{} {}",
                        format!("+{additions}").green(),
                        format!("-{deletions}").red()
                    )
                } else {
                    format!("{line_count} lines")
                };
                if files.is_empty() {
                    Some(stat)
                } else {
                    let mut summary = format!("{stat} in {total_files} file(s)");
                    for f in &files {
                        summary.push_str(&format!("\n      {}", shorten_path(f, 50)));
                    }
                    let remaining = total_files.saturating_sub(5);
                    if remaining > 0 {
                        summary.push_str(&format!("\n      … +{remaining} more"));
                    }
                    Some(summary)
                }
            }
            "grep" | "search" => {
                let match_lines: Vec<&str> = output.lines().collect();
                let total = match_lines.len();
                // Extract unique file names from grep output (file:line:content format)
                let mut files: Vec<&str> = Vec::new();
                for line in match_lines.iter().take(50) {
                    if let Some(colon_pos) = line.find(':') {
                        let file = &line[..colon_pos];
                        if !files.contains(&file) {
                            files.push(file);
                        }
                    }
                }
                if files.is_empty() {
                    Some(format!("{total} matches"))
                } else {
                    let file_count = files.len();
                    let shown: Vec<String> =
                        files.iter().take(3).map(|f| shorten_path(f, 45)).collect();
                    let mut summary = format!("{total} matches in {file_count} file(s)");
                    for f in &shown {
                        summary.push_str(&format!("\n      {f}"));
                    }
                    let remaining = file_count.saturating_sub(3);
                    if remaining > 0 {
                        summary.push_str(&format!("\n      … +{remaining} more files"));
                    }
                    Some(summary)
                }
            }
            "write_file" | "str_replace" | "multi_edit" | "delete_file" => {
                if tool == "delete_file" {
                    return Some("deleted".to_string());
                }
                // str_replace: sentinel-wrapped diff; write_file: JSON `_cli_unified_diff` (same as headless preview).
                let diff_block = extract_cli_diff_block(output);
                if let Some(ref diff) = diff_block {
                    let colored = colorize_diff_summary(diff.as_ref(), 5);
                    if !colored.is_empty() {
                        return Some(colored);
                    }
                }
                // Fallback: check if output itself looks like a diff
                if output
                    .lines()
                    .any(|l| l.starts_with("+++ ") || l.starts_with("--- "))
                {
                    let colored = colorize_diff_summary(output, 5);
                    if !colored.is_empty() {
                        return Some(colored);
                    }
                }
                if tool == "write_file"
                    && let Ok(v) = serde_json::from_str::<Value>(output.trim())
                    && v.get("success").and_then(|s| s.as_bool()) == Some(true)
                {
                    let bytes =
                        v.get("bytes_written").and_then(|b| b.as_u64()).unwrap_or(0) as usize;
                    return Some(format!("{} written", format_byte_size(bytes)));
                }
                if output.trim().is_empty() {
                    Some("done".to_string())
                } else {
                    Some(truncate_line(output.trim(), 60))
                }
            }
            "list_dir" => {
                let entries = output.lines().filter(|l| !l.trim().is_empty()).count();
                Some(format!("{entries} entries"))
            }
            "glob" => {
                let files: Vec<&str> = output.lines().filter(|l| !l.trim().is_empty()).collect();
                let total = files.len();
                if total == 0 {
                    Some("no matches".to_string())
                } else {
                    let shown: Vec<String> = files
                        .iter()
                        .take(3)
                        .map(|f| shorten_path(f.trim(), 50))
                        .collect();
                    let mut summary = format!("{total} file(s)");
                    for f in &shown {
                        summary.push_str(&format!("\n      {f}"));
                    }
                    let remaining = total.saturating_sub(3);
                    if remaining > 0 {
                        summary.push_str(&format!("\n      … +{remaining} more"));
                    }
                    Some(summary)
                }
            }
            _ => {
                if line_count > 1 {
                    Some(format!("{line_count} lines"))
                } else if output.trim().is_empty() {
                    None
                } else {
                    Some(truncate_line(output.trim(), 60))
                }
            }
        }
    }

    /// Clear tool status region (for intermediate turns before next SSE stream).
    #[allow(dead_code)]
    fn clear_tool_region(&mut self) {
        self.stop_tool_stdout_anim();
        let mut g = self.tool_ui.lock().unwrap();
        g.region.clear();
        g.lines.clear();
    }
}

/// Unified diff for CLI summaries: `str_replace` / `multi_edit` sentinels, or `write_file` JSON field.
fn extract_cli_diff_block(output: &str) -> Option<Cow<'_, str>> {
    let start_marker = "<<<MO_AGENT_UNIFIED_DIFF>>>";
    let end_marker = "<<<END_MO_AGENT_UNIFIED_DIFF>>>";
    if let Some(start) = output.find(start_marker) {
        let after = &output[start + start_marker.len()..];
        let end = after.find(end_marker).unwrap_or(after.len());
        let block = after[..end].trim();
        if !block.is_empty() {
            return Some(Cow::Borrowed(block));
        }
    }
    let v = serde_json::from_str::<Value>(output.trim()).ok()?;
    let diff = v.get("_cli_unified_diff")?.as_str()?;
    if diff.is_empty() {
        return None;
    }
    Some(Cow::Owned(diff.to_string()))
}

/// Colorize a unified diff into a compact summary with green +lines and red -lines.
fn colorize_diff_summary(diff: &str, max_lines: usize) -> String {
    let mut parts = Vec::new();
    let mut shown = 0usize;
    let mut total_add = 0usize;
    let mut total_del = 0usize;
    for line in diff.lines() {
        if line.starts_with('+') && !line.starts_with("+++ ") {
            total_add += 1;
            if shown < max_lines {
                parts.push(format!("{}", truncate_line(line, 60).green()));
                shown += 1;
            }
        } else if line.starts_with('-') && !line.starts_with("--- ") {
            total_del += 1;
            if shown < max_lines {
                parts.push(format!("{}", truncate_line(line, 60).red()));
                shown += 1;
            }
        }
    }
    let remaining = (total_add + total_del).saturating_sub(max_lines);
    if remaining > 0 {
        parts.push(format!(
            "… {} {} (+{total_add} -{total_del} total)",
            format!("+{remaining}").dim(),
            "more".dim(),
        ));
    }
    if parts.is_empty() {
        return String::new();
    }
    parts.join("\n    ")
}

/// Format byte size as human-friendly string.
fn format_byte_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Format duration as a human-friendly suffix for the tool description line.
/// Only shown for durations ≥ 1s. Returns e.g. " 3.2s", " 1m 4s", " 12m 30s".
fn format_duration_suffix(ms: u64) -> String {
    if ms < 1_000 {
        return String::new();
    }
    let secs = ms / 1_000;
    if secs < 60 {
        let frac = (ms % 1_000) / 100;
        if frac > 0 {
            format!(" {secs}.{frac}s")
        } else {
            format!(" {secs}s")
        }
    } else {
        let m = secs / 60;
        let s = secs % 60;
        if s > 0 {
            format!(" {m}m {s}s")
        } else {
            format!(" {m}m")
        }
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

/// Shorten a path by keeping the filename and truncating dir prefix with "...".
fn shorten_path(path: &str, max_chars: usize) -> String {
    if path.chars().count() <= max_chars {
        return path.to_string();
    }
    // Keep the filename (last component)
    let parts: Vec<&str> = path.split('/').collect();
    if parts.is_empty() {
        return truncate_line(path, max_chars);
    }
    let filename = parts.last().unwrap_or(&"");
    if filename.chars().count() >= max_chars.saturating_sub(4) {
        // Filename itself is too long, just truncate
        return truncate_line(filename, max_chars);
    }
    // Try to keep one parent dir
    if parts.len() >= 2 {
        let parent = parts[parts.len() - 2];
        let short = format!(".../{parent}/{filename}");
        if short.chars().count() <= max_chars {
            return short;
        }
    }
    format!(".../{filename}")
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
            SseRenderEffect::ThinkingPreviewChunk(s) => render.push_thinking_preview_chunk(&s),
        }
    }
}

/// Consume one /chat/turn SSE stream, render text deltas, collect tool_calls.
///
/// Delegates protocol parsing to runtime's [`consume_sse_stream_cancellable`]; CLI-specific
/// rendering, tool execution, and approval prompts are handled by [`CliSseStreamHost`].
///
/// When `quiet` is true, all terminal output is suppressed but result.full_text is still captured.
///
/// If `cancel_token` is provided, the stream can be cancelled mid-flight by triggering the token.
#[allow(clippy::too_many_arguments)]
pub(super) async fn consume_turn_sse(
    prep_line: ChatTurnPrepLineGuard,
    resp: mo_thin_client::HttpResponse,
    render_md: bool,
    term_width: usize,
    quiet: bool,
    suppress_intermediate_output: bool,
    edge: Option<EdgeSseContext<'_>>,
    pre_clear_lines: usize,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> TurnResult {
    // Release the payload/HTTP prep line here so TTFT (`on_before_sse_read_loop`) can take over
    // on the same stderr row without a multi‑hundred‑ms blank gap.
    drop(prep_line);

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
            let (result, _abort) =
                consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token)
                    .await;
            let lw = host.render.lines_written;
            let md = host.render.md.take();
            let pending = std::mem::take(&mut host.xml_tag_buffer);
            (result, host.edge_tool_round, md, lw, pending)
        } else {
            let mut render = StreamRenderState::with_term_width(
                term_width,
                render_md && !suppress_intermediate_output,
                false,
            );
            if render.md.is_none() {
                render.lines_written = pre_clear_lines;
            }
            let mut host = NoopSseStreamHost;
            let (result, _abort) =
                consume_sse_stream_cancellable(&mut byte_stream, &mut host, idle, cancel_token)
                    .await;
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
        let mut s = StreamRenderState::with_term_width(10, false, false);
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
        let mut s = StreamRenderState::with_term_width(80, false, false);
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

    // ── Thinking preview (JSON / reasoning viewport) ───────────────

    #[test]
    fn buffer_to_visual_lines_preserves_pretty_json_structure() {
        let s = "[\n  {\n    \"question\": \"Q?\",\n    \"options\": [\"a\"]\n  }\n]";
        let lines = super::buffer_to_visual_lines(s, 100);
        assert!(lines.iter().any(|l| l.contains('[')));
        assert!(lines.iter().any(|l| l.contains("\"question\"")));
    }

    #[test]
    fn buffer_to_visual_lines_handles_crlf() {
        // Windows-style \r\n should become single newline
        let s = "line1\r\nline2\r\nline3";
        let lines = super::buffer_to_visual_lines(s, 100);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "line1");
        assert_eq!(lines[1], "line2");
        assert_eq!(lines[2], "line3");
    }

    #[test]
    fn buffer_to_visual_lines_handles_mixed_line_endings() {
        // Mix of \r\n, \n, and \r
        let s = "a\r\nb\nc\rd";
        let lines = super::buffer_to_visual_lines(s, 100);
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn wrap_line_to_width_is_utf8_safe() {
        let rows = super::wrap_line_to_width("在/tmp下面", 2);
        assert!(rows.iter().all(|r| r.chars().count() <= 2));
        assert_eq!(rows.join(""), "在/tmp下面");
    }

    #[test]
    fn hidden_line_count_matches_cursor_style_overflow() {
        let visual = super::buffer_to_visual_lines("a\nb\nc\nd\ne", 80);
        assert_eq!(visual.len(), 5);
        let body_cap = 3usize;
        assert_eq!(visual.len().saturating_sub(body_cap), 2);
        assert_eq!(visual[visual.len() - body_cap..], ["c", "d", "e"]);
    }

    #[test]
    fn thinking_preview_pane_no_top_padding_before_cap() {
        let mut p = super::ThinkingPreviewPane::new(4, 80);
        p.buffer = "line1\nline2".into();
        let (h, b) = p.build_frame();
        assert!(h.is_empty(), "no hidden header while under cap");
        assert_eq!(b.len(), 2);
        assert_eq!(b[0], "line1");
        assert_eq!(b[1], "line2");
    }

    #[test]
    fn thinking_preview_pane_tail_and_header_after_cap() {
        let mut p = super::ThinkingPreviewPane::new(2, 80);
        p.buffer = "a\nb\nc\nd".into();
        let (h, b) = p.build_frame();
        assert_eq!(h, "... (2 lines hidden above)");
        assert_eq!(b, vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn colored_string_equality() {
        use crossterm::style::Stylize;
        // Verify that .dim() produces consistent output for comparison
        let s1 = format!("  {} {}", "◇".dim(), "test".dim());
        let s2 = format!("  {} {}", "◇".dim(), "test".dim());
        assert_eq!(s1, s2, "colored strings should be equal");
    }

    #[test]
    fn thinking_pane_buffer_truncation_is_utf8_safe() {
        let mut pane = super::ThinkingPreviewPane::new(3, 80);
        // Create a string with multi-byte UTF-8 characters
        let chinese = "中文测试内容";
        // Repeat enough times to exceed the 48KB cap
        let repeated = chinese.repeat(10000); // ~60KB of UTF-8 content
        pane.push_chunk(&repeated);
        // After truncation, the buffer should still be valid UTF-8
        // (this would panic if we split a multi-byte char)
        assert!(pane.buffer.is_char_boundary(0));
        assert!(pane.buffer.is_char_boundary(pane.buffer.len()));
        // Verify we can iterate chars without panicking
        let _: usize = pane.buffer.chars().count();
    }

    #[test]
    fn extract_cli_diff_from_write_file_json() {
        let diff_body = "--- a/x.js\n+++ b/x.js\n@@ -1,1 +1,1 @@\n-old\n+new\n";
        let out = serde_json::json!({
            "success": true,
            "bytes_written": 3u32,
            "path": "/tmp/x.js",
            "_cli_unified_diff": diff_body,
        })
        .to_string();
        let got = super::extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), diff_body);
    }

    #[test]
    fn extract_cli_diff_sentinel_wrapped() {
        let embedded = "+++ b/f\n+ok\n";
        let out = format!("<<<MO_AGENT_UNIFIED_DIFF>>>{embedded}<<<END_MO_AGENT_UNIFIED_DIFF>>>");
        let got = super::extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), embedded.trim());
    }
}
