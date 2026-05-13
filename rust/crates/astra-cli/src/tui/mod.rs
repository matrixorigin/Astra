#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

mod app_event;
mod approval;
mod bottom_pane;
mod config_edit_router;
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

/// What the active-cell area should render this frame. Encodes the
/// visual-hierarchy grammar that distinguishes the three layers the
/// user sees at any moment:
///
/// - **Settled** (not represented here) — committed `HistoryCell`s
///   already painted to terminal scrollback. Flat, no border.
/// - **Active** — something's happening right now. Rendered inside
///   a bordered box in the viewport so it's visually distinct from
///   scrollback. `ActiveKind` picks the border colour by what's
///   running: blue for a tool, pink for assistant streaming,
///   dim-grey for a bare reasoning preview.
/// - **Status** — a one-line indicator (`✶ Thinking …`) when we
///   have a turn in flight but no cell content yet. No border —
///   the cue is the spinner, not the frame.
pub(crate) enum ActiveView {
    Empty,
    Status(ratatui::text::Line<'static>),
    Active {
        kind: ActiveKind,
        lines: Vec<ratatui::text::Line<'static>>,
        /// `true` while the cell is still streaming — enables the
        /// flowing-gradient border animation. `false` for rare cases
        /// where a finalized cell lingers in the active slot (never
        /// happens in practice, but the flag keeps the render path
        /// honest).
        live: bool,
    },
}

/// Pick the right border colour and title for the active cell.
/// Mirrors the cell-type → palette mapping used elsewhere: tool
/// output = blue (Cursor-style), assistant body = pink (the gutter
/// colour). Reasoning gets the dim palette so thinking content
/// doesn't visually compete with the tool / assistant it surrounds.
pub(crate) enum ActiveKind {
    Tool,
    Assistant,
    Reasoning,
}

/// Classify the active cell. `None` when the slot is empty.
fn classify_active(cell: &dyn history_cell::HistoryCell) -> Option<ActiveKind> {
    let any = cell.as_any_ref();
    if any.is::<history_cell::tool::ToolCell>() {
        Some(ActiveKind::Tool)
    } else if any.is::<history_cell::assistant::AssistantCell>() {
        Some(ActiveKind::Assistant)
    } else if any.is::<history_cell::reasoning::ReasoningCell>() {
        Some(ActiveKind::Reasoning)
    } else {
        None
    }
}

/// Build the active-view description for the current frame. Order:
///
/// 1. `active_cell` present → `Active` with lines + kind so the
///    caller can draw a bordered frame.
/// 2. No active cell but the status indicator has content →
///    `Status` line (spinner + short label, no frame).
/// 3. Neither → `Empty`. Idle REPL shows nothing above the
///    composer.
fn active_viewport(
    chat_widget: &chat_widget::ChatWidget,
    status: &status_indicator::StatusIndicator,
    width: u16,
) -> ActiveView {
    if let Some(cell) = chat_widget.active_cell() {
        // Reserve 2 cols for the frame border + 2 for padding.
        let inner_w = width.saturating_sub(4).max(20);
        let lines = cell.display_lines(inner_w);
        if !lines.is_empty() {
            let kind = classify_active(cell).unwrap_or(ActiveKind::Assistant);
            let live = cell.is_live();
            return ActiveView::Active { kind, lines, live };
        }
    }
    if let Some(line) = status.render() {
        return ActiveView::Status(line);
    }
    ActiveView::Empty
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
    // blank for visual separation. Response cells (`⎿ Set model to …`)
    // want to hug the `› /cmd` line above —
    // both when paired in the same batch (no blank between them)
    // and when the UserCell flushed in an earlier event (the
    // picker-return path). For the former we detect the pair here
    // and skip its separator; for the latter we also skip the
    // response's OWN leading and trailing blanks so the reply
    // stacks tight onto the previous flush's `› /cmd`.
    let mut batch: Vec<ratatui::text::Line<'static>> = Vec::new();
    for cell in new_cells.iter() {
        batch.extend(cell.display_lines(width));
        let this_is_slash_user = is_slash_user_cell(cell.as_ref());
        let this_is_response = is_response_cell(cell.as_ref());

        // Skip the trailing blank when:
        //   1. This cell is a slash UserCell — always hugs the response
        //      (response may arrive in same batch or next event).
        //   2. This cell is a response — stacks tight, no air below.
        let suppress_blank = this_is_slash_user || this_is_response;
        if !suppress_blank {
            batch.push(ratatui::text::Line::default());
        }
    }
    guard.queue_history_lines(batch);
}

/// Detect a `SystemLevel::Response` cell (the `⎿`-prefixed kind).
/// Used by `flush_chat_widget` to omit the usual trailing blank so
/// the response hugs the `› /cmd` line above it.
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

/// Walk the chat widget's committed history and emit role/text
/// pairs for the `/context dump` JSON file.  Kept here (rather
/// than in `cli::context_dump`) because `history_cell` is a
/// private TUI module — only this crate's TUI layer should
/// downcast cells to concrete types.
pub(crate) fn collect_chat_turns_for_dump(
    chat: &chat_widget::ChatWidget,
) -> Vec<crate::context_dump::ChatTurnDump> {
    use crate::context_dump::ChatTurnDump;
    use history_cell::{
        assistant::AssistantCell, reasoning::ReasoningCell, system::SystemCell, user::UserCell,
    };
    let mut out = Vec::new();
    for cell in chat.history() {
        let any = cell.as_any_ref();
        if let Some(u) = any.downcast_ref::<UserCell>() {
            out.push(ChatTurnDump {
                role: "user".into(),
                text: u.text().to_string(),
            });
        } else if let Some(a) = any.downcast_ref::<AssistantCell>() {
            out.push(ChatTurnDump {
                role: "assistant".into(),
                text: a.source().to_string(),
            });
        } else if let Some(r) = any.downcast_ref::<ReasoningCell>() {
            out.push(ChatTurnDump {
                role: "reasoning".into(),
                text: r.text().to_string(),
            });
        } else if any.downcast_ref::<SystemCell>().is_some() {
            // System cells are UI chrome — skip to keep the dump
            // focused on what the LLM actually consumed.
        }
    }
    out
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
                subcommands: m.subcommands,
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
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
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
                                                            //
                                                            // Exception: if an approval is pending, Up
                                                            // belongs to the approval button row — the
                                                            // user is trying to pick a button, not edit
                                                            // a queued message. Without this guard, the
                                                            // queued-message edit path swallows arrow
                                                            // keys while the approval cell is focused
                                                            // and the user ends up stuck on the first
                                                            // button (Accept).
                                                            if k.code == crossterm::event::KeyCode::Up
                                                                && !bottom_pane.has_pending_approvals()
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
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
                                }
                                                        }
                                                        TuiEvent::Resize | TuiEvent::Draw => {
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
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
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
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
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
                                    let _ = do_draw(&mut guard, lines, &mut bottom_pane);
                                }
                                                }
                                                _ = &mut itick => {
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
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
                                            // Drive the `💾 N%` segment:
                                            // hit rate = cache_read / total_input.
                                            // Only plumbed when the provider
                                            // reported a cache_read value this
                                            // turn — `None` keeps the segment
                                            // off entirely (first turn, non-
                                            // caching provider, etc.).
                                            cache_read_tokens: (turn_cache_read > 0)
                                                .then_some(turn_cache_read),
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
                                    // /config edit completion. Token format:
                                    //   __config_edit__\n<action>\n<toml-body>
                                    // Actions: save_user | save_project | discard | cancel.
                                    // save_* writes the TOML to the target scope
                                    // AND refreshes the process-wide overlay so
                                    // the new values take effect for the next turn.
                                    if let Some(rest) = name.strip_prefix("__config_edit__\n") {
                                        let mut parts = rest.splitn(2, '\n');
                                        let action = parts.next().unwrap_or("").to_string();
                                        let toml_body = parts.next().unwrap_or("").to_string();
                                        let result = crate::tui::config_edit_router::finalize(
                                            &action,
                                            &toml_body,
                                        );
                                        let msg = match result {
                                            Ok(outcome) => {
                                                // If the save produced a new version id,
                                                // emit a ConfigChange journal event
                                                // recording the transition and update
                                                // ReplState so subsequent HeavyCheckpoints
                                                // carry the new pointer.
                                                if let Some(save) = outcome.save.as_ref() {
                                                    let prev = state.config_version_id.clone();
                                                    if let (Some(ref j), Some(ref sid)) = (
                                                        state.journal.as_ref(),
                                                        state.session_id.as_ref(),
                                                    ) {
                                                        let ev = astra_services::session_journal::JournalEvent::config_version_change(
                                                            Some(sid.as_str()),
                                                            state.turn,
                                                            prev.as_deref(),
                                                            &save.new_version_id,
                                                            save.source,
                                                        );
                                                        let _ = j.append(&ev);
                                                    }
                                                    state.config_version_id =
                                                        Some(save.new_version_id.clone());

                                                    // Step 4b: cloud push. Best-effort
                                                    // — if matrix_runtime is None (no
                                                    // cloud configured) or the
                                                    // ingestion worker is gone, we
                                                    // degrade to local-only. The
                                                    // version will sync next time the
                                                    // CLI runs with cloud available,
                                                    // via the same content-addressed
                                                    // id.
                                                    if let Some(ref mc) =
                                                        state.matrix_runtime
                                                    {
                                                        let user_id = state
                                                            .ingestion_user_id
                                                            .clone()
                                                            .unwrap_or_else(|| {
                                                                "anonymous".to_string()
                                                            });
                                                        let row = astra_services::config_version_cloud::ConfigVersionRow {
                                                            version_id: save.new_version_id.clone(),
                                                            user_id,
                                                            toml_body: save.toml_body.clone(),
                                                            created_at_ms: chrono::Utc::now().timestamp_millis(),
                                                            first_seen_session: state.session_id.clone(),
                                                        };
                                                        mc.enqueue_config_version_push(&row);
                                                    }
                                                }
                                                history_cell::system::SystemCell::response(
                                                    outcome.message,
                                                )
                                            }
                                            Err(e) => history_cell::system::SystemCell::error(e),
                                        };
                                        chat_widget.commit_system(msg);
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    // `/model` picker accepted a model name.
                                    // If the model advertises a thinking
                                    // capability, push the second-level
                                    // thinking-mode picker; otherwise commit
                                    // the base name as-is.
                                    if let Some(base_model) =
                                        name.strip_prefix(slash_dispatch::MODEL_PICK_SENTINEL)
                                    {
                                        let base_model = base_model.to_string();
                                        let token = crate::repl_runtime::current_access_token(profile);
                                        let raw = crate::slash_router::fetch_model_list_raw(
                                            api,
                                            token.as_deref(),
                                        )
                                        .await
                                        .unwrap_or_default();
                                        let entry = crate::slash_router::find_model_entry_by_name(
                                            &raw,
                                            &base_model,
                                        );
                                        let thinking_cap = entry
                                            .and_then(crate::slash_router::entry_thinking_capability);
                                        let provider =
                                            entry.and_then(crate::slash_router::entry_provider);
                                        let opts = astra_turn_core::thinking_config::thinking_options_with_capability(
                                            &base_model,
                                            provider,
                                            thinking_cap,
                                        );
                                        if opts.is_empty() {
                                            // Model doesn't think — commit
                                            // the base name directly.
                                            state.model = Some(base_model.clone());
                                            crate::slash_config::set_active_model_for_display(
                                                Some(base_model.clone()),
                                            );
                                            bottom_pane.footer.model = Some(base_model.clone());
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::response(
                                                    format!("Set model to {base_model}"),
                                                ),
                                            );
                                        } else {
                                            use crate::tui::bottom_pane::list_selection_view::{
                                                ListSelectionView, SelectionItem,
                                            };
                                            let items: Vec<SelectionItem> = opts
                                                .iter()
                                                .map(|o| SelectionItem {
                                                    name: o.label.to_string(),
                                                    description: None,
                                                    is_current: o.is_default,
                                                })
                                                .collect();
                                            let prefix = format!(
                                                "{}{}\n",
                                                slash_dispatch::MODEL_THINKING_SENTINEL,
                                                base_model,
                                            );
                                            let view = ListSelectionView::new(
                                                items,
                                                Some(format!(
                                                    "Select thinking mode for {base_model}:",
                                                )),
                                            )
                                            .with_result_prefix(prefix);
                                            bottom_pane.push_view(Box::new(view));
                                        }
                                        let w = guard
                                            .terminal
                                            .size()
                                            .map(|s| s.width)
                                            .unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // `/model` thinking-mode picker accepted a
                                    // label. Compose the final model id with
                                    // the appropriate suffix and commit.
                                    if let Some(rest) =
                                        name.strip_prefix(slash_dispatch::MODEL_THINKING_SENTINEL)
                                    {
                                        let mut parts = rest.splitn(2, '\n');
                                        let base_model =
                                            parts.next().unwrap_or("").to_string();
                                        let label = parts.next().unwrap_or("").to_string();
                                        let token = crate::repl_runtime::current_access_token(profile);
                                        let raw = crate::slash_router::fetch_model_list_raw(
                                            api,
                                            token.as_deref(),
                                        )
                                        .await
                                        .unwrap_or_default();
                                        let entry = crate::slash_router::find_model_entry_by_name(
                                            &raw,
                                            &base_model,
                                        );
                                        let provider =
                                            entry.and_then(crate::slash_router::entry_provider);
                                        let thinking_cap = entry
                                            .and_then(crate::slash_router::entry_thinking_capability);
                                        let opts = astra_turn_core::thinking_config::thinking_options_with_capability(
                                            &base_model,
                                            provider,
                                            thinking_cap,
                                        );
                                        let suffix_opt = opts
                                            .iter()
                                            .find(|o| o.label == label)
                                            .map(|o| astra_turn_core::thinking_config::thinking_suffix_for(&o.config));
                                        let suffix = match suffix_opt {
                                            Some(s) => s,
                                            None => {
                                                // Model catalog shifted between the
                                                // picker's two `fetch_model_list_raw`
                                                // calls (or the server returned fewer
                                                // thinking options) — the chosen label
                                                // no longer maps to a suffix.  Warn
                                                // instead of silently committing the
                                                // bare model name, which would leave
                                                // the user on a different thinking mode
                                                // than they selected.
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(format!(
                                                        "Thinking mode `{label}` is no longer available for {base_model}; model unchanged. Re-open the picker with `/model`."
                                                    )),
                                                );
                                                let w = guard
                                                    .terminal
                                                    .size()
                                                    .map(|s| s.width)
                                                    .unwrap_or(80);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                bottom_pane.sync_popups();
                                                frame_requester.schedule_frame();
                                                continue;
                                            }
                                        };
                                        let composed = format!("{base_model}{suffix}");
                                        state.model = Some(composed.clone());
                                        crate::slash_config::set_active_model_for_display(
                                            Some(composed.clone()),
                                        );
                                        bottom_pane.footer.model = Some(composed.clone());
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::response(format!(
                                                "Set model to {composed}"
                                            )),
                                        );
                                        let w = guard
                                            .terminal
                                            .size()
                                            .map(|s| s.width)
                                            .unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // `/session fork` picker → hand off to
                                    // the line-mode `/session fork <parent>`
                                    // pipeline. Calling `fork_local_session`
                                    // inline here used to leave ReplState in
                                    // a half-done state (session_id /
                                    // journal / CSL all still pointed at the
                                    // parent); the fallback path is the same
                                    // code the line-mode handler runs
                                    // through, so it does the full restore.
                                    if let Some(parent_sid) = name.strip_prefix(slash_dispatch::FORK_PICK_SENTINEL) {
                                        let slash_text = format!("/session fork {parent_sid}");
                                        let slash_result = guard
                                            .with_restored(|| async {
                                                let tok = crate::repl_runtime::current_access_token(
                                                    profile,
                                                );
                                                crate::slash_router::handle_slash_command(
                                                    &slash_text,
                                                    api,
                                                    profile,
                                                    &mut state,
                                                    tok.as_deref(),
                                                    &*startup.selector,
                                                )
                                                .await
                                            })
                                            .await;
                                        match slash_result {
                                            Ok(Ok(true)) => {
                                                break 'main Ok(());
                                            }
                                            Ok(Ok(false)) => {}
                                            Ok(Err(e)) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(e),
                                                );
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(
                                                        format!("Terminal restore failed: {e}"),
                                                    ),
                                                );
                                            }
                                        }
                                        // Post-fork: the line handler may have
                                        // swapped `state.session_id`; refresh
                                        // the footer + ChatWidget replay so
                                        // the user lands on the child session.
                                        bottom_pane.footer.session_id = state
                                            .session_id
                                            .as_ref()
                                            .map(|s| s[..8.min(s.len())].to_string());
                                        let w = guard
                                            .terminal
                                            .size()
                                            .map(|s| s.width)
                                            .unwrap_or(80);
                                        if let Some(ref new_sid) = state.session_id
                                            && !new_sid.is_empty()
                                        {
                                            chat_widget = replay_session_into_widget(
                                                &mut guard,
                                                new_sid,
                                                w,
                                            );
                                        }
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
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
                                            &name,
                                            &mut state,
                                            &mut bottom_pane,
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
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
                                    do_draw(&mut guard, lines, &mut bottom_pane)?;
                                }
                    }
                    TuiEvent::Draw => {
                        {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let lines = active_viewport(&chat_widget, &status_indicator, w);
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
                // If a cell is streaming, request a redraw so the
                // gradient-border animation on `LiveFramedCell` keeps
                // flowing. Without this the frame only redraws on
                // incoming delta/state events and freezes visually
                // between them.
                if chat_widget
                    .active_cell()
                    .is_some_and(|c| c.is_live())
                {
                    frame_requester.schedule_frame();
                }
            }
        }
    };
    drop(guard);
    result
}

/// A rounded-frame renderable whose border characters carry a
/// time-varying gradient — one colour per cell, sweeping around the
/// perimeter. Used in place of a plain `Block`-wrapped paragraph while
/// the active cell is still streaming, so the user sees the frame
/// "breathing" and immediately knows output isn't frozen.
///
/// On freeze (`live == false`) the border collapses to a solid colour
/// chosen by the cell kind — matches the pre-animation behaviour. The
/// static pink `┃ ` gutter used in scrollback is unrelated to this
/// frame; it's painted by `render_body_with_gutter` only after the
/// cell leaves the active slot.
struct LiveFramedCell {
    lines: Vec<ratatui::text::Line<'static>>,
    title: &'static str,
    /// Border colour when NOT live (or fallback for non-truecolor
    /// terminals).
    solid_color: ratatui::style::Color,
    live: bool,
}

impl render::renderable::Renderable for LiveFramedCell {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{Paragraph, Widget};

        if area.width < 2 || area.height < 2 {
            return;
        }

        // Inner paragraph area (leave 1 cell on each side for border).
        let inner = ratatui::layout::Rect {
            x: area.x + 1,
            y: area.y + 1,
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };

        // Paint inner content first (border is drawn over edge cells
        // below, which the paragraph never touches).
        let para = Paragraph::new(ratatui::text::Text::from(self.lines.clone()));
        Widget::render(para, inner, buf);

        // Draw the rounded border character by character, assigning
        // each cell a gradient colour when live.
        let x0 = area.x;
        let y0 = area.y;
        let x1 = area.x + area.width - 1;
        let y1 = area.y + area.height - 1;

        let perimeter = 2 * (area.width as usize + area.height as usize - 2);
        let left_height = area.height.saturating_sub(2) as usize;
        // Sweep once around the perimeter every N seconds. Slow enough
        // that the eye reads "flowing", fast enough that it isn't stuck.
        let period = 3.0_f32;

        let color_at = |idx: usize| -> ratatui::style::Color {
            if !self.live {
                return self.solid_color;
            }
            let (r, g, b) = shimmer::gradient_color_at(idx, perimeter, period);
            ratatui::style::Color::Rgb(r, g, b)
        };

        // Left edge uses a dedicated top-to-bottom gradient sweep:
        // position along the left bar drives hue, time adds a
        // downward-flowing phase. Gives a "color falling down the
        // gutter" effect while running.
        let left_color_at = |row: usize| -> ratatui::style::Color {
            if !self.live {
                return self.solid_color;
            }
            let len = left_height.max(1);
            let (r, g, b) = shimmer::gradient_color_at(row, len, period);
            ratatui::style::Color::Rgb(r, g, b)
        };

        let mut idx: usize = 0;
        // Top edge: ╭ ── ╮
        set_char(buf, x0, y0, '╭', left_color_at(0));
        idx += 1;
        for x in (x0 + 1)..x1 {
            set_char(buf, x, y0, '─', color_at(idx));
            idx += 1;
        }
        set_char(buf, x1, y0, '╮', color_at(idx));
        idx += 1;
        // Right edge
        for y in (y0 + 1)..y1 {
            set_char(buf, x1, y, '│', color_at(idx));
            idx += 1;
        }
        // Bottom edge: ╰ ── ╯ (traverse right-to-left to keep the
        // gradient continuous around the perimeter)
        set_char(buf, x1, y1, '╯', color_at(idx));
        idx += 1;
        for x in ((x0 + 1)..x1).rev() {
            set_char(buf, x, y1, '─', color_at(idx));
            idx += 1;
        }
        set_char(
            buf,
            x0,
            y1,
            '╰',
            left_color_at(left_height.saturating_sub(1)),
        );
        idx += 1;
        // Left edge (top → bottom) with vertical gradient
        for (row, y) in ((y0 + 1)..y1).enumerate() {
            set_char(buf, x0, y, '│', left_color_at(row));
        }
        idx += left_height;
        let _ = idx;

        // Title overlay (dim, on top border). Uses the solid colour so
        // the label stays legible against the animated border.
        let title = format!(" {} ", self.title.trim());
        let title_span = Span::styled(
            title.clone(),
            Style::default()
                .fg(self.solid_color)
                .add_modifier(Modifier::DIM),
        );
        // Anchor title at x0 + 2 so it doesn't overlap the corner.
        let title_x = x0 + 2;
        if title_x + title.chars().count() as u16 <= x1 {
            let line_widget = Line::from(title_span);
            let line_area = ratatui::layout::Rect {
                x: title_x,
                y: y0,
                width: title.chars().count() as u16,
                height: 1,
            };
            ratatui::widgets::WidgetRef::render_ref(&line_widget, line_area, buf);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        // border(2) + content lines
        (self.lines.len() as u16).saturating_add(2)
    }
}

/// Write a single character cell into the buffer with the given fg.
fn set_char(
    buf: &mut ratatui::buffer::Buffer,
    x: u16,
    y: u16,
    ch: char,
    fg: ratatui::style::Color,
) {
    if x >= buf.area.x + buf.area.width || y >= buf.area.y + buf.area.height {
        return;
    }
    let cell = &mut buf[(x, y)];
    cell.set_char(ch);
    cell.set_style(ratatui::style::Style::default().fg(fg));
}

pub(super) fn do_draw(
    guard: &mut TerminalGuard,
    active: ActiveView,
    bottom_pane: &mut BottomPane,
) -> Result<(), String> {
    use ratatui::widgets::Paragraph;
    use render::renderable::{FlexRenderable, Renderable, RenderableItem};

    bottom_pane.pre_draw_tick(std::time::Instant::now());

    let width = guard.terminal.size().map(|s| s.width).unwrap_or(80);

    let ac_renderable: RenderableItem<'_> = match active {
        ActiveView::Empty => RenderableItem::Owned(Box::new(())),
        // Status line (spinner + "Thinking…") renders flush with
        // scrollback — no frame, the spinner itself carries the
        // "something's happening" signal.
        ActiveView::Status(line) => {
            let para = Paragraph::new(ratatui::text::Text::from(vec![line]));
            RenderableItem::Owned(Box::new(para))
        }
        // Active cell gets a rounded bordered box in a colour that
        // matches the cell kind, so the user sees "this is the live
        // thing" at a glance — as opposed to the flat scrollback
        // above. While `live` (still streaming), the frame pulses
        // through a flowing gradient; once finalized the border
        // collapses to a solid colour. Cursor/Kiro style.
        ActiveView::Active { kind, lines, live } => {
            let theme = crate::tui::theme::current();
            let (solid_color, title) = match kind {
                ActiveKind::Tool => (theme.accent, "tool"),
                ActiveKind::Assistant => (theme.gutter, "assistant"),
                ActiveKind::Reasoning => (theme.dim, "thinking"),
            };
            let framed = LiveFramedCell {
                lines,
                title,
                solid_color,
                live,
            };
            RenderableItem::Owned(Box::new(framed))
        }
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
        TuiAppEvent::ToolOutput { .. } => {
            // Progress ticks are handled by the ChatWidget path
            // (updates active ToolCell counters). No bottom-pane or
            // indicator state change.
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
