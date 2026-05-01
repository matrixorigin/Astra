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
mod stream_bridge;
mod streaming;
mod style;
mod task_status;
mod terminal;
mod terminal_palette;
pub(crate) mod ui_adapter;

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

    // ── Hard gate ────────────────────────────────────────────────────────
    if state.perm_manager.mode() == crate::permission_manager::PermissionMode::Prompt {
        eprintln!("TUI mode does not yet support interactive tool approval.\nUse `astra --tui --yes` or drop `--tui`.");
        return Ok(());
    }

    // ── TUI mode overrides ──────────────────────────────────────────────
    let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
    state.tui_render_policy = Some(crate::stream_render::RenderPolicy::Silent);
    let mut tui_cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    state.tui_cancel_token = Some(tui_cancel_token.clone());

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

    let mut active_cell: Option<Box<dyn ChatCell>> = None;
    let mut stream_controller: Option<StreamController> = None;

    frame_requester.schedule_frame();

    let result: Result<(), String> = 'main: loop {
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(tick);

        tokio::select! {
            Some(ev) = event_stream.next() => {
                match ev {
                    TuiEvent::Key(key) => {
                        match bottom_pane.handle_key(key) {
                            BottomPaneAction::SubmitInput(text) if active_cell.is_none() => {
                                // User message → scrollback immediately
                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                let user_lines = UserChatCell::new(text.clone()).display_lines(w);
                                let mut hist = Vec::new();
                                hist.extend(user_lines);
                                guard.queue_history_lines(hist);

                                // Flush user message + draw immediately so it appears before turn starts
                                do_draw(&mut guard, &active_cell, &mut bottom_pane)?;

                                if text.starts_with('/') {
                                    let sys = SystemChatCell::info(format!("Slash: {text} (use line mode)"));
                                    guard.queue_history_lines(sys.display_lines(w));
                                } else {
                                    // Start active assistant cell + turn (pre-start thinking for Working display)
                                    let mut ac = AssistantChatCell::from_rendered(vec![]);
                                    ac.start_thinking();
                                    active_cell = Some(Box::new(ac));
                                    stream_controller = Some(StreamController::new(Some(w as usize)));
                                    bottom_pane.set_task_status(TaskStatus::WaitingModel);

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
                                                            use crossterm::event::{KeyCode, KeyModifiers};
                                                            if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                                                                tui_cancel_token.cancel();
                                                            }
                                                        }
                                                        TuiEvent::Resize | TuiEvent::Draw => {
                                                            let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                Some(ae) = tui_rx.recv() => {
                                                    handle_app_event(ae, &mut stream_controller, &mut active_cell, &mut bottom_pane, &frame_requester);
                                                    let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                }
                                                _ = &mut itick => {
                                                    drain_tick(&mut stream_controller, &mut active_cell, &frame_requester);
                                                    let _ = do_draw(&mut guard, &active_cell, &mut bottom_pane);
                                                }
                                            }
                                        };
                                        r
                                    };

                                    // Drop per-turn sender so bridge sends TurnComplete
                                    state.tui_stream_event_tx = None;

                                    // Drain remaining events
                                    loop {
                                        match tui_rx.recv().await {
                                            Some(TuiAppEvent::TurnComplete) | None => break,
                                            Some(ae) => handle_app_event(ae, &mut stream_controller, &mut active_cell, &mut bottom_pane, &frame_requester),
                                        }
                                    }

                                    // Final flush of stream controller
                                    if let Some(mut sc) = stream_controller.take() {
                                        sc.flush_pending();
                                        while let Some(_) = sc.tick() {}
                                        if let Some(cell) = &mut active_cell {
                                            if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                                                ac.update_rendered_lines(sc.emitted_lines().to_vec());
                                            }
                                        }
                                    }

                                    // Flush active cell → scrollback
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    if let Some(cell) = active_cell.take() {
                                        let lines = cell.display_lines(w);
                                        if !lines.is_empty() {
                                            let mut hist = Vec::new();
                                            hist.extend(lines);
                                            hist.push(ratatui::text::Line::default()); // blank after response
                                            hist.push(ratatui::text::Line::default()); // Codex has 2 blank lines
                                            guard.queue_history_lines(hist);
                                        }
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

                                    let new_tok = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                                    tui_cancel_token = new_tok.clone();
                                    state.tui_cancel_token = Some(new_tok);
                                }
                            }
                            BottomPaneAction::SubmitInput(_) => {}
                            BottomPaneAction::Interrupt | BottomPaneAction::Quit => { break 'main Ok(()); }
                            BottomPaneAction::Consumed => {}
                            BottomPaneAction::Escalate(_) => {}
                        }
                        frame_requester.schedule_frame();
                    }
                    TuiEvent::Resize | TuiEvent::Draw => {
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
                handle_app_event(ae, &mut stream_controller, &mut active_cell, &mut bottom_pane, &frame_requester);
            }
            _ = &mut tick => {
                drain_tick(&mut stream_controller, &mut active_cell, &frame_requester);
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

    // Build the layout exactly like Codex ChatWidget::as_renderable():
    // flex.push(1, active_cell.inset(tlbr(1,0,0,0)))  ← active cell with top inset
    // flex.push(0, bottom_pane.inset(tlbr(1,0,0,0)))  ← bottom pane with top inset
    let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);

    // Build active cell renderable
    let ac_renderable: RenderableItem<'_> = match active_cell {
        Some(cell) => {
            // Wrap cell display in a simple struct that implements Renderable
            let lines = cell.display_lines(width);
            let text = ratatui::text::Text::from(lines);
            let para = ratatui::widgets::Paragraph::new(text);
            RenderableItem::Owned(Box::new(para))
                .inset(Insets::tlbr(1, 0, 0, 0))
        }
        None => RenderableItem::Owned(Box::new(())),
    };

    // Build bottom pane renderable
    let bp_renderable = BottomPaneRenderable(bottom_pane);
    let bp_item = RenderableItem::Owned(Box::new(bp_renderable) as Box<dyn Renderable>)
        .inset(Insets::tlbr(1, 0, 0, 0));

    let mut flex = FlexRenderable::new();
    flex.push(1, ac_renderable);  // active cell: flex=1
    flex.push(0, bp_item);        // bottom pane: flex=0

    let total_h = flex.desired_height(width);

    guard.draw(total_h, |frame| {
        let area = frame.area();
        Clear.render(area, frame.buffer_mut());
        flex.render(area, frame.buffer_mut());

        // Set cursor from bottom_pane
        if let Some((x, y)) = flex.cursor_pos(area) {
            frame.set_cursor_position((x, y));
        }
    }).map_err(|e| format!("draw: {e}"))?;
    Ok(())
}

fn handle_app_event(
    ev: TuiAppEvent,
    sc: &mut Option<StreamController>,
    active_cell: &mut Option<Box<dyn ChatCell>>,
    bottom_pane: &mut BottomPane,
    fr: &FrameRequester,
) {
    match ev {
        TuiAppEvent::Token(text) => {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                    if ac.is_thinking() { ac.finish_thinking(); }
                }
            }
            if let Some(s) = sc {
                s.push_delta(&text);
                if let Some(_) = s.tick() {
                    if let Some(cell) = active_cell {
                        if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                            ac.update_rendered_lines(s.emitted_lines().to_vec());
                        }
                    }
                }
            }
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: std::time::Instant::now() });
            fr.schedule_frame();
        }
        TuiAppEvent::ThinkingStarted => {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() { ac.start_thinking(); }
            }
            fr.schedule_frame();
        }
        TuiAppEvent::ThinkingChunk(text) => {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() { ac.push_thinking_chunk(&text); }
            }
            fr.schedule_frame();
        }
        TuiAppEvent::ThinkingStopped => {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() { ac.finish_thinking(); }
            }
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
            *active_cell = Some(Box::new(ToolChatCell::new_running(name.clone(), description)));
            bottom_pane.set_task_status(TaskStatus::ToolExecuting { name, started_at: std::time::Instant::now() });
            fr.schedule_frame();
        }
        TuiAppEvent::ToolCompleted { name: _, status, duration_ms, output_summary } => {
            if let Some(cell) = active_cell {
                if let Some(tc) = cell.as_any_mut().downcast_mut::<ToolChatCell>() {
                    tc.complete(&status, duration_ms, output_summary);
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

fn drain_tick(
    sc: &mut Option<StreamController>,
    active_cell: &mut Option<Box<dyn ChatCell>>,
    fr: &FrameRequester,
) {
    if let Some(s) = sc {
        // Pure newline-gated: only drain lines that have been committed via \n
        if let Some(_) = s.tick() {
            if let Some(cell) = active_cell {
                if let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantChatCell>() {
                    ac.update_rendered_lines(s.emitted_lines().to_vec());
                }
            }
            fr.schedule_frame();
        }
    }
}

use ratatui::widgets::Widget;

/// Wrapper to make BottomPane implement Renderable for FlexRenderable composition.
struct BottomPaneRenderable<'a>(&'a mut BottomPane);

impl<'a> render::renderable::Renderable for BottomPaneRenderable<'a> {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        // BottomPane::render takes &self, but we have &mut — just use &*self.0
        self.0.render(area, buf);
    }
    fn desired_height(&self, width: u16) -> u16 {
        self.0.desired_height(width)
    }
    fn cursor_pos(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        self.0.cursor_position(area)
    }
}
