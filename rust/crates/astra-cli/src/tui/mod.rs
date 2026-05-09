#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

mod app_event;
mod approval;
mod bottom_pane;
mod context_panel;
// Core (post-refactor): HistoryCell trait + TurnEvent schema +
// single ChatWidget router + on-disk JSONL transcript. See
// `docs/design/tui-refactor.md`.
mod chat_widget;
mod color;
mod custom_terminal;
mod diff_render;
mod event;
mod frame_rate_limiter;
mod frame_requester;
mod history_cell;
mod insert_history;
mod keymap;
mod layout;
mod markdown;
mod markdown_render;
mod mention_menu;
mod render;
mod session_picker;
mod shimmer;
mod slash_dispatch;
mod slash_menu;
mod status_indicator;
mod status_line;
mod stream_bridge;
mod style;
mod table_view;
mod task_status;
mod terminal;
mod terminal_palette;
mod theme;
mod timeline;
mod transcript_jsonl;
mod turn_event;
pub(crate) mod ui_adapter;
mod view_stack;
mod worktrees;
mod wrapping;

use app_event::TuiAppEvent;
use bottom_pane::{BottomPane, BottomPaneAction};
use history_cell::HistoryCell;

use ratatui::widgets::Clear;
use std::time::Duration;
use task_status::TaskStatus;
use terminal::TerminalGuard;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use event::{TuiEvent, TuiEventStream};
use frame_requester::FrameRequester;

/// Build the lines shown in the viewport above the composer.
/// Order of preference:
/// 1. Live `active_cell` on the ChatWidget (assistant streaming,
///    tool running, etc.) — use its own `display_lines`.
/// 2. Otherwise the `StatusIndicator`'s render (one-line
///    "✶ Thinking …" style signal when a turn is in progress).
/// 3. Otherwise empty — idle REPL shows nothing above the
///    composer.
fn active_viewport_lines(
    chat_widget: &chat_widget::ChatWidget,
    status: &status_indicator::StatusIndicator,
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    if let Some(cell) = chat_widget.active_cell() {
        let lines = cell.display_lines(width);
        if !lines.is_empty() {
            return lines;
        }
    }
    if let Some(line) = status.render() {
        return vec![line];
    }
    Vec::new()
}

/// Drain newly-committed cells from the widget and render each
/// to the terminal scrollback. Single choke point for all
/// "a cell just landed in history" writes — callers don't touch
/// `guard.queue_history_lines` directly for chat content anymore.
/// A trailing blank row separates cells visually.
fn flush_chat_widget(
    guard: &mut TerminalGuard,
    chat_widget: &mut chat_widget::ChatWidget,
    width: u16,
) {
    let new_cells = chat_widget.drain_new_committed();
    if new_cells.is_empty() {
        return;
    }
    // Batch layout: each cell renders its lines then gets a trailing
    // blank for visual separation. Claude-Code-style response cells
    // (`⎿ Set model to …`) want to hug the `› /cmd` line above —
    // both when paired in the same batch (no blank between them)
    // and when the UserCell flushed in an earlier event (the
    // picker-return path). For the former we detect the pair here
    // and skip its separator; for the latter we also skip the
    // response's OWN leading and trailing blanks so the reply
    // stacks tight onto the previous flush's `› /cmd`.
    let mut batch: Vec<ratatui::text::Line<'static>> = Vec::new();
    for (i, cell) in new_cells.iter().enumerate() {
        batch.extend(cell.display_lines(width));
        let is_last = i + 1 == new_cells.len();
        let next_is_response = !is_last
            && is_response_cell(new_cells[i + 1].as_ref());
        let this_is_slash_user = is_slash_user_cell(cell.as_ref());
        let this_is_response = is_response_cell(cell.as_ref());

        // Skip the trailing blank in two cases:
        //   1. This cell is a slash UserCell and the next is a
        //      response — they're a visual pair.
        //   2. This cell is a response — its reply should stack
        //      tight onto whatever came next, and nothing in the
        //      current batch should push air below it.
        let suppress_blank =
            (this_is_slash_user && next_is_response) || this_is_response;
        if !suppress_blank {
            batch.push(ratatui::text::Line::default());
        }
    }
    guard.queue_history_lines(batch);
}

/// Detect a system cell rendered with Claude Code's corner-glyph
/// style. Used by `flush_chat_widget` to omit the usual trailing
/// blank so the response hugs the `› /cmd` line above it.
fn is_response_cell(cell: &dyn history_cell::HistoryCell) -> bool {
    cell.as_any_ref()
        .downcast_ref::<history_cell::system::SystemCell>()
        .is_some_and(|sc| sc.level() == crate::tui::turn_event::SystemLevel::Response)
}

/// Detect a UserCell whose text is a slash command (`/model`,
/// `/login`, …). These pair tightly with a following response cell
/// so their trailing blank is suppressed — `› /cmd` hugs `⎿ reply`.
fn is_slash_user_cell(cell: &dyn history_cell::HistoryCell) -> bool {
    cell.as_any_ref()
        .downcast_ref::<history_cell::user::UserCell>()
        .is_some_and(|uc| uc.text().trim_start().starts_with('/'))
}

/// Replay a session's JSONL transcript into a fresh `ChatWidget`,
/// paint the restored cells into the terminal scrollback, and
/// advance the widget's watermark so future ticks don't reflush
/// them. Returns the new widget; caller rebinds.
///
/// A one-line banner is prepended so the user can tell the
/// scrollback they're seeing is restored context, not live.
/// Empty transcripts short-circuit to an empty widget with no
/// banner — there's nothing to tell the user about.
fn replay_session_into_widget(
    guard: &mut TerminalGuard,
    session_id: &str,
    width: u16,
) -> chat_widget::ChatWidget {
    let mut widget = chat_widget::load_resume(session_id);
    let restored = widget.history().len();
    if restored == 0 {
        return widget;
    }
    // Banner first so it lands above the restored cells.
    let banner = history_cell::system::SystemCell::info(format!(
        "Resumed session {} — {} cells restored",
        &session_id[..8.min(session_id.len())],
        restored
    ));
    guard.queue_history_lines(banner.display_lines(width));
    guard.queue_history_lines(vec![ratatui::text::Line::default()]);
    // Paint the restored cells exactly once via the same rendering
    // path that streaming flushes use, so the visual match is
    // lossless.
    flush_chat_widget(guard, &mut widget, width);
    // Belt-and-suspenders: if flush_chat_widget's implementation
    // ever changes to not advance the watermark, this keeps us
    // safe.
    widget.mark_all_flushed();
    widget
}

/// One-shot lookup of the current git branch name via `gix`. Returns
/// `None` when the cwd isn't a git repo, detached HEAD, or errors.
fn detect_git_branch() -> Option<String> {
    let repo = gix::discover(std::env::current_dir().ok()?).ok()?;
    let head = repo.head().ok()?;
    let name = head.referent_name()?;
    Some(name.shorten().to_string())
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

    // ── TUI mode overrides ──────────────────────────────────────────────
    let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
    state.tui_render_policy = Some(crate::stream_render::RenderPolicy::Silent);
    let mut tui_cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    state.tui_cancel_token = Some(tui_cancel_token.clone());

    // Approval channel: tool approval requests from SSE host → TUI overlay
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::chat_stream::ApprovalRequest>();
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

    // Load slash-command catalog for the inline `/` menu.
    {
        let slash_items: Vec<slash_menu::SlashItem> = crate::command_registry::COMMANDS
            .iter()
            .filter(|m| !m.is_alias && !m.name.contains(' '))
            .map(|m| slash_menu::SlashItem {
                name: m.name,
                description: m.description,
            })
            .collect();
        bottom_pane.set_slash_items(slash_items);
    }

    // Install a filesystem-backed file provider for the `@`-mention menu,
    // rooted at the current working directory.
    if let Ok(cwd) = std::env::current_dir() {
        bottom_pane.set_file_provider(std::sync::Arc::new(
            mention_menu::provider::FsFileProvider::new(cwd),
        ));
    }

    // Seed the current git branch into the status line. One-shot read at
    // startup — branch changes rarely mid-session; refresh happens on
    // next launch. Missing/non-git dir is silently ignored.
    if let Some(branch) = detect_git_branch() {
        bottom_pane.footer.git_branch = Some(branch);
    }

    // ChatWidget owns the scrollback + active cell. If the user
    // entered via `astra -c` / `astra --resume <id>`, replay the
    // prior session's JSONL transcript into the widget and paint
    // it to the terminal scrollback exactly once. A brand-new
    // session falls through to an empty widget with an empty sid
    // (persistence becomes a no-op until the server hands out an
    // id on first turn).
    let mut chat_widget = match state.session_id.as_deref() {
        Some(sid) if !sid.is_empty() => {
            let w0 = guard.terminal.size().map(|s| s.width).unwrap_or(80);
            replay_session_into_widget(&mut guard, sid, w0)
        }
        _ => chat_widget::ChatWidget::new(String::new()),
    };
    let mut status_indicator = status_indicator::StatusIndicator::new();
    let mut inject_submit: Option<String> = None;

    frame_requester.schedule_frame();

    let result: Result<(), String> = 'main: loop {
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(tick);

        // After turn ends, load first queued message into composer for review/send.
        // The inner `select!` blocks until the turn completes, so by the time
        // control returns here the turn is always over — no guard needed.
        if let Some(text) = inject_submit.take() {
            bottom_pane.composer.set_text(&text);
            frame_requester.schedule_frame();
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
                        // Ctrl+O: open transcript view. Built on
                        // demand from the ChatWidget's committed
                        // history so the content always matches
                        // what's in scrollback. Blank lines between
                        // cells mirror the single-blank separator
                        // used by `flush_chat_widget`. The terminal
                        // height is threaded through so the overlay
                        // fills the screen on tall windows instead of
                        // stopping at a fixed 16-line peephole.
                        // Ctrl+R: edit last — pull the most recent user
                        // message back into the composer so the user can
                        // re-word and resubmit without retyping. Works only
                        // when idle (no overlay, composer empty) so it
                        // doesn't clobber in-flight drafts. The prior
                        // scrollback stays visible: the retry runs as a
                        // fresh turn below, and the model sees the earlier
                        // attempt + its reply as context (which is the point
                        // — "try again, differently").
                        if key.code == crossterm::event::KeyCode::Char('r')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                            && !bottom_pane.has_active_view()
                            && bottom_pane.composer.is_empty()
                            && let Some(prev) = chat_widget.last_user_text()
                        {
                            bottom_pane.composer.set_text(&prev);
                            frame_requester.schedule_frame();
                            continue;
                        }
                        if key.code == crossterm::event::KeyCode::Char('o')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                            && !bottom_pane.has_active_view()
                        {
                            use bottom_pane::transcript_view::TranscriptView;
                            let size = guard.terminal.size().ok();
                            let w = size.map(|s| s.width).unwrap_or(80);
                            let h = size.map(|s| s.height).unwrap_or(0);
                            let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
                            for cell in chat_widget.history() {
                                lines.extend(cell.display_lines(w));
                                lines.push(ratatui::text::Line::default());
                            }
                            if !lines.is_empty() {
                                bottom_pane.push_view(Box::new(TranscriptView::new(lines, h)));
                            }
                            frame_requester.schedule_frame();
                            continue;
                        }
                        match bottom_pane.handle_key(key) {
                            BottomPaneAction::SubmitInput(text) => {
                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                // Shadow: mirror the user submit into
                                // ChatWidget so its history stays in
                                // sync with legacy scrollback. Does
                                // persistence (when sid is non-empty)
                                // even though rendering still runs
                                // through the legacy path.
                                chat_widget.handle_event(
                                    chat_widget::AppEvent::UserSubmit(text.clone()),
                                );
                                flush_chat_widget(&mut guard, &mut chat_widget, w);

                                {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    do_draw(&mut guard, lines, &mut bottom_pane)?;
                                }

                                if text.starts_with('/') {
                                    // Snapshot session id before dispatch so we
                                    // can detect when a `/resume <id>` fallback
                                    // rebinds it and trigger the replay.
                                    let pre_sid = state.session_id.clone();
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane,
                                        chat_widget: &mut chat_widget, width: w,
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
                                                    chat_widget.commit_system(history_cell::system::SystemCell::error(e));
                                                }
                                                Err(e) => {
                                                    chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Terminal restore failed: {e}")));
                                                }
                                            }
                                        }
                                    }
                                    // Flush the slash-command response
                                    // cells (`⎿ Set model to …`, etc.)
                                    // into scrollback immediately so
                                    // the reply appears under `› /cmd`
                                    // without the ~50ms tick delay.
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    // If the slash command rebound state.session_id
                                    // (resume/new-session paths), swap the
                                    // ChatWidget so its scrollback + persistence
                                    // attach to the restored session.
                                    if state.session_id != pre_sid
                                        && let Some(ref new_sid) = state.session_id
                                        && !new_sid.is_empty()
                                    {
                                        chat_widget = replay_session_into_widget(&mut guard, new_sid, w);
                                    }
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    if let Some(ref s) = state.session_id { bottom_pane.footer.session_id = Some(s[..8.min(s.len())].to_string()); }
                                    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));
                                } else {
                                    bottom_pane.set_task_status(TaskStatus::WaitingModel);
                                    let turn_start = std::time::Instant::now();
                                    let pre_prompt_tokens = state.total_prompt_tokens;
                                    let pre_completion_tokens = state.total_completion_tokens;
                                    let _pre_cost = state.total_session_cost;
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
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
                                }
                                                        }
                                                        TuiEvent::Resize | TuiEvent::Draw => {
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
                                }
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                Some(ae) = tui_rx.recv() => {
                                                    // Track per-turn metrics
                                                    match &ae {
                                                        TuiAppEvent::Token(_)
                                                            if turn_ttft.is_none() => {
                                                                turn_ttft = Some(std::time::Instant::now());
                                                            }
                                                        TuiAppEvent::ToolStarted { .. } => {
                                                            turn_tool_count += 1;
                                                        }
                                                        _ => {}
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    // Shadow mirror into ChatWidget.
                                                    // Clone the event because handle_app_event
                                                    // consumes it by value on the legacy path.
                                                    if let Some(new_ev) = chat_widget::translate(
                                                        ae.clone(),
                                                        chat_widget::TurnContext::default(),
                                                    ) {
                                                        chat_widget.handle_event(new_ev);
                                                    }
                                                    handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
                                }
                                                }
                                                Some(req) = approval_rx.recv() => {
                                                    // Non-blocking: enqueue only. The live, interactive
                                                    // approval card is rendered by BottomPane above the
                                                    // composer so arrow-key focus is visible. Resolve
                                                    // events flush a compact audit line to scrollback.
                                                    let _id = bottom_pane.enqueue_approval(
                                                        req.tool,
                                                        req.header,
                                                        req.detail,
                                                        req.reason,
                                                        req.response_tx,
                                                    );
                                                    frame_requester.schedule_frame();
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
                                }
                                                }
                                                _ = &mut itick => {
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
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
                                                if let Some(new_ev) = chat_widget::translate(
                                                    ae.clone(),
                                                    chat_widget::TurnContext::default(),
                                                ) {
                                                    chat_widget.handle_event(new_ev);
                                                }
                                                handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            }
                                        }
                                    }

                                    // Turn end — ChatWidget handles any
                                    // remaining live cell on TurnComplete.
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);

                                    bottom_pane.set_task_status(TaskStatus::Idle);
                                    status_indicator.set_state(
                                        status_indicator::IndicatorState::Idle,
                                    );
                                    // Session id may have been assigned by
                                    // the server during the turn. Re-seat
                                    // so subsequent turns persist under the
                                    // correct id.
                                    if let Some(ref sid) = state.session_id
                                        && chat_widget.session_id() != sid
                                    {
                                        chat_widget.set_session_id(sid.clone());
                                    }
                                    if let Err(ref e) = turn_result {
                                        // ChatWidget renders the error cell
                                        // into scrollback via the flush.
                                        if let Some(ev) = chat_widget::translate(
                                            TuiAppEvent::TurnError(e.clone()),
                                            chat_widget::TurnContext::default(),
                                        ) {
                                            chat_widget.handle_event(ev);
                                        }
                                    }

                                    // Update footer
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    if let Some(ref s) = state.session_id { bottom_pane.footer.session_id = Some(s[..8.min(s.len())].to_string()); }
                                    bottom_pane.footer.token_usage = Some(format!("{}↑ {}↓", state.total_prompt_tokens, state.total_completion_tokens));
                                    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));
                                    bottom_pane.footer.cost_usd = Some(state.total_session_cost);
                                    // Footer "N% (Mk)" chip shows the CONTEXT WINDOW for
                                    // the most recent turn — i.e. how many input tokens
                                    // the model saw this turn, not cumulative session
                                    // totals. Cumulative would climb to 100% within a few
                                    // turns on any non-trivial chat and the chip becomes
                                    // meaningless. The default 200k budget covers
                                    // Anthropic Opus/Sonnet 4.x; per-model limits will
                                    // land in a later pass.
                                    let turn_prompt = state.total_prompt_tokens - pre_prompt_tokens;
                                    let turn_completion = state.total_completion_tokens - pre_completion_tokens;
                                    let turn_cache_read = state.total_cache_read_tokens - pre_cache_read;
                                    let turn_cache_creation = state.total_cache_creation_tokens - pre_cache_creation;
                                    let turn_input_tokens =
                                        turn_prompt + turn_cache_read + turn_cache_creation;
                                    bottom_pane.footer.token_budget =
                                        Some((turn_input_tokens, 200_000));

                                    // Turn summary: dispatch to ChatWidget,
                                    // which builds the TurnSummaryCell and
                                    // persists it. `flush_chat_widget` below
                                    // paints it into scrollback.
                                    {
                                        let elapsed = turn_start.elapsed();
                                        let ttft_ms = turn_ttft.map(|t| {
                                            t.duration_since(turn_start).as_millis() as u64
                                        });
                                        let ctx = chat_widget::TurnContext {
                                            elapsed_ms: Some(elapsed.as_millis() as u64),
                                            ttft_ms,
                                            tokens_in: Some(turn_prompt + turn_cache_read + turn_cache_creation),
                                            tokens_out: Some(turn_completion),
                                            tools: turn_tool_count,
                                            cumulative_tokens: Some(
                                                state.total_prompt_tokens
                                                    + state.total_completion_tokens
                                                    + state.total_cache_read_tokens
                                                    + state.total_cache_creation_tokens,
                                            ),
                                            cumulative_cost_usd: Some(state.total_session_cost),
                                        };
                                        if let Some(ev) = chat_widget::translate(
                                            TuiAppEvent::TurnComplete,
                                            ctx,
                                        ) {
                                            chat_widget.handle_event(ev);
                                        }
                                    }
                                    // Flush everything new from the widget
                                    // (assistant cell + tool cells +
                                    // possibly TurnSummary + SystemError) to
                                    // scrollback in one shot.
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);

                                    let new_tok = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
                                    tui_cancel_token = new_tok.clone();
                                    state.tui_cancel_token = Some(new_tok);

                                    // Auto-send first queued message (will be picked up next iteration)
                                    inject_submit = bottom_pane.take_next_queued();
                                }
                            }
                            BottomPaneAction::ViewCompleted { result, reopen } => {
                                if let Some(name) = result {
                                    // LoginView / RegisterView completion:
                                    // credentials arrive as a sentinel-
                                    // prefixed string so we can dispatch
                                    // auth without leaving the TUI (no
                                    // more rpassword against bare terminal).
                                    if let Some(rest) = name.strip_prefix("__login__\n") {
                                        let mut parts = rest.splitn(2, '\n');
                                        let username = parts.next().unwrap_or("").to_string();
                                        let password = parts.next().unwrap_or("").to_string();
                                        match crate::auth_flow::do_login(api, profile, &username, &password).await {
                                            Ok(_) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::response(format!("Logged in as {username}")));
                                                crate::post_auth_cloud_resync(profile, &mut state).await;
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Login failed: {e}")));
                                            }
                                        }
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    if let Some(rest) = name.strip_prefix("__register__\n") {
                                        let mut parts = rest.splitn(3, '\n');
                                        let username = parts.next().unwrap_or("").to_string();
                                        let email = parts.next().unwrap_or("").to_string();
                                        let password = parts.next().unwrap_or("").to_string();
                                        match crate::auth_flow::do_register(api, &username, &email, &password).await {
                                            Ok(_) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::response("Registered — logging in…"));
                                                match crate::auth_flow::do_login(api, profile, &username, &password).await {
                                                    Ok(_) => {
                                                        chat_widget.commit_system(history_cell::system::SystemCell::response(format!("Logged in as {username}")));
                                                        crate::post_auth_cloud_resync(profile, &mut state).await;
                                                    }
                                                    Err(e) => {
                                                        chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Auto-login failed: {e}")));
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Register failed: {e}")));
                                            }
                                        }
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    // Session picker result → run the async
                                    // `/resume <id>` pipeline via the usual
                                    // slash fallback path. This is the same
                                    // code the user-typed `/resume <id>` runs
                                    // through, so the full restore logic is
                                    // exercised identically.
                                    if slash_dispatch::looks_like_session_id(&name) {
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        let pre_sid = state.session_id.clone();
                                        let slash_text = format!("/resume {name}");
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
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(e));
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Terminal restore failed: {e}")));
                                            }
                                        }
                                        // If the resume attached a new session
                                        // id, swap the ChatWidget to replay
                                        // that session's transcript. The
                                        // `replay_session_into_widget` helper
                                        // emits its own "resumed N cells"
                                        // banner — so no extra info line here.
                                        if state.session_id != pre_sid
                                            && let Some(ref new_sid) = state.session_id
                                            && !new_sid.is_empty()
                                        {
                                            chat_widget = replay_session_into_widget(&mut guard, new_sid, w);
                                        }
                                        bottom_pane.footer.session_id = state
                                            .session_id
                                            .as_ref()
                                            .map(|s| s[..8.min(s.len())].to_string());
                                    } else {
                                        slash_dispatch::handle_view_result(
                                            &name, &mut state, &mut guard, &mut bottom_pane,
                                            &mut chat_widget,
                                        );
                                    }
                                    // Flush view-driven system cells
                                    // (login success, permission change,
                                    // etc.) into scrollback without waiting
                                    // for the 50ms tick.
                                    let _w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    flush_chat_widget(&mut guard, &mut chat_widget, _w);
                                    bottom_pane.sync_popups();
                                    // Update footer after view actions (model/permission may change)
                                    if let Some(ref m) = state.model { bottom_pane.footer.model = Some(m.clone()); }
                                    bottom_pane.footer.permission_mode = Some(format!("{}", state.perm_manager.mode()));
                                } else if let Some(cmd) = reopen {
                                    // Reopen parent menu (e.g., Esc from stats detail → back to /stats menu)
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane,
                                        chat_widget: &mut chat_widget, width: w,
                                    };
                                    let _ = slash_dispatch::dispatch(&cmd, &mut dctx).await;
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                }
                            }
                            BottomPaneAction::Interrupt | BottomPaneAction::Quit => { break 'main Ok(()); }
                            BottomPaneAction::Consumed => {}
                            BottomPaneAction::Escalate(_) => {}
                            BottomPaneAction::ApprovalResolved { .. } => {
                                // BottomPane already sent the response via its
                                // oneshot; nothing else to do at the outer
                                // event loop yet.
                            }
                        }
                        frame_requester.schedule_frame();
                    }
                    TuiEvent::Resize => {
                        guard.terminal.invalidate_viewport();
                        {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    do_draw(&mut guard, lines, &mut bottom_pane)?;
                                }
                    }
                    TuiEvent::Draw => {
                        {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport_lines(&chat_widget, &status_indicator, w);
                                    do_draw(&mut guard, lines, &mut bottom_pane)?;
                                }
                    }
                    TuiEvent::Paste(text) => {
                        // BottomPane routes short pastes to the textarea
                        // verbatim and folds multi-line pastes behind a
                        // `[Pasted #N · M lines]` placeholder. The
                        // placeholder expands back to the original text
                        // on submit.
                        bottom_pane.handle_paste(&text);
                        frame_requester.schedule_frame();
                    }
                }
            }
            Some(ae) = tui_rx.recv() => {
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                if let Some(new_ev) = chat_widget::translate(
                    ae.clone(),
                    chat_widget::TurnContext::default(),
                ) {
                    chat_widget.handle_event(new_ev);
                }
                handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
            }
            _ = &mut tick => {
                // Pulse the chat-widget scrollback so if any async
                // event was handled since the last draw the new
                // cells land promptly instead of waiting for the
                // next event edge.
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                flush_chat_widget(&mut guard, &mut chat_widget, w);
            }
        }
    };
    drop(guard);
    result
}

pub(super) fn do_draw(
    guard: &mut TerminalGuard,
    active_cell_lines: Vec<ratatui::text::Line<'static>>,
    bottom_pane: &mut BottomPane,
) -> Result<(), String> {
    use render::renderable::{FlexRenderable, Renderable, RenderableItem};

    bottom_pane.pre_draw_tick(std::time::Instant::now());

    let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);

    let ac_renderable: RenderableItem<'_> = if active_cell_lines.is_empty() {
        RenderableItem::Owned(Box::new(()))
    } else {
        let text = ratatui::text::Text::from(active_cell_lines);
        let para = ratatui::widgets::Paragraph::new(text);
        // No top inset — the active cell butts up against the last
        // scrollback line. A 1-row inset previously meant the
        // running tool/thinking cell looked "floated" with a blank
        // between it and scrollback, which then snapped closed once
        // the cell flushed into scrollback (feeling janky).
        RenderableItem::Owned(Box::new(para))
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

    guard
        .draw(total_h, |frame| {
            let area = frame.area();
            Clear.render(area, frame.buffer_mut());
            flex.render(area, frame.buffer_mut());

            if let Some((x, y)) = flex.cursor_pos(area) {
                frame.set_cursor_position((x, y));
            }
        })
        .map_err(|e| format!("draw: {e}"))?;
    Ok(())
}

/// Handle a TUI app event for BOTTOM-PANE state only.
/// Scrollback mutations are handled independently by
/// `chat_widget::handle_event` via the bridge translator; this
/// function updates the task-status pill, the orbiter-equivalent
/// `StatusIndicator`, and nothing else.
fn handle_app_event(
    ev: &TuiAppEvent,
    bottom_pane: &mut BottomPane,
    status_indicator: &mut status_indicator::StatusIndicator,
    fr: &FrameRequester,
) {
    let now = std::time::Instant::now();
    match ev {
        TuiAppEvent::Token(text) => {
            // Bump the per-turn token approximation so the
            // StatusIndicator shows `↓ N tokens` climbing.
            status_indicator.bump_stream_chars(text.chars().count());
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: now });
            // Don't switch the indicator — it's set to Thinking at
            // turn start and remains "Thinking" even once tokens
            // arrive; the active_cell in ChatWidget takes over
            // rendering from here.
        }
        TuiAppEvent::ThinkingStarted => {
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ThinkingChunk(_) => {
            // ChatWidget handles the cell update; nothing to do
            // in the bottom pane. The indicator stays `Thinking`.
        }
        TuiAppEvent::ThinkingStopped => {
            // Keep the indicator active — the model may still be
            // generating the answer body. It flips to `Idle` on
            // TurnComplete / TurnError.
        }
        TuiAppEvent::WaitingForModel => {
            bottom_pane.set_task_status(TaskStatus::WaitingModel);
            status_indicator
                .set_state(status_indicator::IndicatorState::WaitingModel { started_at: now });
        }
        TuiAppEvent::ModelResponding => {
            bottom_pane.set_task_status(TaskStatus::TurnRunning { started_at: now });
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ToolStarted { name, .. } => {
            bottom_pane.set_task_status(TaskStatus::ToolExecuting {
                name: name.clone(),
                started_at: now,
            });
            status_indicator.set_state(status_indicator::IndicatorState::Tool {
                name: name.clone(),
                started_at: now,
            });
        }
        TuiAppEvent::ToolCompleted { .. } => {
            // Flip back to thinking; the ChatWidget committed the
            // tool cell in its own event handler.
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::StatusLine(_) => {}
        TuiAppEvent::TurnComplete | TuiAppEvent::TurnError(_) => {
            bottom_pane.set_task_status(TaskStatus::Idle);
            status_indicator.set_state(status_indicator::IndicatorState::Idle);
        }
    }
    fr.schedule_frame();
}

use ratatui::widgets::Widget;

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
