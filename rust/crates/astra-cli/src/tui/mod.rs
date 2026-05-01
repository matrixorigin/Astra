#[cfg(test)]
mod tests;

mod app_event;
mod bottom_pane;
mod chat_cell;
mod chat_viewport;
mod color;
mod event;
mod frame_rate_limiter;
mod frame_requester;
mod keymap;
mod layout;
mod markdown;
mod markdown_render;
mod markdown_stream;
mod render;
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
    assistant_cell::AssistantChatCell, system_cell::SystemChatCell, user_cell::UserChatCell,
};
use chat_viewport::ChatViewport;
use keymap::{AppAction, AppKeymap};
use ratatui::layout::{Constraint, Layout};
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

    let startup = complete_repl_startup(
        &mut state,
        &mut tracer,
        api,
        profile,
        resume_session_id,
        no_instructions,
    )
    .await?;
    tracer.finish();

    // ── Hard gate: reject Prompt permission mode ────────────────────────
    if state.perm_manager.mode() == crate::permission_manager::PermissionMode::Prompt {
        eprintln!(
            "TUI mode does not yet support interactive tool approval.\n\
             Use `astra --tui --yes` (auto-approve) or drop `--tui` for line mode."
        );
        return Ok(());
    }

    // ── Inject TUI mode overrides ───────────────────────────────────────
    let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
    state.tui_render_policy = Some(crate::stream_render::RenderPolicy::Silent);
    // stream_event_tx is created per-turn (not here) so bridge can detect turn completion
    let mut tui_cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    state.tui_cancel_token = Some(tui_cancel_token.clone());

    // ── Enter TUI ───────────────────────────────────────────────────────
    let mut guard = TerminalGuard::init().map_err(|e| format!("TUI init failed: {e}"))?;

    let (draw_tx, draw_rx) = broadcast::channel(16);
    let frame_requester = FrameRequester::new(draw_tx);
    let mut event_stream = TuiEventStream::new(draw_rx);

    let mut viewport = ChatViewport::new();
    let mut bottom_pane = BottomPane::new();

    if let Some(ref model) = state.model {
        bottom_pane.footer.model = Some(model.clone());
    }
    if let Some(ref sid) = state.session_id {
        bottom_pane.footer.session_id = Some(sid[..8.min(sid.len())].to_string());
    }

    viewport.push_cell(Box::new(SystemChatCell::info(
        "astra TUI — connected to backend. Type a message.".to_string(),
    )));

    frame_requester.schedule_frame();

    let mut stream_controller: Option<StreamController> = None;
    let mut active_cell_idx: Option<usize> = None;
    let mut _turn_running = false;

    let result: Result<(), String> = 'main: loop {
        // If a turn is NOT running, we just poll UI + tui_rx + tick.
        // If a turn IS running, it's handled via a separate inner loop below.
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(tick);

        tokio::select! {
            Some(event) = event_stream.next() => {
                match event {
                    TuiEvent::Key(key) => {
                        match bottom_pane.handle_key(key) {
                            BottomPaneAction::SubmitInput(text) if !_turn_running => {
                                viewport.push_cell(Box::new(UserChatCell::new(text.clone())));

                                if text.starts_with('/') {
                                    viewport.push_cell(Box::new(SystemChatCell::info(
                                        format!("Slash command: {text} (use line mode for full slash support)")
                                    )));
                                } else {
                                    // Start a turn — run it in a concurrent select loop
                                    let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    stream_controller = Some(StreamController::new(Some(width as usize)));
                                    let empty_cell = AssistantChatCell::from_rendered(vec![]);
                                    active_cell_idx = Some(viewport.push_cell_get_idx(Box::new(empty_cell)));
                                    bottom_pane.set_task_status(TaskStatus::WaitingModel);
                                    _turn_running = true;
                                    frame_requester.schedule_frame();

                                    // Create per-turn bridge so TurnComplete fires when this turn's stream ends
                                    let turn_stream_tx = stream_bridge::create_per_turn_bridge(tui_tx.clone());
                                    state.tui_stream_event_tx = Some(turn_stream_tx);

                                    // Run the turn in a block so &mut state borrow ends before post-turn reads
                                    let turn_result = {
                                        let ctx = crate::repl_turn::ReplTurnContext {
                                            api,
                                            profile,
                                            selector: &*startup.selector,
                                        };
                                        let current_token = crate::repl_runtime::current_access_token(profile);
                                        let mut tui_ui = ui_adapter::TuiUiAdapter::new(tui_tx.clone());
                                        let turn_future = crate::repl_turn::handle_chat_input_with_ui(
                                            text,
                                            current_token.as_deref(),
                                            &mut state,
                                            ctx,
                                            &mut tui_ui,
                                        );
                                        tokio::pin!(turn_future);

                                        let r: Result<(), String> = loop {
                                            let inner_tick = tokio::time::sleep(Duration::from_millis(50));
                                            tokio::pin!(inner_tick);

                                            tokio::select! {
                                                result = &mut turn_future => {
                                                    break result;
                                                }
                                                Some(tui_event) = event_stream.next() => {
                                                    handle_ui_event_during_turn(
                                                        tui_event, &mut guard, &mut viewport, &mut bottom_pane,
                                                        &frame_requester, &tui_cancel_token,
                                                    );
                                                }
                                                Some(app_event) = tui_rx.recv() => {
                                                    handle_app_event_during_turn(
                                                        app_event, &mut stream_controller, active_cell_idx,
                                                        &mut viewport, &mut bottom_pane, &frame_requester,
                                                    );
                                                }
                                                _ = &mut inner_tick => {
                                                    drain_stream_tick(&mut stream_controller, active_cell_idx, &mut viewport, &frame_requester);
                                                }
                                            }
                                        };
                                        r
                                    };
                                    // &mut state borrow is now released

                                    // Drop the per-turn stream sender so bridge detects closure
                                    state.tui_stream_event_tx = None;

                                    // Wait for TurnComplete from bridge to ensure all tokens
                                    // have been forwarded before finalizing.
                                    loop {
                                        match tui_rx.recv().await {
                                            Some(TuiAppEvent::TurnComplete) | None => break,
                                            Some(evt) => {
                                                handle_app_event_during_turn(
                                                    evt, &mut stream_controller, active_cell_idx,
                                                    &mut viewport, &mut bottom_pane, &frame_requester,
                                                );
                                            }
                                        }
                                    }

                                    // Finalize stream controller
                                    if let Some(sc) = stream_controller.take() {
                                        let (cell, _) = sc.finalize();
                                        if let Some(cell) = cell {
                                            if let Some(idx) = active_cell_idx {
                                                viewport.replace_cell(idx, cell);
                                            } else {
                                                viewport.push_cell(cell);
                                            }
                                        }
                                    }
                                    active_cell_idx = None;
                                    _turn_running = false;
                                    bottom_pane.set_task_status(TaskStatus::Idle);
                                    // Reset cancel token for next turn
                                    let new_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                                    tui_cancel_token = new_token.clone();
                                    state.tui_cancel_token = Some(new_token);

                                    if let Err(e) = turn_result {
                                        viewport.push_cell(Box::new(SystemChatCell::error(e)));
                                    }

                                    if let Some(ref model) = state.model {
                                        bottom_pane.footer.model = Some(model.clone());
                                    }
                                    if let Some(ref sid) = state.session_id {
                                        bottom_pane.footer.session_id = Some(sid[..8.min(sid.len())].to_string());
                                    }
                                    bottom_pane.footer.token_usage = Some(format!(
                                        "{}↑ {}↓", state.total_prompt_tokens, state.total_completion_tokens
                                    ));
                                    frame_requester.schedule_frame();
                                }
                            }
                            BottomPaneAction::SubmitInput(_) => {} // turn in progress
                            BottomPaneAction::Interrupt | BottomPaneAction::Quit => {
                                break 'main Ok(());
                            }
                            BottomPaneAction::Consumed => {}
                            BottomPaneAction::Escalate(ek) => {
                                if let Some(action) = AppKeymap::resolve(ek) {
                                    handle_app_action(action, &mut viewport, &guard);
                                }
                            }
                        }
                        frame_requester.schedule_frame();
                    }
                    TuiEvent::Resize | TuiEvent::Draw => {
                        draw_frame(&mut guard, &mut viewport, &mut bottom_pane)?;
                    }
                    TuiEvent::Paste(text) => {
                        for c in text.chars() {
                            let fk = crossterm::event::KeyEvent::new(
                                crossterm::event::KeyCode::Char(c),
                                crossterm::event::KeyModifiers::NONE,
                            );
                            let _ = bottom_pane.handle_key(fk);
                        }
                        frame_requester.schedule_frame();
                    }
                }
            }
            // Drain any stale app events between turns
            Some(app_event) = tui_rx.recv() => {
                handle_app_event_during_turn(
                    app_event, &mut stream_controller, active_cell_idx,
                    &mut viewport, &mut bottom_pane, &frame_requester,
                );
            }
            _ = &mut tick => {
                drain_stream_tick(&mut stream_controller, active_cell_idx, &mut viewport, &frame_requester);
            }
        }
    };

    drop(guard);
    result
}

fn draw_frame(
    guard: &mut TerminalGuard,
    viewport: &mut ChatViewport,
    bottom_pane: &mut BottomPane,
) -> Result<(), String> {
    bottom_pane.pre_draw_tick(std::time::Instant::now());
    guard
        .terminal
        .draw(|frame| {
            let area = frame.area();
            let bottom_h = bottom_pane.desired_height(area.width);
            let chunks = Layout::vertical([
                Constraint::Min(1),
                Constraint::Length(bottom_h),
            ])
            .split(area);

            viewport.render(chunks[0], frame.buffer_mut());
            bottom_pane.render(chunks[1], frame.buffer_mut());

            if let Some((x, y)) = bottom_pane.cursor_position(chunks[1]) {
                frame.set_cursor_position((x, y));
            }
        })
        .map_err(|e| format!("draw failed: {e}"))?;
    Ok(())
}

fn handle_ui_event_during_turn(
    event: TuiEvent,
    guard: &mut TerminalGuard,
    viewport: &mut ChatViewport,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
    cancel_token: &tokio_util::sync::CancellationToken,
) {
    match event {
        TuiEvent::Key(key) => {
            use crossterm::event::{KeyCode, KeyModifiers};
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                cancel_token.cancel();
                viewport.push_cell(Box::new(SystemChatCell::info("Interrupting...".to_string())));
                frame_requester.schedule_frame();
                return;
            }
            // During turn: allow typing (builds draft) but block submit
            if key.code == KeyCode::Enter
                && !key.modifiers.contains(KeyModifiers::SHIFT)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                // Don't submit — keep draft in composer, show hint
                return;
            }
            match bottom_pane.handle_key(key) {
                BottomPaneAction::Escalate(ek) => {
                    if let Some(action) = AppKeymap::resolve(ek) {
                        handle_app_action(action, viewport, guard);
                    }
                }
                BottomPaneAction::SubmitInput(_) => {
                    // Should not happen since we blocked Enter above, but safety net
                }
                _ => {}
            }
            frame_requester.schedule_frame();
        }
        TuiEvent::Resize | TuiEvent::Draw => {
            let _ = draw_frame(guard, viewport, bottom_pane);
        }
        TuiEvent::Paste(_) => {}
    }
}

fn handle_app_event_during_turn(
    app_event: TuiAppEvent,
    stream_controller: &mut Option<StreamController>,
    active_cell_idx: Option<usize>,
    viewport: &mut ChatViewport,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) {
    match app_event {
        TuiAppEvent::Token(text) => {
            if let Some(sc) = stream_controller {
                sc.push_delta(&text);
                if let Some(_lines) = sc.tick() {
                    if let Some(idx) = active_cell_idx {
                        let all_lines = sc.emitted_lines().to_vec();
                        viewport.replace_cell(
                            idx,
                            Box::new(AssistantChatCell::from_rendered(all_lines)),
                        );
                    }
                }
            }
            bottom_pane.set_task_status(TaskStatus::TurnRunning {
                started_at: std::time::Instant::now(),
            });
            frame_requester.schedule_frame();
        }
        TuiAppEvent::WaitingForModel => {
            bottom_pane.set_task_status(TaskStatus::WaitingModel);
            frame_requester.schedule_frame();
        }
        TuiAppEvent::ModelResponding => {
            bottom_pane.set_task_status(TaskStatus::TurnRunning {
                started_at: std::time::Instant::now(),
            });
            frame_requester.schedule_frame();
        }
        TuiAppEvent::ToolStarted { name, .. } => {
            bottom_pane.set_task_status(TaskStatus::ToolExecuting {
                name: name.clone(),
                started_at: std::time::Instant::now(),
            });
            viewport.push_cell(Box::new(SystemChatCell::info(format!("⧗ Running: {name}"))));
            frame_requester.schedule_frame();
        }
        TuiAppEvent::ToolCompleted {
            name,
            status,
            duration_ms,
            ..
        } => {
            let icon = if status == "success" { "✓" } else { "✗" };
            viewport.push_cell(Box::new(SystemChatCell::info(format!(
                "{icon} {name} ({duration_ms}ms)"
            ))));
            frame_requester.schedule_frame();
        }
        TuiAppEvent::ThinkingStarted => {
            viewport.push_cell(Box::new(SystemChatCell::info("Thinking...".to_string())));
            frame_requester.schedule_frame();
        }
        TuiAppEvent::ThinkingChunk(_) | TuiAppEvent::ThinkingStopped => {
            frame_requester.schedule_frame();
        }
        TuiAppEvent::StatusLine(text) => {
            viewport.push_cell(Box::new(SystemChatCell::info(text)));
            frame_requester.schedule_frame();
        }
        TuiAppEvent::TurnComplete | TuiAppEvent::TurnError(_) => {
            bottom_pane.set_task_status(TaskStatus::Idle);
            frame_requester.schedule_frame();
        }
    }
}

fn drain_stream_tick(
    stream_controller: &mut Option<StreamController>,
    active_cell_idx: Option<usize>,
    viewport: &mut ChatViewport,
    frame_requester: &FrameRequester,
) {
    if let Some(sc) = stream_controller {
        if let Some(_lines) = sc.tick() {
            if let Some(idx) = active_cell_idx {
                let all_lines = sc.emitted_lines().to_vec();
                viewport.replace_cell(
                    idx,
                    Box::new(AssistantChatCell::from_rendered(all_lines)),
                );
            }
            frame_requester.schedule_frame();
        }
    }
}

fn handle_app_action(action: AppAction, viewport: &mut ChatViewport, guard: &TerminalGuard) {
    let size = guard.terminal.size().unwrap_or_default();
    let vp_height = size.height.saturating_sub(6);
    match action {
        AppAction::ScrollPageUp => viewport.scroll_page_up(vp_height),
        AppAction::ScrollPageDown => viewport.scroll_page_down(vp_height),
        AppAction::JumpToTop => viewport.jump_to_top(),
        AppAction::JumpToBottom => viewport.jump_to_bottom(size.width, vp_height),
        AppAction::ForceRedraw | AppAction::ToggleTranscript => {}
    }
}
