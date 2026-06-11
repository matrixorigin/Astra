//! TUI outer event loop.
//!
//! Owns [`run_tui_session`] — the entry point that ratatui mode
//! runs under for the lifetime of the interactive session. The loop:
//!
//! 1. Completes business bootstrap (auth, state, task stores,
//!    startup trace) BEFORE entering TUI so startup errors still
//!    land in normal stderr.
//! 2. Installs [`stream_bridge`]'s channels so SSE events from the
//!    chat host flow into the TUI as [`TuiAppEvent`]s.
//! 3. Seeds a [`ChatWidget`], [`BottomPane`], [`StatusIndicator`],
//!    and [`TaskBoardObserver`].
//! 4. Runs a `tokio::select!` over: keyboard events, draw ticks,
//!    approval requests, and mid-turn app events.
//!
//! The draw pipeline lives in `super::draw`; priority is
//! `Active > TaskBoard > Status > NextHint > Empty`.

use std::{sync::Arc, time::Duration};

#[cfg(test)]
use astra_services::session_journal::ToolCallRecord;
use astra_services::session_journal::{JournalEvent, JournalEventType};
use astra_turn_core::context_assembly_trace::ContextAssemblyTrace;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;

use crate::explain_dag::{ExplainTurnMeta, render_explain_dag};

use super::app_event::TuiAppEvent;
use super::bottom_pane::{BottomPane, BottomPaneAction};
use super::chat_widget::UserEvent;
use super::draw::{active_viewport, do_draw};
use super::event::{TuiEvent, TuiEventStream};
use super::frame_requester::FrameRequester;
use super::history_cell::HistoryCell;
use super::keymap::{AppAction, AppKeymap};
use super::render::line_utils::sanitize_lines_for_terminal;
use super::task_status::TaskStatus;
use super::terminal::TerminalGuard;

use super::agent_view::*;
use super::bg_task_proxy::*;
use super::bg_task_rendering::*;
use super::plan_mode::*;
#[cfg(test)]
use super::status_line;
use super::{
    bottom_pane, chat_widget, history_cell, mention_menu, resume_summary, slash_dispatch,
    slash_menu, status_indicator, stream_bridge, task_board_observer, ui_adapter,
};

const AGENT_DRILLDOWN_RECENT_COMPLETED: usize = 5;
const WORKSPACE_TRUST_SENTINEL: &str = "__workspace_trust__\n";
const DEFERRED_INPUT_APPLIED_PREFIX: &str = "__deferred_input_applied__:";

fn ctrl_b_promoted_agent_message(agent_id: &str, description: &str) -> String {
    let description = description.trim();
    if description.is_empty() {
        format!("Backgrounded agent {agent_id}. Opened background tasks.")
    } else {
        format!("Backgrounded agent {agent_id} ({description}). Opened background tasks.")
    }
}

fn should_show_ctrl_b_background_hint(detach_ready: bool) -> bool {
    detach_ready
}

fn visible_bash_tool_is_running(status_indicator: &status_indicator::StatusIndicator) -> bool {
    matches!(
        status_indicator.state(),
        status_indicator::IndicatorState::Tool { name, .. } if name == "bash"
    )
}

fn is_background_task_manage_key(key: &crossterm::event::KeyEvent) -> bool {
    key.code == crossterm::event::KeyCode::Down
        && key
            .modifiers
            .contains(crossterm::event::KeyModifiers::SHIFT)
}

fn set_bash_background_hint_enabled(
    chat_widget: &mut chat_widget::ChatWidget,
    status_indicator: &mut status_indicator::StatusIndicator,
    enabled: bool,
) {
    chat_widget.set_bash_background_hint_enabled(enabled);
    status_indicator.set_bash_background_hint_enabled(enabled);
}

async fn install_bash_detach_listener(
    slot: &astra_tools::detach::DetachShellSlot,
    chat_widget: &mut chat_widget::ChatWidget,
    status_indicator: &mut status_indicator::StatusIndicator,
) -> astra_tools::detach::DetachShellListener {
    let (handle, listener) = astra_tools::detach::new_detach_pair();
    *slot.lock().await = Some(handle);
    set_bash_background_hint_enabled(chat_widget, status_indicator, false);
    listener
}

fn bash_detach_hint_enabled(
    listener: Option<&astra_tools::detach::DetachShellListener>,
    _status_indicator: &status_indicator::StatusIndicator,
) -> bool {
    let Some(listener) = listener else {
        return false;
    };
    should_show_ctrl_b_background_hint(listener.is_active())
}

type BashDetachHandoffResult = Result<astra_tools::detach::DetachedShellPayload, String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReopenTarget {
    Agents,
}

impl ReopenTarget {
    const AGENTS: &'static str = "agents";

    fn as_str(self) -> &'static str {
        match self {
            Self::Agents => Self::AGENTS,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            Self::AGENTS => Some(Self::Agents),
            _ => None,
        }
    }
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
    let batch = render_history_batch_lines(&new_cells, width);
    guard.queue_history_lines(batch);
}

fn deferred_input_preview(text: &str) -> String {
    let single_line = text.trim().replace('\n', " ↩ ");
    let mut preview: String = single_line.chars().take(120).collect();
    if single_line.chars().count() > 120 {
        preview.push_str("...");
    }
    preview
}

async fn submit_deferred_tui_input(
    run_control: &std::sync::Arc<
        std::sync::Mutex<
            Option<
                std::sync::Arc<crate::cli::turn::local_run_control::LocalDeferredInputRunControl>,
            >,
        >,
    >,
    text: &str,
) -> Result<(), String> {
    let provider = astra_core::sync_poison::recover_mutex_lock(run_control)
        .clone()
        .ok_or_else(|| {
            "Current turn is not ready to accept deferred input yet. Press Ctrl+C to interrupt immediately."
                .to_string()
        })?;
    provider.enqueue_text(text)
}

fn render_history_batch_lines(
    cells: &[Arc<dyn history_cell::HistoryCell>],
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    // Batch layout: each cell renders its lines then gets spacing based on
    // its transcript role. Slash command pairs stay tight, compact notes keep
    // a single blank, and primary content blocks get a little more air.
    let mut batch: Vec<ratatui::text::Line<'static>> = Vec::new();
    for (idx, cell) in cells.iter().enumerate() {
        batch.extend(cell.display_lines(width));
        let next = cells.get(idx + 1).map(|next| next.as_ref());
        for _ in 0..history_cell::separator_rows_after(cell.as_ref(), next) {
            batch.push(ratatui::text::Line::default());
        }
    }
    batch
}

fn render_transcript_view_lines(
    chat_widget: &chat_widget::ChatWidget,
    width: u16,
) -> Vec<ratatui::text::Line<'static>> {
    let mut lines: Vec<ratatui::text::Line<'static>> = Vec::new();
    let history = chat_widget.history();
    for (idx, cell) in history.iter().enumerate() {
        lines.extend(sanitize_lines_for_terminal(cell.display_lines(width)));
        let next = history
            .get(idx + 1)
            .map(|next| next.as_ref())
            .or_else(|| chat_widget.active_cell());
        for _ in 0..history_cell::separator_rows_after(cell.as_ref(), next) {
            lines.push(ratatui::text::Line::default());
        }
    }
    if let Some(active) = chat_widget.active_cell() {
        lines.extend(sanitize_lines_for_terminal(active.display_lines(width)));
        for _ in 0..history_cell::trailing_blank_rows(active) {
            lines.push(ratatui::text::Line::default());
        }
    }
    while lines.last().is_some_and(|line| line.spans.is_empty()) {
        lines.pop();
    }
    lines
}

fn surface_status_line_system_cell(event: &TuiAppEvent, chat_widget: &mut chat_widget::ChatWidget) {
    match event {
        TuiAppEvent::PermissionAutoApproved { tool, reason } => {
            chat_widget.commit_system(history_cell::system::SystemCell::info(
                astra_turn_core::permission::notice::format_auto_approved_permission(tool, reason)
                    .trim()
                    .to_string(),
            ));
        }
        TuiAppEvent::StatusLine(text) => {
            if let Some(message) = text.strip_prefix(DEFERRED_INPUT_APPLIED_PREFIX) {
                chat_widget.commit_deferred_user(message.trim().to_string());
            }
        }
        _ => {}
    };
}

fn context_trace_count(state: &crate::cli::session::session_state::SessionState) -> usize {
    state
        .observability_session
        .as_ref()
        .map(|session| {
            let guard = astra_core::sync_poison::recover_rwlock_read(&session);
            guard.context_traces.len()
        })
        .unwrap_or(0)
}

fn total_input_tokens(
    fresh_prompt_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
) -> u64 {
    fresh_prompt_tokens
        .saturating_add(cache_read_tokens)
        .saturating_add(cache_creation_tokens)
}

fn latest_context_trace_since(
    state: &crate::cli::session::session_state::SessionState,
    baseline_cached_turn_id: Option<&str>,
    baseline_count: usize,
) -> Option<ContextAssemblyTrace> {
    if let Some(trace) = state.latest_context_assembly_trace.as_ref()
        && baseline_cached_turn_id != Some(trace.turn_id.as_str())
    {
        return Some(trace.clone());
    }
    let session = state.observability_session.as_ref()?;
    let guard = astra_core::sync_poison::recover_rwlock_read(&session);
    (guard.context_traces.len() > baseline_count)
        .then(|| guard.context_traces.last().cloned())
        .flatten()
}

fn current_turn_event(
    state: &crate::cli::session::session_state::SessionState,
) -> Option<&JournalEvent> {
    state.last_turn_event.as_ref().filter(|event| {
        event.event_type == JournalEventType::Turn && event.turn == Some(state.turn)
    })
}

fn commit_explain_dag(
    state: &crate::cli::session::session_state::SessionState,
    explain_items: &[serde_json::Value],
    baseline_cached_turn_id: Option<&str>,
    baseline_context_traces: usize,
    chat_widget: &mut chat_widget::ChatWidget,
) -> bool {
    if state.explain == crate::cli::session::session_state::ExplainMode::Off {
        return false;
    }
    let trace = latest_context_trace_since(state, baseline_cached_turn_id, baseline_context_traces);
    let turn_event = current_turn_event(state);
    let meta = turn_event.map(ExplainTurnMeta::from_journal_event);
    let Some(text) = render_explain_dag(
        trace.as_ref(),
        meta.as_ref(),
        explain_items,
        state.explain == crate::cli::session::session_state::ExplainMode::Verbose,
    ) else {
        return false;
    };
    chat_widget.commit_system(history_cell::system::SystemCell::info(text));
    true
}

/// Try to dispatch a kill sentinel emitted by the InFlightAgentsView.
///
/// Returns `true` when the sentinel matched (caller should `continue`
/// the dispatch loop). Takes only the spawner + task service handles
/// (Arcs) so the call site doesn't need a mutable session-state borrow
/// — call sites that already hold &mut state can pass clones, call
/// sites inside async blocks can pre-clone before awaiting.
///
/// Looks at the spawner first — that's the canonical kill path for
/// `agent(action='spawn')`-style children — and ALSO fires the durable-task
/// service path for task-backed children. Both calls are
/// fire-and-forget so the UI doesn't block on a hung backend; the
/// spawner + task_service both honor cooperative cancel and will
/// eventually surface a terminal status the chat strip refreshes
/// against.
fn try_dispatch_agent_kill_sentinel(
    sentinel: &str,
    spawner: Option<std::sync::Arc<astra_runtime::orchestration::DynamicAgentSpawner>>,
    task_service: Option<std::sync::Arc<dyn astra_services::TaskService>>,
    chat_widget: &mut chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    frame_requester: &FrameRequester,
) -> bool {
    let Some(agent_id) = bottom_pane::in_flight_agents_view::parse_kill_sentinel(sentinel) else {
        return false;
    };
    let agent_id_owned = agent_id.to_string();

    // Mark the row Cancelling immediately so the user gets feedback even
    // if the actual cancel takes a moment to land.
    chat_widget.mark_agent_controls_cancelling(std::slice::from_ref(&agent_id_owned));

    let mut dispatched = false;
    if let Some(spawner) = spawner {
        let aid = agent_id_owned.clone();
        tokio::spawn(async move {
            // The spawner ignores unknown ids, so this is safe even if
            // the agent already finished between the user pressing x
            // and us dispatching. Reason text shows up in the journal.
            let _ = spawner
                .cancel_agent(&aid, "user-requested via Ctrl+G x")
                .await;
        });
        dispatched = true;
    }
    if let Some(task_service) = task_service {
        // Durable-task children: also try the task-service path. If the
        // id is dynamic-only this is a no-op error (which we log, not
        // user-facing); if it's a durable task this is the only path
        // that actually marks it Cancelled in MatrixOne.
        let aid = agent_id_owned.clone();
        tokio::spawn(async move {
            if let Err(e) = task_service
                .update_status(&aid, astra_services::TaskStatus::Cancelled)
                .await
            {
                tracing::debug!(
                    target: "astra_cli::tui",
                    task_id = %aid,
                    error = %e,
                    "Ctrl+G x: task_service cancel rejected (likely a non-durable agent id)"
                );
            }
        });
        dispatched = true;
    }
    if !dispatched {
        chat_widget.commit_system(history_cell::system::SystemCell::error(format!(
            "No spawner or task service available; cannot kill agent {agent_id}",
        )));
    }
    bottom_pane.sync_popups();
    frame_requester.schedule_frame();
    true
}

/// Prose submits should hit scrollback immediately; slash commands wait
/// until their paired response/view result is ready so `› /cmd` and
/// `Result · reply` land in one flush with no synthetic blank row between them.
fn should_flush_submitted_user_cell_immediately(text: &str) -> bool {
    !text.trim_start().starts_with('/')
}

/// Whether a `!cmd` shell command needs a real TTY (inherited stdio) rather
/// than the default Command::output() pipe-capture path. Used by the `!`
/// prefix handler in the TUI to pick a strategy: pipe-capture commits output
/// to chat scrollback (good for `!ls`), inherited stdio hands the terminal
/// to the child (required for `!vim`, `!less`, etc.).
///
/// We look at the basename of the first whitespace-delimited token. This
/// misses sudo-wrapped commands (`sudo vim`) and env-prefixed forms
/// (`EDITOR=vim git commit`), which intentionally fall back to capture; if
/// those become a problem, extend the check, don't try to parse the shell.
fn shell_command_needs_tty(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or("");
    let basename = first.rsplit('/').next().unwrap_or("");
    matches!(
        basename,
        "vim"
            | "vi"
            | "nvim"
            | "nano"
            | "emacs"
            | "ed"
            | "less"
            | "more"
            | "most"
            | "man"
            | "htop"
            | "top"
            | "btop"
            | "btm"
            | "tmux"
            | "screen"
            | "ssh"
            | "mosh"
            | "telnet"
    )
}

fn should_flush_after_slash_dispatch(result: &slash_dispatch::SlashResult) -> bool {
    !matches!(result, slash_dispatch::SlashResult::Deferred)
}

fn next_pending_deferred_slash_flush(result: &slash_dispatch::SlashResult) -> bool {
    matches!(result, slash_dispatch::SlashResult::Deferred)
}

fn should_flush_ambient_commits(pending_deferred_slash_flush: bool) -> bool {
    !pending_deferred_slash_flush
}
fn refresh_footer_from_state(
    bottom_pane: &mut BottomPane,
    state: &crate::cli::session::session_state::SessionState,
) {
    bottom_pane.footer.model = state.model.clone();
    bottom_pane.footer.session_id = state
        .session_id
        .as_ref()
        .map(|sid| sid[..8.min(sid.len())].to_string());
    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
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
///
/// Cached process-wide; see `crate::git_branch_cache`.
fn detect_git_branch() -> Option<String> {
    crate::git_branch_cache::detect_git_branch_cached()
}

/// Check if the terminal supports TUI mode.
pub(crate) fn can_run_tui() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map_or(true, |t| t != "dumb")
}

pub(crate) async fn run_tui_session(
    api: &astra_thin_client::ThinClient,
    profile: Option<&str>,
    initial_model: Option<&str>,
    resume_session_id: Option<&str>,
    no_instructions: bool,
    max_budget: f64,
    cli_context: &crate::cli::cli_config::cli_context::CliContext,
) -> Result<(), String> {
    use crate::cli::session::session_runtime::{
        initialize_session_state, install_task_service, install_task_store, resolve_task_service,
        resolve_task_store,
    };
    use crate::cli::session::session_startup::complete_session_startup;
    use crate::cli::startup_trace::StartupTracer;

    // ── Ensure terminal is in sane state before startup output ────────
    // Previous astra crashes may leave terminal in raw mode, causing
    // startup eprintln output to lose carriage returns.
    let _ = crossterm::terminal::disable_raw_mode();

    // ── Initialize the gradient gutter time origin (PR #335) ─────────
    // Without this, the first cell to finalize before any
    // `elapsed_since_start()` call would saturate to 0 and the gutter
    // would jump on freeze. Eager init guarantees `>= PROCESS_START`.
    super::shimmer::init_time_origin();

    // ── Business initialization BEFORE entering TUI ─────────────────────
    let mut tracer = StartupTracer::new();
    crate::cli::session::session_runtime::try_silent_auth(api, profile).await;
    tracer.phase("auth");
    let mut state = initialize_session_state(profile, initial_model, cli_context);
    let task_service = resolve_task_service(profile).await;
    install_task_service(&mut state, task_service);
    let (task_store, task_notify_tx) = resolve_task_store(profile, Some(&api.api_origin())).await;
    install_task_store(&mut state, task_store);
    state.task_notify_tx = task_notify_tx.clone();
    if max_budget > 0.0 {
        state.max_budget_limit = max_budget;
    }
    tracer.phase("state_init");
    let _startup = complete_session_startup(
        &mut state,
        &mut tracer,
        api,
        profile,
        resume_session_id,
        no_instructions,
        cli_context,
    )
    .await?;
    tracer.finish(state.session_id.as_deref());

    // ── TUI mode overrides ──────────────────────────────────────────────
    let (tui_tx, mut tui_rx) = stream_bridge::create_channels();
    state.tui_render_policy = Some(crate::cli::stream::stream_render::RenderPolicy::Silent);
    let mut tui_cancel_token = std::sync::Arc::new(tokio_util::sync::CancellationToken::new());
    state.tui_cancel_token = Some(tui_cancel_token.clone());

    // Approval channel: tool approval requests from SSE host → TUI overlay
    let (approval_tx, mut approval_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::cli::chat_stream::ApprovalRequest>();
    state.tui_approval_request_tx = Some(approval_tx);
    let (ask_user_tx, mut ask_user_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::cli::chat_stream::AskUserRequest>();
    state.tui_ask_user_request_tx = Some(ask_user_tx);
    let (plan_review_tx, mut plan_review_rx) =
        tokio::sync::mpsc::unbounded_channel::<crate::cli::chat_stream::PlanReviewRequest>();
    state.tui_plan_review_request_tx = Some(plan_review_tx);

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
    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
    // Lock-free observer of `perm_manager.mode()` so the inner-tick
    // path can refresh the status-line chip while the agentic loop
    // holds `&mut state`. Without this, mid-turn pivots
    // (`exit_plan_mode` flipping Plan → Auto on the next-turn
    // boundary) only land on screen when the outer select wakes up.
    let perm_mode_mirror = state.perm_manager.mode_mirror_handle();
    // Wire the same mirror into the footer so every frame render
    // reads the live mode from the atomic mirror rather than the
    // cached `permission_mode` field. This eliminates the ~50 ms
    // staleness window that the tick-based self-healing had.
    bottom_pane.footer.set_mode_mirror(perm_mode_mirror.clone());

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
        let slash_items: Vec<slash_menu::SlashItem> = crate::cli::command_registry::COMMANDS
            .iter()
            .filter(|m| !m.is_alias && !m.name.contains(' '))
            .map(|m| slash_menu::SlashItem {
                name: m.name,
                description: m.description,
                subcommands: m.subcommands,
                group: Some(m.group),
                tui_handler: m.tui_handler,
                usage_examples: m.usage_examples,
                ..Default::default()
            })
            .collect();
        bottom_pane.set_slash_items(slash_items);

        // Seed dynamic MCP completions from any servers already connected at
        // startup (e.g. from a resumed session or fast-connecting transports).
        let mcp_extras = {
            let mgr = state.mcp_manager.read().await;
            crate::cli::slash::slash_mcp::build_mcp_extra_subcommands(&mgr)
        };
        bottom_pane.update_mcp_completions(mcp_extras);
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

    if let Some(prompt) = state.perm_manager.workspace_trust_startup_prompt() {
        use crate::tui::bottom_pane::list_selection_view::{ListSelectionView, SelectionItem};

        let items = vec![
            SelectionItem {
                name: "Trust Workspace".into(),
                description: Some("Enable saved workspace rules for this path".into()),
                is_current: false,
            },
            SelectionItem {
                name: "Continue This Session".into(),
                description: Some(
                    "Keep saved workspace rules off for now; ask again next time".into(),
                ),
                is_current: false,
            },
            SelectionItem {
                name: "Mark Untrusted".into(),
                description: Some(
                    "Keep saved workspace rules off and stop asking on startup".into(),
                ),
                is_current: false,
            },
        ];
        bottom_pane.push_view(Box::new(
            ListSelectionView::new(items, Some(prompt.header))
                .with_result_prefix(WORKSPACE_TRUST_SENTINEL),
        ));
    } else if let Some(notice) = state.perm_manager.workspace_trust_notice() {
        chat_widget.commit_system(history_cell::system::SystemCell::info(notice));
    }

    // Resume-time summary: surface background shells that reached
    // terminal state while the user was away. One ResumeSummary
    // rollup becomes a single banner cell at the top of scrollback
    // — it comes AFTER the replay so the banner is the last thing
    // the user sees before the prompt lands. Silent when the summary
    // is empty (either no resume, no task_service, or nothing
    // finished since the last recorded turn).
    if let (Some(svc), Some(sid)) = (state.task_service.clone(), state.session_id.clone())
        && !sid.is_empty()
    {
        let user_id = state
            .ingestion_user_id
            .clone()
            .unwrap_or_else(|| "local".into());
        match svc
            .list_recent_tasks_for_session(&user_id, &sid, None)
            .await
        {
            Ok(items) => {
                let cutoff =
                    resume_summary::last_seen_cutoff(state.last_turn_event.as_ref()).unwrap_or("");
                let summary = resume_summary::summarize(&items, &sid, cutoff);
                if !summary.is_empty() {
                    chat_widget.commit_resume_summary(summary.render());
                }
            }
            Err(e) => tracing::debug!(
                target: "astra_cli::tui",
                error = %e,
                "resume summary: list_recent_tasks failed; skipping banner"
            ),
        }
    }
    let mut status_indicator = status_indicator::StatusIndicator::new();
    let mut pending_deferred_slash_flush = false;

    // Task board observer + toggle state. Observer is tick-driven
    // (see task_board_observer.rs rationale); no background loop
    // holding locks across `.await`. Ctrl+T flips the toggle; when
    // the board transitions from empty to non-empty we auto-open it.
    let task_board = task_board_observer::TaskBoardObserver::new(
        state.task_manager.store(),
        state.session_id.clone().unwrap_or_default(),
    );
    let mut board_expanded = false;

    // Background task registry — owns spawned shell/agent processes.
    let mut background_registry = super::background_tasks::BackgroundTaskRegistry::new(
        background_task_output_dir(state.session_id.as_deref()),
    );
    let mut restored_local_agent_task_projections =
        restore_background_task_projections(&mut background_registry, state.session_id.as_deref());
    let mut background_task_projection_cache = background_registry.export_shell_task_projections();
    let mut background_local_agent_projection_cache = restored_local_agent_task_projections.clone();
    let mut background_registry_session_id = state.session_id.clone();
    // User's explicit Ctrl+T choice. `None` = auto-rules apply;
    // `Some(true|false)` = honour the user's pin even when the
    // auto-hide timer fires or new tasks appear. Reset by
    // `resolve_board_visibility` when the task list empties out.
    let mut board_user_pin: Option<bool> = None;

    frame_requester.schedule_frame();

    let result: Result<(), String> = 'main: loop {
        guard
            .ensure_tui_modes()
            .map_err(|e| format!("failed to restore terminal input mode: {e}"))?;
        let tick = tokio::time::sleep(Duration::from_millis(50));
        tokio::pin!(tick);

        tokio::select! {
            Some(ev) = event_stream.next() => {
                match ev {
                    TuiEvent::Key(key) => {
                        if let Some(AppAction::ToggleTranscript) = AppKeymap::resolve(key) {
                            use bottom_pane::transcript_view::TranscriptView;
                            if bottom_pane.transcript_view_is_open() {
                                bottom_pane.close_active_view();
                            } else {
                                // Close any other active view first so the
                                // transcript overlay can take over.
                                if bottom_pane.has_active_view() {
                                    bottom_pane.close_active_view();
                                }
                                let size = guard.terminal.size().ok();
                                let w = size.map(|s| s.width).unwrap_or(80);
                                let h = size.map(|s| s.height).unwrap_or(0);
                                let lines = render_transcript_view_lines(&chat_widget, w);
                                if !lines.is_empty() {
                                    bottom_pane.push_view(Box::new(TranscriptView::new(lines, h)));
                                } else {
                                    chat_widget.commit_system(history_cell::system::SystemCell::info(
                                        "Transcript is empty so far.".to_string(),
                                    ));
                                }
                            }
                            frame_requester.schedule_frame();
                            continue;
                        }
                        // Ctrl+L: force full redraw
                        if key.code == crossterm::event::KeyCode::Char('l')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        {
                            let _ = guard.terminal.clear();
                            guard.terminal.invalidate_viewport();
                            frame_requester.schedule_frame();
                            continue;
                        }
                        if is_background_task_manage_key(&key) && !bottom_pane.has_active_view() {
                            let _ = force_open_background_task_view(
                                &mut background_registry,
                                state.agent_spawner.as_ref(),
                                &restored_local_agent_task_projections,
                                &mut bottom_pane,
                                &frame_requester,
                            )
                            .await;
                            frame_requester.schedule_frame();
                            continue;
                        }
                        // Ctrl+T: toggle task board panel. When expanded,
                        // the active viewport renders the full list;
                        // when collapsed, a single `Next: <subject>` hint
                        // folds into the status line. Ignored while the
                        // bottom pane has a modal-style view so it
                        // doesn't steal the key from approvals/pickers.
                        //
                        // INVARIANT: any path that flips `board_expanded`
                        // on an all-completed board MUST pair it with
                        // `reveal_completed_for_review` / `hide_completed_after_review`.
                        // The hide-after-all-done idle timer sets
                        // `snapshot.hidden = true`, and the expanded
                        // viewport short-circuits to empty when that
                        // flag is set. If a future code path toggles
                        // `board_expanded` without going through these
                        // helpers, it'll claim the board is expanded
                        // but render nothing.
                        if key.code == crossterm::event::KeyCode::Char('t')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        {
                            // Ctrl+Shift+T toggles the cross-session task
                            // board view. Gated on no-active-view because
                            // this is a rarer power key and picker-modal
                            // collisions would be confusing. Regular
                            // Ctrl+T intentionally passes through: it's
                            // the user-pin, and a streaming active cell
                            // must not swallow their intent.
                            if key
                                .modifiers
                                .contains(crossterm::event::KeyModifiers::SHIFT)
                                && !bottom_pane.has_active_view()
                            {
                                task_board.toggle_view_mode();
                                frame_requester.schedule_frame();
                                continue;
                            }
                            if !bottom_pane.has_active_view()
                                && open_background_task_view(
                                    &mut background_registry,
                                    state.agent_spawner.as_ref(),
                                    &restored_local_agent_task_projections,
                                    &mut bottom_pane,
                                    &frame_requester,
                                )
                                .await
                            {
                                continue;
                            }
                            // Flip the user pin. The tick loop's
                            // `resolve_board_visibility` turns this into
                            // the actual `board_expanded` each frame so
                            // auto-hide never overrides a user's choice.
                            let new_pin = !board_expanded;
                            board_user_pin = Some(new_pin);
                            board_expanded = new_pin;
                            if new_pin {
                                task_board.reveal_completed_for_review();
                            } else {
                                task_board.hide_completed_after_review();
                            }
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
                        // Ctrl+G: drill into one of N parallel agents.
                        // Opens InFlightAgentsView listing every live
                        // TaskCell PLUS up to 5 recently-completed
                        // ones (so the user can still drill into
                        // finished agents after the live strip is
                        // gone). Empty case shows a toast instead of
                        // silently no-op'ing — the silent no-op was
                        // observed as broken UX in session 2a98814b.
                        if key.code == crossterm::event::KeyCode::Char('g')
                            && key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                            && !bottom_pane.has_active_view()
                        {
                            use bottom_pane::in_flight_agents_view::InFlightAgentsView;
                            let rows = chat_widget
                                .agents_drilldown_rows(AGENT_DRILLDOWN_RECENT_COMPLETED);
                            if rows.is_empty() {
                                chat_widget.commit_system(
                                    history_cell::system::SystemCell::info(
                                        "No parallel agents to drill into yet. \
                                         Spawn some with `agent(action='spawn', ...)` first."
                                            .to_string(),
                                    ),
                                );
                            } else {
                                bottom_pane.push_view(Box::new(InFlightAgentsView::new(rows)));
                            }
                            frame_requester.schedule_frame();
                            continue;
                        }
                        match bottom_pane.handle_key(key) {
                            BottomPaneAction::CyclePermissionMode => {
                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                let next_mode = slash_dispatch::next_permission_mode_for_cycle(
                                    state.perm_manager.mode(),
                                );
                                state.perm_manager.set_mode(next_mode);
                                chat_widget.commit_system(
                                    crate::tui::history_cell::system::SystemCell::response(
                                        slash_dispatch::permission_mode_feedback(next_mode),
                                    ),
                                );
                                // Re-evaluate the pending approval queue
                                // so the chip and pending count agree.
                                // Without this, an approval generated
                                // under the previous (more restrictive)
                                // mode lingers in the queue while the
                                // chip says e.g. `auto` — see session
                                // 6953d1da regression note on
                                // `BottomPane::reevaluate_approvals_for_mode`.
                                let released = bottom_pane
                                    .reevaluate_approvals_for_mode(next_mode);
                                if released > 0 {
                                    chat_widget.commit_system(
                                        crate::tui::history_cell::system::SystemCell::response(
                                            format!(
                                                "  ✓ {released} pending approval(s) auto-resolved by the new mode",
                                            ),
                                        ),
                                    );
                                }
                                refresh_footer_from_state(&mut bottom_pane, &state);
                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                frame_requester.schedule_frame();
                            }
                            BottomPaneAction::SubmitInput(text) => {
                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);

                                // ! prefix: execute the rest as a shell command.
                                //
                                // Two paths, selected by a whitelist of programs known
                                // to require a real TTY:
                                //
                                //   - TTY commands (vim, less, htop, ssh, ...) — run
                                //     with inherited stdio (same as Ctrl-E external
                                //     editor flow). The child takes over the terminal;
                                //     we don't capture output. Output stays on screen
                                //     but only an "! cmd" marker goes to chat.
                                //
                                //   - Everything else (ls, grep, cat, git, ...) — run
                                //     with Command::output() pipes; the captured stdout
                                //     and stderr are committed into chat scrollback as
                                //     a SystemCell so they survive the next TUI redraw.
                                //
                                // The whitelist is intentionally short; add commands as
                                // they come up.
                                if let Some(cmd_ref) = text.trim_start().strip_prefix('!') {
                                    let cmd = cmd_ref.trim().to_string();
                                    if !cmd.is_empty() {
                                        if shell_command_needs_tty(&cmd) {
                                            // Interactive path — same pattern as
                                            // BottomPaneAction::OpenExternalEditor.
                                            let _ = crossterm::terminal::disable_raw_mode();
                                            let _ = crossterm::execute!(
                                                std::io::stdout(),
                                                crossterm::event::DisableBracketedPaste,
                                                crossterm::cursor::Show
                                            );
                                            println!("! {cmd}");
                                            let status = std::process::Command::new("sh")
                                                .arg("-c")
                                                .arg(&cmd)
                                                .status();
                                            if let Err(err) = guard.ensure_tui_modes() {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(format!(
                                                        "! {cmd}: failed to restore TUI modes: {err}"
                                                    )),
                                                );
                                            }
                                            guard.terminal.invalidate_viewport();
                                            match status {
                                                Ok(s) if s.success() => {
                                                    chat_widget.commit_system(
                                                        history_cell::system::SystemCell::response(
                                                            format!("! {cmd}"),
                                                        ),
                                                    );
                                                }
                                                Ok(s) => {
                                                    chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!(
                                                                "! {cmd}: exit {}",
                                                                s.code().unwrap_or(-1)
                                                            ),
                                                        ),
                                                    );
                                                }
                                                Err(e) => {
                                                    chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!("! {cmd}: {e}"),
                                                        ),
                                                    );
                                                }
                                            }
                                        } else {
                                            // Capture path — pipes, commit captured
                                            // output to chat scrollback.
                                            match std::process::Command::new("sh")
                                                .arg("-c")
                                                .arg(&cmd)
                                                .output()
                                            {
                                                Ok(out) => {
                                                    let stdout =
                                                        String::from_utf8_lossy(&out.stdout);
                                                    let stderr =
                                                        String::from_utf8_lossy(&out.stderr);
                                                    let combined =
                                                        format!("{stdout}{stderr}")
                                                            .trim()
                                                            .to_string();
                                                    if combined.is_empty() {
                                                        if out.status.success() {
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    format!("! {cmd}"),
                                                                ),
                                                            );
                                                        } else {
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    format!(
                                                                        "! {cmd}: exit {}",
                                                                        out.status
                                                                            .code()
                                                                            .unwrap_or(-1)
                                                                    ),
                                                                ),
                                                            );
                                                        }
                                                    } else {
                                                        let prefixed: String = combined
                                                            .lines()
                                                            .map(|l| format!("┃ {l}"))
                                                            .collect::<Vec<_>>()
                                                            .join("\n");
                                                        let body =
                                                            format!("! {cmd}\n{prefixed}");
                                                        if out.status.success() {
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::info(body),
                                                            );
                                                        } else {
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    format!(
                                                                        "{body}\n┃ exit {}",
                                                                        out.status
                                                                            .code()
                                                                            .unwrap_or(-1)
                                                                    ),
                                                                ),
                                                            );
                                                        }
                                                    }
                                                }
                                                Err(e) => {
                                                    chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!("! {cmd}: {e}"),
                                                        ),
                                                    );
                                                }
                                            }
                                        }
                                    }
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    refresh_footer_from_state(&mut bottom_pane, &state);
                                    frame_requester.schedule_frame();
                                    continue;
                                }

                                let flush_user_immediately =
                                    should_flush_submitted_user_cell_immediately(&text);
                                // Shadow: mirror the user submit into
                                // ChatWidget so its history stays in
                                // sync with transcript scrollback. Does
                                // persistence (when sid is non-empty)
                                // even though rendering still runs
                                // through the existing app-event path.
                                chat_widget.handle_event(
                                    chat_widget::AppEvent::User(UserEvent::Submit(text.clone())),
                                );
                                if flush_user_immediately {
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);

                                    {
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        let frame = active_viewport(
                                            &chat_widget,
                                            &status_indicator,
                                            Some(&*task_board),
                                            board_expanded,
                                            board_user_pin,
                                            w,
                                            guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                        );
                                        board_expanded = frame.resolved_board_expanded;
                                        do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board)?;
                                    }
                                }

                                let mut inline_chat_submit = None;
                                if let Some(plan_goal) = slash_plan_goal(&text) {
                                    let before = capture_plan_mode_ui_snapshot(&state);
                                    let Some(token) =
                                        crate::cli::plan::plan_lifecycle::fresh_token_for_plan(api, profile).await
                                    else {
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::error(
                                                "Not logged in. Use /login.",
                                            ),
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        frame_requester.schedule_frame();
                                        continue;
                                    };
                                    match crate::cli::plan::plan_lifecycle::enter_remote_plan_mode(
                                        api,
                                        profile,
                                        &token,
                                        &mut state,
                                        plan_goal,
                                    )
                                    .await
                                    {
                                        Ok(_) => {
                                            inline_chat_submit = Some(plan_goal.to_string());
                                            commit_plan_transition_notice(
                                                &mut chat_widget,
                                                &before,
                                                &state,
                                                true,
                                            );
                                            if let Some(ref sid) = state.session_id
                                                && chat_widget.session_id() != sid
                                            {
                                                chat_widget.set_session_id(sid.clone());
                                                task_board.rebind_session(sid.clone());
                                                board_user_pin = None;
                                            }
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        }
                                        Err(error) => {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(error),
                                            );
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            frame_requester.schedule_frame();
                                            continue;
                                        }
                                    }
                                }

                                if text.starts_with('/') && inline_chat_submit.is_none() {
                                    // Snapshot session id before dispatch so we
                                    // can detect when a `/resume <id>` fallback
                                    // rebinds it and trigger the replay.
                                    let pre_sid = state.session_id.clone();
                                    let pre_plan_snapshot = text
                                        .trim_start()
                                        .starts_with("/plan")
                                        .then(|| capture_plan_mode_ui_snapshot(&state));
                                    let mut dctx = slash_dispatch::DispatchContext {
                                        api, profile, state: &mut state,
                                        guard: &mut guard, bottom_pane: &mut bottom_pane,
                                        chat_widget: &mut chat_widget, width: w,
                                    };
                                    let result = slash_dispatch::dispatch(&text, &mut dctx).await;
                                    pending_deferred_slash_flush =
                                        next_pending_deferred_slash_flush(&result);
                                    match result {
                                        slash_dispatch::SlashResult::Handled => {}
                                        slash_dispatch::SlashResult::Deferred => {}
                                        slash_dispatch::SlashResult::Exit => { break 'main Ok(()); }
                                        slash_dispatch::SlashResult::Fallback => {
                                            let slash_text = text.clone();
                                            let slash_result = guard.with_restored(|| async {
                                                let token = crate::cli::session::session_runtime::fresh_access_token(api, profile).await;
                                                crate::cli::slash::slash_router::handle_slash_command(
                                                    &slash_text, api, profile, &mut state,
                                                    token.as_deref(),
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
                                        slash_dispatch::SlashResult::Forward(ref forward_text) => {
                                            bottom_pane.composer.set_text(forward_text);
                                        }
                                    }
                                    // Flush the slash-command response
                                    // cells (`⎿ Set model to …`, etc.)
                                    // into scrollback immediately so
                                    // the reply appears under `› /cmd`
                                    // without the ~50ms tick delay.
                                    if should_flush_after_slash_dispatch(&result) {
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }
                                    // If the slash command rebound state.session_id
                                    // (resume/new-session paths), swap the
                                    // ChatWidget so its scrollback + persistence
                                    // attach to the restored session.
                                    if state.session_id != pre_sid
                                        && let Some(ref new_sid) = state.session_id
                                        && !new_sid.is_empty()
                                    {
                                        chat_widget = replay_session_into_widget(&mut guard, new_sid, w);
                                        task_board.rebind_session(new_sid.clone());
                                        // New session → clear the
                                        // user pin so the first
                                        // non-empty tick on this session
                                        // re-enters auto-rules.
                                        board_user_pin = None;
                                    }
                                    refresh_footer_from_state(&mut bottom_pane, &state);
                                    // After any /mcp command refresh the dynamic
                                    // server/tool completions so that a freshly
                                    // added or removed server is immediately
                                    // visible in the tab-completion menu.
                                    if text.starts_with("/mcp") {
                                        let mcp_extras = {
                                            let mgr = state.mcp_manager.read().await;
                                            crate::cli::slash::slash_mcp::build_mcp_extra_subcommands(&mgr)
                                        };
                                        bottom_pane.update_mcp_completions(mcp_extras);
                                    }
                                    if let Some(before) = pre_plan_snapshot.as_ref() {
                                        commit_plan_transition_notice(
                                            &mut chat_widget,
                                            before,
                                            &state,
                                            true,
                                        );
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }
                                } else {
                                    let submit_was_inline_plan_goal = inline_chat_submit.is_some();
                                    let submit_text = inline_chat_submit.unwrap_or(text);
                                    if crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(
                                        &state,
                                    ) {
                                        let Some(token) =
                                            crate::cli::plan::plan_lifecycle::fresh_token_for_plan(api, profile)
                                                .await
                                        else {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(
                                                    "Not logged in. Use /login.",
                                                ),
                                            );
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            frame_requester.schedule_frame();
                                            continue;
                                        };
                                        match crate::cli::plan::plan_lifecycle::enter_remote_plan_mode(
                                            api,
                                            profile,
                                            &token,
                                            &mut state,
                                            &submit_text,
                                        )
                                        .await
                                        {
                                            Ok(_) => {
                                                // After bare `/plan`, the first plain message is
                                                // the user's real planning goal. Don't insert a
                                                // synthetic `Plan goal set ...` system line above
                                                // the actual planning/model output.
                                                if let Some(ref sid) = state.session_id
                                                    && chat_widget.session_id() != sid
                                                {
                                                    chat_widget.set_session_id(sid.clone());
                                                    task_board.rebind_session(sid.clone());
                                                    board_user_pin = None;
                                                }
                                                refresh_footer_from_state(&mut bottom_pane, &state);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            }
                                            Err(error) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(error),
                                                );
                                                refresh_footer_from_state(&mut bottom_pane, &state);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                frame_requester.schedule_frame();
                                                continue;
                                            }
                                        }
                                    } else {
                                        if !submit_was_inline_plan_goal {
                                            if let Some(plan_command) =
                                                crate::cli::plan::plan_commands::parse_plan_command(&submit_text)
                                                    .filter(|command| {
                                                        crate::cli::plan::plan_commands::is_plan_command_available(
                                                            &state, command,
                                                        )
                                                    })
                                            {
                                                match plan_command {
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::Go => {
                                                        let go_result = guard
                                                            .with_restored(|| async {
                                                                let Some(token) =
                                                                    crate::cli::plan::plan_lifecycle::fresh_token_for_plan(
                                                                        api, profile,
                                                                    )
                                                                    .await
                                                                else {
                                                                    return Err(
                                                                        "Not logged in. Use /login."
                                                                            .to_string(),
                                                                    );
                                                                };
                                                                crate::cli::plan::plan_commands::prepare_plan_execution(
                                                                    &mut state, api, &token,
                                                                )
                                                                .await?;
                                                                crate::cli::plan::plan_runtime::start_and_monitor_plan(
                                                                    &mut state,
                                                                    Some(&token),
                                                                    api,
                                                                    profile,
                                                                )
                                                                .await
                                                            })
                                                            .await;
                                                        match go_result {
                                                            Ok(Ok(())) => {
                                                                let message = if state
                                                                    .executing_plan
                                                                    .is_some()
                                                                {
                                                                    "Plan run paused. Use `show`, `rewind …`, `correct …`, or `go` to continue.".to_string()
                                                                } else if state
                                                                    .plan_execution_last_error
                                                                    .is_some()
                                                                {
                                                                    "Plan run ended with an error. Rewind or adjust it before trying `go` again.".to_string()
                                                                } else {
                                                                    "Plan run finished. Back in normal chat.".to_string()
                                                                };
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::response(
                                                                        message,
                                                                    ),
                                                                );
                                                            }
                                                            Ok(Err(error)) => {
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::error(
                                                                        error,
                                                                    ),
                                                                );
                                                            }
                                                            Err(error) => {
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::error(
                                                                        format!(
                                                                            "Terminal restore failed: {error}"
                                                                        ),
                                                                    ),
                                                                );
                                                            }
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::Show => {
                                                        match crate::cli::plan::plan_commands::render_plan_snapshot(
                                                            &state,
                                                        ) {
                                                            Ok(message) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    message,
                                                                ),
                                                            ),
                                                            Err(error) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    error,
                                                                ),
                                                            ),
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::Rewind {
                                                        anchor,
                                                    } => {
                                                        let token =
                                                            crate::cli::plan::plan_lifecycle::fresh_token_for_plan(
                                                                api, profile,
                                                            )
                                                            .await;
                                                        match crate::cli::plan::plan_commands::rewind_plan(
                                                            &mut state,
                                                            api,
                                                            token.as_deref(),
                                                            &anchor,
                                                        )
                                                        .await
                                                        {
                                                            Ok(message) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    message,
                                                                ),
                                                            ),
                                                            Err(error) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    error,
                                                                ),
                                                            ),
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                    crate::cli::plan::plan_commands::ParsedPlanCommand::AddCorrection { .. }
                                                    | crate::cli::plan::plan_commands::ParsedPlanCommand::ClearCorrections => {
                                                        match crate::cli::plan::plan_commands::apply_plan_correction(
                                                            &mut state,
                                                            &plan_command,
                                                        ) {
                                                            Ok(message) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::response(
                                                                    message,
                                                                ),
                                                            ),
                                                            Err(error) => chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    error,
                                                                ),
                                                            ),
                                                        }
                                                        refresh_footer_from_state(
                                                            &mut bottom_pane,
                                                            &state,
                                                        );
                                                        flush_chat_widget(
                                                            &mut guard,
                                                            &mut chat_widget,
                                                            w,
                                                        );
                                                        frame_requester.schedule_frame();
                                                        continue;
                                                    }
                                                }
                                            }
                                            if state.executing_plan.is_some()
                                                && !state.plan_mode_active()
                                                && crate::cli::plan::plan_commands::abandon_plan_execution(
                                                    &mut state,
                                                )
                                            {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::info(
                                                        "Paused plan abandoned — continuing with normal chat.".to_string(),
                                                    ),
                                                );
                                                refresh_footer_from_state(
                                                    &mut bottom_pane,
                                                    &state,
                                                );
                                                flush_chat_widget(
                                                    &mut guard,
                                                    &mut chat_widget,
                                                    w,
                                                );
                                            }
                                        }
                                        if looks_like_implicit_plan_request(&submit_text) {
                                        let before = capture_plan_mode_ui_snapshot(&state);
                                        let Some(token) =
                                            crate::cli::plan::plan_lifecycle::fresh_token_for_plan(api, profile)
                                                .await
                                        else {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::error(
                                                    "Not logged in. Use /login.",
                                                ),
                                            );
                                            refresh_footer_from_state(&mut bottom_pane, &state);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            frame_requester.schedule_frame();
                                            continue;
                                        };
                                        match crate::cli::plan::plan_lifecycle::enter_remote_plan_mode(
                                            api,
                                            profile,
                                            &token,
                                            &mut state,
                                            &submit_text,
                                        )
                                        .await
                                        {
                                            Ok(_) => {
                                                commit_plan_transition_notice(
                                                    &mut chat_widget,
                                                    &before,
                                                    &state,
                                                    true,
                                                );
                                                if let Some(ref sid) = state.session_id
                                                    && chat_widget.session_id() != sid
                                                {
                                                    chat_widget.set_session_id(sid.clone());
                                                    task_board.rebind_session(sid.clone());
                                                    board_user_pin = None;
                                                }
                                                refresh_footer_from_state(&mut bottom_pane, &state);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                frame_requester.schedule_frame();
                                                continue;
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(e),
                                                );
                                                refresh_footer_from_state(&mut bottom_pane, &state);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                frame_requester.schedule_frame();
                                                continue;
                                            }
                                        }
                                        }
                                    }

                                    bottom_pane.set_task_status(TaskStatus::WaitingModel);
                                    let turn_start = std::time::Instant::now();
                                    let pre_prompt_tokens = state.total_prompt_tokens;
                                    let pre_completion_tokens = state.total_completion_tokens;
                                    let _pre_cost = state.total_session_cost;
                                    let pre_cache_read = state.total_cache_read_tokens;
                                    let pre_cache_creation = state.total_cache_creation_tokens;
                                    let pre_cached_context_trace_turn_id = state
                                        .latest_context_assembly_trace
                                        .as_ref()
                                        .map(|trace| trace.turn_id.clone());
                                    let pre_context_trace_count = context_trace_count(&state);
                                    let mut turn_tool_count: u32 = 0;
                                    let mut turn_ttft: Option<std::time::Instant> = None;
                                    let mut explain_items: Vec<serde_json::Value> = Vec::new();
                                    // Phase 3b.3c: prime the bash detach slot for this
                                    // turn. The bash runner takes the handle on entry;
                                    // we keep the listener so a Ctrl+B keypress can
                                    // fire the signal and await the live child + streams
                                    // payload back. Replaces any stale handle from a
                                    // prior turn that was never consumed (e.g. the model
                                    // didn't run bash last turn).
                                    let mut bash_detach_listener =
                                        Some(
                                            install_bash_detach_listener(
                                                &state.bash_detach_slot,
                                                &mut chat_widget,
                                                &mut status_indicator,
                                            )
                                            .await,
                                        );
                                    let (
                                        bash_detach_handoff_tx,
                                        mut bash_detach_handoff_rx,
                                    ) = tokio::sync::mpsc::unbounded_channel::<
                                        BashDetachHandoffResult,
                                    >();
                                    let mut bash_detach_request_pending = false;
                                    let mut active_bash_tool_use_id: Option<String> = None;
                                    let mut active_bash_description: Option<String> = None;

                                    let turn_tx = stream_bridge::create_per_turn_bridge(tui_tx.clone());
                                    let live_sink = stream_bridge::create_agent_live_sink(tui_tx.clone());
                                    state.tui_stream_event_tx = Some(turn_tx);
                                    state.tui_agent_live_event_sink = Some(live_sink);

                                    let turn_result = {
                                        // Snapshot the Arc<dyn TaskService> before the
                                        // turn borrows `state` mutably — Ctrl+C still
                                        // needs to issue cancel RPCs for in-flight
                                        // sub-agents, and the mutable borrow prevents
                                        // us from reaching through `state` inside the
                                        // inner select.
                                        let task_service_for_cancel = state.task_service.clone();
                                        let agent_spawner_for_cancel = state.agent_spawner.clone();
                                        let active_turn_local_run_control =
                                            state.active_turn_local_run_control.clone();
                                        let bash_detach_slot_for_ctrl_b =
                                            state.bash_detach_slot.clone();
                                        let background_registry_turn_session_id =
                                            state.session_id.clone();
                                        let background_registry_turn_model = state.model.clone();
                                        // Snapshot the bg-task command queue so the inner-tick
                                        // drainer below can serve `task_output(block=true)` calls
                                        // mid-turn. Without this, the tool side awaits a reply
                                        // that the outer-tick drainer can't deliver — the outer
                                        // loop is parked on this turn future.
                                        let bg_task_commands_for_tick = state.bg_task_commands.clone();
                                        let agent_spawner_for_tick = state.agent_spawner.clone();
                                        let ctx = crate::cli::turn::turn_entry::TurnContext { api, profile };
                                        let token = crate::cli::session::session_runtime::fresh_access_token(api, profile).await;
                                        let mut tui_ui = ui_adapter::TuiUiAdapter::new(tui_tx.clone());
                                        let fut = crate::cli::turn::turn_entry::handle_chat_input_with_ui(submit_text, token.as_deref(), &mut state, ctx, &mut tui_ui);
                                        tokio::pin!(fut);

                                        let mut turn_result_ready: Option<Result<(), String>> = None;
                                        let r: Result<(), String> = loop {
                                            if let Err(e) = guard.ensure_tui_modes() {
                                                break Err(format!(
                                                    "failed to restore terminal input mode: {e}"
                                                ));
                                            }
                                            let itick = tokio::time::sleep(Duration::from_millis(80));
                                            tokio::pin!(itick);
                                            tokio::select! {
                                                result = &mut fut, if turn_result_ready.is_none() => {
                                                    if bash_detach_request_pending {
                                                        turn_result_ready = Some(result);
                                                        continue;
                                                    }
                                                    break result;
                                                }
                                                Some(tev) = event_stream.next() => {
                                                    match tev {
                                                        TuiEvent::Key(k) => {
                                                            // Shift+Tab cycles permission mode mid-turn.
                                                            // The current turn keeps running with the
                                                            // schema it was assembled with (Invariant
                                                            // I8); only the next turn picks up the new
                                                            // mode. We refresh the chip via the lock-free
                                                            // mirror so the user sees the pivot land
                                                            // immediately.
                                                            if k.code == crossterm::event::KeyCode::BackTab {
                                                                let next_mode = slash_dispatch::next_permission_mode_for_cycle(
                                                                    perm_mode_mirror.current(),
                                                                );
                                                                // Mid-turn: agentic loop holds &mut state, so we
                                                                // cannot borrow perm_manager. Stage on the
                                                                // mirror; the host calls pull_mode_from_mirror
                                                                // at the next turn boundary so `self.mode`
                                                                // catches up.
                                                                perm_mode_mirror.stage(next_mode);
                                                                bottom_pane.footer.permission_mode =
                                                                    Some(next_mode);
                                                                // Reflect the staged mode in the approval queue
                                                                // immediately so the chip and pending count
                                                                // agree. perm_manager.mode() will catch up at
                                                                // the next turn boundary, but the queue's
                                                                // visible state shouldn't lag.
                                                                let released = bottom_pane
                                                                    .reevaluate_approvals_for_mode(next_mode);
                                                                if released > 0 {
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::response(
                                                                            format!(
                                                                                "  ✓ {released} pending approval(s) auto-resolved by the new mode",
                                                                            ),
                                                                        ),
                                                                    );
                                                                }
                                                                chat_widget.commit_system(
                                                                    history_cell::system::SystemCell::response(
                                                                        slash_dispatch::permission_mode_feedback(next_mode),
                                                                    ),
                                                                );
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            // Shift+↓ shares Ctrl+B's verb: "show me the background
                                                            // panel". Always force-open so the user has a stable
                                                            // surface to land on, even when the registry is briefly
                                                            // empty (e.g. during bash detach handoff or while
                                                            // task_output is blocking on stdout). Empty state
                                                            // renders as "No background tasks." inside the view.
                                                            if is_background_task_manage_key(&k)
                                                                && !bottom_pane.has_active_view()
                                                            {
                                                                let _ = force_open_background_task_view(
                                                                    &mut background_registry,
                                                                    agent_spawner_for_cancel.as_ref(),
                                                                    &restored_local_agent_task_projections,
                                                                    &mut bottom_pane,
                                                                    &frame_requester,
                                                                )
                                                                .await;
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            // Ctrl+B: foreground bash/agent → background promotion.
                                                            // If a bash invocation is currently in flight and
                                                            // listening on the detach signal, fire it: the
                                                            // runner transfers child + live streams to the
                                                            // BackgroundTaskRegistry without kill, output
                                                            // continues uninterrupted, and the bash tool
                                                            // returns a <bash_detached> marker with the
                                                            // concrete background task id. Otherwise, try
                                                            // to promote a foreground
                                                            // synchronous agent wait into a normal background
                                                            // agent. If neither path is available, the key is
                                                            // explicitly unavailable and does not cancel the
                                                            // parent turn.
                                                            // Ctrl+B is a single verb: "show me the background panel".
                                                            // Side-effects (promote a backgroundable bash, promote a
                                                            // foreground agent) compose with that verb; they never
                                                            // replace it. The panel is always opened — even if the
                                                            // registry is empty in this instant — so the user has
                                                            // a stable surface to land on. There is no "unavailable"
                                                            // path: any state is a renderable panel state.
                                                            if k.code == crossterm::event::KeyCode::Char('b')
                                                                && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                                                            {
                                                                // Side-effect 1: detach an in-flight backgroundable bash.
                                                                // Skipped if a detach is already mid-flight (handoff
                                                                // pending) or if the registry is at the shell-task cap.
                                                                let detach_fired = if !bash_detach_request_pending
                                                                    && background_registry.can_spawn_shell_task()
                                                                    && let Some(listener) = bash_detach_listener.take()
                                                                {
                                                                    if listener.is_active() && listener.signal_tx.send(true).is_ok() {
                                                                        listener.retire();
                                                                        bash_detach_request_pending = true;
                                                                        set_bash_background_hint_enabled(
                                                                            &mut chat_widget,
                                                                            &mut status_indicator,
                                                                            false,
                                                                        );
                                                                        let handoff_tx = bash_detach_handoff_tx.clone();
                                                                        tokio::spawn(async move {
                                                                            let result = match listener.payload_rx.await {
                                                                                Ok(payload) => Ok(payload),
                                                                                Err(_) => Err(
                                                                                    "bash runner ended before handing off the process."
                                                                                        .to_string(),
                                                                                ),
                                                                            };
                                                                            let _ = handoff_tx.send(result);
                                                                        });
                                                                        true
                                                                    } else {
                                                                        // Inactive listener or closed signal channel:
                                                                        // restore so a later bash can pick it up.
                                                                        bash_detach_listener = Some(listener);
                                                                        false
                                                                    }
                                                                } else {
                                                                    false
                                                                };

                                                                // Side-effect 2: promote a foreground agent. Only
                                                                // fires if no bash detach was triggered this press
                                                                // (one promotion per Ctrl+B, never both).
                                                                let mut promoted_agent_id: Option<String> = None;
                                                                if !detach_fired
                                                                    && let Some(spawner) = agent_spawner_for_cancel.as_ref()
                                                                    && let Some(agent) = spawner
                                                                        .promote_foreground_agent_to_background(None)
                                                                        .await
                                                                {
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::info(
                                                                            ctrl_b_promoted_agent_message(
                                                                                &agent.agent_id,
                                                                                &agent.description,
                                                                            ),
                                                                        ),
                                                                    );
                                                                    promoted_agent_id = Some(agent.agent_id);
                                                                    tui_cancel_token.cancel();
                                                                }

                                                                // Always: open the panel. Empty registry renders as
                                                                // "No background tasks." inside the view; a pending
                                                                // bash handoff renders as a ghost row pinned to top.
                                                                if detach_fired {
                                                                    let pending_title = active_bash_description
                                                                        .as_deref()
                                                                        .unwrap_or("Bash");
                                                                    let pending_rows = vec![
                                                                        pending_bash_handoff_row(pending_title, 0),
                                                                    ];
                                                                    let _ = reveal_background_task_view_with_extra_rows(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                        pending_rows,
                                                                        Some(PENDING_BASH_HANDOFF_TASK_ID),
                                                                    )
                                                                    .await;
                                                                } else if let Some(agent_id) = promoted_agent_id.as_deref() {
                                                                    let _ = reveal_background_task_view(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                        Some(agent_id),
                                                                    )
                                                                    .await;
                                                                } else {
                                                                    let _ = force_open_background_task_view(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                    )
                                                                    .await;
                                                                }
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            // Ctrl+G: drill into one of N parallel agents — also
                                                            // works MID-TURN (this is the path that matters: the
                                                            // user typically wants to drill in WHILE agents are
                                                            // running, not after). Pre-fix the inner select fell
                                                            // through to bottom_pane.handle_key, which routed
                                                            // Ctrl+G into the composer as a raw character. Same
                                                            // semantics as the outer Ctrl+G handler — non-empty
                                                            // rows push the InFlightAgentsView; empty case shows
                                                            // a toast. Does NOT cancel the turn.
                                                            if k.code == crossterm::event::KeyCode::Char('g')
                                                                && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                                                                && !bottom_pane.has_active_view()
                                                            {
                                                                use bottom_pane::in_flight_agents_view::InFlightAgentsView;
                                                                let rows = chat_widget
                                                                    .agents_drilldown_rows(AGENT_DRILLDOWN_RECENT_COMPLETED);
                                                                if rows.is_empty() {
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::info(
                                                                            "No parallel agents to drill into yet. \
                                                                             Spawn some with `agent(action='spawn', ...)` first."
                                                                                .to_string(),
                                                                        ),
                                                                    );
                                                                } else {
                                                                    bottom_pane.push_view(Box::new(InFlightAgentsView::new(rows)));
                                                                }
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            // Ctrl+T mid-turn: same toggle semantics as the
                                                            // outer-loop handler. Without this branch the
                                                            // inner select's match-arm fell through into
                                                            // bottom_pane.handle_key, which silently
                                                            // ignored Ctrl+T — so the user saw "no
                                                            // response" any time the board was busy.
                                                            // Mirrors the outer handler's INVARIANT:
                                                            // pair board_expanded flips with
                                                            // reveal/hide_completed_after_review so the
                                                            // hide-after-all-done flag doesn't render
                                                            // the expanded panel as empty.
                                                            if k.code == crossterm::event::KeyCode::Char('t')
                                                                && k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                                                            {
                                                                if k
                                                                    .modifiers
                                                                    .contains(crossterm::event::KeyModifiers::SHIFT)
                                                                    && !bottom_pane.has_active_view()
                                                                {
                                                                    task_board.toggle_view_mode();
                                                                    frame_requester.schedule_frame();
                                                                    continue;
                                                                }
                                                                if !bottom_pane.has_active_view()
                                                                    && open_background_task_view(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                    )
                                                                    .await
                                                                {
                                                                    continue;
                                                                }
                                                                let new_pin = !board_expanded;
                                                                board_user_pin = Some(new_pin);
                                                                board_expanded = new_pin;
                                                                if new_pin {
                                                                    task_board.reveal_completed_for_review();
                                                                } else {
                                                                    task_board.hide_completed_after_review();
                                                                }
                                                                frame_requester.schedule_frame();
                                                                continue;
                                                            }
                                                            // During turn: composer stays usable.
                                                            // Enter queues a deferred input against the active run.
                                                            // Ctrl+C interrupts.
                                                            match bottom_pane.handle_key(k) {
                                                                    BottomPaneAction::SubmitInput(queued_text) => {
                                                                        // Agent drill-in sentinel: user pressed Enter
                                                                        // on a row in InFlightAgentsView mid-turn.
                                                                        // Without this strip, the sentinel string
                                                                        // ("__agent_drilldown__\n<id>") would be
                                                                        // queued as a chat message — broken UX.
                                                                        // Mirrors the outer-select handler at
                                                                        // ~line 1004.
                                                                        if let Some(agent_id) = bottom_pane::in_flight_agents_view::parse_drilldown_sentinel(&queued_text) {
                                                                            if let Some(tc) =
                                                                                chat_widget.task_cell_anywhere(agent_id)
                                                                            {
                                                                                use bottom_pane::task_detail_view::TaskDetailView;
                                                                                bottom_pane.push_view(Box::new(
                                                                                    TaskDetailView::from_task_cell(tc)
                                                                                        .with_live_task_id(agent_id.to_string())
                                                                                        .with_reopen(ReopenTarget::Agents.as_str()),
                                                                                ));
                                                                            } else {
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::info(
                                                                                        format!(
                                                                                            "Agent {agent_id} not found in live or recent history."
                                                                                        ),
                                                                                    ),
                                                                                );
                                                                            }
                                                                            bottom_pane.sync_popups();
                                                                            frame_requester.schedule_frame();
                                                                            continue;
                                                                        }
                                                                        match submit_deferred_tui_input(
                                                                            &active_turn_local_run_control,
                                                                            &queued_text,
                                                                        )
                                                                        .await
                                                                        {
                                                                            Ok(()) => {
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::info(
                                                                                        format!(
                                                                                            "Queued for next tool call: {}",
                                                                                            deferred_input_preview(&queued_text)
                                                                                        ),
                                                                                    ),
                                                                                );
                                                                            }
                                                                            Err(error) => {
                                                                                bottom_pane.composer.set_text(&queued_text);
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::error(error),
                                                                                );
                                                                            }
                                                                        }
                                                                    }
                                                                    BottomPaneAction::ViewSideEffect { result } => {
                                                                        if try_dispatch_background_task_stop_sentinel(
                                                                            &result,
                                                                            &mut background_registry,
                                                                            agent_spawner_for_cancel.clone(),
                                                                            &restored_local_agent_task_projections,
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        )
                                                                        .await
                                                                        {
                                                                            continue;
                                                                        }
                                                                        if try_dispatch_background_task_output_sentinel(
                                                                            &result,
                                                                            &mut background_registry,
                                                                            agent_spawner_for_cancel.clone(),
                                                                            &restored_local_agent_task_projections,
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        )
                                                                        .await
                                                                        {
                                                                            continue;
                                                                        }
                                                                        let _ = try_dispatch_agent_kill_sentinel(
                                                                            &result,
                                                                            agent_spawner_for_cancel.clone(),
                                                                            task_service_for_cancel.clone(),
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        );
                                                                        let rows = chat_widget.agents_drilldown_rows(AGENT_DRILLDOWN_RECENT_COMPLETED);
                                                                        bottom_pane.refresh_agent_rows(rows);
                                                                        frame_requester.schedule_frame();
                                                                    }
                                                                    BottomPaneAction::ViewCompleted { result: Some(name), reopen: _ } => {
                                                                        if try_dispatch_background_task_stop_sentinel(
                                                                            &name,
                                                                            &mut background_registry,
                                                                            agent_spawner_for_cancel.clone(),
                                                                            &restored_local_agent_task_projections,
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        )
                                                                        .await
                                                                        {
                                                                            continue;
                                                                        }
                                                                        if try_dispatch_background_task_output_sentinel(
                                                                            &name,
                                                                            &mut background_registry,
                                                                            agent_spawner_for_cancel.clone(),
                                                                            &restored_local_agent_task_projections,
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        )
                                                                        .await
                                                                        {
                                                                            continue;
                                                                        }
                                                                        if try_dispatch_agent_kill_sentinel(
                                                                            &name,
                                                                            agent_spawner_for_cancel.clone(),
                                                                            task_service_for_cancel.clone(),
                                                                            &mut chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        ) {
                                                                            continue;
                                                                        }
                                                                        if let Some(agent_id) = bottom_pane::in_flight_agents_view::parse_drilldown_sentinel(&name) {
                                                                            if let Some(tc) =
                                                                                chat_widget.task_cell_anywhere(agent_id)
                                                                            {
                                                                                use bottom_pane::task_detail_view::TaskDetailView;
                                                                                bottom_pane.push_view(Box::new(
                                                                                    TaskDetailView::from_task_cell(tc)
                                                                                        .with_live_task_id(agent_id.to_string())
                                                                                        .with_reopen(ReopenTarget::Agents.as_str()),
                                                                                ));
                                                                            } else {
                                                                                chat_widget.commit_system(
                                                                                    history_cell::system::SystemCell::info(
                                                                                        format!(
                                                                                            "Agent {agent_id} not found in live or recent history."
                                                                                        ),
                                                                                    ),
                                                                                );
                                                                            }
                                                                            bottom_pane.sync_popups();
                                                                            frame_requester.schedule_frame();
                                                                            continue;
                                                                        }
                                                                    }
                                                                    BottomPaneAction::ViewCompleted {
                                                                        result: None,
                                                                        reopen: Some(cmd),
                                                                    } if ReopenTarget::parse(&cmd)
                                                                        == Some(ReopenTarget::Agents)
                                                                        && reopen_agents_view(
                                                                            &chat_widget,
                                                                            &mut bottom_pane,
                                                                            &frame_requester,
                                                                        ) =>
                                                                    {
                                                                        continue;
                                                                    }
                                                                    BottomPaneAction::Interrupt | BottomPaneAction::Quit => {
                                                                        // Fan out cancel to every in-flight
                                                                        // sub-agent TaskCell so Ctrl+C
                                                                        // doesn't just kill the parent turn
                                                                        // while children keep running in
                                                                        // the durable worker.
                                                                        let ids: Vec<String> = chat_widget
                                                                            .in_flight_task_ids()
                                                                            .to_vec();
                                                                        chat_widget.mark_agent_controls_cancelling(&ids);
                                                                        // Report the count of tasks the user
                                                                        // *targeted* with Ctrl+C, not just the
                                                                        // ones the durable-task service acked.
                                                                        // Service errors are logged separately;
                                                                        // a non-acked task is usually a
                                                                        // synchronous-spawn child the worker
                                                                        // never persisted, but the user's
                                                                        // intent was to interrupt all six
                                                                        // running children, so the banner
                                                                        // should say "Cancelled 6", not "1".
                                                                        let cancelled_count = ids.len();
                                                                        if !ids.is_empty()
                                                                            && let Some(ref svc) = task_service_for_cancel
                                                                        {
                                                                            let svc = svc.clone();
                                                                            let errs = super::cancel_fanout::fanout(
                                                                                &ids,
                                                                                move |id| {
                                                                                    let svc = svc.clone();
                                                                                    async move {
                                                                                        svc.update_status(
                                                                                            &id,
                                                                                            astra_services::TaskStatus::Cancelled,
                                                                                        )
                                                                                        .await
                                                                                    }
                                                                                },
                                                                            )
                                                                            .await;
                                                                            for (id, e) in &errs {
                                                                                tracing::warn!(
                                                                                    target: "astra_cli::tui",
                                                                                    task_id = %id,
                                                                                    error = %e,
                                                                                    "ctrl+c cancel fan-out: cancel rpc failed"
                                                                                );
                                                                            }
                                                                        }
                                                                        // Scrollback banner so the user sees
                                                                        // the cascade — silent on zero-task
                                                                        // Ctrl+C so routine interrupts stay
                                                                        // noise-free.
                                                                        chat_widget.commit_cancel_banner(cancelled_count);
                                                                        tui_cancel_token.cancel();
                                                                    }
                                                                    _ => {}
                                                                }
                                                            frame_requester.schedule_frame();
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                        }
                                                        TuiEvent::Resize | TuiEvent::Draw => {
                                                            {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                        }
                                                        _ => {}
                                                    }
                                                }
                                                Some(handoff) = bash_detach_handoff_rx.recv() => {
                                                    bash_detach_request_pending = false;
                                                    match handoff {
                                                        Ok(p) => {
                                                            let astra_tools::detach::DetachedShellPayload {
                                                                child,
                                                                stdout,
                                                                stderr,
                                                                command,
                                                                partial_stdout,
                                                                partial_stderr,
                                                                adoption_tx,
                                                            } = p;
                                                            let id = match background_registry.adopt_detached_shell(
                                                                child,
                                                                stdout,
                                                                stderr,
                                                                &command,
                                                                partial_stdout,
                                                                partial_stderr,
                                                            ) {
                                                                Ok(id) => {
                                                                    let _ = adoption_tx.send(Ok(id.clone()));
                                                                    id
                                                                },
                                                                Err(error) => {
                                                                    let _ = adoption_tx.send(Err(error.clone()));
                                                                    chat_widget.commit_system(
                                                                        history_cell::system::SystemCell::error(
                                                                            format!("⏎ Backgrounding failed: {error}")
                                                                        ),
                                                                    );
                                                                    set_bash_background_hint_enabled(
                                                                        &mut chat_widget,
                                                                        &mut status_indicator,
                                                                        false,
                                                                    );
                                                                    let _ = reveal_background_task_view_with_extra_rows(
                                                                        &mut background_registry,
                                                                        agent_spawner_for_cancel.as_ref(),
                                                                        &restored_local_agent_task_projections,
                                                                        &mut bottom_pane,
                                                                        &frame_requester,
                                                                        Vec::new(),
                                                                        None,
                                                                    )
                                                                    .await;
                                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                                    frame_requester.schedule_frame();
                                                                    if let Some(result) = turn_result_ready.take() {
                                                                        break result;
                                                                    }
                                                                    continue;
                                                                }
                                                            };
                                                            let selected_id = id.clone();
                                                            let _ = chat_widget.mark_active_bash_backgrounded(
                                                                active_bash_tool_use_id.as_deref(),
                                                                selected_id.as_str(),
                                                            );
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::info(
                                                                    format!("⏎ Backgrounded as {id}. Opened background task details; S stop, Esc list, Q close.")
                                                                ),
                                                            );
                                                            set_bash_background_hint_enabled(
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                                false,
                                                            );
                                                            persist_background_task_projections_if_changed(
                                                                &mut background_registry,
                                                                background_registry_turn_session_id.as_deref(),
                                                                background_registry_turn_model.as_deref(),
                                                                &mut background_task_projection_cache,
                                                            );
                                                            let _ = reveal_background_task_view(
                                                                &mut background_registry,
                                                                agent_spawner_for_cancel.as_ref(),
                                                                &restored_local_agent_task_projections,
                                                                &mut bottom_pane,
                                                                &frame_requester,
                                                                Some(selected_id.as_str()),
                                                            )
                                                            .await;
                                                        }
                                                        Err(error) => {
                                                            *bash_detach_slot_for_ctrl_b.lock().await = None;
                                                            chat_widget.commit_system(
                                                                history_cell::system::SystemCell::error(
                                                                    format!("⏎ Backgrounding failed: {error}")
                                                                ),
                                                            );
                                                            set_bash_background_hint_enabled(
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                                false,
                                                            );
                                                            let _ = reveal_background_task_view_with_extra_rows(
                                                                &mut background_registry,
                                                                agent_spawner_for_cancel.as_ref(),
                                                                &restored_local_agent_task_projections,
                                                                &mut bottom_pane,
                                                                &frame_requester,
                                                                Vec::new(),
                                                                None,
                                                            )
                                                            .await;
                                                        }
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                    frame_requester.schedule_frame();
                                                    if let Some(result) = turn_result_ready.take() {
                                                        break result;
                                                    }
                                                    continue;
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
                                                        TuiAppEvent::ExplainReport(items) if !items.is_empty() => {
                                                            explain_items.extend(items.clone());
                                                            continue;
                                                        }
                                                        _ => {}
                                                    }
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    // Shadow mirror into ChatWidget.
                                                    // Clone the event because handle_app_event
                                                    // consumes it by value on the app-event path.
                                                    if let Some(new_ev) = chat_widget::translate(
                                                        ae.clone(),
                                                        chat_widget::TurnContext::default(),
                                                    ) {
                                                        chat_widget.handle_event(new_ev);
                                                        refresh_open_agent_views_for_event(&ae, &chat_widget, &mut bottom_pane);
                                                    }
                                                    surface_status_line_system_cell(
                                                        &ae,
                                                        &mut chat_widget,
                                                    );
                                                    handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                                                    let should_rearm_bash_detach =
                                                        bash_detach_listener.is_none()
                                                            && match &ae {
                                                                TuiAppEvent::ToolStarted {
                                                                    name,
                                                                    description,
                                                                    tool_use_id,
                                                                    parent_tool_use_id,
                                                                    ..
                                                                } => {
                                                                    if name == "bash" && parent_tool_use_id.is_none() {
                                                                        active_bash_tool_use_id = Some(tool_use_id.clone());
                                                                        active_bash_description = Some(description.clone());
                                                                    }
                                                                    name == "bash"
                                                                }
                                                                TuiAppEvent::ToolCompleted {
                                                                    tool_use_id,
                                                                    ..
                                                                } => {
                                                                    if active_bash_tool_use_id.as_deref() == Some(tool_use_id.as_str()) {
                                                                        active_bash_tool_use_id = None;
                                                                        active_bash_description = None;
                                                                    }
                                                                    true
                                                                }
                                                                _ => false,
                                                            };
                                                    if should_rearm_bash_detach {
                                                        bash_detach_listener = Some(
                                                            install_bash_detach_listener(
                                                                &bash_detach_slot_for_ctrl_b,
                                                                &mut chat_widget,
                                                                &mut status_indicator,
                                                            )
                                                            .await,
                                                        );
                                                    }
                                                    let bash_hint_enabled =
                                                        bash_detach_hint_enabled(
                                                            bash_detach_listener.as_ref(),
                                                            &status_indicator,
                                                        );
                                                    set_bash_background_hint_enabled(
                                                        &mut chat_widget,
                                                        &mut status_indicator,
                                                        bash_hint_enabled,
                                                    );
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                }
                                                Some(req) = approval_rx.recv() => {
                                                    // Non-blocking: enqueue only. The live, interactive
                                                    // approval card is rendered by BottomPane above the
                                                    // composer so arrow-key focus is visible. Resolve
                                                    // events flush a compact audit line to scrollback.
                                                    let _id = if let Some(metadata) = req.metadata {
                                                        bottom_pane.enqueue_approval_with_metadata(
                                                            req.tool,
                                                            req.header,
                                                            req.detail,
                                                            req.reason,
                                                            req.args,
                                                            req.response_tx,
                                                            *metadata,
                                                        )
                                                    } else {
                                                        bottom_pane.enqueue_approval(
                                                            req.tool,
                                                            req.header,
                                                            req.detail,
                                                            req.reason,
                                                            req.args,
                                                            req.response_tx,
                                                        )
                                                    };
                                                    frame_requester.schedule_frame();
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                 }
                                                Some(req) = ask_user_rx.recv() => {
                                                    // Draft transition: show a brief
                                                    // indicator before the ask-user form
                                                    // opens so the user isn't surprised by
                                                    // a sudden modal.
                                                    chat_widget.commit_system(
                                                        crate::tui::history_cell::system::SystemCell::response(
                                                            "🤔 The agent needs your input — opening question…",
                                                        ),
                                                    );
                                                    bottom_pane.enqueue_ask_user(req.prompt, req.response_tx);
                                                    frame_requester.schedule_frame();
                                                    {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                }
                                                }
                                                Some(req) = plan_review_rx.recv() => {
                                                    bottom_pane.enqueue_plan_review(req.plan_markdown, req.response_tx);
                                                    frame_requester.schedule_frame();
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    let frame = active_viewport(
                                                        &chat_widget,
                                                        &status_indicator,
                                                        Some(&*task_board),
                                                        board_expanded,
                                                        board_user_pin,
                                                        w,
                                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                                    );
                                                    board_expanded = frame.resolved_board_expanded;
                                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                                }
                                                _ = &mut itick => {
                                                    // Refresh the permission-mode chip via the
                                                    // lock-free mirror — the agentic loop holds
                                                    // `&mut state` so reading `state.perm_manager`
                                                    // here would clash. Catches turn-boundary
                                                    // pivots (e.g. exit_plan_mode → Auto) within
                                                    // one inner tick.
                                                    let live_mode = perm_mode_mirror.current();
                                                    if bottom_pane.footer.permission_mode
                                                        != Some(live_mode)
                                                    {
                                                        bottom_pane.footer.permission_mode = Some(live_mode);
                                                    }
                                                    // Drain the bg-task command queue so a
                                                    // tool-side `task_output(block=true)` waiting
                                                    // for a snapshot reply makes progress while
                                                    // this turn is still in flight. The outer
                                                    // tick is parked on `&mut fut`, so without
                                                    // this drainer the tool waits forever.
                                                    drain_bg_task_commands(
                                                        &bg_task_commands_for_tick,
                                                        &mut background_registry,
                                                        agent_spawner_for_tick.as_ref(),
                                                        &restored_local_agent_task_projections,
                                                    )
                                                    .await;
                                                    // Move terminal completions onto the handle
                                                    // status so the next snapshot reply names
                                                    // the task as completed/failed/killed.
                                                    // Inner-tick poll yields events we drop:
                                                    // the outer tick will pick up the next
                                                    // `poll_completions()` (events stay queued
                                                    // until consumed there) and emit the system
                                                    // message + task_notification.
                                                    background_registry.drain_join_set();
                                                    let bash_hint_enabled =
                                                        bash_detach_hint_enabled(
                                                            bash_detach_listener.as_ref(),
                                                            &status_indicator,
                                                        );
                                                    set_bash_background_hint_enabled(
                                                        &mut chat_widget,
                                                        &mut status_indicator,
                                                        bash_hint_enabled,
                                                    );
                                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                                    board_expanded = frame.resolved_board_expanded;
                                                    let _ = do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board);
                                                }
                                            }
                                        };
                                        r
                                    };

                                    state.tui_stream_event_tx = None;
                                    state.tui_agent_live_event_sink = None;

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
                                                    TuiAppEvent::ExplainReport(items) if !items.is_empty() => {
                                                        explain_items.extend(items.clone());
                                                        continue;
                                                    }
                                                    _ => {}
                                                }
                                                if let Some(new_ev) = chat_widget::translate(
                                                    ae.clone(),
                                                    chat_widget::TurnContext::default(),
                                                ) {
                                                    chat_widget.handle_event(new_ev);
                                                        refresh_open_agent_views_for_event(&ae, &chat_widget, &mut bottom_pane);
                                                }
                                                surface_status_line_system_cell(
                                                    &ae,
                                                    &mut chat_widget,
                                                );
                                                handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                            }
                                        }
                                    }

                                    if turn_result.is_ok()
                                        && commit_explain_dag(
                                            &state,
                                            &explain_items,
                                            pre_cached_context_trace_turn_id.as_deref(),
                                            pre_context_trace_count,
                                            &mut chat_widget,
                                        )
                                    {
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    }

                                    // Turn end — ChatWidget handles any
                                    // remaining live cell on TurnComplete.
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    set_bash_background_hint_enabled(
                                        &mut chat_widget,
                                        &mut status_indicator,
                                        false,
                                    );

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
                                        task_board.rebind_session(sid.clone());
                                        board_user_pin = None;
                                    }
                                    if let Err(ref e) = turn_result {
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
                                    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
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
                                    let turn_input = total_input_tokens(
                                        turn_prompt,
                                        turn_cache_read,
                                        turn_cache_creation,
                                    );
                                    bottom_pane.footer.token_budget =
                                        Some((turn_input, 200_000));

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
                                            tokens_in: Some(turn_input),
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

                                }
                            }
                            BottomPaneAction::OpenExternalEditor(initial) => {
                                let _ = crossterm::terminal::disable_raw_mode();
                                let _ = crossterm::execute!(
                                    std::io::stdout(),
                                    crossterm::event::DisableBracketedPaste,
                                    crossterm::cursor::Show
                                );
                                let edit_result =
                                    crate::tui::external_editor::edit_in_external_editor(&initial);
                                if let Err(err) = guard.ensure_tui_modes() {
                                    chat_widget.commit_system(history_cell::system::SystemCell::error(
                                        format!("Failed to restore TUI modes after external editor: {err}"),
                                    ));
                                }
                                guard.terminal.invalidate_viewport();
                                match edit_result {
                                    Ok(edited) => {
                                        bottom_pane.replace_composer_text(&edited);
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::response(
                                                "  ⎿  loaded draft from external editor".to_string(),
                                            ),
                                        );
                                    }
                                    Err(error) => {
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::error(format!(
                                                "External editor failed: {error}"
                                            )),
                                        );
                                    }
                                }
                            }
                            BottomPaneAction::ViewSideEffect { result } => {
                                // View emitted a sentinel but stayed open
                                // (e.g. Ctrl+G drill view's `x` kill).
                                // Route the sentinel to the appropriate
                                // handler without popping the view.
                                if try_dispatch_background_task_stop_sentinel(
                                    &result,
                                    &mut background_registry,
                                    state.agent_spawner.clone(),
                                    &restored_local_agent_task_projections,
                                    &mut chat_widget,
                                    &mut bottom_pane,
                                    &frame_requester,
                                )
                                .await
                                {
                                    continue;
                                }
                                if try_dispatch_background_task_output_sentinel(
                                    &result,
                                    &mut background_registry,
                                    state.agent_spawner.clone(),
                                    &restored_local_agent_task_projections,
                                    &mut chat_widget,
                                    &mut bottom_pane,
                                    &frame_requester,
                                )
                                .await
                                {
                                    continue;
                                }
                                let _ = try_dispatch_agent_kill_sentinel(
                                    &result,
                                    state.agent_spawner.clone(),
                                    state.task_service.clone(),
                                    &mut chat_widget,
                                    &mut bottom_pane,
                                    &frame_requester,
                                );
                                // Refresh strip rows so the view sees
                                // Cancelling status immediately.
                                let rows = chat_widget
                                    .agents_drilldown_rows(AGENT_DRILLDOWN_RECENT_COMPLETED);
                                bottom_pane.refresh_agent_rows(rows);
                                frame_requester.schedule_frame();
                            }
                            BottomPaneAction::ViewCompleted { result, reopen } => {
                                if let Some(name) = result {
                                    if try_dispatch_background_task_stop_sentinel(
                                        &name,
                                        &mut background_registry,
                                        state.agent_spawner.clone(),
                                        &restored_local_agent_task_projections,
                                        &mut chat_widget,
                                        &mut bottom_pane,
                                        &frame_requester,
                                    )
                                    .await
                                    {
                                        continue;
                                    }
                                    if try_dispatch_background_task_output_sentinel(
                                        &name,
                                        &mut background_registry,
                                        state.agent_spawner.clone(),
                                        &restored_local_agent_task_projections,
                                        &mut chat_widget,
                                        &mut bottom_pane,
                                        &frame_requester,
                                    )
                                    .await
                                    {
                                        continue;
                                    }
                                    // Kill sentinel: route to the spawner / task service before
                                    // any drilldown handling.
                                    if try_dispatch_agent_kill_sentinel(
                                        &name,
                                        state.agent_spawner.clone(),
                                        state.task_service.clone(),
                                        &mut chat_widget,
                                        &mut bottom_pane,
                                        &frame_requester,
                                    ) {
                                        continue;
                                    }
                                    // LoginView / RegisterView completion:
                                    // Agent drill-in sentinel: user
                                    // selected one parallel agent in
                                    // InFlightAgentsView; open its
                                    // TaskDetailView. We check both
                                    // the live register AND history
                                    // so completed agents are still
                                    // drillable (Ctrl+G surfaces them
                                    // via `agents_drilldown_rows`).
                                    if let Some(agent_id) = bottom_pane::in_flight_agents_view::parse_drilldown_sentinel(&name) {
                                        if let Some(tc) =
                                            chat_widget.task_cell_anywhere(agent_id)
                                        {
                                            use bottom_pane::task_detail_view::TaskDetailView;
                                            bottom_pane.push_view(Box::new(
                                                TaskDetailView::from_task_cell(tc)
                                                    .with_live_task_id(agent_id.to_string()),
                                            ));
                                        } else {
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::info(
                                                    format!(
                                                        "Agent {agent_id} not found in live or recent history."
                                                    ),
                                                ),
                                            );
                                        }
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    if let Some(choice) =
                                        name.strip_prefix(WORKSPACE_TRUST_SENTINEL)
                                    {
                                        match choice {
                                            "Trust Workspace" => {
                                                match state.perm_manager.trust_workspace() {
                                                    Ok(message) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::response(
                                                            message,
                                                        ),
                                                    ),
                                                    Err(err) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!(
                                                                "Failed to trust workspace: {err}"
                                                            ),
                                                        ),
                                                    ),
                                                }
                                            }
                                            "Continue This Session" => {
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::info(
                                                        "Continuing without trusting this workspace. Saved workspace rules stay off for this session."
                                                            .to_string(),
                                                    ),
                                                );
                                            }
                                            "Mark Untrusted" => {
                                                match state.perm_manager.untrust_workspace() {
                                                    Ok(message) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::response(
                                                            message,
                                                        ),
                                                    ),
                                                    Err(err) => chat_widget.commit_system(
                                                        history_cell::system::SystemCell::error(
                                                            format!(
                                                                "Failed to mark workspace untrusted: {err}"
                                                            ),
                                                        ),
                                                    ),
                                                }
                                            }
                                            _ => {}
                                        }
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    // credentials arrive as a sentinel-
                                    // prefixed string so we can dispatch
                                    // auth without leaving the TUI (no
                                    // more rpassword against bare terminal).
                                    if let Some(rest) = name.strip_prefix("__login__\n") {
                                        let mut parts = rest.splitn(2, '\n');
                                        let username = parts.next().unwrap_or("").to_string();
                                        let password = parts.next().unwrap_or("").to_string();
                                        match crate::cli::auth_flow::do_login(api, profile, &username, &password).await {
                                            Ok(_) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::response(format!("Logged in as {username}")));
                                                crate::post_auth_cloud_resync(profile, &mut state).await;
                                            }
                                            Err(e) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::error(format!("Login failed: {e}")));
                                            }
                                        }
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    if let Some(rest) = name.strip_prefix("__register__\n") {
                                        let mut parts = rest.splitn(3, '\n');
                                        let username = parts.next().unwrap_or("").to_string();
                                        let email = parts.next().unwrap_or("").to_string();
                                        let password = parts.next().unwrap_or("").to_string();
                                        match crate::cli::auth_flow::do_register(api, profile, &username, &email, &password).await {
                                            Ok(_) => {
                                                chat_widget.commit_system(history_cell::system::SystemCell::response("Registered — logging in…"));
                                                match crate::cli::auth_flow::do_login(api, profile, &username, &password).await {
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
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }
                                    // /config edit completion.
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
                                                }
                                                history_cell::system::SystemCell::response(outcome.message)
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

                                    // `/model` picker → check thinking capability.
                                    if let Some(base_model) =
                                        name.strip_prefix(slash_dispatch::MODEL_PICK_SENTINEL)
                                    {
                                        let base_model = base_model.to_string();
                                        let token = crate::cli::session::session_runtime::fresh_access_token(api, profile).await;
                                        let raw = crate::cli::slash::slash_router::fetch_model_list_raw(
                                            api,
                                            token.as_deref(),
                                        )
                                        .await
                                        .unwrap_or_default();
                                        let entry = crate::cli::slash::slash_router::find_model_entry_by_name(
                                            &raw,
                                            &base_model,
                                        );
                                        let thinking_cap = entry
                                            .and_then(crate::cli::slash::slash_router::entry_thinking_capability);
                                        let provider =
                                            entry.and_then(crate::cli::slash::slash_router::entry_provider);
                                        let opts = astra_turn_core::thinking_config::thinking_options_with_capability(
                                            &base_model,
                                            provider,
                                            thinking_cap,
                                        );
                                        if opts.is_empty() {
                                            state.model = Some(base_model.clone());
                                            crate::cli::slash::slash_config::set_active_model_for_display(
                                                Some(base_model.clone()),
                                            );
                                            bottom_pane.footer.model = Some(base_model.clone());
                                            chat_widget.commit_system(
                                                history_cell::system::SystemCell::response(
                                                    format!("Set model to {base_model}"),
                                                ),
                                            );
                                            pending_deferred_slash_flush = false;
                                            let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                            flush_chat_widget(&mut guard, &mut chat_widget, w);
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
                                                Some(format!("Select thinking mode for {base_model}:")),
                                            )
                                            .with_footer_hint(
                                                slash_dispatch::MODEL_THINKING_PICKER_FOOTER_HINT,
                                            )
                                            .with_result_prefix(prefix);
                                            bottom_pane.push_view(Box::new(view));
                                        }
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // `/model` thinking-mode picker.
                                    if let Some(rest) =
                                        name.strip_prefix(slash_dispatch::MODEL_THINKING_SENTINEL)
                                    {
                                        let mut parts = rest.splitn(2, '\n');
                                        let base_model = parts.next().unwrap_or("").to_string();
                                        let label = parts.next().unwrap_or("").to_string();
                                        let token = crate::cli::session::session_runtime::fresh_access_token(api, profile).await;
                                        let raw = crate::cli::slash::slash_router::fetch_model_list_raw(
                                            api,
                                            token.as_deref(),
                                        )
                                        .await
                                        .unwrap_or_default();
                                        let entry = crate::cli::slash::slash_router::find_model_entry_by_name(
                                            &raw,
                                            &base_model,
                                        );
                                        let provider =
                                            entry.and_then(crate::cli::slash::slash_router::entry_provider);
                                        let thinking_cap = entry
                                            .and_then(crate::cli::slash::slash_router::entry_thinking_capability);
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
                                                chat_widget.commit_system(
                                                    history_cell::system::SystemCell::error(format!(
                                                        "Thinking mode `{label}` is no longer available for {base_model}; model unchanged."
                                                    )),
                                                );
                                                pending_deferred_slash_flush = false;
                                                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                                flush_chat_widget(&mut guard, &mut chat_widget, w);
                                                bottom_pane.sync_popups();
                                                frame_requester.schedule_frame();
                                                continue;
                                            }
                                        };
                                        let composed = format!("{base_model}{suffix}");
                                        state.model = Some(composed.clone());
                                        crate::cli::slash::slash_config::set_active_model_for_display(
                                            Some(composed.clone()),
                                        );
                                        bottom_pane.footer.model = Some(composed.clone());
                                        chat_widget.commit_system(
                                            history_cell::system::SystemCell::response(format!(
                                                "Set model to {composed}"
                                            )),
                                        );
                                        pending_deferred_slash_flush = false;
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        flush_chat_widget(&mut guard, &mut chat_widget, w);
                                        bottom_pane.sync_popups();
                                        frame_requester.schedule_frame();
                                        continue;
                                    }

                                    // Session picker result → run the async
                                    // `/resume <id>` pipeline via the usual
                                    // slash fallback path.
                                    if slash_dispatch::looks_like_session_id(&name) {
                                        let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                        let pre_sid = state.session_id.clone();
                                        let slash_text = format!("/resume {name}");
                                        let slash_result = guard.with_restored(|| async {
                                            let token = crate::cli::session::session_runtime::fresh_access_token(api, profile).await;
                                            crate::cli::slash::slash_router::handle_slash_command(
                                                &slash_text, api, profile, &mut state,
                                                token.as_deref(),
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
                                            task_board.rebind_session(new_sid.clone());
                                            board_user_pin = None;
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
                                    bottom_pane.footer.permission_mode = Some(state.perm_manager.mode());
                                    // Clear the deferred-flush flag for any
                                    // ViewCompleted-with-name path that fell
                                    // through to here without an explicit
                                    // clear (looks_like_session_id, generic
                                    // handle_view_result). Without this,
                                    // ambient TUI flushes stay suppressed
                                    // for the rest of the session.
                                    pending_deferred_slash_flush = false;
                                } else if pending_deferred_slash_flush {
                                    // The deferred view returned with no
                                    // result name (typically an Esc-cancel
                                    // or any slash that consumed the action
                                    // entirely). Cells committed during the
                                    // deferred window — e.g. background
                                    // permission auto-approval banners
                                    // surfaced by `surface_status_line_system_cell`
                                    // — must still land in scrollback. The
                                    // original code skipped flush and only
                                    // advanced the watermark, dropping
                                    // those cells silently. `flush_chat_widget`
                                    // both renders pending cells AND
                                    // advances the watermark via
                                    // `drain_new_committed`, so it
                                    // subsumes the bare `mark_all_flushed`.
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                                    pending_deferred_slash_flush = false;
                                } else if reopen
                                    .as_deref()
                                    .and_then(ReopenTarget::parse)
                                    == Some(ReopenTarget::Agents)
                                {
                                    let _ = reopen_agents_view(
                                        &chat_widget,
                                        &mut bottom_pane,
                                        &frame_requester,
                                    );
                                    // Reopen-Agents path: a deferred slash
                                    // could have set this flag and only
                                    // requested an Agents view reopen on
                                    // close. Clear so ambient flushes
                                    // resume.
                                    pending_deferred_slash_flush = false;
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
                                    // Generic reopen path: same rationale
                                    // as the Agents branch above.
                                    pending_deferred_slash_flush = false;
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
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board)?;
                                }
                    }
                    TuiEvent::Draw => {
                        {
                                    let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                                    let frame = active_viewport(
                                        &chat_widget,
                                        &status_indicator,
                                        Some(&*task_board),
                                        board_expanded,
                                        board_user_pin,
                                        w,
                                        guard.terminal.size().map(|s| s.height).unwrap_or(24),
                                    );
                                    board_expanded = frame.resolved_board_expanded;
                                    do_draw(&mut guard, frame.active, frame.multi_agent, &mut bottom_pane, Some((&*task_board, board_expanded)), frame.task_board)?;
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
                    refresh_open_agent_views_for_event(&ae, &chat_widget, &mut bottom_pane);
                }
                surface_status_line_system_cell(&ae, &mut chat_widget);
                handle_app_event(&ae, &mut bottom_pane, &mut status_indicator, &frame_requester);
                if should_flush_ambient_commits(pending_deferred_slash_flush) {
                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                }
            }
            _ = &mut tick => {
                // Re-derive permission-mode chip from live state so
                // mode pivots driven by the agentic loop (e.g. the
                // `exit_plan_mode` overlay handing the next turn back
                // to Auto) reach the status line within one tick
                // instead of waiting for the next turn boundary that
                // happens to call `refresh_footer_from_state`. Cheap:
                // a string format and an Option<u64> compare per 50ms.
                let live_mode_enum = state.perm_manager.mode();
                if bottom_pane.footer.permission_mode != Some(live_mode_enum) {
                    bottom_pane.footer.permission_mode = Some(live_mode_enum);
                    // Mode just shifted (driven by host-side
                    // pull_mode_from_mirror after exit_plan_mode
                    // overlay or mid-turn Shift+Tab). Re-evaluate
                    // the approval queue against the new mode so
                    // any pending entries the new mode would
                    // auto-approve are released — same machinery
                    // the keystroke paths use.
                    let released = bottom_pane.reevaluate_approvals_for_mode(live_mode_enum);
                    if released > 0 {
                        chat_widget.commit_system(
                            crate::tui::history_cell::system::SystemCell::response(
                                format!(
                                    "  ✓ {released} pending approval(s) auto-resolved by the new mode",
                                ),
                            ),
                        );
                    }
                    frame_requester.schedule_frame();
                }
                // Pulse the chat-widget scrollback so if any async
                // event was handled since the last draw the new
                // cells land promptly instead of waiting for the
                // next event edge.
                let w = guard.terminal.size().map(|s| s.width).unwrap_or(80);
                if should_flush_ambient_commits(pending_deferred_slash_flush) {
                    flush_chat_widget(&mut guard, &mut chat_widget, w);
                }
                // If a cell is streaming, request a redraw so the
                // gradient gutter on `LiveFramedCell` keeps flowing.
                // Without this, the gutter only redraws on incoming
                // delta/state events and appears stuck. (PR #335)
                if chat_widget.active_cell().is_some_and(|c| c.is_live()) {
                    frame_requester.schedule_frame();
                }
                // Poll the task-board observer. No-op most ticks
                // (gated by POLL_INTERVAL); spawns a one-shot fetch
                // when due. Visibility now flows through the pure
                // `board_pin::resolve_board_visibility` state
                // machine so auto-open/hide never fights with the
                // user's explicit Ctrl+T pin.
                if state.session_id != background_registry_session_id {
                    persist_background_task_projections_if_changed(
                        &mut background_registry,
                        background_registry_session_id.as_deref(),
                        state.model.as_deref(),
                        &mut background_task_projection_cache,
                    );
                    let _ = persist_background_local_agent_task_projections_if_changed(
                        state.agent_spawner.as_ref(),
                        &restored_local_agent_task_projections,
                        background_registry_session_id.as_deref(),
                        state.model.as_deref(),
                        &mut background_local_agent_projection_cache,
                    )
                    .await;
                    let old_session_was_unbound = background_registry_session_id
                        .as_deref()
                        .is_none_or(|sid| sid.is_empty());
                    let new_session_is_bound = state
                        .session_id
                        .as_deref()
                        .is_some_and(|sid| !sid.is_empty());
                    if old_session_was_unbound && new_session_is_bound {
                        background_registry
                            .rebind_output_dir(background_task_output_dir(state.session_id.as_deref()));
                        background_task_projection_cache = Vec::new();
                        background_local_agent_projection_cache = Vec::new();
                        background_registry_session_id = state.session_id.clone();
                    } else {
                        background_registry.kill_all();
                        background_registry = super::background_tasks::BackgroundTaskRegistry::new(
                            background_task_output_dir(state.session_id.as_deref()),
                        );
                        restored_local_agent_task_projections =
                            restore_background_task_projections(
                                &mut background_registry,
                                state.session_id.as_deref(),
                            );
                        background_task_projection_cache =
                            background_registry.export_shell_task_projections();
                        background_local_agent_projection_cache =
                            restored_local_agent_task_projections.clone();
                        background_registry_session_id = state.session_id.clone();
                    }
                }
                // Drain background shell commands from the tool executor.
                drain_bg_task_commands(
                    &state.bg_task_commands,
                    &mut background_registry,
                    state.agent_spawner.as_ref(),
                    &restored_local_agent_task_projections,
                )
                .await;

                // Poll background shell completions.
                let bg_events = background_registry.poll_completions();
                for ev in &bg_events {
                    let notification = super::background_tasks::format_notification_xml(ev);
                    if !notification.is_empty() {
                        state.pending_bg_notifications.push(notification);
                    }
                }
                for msg in background_task_event_system_messages(&bg_events) {
                    chat_widget.commit_system(
                        history_cell::system::SystemCell::info(&msg),
                    );
                    frame_requester.schedule_frame();
                }
                // Stall check every tick (internal timer gates at 5s intervals).
                background_registry.stall_check();
                persist_background_task_projections_if_changed(
                    &mut background_registry,
                    state.session_id.as_deref(),
                    state.model.as_deref(),
                    &mut background_task_projection_cache,
                );
                restored_local_agent_task_projections =
                    persist_background_local_agent_task_projections_if_changed(
                        state.agent_spawner.as_ref(),
                        &restored_local_agent_task_projections,
                        state.session_id.as_deref(),
                        state.model.as_deref(),
                        &mut background_local_agent_projection_cache,
                    )
                    .await;
                let rows_for_footer =
                    background_task_rows_with_agents(
                        &mut background_registry,
                        state.agent_spawner.as_ref(),
                        &restored_local_agent_task_projections,
                    )
                    .await;
                if bottom_pane.accepts_background_task_rows() {
                    bottom_pane.refresh_background_task_rows(rows_for_footer.clone());
                    frame_requester.schedule_frame();
                }

                // Surface bg task counts on the status line from the
                // same typed rows used by the switcher. That keeps
                // footer and list projection in lock-step as more
                // task kinds land.
                sync_background_task_footer_from_rows(&mut bottom_pane, &rows_for_footer);

                task_board.maybe_refresh();
                let snap = task_board.snapshot();
                let has_tasks = !snap.tasks.is_empty();
                let (next_expanded, reset_pin) = super::board_pin::resolve_board_visibility(
                    board_expanded,
                    board_user_pin,
                    has_tasks,
                    snap.hidden,
                );
                if reset_pin {
                    board_user_pin = None;
                }
                if next_expanded != board_expanded {
                    board_expanded = next_expanded;
                    frame_requester.schedule_frame();
                }
            }
        }
    };
    // Clean up background shells on exit.
    background_registry.kill_all();
    drop(guard);
    result
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
        TuiAppEvent::AgentControlStarted { label, .. } => {
            bottom_pane.set_task_status(TaskStatus::ToolExecuting {
                name: label.clone(),
                started_at: now,
            });
            status_indicator.set_state(status_indicator::IndicatorState::Tool {
                name: label.clone(),
                started_at: now,
            });
        }
        TuiAppEvent::ToolCompleted { .. } => {
            // Flip back to thinking; the ChatWidget committed the
            // tool cell in its own event handler.
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::AgentControlCompleted { .. } => {
            status_indicator
                .set_state(status_indicator::IndicatorState::Thinking { started_at: now });
        }
        TuiAppEvent::ToolOutput { .. } => {
            // Progress ticks handled by ChatWidget via the bridge.
        }
        TuiAppEvent::AgentLive(_)
        | TuiAppEvent::AgentLiveBatch(_)
        | TuiAppEvent::StatusLine(_)
        | TuiAppEvent::Compaction(_)
        | TuiAppEvent::ExplainReport(_)
        | TuiAppEvent::VerdictReport(_)
        | TuiAppEvent::TurnWarning(_)
        | TuiAppEvent::TurnInfo(_)
        | TuiAppEvent::PermissionAutoApproved { .. } => {}
        TuiAppEvent::TurnComplete | TuiAppEvent::TurnError(_) => {
            bottom_pane.set_task_status(TaskStatus::Idle);
            status_indicator.set_state(status_indicator::IndicatorState::Idle);
        }
    }
    fr.schedule_frame();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::turn::local_run_control::LocalDeferredInputRunControl;
    use crate::tui::background_tasks::BgTaskEvent;
    use astra_runtime::turn::run_control::RunInputProvider;
    use astra_turn_core::orchestration_spawn_tool::{SpawnAgentInput, SpawnAgentOutput};
    use astra_turn_core::orchestration_types::{
        AgentStatus, SpawnedAgentInfo, SpawnedAgentMetrics,
    };
    use std::path::PathBuf;

    async fn wait_for_background_shell_terminal(
        registry: &mut crate::tui::background_tasks::BackgroundTaskRegistry,
        id: &str,
    ) {
        crate::tests::wait_until(
            std::time::Duration::from_secs(3),
            std::time::Duration::from_millis(25),
            || {
                registry.drain_join_set();
                registry
                    .get(id)
                    .map(|handle| {
                        matches!(handle.projected_status(), "completed" | "failed" | "killed")
                    })
                    .unwrap_or(false)
            },
        )
        .await
        .unwrap_or_else(|()| {
            let status = registry
                .get(id)
                .map(|handle| handle.projected_status())
                .unwrap_or("missing");
            panic!("background shell {id} did not terminate; current status: {status}");
        });
    }

    fn agent_info(
        agent_id: &str,
        status: AgentStatus,
        run_in_background: bool,
    ) -> SpawnedAgentInfo {
        SpawnedAgentInfo {
            agent_id: agent_id.to_string(),
            run_id: format!("run-{agent_id}"),
            parent_run_id: "root".to_string(),
            agent_type: "task".to_string(),
            description: "review auth flow".to_string(),
            status,
            started_at: std::time::SystemTime::now(),
            metrics: SpawnedAgentMetrics::default(),
            has_permission_issues: false,
            run_in_background,
            fanout_slot: None,
        }
    }

    fn test_spawn_context() -> astra_runtime::orchestration::SpawnContext {
        astra_runtime::orchestration::SpawnContext {
            parent_run_id: "root".to_string(),
            parent_agent_id: "root".to_string(),
            recursion_depth: 0,
            parent_is_fork_child: false,
            working_dir: PathBuf::from("/tmp"),
            inherited_permissions: None,
            inherited_skills: Vec::new(),
            live_event_sink: None,
            trace_context: None,
            spawn_tool_call_id: None,
        }
    }

    fn test_agent_spawner(
        executor: Arc<dyn astra_runtime::orchestration::SpawnAgentExecutor>,
    ) -> Arc<astra_runtime::orchestration::DynamicAgentSpawner> {
        let transport = Arc::new(astra_messaging::InProcessTransport::new());
        let tracker = Arc::new(astra_runtime::server::delegation::engine::DelegationTracker::new());
        let router = Arc::new(astra_messaging::AgentMailboxRouter::new(transport, tracker));
        Arc::new(
            astra_runtime::orchestration::DynamicAgentSpawner::new(router).with_executor(executor),
        )
    }

    struct PendingAgentExecutor;

    #[async_trait::async_trait]
    impl astra_runtime::orchestration::SpawnAgentExecutor for PendingAgentExecutor {
        async fn execute(
            &self,
            _config: astra_runtime::orchestration::SpawnRunConfig,
        ) -> Result<astra_runtime::orchestration::SpawnRunResult, String> {
            std::future::pending::<Result<astra_runtime::orchestration::SpawnRunResult, String>>()
                .await
        }
    }

    /// REGRESSION (reviewer L3 — Architecture): the
    /// `ReopenTarget::as_str() ↔ ReopenTarget::parse()` round-trip
    /// MUST be lossless for every variant. The dispatcher channel
    /// (`BottomPaneAction::ViewCompleted { reopen: Option<String> }`)
    /// carries the string-form across the view boundary, so a typo
    /// in the constant would silently break re-open semantics
    /// without compile-checking. Pin every variant — when a future
    /// `ReopenTarget::Foo` lands, add it to the array below and the
    /// round-trip check covers it.
    #[test]
    fn reopen_target_round_trips_through_string() {
        let variants: &[ReopenTarget] = &[ReopenTarget::Agents];
        for &target in variants {
            let encoded = target.as_str();
            let decoded = ReopenTarget::parse(encoded).expect("known variant must round-trip");
            assert_eq!(decoded, target, "variant {encoded} did not round-trip");
        }
    }

    #[test]
    fn ctrl_b_promoted_agent_message_is_user_facing() {
        let message = ctrl_b_promoted_agent_message("reviewer@run-1", "review auth");

        assert!(message.contains("Backgrounded agent reviewer@run-1"));
        assert!(message.contains("review auth"));
        assert!(message.contains("Opened background tasks"));
        assert!(!message.contains("agent(action="), "{message}");
        assert!(!message.contains("task_output"), "{message}");
        assert!(!message.contains("job("), "{message}");
    }

    #[test]
    fn background_task_row_for_local_agent_only_projects_background_agents() {
        let foreground = agent_info(
            "agent-foreground",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
            false,
        );
        assert!(
            background_task_row_for_local_agent(&foreground).is_none(),
            "foreground sync agents must not appear in the background task footer"
        );

        let background = agent_info(
            "agent-background",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
            true,
        );
        let row = background_task_row_for_local_agent(&background)
            .expect("background agent should project to a task row");
        assert_eq!(
            row.kind,
            bottom_pane::background_task_view::BackgroundTaskKind::LocalAgent
        );
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Running
        );
        assert_eq!(row.title, "review auth flow");

        let counts = status_line::BackgroundTaskCounts::from_rows(&[row]);
        assert_eq!(counts.local_agents, 1);
        assert_eq!(counts.running, 0);
    }

    #[test]
    fn failed_background_agent_projects_as_failed_local_agent_attention() {
        let failed = agent_info(
            "agent-failed",
            AgentStatus::Failed {
                error: "review failed".into(),
                finish_reason: Some("failed".into()),
            },
            true,
        );
        let row = background_task_row_for_local_agent(&failed)
            .expect("failed background agent should remain reachable");

        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Failed
        );
        assert_eq!(row.output_tail.as_deref(), Some("review failed"));
        assert_eq!(row.terminal_reason.as_deref(), Some("failed"));

        let counts = status_line::BackgroundTaskCounts::from_rows(&[row]);
        assert_eq!(counts.failed_local_agents, 1);
    }

    #[test]
    fn background_task_rows_xml_projects_local_agent_rows() {
        let agent = agent_info(
            "agent-1",
            AgentStatus::Running {
                activity: "reviewing auth middleware".into(),
            },
            true,
        );
        let row = background_task_row_for_local_agent(&agent)
            .expect("background agent should project to a task row");

        let xml = render_background_task_rows_xml(&[row]);

        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("id=\"agent-1\""), "{xml}");
        assert!(xml.contains("kind=\"local agent\""), "{xml}");
        assert!(xml.contains("status=\"running\""), "{xml}");
        assert!(xml.contains("description=\"review auth flow\""), "{xml}");
        assert!(
            xml.contains("preview=\"reviewing auth middleware\""),
            "{xml}"
        );
        assert!(!xml.contains("Job"), "{xml}");
    }

    #[test]
    fn background_local_agent_row_preserves_fanout_membership_for_footer_and_switcher() {
        let mut agent = agent_info(
            "agent-auth",
            AgentStatus::Running {
                activity: "reviewing auth middleware".into(),
            },
            true,
        );
        agent.fanout_slot = Some(
            astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity::new(
                "review-1", 3, 0,
            )
            .unwrap(),
        );

        let row =
            background_task_row_for_local_agent_with_fanout_title(&agent, Some("review fanout"))
                .expect("background fanout agent should project to a task row");
        let fanout = row.fanout.as_ref().expect("fanout metadata");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 0);

        let summaries =
            status_line::BackgroundTaskFanoutSummary::from_rows(std::slice::from_ref(&row));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].target_count, 3);
        assert_eq!(summaries[0].running, 1);

        let xml = render_background_task_rows_xml(&[row]);
        assert!(xml.contains("fanout_group_id=\"review-1\""), "{xml}");
        assert!(
            xml.contains("fanout_group_title=\"review fanout\""),
            "{xml}"
        );
        assert!(xml.contains("fanout_target_count=\"3\""), "{xml}");
        assert!(xml.contains("fanout_slot_index=\"0\""), "{xml}");
    }

    #[test]
    fn background_task_row_projects_rejected_fanout_slot_without_agent_history() {
        let mut group =
            astra_turn_core::orchestration_fanout_group::AgentFanoutGroupProjection::new(
                "review-1",
                "review fanout",
                3,
            );
        group
            .set_slot_request(1, "api reviewer", "review API surface")
            .unwrap();
        group
            .record_spawn_rejected(1, "concurrency cap reached")
            .unwrap();

        let row = background_task_row_for_rejected_fanout_slot(&group, &group.slots[1])
            .expect("rejected fanout slot should project to a task row");

        assert_eq!(row.id, "fanout:review-1:slot:1:spawn_rejected");
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Failed
        );
        assert_eq!(row.title, "review API surface");
        assert_eq!(row.output_tail.as_deref(), Some("concurrency cap reached"));
        assert_eq!(
            row.terminal_reason.as_deref(),
            Some("concurrency cap reached")
        );
        assert_eq!(
            row.live_control,
            bottom_pane::background_task_view::LiveControlState::UnsupportedInMode
        );

        let fanout = row.fanout.as_ref().expect("fanout metadata");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 1);
        assert_eq!(fanout.slot_label, "review API surface");

        let summaries =
            status_line::BackgroundTaskFanoutSummary::from_rows(std::slice::from_ref(&row));
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].failed, 1);

        let xml = render_background_task_rows_xml(std::slice::from_ref(&row));
        assert!(
            xml.contains("id=\"fanout:review-1:slot:1:spawn_rejected\""),
            "{xml}"
        );
        assert!(xml.contains("status=\"failed\""), "{xml}");
        assert!(xml.contains("preview=\"concurrency cap reached\""), "{xml}");

        let snapshot =
            background_task_output_snapshot_for_rejected_fanout_slot(&group, &group.slots[1], 0, 9)
                .expect("snapshot");
        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.status, "failed");
        assert!(snapshot.terminal);
        assert_eq!(snapshot.output, "concurren");
        assert_eq!(snapshot.total_bytes, "concurrency cap reached".len() as u64);
    }

    #[test]
    fn background_task_output_snapshot_projects_local_agent_state() {
        let agent = agent_info(
            "agent-1",
            AgentStatus::Running {
                activity: "reviewing auth middleware".into(),
            },
            true,
        );

        let snapshot = background_task_output_snapshot_for_local_agent(&agent, 0, 8192);

        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.title.as_deref(), Some("review auth flow"));
        assert_eq!(snapshot.status, "running");
        assert_eq!(snapshot.output, "reviewing auth middleware");
        assert_eq!(snapshot.output_ref, "agent_state: agent-1");
        assert!(!snapshot.terminal);
    }

    fn restored_local_agent_projection(
        status: &str,
    ) -> astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: "agent-restored".into(),
            status: status.into(),
            title: "review auth flow".into(),
            started_at_ms: 1,
            ended_at_ms: None,
            output_tail: Some("reviewing auth middleware".into()),
            terminal_reason: None,
            fanout: None,
        }
    }

    fn restored_fanout_local_agent_projection(
        status: &str,
    ) -> astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: "agent-restored-fanout".into(),
            status: status.into(),
            title: "review auth flow".into(),
            started_at_ms: 1,
            ended_at_ms: None,
            output_tail: Some("reviewing auth middleware".into()),
            terminal_reason: None,
            fanout: Some(
                astra_services::session_workspace::BackgroundLocalAgentFanoutProjection {
                    group_id: "review-1".into(),
                    group_title: "review fanout".into(),
                    target_count: 3,
                    slot_index: 1,
                    slot_label: "auth review".into(),
                },
            ),
        }
    }

    #[tokio::test]
    async fn restored_local_agent_projects_as_unavailable_stale_task() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let rows = background_task_rows_with_agents(&mut registry, None, &restored).await;

        let row = rows
            .iter()
            .find(|row| row.id == "agent-restored")
            .expect("restored local agent row");
        assert_eq!(
            row.kind,
            bottom_pane::background_task_view::BackgroundTaskKind::LocalAgent
        );
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Unavailable
        );
        assert_eq!(
            row.live_control,
            bottom_pane::background_task_view::LiveControlState::StaleHandle
        );
        assert_eq!(
            row.output_tail.as_deref(),
            Some("reviewing auth middleware")
        );

        let counts = status_line::BackgroundTaskCounts::from_rows(&rows);
        assert_eq!(counts.unavailable_local_agents, 1);
        assert!(!counts.is_empty());
    }

    #[tokio::test]
    async fn background_task_list_xml_includes_restored_local_agent_without_spawner() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let xml = render_background_task_list_xml_with_agents(&mut registry, None, &restored).await;

        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("id=\"agent-restored\""), "{xml}");
        assert!(xml.contains("kind=\"local agent\""), "{xml}");
        assert!(xml.contains("status=\"unavailable\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
        assert!(
            xml.contains("preview=\"reviewing auth middleware\""),
            "{xml}"
        );
        assert!(!xml.contains("Job"), "{xml}");
    }

    #[tokio::test]
    async fn restored_local_agent_keeps_fanout_group_metadata_for_resume_footer() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_fanout_local_agent_projection("running")];

        let rows = background_task_rows_with_agents(&mut registry, None, &restored).await;
        let row = rows
            .iter()
            .find(|row| row.id == "agent-restored-fanout")
            .expect("restored fanout row");
        let fanout = row.fanout.as_ref().expect("fanout metadata");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 1);
        assert_eq!(
            row.status,
            bottom_pane::background_task_view::BackgroundTaskStatus::Unavailable
        );

        let summaries = status_line::BackgroundTaskFanoutSummary::from_rows(&rows);
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].target_count, 3);
        assert_eq!(summaries[0].unavailable, 1);

        let xml = render_background_task_rows_xml(&rows);
        assert!(xml.contains("fanout_group_id=\"review-1\""), "{xml}");
        assert!(xml.contains("fanout_target_count=\"3\""), "{xml}");
        assert!(xml.contains("live_control=\"stale_handle\""), "{xml}");
    }

    #[tokio::test]
    async fn task_output_command_reads_restored_local_agent_projection() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let snapshot = background_task_output_snapshot_with_agents(
            &mut registry,
            None,
            &restored,
            "agent-restored",
            0,
            8192,
        )
        .await
        .expect("restored projection should be readable");

        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.status, "unavailable");
        assert!(snapshot.terminal);
        assert_eq!(snapshot.output, "reviewing auth middleware");
        assert_eq!(snapshot.output_ref, "workspace_projection: agent-restored");
    }

    #[tokio::test]
    async fn task_stop_command_reports_stale_handle_for_restored_local_agent() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let restored = vec![restored_local_agent_projection("running")];

        let error =
            stop_background_task_with_agents(&mut registry, None, &restored, "agent-restored")
                .await
                .expect_err("restored local agent has no live handle");

        assert!(error.contains("stale handle"), "{error}");
    }

    #[tokio::test]
    async fn background_task_list_xml_includes_local_agent_without_shells() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        assert!(matches!(spawned, SpawnAgentOutput::Launched { .. }));

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));

        let xml =
            render_background_task_list_xml_with_agents(&mut registry, Some(&spawner), &[]).await;

        assert!(xml.contains("<background_tasks count=\"1\">"), "{xml}");
        assert!(xml.contains("kind=\"local agent\""), "{xml}");
        assert!(xml.contains("description=\"review auth flow\""), "{xml}");
        assert!(!xml.contains("kind=\"shell\""), "{xml}");
        assert!(!xml.contains("Job"), "{xml}");

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[tokio::test]
    async fn task_stop_command_can_cancel_local_agent() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));

        stop_background_task_with_agents(&mut registry, Some(&spawner), &[], &agent_id)
            .await
            .expect("task_stop should cancel a background local agent");

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled agent should remain in history");
        assert!(matches!(
            archived.status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
    }

    #[tokio::test]
    async fn task_output_command_projects_local_agent_without_shell_output() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));

        let snapshot = background_task_output_snapshot_with_agents(
            &mut registry,
            Some(&spawner),
            &[],
            &agent_id,
            0,
            8192,
        )
        .await
        .expect("task_output should project a background local agent");

        assert_eq!(snapshot.kind, "local agent");
        assert_eq!(snapshot.title.as_deref(), Some("review auth flow"));
        assert_ne!(snapshot.output_ref, "");

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[tokio::test]
    async fn background_task_output_sentinel_projects_local_agent() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let mut chat_widget = chat_widget::ChatWidget::new("");
        let mut bottom_pane = BottomPane::new();
        let sentinel = format!(
            "{}{}",
            bottom_pane::background_task_view::BACKGROUND_TASK_OUTPUT_SENTINEL,
            agent_id
        );

        assert!(
            try_dispatch_background_task_output_sentinel(
                &sentinel,
                &mut registry,
                Some(spawner.clone()),
                &[],
                &mut chat_widget,
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "background task output sentinel should be consumed for local agents"
        );

        let last_system = chat_widget
            .history()
            .last()
            .and_then(|cell| {
                cell.as_any_ref()
                    .downcast_ref::<history_cell::system::SystemCell>()
            })
            .expect("output sentinel should commit a system message");
        assert!(
            last_system.message().contains("Read local agent output"),
            "local agent output should be rendered as local-agent output, got: {}",
            last_system.message()
        );
        assert!(
            last_system.message().contains("review auth flow"),
            "local agent output should include the task title, got: {}",
            last_system.message()
        );

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[tokio::test]
    async fn background_task_stop_sentinel_can_cancel_local_agent() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let mut chat_widget = chat_widget::ChatWidget::new("");
        let mut bottom_pane = BottomPane::new();
        let sentinel = format!(
            "{}{}",
            bottom_pane::background_task_view::BACKGROUND_TASK_STOP_SENTINEL,
            agent_id
        );

        assert!(
            try_dispatch_background_task_stop_sentinel(
                &sentinel,
                &mut registry,
                Some(spawner.clone()),
                &[],
                &mut chat_widget,
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "background task stop sentinel should be consumed"
        );

        let archived = spawner
            .get_agent_state_any(&agent_id)
            .await
            .expect("cancelled agent should remain in history");
        assert!(matches!(
            archived.status,
            AgentStatus::Cancelled { by_user: true, .. }
        ));
    }

    #[tokio::test]
    async fn background_task_switcher_opens_for_local_agent_without_shells() {
        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        assert!(matches!(spawned, SpawnAgentOutput::Launched { .. }));

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("bg"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            open_background_task_view(
                &mut registry,
                Some(&spawner),
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "local agent rows must open the background task switcher even when no shell tasks exist"
        );
        assert!(bottom_pane.has_active_view());

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[test]
    fn ctrl_b_background_hint_requires_detach() {
        assert!(should_show_ctrl_b_background_hint(true));
        assert!(!should_show_ctrl_b_background_hint(false));
    }

    #[test]
    fn shift_down_is_background_task_manage_key() {
        let shift_down = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::SHIFT,
        );
        let plain_down = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Down,
            crossterm::event::KeyModifiers::NONE,
        );
        let ctrl_b = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('b'),
            crossterm::event::KeyModifiers::CONTROL,
        );

        assert!(is_background_task_manage_key(&shift_down));
        assert!(!is_background_task_manage_key(&plain_down));
        assert!(!is_background_task_manage_key(&ctrl_b));
    }

    #[test]
    fn bash_tool_completed_returns_foreground_status_to_thinking() {
        let mut bottom_pane = BottomPane::new();
        let mut status_indicator = status_indicator::StatusIndicator::new();

        handle_app_event(
            &TuiAppEvent::ToolStarted {
                name: "bash".to_string(),
                description: "$ make check".to_string(),
                tool_use_id: "tu_bash".to_string(),
                parent_tool_use_id: None,
            },
            &mut bottom_pane,
            &mut status_indicator,
            &FrameRequester::test_dummy(),
        );
        status_indicator.set_bash_background_hint_enabled(true);
        assert!(matches!(
            status_indicator.state(),
            status_indicator::IndicatorState::Tool { name, .. } if name == "bash"
        ));
        assert!(visible_bash_tool_is_running(&status_indicator));

        handle_app_event(
            &TuiAppEvent::ToolCompleted {
                name: "bash".to_string(),
                description: "$ make check".to_string(),
                status: "success".to_string(),
                duration_ms: 1200,
                output_summary: Some(
                    "<bash_detached>The bash command was promoted to background task bg-shell-1.</bash_detached>"
                        .to_string(),
                ),
                output: None,
                tool_use_id: "tu_bash".to_string(),
                parent_tool_use_id: None,
            },
            &mut bottom_pane,
            &mut status_indicator,
            &FrameRequester::test_dummy(),
        );

        assert!(matches!(
            status_indicator.state(),
            status_indicator::IndicatorState::Thinking { .. }
        ));
        assert!(
            !visible_bash_tool_is_running(&status_indicator),
            "backgrounded Bash must no longer be the foreground activity after ToolCompleted"
        );
    }

    #[tokio::test]
    async fn background_task_rows_include_typed_status_and_combined_tail() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-row-projection"),
        );
        let id = registry.spawn_shell("printf 'stdout-line'; printf 'stderr-line' >&2", "demo");

        for _ in 0..50 {
            registry.poll_completions();
            if registry.get(&id).unwrap().status().as_str() == "completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let rows = background_task_rows(&mut registry);
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .expect("spawned task should project into switcher rows");
        assert_eq!(row.kind.as_str(), "shell");
        assert_eq!(row.status.as_str(), "completed");
        let output_ref = row.output_ref.as_deref().unwrap_or_default();
        assert!(output_ref.contains("stdout:"), "{output_ref:?}");
        assert!(output_ref.contains("stderr:"), "{output_ref:?}");
        let tail = row.output_tail.as_deref().unwrap_or_default();
        assert!(tail.contains("stdout-line"), "{tail:?}");
        assert!(tail.contains("stderr-line"), "{tail:?}");
        assert!(
            row.total_bytes.unwrap_or_default() >= "stdout-linestderr-line".len() as u64,
            "total bytes should include captured stdout and stderr"
        );
    }

    #[tokio::test]
    async fn background_task_rows_surface_missing_output_artifact() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-missing-output"),
        );
        let id = registry.spawn_shell("printf 'done'", "missing output artifact");

        for _ in 0..50 {
            registry.poll_completions();
            if registry.get(&id).unwrap().status().as_str() == "completed" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let stdout_path = registry.get(&id).unwrap().stdout_path.clone();
        std::fs::remove_file(&stdout_path).expect("remove captured stdout artifact");

        let rows = background_task_rows(&mut registry);
        let row = rows
            .iter()
            .find(|row| row.id == id)
            .expect("spawned task should project into switcher rows");
        let tail = row.output_tail.as_deref().unwrap_or_default();

        assert!(tail.contains("Output artifact missing"), "{tail}");
        assert!(tail.contains(&stdout_path.display().to_string()), "{tail}");
        assert!(row.total_bytes.is_none(), "{row:?}");
        assert!(row.output_offset.is_none(), "{row:?}");
    }

    #[test]
    fn background_task_rows_project_restored_running_as_unavailable_stale() {
        let temp = tempfile::TempDir::new().unwrap();
        let stdout = temp.path().join("restored.stdout");
        let stderr = temp.path().join("restored.stderr");
        std::fs::write(&stdout, "line from previous session\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-restored-row"),
        );
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    id: "bg-shell-restored".into(),
                    status: "running".into(),
                    title: "cargo build".into(),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: None,
                    terminal_reason: None,
                },
            )
            .unwrap();

        let rows = background_task_rows(&mut registry);
        let row = rows
            .iter()
            .find(|row| row.id == "bg-shell-restored")
            .expect("restored row");

        assert_eq!(row.status.as_str(), "unavailable");
        assert_eq!(row.started_at_ms, Some(1));
        assert_eq!(row.ended_at_ms, None);
        assert_eq!(
            row.live_control,
            bottom_pane::background_task_view::LiveControlState::StaleHandle
        );
        assert_eq!(
            row.output_tail.as_deref(),
            Some("line from previous session")
        );
    }

    #[test]
    #[serial_test::serial]
    fn background_task_projection_persistence_round_trips_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "bg-projection-session";
        let mut workspace = astra_services::session_workspace::WorkspaceMetadata::with_context(
            session_id,
            "gpt-5",
            "/tmp",
            Some("main"),
        );
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let stdout = temp.path().join("restored.stdout");
        let stderr = temp.path().join("restored.stderr");
        std::fs::write(&stdout, "persisted\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-persist"),
        );
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    id: "bg-shell-persist".into(),
                    status: "completed".into(),
                    title: "cargo test".into(),
                    started_at_ms: 42,
                    ended_at_ms: Some(84),
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: Some(0),
                    terminal_reason: Some("exit code 0".into()),
                },
            )
            .unwrap();

        let mut cache = Vec::new();
        persist_background_task_projections_if_changed(
            &mut registry,
            Some(session_id),
            Some("gpt-5"),
            &mut cache,
        );
        workspace = astra_services::session_workspace::read_workspace(session_id).unwrap();

        assert_eq!(workspace.background_shell_tasks.len(), 1);
        assert_eq!(workspace.background_shell_tasks[0].id, "bg-shell-persist");
        assert_eq!(workspace.background_shell_tasks[0].status, "completed");
        assert_eq!(workspace.background_shell_tasks[0].ended_at_ms, Some(84));
        assert_eq!(workspace.background_shell_tasks[0].exit_code, Some(0));
        assert_eq!(cache, workspace.background_shell_tasks);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn background_local_agent_projection_persistence_round_trips_workspace() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = astra_services::session_journal::JournalDirGuard::new(temp.path());
        let session_id = "bg-local-agent-projection-session";
        let workspace = astra_services::session_workspace::WorkspaceMetadata::with_context(
            session_id,
            "gpt-5",
            "/tmp",
            Some("main"),
        );
        astra_services::session_workspace::write_workspace(&workspace).unwrap();

        let spawner = test_agent_spawner(Arc::new(PendingAgentExecutor));
        let input = SpawnAgentInput {
            description: "review auth flow".to_string(),
            prompt: "review auth flow".to_string(),
            agent_type: "explore".to_string(),
            run_in_background: true,
            ..Default::default()
        };
        let spawned = spawner.spawn(input, &test_spawn_context()).await.unwrap();
        let agent_id = match spawned {
            SpawnAgentOutput::Launched { agent_id, .. } => agent_id,
            other => panic!("expected launched background agent, got {other:?}"),
        };

        let mut cache = Vec::new();
        let projections = persist_background_local_agent_task_projections_if_changed(
            Some(&spawner),
            &[],
            Some(session_id),
            Some("gpt-5"),
            &mut cache,
        )
        .await;
        let workspace = astra_services::session_workspace::read_workspace(session_id).unwrap();

        assert_eq!(workspace.background_local_agent_tasks.len(), 1);
        assert_eq!(workspace.background_local_agent_tasks[0].id, agent_id);
        assert_eq!(
            workspace.background_local_agent_tasks[0].title,
            "review auth flow"
        );
        assert_eq!(cache, workspace.background_local_agent_tasks);
        assert_eq!(projections, workspace.background_local_agent_tasks);

        spawner
            .shutdown_and_wait(std::time::Duration::from_millis(1))
            .await;
    }

    #[tokio::test]
    async fn background_task_output_snapshot_drains_completion_before_status() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-output-snapshot"),
        );
        let id = registry.spawn_shell("printf 'done\\n'", "quick output");

        wait_for_background_shell_terminal(&mut registry, &id).await;
        let snapshot =
            background_task_output_snapshot(&mut registry, &id, 0, 1024).expect("snapshot");

        assert_eq!(snapshot.status, "completed");
        assert!(snapshot.terminal, "{snapshot:?}");
        assert!(snapshot.output.contains("done"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stdout:"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stderr:"), "{snapshot:?}");
    }

    #[tokio::test]
    async fn background_task_output_snapshot_includes_stderr_only_shell_output() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-output-stderr"),
        );
        let id = registry.spawn_shell("printf 'stderr-line\\n' >&2; exit 2", "stderr output");

        wait_for_background_shell_terminal(&mut registry, &id).await;
        let snapshot =
            background_task_output_snapshot(&mut registry, &id, 0, 1024).expect("snapshot");

        assert_eq!(snapshot.status, "failed");
        assert!(snapshot.terminal, "{snapshot:?}");
        assert!(snapshot.output.contains("<stderr>"), "{snapshot:?}");
        assert!(snapshot.output.contains("stderr-line"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stdout:"), "{snapshot:?}");
        assert!(snapshot.output_ref.contains("stderr:"), "{snapshot:?}");
    }

    #[test]
    fn background_task_output_snapshot_projects_restored_running_as_unavailable() {
        let temp = tempfile::TempDir::new().unwrap();
        let stdout = temp.path().join("restored.stdout");
        let stderr = temp.path().join("restored.stderr");
        std::fs::write(&stdout, "old output\n").unwrap();
        std::fs::write(&stderr, "").unwrap();
        let mut registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("bg-task-output-restored"),
        );
        registry
            .restore_shell_task_projection(
                astra_services::session_workspace::BackgroundShellTaskProjection {
                    id: "bg-shell-restored".into(),
                    status: "running".into(),
                    title: "cargo build".into(),
                    started_at_ms: 1,
                    ended_at_ms: None,
                    stdout_path: stdout.display().to_string(),
                    stderr_path: stderr.display().to_string(),
                    exit_code: None,
                    terminal_reason: None,
                },
            )
            .unwrap();

        let snapshot = background_task_output_snapshot(&mut registry, "bg-shell-restored", 0, 1024)
            .expect("snapshot");

        assert_eq!(snapshot.status, "unavailable");
        assert!(snapshot.terminal, "{snapshot:?}");
        assert_eq!(snapshot.output, "old output\n");
    }

    #[tokio::test]
    async fn background_task_switcher_opens_for_failed_but_not_completed_only() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut completed_registry = crate::tui::background_tasks::BackgroundTaskRegistry::new(
            temp.path().join("completed-only"),
        );
        let completed_id = completed_registry.spawn_shell("true", "completed");
        for _ in 0..50 {
            completed_registry.poll_completions();
            if completed_registry
                .get(&completed_id)
                .is_some_and(|h| h.status().as_str() == "completed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let mut bottom_pane = BottomPane::new();
        assert!(
            !open_background_task_view(
                &mut completed_registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "completed-only background tasks should not steal Ctrl+T from the task board"
        );
        assert!(!bottom_pane.has_active_view());

        let mut failed_registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("failed"));
        let failed_id = failed_registry.spawn_shell("/definitely_missing_astra_binary", "failed");
        for _ in 0..50 {
            failed_registry.poll_completions();
            if failed_registry
                .get(&failed_id)
                .is_some_and(|h| h.status().as_str() == "failed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            open_background_task_view(
                &mut failed_registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "failed background tasks must remain reachable from Ctrl+T"
        );
        assert!(bottom_pane.has_active_view());
    }

    #[tokio::test]
    async fn background_task_switcher_opens_for_pending_bash_handoff() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("pending"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            reveal_background_task_view_with_extra_rows(
                &mut registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
                vec![pending_bash_handoff_row("$ make build", 0)],
                Some(PENDING_BASH_HANDOFF_TASK_ID),
            )
            .await,
            "Ctrl+B should open background tasks immediately while Bash handoff is pending"
        );

        assert!(bottom_pane.has_active_view());
        let counts = bottom_pane
            .footer
            .bg_task_counts
            .expect("pending handoff row should surface footer counts");
        assert_eq!(counts.running, 1);
    }

    /// Regression: Ctrl+B must always land the user in the background panel,
    /// even when the registry is empty in this instant. The trace this guards
    /// against: bash detached → task_output blocking on stdout for ~30s →
    /// user presses Ctrl+B → previously hit "Backgrounding unavailable" with
    /// no escape hatch. Now an empty registry renders as "No background tasks."
    /// inside the view, so the user always has a navigable surface.
    #[tokio::test]
    async fn force_open_background_task_view_opens_panel_on_empty_registry() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("empty"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            force_open_background_task_view(
                &mut registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "Ctrl+B must open the background panel even on an empty registry"
        );
        assert!(
            bottom_pane.has_active_view(),
            "force-open must push the view onto the bottom pane stack"
        );
    }

    /// Regression: `drain_bg_task_commands` must serve a queued
    /// `GetOutputSince` so a tool-side `task_output(block=true)` waiting on
    /// the reply makes progress mid-turn. Pre-fix the drain only ran on the
    /// outer tick, which was parked on `&mut fut`, hanging tasks for
    /// minutes (you saw 324s).
    #[tokio::test]
    async fn drain_bg_task_commands_serves_get_output_since_synchronously() {
        use crate::edge_tools::BgTaskCommand;

        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("drain"));
        let id = registry.spawn_shell("true", "drain test");
        for _ in 0..50 {
            registry.poll_completions();
            if registry
                .get(&id)
                .is_some_and(|h| h.projected_status() == "completed")
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let queue: std::sync::Arc<std::sync::Mutex<Vec<BgTaskCommand>>> = Default::default();
        let (tx, rx) = tokio::sync::oneshot::channel();
        queue.lock().unwrap().push(BgTaskCommand::GetOutputSince {
            task_id: id.clone(),
            offset: 0,
            max_bytes: 4096,
            reply: tx,
        });

        drain_bg_task_commands(&queue, &mut registry, None, &[]).await;

        let snapshot = rx
            .await
            .expect("drain must reply on the oneshot channel")
            .expect("snapshot must succeed for a known task id");
        assert!(
            snapshot.terminal,
            "completed task must be reported as terminal so task_output(block=true) can return: {snapshot:?}",
        );
    }

    /// The non-force entrypoint must still fall through on an empty registry
    /// so Ctrl+T can route to the task board instead of stealing the key.
    #[tokio::test]
    async fn open_background_task_view_falls_through_on_empty_registry() {
        let temp = tempfile::TempDir::new().unwrap();
        let mut registry =
            crate::tui::background_tasks::BackgroundTaskRegistry::new(temp.path().join("empty2"));
        let mut bottom_pane = BottomPane::new();

        assert!(
            !open_background_task_view(
                &mut registry,
                None,
                &[],
                &mut bottom_pane,
                &FrameRequester::test_dummy(),
            )
            .await,
            "Ctrl+T must yield when no actionable background tasks exist"
        );
        assert!(
            !bottom_pane.has_active_view(),
            "non-force open must not push the view on empty registries"
        );
    }

    #[tokio::test]
    async fn submit_deferred_tui_input_enqueues_against_active_local_run_control() {
        let run_control = Arc::new(std::sync::Mutex::new(Some(
            LocalDeferredInputRunControl::shared(),
        )));

        submit_deferred_tui_input(&run_control, "先停下来吧")
            .await
            .expect("deferred input should be accepted");

        let provider = astra_core::sync_poison::recover_mutex_lock(&run_control)
            .clone()
            .expect("run control should stay installed");
        let polled = provider.poll_user_inputs("run-local", 0).await;
        assert_eq!(
            polled.inputs.len(),
            1,
            "one deferred input should be queued"
        );
        assert_eq!(polled.inputs[0].input["content"], "先停下来吧");
    }

    #[tokio::test]
    async fn submit_deferred_tui_input_rejects_missing_local_run_control() {
        let run_control = Arc::new(std::sync::Mutex::new(None));
        let error = submit_deferred_tui_input(&run_control, "先停下来吧")
            .await
            .expect_err("missing run control must be rejected locally");
        assert!(
            error.contains("not ready to accept deferred input"),
            "missing run control should surface a local readiness error"
        );
    }

    #[test]
    fn active_turn_submit_routes_to_run_input_api_not_local_queue() {
        let source = include_str!("event_loop.rs");
        let arm_start = source
            .find("BottomPaneAction::SubmitInput(queued_text) => {")
            .expect("active-turn SubmitInput arm must exist");
        let arm_end = source[arm_start..]
            .find("BottomPaneAction::ViewSideEffect { result } => {")
            .expect("active-turn SubmitInput arm must end before ViewSideEffect");
        let arm = &source[arm_start..arm_start + arm_end];

        assert!(
            arm.contains("submit_deferred_tui_input("),
            "active-turn Enter must queue against the live local run control via submit_deferred_tui_input"
        );
        assert!(
            !arm.contains("queued_messages.push(queued_text)"),
            "active-turn Enter must not fall back to the old local queue that waited until turn end"
        );
        assert!(
            !arm.contains("state.run_id.clone()"),
            "active-turn Enter must not read the stale per-session run_id"
        );
        assert!(
            arm.contains("active_turn_local_run_control"),
            "active-turn Enter should use the live local run-control slot for the current turn"
        );
    }

    #[test]
    fn deferred_input_status_lines_are_committed_to_chat_history() {
        let source = include_str!("event_loop.rs");
        assert!(
            source.contains("DEFERRED_INPUT_APPLIED_PREFIX"),
            "deferred-input feedback should have a dedicated status-line prefix"
        );
        assert!(
            source.contains("text.strip_prefix(DEFERRED_INPUT_APPLIED_PREFIX)"),
            "TUI should surface deferred-input-applied status lines into chat history"
        );
        assert!(
            source.contains("chat_widget.commit_deferred_user"),
            "applied deferred input should be rendered as a user transcript row"
        );
    }

    #[test]
    fn startup_workspace_trust_prompt_is_wired_into_tui_boot() {
        let source = include_str!("event_loop.rs");
        assert!(
            source.contains("workspace_trust_startup_prompt()"),
            "run_tui_session should query the permission manager for a startup trust prompt"
        );
        assert!(
            source.contains("Trust Workspace")
                && source.contains("Continue This Session")
                && source.contains("Mark Untrusted"),
            "startup trust picker should offer trust / continue-once / untrust choices"
        );
    }

    #[test]
    fn startup_workspace_trust_picker_results_are_handled() {
        let source = include_str!("event_loop.rs");
        let arm = source
            .find("name.strip_prefix(WORKSPACE_TRUST_SENTINEL)")
            .expect("view completion should handle the workspace trust picker sentinel");
        let body = &source[arm..];
        assert!(
            body.contains("trust_workspace()")
                && body.contains("untrust_workspace()")
                && body.contains("Continuing without trusting this workspace."),
            "workspace trust picker must wire all three outcomes"
        );
    }

    #[test]
    fn reopen_target_parse_rejects_unknown() {
        assert_eq!(ReopenTarget::parse(""), None);
        assert_eq!(ReopenTarget::parse("not-a-target"), None);
        assert_eq!(ReopenTarget::parse("Agents"), None, "case-sensitive");
    }

    #[test]
    fn implicit_plan_request_detector_accepts_actionable_prompts() {
        assert!(looks_like_implicit_plan_request(
            "帮我计划在/tmp下生成一个报销系统"
        ));
        assert!(looks_like_implicit_plan_request(
            "help me plan how to refactor the auth middleware"
        ));
        assert!(looks_like_implicit_plan_request(
            "please plan how to migrate this service to Axum"
        ));
    }

    #[test]
    fn implicit_plan_request_detector_rejects_meta_plan_questions() {
        assert!(!looks_like_implicit_plan_request("现在plan是怎么工作的?"));
        assert!(!looks_like_implicit_plan_request("what is /plan mode?"));
        assert!(!looks_like_implicit_plan_request("/plan"));
    }

    #[test]
    fn plan_transition_notice_covers_enter_goal_and_exit() {
        let inactive = PlanModeUiSnapshot::default();
        let entered_empty = PlanModeUiSnapshot {
            active: true,
            goal: String::new(),
            executing: false,
        };
        let entered_goal = PlanModeUiSnapshot {
            active: true,
            goal: "Implement auth middleware".into(),
            executing: false,
        };
        let exited_running = PlanModeUiSnapshot {
            active: false,
            goal: String::new(),
            executing: true,
        };

        let enter_msg = plan_transition_notice(&inactive, &entered_empty, false)
            .expect("entering plan mode should announce itself");
        assert!(enter_msg.contains("Plan mode active"));
        assert!(enter_msg.contains("describe your goal"));

        let goal_msg = plan_transition_notice(&entered_empty, &entered_goal, false)
            .expect("setting the first goal should be surfaced");
        assert!(goal_msg.contains("Plan goal set"));
        assert!(goal_msg.contains("Implement auth middleware"));

        let exit_msg = plan_transition_notice(&entered_goal, &exited_running, false)
            .expect("background execution exit should be surfaced");
        assert!(exit_msg.contains("running in the background"));
    }

    #[test]
    fn submit_input_routes_plan_mode_and_implicit_plan_requests_before_chat_turn() {
        let source = include_str!("event_loop.rs");
        let arm_start = source
            .find("BottomPaneAction::SubmitInput(text) => {")
            .expect("SubmitInput arm must exist");
        let arm_end = source[arm_start..]
            .find("BottomPaneAction::ViewCompleted { result, reopen } => {")
            .expect("SubmitInput arm must end before ViewCompleted");
        let arm = &source[arm_start..arm_start + arm_end];

        assert!(
            arm.contains("slash_plan_goal(&text)")
                && arm.contains("crate::cli::plan::plan_lifecycle::enter_remote_plan_mode("),
            "/plan <goal> should pre-enter remote plan mode before the chat turn"
        );
        assert!(
            arm.contains("looks_like_implicit_plan_request(&submit_text)")
                && arm.contains("crate::cli::plan::plan_lifecycle::enter_remote_plan_mode("),
            "plain planning requests should enter remote /plan semantics before normal chat turns"
        );
        assert!(
            arm.contains("crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(")
                && arm.contains("crate::cli::plan::plan_lifecycle::enter_remote_plan_mode("),
            "the first plain message after bare /plan should bind remote plan mode and continue as chat"
        );
    }

    #[test]
    fn pending_local_plan_goal_binding_does_not_emit_synthetic_notice() {
        let source = include_str!("event_loop.rs");
        let branch_start = source
            .find("if crate::cli::plan::plan_lifecycle::looks_like_pending_local_plan_entry(")
            .expect("pending local plan entry branch must exist");
        let branch_end = source[branch_start..]
            .find("} else {")
            .map(|offset| branch_start + offset)
            .expect("pending local plan entry branch must end before the generic else");
        let branch = &source[branch_start..branch_end];

        assert!(
            !branch.contains("commit_plan_transition_notice("),
            "the first real goal after bare /plan should flow directly to model output"
        );
    }

    #[test]
    fn submit_input_handles_plan_text_commands_before_normal_chat() {
        let source = include_str!("event_loop.rs");
        let arm_start = source
            .find("BottomPaneAction::SubmitInput(text) => {")
            .expect("SubmitInput arm must exist");
        let arm_end = source[arm_start..]
            .find("BottomPaneAction::ViewCompleted { result, reopen } => {")
            .expect("SubmitInput arm must end before ViewCompleted");
        let arm = &source[arm_start..arm_start + arm_end];

        assert!(
            arm.contains("crate::cli::plan::plan_commands::parse_plan_command(&submit_text)")
                && arm.contains("crate::cli::plan::plan_runtime::start_and_monitor_plan("),
            "plain TUI submits should intercept go/show/rewind/correct plan commands and wire `go` into the real plan runtime"
        );
        assert!(
            arm.contains("crate::cli::plan::plan_commands::abandon_plan_execution("),
            "ordinary prose after a paused plan should abandon the paused execution instead of keeping stale state around"
        );
    }

    /// REGRESSION (review M1): the `else if pending_deferred_slash_flush`
    /// branch must call `flush_chat_widget` before clearing the flag,
    /// otherwise any system cells committed during the deferred window
    /// (e.g. background permission-auto-approval banners surfaced by
    /// `surface_status_line_system_cell`) get silently dropped: the bare
    /// `mark_all_flushed()` advances the watermark without rendering.
    /// Pinned via source text — the construct is buried inside a
    /// 6-level-nested match-arm and is awkward to invoke in isolation.
    #[test]
    fn deferred_slash_flush_branch_renders_pending_cells() {
        let source = include_str!("event_loop.rs");
        // Locate the branch by its distinctive guard.
        let needle = "} else if pending_deferred_slash_flush {";
        let start = source
            .find(needle)
            .expect("deferred-flush branch must exist");
        let body_end = source[start..]
            .find("} else if reopen")
            .expect("branch must close before next else-if");
        let body = &source[start..start + body_end];
        assert!(
            body.contains("flush_chat_widget(&mut guard, &mut chat_widget"),
            "deferred-flush branch must call flush_chat_widget so pending \
             cells reach scrollback; bare mark_all_flushed drops them. Body:\n{body}"
        );
        assert!(
            body.contains("pending_deferred_slash_flush = false"),
            "deferred-flush branch must clear the flag after handling. Body:\n{body}"
        );
    }

    /// REGRESSION (review M4): every branch of the ViewCompleted arm
    /// in `run_tui_session` must clear `pending_deferred_slash_flush`,
    /// otherwise the flag can stick `true` forever and `should_flush_ambient_commits`
    /// permanently suppresses TUI tick + app-event flushes — breaking
    /// every subsequent slash menu, ambient banner, and live update.
    ///
    /// The trigger: a deferred slash command resolves via a path that
    /// did not previously match any of the 6 explicit clear sites
    /// (e.g. `looks_like_session_id`, fall-through `handle_view_result`,
    /// `reopen=Agents`, generic `reopen=<cmd>`).
    ///
    /// Pin the property at the source level — checking each ViewCompleted
    /// sub-branch contains a clear line. When future sub-branches land,
    /// they fail this test until they add their own clear (or refactor
    /// to a single early-clear).
    #[test]
    fn every_view_completed_branch_clears_deferred_flush_flag() {
        let source = include_str!("event_loop.rs");
        // Locate the outer ViewCompleted arm in run_tui_session by a
        // distinctive opening (the inner `tokio::select!` at line ~870
        // also has a ViewCompleted arm but its body is much shorter).
        // The outer arm runs from "BottomPaneAction::ViewCompleted { result, reopen }"
        // through the next arm "BottomPaneAction::Interrupt".
        let arm_start = source
            .find("BottomPaneAction::ViewCompleted { result, reopen } => {")
            .expect("outer ViewCompleted arm must exist");
        let arm_end_offset = source[arm_start..]
            .find("BottomPaneAction::Interrupt | BottomPaneAction::Quit => { break 'main")
            .expect("outer arm must close before Interrupt/Quit");
        let arm = &source[arm_start..arm_start + arm_end_offset];

        let post_view = arm
            .find("// Flush view-driven system cells")
            .expect("post-name flush comment must exist");
        let post_view_block = &arm[post_view..];
        let next_else = post_view_block
            .find("} else if pending_deferred_slash_flush")
            .expect("post-name block must end before deferred-flush else-if");
        let post_view_segment = &post_view_block[..next_else];
        assert!(
            post_view_segment.contains("pending_deferred_slash_flush = false"),
            "ViewCompleted-with-name fallthrough (looks_like_session_id, \
             generic handle_view_result) must clear the deferred-flush flag, \
             else /<cmd> resolved via that path leaves ambient flushes \
             suppressed. Segment:\n{post_view_segment}"
        );

        let agents_idx = arm
            .find("== Some(ReopenTarget::Agents)")
            .expect("agents-reopen branch must exist");
        let agents_block = &arm[agents_idx..];
        let after_agents = agents_block
            .find("} else if let Some(cmd) = reopen")
            .expect("agents-reopen branch must close before generic reopen");
        let agents_body = &agents_block[..after_agents];
        assert!(
            agents_body.contains("pending_deferred_slash_flush = false"),
            "agents-reopen branch must clear the deferred-flush flag. Body:\n{agents_body}"
        );

        let generic_idx = arm
            .find("} else if let Some(cmd) = reopen {")
            .expect("generic-reopen branch must exist");
        let generic_body = &arm[generic_idx..];
        assert!(
            generic_body.contains("pending_deferred_slash_flush = false"),
            "generic-reopen branch must clear the deferred-flush flag. Body:\n{generic_body}"
        );
    }

    #[test]
    fn detail_refresh_is_scoped_to_incoming_agent_id() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::Wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                agent_id: "agent-a".into(),
                kind: AgentLiveEventKind::OutputDelta("a".into()),
            }),
        ));
        chat_widget.handle_event(chat_widget::AppEvent::Wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                agent_id: "agent-b".into(),
                kind: AgentLiveEventKind::OutputDelta("b".into()),
            }),
        ));

        let mut bottom_pane = BottomPane::new();
        let cell = chat_widget.task_cell_anywhere("agent-a").unwrap();
        bottom_pane.push_view(Box::new(
            bottom_pane::task_detail_view::TaskDetailView::from_task_cell(cell)
                .with_live_task_id("agent-a"),
        ));

        let unrelated = TuiAppEvent::AgentLive(AgentLiveEvent {
            agent_id: "agent-b".into(),
            kind: AgentLiveEventKind::OutputDelta("more b".into()),
        });
        assert!(
            !refresh_open_agent_detail_for_event(&unrelated, &chat_widget, &mut bottom_pane),
            "non-open agent events must not rebuild the open detail view"
        );

        let related = TuiAppEvent::AgentLive(AgentLiveEvent {
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::OutputDelta("more a".into()),
        });
        assert!(
            refresh_open_agent_detail_for_event(&related, &chat_widget, &mut bottom_pane),
            "open agent events should refresh the detail view"
        );
    }

    #[test]
    fn detail_refresh_skips_work_when_no_agent_detail_is_open() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::Wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                agent_id: "agent-a".into(),
                kind: AgentLiveEventKind::OutputDelta("a".into()),
            }),
        ));

        let event = TuiAppEvent::AgentLive(AgentLiveEvent {
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::OutputDelta("more a".into()),
        });
        let mut bottom_pane = BottomPane::new();
        assert!(
            !refresh_open_agent_detail_for_event(&event, &chat_widget, &mut bottom_pane),
            "agent live events should not rebuild detail rows unless a matching detail view is open"
        );
    }

    #[test]
    fn agent_monitor_refresh_ignores_token_only_events() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use bottom_pane::in_flight_agents_view::{AgentRow, AgentRowStatus, InFlightAgentsView};

        let chat_widget = chat_widget::ChatWidget::new(String::new());
        let mut bottom_pane = BottomPane::new();
        bottom_pane.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
            agent_id: "agent-a".into(),
            name: "agent-a".into(),
            child_count: 0,
            elapsed_ms: 0,
            status: AgentRowStatus::Live,
            fanout: None,
        }])));

        let token = TuiAppEvent::AgentLive(AgentLiveEvent {
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::OutputDelta("token".into()),
        });
        assert!(
            !refresh_open_agent_monitor_for_event(&token, &chat_widget, &mut bottom_pane),
            "token-only events must not rebuild the agent monitor rows"
        );
    }

    #[test]
    fn agent_monitor_refreshes_for_row_affecting_events_only_when_open() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use bottom_pane::in_flight_agents_view::{AgentRow, AgentRowStatus, InFlightAgentsView};

        let mut chat_widget = chat_widget::ChatWidget::new(String::new());
        chat_widget.handle_event(chat_widget::AppEvent::Wire(
            chat_widget::WireEvent::AgentLive(AgentLiveEvent {
                agent_id: "agent-a".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "bash".into(),
                    description: "work".into(),
                    tool_use_id: "tool-1".into(),
                },
            }),
        ));
        let event = TuiAppEvent::AgentLive(AgentLiveEvent {
            agent_id: "agent-a".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "more work".into(),
                tool_use_id: "tool-2".into(),
            },
        });

        let mut closed = BottomPane::new();
        assert!(
            !refresh_open_agent_monitor_for_event(&event, &chat_widget, &mut closed),
            "row-affecting events should not build rows when monitor is closed"
        );

        let mut open = BottomPane::new();
        open.push_view(Box::new(InFlightAgentsView::new(vec![AgentRow {
            agent_id: "agent-a".into(),
            name: "agent-a".into(),
            child_count: 0,
            elapsed_ms: 0,
            status: AgentRowStatus::Live,
            fanout: None,
        }])));
        assert!(refresh_open_agent_monitor_for_event(
            &event,
            &chat_widget,
            &mut open
        ));
    }

    #[test]
    fn slash_submits_delay_user_flush_so_response_can_hug() {
        assert!(!should_flush_submitted_user_cell_immediately("/model"));
        assert!(!should_flush_submitted_user_cell_immediately("   /help"));
        assert!(should_flush_submitted_user_cell_immediately("hi"));
    }

    #[test]
    fn deferred_slash_dispatch_keeps_user_cell_pending() {
        assert!(!should_flush_after_slash_dispatch(
            &slash_dispatch::SlashResult::Deferred
        ));
        assert!(should_flush_after_slash_dispatch(
            &slash_dispatch::SlashResult::Handled
        ));
    }

    #[test]
    fn ambient_flush_waits_while_deferred_slash_pair_is_pending() {
        assert!(!should_flush_ambient_commits(true));
        assert!(should_flush_ambient_commits(false));
    }

    #[test]
    fn total_input_tokens_includes_cache_buckets_exactly_once() {
        assert_eq!(total_input_tokens(1200, 800, 100), 2100);
        assert_eq!(total_input_tokens(0, 5000, 0), 5000);
    }

    #[test]
    fn non_deferred_slash_dispatch_clears_pending_pair_state() {
        assert!(!next_pending_deferred_slash_flush(
            &slash_dispatch::SlashResult::Handled
        ));
        assert!(!next_pending_deferred_slash_flush(
            &slash_dispatch::SlashResult::Fallback
        ));
        assert!(next_pending_deferred_slash_flush(
            &slash_dispatch::SlashResult::Deferred
        ));
    }

    #[test]
    fn render_history_batch_lines_keeps_prose_gap() {
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> =
            vec![Arc::new(history_cell::user::UserCell::new("hi"))];
        let lines = render_history_batch_lines(&cells, 80);

        assert!(
            lines
                .last()
                .is_some_and(|line| line.spans.iter().all(|span| span.content.is_empty())),
            "prose cells should keep a trailing separator row"
        );
        let blank_count = lines
            .iter()
            .rev()
            .take_while(|line| line.spans.iter().all(|span| span.content.is_empty()))
            .count();
        assert_eq!(blank_count, 1, "prose cells should end with one blank row");
    }

    #[test]
    fn render_history_batch_lines_gives_tool_blocks_more_air() {
        let mut tool = history_cell::tool::ToolCell::new_running("bash", "ls /tmp");
        tool.complete("success", 42, String::new(), Some("3 entries".into()), None);
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> = vec![Arc::new(tool)];
        let lines = render_history_batch_lines(&cells, 80);

        let blank_count = lines
            .iter()
            .rev()
            .take_while(|line| line.spans.is_empty())
            .count();
        assert_eq!(blank_count, 1, "tool blocks should end with one blank row");
    }

    #[test]
    fn render_history_batch_lines_keeps_slash_user_readable() {
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> =
            vec![Arc::new(history_cell::user::UserCell::new("/allow"))];
        let lines = render_history_batch_lines(&cells, 80);

        assert_eq!(
            lines
                .iter()
                .rev()
                .take_while(|line| line.spans.iter().all(|span| span.content.is_empty()))
                .count(),
            1,
            "slash command should keep one trailing blank row"
        );
    }

    #[test]
    fn render_history_batch_lines_gives_slash_pair_one_breath() {
        let slash = history_cell::user::UserCell::new("/allow");
        let slash_rows = slash.display_lines(80).len();
        let cells: Vec<Arc<dyn history_cell::HistoryCell>> = vec![
            Arc::new(slash),
            Arc::new(history_cell::system::SystemCell::response("Mode → Auto")),
        ];
        let lines = render_history_batch_lines(&cells, 80);

        assert_eq!(
            lines
                .iter()
                .rev()
                .take_while(|line| line.spans.iter().all(|span| span.content.is_empty()))
                .count(),
            1,
            "slash command and response should end with one blank row"
        );
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert!(
            rendered.iter().any(|line| line.contains("/allow")),
            "slash command row present"
        );
        let response_idx = rendered
            .iter()
            .position(|line| line.contains("Mode → Auto"))
            .expect("response row present");
        assert_eq!(
            response_idx, slash_rows,
            "slash response should start right after the slash card's own breathing room"
        );
    }

    #[test]
    fn transcript_view_lines_include_active_cell_output() {
        let mut w = chat_widget::ChatWidget::new("");
        w.handle_event(chat_widget::AppEvent::User(chat_widget::UserEvent::Submit(
            "review".into(),
        )));
        w.handle_event(chat_widget::AppEvent::Wire(
            chat_widget::WireEvent::AnswerDelta("still working".into()),
        ));

        let rendered = render_transcript_view_lines(&w, 80);
        let text: Vec<String> = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();

        assert!(
            text.iter().any(|line| line.contains("still working")),
            "active assistant output should be visible in transcript overlay"
        );
    }

    #[test]
    fn render_explain_dag_formats_rounds_cache_and_batches() {
        let mut trace = ContextAssemblyTrace {
            turn_id: "turn-2".into(),
            session_id: "sess-1".into(),
            ..Default::default()
        };
        trace.system_prompt.total_tokens = 3943;
        trace.token_budget.total_used = 7658;
        trace.token_budget.max_tokens = 160_000;
        trace.token_budget.history_tokens = 7;
        trace.token_budget.tool_schema_tokens = 3708;
        trace
            .history
            .turns_retained
            .push(astra_turn_core::context_assembly_trace::TurnRetention {
                turn_index: 0,
                role: "assistant".into(),
                tokens: 7,
                has_tool_calls: false,
            });
        trace.memory.candidates_considered = 5;
        trace.memory.retrieval_latency_ms = 51;
        trace.tools.tools_available = 27;
        trace.tools.selection_strategy = "registry".into();
        trace
            .tools
            .tools_selected
            .push(astra_turn_core::context_assembly_trace::ToolSelected {
                tool_name: "bash".into(),
                score: 1.0,
                tokens: 243,
                selection_factors: Vec::new(),
            });
        trace
            .tools
            .tools_selected
            .push(astra_turn_core::context_assembly_trace::ToolSelected {
                tool_name: "read_file".into(),
                score: 0.9,
                tokens: 128,
                selection_factors: Vec::new(),
            });
        let mut turn_event = astra_services::session_journal::JournalEvent::turn(
            Some("sess-1"),
            2,
            Some("gpt-5"),
            "hi",
            "done",
            2,
            10_023,
            32,
            2_930,
        )
        .with_cache_tokens(900, 200)
        .with_tool_calls(vec![
            ToolCallRecord {
                tool_call_id: Some("call-1".into()),
                name: "bash".into(),
                ok: true,
                ms: 3000,
                batch_id: Some("parallel-1".into()),
                parallel: Some(true),
                round: Some(0),
                start_offset_ms: Some(40),
                args_preview: Some("{\"command\":\"git status\"}".into()),
                ..Default::default()
            },
            ToolCallRecord {
                tool_call_id: Some("call-2".into()),
                name: "read_file".into(),
                ok: true,
                ms: 48,
                batch_id: Some("parallel-1".into()),
                parallel: Some(true),
                round: Some(0),
                file_path: Some("README.md".into()),
                ..Default::default()
            },
        ]);
        turn_event.ttft_ms = Some(1900);
        turn_event.context_ms = Some(88);
        turn_event.memoria_ms = Some(51);
        turn_event.total_llm_ms = Some(2930);
        turn_event.total_tool_ms = Some(3048);
        turn_event.llm_rounds = Some(1);
        let explain_items = vec![serde_json::json!({
            "total_ms": 2930,
            "prompt_tokens": 10023,
            "completion_tokens": 32,
            "steps": [{
                "step": "llm",
                "duration_ms": 2930,
                "in": 10023,
                "cached_in": 900,
                "cache_write": 200,
                "out": 32,
                "tool_calls": 2
            }],
            "routing": {
                "intent": "default",
                "confidence": 0.0,
                "tier": 0,
                "skipped": false,
                "reason": ""
            }
        })];

        let meta = ExplainTurnMeta::from_journal_event(&turn_event);
        let text =
            render_explain_dag(Some(&trace), Some(&meta), &explain_items, false).expect("text");
        assert!(text.contains("Explain Analyze DAG — turn-2"));
        assert!(text.contains("context_assembly ms=88ms budget=7658/160000 (4.8%)"));
        assert!(text.contains(
            "llm ms=2.9s fresh_in=10023 cache_read=900 cache_write=200 out=32 tool_calls=2"
        ));
        assert!(text.contains("batch[parallel-1] parallel tools=2"));
        assert!(text.contains("bash ok ms=3.0s offset=40ms id=call-1"));
        assert!(text.contains("read_file ok ms=48ms id=call-2 path=README.md"));
    }

    #[test]
    fn commit_explain_dag_commits_trace_to_history() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.explain = crate::cli::session::session_state::ExplainMode::On;
        state.turn = 9;
        state.latest_context_assembly_trace = Some(ContextAssemblyTrace {
            turn_id: "turn-9".into(),
            session_id: "sid-trace".into(),
            token_budget: astra_turn_core::context_assembly_trace::TokenBudgetTrace {
                total_used: 1024,
                max_tokens: 4096,
                ..Default::default()
            },
            ..Default::default()
        });
        state.last_turn_event = Some(astra_services::session_journal::JournalEvent::turn(
            Some("sid-trace"),
            9,
            Some("gpt-5"),
            "hi",
            "hello",
            0,
            12,
            8,
            1200,
        ));
        let mut widget = chat_widget::ChatWidget::new("");

        assert!(commit_explain_dag(&state, &[], None, 0, &mut widget));

        let sys = widget
            .history()
            .last()
            .and_then(|cell| {
                cell.as_any_ref()
                    .downcast_ref::<history_cell::system::SystemCell>()
            })
            .expect("expected a committed system cell");
        assert!(sys.message().contains("Explain Analyze DAG — turn-9"));
    }

    #[test]
    fn commit_explain_dag_skips_unchanged_cached_trace() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.explain = crate::cli::session::session_state::ExplainMode::On;
        state.latest_context_assembly_trace = Some(ContextAssemblyTrace {
            turn_id: "turn-9".into(),
            session_id: "sid-trace".into(),
            ..Default::default()
        });
        let mut widget = chat_widget::ChatWidget::new("");

        assert!(!commit_explain_dag(
            &state,
            &[],
            Some("turn-9"),
            0,
            &mut widget,
        ));
        assert!(widget.history().is_empty());
    }

    #[test]
    fn commit_explain_dag_preserves_unknown_cache_write_marker() {
        let mut state = crate::cli::session::session_state::SessionState::default();
        state.explain = crate::cli::session::session_state::ExplainMode::On;
        state.turn = 4;
        state.last_turn_event = Some(astra_services::session_journal::JournalEvent::turn(
            Some("sid-trace"),
            4,
            Some("gpt-5"),
            "hi",
            "hello",
            0,
            12,
            8,
            1200,
        ));
        state
            .last_turn_event
            .as_mut()
            .expect("turn event")
            .cache_read_tokens = Some(144);
        let mut widget = chat_widget::ChatWidget::new("");

        assert!(commit_explain_dag(&state, &[], None, 0, &mut widget));

        let sys = widget
            .history()
            .last()
            .and_then(|cell| {
                cell.as_any_ref()
                    .downcast_ref::<history_cell::system::SystemCell>()
            })
            .expect("expected a committed system cell");
        assert!(sys.message().contains("cache_write=?"));
    }

    #[test]
    fn background_task_event_system_message_uses_typed_shell_vocabulary() {
        let completed = background_task_event_system_message(&BgTaskEvent::Completed {
            id: "bg-shell-1".to_string(),
            title: "cargo test -p astra-cli".to_string(),
            exit_code: Some(0),
            summary: "ok".to_string(),
        })
        .expect("completed should notify");
        assert!(
            completed.contains("Background shell \"cargo test -p astra-cli\" completed (exit 0)"),
            "{completed}"
        );
        assert!(!completed.contains("Background command"));

        let stalled = background_task_event_system_message(&BgTaskEvent::Stalled {
            id: "bg-shell-2".to_string(),
            title: "python script.py".to_string(),
            last_output_tail: "Continue? [y/N]".to_string(),
        })
        .expect("stalled should notify");
        assert!(stalled.contains("waiting for input"), "{stalled}");
        assert!(stalled.contains("\"python script.py\""), "{stalled}");
        assert!(!stalled.contains("Background command"));

        let killed = background_task_event_system_message(&BgTaskEvent::Killed {
            id: "bg-shell-3".to_string(),
            title: "deploy.sh".to_string(),
        })
        .expect("killed should notify");
        assert!(killed.contains("stopped"), "{killed}");
        assert!(killed.contains("\"deploy.sh\""), "{killed}");
        assert!(!killed.contains("killed"), "{killed}");
    }

    #[test]
    fn background_task_output_system_message_includes_title_offsets_and_lines() {
        let message = format_background_task_output_system_message(
            "bg-shell-1",
            "npm run dev",
            "running",
            8192,
            13_244,
            312,
            "Listening on http://localhost:5173/\n",
        );

        assert!(
            message.contains("Read shell output bg-shell-1"),
            "{message}"
        );
        assert!(message.contains("\"npm run dev\""), "{message}");
        assert!(message.contains("1 new line"), "{message}");
        assert!(message.contains("offset 8192 -> 13244"), "{message}");
        assert!(message.contains("total 13244 bytes"), "{message}");
        assert!(message.contains("312 total lines"), "{message}");
        assert!(message.contains("still running"), "{message}");
        assert!(message.contains("Output chunk:"), "{message}");
        assert!(
            message.contains("Listening on http://localhost:5173/"),
            "{message}"
        );
        assert!(
            !message.contains("Background shell bg-shell-1 output"),
            "{message}"
        );
    }

    #[test]
    fn background_task_output_system_message_names_terminal_empty_output() {
        let message = format_background_task_output_system_message(
            "bg-shell-2",
            "cargo test -p astra-cli",
            "completed",
            0,
            0,
            0,
            "",
        );

        assert!(
            message.contains("Read shell output bg-shell-2"),
            "{message}"
        );
        assert!(message.contains("Completed with no output"), "{message}");
        assert!(message.contains("offset 0 -> 0"), "{message}");
        assert!(!message.contains("No output captured yet"), "{message}");
    }

    #[test]
    fn background_task_stop_terminal_race_is_not_reported_as_failure() {
        let message = format_background_task_stop_error_system_message(
            "bg-shell-1",
            "background shell 'bg-shell-1' already terminated",
        );

        assert_eq!(message, "Background task bg-shell-1 already finished.");
        assert!(is_background_task_terminal_race_error(
            "background shell 'bg-shell-1' already terminated"
        ));
        assert!(!message.contains("Failed to stop"), "{message}");
    }

    #[test]
    fn background_task_stop_stale_handle_is_not_reported_as_generic_failure() {
        let message = format_background_task_stop_error_system_message(
            "bg-shell-1",
            "background shell 'bg-shell-1' has a stale handle",
        );

        assert!(message.contains("cannot be stopped"), "{message}");
        assert!(message.contains("no live process handle"), "{message}");
        assert!(!message.contains("Failed to stop"), "{message}");
    }

    #[test]
    fn background_task_output_read_unknown_id_uses_typed_not_found() {
        let message = format_background_task_output_read_error(
            "bg-shell-missing",
            "no background shell with id 'bg-shell-missing'",
        );

        assert_eq!(message, "Background task not found: bg-shell-missing");
        assert!(!message.contains("background shell with id"), "{message}");
        assert!(!message.contains("Output unavailable"), "{message}");
    }

    #[test]
    fn background_task_stop_unknown_id_uses_typed_not_found() {
        let message = format_background_task_stop_error_system_message(
            "bg-shell-missing",
            "no background shell with id 'bg-shell-missing'",
        );

        assert_eq!(message, "Background task not found: bg-shell-missing");
        assert!(!message.contains("Failed to stop"), "{message}");
        assert!(!message.contains("background shell with id"), "{message}");
    }

    #[test]
    fn background_task_event_system_messages_collapses_many_successes() {
        let messages = background_task_event_system_messages(&[
            BgTaskEvent::Completed {
                id: "bg-shell-1".to_string(),
                title: "cmd one".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
            BgTaskEvent::Completed {
                id: "bg-shell-2".to_string(),
                title: "cmd two".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
        ]);

        assert_eq!(messages, vec!["2 background shells completed".to_string()]);
    }

    #[test]
    fn background_task_event_system_messages_keeps_attention_events_explicit() {
        let messages = background_task_event_system_messages(&[
            BgTaskEvent::Completed {
                id: "bg-shell-1".to_string(),
                title: "cmd one".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
            BgTaskEvent::Failed {
                id: "bg-shell-2".to_string(),
                title: "npm test".to_string(),
                error: "exit 2".to_string(),
            },
            BgTaskEvent::Completed {
                id: "bg-shell-3".to_string(),
                title: "cmd three".to_string(),
                exit_code: Some(0),
                summary: "ok".to_string(),
            },
            BgTaskEvent::Killed {
                id: "bg-shell-4".to_string(),
                title: "deploy.sh".to_string(),
            },
            BgTaskEvent::Stalled {
                id: "bg-shell-5".to_string(),
                title: "python script.py".to_string(),
                last_output_tail: "Continue? [y/N]".to_string(),
            },
        ]);

        assert_eq!(messages[0], "2 background shells completed");
        assert!(messages[1].contains("\"npm test\" failed"), "{messages:?}");
        assert!(
            messages[2].contains("\"deploy.sh\" was stopped"),
            "{messages:?}"
        );
        assert!(
            messages[3].contains("\"python script.py\" appears to be waiting for input"),
            "{messages:?}"
        );
        assert_eq!(messages.len(), 4);
    }

    #[test]
    fn background_task_event_system_messages_does_not_collapse_unknown_or_nonzero_exit() {
        let messages = background_task_event_system_messages(&[
            BgTaskEvent::Completed {
                id: "bg-shell-1".to_string(),
                title: "false".to_string(),
                exit_code: Some(1),
                summary: "exit 1".to_string(),
            },
            BgTaskEvent::Completed {
                id: "bg-shell-2".to_string(),
                title: "".to_string(),
                exit_code: None,
                summary: "unknown exit".to_string(),
            },
        ]);

        assert_eq!(messages.len(), 2);
        assert!(messages[0].contains("\"false\" completed"), "{messages:?}");
        assert!(messages[1].contains("bg-shell-2 completed"), "{messages:?}");
        assert!(
            messages
                .iter()
                .all(|msg| !msg.contains("background shells completed")),
            "{messages:?}"
        );
    }
}
