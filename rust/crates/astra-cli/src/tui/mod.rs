#[cfg(test)]
mod tests;
#[cfg(test)]
mod layout_test;

mod app_event;
mod bottom_pane;
mod chat_cell;
mod chat_viewport;
mod color;
mod custom_terminal;
mod diff_render;
mod event;
mod frame_rate_limiter;
mod frame_requester;
mod insert_history;
mod keymap;
mod layout;
mod markdown;
mod markdown_render;
mod markdown_stream;
mod render;
mod shimmer;
mod slash_dispatch;
mod stream_bridge;
mod streaming;
mod style;
mod task_status;
mod terminal;
mod terminal_palette;
pub(crate) mod ui_adapter;
mod wrapping;

use app_event::TuiAppEvent;
use bottom_pane::{BottomPane, BottomPaneAction};
use chat_cell::{
    assistant_cell::AssistantChatCell, system_cell::SystemChatCell,
    tool_cell::ToolChatCell, user_cell::UserChatCell, ChatCell,
};

use ratatui::widgets::Clear;
use std::time::Duration;
use streaming::controller::StreamController;
use task_status::TaskStatus;
use terminal::TerminalGuard;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use event::{TuiEvent, TuiEventStream};
use frame_requester::FrameRequester;

/// Flush a completed cell to terminal scrollback with trailing blank lines.
fn flush_cell_to_scrollback(
    guard: &mut TerminalGuard,
    cell: Box<dyn ChatCell>,
    width: u16,
    transcript: &mut Vec<ratatui::text::Line<'static>>,
) {
    let display = cell.display_lines(width);
    let trans = cell.transcript_lines(width);

    if !display.is_empty() {
        let mut hist = Vec::new();
        hist.extend(display);
        hist.push(ratatui::text::Line::default());
        hist.push(ratatui::text::Line::default());
        guard.queue_history_lines(hist);
    }

    if !trans.is_empty() {
        transcript.extend(trans);
        transcript.push(ratatui::text::Line::default());
    }
}

/// Flush a streaming mini-cell to scrollback (no trailing blanks — continuation).
fn flush_mini_cell(
    guard: &mut TerminalGuard,
    cell: Box<dyn ChatCell>,
    width: u16,
    transcript: &mut Vec<ratatui::text::Line<'static>>,
) {
    let display = cell.display_lines(width);
    let trans = cell.transcript_lines(width);
    if !display.is_empty() {
        guard.queue_history_lines(display);
    }
    if !trans.is_empty() {
        transcript.extend(trans);
    }
}

/// Finalize stream controller: emit final mini-cell + trailing blanks.
fn finalize_stream(
    sc: &mut Option<StreamController>,
    guard: &mut TerminalGuard,
    width: u16,
    transcript: &mut Vec<ratatui::text::Line<'static>>,
) {
    if let Some(mut controller) = sc.take() {
        let (final_cell, _source) = controller.finalize();
        if let Some(cell) = final_cell {
            flush_mini_cell(guard, cell, width, transcript);
        }
        guard.queue_history_lines(vec![
            ratatui::text::Line::default(),
            ratatui::text::Line::default(),
        ]);
        transcript.push(ratatui::text::Line::default());
    }
}

/// Check if the terminal supports TUI mode.
pub(crate) fn can_run_tui() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

pub(crate) async fn run_tui_repl(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    resume_session_id: Option<&str>,
    no_instructions: bool,
    max_budget: f64,
) -> Result<(), String> {
    use crate::repl_runtime::{build_repl_editor, initialize_repl_state};
    use crate::repl_startup::complete_repl_startup;
    use crate::startup_trace::StartupTracer;

    // ── Ensure terminal is in sane state before startup output ────────
    // Previous astra crashes may leave terminal in raw mode, causing
    // startup eprintln output to lose carriage returns.
    let _ = crossterm::terminal::disable_raw_mode();

    // ── Business initialization BEFORE entering TUI ─────────────────────
    let mut tracer = StartupTracer::new();
    crate::repl_runtime::try_silent_auth(api, profile).await;
    tracer.phase("auth");
    let (_editor, _hist_path) = build_repl_editor()?;
    tracer.phase("editor");
    let mut state = initialize_repl_state(profile, initial_model);
    if max_budget > 0.0 {
        state.max_budget_limit = max_budget;
    }
    tracer.phase("state_init");
    let startup = complete_repl_startup(&mut state, &mut tracer, api, profile, resume_session_id, no_instructions).await?;
    tracer.finish();

    // ── TUI mode overrides ──────────────────────────────────────────────
    let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
    state.tui_render_policy = Some(crate::stream_render::RenderPolicy::Silent);
    let mut tui_cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    state.tui_cancel_token = Some(tui_cancel_token.clone());

    // Approval channel: tool approval requests from SSE host → TUI overlay
    let (approval_tx, mut approval_rx) = tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::ApprovalRequest>();
    state.tui_approval_request_tx = Some(approval_tx);

    // ── Enter TUI ───────────────────────────────────────────────────────
    let mut guard = TerminalGuard::init().map_err(|e| format!("TUI init failed: {e}"))?;
    let (draw_tx, draw_rx) = broadcast::channel(16);
    let frame_requester = FrameRequester::new(draw_tx);
    let mut event_stream = TuiEventStream::new(draw_rx);

    let mut bottom_pane = BottomPane::new();
    if let Some(ref model) = state.model {
        bottom_pane.footer.model = Some(model.clone());
    }
    if let Some(ref sid) = state.session_id {
        bottom_pane.footer.session_id = Some(sid[..8.min(sid.len())].to_string());
    }
    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));

    // Load skill items for $ mention popup
    {
        let manifests = state.unified_skill_registry.all_manifests();
        let skill_items: Vec<bottom_pane::skill_popup::SkillItem> = manifests
            .into_iter()
            .filter(|m| m.user_invocable)
            .map(|m| bottom_pane::skill_popup::SkillItem {
                name: m.name.clone(),
                description: m.description.clone(),
                source: format!("{:?}", m.source),
            })
            .collect();
        bottom_pane.set_skill_items(skill_items);
    }

    let mut active_cell: Option<Box<dyn ChatCell>> = None;
    let mut stream_controller: Option<StreamController> = None;
    let mut transcript: Vec<ratatui::text::Line<'static>> = Vec::new();
    let mut inject_submit: Option<String> = None;

    frame_requester.schedule_frame();

    let result: Result<(), String> = 'main: loop {
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(tick);

        // After turn ends, load first queued message into composer for review/send
        if active_cell.is_none() && stream_controller.is_none() {
            if let Some(text) = inject_submit.take() {
                bottom_pane.composer.set_text(&text);
                frame_requester.schedule_frame();
            }
        }

        tokio::select! {
            Some(ev) = event_stream.next() => {
                match ev {
                    TuiEvent::Key(key) => {
                        // Ctrl+L: force full redraw
                        if key.code == crossterm::event::KeyCode::Char('l')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        {
                            let _ = guard.terminal.clear();
                            guard.terminal.invalidate_viewport();
                            frame_requester.schedule_frame();
                            continue;
                        }
                        // Ctrl+O: open transcript view
                        if key.code == crossterm::event::KeyCode::Char('o')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                            && !bottom_pane.has_active_view()
                        {
                            use bottom_pane::transcript_view::TranscriptView;
                            if transcript.is_empty() {
                                // nothing to show
                            } else {
                                bottom_pane.push_view(Box::new(TranscriptView::new(transcript.clone())));
                            }
                            frame_requester.schedule_frame();
                            continue;
                        }
                        match bottom_pane.handle_key(key) {
                            BottomPaneAction::SubmitInput(text) if active_cell.is_none() => {
                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                let user_cell = UserChatCell::new(text.clone());
                                let user_lines = user_cell.display_lines(w);
                                transcript.extend(user_cell.transcript_lines(w));
                                transcript.push(ratatui::text::Line::default());
                                guard.queue_history_lines(user_lines);

                                do_draw(&mut guard, &active_cell, &mut bottom_pane)?;

                                if text.starts_with('/') {
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane, width: w,
                                    };
                                    let result = slash_dispatch::dispatch(&text, &mut dctx).await;
                                    match result {
                                        slash_dispatch::SlashResult::Handled => {}
                                        slash_dispatch::SlashResult::Exit => { break 'main Ok(()); }
                                        slash_dispatch::SlashResult::Fallback => {
                                            let slash_text = text.clone();
                                            let slash_result = guard.with_restored(|| async {
                                                let token = crate::repl_runtime::current_access_token(profile);
                                                crate::slash_router::handle_slash_command(
                                                    &slash_text, api, profile, &mut state,
                                                    token.as_deref(), &*startup.selector,
                                                ).await
                                            }).await;
                                            match slash_result {
                                                Ok(Ok(true)) => { break 'main Ok(()); }
                                                Ok(Ok(false)) => {}
                                                Ok(Err(e)) => {
                                                    let err = SystemChatCell::error(e);
                                                    guard.queue_history_lines(err.display_lines(w));
                                                }
                                                Err(e) => {
                                                    let err = SystemChatCell::error(format!("Terminal restore failed: {e}"));
                                                    guard.queue_history_lines(err.display_lines(w));
                                                }
                                            }
                                        }
                                    }
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    if let Some(ref s) = state.session_id { bottom_pane.footer.session_id = Some(s[..8.min(s.len())].to_string()); }
                                    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));
                                } else {
                                    let mut ac = AssistantChatCell::from_rendered(vec![]);
                                    ac.start_thinking();
                                    active_cell = Some(Box::new(ac));
                                    stream_controller = Some(StreamController::new(Some(w as usize)));
                                    bottom_pane.set_task_status(TaskStatus::WaitingModel);
                                    let turn_start = std::time::Instant::now();
                                    let pre_prompt_tokens = state.total_prompt_tokens;
                                    let pre_completion_tokens = state.total_completion_tokens;
                                    let pre_cost = state.total_session_cost;
                                    let pre_cache_read = state.total_cache_read_tokens;
                                    let pre_cache_creation = state.total_cache_creation_tokens;
                                    let mut turn_tool_count: u32 = 0;
                                    let mut turn_ttft: Option<std::time::Instant> = None;

                                    let turn_tx = stream_bridge::create_per_turn_bridge(tui_tx.clone());
                                    state.tui_stream_event_tx = Some(turn_tx);

                                    let turn_result = {
                                        let ctx = crate::repl_turn::ReplTurnContext { api, profile, selector: &*startup.selector };
                                        let token = crate::repl_runtime::current_access_token(profile);
                                        let mut tui_ui = ui_adapter::TuiUiAdapter::new(tui_tx.clone());
                                        let fut = crate::repl_turn::handle_chat_input_with_ui(text, token.as_deref(), &mut state, ctx, &mut tui_ui);
                                        tokio::pin!(fut);

                                        let r: Result<(), String> = loop {
                                            let itick = tokio::time::sleep(Duration::from_millis(80));
                                            tokio::pin!(itick);
                                            tokio::select! {
                                                result = &mut fut => { break result; }
                                                Some(tev) = event_stream.next() => {
                                                    match tev {
                                                        TuiEvent::Key(k) => {
                                                            // During turn: composer stays usable.
                                                            // Enter queues message (shown as preview, not in scrollback).
                                                            // Up edits last queued. Ctrl+C interrupts.
                                                            // Up arrow with queued messages → edit last
                                                            if k.code == crossterm::event::KeyCode::Up
                                                                && !bottom_pane.queued_messages.is_empty()
                                                                && bottom_pane.composer.is_empty()
                                                            {
                                                                bottom_pane.edit_last_queued();
                                                            } else {
                                                                match bottom_pane.handle_key(k) {
                                                                    BottomPaneAction::SubmitInput(queued_text) => {
                                                                        bottom_pane.queued_messages.push(queued_text);
                                                                    }
                                                                    BottomPaneAction::Interrupt | BottomPaneAction::Quit => {
                                                                        tui_cancel_token.cancel();
                                                                    }
                                                                    _ => {}
                                                                }
                                                            }
                                                            frame_requester.schedule_frame();
                                                            let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                        }
                                                        TuiEvent::Resize | TuiEvent::Draw => {
                                                            let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                Some(ae) = tui_rx.recv() => {
                                                    // Track per-turn metrics
                                                    match &ae {
                                                        TuiAppEvent::Token(_) => {
                                                            if turn_ttft.is_none() {
                                                                turn_ttft = Some(std::time::Instant::now());
                                                            }
                                                        }
                                                        TuiAppEvent::ToolStarted { .. } => {
                                                            turn_tool_count += 1;
                                                        }
                                                        _ => {}
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    handle_app_event(ae, &mut guard, w, &mut stream_controller, &mut active_cell, &mut bottom_pane, &frame_requester, &mut transcript);
                                                    let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                }
                                                Some(req) = approval_rx.recv() => {
                                                    use bottom_pane::approval_overlay::ApprovalOverlay;
                                                    let overlay = ApprovalOverlay::new(
                                                        req.tool,
                                                        req.header,
                                                        req.detail,
                                                        req.reason,
                                                        req.response_tx,
                                                    );
                                                    bottom_pane.push_view(Box::new(overlay));
                                                    frame_requester.schedule_frame();
                                                    let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                }
                                                _ = &mut itick => {
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80); drain_tick(&mut stream_controller, &mut guard, w, &mut transcript, &frame_requester);
                                                    let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                }
                                            }
                                        };
                                        r
                                    };

                                    state.tui_stream_event_tx = None;

                                    // Drain remaining events (also track ttft/tools)
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    loop {
                                        match tui_rx.recv().await {
                                            Some(TuiAppEvent::TurnComplete) | None => break,
                                            Some(ae) => {
                                                match &ae {
                                                    TuiAppEvent::Token(_) if turn_ttft.is_none() => {
                                                        turn_ttft = Some(std::time::Instant::now());
                                                    }
                                                    TuiAppEvent::ToolStarted { .. } => {
                                                        turn_tool_count += 1;
                                                    }
                                                    _ => {}
                                                }
                                                handle_app_event(ae, &mut guard, w, &mut stream_controller, &mut active_cell, &mut bottom_pane, &frame_requester, &mut transcript);
                                            }
                                        }
                                    }

                                    // Finalize stream — emit remaining mini-cell + trailing blanks
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    finalize_stream(&mut stream_controller, &mut guard, w, &mut transcript);

                                    // Flush any remaining active cell (thinking indicator, tool cell)
                                    if let Some(cell) = active_cell.take() {
                                        flush_cell_to_scrollback(&mut guard, cell, w, &mut transcript);
                                    }

                                    bottom_pane.set_task_status(TaskStatus::Idle);
                                    if let Err(e) = turn_result {
                                        let err = SystemChatCell::error(e);
                                        guard.queue_history_lines(err.display_lines(w));
                                    }

                                    // Update footer
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    if let Some(ref s) = state.session_id { bottom_pane.footer.session_id = Some(s[..8.min(s.len())].to_string()); }
                                    bottom_pane.footer.token_usage = Some(format!("{}↑ {}↓", state.total_prompt_tokens, state.total_completion_tokens));
                                    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));

                                    // Turn summary separator
                                    {
                                        let turn_prompt = state.total_prompt_tokens - pre_prompt_tokens;
                                        let turn_completion = state.total_completion_tokens - pre_completion_tokens;
                                        let turn_cost = state.total_session_cost - pre_cost;
                                        let turn_cache_read = state.total_cache_read_tokens - pre_cache_read;
                                        let turn_cache_creation = state.total_cache_creation_tokens - pre_cache_creation;
                                        let elapsed = turn_start.elapsed();
                                        let ttft_ms = turn_ttft.map(|t| {
                                            t.duration_since(turn_start).as_millis() as u64
                                        });
                                        let summary = format_turn_summary(
                                            &state, turn_prompt, turn_completion,
                                            turn_cache_read, turn_cache_creation,
                                            turn_cost, elapsed, ttft_ms, turn_tool_count,
                                        );
                                        let dim = ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray);
                                        let line = ratatui::text::Line::from(
                                            ratatui::text::Span::styled(summary, dim),
                                        );
                                        guard.queue_history_lines(vec![line, ratatui::text::Line::default()]);
                                    }

                                    let new_tok = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                                    tui_cancel_token = new_tok.clone();
                                    state.tui_cancel_token = Some(new_tok);

                                    // Auto-send first queued message (will be picked up next iteration)
                                    inject_submit = bottom_pane.take_next_queued();
                                }
                            }
                            BottomPaneAction::SubmitInput(_) => {}
                            BottomPaneAction::ViewCompleted { result, reopen } => {
                                if let Some(name) = result {
                                    slash_dispatch::handle_view_result(
                                        &name, &mut state, &mut guard, &mut bottom_pane,
                                    );
                                    bottom_pane.sync_popups();
                                    // Update footer after view actions (model/permission may change)
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));
                                } else if let Some(cmd) = reopen {
                                    // Reopen parent menu (e.g., Esc from stats detail → back to /stats menu)
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane, width: w,
                                    };
                                    let _ = slash_dispatch::dispatch(&cmd, &mut dctx).await;
                                }
                            }
                            BottomPaneAction::Interrupt | BottomPaneAction::Quit => { break 'main Ok(()); }
                            BottomPaneAction::Consumed => {}
                            BottomPaneAction::Escalate(_) => {}
                        }
                        frame_requester.schedule_frame();
                    }
                    TuiEvent::Resize => {
                        guard.terminal.invalidate_viewport();
                        do_draw(&mut guard, &active_cell, &mut bottom_pane)?;
                    }
                    TuiEvent::Draw => {
                        do_draw(&mut guard, &active_cell, &mut bottom_pane)?;
                    }
                    TuiEvent::Paste(text) => {
                        for c in text.chars() {
                            let fk = crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Char(c), crossterm::event::KeyModifiers::NONE);
                            let _ = bottom_pane.handle_key(fk);
                        }
                        frame_requester.schedule_frame();
                    }
                }
            }
            Some(ae) = tui_rx.recv() => {
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                handle_app_event(ae, &mut guard, w, &mut stream_controller, &mut active_cell, &mut bottom_pane, &frame_requester, &mut transcript);
            }
            _ = &mut tick => {
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80); drain_tick(&mut stream_controller, &mut guard, w, &mut transcript, &frame_requester);
            }
        }
    };
    drop(guard);
    result
}

fn do_draw(
    guard: &mut TerminalGuard,
    active_cell: &Option<Box<dyn ChatCell>>,
    bottom_pane: &mut BottomPane,
) -> Result<(), String> {
    use render::renderable::{FlexRenderable, RenderableItem, Renderable, RenderableExt};
    use render::Insets;

    bottom_pane.pre_draw_tick(std::time::Instant::now());

    let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);

    let ac_renderable: RenderableItem<'_> = match active_cell {
        Some(cell) => {
            let lines = cell.display_lines(width);
            let text = ratatui::text::Text::from(lines);
            let para = ratatui::widgets::Paragraph::new(text);
            RenderableItem::Owned(Box::new(para))
                .inset(Insets::tlbr(1, 0, 0, 0))
        }
        None => RenderableItem::Owned(Box::new(())),
    };

    // Thin dim separator between scrollback area and composer
    let sep_line = ratatui::text::Line::from(ratatui::text::Span::styled(
        "─".repeat(width as usize),
        ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
    ));
    let sep_renderable = RenderableItem::Owned(Box::new(sep_line));

    let bp_renderable = BottomPaneRenderable(bottom_pane);
    let bp_item = RenderableItem::Owned(Box::new(bp_renderable) as Box<dyn Renderable>);

    let mut flex = FlexRenderable::new();
    flex.push(1, ac_renderable);
    flex.push(0, sep_renderable);
    flex.push(0, bp_item);

    let total_h = flex.desired_height(width);

    guard.draw(total_h, |frame| {
        let area = frame.area();
        Clear.render(area, frame.buffer_mut());
        flex.render(area, frame.buffer_mut());

        if let Some((x, y)) = flex.cursor_pos(area) {
            frame.set_cursor_position((x, y));
        }
    }).map_err(|e| format!("draw: {e}"))?;
    Ok(())
}

/// Handle a TUI app event. When a cell transition occurs (tool→assistant or
/// assistant→tool), the previous cell is flushed to scrollback automatically.
#[allow(clippy::too_many_arguments)]
fn handle_app_event(
    ev: TuiAppEvent,
    guard: &mut TerminalGuard,
    width: u16,
    sc: &mut Option<StreamController>,
    active_cell: &mut Option<Box<dyn ChatCell>>,
    bottom_pane: &mut BottomPane,
    fr: &FrameRequester,
    transcript: &mut Vec<ratatui::text::Line<'static>>,
) {
    match ev {
        TuiAppEvent::Token(text) => {
            // If active cell is a ToolChatCell, flush it before streaming text
            let need_new_stream = active_cell
                .as_ref()
                .map(|c| c.as_any_ref().is::<ToolChatCell>())
                .unwrap_or(false);
            if need_new_stream {
                if let Some(cell) = active_cell.take() {
                    flush_cell_to_scrollback(guard, cell, width, transcript);
                }
                *sc = Some(StreamController::new(Some(width as usize)));
            }

            // Clear thinking state — tokens are flowing.
            // Save thinking content to transcript before discarding.
            if let Some(cell) = active_cell.as_ref() {
                if cell.as_any_ref().is::<AssistantChatCell>() {
                    let trans = cell.transcript_lines(width);
                    if !trans.is_empty() {
                        transcript.extend(trans);
                        transcript.push(ratatui::text::Line::default());
                    }
                    active_cell.take();
                }
            }

            // Ensure stream controller exists
            if sc.is_none() {
                *sc = Some(StreamController::new(Some(width as usize)));
            }

            // Push delta and drain any ready lines as mini-cells to scrollback
            if let Some(s) = sc {
                if s.push_delta(&text) {
                    // Newline crossed — drain catch-up batch
                    let (cell, _idle) = s.on_commit_tick_batch(5);
                    if let Some(cell) = cell {
                        flush_mini_cell(guard, cell, width, transcript);
                    }
                }
            }
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: std::time::Instant::now() });
            fr.schedule_frame();
        }
        TuiAppEvent::ThinkingStarted => {
            // If active cell is a tool, flush it first
            let is_tool = active_cell
                .as_ref()
                .map(|c| c.as_any_ref().is::<ToolChatCell>())
                .unwrap_or(false);
            if is_tool {
                if let Some(cell) = active_cell.take() {
                    flush_cell_to_scrollback(guard, cell, width, transcript);
                }
            }

            // Create assistant cell if none exists
            if active_cell.is_none() {
                let mut ac = AssistantChatCell::from_rendered(vec![]);
                ac.start_thinking();
                *active_cell = Some(Box::new(ac));
                *sc = Some(StreamController::new(Some(width as usize)));
            } else if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                    ac.start_thinking();
                }
            }
            fr.schedule_frame();
        }
        TuiAppEvent::ThinkingChunk(text) => {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                    ac.push_thinking_chunk(&text);
                }
            } else {
                // Active cell already taken (tokens flowing) — append to transcript
                let dim_italic = ratatui::style::Style::default()
                    .fg(ratatui::style::Color::DarkGray)
                    .add_modifier(ratatui::style::Modifier::ITALIC);
                for line in text.lines() {
                    let preview: String = line.chars().take(width as usize - 6).collect();
                    transcript.push(ratatui::text::Line::from(
                        ratatui::text::Span::styled(format!("  │ {preview}"), dim_italic),
                    ));
                }
            }
            fr.schedule_frame();
        }
        TuiAppEvent::ThinkingStopped => {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                    ac.finish_thinking();
                }
            }
            // No action needed if active_cell is None — thinking already saved to transcript
            fr.schedule_frame();
        }
        TuiAppEvent::WaitingForModel => {
            bottom_pane.set_task_status(TaskStatus::WaitingModel);
            fr.schedule_frame();
        }
        TuiAppEvent::ModelResponding => {
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: std::time::Instant::now() });
            fr.schedule_frame();
        }
        TuiAppEvent::ToolStarted { name, description } => {
            // Finalize any active stream — flush remaining mini-cell
            finalize_stream(sc, guard, width, transcript);
            // Flush any non-streaming active cell (thinking indicator, etc.)
            if let Some(cell) = active_cell.take() {
                flush_cell_to_scrollback(guard, cell, width, transcript);
            }

            *active_cell = Some(Box::new(ToolChatCell::new_running(name.clone(), description)));
            bottom_pane.set_task_status(TaskStatus::ToolExecuting { name, started_at: std::time::Instant::now() });
            fr.schedule_frame();
        }
        TuiAppEvent::ToolCompleted { name: _, description, status, duration_ms, output_summary, output } => {
            if let Some(cell) = active_cell {
                if let Some(tc) = cell.as_any_mut().downcast_mut::<ToolChatCell>() {
                    tc.complete(&status, duration_ms, description, output_summary, output);
                }
            }
            fr.schedule_frame();
        }
        TuiAppEvent::StatusLine(_) => {}
        TuiAppEvent::TurnComplete | TuiAppEvent::TurnError(_) => {
            bottom_pane.set_task_status(TaskStatus::Idle);
            fr.schedule_frame();
        }
    }
}

/// Codex pattern: each tick drains queued lines into a mini-cell,
/// immediately flushed to scrollback. No bulk flush at turn end.
fn drain_tick(
    sc: &mut Option<StreamController>,
    guard: &mut TerminalGuard,
    width: u16,
    transcript: &mut Vec<ratatui::text::Line<'static>>,
    fr: &FrameRequester,
) {
    if let Some(s) = sc {
        let (cell, _idle) = s.on_commit_tick();
        if let Some(cell) = cell {
            flush_mini_cell(guard, cell, width, transcript);
            fr.schedule_frame();
        }
    }
}

use ratatui::widgets::Widget;

#[allow(clippy::too_many_arguments)]
fn format_turn_summary(
    state: &crate::repl_state::ReplState,
    prompt_tokens: u64,
    completion_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    turn_cost: f64,
    elapsed: std::time::Duration,
    ttft_ms: Option<u64>,
    tool_count: u32,
) -> String {
    let elapsed_str = if elapsed.as_secs() >= 60 {
        format!("{}m{:.0}s", elapsed.as_secs() / 60, elapsed.as_secs() % 60)
    } else {
        format!("{:.1}s", elapsed.as_secs_f64())
    };

    let total_input = prompt_tokens + cache_read_tokens + cache_creation_tokens;
    let total_tokens = total_input + completion_tokens;
    let tokens_str = if total_tokens > 1000 {
        format!("{:.1}k", total_tokens as f64 / 1000.0)
    } else {
        format!("{total_tokens}")
    };
    let prompt_short = if total_input > 1000 {
        format!("{:.1}k", total_input as f64 / 1000.0)
    } else {
        format!("{total_input}")
    };
    let completion_short = if completion_tokens > 1000 {
        format!("{:.1}k", completion_tokens as f64 / 1000.0)
    } else {
        format!("{completion_tokens}")
    };

    let mut parts = Vec::new();

    if let Some(ref model) = state.model {
        parts.push(format!("model:{model}"));
    }

    parts.push(format!("tokens:{tokens_str} (↑{prompt_short} ↓{completion_short})"));

    if turn_cost > 0.0 {
        parts.push(crate::slash_stats::format_cost(turn_cost));
    }

    parts.push(elapsed_str);

    if let Some(ttft) = ttft_ms {
        if ttft > 0 {
            parts.push(format!("ttft:{ttft}ms"));
        }
    }

    if tool_count > 0 {
        parts.push(format!(
            "{} tool{}",
            tool_count,
            if tool_count == 1 { "" } else { "s" }
        ));
    }

    if cache_read_tokens > 0 {
        let cache_pct = cache_read_tokens as f64 / total_input.max(1) as f64 * 100.0;
        parts.push(format!("cache:{cache_pct:.0}%"));
    }

    let session_cost = state.total_session_cost;
    let mut line = format!("  ─ {} ─", parts.join(" │ "));
    if session_cost > 0.0 && state.turn > 0 {
        line.push_str(&format!("  session: {}", crate::slash_stats::format_cost(session_cost)));
    }
    line
}

struct BottomPaneRenderable<'a>(&'a mut BottomPane);

impl<'a> render::renderable::Renderable for BottomPaneRenderable<'a> {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        self.0.render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }
    fn cursor_pos(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        self.0.cursor_position(area)
    }
}


