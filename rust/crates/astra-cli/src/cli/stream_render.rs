use super::*;
use astra_runtime::turn::chat_turn_sse_dispatch::{
    ChatTurnSseAccum, SseRenderEffect, dispatch_chat_turn_sse_event_block,
};
use astra_runtime::turn::sse_edge_stderr_lines::{
    edge_sse_post_approval_fail_line, edge_sse_post_tool_result_fail_line,
};
use astra_runtime::turn::sse_stream_host::{
    EdgeApprovalResult, EdgeToolExecResult, NoopSseStreamHost, STREAM_IDLE_TIMEOUT_MS,
    SseStreamHost, ToolBatchRequest, consume_sse_stream_cancellable, is_tool_concurrency_safe,
};
use astra_runtime::turn::tool_result_semantics::cloud_tool_result_status_label;
use crossterm::style::Stylize;
use futures_util::StreamExt;
use serde_json::Value;
use std::io::{IsTerminal, Write};
use std::ops::{Deref, DerefMut};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// CLI formatting utilities
use super::cli_formatting::{
    colorize_diff_summary, extract_cli_diff_block, format_byte_size, format_duration_suffix,
    highlight_code_line, shorten_path, truncate_line,
};

// Effects module types
use super::effects::{
    ThinkingSpinnerKind, ToolRegionState, ToolStdoutLineAnim, thinking_viewport_rows,
};

pub use astra_runtime::turn::chat_turn_sse_dispatch::ChatTurnEdgePending;

// Re-export effects types for callers
pub(crate) use super::effects::{ChatPrepPhaseLabel, ChatTurnPrepLineGuard};
pub(super) use super::effects::{
    Spinner, ThinkingPreviewPane, ToolRunningLineSpinner, TtftWaitLineSpinner,
};

/// When set, SSE `tool_request` / `approval_required` are handled and posted to the cloud API.
pub(super) struct EdgeSseContext<'a> {
    pub api: &'a astra_thin_client::ThinClient,
    pub token: &'a str,
    pub executor_id: &'a str,
    pub executor: &'a mut crate::edge_tools::ToolExecutor,
    pub quiet: bool,
    pub suppress_intermediate_output: bool,
    /// Skip `StreamText` effects only (reasoning preview / spinners still run).
    pub hide_streaming_assistant_text: bool,
    /// When hiding assistant text (plan-only), still show `reasoning_delta` in the thinking viewport.
    pub show_reasoning_preview: bool,
    pub perm_manager: Option<&'a mut crate::permission_manager::PermissionManager>,
    /// Optional cancellation token to abort SSE stream on auth failure.
    pub cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Optional channel for forwarding fine-grained stream events.
    pub stream_event_tx: Option<super::chat_stream::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    pub approval_request_tx: Option<super::chat_stream::ApprovalRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls in the SSE stream.
    pub skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
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
/// - Cloud API posting (tool results, approvals) via [`astra_thin_client::ThinClient`]
struct CliSseStreamHost<'a> {
    api: &'a astra_thin_client::ThinClient,
    token: &'a str,
    executor_id: &'a str,
    executor: &'a mut crate::edge_tools::ToolExecutor,
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
    /// Optional cancellation token to abort SSE stream on auth failure.
    cancel_token: Option<&'a tokio_util::sync::CancellationToken>,
    /// Optional channel for forwarding fine-grained stream events.
    stream_event_tx: Option<super::chat_stream::StreamEventTx>,
    /// Optional channel for async tool approval requests during plan execution.
    approval_request_tx: Option<super::chat_stream::ApprovalRequestTx>,
    /// Skill resolver for intercepting "skill" tool calls.
    skill_resolver: Option<std::sync::Arc<dyn astra_runtime::turn::skill_tool::SkillResolver>>,
    // skill_fired_this_turn removed: was causing infinite loops in bridge
    // cloud loop. Server-side skill exclusivity in agentic_loop_host.rs
    // handles this instead.
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
            cancel_token: ctx.cancel_token,
            stream_event_tx: ctx.stream_event_tx,
            approval_request_tx: ctx.approval_request_tx,
            skill_resolver: ctx.skill_resolver,
        }
    }

    /// Push text to the active renderer (markdown or raw stdout).
    fn render_text(&mut self, s: &str) {
        if let Some(pane) = self.render.thinking_pane.take() {
            let summary = pane.summary_line();
            self.render.clear_thinking_with_summary(pane, &summary);
        }
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

/// Extract the first absolute path from a bash command string.
/// Used to determine which directory to expand the sandbox to when the user
/// approves a sandbox-denied bash command.
fn extract_first_absolute_path(command: &str) -> Option<String> {
    for token in command.split_whitespace() {
        if token.starts_with('/') && !token.starts_with("//") && !token.contains('$') {
            // Strip trailing punctuation that might be shell syntax
            let clean = token.trim_end_matches([';', '&', ')']);
            if !clean.is_empty() {
                return Some(clean.to_string());
            }
        }
    }
    None
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
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::WaitingForModel);
        }
        if self.quiet || self.suppress_intermediate_output {
            return;
        }
        self.render.start_waiting_for_model();
    }

    fn on_first_sse_frame(&mut self) {
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::ModelResponding);
        }
        // Don't stop the TTFT spinner here — the first SSE frame is often
        // metadata (session_info, usage) not visible content.  Let the
        // spinner run until actual thinking/text arrives, which will
        // dismiss it via StartThinkingSpinner or StopThinkingSpinner.
    }

    fn on_idle_tick(&mut self) {
        if self.quiet || self.suppress_intermediate_output {
            return;
        }
        self.render.tick_thinking_pane();
    }

    fn on_render_effects(&mut self, effects: Vec<SseRenderEffect>) {
        // Forward to stream event channel (even when quiet/suppress are on)
        if let Some(tx) = &self.stream_event_tx {
            use super::chat_stream::StreamEvent;
            for effect in &effects {
                let ev = match effect {
                    SseRenderEffect::StreamText(s) if !s.is_empty() => {
                        Some(StreamEvent::Token(s.clone()))
                    }
                    SseRenderEffect::StartThinkingSpinner => Some(StreamEvent::Thinking(true)),
                    SseRenderEffect::StopThinkingSpinner => Some(StreamEvent::Thinking(false)),
                    SseRenderEffect::ThinkingPreviewChunk(s) if !s.is_empty() => {
                        Some(StreamEvent::ThinkingChunk(s.clone()))
                    }
                    _ => None,
                };
                if let Some(ev) = ev {
                    let _ = tx.send(ev);
                }
            }
        }

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
                        // Plan decompose mode: don't show the raw JSON body in
                        // the thinking preview.  Only genuine <thinking> content
                        // (via ThinkingPreviewChunk) should appear there.
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
        // Forward tool-started event to observer channel
        let tool_description = self.render.format_tool_description(tool, args);
        if let Some(tx) = &self.stream_event_tx {
            let _ = tx.send(super::chat_stream::StreamEvent::ToolStarted {
                name: tool.to_string(),
                description: tool_description.clone(),
            });
        }

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
            Some(pm) => {
                // Always use non-blocking check to avoid freezing the async SSE consumer.
                match pm.check_nonblocking(tool, args) {
                    crate::permission_manager::PermissionDecision::Allow => true,
                    crate::permission_manager::PermissionDecision::Deny(_reason) => false,
                    crate::permission_manager::PermissionDecision::NeedApproval {
                        tool: t,
                        header,
                        detail,
                        reason,
                    } => {
                        if let Some(tx) = &self.approval_request_tx {
                            // Plan execution: route approval through channel to REPL
                            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                            let _ = tx.send(super::chat_stream::ApprovalRequest {
                                tool: t.clone(),
                                header,
                                detail,
                                reason,
                                response_tx: resp_tx,
                            });
                            let result = if let Some(token) = self.cancel_token {
                                tokio::select! {
                                    biased;
                                    _ = token.cancelled() => false,
                                    r = resp_rx => r.unwrap_or(false),
                                }
                            } else {
                                resp_rx.await.unwrap_or(false)
                            };
                            if result {
                                pm.record_approval(&t, true);
                            }
                            result
                        } else {
                            // Normal interactive mode: prompt on a blocking thread so we
                            // don't freeze the async SSE consumer.  Rustyline is inactive
                            // during turn execution, so the terminal is available.
                            //
                            // Stop the running-tool spinner so the approval prompt is
                            // visible (the spinner continuously overwrites the line).
                            self.render.stop_tool_stderr_running();
                            self.render.stop_tool_stdout_anim();
                            use crossterm::style::Stylize;
                            eprintln!("  {}", format!("⚠  {header}").yellow());
                            if let Some(d) = &detail {
                                eprintln!("{}", d.as_str().dim());
                            }
                            if !reason.is_empty() {
                                eprintln!("  {}", reason.dim());
                            }
                            let ch = tokio::task::spawn_blocking(|| {
                                crate::permission_manager::PermissionManager::prompt_approval(
                                    crate::permission_manager::ApprovalPromptKind::LocalStandard,
                                )
                            })
                            .await
                            .unwrap_or('n');
                            let approved = matches!(ch, 'y' | 'a' | '!');
                            if approved {
                                pm.record_approval(&t, true);
                            }
                            if ch == '!' {
                                pm.set_mode(crate::permission_manager::PermissionMode::Auto);
                                eprintln!(
                                    "  {}",
                                    "  ⚡ Auto-run enabled for this session. Use /allow prompt to restore."
                                    .yellow()
                                );
                            }
                            if ch == 'a' {
                                let rule =
                                    crate::permission_manager::PermissionManager::make_allow_rule(
                                        &t, args,
                                    );
                                pm.add_allow_rule(&rule);
                                let scope = if pm.has_project_root() {
                                    "project"
                                } else {
                                    "session"
                                };
                                eprintln!(
                                    "  {}",
                                    format!("  ✓ {rule}: always allowed ({scope})").dim()
                                );
                            }
                            if ch == 's' {
                                pm.record_approval(&t, false);
                                eprintln!("  {}", format!("  ✗ {t}: skipped for session").dim());
                            }
                            approved
                        }
                    }
                }
            }
            None => true,
        };
        let start = std::time::Instant::now();
        let output = if allowed {
            if tool == astra_runtime::turn::skill_tool::SKILL_TOOL_NAME {
                if let Some(resolver) = &self.skill_resolver {
                    astra_runtime::turn::skill_tool::execute_skill_inline(
                        resolver.as_ref(),
                        tool,
                        args,
                    )
                    .await
                } else {
                    "Error: skill resolver not available".to_string()
                }
            } else if tool == astra_runtime::turn::skill_tool::DISCOVER_SKILLS_TOOL_NAME {
                if let Some(resolver) = &self.skill_resolver {
                    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
                    let catalog = resolver.available_skills();
                    let (text, _) = astra_runtime::turn::skill_tool::execute_discover_skills(
                        query,
                        &catalog,
                        std::collections::HashSet::new(),
                        None,
                    );
                    text
                } else {
                    "Error: skill resolver not available".to_string()
                }
            } else if tool == astra_runtime::turn::agentic_loop_host::DELEGATE_TOOL_NAME {
                // Delegate calls are intercepted at Step 3b of the agentic loop
                // (partition_and_execute_delegations) where the delegation engine
                // runs sub-agents. Return a deferred acknowledgment so the server
                // sees a success (not an error) and the model doesn't give up.
                "Delegation request acknowledged. The delegation engine will execute \
                 this request now, the parent agent will pause while sub-agents \
                 run and aggregate, and the summarized results will be injected \
                 before the parent agent finishes."
                    .to_string()
            } else {
                let result = self.executor.execute(tool, args).await;
                // If the sandbox denied the operation, prompt the user for
                // authorization. On approval, temporarily expand the sandbox
                // boundary and retry the tool.
                if result.starts_with(crate::edge_tools::SANDBOX_DENIED_PREFIX) {
                    if let Some(pm) = &mut self.perm_manager {
                        let sandbox_msg = &result[crate::edge_tools::SANDBOX_DENIED_PREFIX.len()..];
                        let sandbox_tool_key = format!("sandbox_expand:{tool}");
                        let decision = pm.check_nonblocking(
                            &sandbox_tool_key,
                            &serde_json::json!({"reason": sandbox_msg}),
                        );
                        let approved = match decision {
                            crate::permission_manager::PermissionDecision::Allow => true,
                            crate::permission_manager::PermissionDecision::Deny(_) => false,
                            crate::permission_manager::PermissionDecision::NeedApproval {
                                detail,
                                reason,
                                ..
                            } => {
                                if let Some(tx) = &self.approval_request_tx {
                                    // Plan execution mode: route through channel
                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                    let _ = tx.send(super::chat_stream::ApprovalRequest {
                                        tool: sandbox_tool_key.clone(),
                                        header: format!("🔒 {sandbox_msg}"),
                                        detail,
                                        reason,
                                        response_tx: resp_tx,
                                    });
                                    let grant = if let Some(token) = self.cancel_token {
                                        tokio::select! {
                                            biased;
                                            _ = token.cancelled() => false,
                                            r = resp_rx => r.unwrap_or(false),
                                        }
                                    } else {
                                        resp_rx.await.unwrap_or(false)
                                    };
                                    if grant {
                                        pm.record_approval(&sandbox_tool_key, true);
                                    }
                                    grant
                                } else {
                                    // Interactive mode: prompt directly
                                    self.render.stop_tool_stderr_running();
                                    self.render.stop_tool_stdout_anim();
                                    use crossterm::style::Stylize;
                                    eprintln!("  {}", format!("🔒 {sandbox_msg}").yellow());
                                    if let Some(d) = &detail {
                                        eprintln!("{}", d.as_str().dim());
                                    }
                                    if !reason.is_empty() {
                                        eprintln!("  {}", reason.dim());
                                    }
                                    let ch = tokio::task::spawn_blocking(
                                        || {
                                            crate::permission_manager::PermissionManager::prompt_approval(
                                                crate::permission_manager::ApprovalPromptKind::LocalStandard,
                                            )
                                        },
                                    )
                                    .await
                                    .unwrap_or('n');
                                    let grant = matches!(ch, 'y' | 'a' | '!');
                                    if grant {
                                        pm.record_approval(&sandbox_tool_key, true);
                                    }
                                    if ch == '!' {
                                        pm.set_mode(
                                            crate::permission_manager::PermissionMode::Auto,
                                        );
                                        use crossterm::style::Stylize;
                                        eprintln!(
                                            "  {}",
                                            "  ⚡ Auto-run enabled for this session. Use /allow prompt to restore."
                                            .yellow()
                                        );
                                    }
                                    if ch == 's' {
                                        pm.record_approval(&sandbox_tool_key, false);
                                    }
                                    grant
                                }
                            }
                        };
                        if approved {
                            // Temporarily expand sandbox to allow the requested path,
                            // then retry the tool.
                            // Use parent directory so sibling files are also accessible,
                            // but never expand to "/" (would open entire filesystem).
                            let expand_dir = args
                                .get("path")
                                .or_else(|| args.get("file_path"))
                                .and_then(serde_json::Value::as_str)
                                .and_then(|p| {
                                    let parent = std::path::Path::new(p).parent()?;
                                    if parent == std::path::Path::new("/") {
                                        // For root-level files like /passwd, expand to
                                        // the file itself, not "/"
                                        Some(std::path::PathBuf::from(p))
                                    } else {
                                        Some(parent.to_path_buf())
                                    }
                                })
                                .or_else(|| {
                                    args.get("command")
                                        .and_then(serde_json::Value::as_str)
                                        .and_then(extract_first_absolute_path)
                                        .and_then(|p| {
                                            let parent = std::path::Path::new(&p).parent()?;
                                            if parent == std::path::Path::new("/") {
                                                Some(std::path::PathBuf::from(&p))
                                            } else {
                                                Some(parent.to_path_buf())
                                            }
                                        })
                                });
                            if let Some(dir) = expand_dir {
                                self.executor.expand_sandbox_path(dir);
                            }
                            self.executor.execute(tool, args).await
                        } else {
                            result
                        }
                    } else {
                        result
                    }
                } else {
                    result
                }
            }
        } else {
            "Permission denied".to_string()
        };
        let status = if !allowed {
            "error"
        } else {
            cloud_tool_result_status_label(&output)
        };
        let duration_ms = start.elapsed().as_millis() as u64;

        // Forward tool-completed event to observer channel
        if let Some(tx) = &self.stream_event_tx {
            let output_summary = self
                .render
                .format_output_summary(tool, &output, status)
                .unwrap_or_default();
            let tool_description = self.render.format_tool_description(tool, args);
            let _ = tx.send(super::chat_stream::StreamEvent::ToolCompleted {
                name: tool.to_string(),
                description: tool_description,
                status: status.to_string(),
                duration_ms,
                output_summary: if output_summary.is_empty() {
                    None
                } else {
                    Some(output_summary)
                },
            });
        }

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
        let body = astra_thin_client::ToolResultRequest {
            request_id: request_id.to_string(),
            status: status.to_string(),
            output: Some(output),
            duration_ms: Some(duration_ms),
        };
        let post_result = self
            .api
            .post_tool_result(Some(self.token), Some(self.executor_id), &body)
            .await;

        if let Err(ref e) = post_result {
            // Check if this is a 401 Unauthorized - session is invalid, abort SSE stream
            let is_auth_failure = matches!(
                e,
                astra_thin_client::ThinClientError::Api { status, .. }
                    if status.as_u16() == 401
            );

            if is_auth_failure {
                // Cancel the SSE stream immediately - don't wait for idle timeout
                if let Some(token) = self.cancel_token {
                    token.cancel();
                }
                if !self.quiet {
                    eprintln!(
                        "{}",
                        "Session expired. Please re-authenticate with `astra auth login`.".red()
                    );
                }
            } else if !self.quiet && !self.suppress_intermediate_output {
                eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
            }
        }
        self.edge_tool_round
            .last()
            .cloned()
            .unwrap_or_else(|| EdgeToolExecResult {
                request_id: String::new(),
                tool: String::new(),
                args: serde_json::Value::Null,
                output: "Error: no tool result recorded".to_string(),
                status: "error".to_string(),
                duration_ms: 0,
            })
    }

    async fn resolve_approval(
        &mut self,
        request_id: &str,
        tool: &str,
        detail: Option<&str>,
    ) -> EdgeApprovalResult {
        // `resolve_cloud_approval` writes to stderr only. Never bump `lines_written` here:
        // that counter drives stdout `MoveUp` when clearing streamed text before the first
        // tool line; mixing in stderr line counts caused a large blank gap after prompts.
        //
        // Stop spinner/animation before prompting so inquire::Select renders
        // cleanly and doesn't fight the running-tool spinner on stderr.
        self.render.stop_tool_stderr_running();
        self.render.stop_tool_stdout_anim();
        self.render.stop_thinking();
        let decision = match &mut self.perm_manager {
            Some(pm) => {
                pm.resolve_cloud_approval_async(tool, detail, self.quiet)
                    .await
            }
            None => astra_thin_client::ApprovalDecision::Deny,
        };
        let decision_str = match &decision {
            astra_thin_client::ApprovalDecision::Allow => "allow",
            _ => "deny",
        };
        let body = astra_thin_client::ApprovalRespondRequest {
            request_id: request_id.to_string(),
            decision,
            reason: None,
        };
        let post_result = self.api.post_approval(Some(self.token), &body).await;

        if let Err(ref e) = post_result {
            // Check if this is a 401 Unauthorized - session is invalid, abort SSE stream
            let is_auth_failure = matches!(
                e,
                astra_thin_client::ThinClientError::Api { status, .. }
                    if status.as_u16() == 401
            );

            if is_auth_failure {
                // Cancel the SSE stream immediately - don't wait for idle timeout
                if let Some(token) = self.cancel_token {
                    token.cancel();
                }
                if !self.quiet {
                    eprintln!(
                        "{}",
                        "Session expired. Please re-authenticate with `astra auth login`.".red()
                    );
                }
            } else if !self.quiet && !self.suppress_intermediate_output {
                eprintln!("{}", edge_sse_post_approval_fail_line(e).yellow());
            }
        }
        EdgeApprovalResult {
            request_id: request_id.to_string(),
            decision: decision_str.to_string(),
            reason: None,
        }
    }

    /// Parallel batch execution for concurrent-safe tools.
    ///
    /// Sequential (side-effect) tools run first via [`execute_tool`](Self::execute_tool).
    /// Then all concurrent-safe tools execute in parallel via `join_all`, overlapping
    /// network I/O for async tools (GitHub, Memoria, MCP).
    async fn execute_tools_batch(
        &mut self,
        requests: Vec<ToolBatchRequest>,
    ) -> Vec<EdgeToolExecResult> {
        let n = requests.len();

        // Fast path: ≤1 tool — use existing sequential code.
        if n <= 1 {
            let mut out = Vec::with_capacity(n);
            for req in requests {
                out.push(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            return out;
        }

        // Classify by concurrency safety.
        let conc_flags: Vec<bool> = requests
            .iter()
            .map(|req| is_tool_concurrency_safe(&req.tool))
            .collect();
        let conc_count = conc_flags.iter().filter(|&&f| f).count();

        // < 2 concurrent-safe tools: no parallelism benefit.
        if conc_count < 2 {
            let mut out = Vec::with_capacity(n);
            for req in requests {
                out.push(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            return out;
        }

        let mut results: Vec<Option<EdgeToolExecResult>> = (0..n).map(|_| None).collect();

        // Collect concurrent-safe requests (preserving order) and run sequential ones first.
        let mut conc_reqs: Vec<(usize, &ToolBatchRequest)> = Vec::with_capacity(conc_count);
        for (i, req) in requests.iter().enumerate() {
            if conc_flags[i] {
                conc_reqs.push((i, req));
            } else {
                // Side-effect tools execute eagerly in original order.
                results[i] = Some(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
        }

        // Pre-check: can all concurrent tools auto-proceed?
        // Read-only tools hit the fast-path in check_nonblocking (SideEffect::Read → Allow).
        let mut all_allowed = true;
        for (_, req) in &conc_reqs {
            let ok = match &mut self.perm_manager {
                Some(pm) => matches!(
                    pm.check_nonblocking(&req.tool, &req.args),
                    crate::permission_manager::PermissionDecision::Allow
                ),
                None => true,
            };
            if !ok {
                all_allowed = false;
                break;
            }
        }

        if !all_allowed {
            // Rare for read-only tools. Fall back to sequential.
            for (i, req) in conc_reqs {
                results[i] = Some(
                    self.execute_tool(&req.request_id, &req.tool, &req.args)
                        .await,
                );
            }
            return results
                .into_iter()
                .map(|r| r.expect("all tool result slots filled"))
                .collect();
        }

        // ── Phase 1: Pre-execution UI setup (sequential, &mut self) ──
        let mut ui_indices: Vec<Option<usize>> = Vec::with_capacity(conc_reqs.len());
        for (_, req) in &conc_reqs {
            // Forward tool-started event.
            let desc = self.render.format_tool_description(&req.tool, &req.args);
            if let Some(tx) = &self.stream_event_tx {
                let _ = tx.send(super::chat_stream::StreamEvent::ToolStarted {
                    name: req.tool.clone(),
                    description: desc,
                });
            }
            // First-tool clearing (once per turn).
            if !self.tool_work_detected {
                self.tool_work_detected = true;
                self.xml_tag_buffer.clear();
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
            self.render.stop_thinking();
            let tool_idx = if !self.quiet && !self.suppress_intermediate_output {
                Some(self.render.tool_start(&req.tool, &req.args))
            } else {
                None
            };
            ui_indices.push(tool_idx);
        }

        // ── Phase 2: Parallel execution via tokio::spawn ──
        // Each tool runs on a separate runtime thread for true parallelism
        // (sync tools block their thread; async tools yield normally).
        //
        // SAFETY: `ScopedJoinHandles` aborts all spawned tasks on drop,
        // guaranteeing `executor` remains valid for every task's lifetime.
        let executor: &crate::edge_tools::ToolExecutor = &*self.executor;

        /// Wrapper that makes a `*const ToolExecutor` safely `Send`.
        /// Dereferencing requires an `unsafe` call via [`as_ref`](Self::as_ref).
        struct ExecHandle(*const crate::edge_tools::ToolExecutor);
        // SAFETY: ToolExecutor is Sync, so &ToolExecutor is Send.
        // ExecHandle is used only to ferry the pointer into spawned tasks.
        unsafe impl Send for ExecHandle {}
        unsafe impl Sync for ExecHandle {}
        impl ExecHandle {
            /// # Safety
            /// The pointee must still be alive.
            unsafe fn as_ref(&self) -> &crate::edge_tools::ToolExecutor {
                unsafe { &*self.0 }
            }
        }

        struct ScopedJoinHandles(Vec<tokio::task::JoinHandle<(String, u64)>>);
        impl Drop for ScopedJoinHandles {
            fn drop(&mut self) {
                for h in &self.0 {
                    h.abort();
                }
            }
        }

        let handle = ExecHandle(executor as *const _);
        let mut scope = ScopedJoinHandles(Vec::with_capacity(conc_reqs.len()));
        for (_, req) in &conc_reqs {
            let tool = req.tool.clone();
            let args = req.args.clone();
            let h = ExecHandle(handle.0);
            scope.0.push(tokio::spawn(async move {
                // SAFETY: ScopedJoinHandles aborts on drop — pointee is alive.
                let exec = unsafe { h.as_ref() };
                let t0 = Instant::now();
                let output = exec.execute(&tool, &args).await;
                (output, t0.elapsed().as_millis() as u64)
            }));
        }
        let mut outputs: Vec<(String, u64)> = Vec::with_capacity(scope.0.len());
        for jh in std::mem::take(&mut scope.0) {
            match jh.await {
                Ok(result) => outputs.push(result),
                Err(e) => outputs.push((format!("Tool execution panicked: {e}"), 0)),
            }
        }

        // ── Phase 3: Post-execution (sequential, &mut self) ──
        for (pos, (output, duration_ms)) in outputs.into_iter().enumerate() {
            let (orig_idx, req) = conc_reqs[pos];
            let status = cloud_tool_result_status_label(&output);

            // Forward tool-completed event.
            if let Some(tx) = &self.stream_event_tx {
                let output_summary = self
                    .render
                    .format_output_summary(&req.tool, &output, status)
                    .unwrap_or_default();
                let desc = self.render.format_tool_description(&req.tool, &req.args);
                let _ = tx.send(super::chat_stream::StreamEvent::ToolCompleted {
                    name: req.tool.clone(),
                    description: desc,
                    status: status.to_string(),
                    duration_ms,
                    output_summary: if output_summary.is_empty() {
                        None
                    } else {
                        Some(output_summary)
                    },
                });
            }

            // Tool-done UI.
            if let Some(idx) = ui_indices[pos] {
                self.render
                    .tool_done(idx, &req.tool, &req.args, status, duration_ms, &output);
            }

            let result = EdgeToolExecResult {
                request_id: req.request_id.clone(),
                tool: req.tool.clone(),
                args: req.args.clone(),
                output: output.clone(),
                status: status.to_string(),
                duration_ms,
            };
            self.edge_tool_round.push(result.clone());
            results[orig_idx] = Some(result);

            // Post tool result to cloud API.
            let body = astra_thin_client::ToolResultRequest {
                request_id: req.request_id.clone(),
                status: status.to_string(),
                output: Some(output),
                duration_ms: Some(duration_ms),
            };
            let post_result = self
                .api
                .post_tool_result(Some(self.token), Some(self.executor_id), &body)
                .await;
            if let Err(ref e) = post_result {
                let is_auth = matches!(
                    e,
                    astra_thin_client::ThinClientError::Api { status, .. }
                        if status.as_u16() == 401
                );
                if is_auth {
                    if let Some(token) = self.cancel_token {
                        token.cancel();
                    }
                    if !self.quiet {
                        eprintln!(
                            "{}",
                            "Session expired. Please re-authenticate with `astra auth login`."
                                .red()
                        );
                    }
                    break;
                } else if !self.quiet && !self.suppress_intermediate_output {
                    eprintln!("{}", edge_sse_post_tool_result_fail_line(e).yellow());
                }
            }
        }

        results
            .into_iter()
            .map(|r| r.expect("all tool result slots filled"))
            .collect()
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
    /// stderr preview for `reasoning_delta`: grows until viewport cap, then tail + hidden count (see `ASTRA_THINKING_VIEWPORT_LINES`).
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
                "Thinking".to_string(),
            )));
        }
    }

    fn push_thinking_preview_chunk(&mut self, chunk: &str) {
        if chunk.is_empty() || self.suppress_reasoning_viewport {
            return;
        }
        self.thinking_start.get_or_insert_with(Instant::now);
        let rows = thinking_viewport_rows();
        // ThinkingPreviewPane and StreamingMarkdown both use stdout (TerminalRegion).
        // Before updating thinking pane, pause markdown's unstable region to avoid
        // cursor desync between independent regions.
        if rows > 0 && io::stdout().is_terminal() {
            if let Some(md) = &mut self.md {
                md.pause_unstable();
            }
            if self.thinking_pane.is_none() {
                self.thinking_pane = Some(ThinkingPreviewPane::new(rows, self.term_width));
            }
            if let Some(pane) = &mut self.thinking_pane {
                pane.push_chunk(chunk);
            }
        }
    }

    /// Refresh the thinking pane header (elapsed time) without new content.
    fn tick_thinking_pane(&mut self) {
        if let Some(pane) = &mut self.thinking_pane {
            if let Some(md) = &mut self.md {
                md.pause_unstable();
            }
            pane.tick();
        }
    }

    fn stop_thinking(&mut self) {
        let summary = self.thinking_pane.as_ref().map(|pane| pane.summary_line());
        if let Some(mut pane) = self.thinking_pane.take() {
            pane.clear();
        }
        if let Some(spinner) = self.thinking_spinner.take() {
            spinner.stop_clear();
        }
        let skip_thought_duration_log = self.waiting_for_first_sse;
        self.waiting_for_first_sse = false;
        if let Some(_start) = self.thinking_start.take()
            && !skip_thought_duration_log
            && let Some(line) = summary
        {
            if self.md.is_none() {
                println!("{line}");
                let _ = io::stdout().flush();
                self.lines_written += 1;
                self.col = 0;
            } else {
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
        }
    }

    fn clear_thinking_with_summary(&mut self, mut pane: ThinkingPreviewPane, summary: &str) {
        pane.clear();
        if self.md.is_none() {
            println!("{summary}");
            let _ = io::stdout().flush();
            self.lines_written += 1;
            self.col = 0;
        } else {
            eprintln!("{summary}");
            self.stderr_lines += 1;
        }
    }

    /// Show a tool as "running" with Cursor-style description (single line).
    fn tool_start(&mut self, tool: &str, args: &Value) -> usize {
        let description = self.format_tool_description(tool, args);
        let styled_desc = style_tool_description(tool, &description);
        if let Some(pane) = self.thinking_pane.take() {
            let summary = pane.summary_line();
            self.clear_thinking_with_summary(pane, &summary);
        }
        self.suppress_reasoning_viewport = true;
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            if io::stderr().is_terminal() {
                // Spinner uses plain description (truncated internally with .dim())
                self.tool_stderr_running = Some(ToolRunningLineSpinner::start(description));
            } else {
                let line = format!("  {} {} …", "⬢".cyan(), styled_desc);
                eprintln!("{line}");
                self.stderr_lines += 1;
            }
            return 0;
        }
        self.stop_tool_stdout_anim();
        let idx = {
            let mut g = self.tool_ui.lock().unwrap_or_else(|e| e.into_inner());
            let idx = g.lines.len();
            let line = format!("  {} {} …", "⬢".cyan(), styled_desc);
            g.lines.push(line);
            let lines = g.lines.clone();
            g.region.update(lines);
            idx
        };
        self.tool_stdout_anim = Some(ToolStdoutLineAnim::start(
            self.tool_ui.clone(),
            idx,
            description, // Plain text for spinner animation
        ));
        idx
    }

    /// Format a Cursor-style tool description: "Grepped pattern in path", "Read file lines X-Y"
    fn format_tool_description(&self, tool: &str, args: &Value) -> String {
        self.format_tool_description_with_output(tool, args, None)
    }

    /// Format tool description, optionally adjusting based on output.
    /// For read_file, detects auto-expand and adjusts description accordingly.
    fn format_tool_description_with_output(
        &self,
        tool: &str,
        args: &Value,
        output: Option<&str>,
    ) -> String {
        // Dynamic budget based on terminal width.
        // Layout: "  ✓ {description} {duration}" — prefix ~6 chars, duration ~6 chars.
        let term_w = crossterm::terminal::size()
            .map(|(c, _)| c as usize)
            .unwrap_or(80);
        let desc_budget = term_w.saturating_sub(14); // room for prefix + duration
        // Path budget: description budget minus the label prefix (e.g. "Reading: ")
        let path_budget = |prefix_len: usize| desc_budget.saturating_sub(prefix_len).max(20);

        match tool {
            "bash" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, path_budget(2)))
            }
            "read_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                let start = args.get("start_line").and_then(Value::as_u64);
                let end = args.get("end_line").and_then(Value::as_u64);
                let short_path = shorten_path(path, path_budget(10)); // "Reading: "

                // Check if auto-expanded (ranged request but full file returned)
                let auto_expanded = output
                    .map(|o| o.starts_with("[Auto-expanded to full file"))
                    .unwrap_or(false);

                if auto_expanded {
                    format!("Reading: {short_path} (full)")
                } else {
                    match (start, end) {
                        (Some(s), Some(e)) => format!("Reading: {short_path}:{s}-{e}"),
                        (Some(s), None) => format!("Reading: {short_path}:{s}-"),
                        _ => format!("Reading: {short_path}"),
                    }
                }
            }
            "write_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Writing: {}", shorten_path(path, path_budget(9)))
            }
            "str_replace" | "multi_edit" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Editing: {}", shorten_path(path, path_budget(9)))
            }
            "delete_file" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Deleting: {}", shorten_path(path, path_budget(10)))
            }
            "list_dir" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
                format!("Listing: {}", shorten_path(path, path_budget(9)))
            }
            "grep" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                let glob_filter = args.get("glob").and_then(Value::as_str);
                let path = args.get("path").and_then(Value::as_str);
                let pat_budget = desc_budget / 3;
                let short_pattern = truncate_line(pattern, pat_budget);
                match (glob_filter, path) {
                    (Some(g), _) => format!("Grep: \"{short_pattern}\" in {g}"),
                    (None, Some(p)) => {
                        let p_budget = desc_budget.saturating_sub(10 + pat_budget);
                        format!("Grep: \"{short_pattern}\" in {}", shorten_path(p, p_budget))
                    }
                    _ => format!("Grep: \"{short_pattern}\""),
                }
            }
            "glob" => {
                let pattern = args.get("pattern").and_then(Value::as_str).unwrap_or("");
                format!("Glob: {}", truncate_line(pattern, path_budget(6)))
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
                format!("Git show {}", truncate_line(commit, path_budget(9)))
            }
            "git_diff" => {
                let staged = args.get("staged").and_then(Value::as_bool).unwrap_or(false);
                let path = args.get("path").and_then(Value::as_str);
                match (staged, path) {
                    (true, Some(p)) => {
                        format!("Git diff --staged {}", shorten_path(p, path_budget(18)))
                    }
                    (true, None) => "Git diff --staged".to_string(),
                    (false, Some(p)) => {
                        format!("Git diff {}", shorten_path(p, path_budget(10)))
                    }
                    _ => "Git diff".to_string(),
                }
            }
            "git_blame" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Git blame {}", shorten_path(path, path_budget(10)))
            }
            "git_commit" => {
                let msg = args.get("message").and_then(Value::as_str).unwrap_or("");
                format!("Git commit \"{}\"", truncate_line(msg, path_budget(13)))
            }
            "find_definition" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Find definition of {}", truncate_line(symbol, path_budget(20)))
            }
            "find_references" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Find references to {}", truncate_line(symbol, path_budget(19)))
            }
            "symbol_search" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Search symbol {}", truncate_line(symbol, path_budget(15)))
            }
            "symbols" => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                format!("Get symbols in {}", shorten_path(path, path_budget(16)))
            }
            "call_graph" => {
                let symbol = args.get("symbol").and_then(Value::as_str).unwrap_or("");
                format!("Call graph for {}", truncate_line(symbol, path_budget(16)))
            }
            "run_build_test" => {
                let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
                format!("$ {}", truncate_line(cmd, path_budget(2)))
            }
            "web_fetch" => {
                let url = args.get("url").and_then(Value::as_str).unwrap_or("");
                format!("Fetching: {}", truncate_line(url, path_budget(10)))
            }
            "github_get_pr" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                let num = args.get("pr_number").and_then(Value::as_u64).unwrap_or(0);
                format!("Getting PR: {owner}/{repo}#{num}")
            }
            "github_list_prs" => {
                let owner = args.get("owner").and_then(Value::as_str).unwrap_or("");
                let repo = args.get("repo").and_then(Value::as_str).unwrap_or("");
                format!("Listing PRs: {owner}/{repo}")
            }
            // Memory tools with natural verbs
            "memory_retrieve" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!("Recalling: \"{}\"", truncate_line(query, path_budget(13)))
            }
            "memory_store" => {
                let content = args.get("content").and_then(Value::as_str).unwrap_or("");
                format!("Storing: \"{}\"", truncate_line(content, path_budget(11)))
            }
            "memory_search" => {
                let query = args.get("query").and_then(Value::as_str).unwrap_or("");
                format!("Searching memory: \"{}\"", truncate_line(query, path_budget(20)))
            }
            "memory_purge" => "Purging memory".to_string(),
            "memory_correct" => "Correcting memory".to_string(),
            "memory_profile" => "Checking profile".to_string(),
            // Skill tool — show specific skill name
            "skill" => {
                let skill_name = args
                    .get("skill_name")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                format!("Running skill: {}", truncate_line(skill_name, path_budget(16)))
            }
            other if other.starts_with("mcp_") => {
                let rest = &other[4..];
                if let Some(sep) = rest.find('_') {
                    let server = &rest[..sep];
                    let tool_name = &rest[sep + 1..];
                    format!(
                        "MCP {server} {}",
                        truncate_line(tool_name, path_budget(5 + server.len()))
                    )
                } else {
                    format!("MCP {rest}")
                }
            }
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
            (theme::icon_err(), format!("    {}", err_msg.red()))
        } else {
            let summary_line = match output_summary {
                Some(summary) => format!("    {}", summary.dim()),
                None => String::new(),
            };
            (theme::icon_ok(), summary_line)
        };
        if self.md.is_some() {
            self.stop_tool_stderr_running();
            let description = self.format_tool_description_with_output(tool, args, Some(output));
            let styled_desc = style_tool_description(tool, &description);
            let dur_display = format!("{}", duration_suffix.dim());
            let mut out_lines = 1usize;
            if status == "error" {
                eprintln!("  {} {}{}", theme::icon_err(), styled_desc, dur_display);
            } else {
                eprintln!("  {} {}{}", theme::icon_ok(), styled_desc, dur_display);
            }
            if !line.is_empty() {
                eprintln!("{line}");
                out_lines = out_lines.saturating_add(line.matches('\n').count() + 1);
            }
            self.stderr_lines = self.stderr_lines.saturating_add(out_lines);
            return;
        }
        self.stop_tool_stdout_anim();
        let description = self.format_tool_description_with_output(tool, args, Some(output));
        let styled_desc = style_tool_description(tool, &description);
        let dur_display = format!("{}", duration_suffix.dim());
        let mut g = self.tool_ui.lock().unwrap_or_else(|e| e.into_inner());
        if idx < g.lines.len() {
            g.lines[idx] = format!("  {icon} {styled_desc}{dur_display}");
            if !line.is_empty() {
                let insert_pos = idx + 1;
                if insert_pos <= g.lines.len() {
                    g.lines.insert(insert_pos, line);
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
            "read_file" | "view_file" => {
                // Show first few lines of file content (like Cursor/Claude Code)
                // Only skip our metadata lines, not code that happens to start with '['
                let is_metadata = |l: &&str| {
                    l.starts_with("[Auto-expanded")
                        || l.starts_with("[truncated")
                        || l.starts_with("⚠ WARNING:")
                        || l.starts_with("⚠ Note:")
                };

                // Count all non-empty, non-metadata lines for accurate remaining count
                let total_content_lines = output
                    .lines()
                    .filter(|l| !is_metadata(l) && !l.is_empty())
                    .count();

                let content_lines: Vec<&str> = output
                    .lines()
                    .filter(|l| !is_metadata(l) && !l.is_empty())
                    .take(10)
                    .collect();

                if content_lines.is_empty() {
                    return Some(format!(
                        "{line_count} lines, {}",
                        format_byte_size(byte_size)
                    ));
                }

                let mut parts: Vec<String> = content_lines
                    .iter()
                    .map(|l| {
                        let truncated = truncate_line(l, 65);
                        highlight_code_line(&truncated)
                    })
                    .collect();
                let remaining = total_content_lines.saturating_sub(content_lines.len());
                if remaining > 0 {
                    parts.push(format!("{}", format!("… +{remaining} more lines").dim()));
                }
                Some(parts.join("\n    "))
            }
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
                // Show actual match content (first 5 matches with highlighting)
                let preview_lines: Vec<String> = match_lines
                    .iter()
                    .take(5)
                    .map(|line| {
                        // Parse "file:line:content" or "file:content" format
                        let truncated = truncate_line(line, 65);
                        highlight_code_line(&truncated)
                    })
                    .collect();

                if files.is_empty() {
                    Some(format!("{total} matches"))
                } else {
                    let file_count = files.len();
                    let mut summary = format!("{total} matches in {file_count} file(s)");
                    for line in &preview_lines {
                        summary.push_str(&format!("\n    {line}"));
                    }
                    let remaining = total.saturating_sub(5);
                    if remaining > 0 {
                        summary.push_str(&format!(
                            "\n    {}",
                            format!("… +{remaining} more matches").dim()
                        ));
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
                    // Show file paths with dim styling for directory parts
                    let shown: Vec<String> = files
                        .iter()
                        .take(5)
                        .map(|f| {
                            let path = f.trim().trim_end_matches('/'); // Remove trailing slash
                            if let Some(last_slash) = path.rfind('/') {
                                let filename = &path[last_slash + 1..];
                                if filename.is_empty() {
                                    // Path like "/" or unusual case
                                    path.to_string()
                                } else {
                                    format!("{}{}", path[..=last_slash].dim(), filename)
                                }
                            } else {
                                path.to_string()
                            }
                        })
                        .collect();
                    let mut summary = format!("{total} file(s)");
                    for f in &shown {
                        summary.push_str(&format!("\n    {f}"));
                    }
                    let remaining = total.saturating_sub(5);
                    if remaining > 0 {
                        summary
                            .push_str(&format!("\n    {}", format!("… +{remaining} more").dim()));
                    }
                    Some(summary)
                }
            }
            // Skill tool — show first few meaningful output lines
            "skill" => {
                if output.trim().is_empty() {
                    return None;
                }
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
            // MCP tools — show first few output lines (same as bash/skill)
            other if other.starts_with("mcp_") => {
                if output.trim().is_empty() {
                    return None;
                }
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
        let mut g = self.tool_ui.lock().unwrap_or_else(|e| e.into_inner());
        g.region.clear();
        g.lines.clear();
    }
}

/// Apply bold+magenta styling to Skill/MCP tool description prefixes,
/// matching the visual weight of built-in tools like Read/Edit/Write.
pub(crate) fn style_tool_description(tool: &str, description: &str) -> String {
    if tool == "skill" {
        // "Running skill: code-review" → bold magenta "Running skill:" + rest
        if let Some(rest) = description.strip_prefix("Running skill:") {
            return format!("{}{}", "Running skill:".magenta().bold(), rest);
        }
    } else if tool.starts_with("mcp_") {
        // "MCP server tool" → bold magenta "MCP" + rest
        if let Some(rest) = description.strip_prefix("MCP") {
            return format!("{}{}", "MCP".magenta().bold(), rest);
        }
    }
    description.to_string()
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
    resp: astra_thin_client::HttpResponse,
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
    // Check both server-side tool_calls AND edge tools (git, grep, etc.)
    let has_any_tool_work = result.has_tool_calls || !result.edge_tool_round.is_empty();
    if has_any_tool_work {
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
        let block = "data: {\"type\":\"approval_required\",\"request_id\":\"ap-1\",\"tool\":\"write_file\",\"path\":\"src/x.rs\",\"detail\":\"src/x.rs\"}\n\n";
        dispatch_turn_event_block(block, &mut r, &mut s, true, &mut pending);
        assert_eq!(pending.len(), 1);
        match &pending[0] {
            ChatTurnEdgePending::ApprovalRequired {
                request_id,
                tool,
                detail,
            } => {
                assert_eq!(request_id, "ap-1");
                assert_eq!(tool, "write_file");
                assert_eq!(detail.as_deref(), Some("src/x.rs"));
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

    #[test]
    fn colored_string_equality() {
        use crossterm::style::Stylize;
        // Verify that .dim() produces consistent output for comparison
        let s1 = format!("  {} {}", "◇".dim(), "test".dim());
        let s2 = format!("  {} {}", "◇".dim(), "test".dim());
        assert_eq!(s1, s2, "colored strings should be equal");
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
        let out = format!("<<<ASTRA_UNIFIED_DIFF>>>{embedded}<<<END_ASTRA_UNIFIED_DIFF>>>");
        let got = super::extract_cli_diff_block(&out).expect("diff");
        assert_eq!(got.as_ref(), embedded.trim());
    }

    // ── extract_first_absolute_path ─────────────────────────────────────

    #[test]
    fn extract_absolute_path_from_cat_command() {
        assert_eq!(
            extract_first_absolute_path("cat /etc/passwd"),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn extract_absolute_path_skips_relative() {
        assert_eq!(extract_first_absolute_path("cat src/main.rs"), None);
    }

    #[test]
    fn extract_absolute_path_skips_variable() {
        assert_eq!(extract_first_absolute_path("cat $HOME/.bashrc"), None);
    }

    #[test]
    fn extract_absolute_path_skips_unc() {
        assert_eq!(extract_first_absolute_path("cat //server/share"), None);
    }

    #[test]
    fn extract_absolute_path_strips_trailing_semicolon() {
        assert_eq!(
            extract_first_absolute_path("cat /etc/passwd;"),
            Some("/etc/passwd".to_string())
        );
    }

    #[test]
    fn extract_absolute_path_empty_command() {
        assert_eq!(extract_first_absolute_path(""), None);
    }

    // ── style_tool_description tests ──

    #[test]
    fn style_skill_description_has_bold_prefix() {
        let styled = style_tool_description("skill", "Running skill: code-review");
        // Should contain ANSI codes (bold+magenta) and the skill name
        assert!(styled.contains("code-review"));
        assert!(styled.contains("Running skill"));
        // Plain text without ANSI should NOT match (it has escape sequences)
        assert_ne!(styled, "Running skill: code-review");
    }

    #[test]
    fn style_mcp_description_has_bold_prefix() {
        let styled = style_tool_description("mcp_github_search", "MCP github search");
        assert!(styled.contains("search"));
        assert!(styled.contains("MCP"));
        assert_ne!(styled, "MCP github search");
    }

    #[test]
    fn style_regular_tool_unchanged() {
        let styled = style_tool_description("read_file", "Reading: src/main.rs");
        assert_eq!(styled, "Reading: src/main.rs");
    }

    #[test]
    fn style_bash_tool_unchanged() {
        let styled = style_tool_description("bash", "$ echo hello");
        assert_eq!(styled, "$ echo hello");
    }

    // ── Skill/MCP format_tool_description tests ──

    #[test]
    fn format_skill_description() {
        let r = StreamRenderState::new();
        let args = serde_json::json!({"skill_name": "code-review"});
        let desc = r.format_tool_description("skill", &args);
        assert_eq!(desc, "Running skill: code-review");
    }

    #[test]
    fn format_mcp_description_with_server_and_tool() {
        let r = StreamRenderState::new();
        let desc = r.format_tool_description("mcp_github_search_repos", &serde_json::json!({}));
        assert_eq!(desc, "MCP github search_repos");
    }

    #[test]
    fn format_mcp_description_no_underscore() {
        let r = StreamRenderState::new();
        let desc = r.format_tool_description("mcp_mytool", &serde_json::json!({}));
        assert_eq!(desc, "MCP mytool");
    }

    // ── Skill/MCP output summary tests ──

    #[test]
    fn skill_output_summary_shows_lines() {
        let r = StreamRenderState::new();
        let output = "Result line 1\nResult line 2\nResult line 3\nLine 4\nLine 5";
        let summary = r.format_output_summary("skill", output, "ok");
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("Result line 1"));
        assert!(s.contains("Result line 2"));
        assert!(s.contains("… +2 more lines"));
    }

    #[test]
    fn skill_output_summary_empty() {
        let r = StreamRenderState::new();
        assert!(r.format_output_summary("skill", "", "ok").is_none());
        assert!(
            r.format_output_summary("skill", "   \n  \n", "ok")
                .is_none()
        );
    }

    #[test]
    fn mcp_output_summary_shows_lines() {
        let r = StreamRenderState::new();
        let output = "Found 3 repos\nrepo1\nrepo2\nrepo3";
        let summary = r.format_output_summary("mcp_github_search", output, "ok");
        assert!(summary.is_some());
        let s = summary.unwrap();
        assert!(s.contains("Found 3 repos"));
        assert!(s.contains("repo1"));
    }

    #[test]
    fn mcp_output_summary_empty() {
        let r = StreamRenderState::new();
        assert!(
            r.format_output_summary("mcp_github_search", "", "ok")
                .is_none()
        );
    }
}
