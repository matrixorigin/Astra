//! Frame composition: `ActiveView` priority resolver + `do_draw`.
//!
//! The TUI's draw cycle is: compute which source owns the active-cell
//! region (active cell / task board / status line / next-hint / empty),
//! then paint `active | separator | bottom pane` into the ratatui
//! frame. This file owns that pipeline so the outer event loop in
//! [`super::event_loop`] doesn't carry render details.
//!
//! See `ARCHITECTURE.md` for the visual-hierarchy grammar.

use astra_tools::task_mgmt::TaskStoreHealth;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::agent_run_projection::{AgentProjectionConfidence, AgentRunState, AgentRunStatus};
use super::bottom_pane::{BottomPane, ConversationTab, footer::Footer};
use super::history_cell::assistant::AssistantCell;
use super::render::line_utils::sanitize_lines_for_terminal;
use super::render::renderable::{FlexRenderable, Renderable, RenderableItem};
use super::task_board_observer::{
    ProjectedTaskTruthState, TaskBoardObserver, TaskBoardProjection, TaskBoardTruthState,
};
use super::terminal::TerminalGuard;
use super::{chat_widget, status_indicator, task_list};
use crate::cli::effects::truncate_label;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

// ───────────────────────────────────────────────────────────────────────
// Active-view grammar
// ───────────────────────────────────────────────────────────────────────

/// What the active-cell area should render this frame. Encodes the
/// visual-hierarchy grammar that distinguishes the three layers the
/// user sees at any moment:
///
/// - **Settled** (not represented here) — committed `HistoryCell`s
///   already painted to terminal scrollback. Flat, no border.
/// - **Active** — something's happening right now. Rendered with a
///   left `█` gutter whose semantic colour distinguishes live and
///   settled work without treating animation as progress evidence.
/// - **Status** — a one-line indicator (`✶ Thinking …`) when we
///   have a turn in flight but no cell content yet. No border —
///   the cue is the spinner, not the frame.
pub(crate) enum ActiveView {
    Empty,
    Status(Line<'static>),
    Active {
        lines: Vec<Line<'static>>,
        /// `true` while still streaming. `false` once finalised.
        live: bool,
    },
}

/// Single live agent's render data inside `ViewportFrame.multi_agent`.
///
/// Compact per-agent row: `{status_icon} {name} · {tools} · {children} ·
/// {elapsed}`. The whole strip renders
/// inside ONE `LiveFramedCell` with a single semantic gutter so the
/// user sees N agents as a tidy panel, not N stacked frames.
pub(crate) struct MultiAgentEntry {
    pub agent_id: String,
    /// Display label — name from the agent spawn action if set, falling back
    /// to description. Building it lives in `agent_monitor_snapshot`-
    /// adjacent territory; here we just render the prepared string.
    pub label: String,
    pub activity: crate::tui::agent_run_projection::AgentActivityCounts,
    pub elapsed_ms: u64,
    pub state: AgentRunState,
}

/// Build the strip header.
///
/// Renders a per-status breakdown so the user sees
/// `live` / `cancelling` / `failed` / `cancelled` / `done` at a glance,
/// not just the total. Pre-fix the header only said "▶ N parallel
/// agents" — failures were invisible, leading users to spawn
/// replacement agents on top of dead ones without realising.
///
/// `cancelled` / `cancelling` are split out from `failed` because user
/// cancellation is an intent, not an alarm — surfacing it as a separate
/// bucket avoids confusing a user-requested stop with an agent failure.
///
/// Hint advertises both `Ctrl+G` (open monitor) and `X` (stop from
/// inside the monitor) so the affordance is discoverable.
pub(crate) fn multi_agent_strip_header(cells: &[MultiAgentEntry]) -> String {
    let total = cells.len();
    let uncertain = cells
        .iter()
        .filter(|c| {
            matches!(
                c.state.confidence,
                AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
            )
        })
        .count();
    let confident = |entry: &&MultiAgentEntry| {
        !matches!(
            entry.state.confidence,
            AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
        )
    };
    let live = cells
        .iter()
        .filter(confident)
        .filter(|c| {
            matches!(
                c.state.status,
                AgentRunStatus::Starting
                    | AgentRunStatus::Running
                    | AgentRunStatus::Pausing
                    | AgentRunStatus::Resuming
            )
        })
        .count();
    let waiting = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Waiting)
        .count();
    let paused = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Paused)
        .count();
    let cancelling = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Cancelling)
        .count();
    let failed = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Failed)
        .count();
    let interrupted = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Interrupted)
        .count();
    let cancelled = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Cancelled)
        .count();
    let done = cells
        .iter()
        .filter(confident)
        .filter(|c| c.state.status == AgentRunStatus::Completed)
        .count();

    let mut breakdown = Vec::with_capacity(5);
    if live > 0 {
        breakdown.push(format!("{live} live"));
    }
    if waiting > 0 {
        breakdown.push(format!("{waiting} waiting"));
    }
    if paused > 0 {
        breakdown.push(format!("{paused} paused"));
    }
    if cancelling > 0 {
        breakdown.push(format!("{cancelling} cancelling"));
    }
    if failed > 0 {
        breakdown.push(format!("{failed} failed"));
    }
    if interrupted > 0 {
        breakdown.push(format!("{interrupted} interrupted"));
    }
    if cancelled > 0 {
        breakdown.push(format!("{cancelled} cancelled"));
    }
    if uncertain > 0 {
        breakdown.push(format!("{uncertain} unconfirmed"));
    }
    if done > 0 {
        breakdown.push(format!("{done} done"));
    }
    let breakdown = if breakdown.is_empty() {
        String::new()
    } else {
        format!(" · {}", breakdown.join(" · "))
    };

    format!(
        "{} {total} parallel agents{breakdown} · Ctrl+G manage",
        crate::tui::glyphs::current().agent_fanout,
    )
}

/// The workbench keeps this one-line activity strip visible above a focused
/// root or agent transcript. It is intentionally a navigation/status cue,
/// not a second agent list: `Ctrl+G` remains the place to inspect and control
/// individual runs.
fn workspace_agent_activity_line(cells: &[MultiAgentEntry], width: u16) -> Line<'static> {
    let text = truncate_label(&multi_agent_strip_header(cells), width as usize);
    Line::from(Span::styled(
        format!("  {text}"),
        Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(ratatui::style::Modifier::BOLD),
    ))
}

/// A compact browser-like tab strip for retained conversations. On narrow
/// terminals the active label wins over an incomplete list: hiding which
/// conversation is on screen is worse than omitting distant tabs that remain
/// reachable through Ctrl+G and Shift+Left/Right.
fn workspace_conversation_tab_line(tabs: &[ConversationTab], width: u16) -> Option<Line<'static>> {
    if tabs.len() < 2 || width == 0 {
        return None;
    }

    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let active_style = Style::default()
        .fg(theme.accent)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let label_budget = 22;
    let labels = tabs
        .iter()
        .map(|tab| truncate_label(&tab.label, label_budget))
        .collect::<Vec<_>>();
    let shortcut = " · Shift+←/→ switch";
    let full_width = 2
        + labels
            .iter()
            .map(|label| UnicodeWidthStr::width(label.as_str()) + 2)
            .sum::<usize>()
        + tabs.len().saturating_sub(1) * 3
        + UnicodeWidthStr::width(shortcut);

    if full_width <= usize::from(width) {
        let mut spans = vec![Span::styled("  ", dim)];
        for (index, (tab, label)) in tabs.iter().zip(labels).enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", dim));
            }
            let marker = if tab.active { "● " } else { "○ " };
            spans.push(Span::styled(
                format!("{marker}{label}"),
                if tab.active { active_style } else { dim },
            ));
        }
        spans.push(Span::styled(shortcut, dim));
        return Some(Line::from(spans));
    }

    let active_label = tabs
        .iter()
        .zip(labels)
        .find_map(|(tab, label)| tab.active.then_some(label))
        .unwrap_or_else(|| "conversation".to_string());
    let summary = format!(
        "  {} conversations · ● {active_label} · Shift+←/→ switch",
        tabs.len()
    );
    Some(Line::from(Span::styled(
        truncate_label(&summary, usize::from(width)),
        active_style,
    )))
}

/// Render the primary workbench canvas without a terminal side effect. Keeping
/// this composition separate makes the transcript + live-agent coexistence
/// directly testable against a real ratatui frame.
fn render_primary_workspace(
    area: Rect,
    buf: &mut Buffer,
    bottom_pane: &mut BottomPane,
    activity: Option<&Line<'static>>,
) -> Option<(u16, u16)> {
    Clear.render(area, buf);
    let mut chrome = Vec::with_capacity(2);
    if let Some(tab_line) =
        workspace_conversation_tab_line(&bottom_pane.conversation_tabs(), area.width)
    {
        chrome.push(tab_line);
    }
    if let Some(activity) = activity {
        chrome.push(activity.clone());
    }
    let workspace_area = if !chrome.is_empty()
        && area.height
            > u16::try_from(chrome.len())
                .unwrap_or(u16::MAX)
                .saturating_add(3)
    {
        let mut constraints = vec![ratatui::layout::Constraint::Length(1); chrome.len()];
        constraints.push(ratatui::layout::Constraint::Min(0));
        let areas = ratatui::layout::Layout::vertical(constraints).split(area);
        for (line, chrome_area) in chrome.into_iter().zip(areas.iter()) {
            Paragraph::new(line).render(*chrome_area, buf);
        }
        *areas.last().expect("content area follows workspace chrome")
    } else {
        area
    };
    // The transcript owns this exact content rectangle, not the outer
    // terminal. Workspace tabs and activity chrome consume rows above it;
    // sizing against the full terminal leaves the last transcript rows
    // unreachable whenever that chrome is visible.
    if bottom_pane.conversation_tab_is_open() {
        bottom_pane.prepare_conversation_workspace(workspace_area.height, workspace_area.width);
    }
    bottom_pane.render(workspace_area, buf);
    bottom_pane.cursor_position(workspace_area)
}

/// Pair of (ActiveView, TaskBoard lines) produced by
/// [`active_viewport`]. The task board is its OWN slot — not an
/// `ActiveView` variant — so a streaming active cell (tool /
/// assistant) no longer replaces the board mid-frame.
///
/// `active` priority (inside the single active-cell slot):
///   1. `active_cell` present → `Active` with lines + kind so the
///      caller can draw a bordered frame.
///   2. Status indicator has content → `Status` line (spinner +
///      short label, no frame). When the board is collapsed but
///      there's an in-flight task, a `Next: <subject>` hint is
///      folded into the status line.
///   3. Nothing → `Empty`. Idle REPL shows nothing above the composer.
pub(crate) struct ViewportFrame {
    pub active: ActiveView,
    /// Parallel agents to render ABOVE the active cell. Rendered as
    /// a stack of `LiveFramedCell`s with a "▶ N parallel agents"
    /// header. Co-exists with `active` (which usually holds an
    /// AssistantCell/ReasoningCell during the parent turn) — they
    /// occupy independent strip rows so neither displaces the
    /// other. `None` when no parallel agents are live.
    pub multi_agent: Option<Vec<MultiAgentEntry>>,
    /// Pre-rendered task board. `None` when the board should not
    /// draw (collapsed, hidden by idle timer, empty, etc.).
    pub task_board: Option<Vec<Line<'static>>>,
    /// The resolved board visibility state after evaluating the
    /// snapshot. Callers MUST write this back to their local
    /// `board_expanded` so that in-turn draws self-correct when
    /// tasks appear mid-turn (rather than waiting for the outer tick).
    pub resolved_board_expanded: bool,
}

fn task_board_truth_line(
    state: TaskBoardTruthState,
    store_health: TaskStoreHealth,
    width: u16,
) -> Option<Line<'static>> {
    if width == 0 {
        return None;
    }
    let theme = crate::tui::theme::current();
    let (text, color) = match state {
        // An unbound task lane has no task evidence and no action the user can
        // take from this surface. Rendering it beside an active run falsely
        // reads as a run lifecycle failure, so keep the optional lane absent.
        TaskBoardTruthState::Unbound => return None,
        TaskBoardTruthState::Loading => ("Checklist · syncing", theme.dim),
        TaskBoardTruthState::Confirmed => return None,
        TaskBoardTruthState::Refreshing => (
            "Checklist · syncing · showing last confirmed checklist",
            theme.accent,
        ),
        TaskBoardTruthState::Stale => match store_health {
            TaskStoreHealth::AuthenticationRequired => (
                "Checklist sync needs sign-in · showing last confirmed checklist",
                theme.warn,
            ),
            TaskStoreHealth::SessionUnavailable => (
                "Checklist sync unavailable for this session · showing last confirmed checklist",
                theme.warn,
            ),
            TaskStoreHealth::ServiceUnavailable => (
                "Checklist storage unavailable · showing confirmed checklist · Ctrl+T → R refresh",
                theme.warn,
            ),
            TaskStoreHealth::TransportUnavailable => (
                "Checklist server unreachable · showing confirmed checklist · Ctrl+T → R refresh",
                theme.warn,
            ),
            TaskStoreHealth::ProtocolMismatch => (
                "Checklist sync protocol mismatch · showing last confirmed checklist",
                theme.warn,
            ),
            TaskStoreHealth::Unknown | TaskStoreHealth::Ready => (
                "Checklist sync delayed · showing last confirmed checklist · Ctrl+T → R refresh",
                theme.warn,
            ),
        },
        TaskBoardTruthState::Unavailable => match store_health {
            TaskStoreHealth::AuthenticationRequired => ("Checklist sync needs sign-in", theme.warn),
            TaskStoreHealth::SessionUnavailable => {
                ("Checklist sync unavailable for this session", theme.warn)
            }
            TaskStoreHealth::ServiceUnavailable => (
                "Checklist storage unavailable · Ctrl+T → R refresh",
                theme.warn,
            ),
            TaskStoreHealth::TransportUnavailable => (
                "Checklist server unreachable · Ctrl+T → R refresh",
                theme.warn,
            ),
            TaskStoreHealth::ProtocolMismatch => (
                "Checklist sync protocol mismatch · check client/server versions",
                theme.warn,
            ),
            TaskStoreHealth::Unknown | TaskStoreHealth::Ready => (
                "Checklist sync unavailable · Ctrl+T → R refresh",
                theme.warn,
            ),
        },
    };
    Some(Line::from(Span::styled(
        truncate_state_line(text, width as usize),
        Style::default().fg(color),
    )))
}

/// A degraded canonical plan source must be visible when we are still showing
/// its last confirmed steps. Do not render an initial unavailable/loading
/// source: a session without a plan should stay quiet instead of suggesting a
/// work failure that may not exist.
fn projected_task_truth_line(state: ProjectedTaskTruthState, width: u16) -> Option<Line<'static>> {
    if width == 0 || state != ProjectedTaskTruthState::Stale {
        return None;
    }
    let theme = crate::tui::theme::current();
    Some(Line::from(Span::styled(
        truncate_state_line(
            "Plan state delayed · showing last confirmed steps",
            width as usize,
        ),
        Style::default().fg(theme.warn),
    )))
}

fn truncate_state_line(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if text.width() <= max_width {
        return text.to_string();
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let char_width = ch.width().unwrap_or(0);
        if used + char_width > max_width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += char_width;
    }
    out.push('…');
    out
}

pub(crate) fn active_viewport(
    chat_widget: &chat_widget::ChatWidget,
    status: &status_indicator::StatusIndicator,
    board: Option<&TaskBoardObserver>,
    board_expanded: bool,
    board_user_pin: Option<bool>,
    width: u16,
    rows: u16,
) -> ViewportFrame {
    // Reserve 2 cols for the `█ ` gutter.
    let inner_w = width.saturating_sub(2).max(20);

    // Multi-agent strip: one compact row per logical background
    // agent, not per control-plane spawn/get_result action
    // call. Co-exists with `active_cell` (assistant streaming WHILE
    // sub-agents run shows both).
    //
    // Visibility rule: show the strip while ANY agent is still live
    // (running/cancelling). Completed-only and cancelled-only strips
    // linger for `STRIP_LINGER` so the user can glance at the final
    // state, then dismiss. Failed entries stay visible until a new
    // turn state replaces them, so failures are not silently missed
    // after a short linger timeout. Users can still drill in via Ctrl+G.
    const STRIP_LINGER: std::time::Duration = std::time::Duration::from_secs(5);
    let _ = inner_w; // retained for future per-row layout decisions
    let agent_ids = chat_widget.agent_run_ids();
    let multi_agent_active = if !agent_ids.is_empty() {
        let cells: Vec<MultiAgentEntry> = agent_ids
            .iter()
            .filter_map(|id| {
                chat_widget.agent_run_cell(id).map(|tc| MultiAgentEntry {
                    agent_id: id.clone(),
                    label: tc.description.clone(),
                    activity: chat_widget
                        .agent_run_activity_counts(id)
                        .expect("agent id and activity are read from one registry"),
                    elapsed_ms: tc
                        .duration_ms
                        .unwrap_or_else(|| tc.started_at.elapsed().as_millis() as u64),
                    state: chat_widget
                        .agent_run_state(id)
                        .expect("agent id and projection are read from one registry"),
                })
            })
            .collect();

        let any_live = cells.iter().any(|entry| entry.state.is_actionable_active());
        let any_recently_terminal = !any_live
            && agent_ids.iter().any(|id| {
                chat_widget
                    .agent_run_terminal_at(id)
                    .is_some_and(|terminal_at| terminal_at.elapsed() < STRIP_LINGER)
            });
        if should_show_multi_agent_strip(&cells, any_recently_terminal) {
            Some(cells)
        } else {
            None
        }
    } else {
        None
    };

    // The active viewport has finite physical space. A mutable assistant reply
    // must therefore never build/render its whole growing Markdown document on
    // every token; use the bounded live tail and reserve rich Markdown for the
    // finalized background layout path.
    let live_assistant_rows = usize::from(rows.saturating_sub(10).clamp(4, 48));
    let active = chat_widget.active_cell().and_then(|cell| {
        let lines = if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>()
            && cell.is_live()
        {
            assistant.live_viewport_lines(inner_w, live_assistant_rows)
        } else {
            cell.display_lines(inner_w)
        };
        let lines = sanitize_lines_for_terminal(lines);
        if lines.is_empty() {
            None
        } else {
            Some((lines, cell.is_live()))
        }
    });
    // Pump the observer before reading its snapshot. Without this,
    // mid-turn `task_board.create` / `task_board.update` calls land in
    // `session_todos` but never propagate into the snapshot until
    // the outer-tick branch fires `maybe_refresh()` — which can
    // be 30+ seconds for a long agentic loop. The observer's own
    // dirty/window gating makes per-frame calls cheap (no fetch
    // unless something actually changed).
    if let Some(b) = board {
        b.maybe_refresh();
    }
    // Rows and confidence are cloned under one observer lock. Reading them
    // separately could pair a pre-refresh cache with post-refresh confidence.
    let projection = board.map(TaskBoardObserver::active_projection);
    let truth_state = projection.as_ref().map(TaskBoardProjection::truth_state);
    let has_confirmed_truth = truth_state.is_some_and(TaskBoardTruthState::has_confirmed_truth);
    let has_tasks = projection
        .as_ref()
        .is_some_and(TaskBoardProjection::has_tasks);
    // `hidden` only records an explicit compact-board collapse. It is never
    // derived from a terminal-task timeout, so completed work remains
    // reachable through Ctrl+T and the primary Taskboard.
    let hidden = matches!(
        projection.as_ref(),
        Some(TaskBoardProjection::Single { snapshot, .. }) if snapshot.hidden
    );
    // Resolve board visibility from the FRESH snapshot every frame.
    // This is the critical fix: previously, resolve_board_visibility
    // only ran in the outer-tick, so in-turn draws always saw the
    // stale `board_expanded = false` from turn start. Now every draw
    // self-corrects — if task_board.create lands mid-turn, the board opens
    // on the very next frame (≤50ms).
    let (resolved_expanded, _reset_pin) =
        super::board_pin::resolve_board_visibility(board_expanded, board_user_pin, has_tasks);

    // Three-mode board:
    //   - hidden                       → render nothing
    //   - !expanded, has tasks         → one-line collapsed summary
    //   - expanded                     → full panel (single or multi-session)
    //
    // Earlier flow tied "no board" to "render Next: hint into the
    // active region". With bottom-anchor + always-on collapsed
    // summary, the hint is now redundant — the summary IS the hint.
    let store_health = projection
        .as_ref()
        .map(TaskBoardProjection::store_health)
        .unwrap_or_default();
    let mut state_lines = Vec::with_capacity(2);
    // Do not surface an automatically-refreshing optional checklist lane during
    // startup when it contributes no task. Once the user opens the board or
    // there is any displayable work, show its independently attributable
    // health without hiding the canonical task projection.
    let checklist_lane_is_relevant =
        has_confirmed_truth || has_tasks || board_expanded || board_user_pin.is_some();
    if checklist_lane_is_relevant
        && let Some(line) =
            truth_state.and_then(|state| task_board_truth_line(state, store_health, width))
    {
        state_lines.push(line);
    }
    if let Some(line) = projection.as_ref().and_then(|projection| match projection {
        TaskBoardProjection::Single {
            projected_truth_state,
            ..
        }
        | TaskBoardProjection::All {
            projected_truth_state,
            ..
        } => projected_task_truth_line(*projected_truth_state, width),
    }) {
        state_lines.push(line);
    }
    let row_budget = rows.saturating_sub(state_lines.len() as u16);
    let mut cached_lines = if has_tasks && !hidden {
        match projection.as_ref() {
            Some(TaskBoardProjection::Single { snapshot, .. }) if resolved_expanded => {
                let fresh_task_ids = board
                    .map(TaskBoardObserver::fresh_task_id_set)
                    .unwrap_or_default();
                task_list::render_with_fresh_predicate(
                    &snapshot.tasks,
                    width,
                    row_budget,
                    true,
                    |task_id| fresh_task_ids.contains(task_id),
                )
            }
            Some(TaskBoardProjection::All { snapshot, .. }) if resolved_expanded => {
                task_list::render_multi(&snapshot.per_session, width, row_budget)
            }
            Some(TaskBoardProjection::Single { snapshot, .. }) => {
                task_list::render_collapsed_summary(&snapshot.tasks, width)
                    .into_iter()
                    .collect()
            }
            Some(TaskBoardProjection::All { snapshot, .. }) => {
                let tasks = snapshot
                    .per_session
                    .iter()
                    .flat_map(|(_, tasks)| tasks.iter().cloned())
                    .collect::<Vec<_>>();
                task_list::render_collapsed_multi_summary(&tasks, width)
                    .into_iter()
                    .collect()
            }
            None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let task_board = if state_lines.is_empty() && cached_lines.is_empty() {
        None
    } else {
        let mut lines = Vec::with_capacity(cached_lines.len() + state_lines.len());
        lines.append(&mut state_lines);
        lines.append(&mut cached_lines);
        Some(lines)
    };
    let status_line = status.render();
    // Hint suppressed once the collapsed summary covers the same
    // ground (current/next + status icon). Only fall back to the
    // hint when we genuinely have no board slot to fill.
    let next_hint = match projection.as_ref() {
        Some(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            snapshot,
            ..
        }) if task_board.is_none() && !hidden => {
            task_list::render_next_hint(&snapshot.tasks, width)
        }
        _ => None,
    };
    let active = pick_active_view(active, status_line, next_hint);
    ViewportFrame {
        active,
        multi_agent: multi_agent_active,
        task_board,
        resolved_board_expanded: resolved_expanded,
    }
}

fn should_show_multi_agent_strip(cells: &[MultiAgentEntry], any_recently_terminal: bool) -> bool {
    let any_live = cells.iter().any(|entry| entry.state.is_actionable_active());
    let any_attention = cells.iter().any(|entry| {
        entry.state.status.is_failure()
            || entry.state.status == AgentRunStatus::Interrupted
            || matches!(
                entry.state.confidence,
                AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
            )
    });
    any_live || any_attention || any_recently_terminal
}

/// Pure priority resolver. Priority: **Active > Status > NextHint >
/// Empty**. The task board is NOT on this chain anymore — it has its
/// own sibling slot in the frame so a streaming active cell cannot
/// flicker-replace it. See [`ViewportFrame`].
pub(crate) fn pick_active_view(
    active: Option<(Vec<Line<'static>>, bool)>,
    status_line: Option<Line<'static>>,
    next_hint: Option<Line<'static>>,
) -> ActiveView {
    // Priority: Active > Status > NextHint > Empty. Multi-agent
    // strip is now a SIBLING field on ViewportFrame (rendered
    // above the active slot) so this resolver no longer needs to
    // arbitrate between them — both can co-exist.
    if let Some((lines, live)) = active {
        return ActiveView::Active { lines, live };
    }
    if let Some(line) = status_line {
        return ActiveView::Status(line);
    }
    if let Some(line) = next_hint {
        return ActiveView::Status(line);
    }
    ActiveView::Empty
}

// ───────────────────────────────────────────────────────────────────────
// Footer sync
// ───────────────────────────────────────────────────────────────────────

/// Reflect the task-board observer's current state on the footer
/// so the status-line chip is accurate on the very next draw. Kept
/// separate from `active_viewport` because several call sites draw
/// without recomputing the active view (tick path) and still want
/// the chip refreshed.
pub(crate) fn sync_task_footer(
    footer: &mut Footer,
    board: &TaskBoardObserver,
    board_expanded: bool,
) {
    // Use the no-clone counts() helper — this runs on every draw
    // and snapshot() would otherwise clone the full task vec just
    // for the two integers the footer chip needs.
    let (open, total, _hidden) = board.counts();
    footer.task_counts = if total == 0 {
        None
    } else {
        Some((open, total))
    };
    footer.task_board_expanded = board_expanded;
}

// ───────────────────────────────────────────────────────────────────────
// Frame composition
// ───────────────────────────────────────────────────────────────────────

/// Paint one frame. Layout top-to-bottom:
/// `task_board (optional) | active | separator | bottom_pane`.
/// The task board is its OWN slot — it does NOT race the active
/// cell for priority, so a streaming tool/assistant cell can't
/// hide it mid-frame.
pub(crate) fn do_draw(
    guard: &mut TerminalGuard,
    active: ActiveView,
    multi_agent: Option<Vec<MultiAgentEntry>>,
    bottom_pane: &mut BottomPane,
    task_board: Option<(&TaskBoardObserver, bool)>,
    task_board_lines: Option<Vec<Line<'static>>>,
) -> Result<(), String> {
    guard
        .ensure_tui_modes()
        .map_err(|e| format!("failed to restore terminal input mode: {e}"))?;
    if let Some((board, expanded)) = task_board {
        sync_task_footer(&mut bottom_pane.footer, board, expanded);
        bottom_pane.refresh_task_board(&board.active_projection());
    }
    bottom_pane.pre_draw_tick(std::time::Instant::now());

    let terminal_size = guard.terminal.size().unwrap_or(ratatui::layout::Size {
        width: 80,
        height: 24,
    });
    let width = terminal_size.width;
    let workspace_agent_activity = multi_agent
        .as_deref()
        .map(|cells| workspace_agent_activity_line(cells, width));

    // Root and delegated conversations are peers. Once one is focused, it
    // replaces the root chat canvas rather than rendering as another pane
    // below it. The run navigator remains reachable through Ctrl+G and
    // Left/Esc returns to it, giving this surface browser-tab semantics.
    if bottom_pane.primary_workspace_is_open() {
        let height = terminal_size.height.max(1);
        return guard
            .draw(height, |frame| {
                let area = frame.area();
                if let Some((x, y)) = render_primary_workspace(
                    area,
                    frame.buffer_mut(),
                    bottom_pane,
                    workspace_agent_activity.as_ref(),
                ) {
                    frame.set_cursor_position((x, y));
                }
            })
            .map_err(|error| format!("draw conversation workspace: {error}"));
    }

    let ac_renderable: RenderableItem<'_> = match active {
        ActiveView::Empty => RenderableItem::Owned(Box::new(())),
        // Status line (spinner + "Thinking…") renders flush with
        // scrollback — no frame, the spinner itself carries the
        // "something's happening" signal.
        ActiveView::Status(line) => {
            let para = Paragraph::new(ratatui::text::Text::from(vec![line]));
            RenderableItem::Owned(Box::new(para))
        }
        // Active work has a stable semantic gutter rather than a color
        // animation. New stream events still schedule draws immediately.
        ActiveView::Active { lines, live } => {
            let framed = LiveFramedCell { lines, live };
            RenderableItem::Owned(Box::new(framed))
        }
    };

    // Multi-agent strip: one compact gradient-gutter frame containing a header and one short row
    // per live agent. Renders as e.g.:
    //
    //   █ ▶ 3 parallel agents · 1 live · 1 failed · 1 done · Ctrl+G manage
    //   █ ◦ review_tui      · 2 tools · 1 child · 12s
    //   █ ✗ review_fixes    · 8s
    //   █ ✓ review_refactor · 4 tools · 18s
    //
    // (status: ◦ live / ✓ completed / ✗ failed)
    let multi_agent_renderable: Option<RenderableItem<'_>> = multi_agent.map(|cells| {
        let theme = crate::tui::theme::current();
        // Split live / failed / done so a 3-agent strip with one
        // failure is visible at a glance — without this the user only
        // sees "▶ 3 parallel agents" while one is silently dead.
        // The strip advertises the key that actually works from the chat
        // surface. Stop controls are shown after Ctrl+G opens Workbench;
        // claiming `X stop` here would conflict with ordinary text input.
        let header_line = Line::from(ratatui::text::Span::styled(
            multi_agent_strip_header(&cells),
            ratatui::style::Style::default()
                .fg(theme.accent)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));

        // Width budget for the label — leave room for status icon (2),
        // Leave room for activity counts and elapsed time.
        // (~6) ≈ 24 chars of overhead.
        let total_overhead = 24;
        let label_budget = (width as usize).saturating_sub(2 + total_overhead).max(10);

        let dim = ratatui::style::Style::default().fg(theme.dim);
        let glyphs = crate::tui::glyphs::current();
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(cells.len() + 1);
        lines.push(header_line);
        for entry in cells {
            let (icon, icon_color) = match entry.state.confidence {
                AgentProjectionConfidence::Unconfirmed => (glyphs.agent_unconfirmed, theme.dim),
                AgentProjectionConfidence::Stale => (glyphs.agent_stale, theme.warn),
                AgentProjectionConfidence::Observed | AgentProjectionConfidence::Confirmed => {
                    match entry.state.status {
                        AgentRunStatus::Starting
                        | AgentRunStatus::Running
                        | AgentRunStatus::Pausing
                        | AgentRunStatus::Resuming => (glyphs.agent_running, theme.warn),
                        AgentRunStatus::Waiting | AgentRunStatus::Paused => {
                            (glyphs.agent_waiting, theme.warn)
                        }
                        AgentRunStatus::Cancelling => (glyphs.agent_cancelling, theme.warn),
                        AgentRunStatus::Completed | AgentRunStatus::Delegated => {
                            (glyphs.agent_completed, theme.success)
                        }
                        AgentRunStatus::Interrupted => (glyphs.agent_interrupted, theme.warn),
                        AgentRunStatus::Failed => (glyphs.agent_failed, theme.error),
                        AgentRunStatus::Cancelled => (glyphs.agent_cancelled, theme.dim),
                    }
                }
            };
            let label = truncate_label(&entry.label, label_budget);
            let trailing = match entry.state.confidence {
                AgentProjectionConfidence::Unconfirmed => " · Status unconfirmed",
                AgentProjectionConfidence::Stale => " · Stale",
                AgentProjectionConfidence::Observed | AgentProjectionConfidence::Confirmed => {
                    match entry.state.status {
                        AgentRunStatus::Starting => " · Starting…",
                        AgentRunStatus::Waiting => " · Waiting",
                        AgentRunStatus::Paused => " · Paused",
                        AgentRunStatus::Pausing => " · Pausing…",
                        AgentRunStatus::Resuming => " · Resuming…",
                        AgentRunStatus::Cancelling => " · Cancelling…",
                        AgentRunStatus::Interrupted => " · Interrupted",
                        AgentRunStatus::Cancelled => " · Cancelled",
                        AgentRunStatus::Delegated => " · Delegated",
                        AgentRunStatus::Running
                        | AgentRunStatus::Completed
                        | AgentRunStatus::Failed => "",
                    }
                }
            };
            let activity = compact_agent_activity(entry.activity);
            let suffix = if activity.is_empty() {
                format!(" · {}{}", format_short_elapsed(entry.elapsed_ms), trailing)
            } else {
                format!(
                    " · {activity} · {}{}",
                    format_short_elapsed(entry.elapsed_ms),
                    trailing,
                )
            };
            let row = Line::from(vec![
                ratatui::text::Span::styled(
                    icon.to_string(),
                    ratatui::style::Style::default().fg(icon_color),
                ),
                ratatui::text::Span::raw(" "),
                ratatui::text::Span::raw(label),
                ratatui::text::Span::styled(suffix, dim),
            ]);
            lines.push(row);
        }

        let framed = LiveFramedCell { lines, live: true };
        RenderableItem::Owned(Box::new(framed) as Box<dyn Renderable>)
    });

    // Breathing room between scrollback and the bottom surface.
    // Earlier versions used a full-width rule here, but the extra line
    // made the composer feel boxed off rather than integrated with the
    // rest of the screen.
    let sep_line = separator_line(width);
    let sep_renderable = RenderableItem::Owned(Box::new(sep_line));

    let bp_renderable = BottomPaneRenderable(bottom_pane);
    let bp_item = RenderableItem::Owned(Box::new(bp_renderable) as Box<dyn Renderable>);

    let mut flex = FlexRenderable::new();
    // Layout (top → bottom):
    //   active cell + scrollback   (weight=1, soaks remaining space)
    //   multi-agent strip          (weight=0, only if any sub-agents are live)
    //   separator                  (weight=0)  ← visual break above board
    //   task board                 (weight=0, pinned just above composer)
    //   blank spacer               (weight=0)  ← only when board renders
    //   bottom pane / composer     (weight=0)
    //
    // Earlier iterations stacked the board ABOVE the active cell. That broke down for
    // long agentic turns: streaming text kept pushing the board further
    // from the composer until it was off-screen entirely. Bottom-anchor
    // keeps the board adjacent to the composer — the user's eye only
    // moves between two adjacent regions (composer ↔ board).
    //
    // The blank spacer between board and composer prevents the
    // collapsed summary from clinging to the input prompt — without
    // it the eye reads `⠋ N tasks ...› Ask astra ...` as one cramped
    // strip. Only inserted when the board actually renders so an
    // empty-task session doesn't waste a row.
    flex.push(1, ac_renderable);
    if let Some(item) = multi_agent_renderable {
        flex.push(0, item);
    }
    flex.push(0, sep_renderable);
    let board_rendered = task_board_lines
        .as_ref()
        .is_some_and(|lines| !lines.is_empty());
    if let Some(lines) = task_board_lines
        && !lines.is_empty()
    {
        let para = Paragraph::new(ratatui::text::Text::from(lines));
        flex.push(0, RenderableItem::Owned(Box::new(para)));
    }
    if board_rendered {
        let spacer = Paragraph::new(ratatui::text::Text::from(vec![Line::from(
            ratatui::text::Span::raw(""),
        )]));
        flex.push(0, RenderableItem::Owned(Box::new(spacer)));
    }
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

fn separator_line(width: u16) -> Line<'static> {
    let dim = Style::default().fg(crate::tui::theme::current().dim);
    Line::from(Span::styled("─".repeat(width as usize), dim))
}

struct BottomPaneRenderable<'a>(&'a mut BottomPane);

impl<'a> Renderable for BottomPaneRenderable<'a> {
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

// ───────────────────────────────────────────────────────────────────────
// LiveFramedCell — gradient gutter renderer for the active cell
// ───────────────────────────────────────────────────────────────────────

/// Left-gutter renderable: a solid `█` bar on the left edge. Its semantic
/// color communicates whether the cell is live or settled; motion is not
/// treated as evidence that useful work is happening.
struct LiveFramedCell {
    lines: Vec<Line<'static>>,
    live: bool,
}

impl super::render::renderable::Renderable for LiveFramedCell {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        use ratatui::widgets::{Paragraph, Widget};

        // Need at least 3 cols: `█` + space + 1 col of content.
        if area.width < 3 || area.height == 0 {
            return;
        }

        // Inner paragraph area: 2 cols reserved for `█ ` on the left.
        let inner = ratatui::layout::Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: area.height,
        };

        let para = Paragraph::new(ratatui::text::Text::from(self.lines.clone()));
        Widget::render(para, inner, buf);

        let theme = crate::tui::theme::current();
        let color = if self.live {
            theme.gutter
        } else {
            theme.gutter_frozen
        };
        for row in 0..area.height as usize {
            set_char(buf, area.x, area.y + row as u16, '█', color);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        self.lines.len() as u16
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

/// Compact "12s" / "1m30s" / "850ms" — used by the multi-agent strip
/// rows so each agent's elapsed time fits in ~6 chars.
pub(crate) fn format_short_elapsed(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{}s", ms / 1_000)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1_000;
        if secs == 0 {
            format!("{mins}m")
        } else {
            format!("{mins}m{secs}s")
        }
    }
}

fn compact_agent_activity(
    activity: crate::tui::agent_run_projection::AgentActivityCounts,
) -> String {
    let mut parts = Vec::new();
    if activity.tool_calls > 0 {
        parts.push(format!(
            "{} tool{}",
            activity.tool_calls,
            if activity.tool_calls == 1 { "" } else { "s" }
        ));
    }
    if activity.child_agents > 0 {
        let qualifier = if activity.child_agents_partial {
            "≥"
        } else {
            ""
        };
        parts.push(format!(
            "{qualifier}{} child{}",
            activity.child_agents,
            if activity.child_agents == 1 {
                ""
            } else {
                "ren"
            }
        ));
    }
    if activity.messages_sent > 0 || activity.messages_received > 0 {
        parts.push(format!(
            "↑{} ↓{}",
            activity.messages_sent, activity.messages_received
        ));
    }
    parts.join(" · ")
}

// ───────────────────────────────────────────────────────────────────────
// Priority resolver tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod active_view_priority_tests {
    //! Truth-table pinning for `pick_active_view`'s priority:
    //! **Active > Status > NextHint > Empty**.
    //!
    //! The task board lives in its OWN slot (see [`ViewportFrame`]),
    //! not on this chain — these tests pin the remaining 2^3 table.
    use super::{ActiveView, pick_active_view};
    use ratatui::text::{Line, Span};

    fn lines(tag: &str) -> Vec<Line<'static>> {
        vec![Line::from(Span::raw(tag.to_string()))]
    }
    fn line(tag: &str) -> Line<'static> {
        Line::from(Span::raw(tag.to_string()))
    }

    fn discriminate(v: &ActiveView) -> &'static str {
        match v {
            ActiveView::Empty => "Empty",
            ActiveView::Status(_) => "Status",
            ActiveView::Active { .. } => "Active",
        }
    }
    fn first_text(v: &ActiveView) -> String {
        match v {
            ActiveView::Empty => String::new(),
            ActiveView::Status(l) => l
                .spans
                .first()
                .map(|s| s.content.to_string())
                .unwrap_or_default(),
            ActiveView::Active { lines, .. } => lines
                .first()
                .and_then(|l| l.spans.first())
                .map(|s| s.content.to_string())
                .unwrap_or_default(),
        }
    }

    #[test]
    fn active_beats_status_and_hint() {
        let v = pick_active_view(
            Some((lines("active-tool"), true)),
            Some(line("status")),
            Some(line("next-hint")),
        );
        assert_eq!(discriminate(&v), "Active");
        assert_eq!(first_text(&v), "active-tool");
    }

    #[test]
    fn status_beats_hint() {
        let v = pick_active_view(None, Some(line("status")), Some(line("next-hint")));
        assert_eq!(discriminate(&v), "Status");
        assert_eq!(first_text(&v), "status");
    }

    #[test]
    fn hint_used_when_alone() {
        let v = pick_active_view(None, None, Some(line("next-hint")));
        assert_eq!(discriminate(&v), "Status");
        assert_eq!(first_text(&v), "next-hint");
    }

    #[test]
    fn empty_when_all_none() {
        let v = pick_active_view(None, None, None);
        assert_eq!(discriminate(&v), "Empty");
    }

    #[test]
    fn exhaustive_priority_table() {
        let cases: &[(bool, bool, bool, &str)] = &[
            (false, false, false, "Empty"),
            (false, false, true, "Status"),
            (false, true, false, "Status"),
            (false, true, true, "Status"),
            (true, false, false, "Active"),
            (true, false, true, "Active"),
            (true, true, false, "Active"),
            (true, true, true, "Active"),
        ];
        for &(a, s, h, expected) in cases {
            let v = pick_active_view(
                if a { Some((lines("a"), true)) } else { None },
                if s { Some(line("s")) } else { None },
                if h { Some(line("h")) } else { None },
            );
            assert_eq!(
                discriminate(&v),
                expected,
                "priority broken for (active={a}, status={s}, hint={h})"
            );
        }
    }
}

#[cfg(test)]
mod task_board_draw_tests {
    use super::{
        ActiveView, active_viewport, projected_task_truth_line, sync_task_footer,
        task_board_truth_line,
    };
    use crate::tui::{
        bottom_pane::footer::Footer,
        chat_widget, status_indicator,
        task_board_observer::{ProjectedTaskTruthState, TaskBoardTruthState},
    };
    use astra_tools::task_mgmt::{
        InMemoryTaskStore, SessionTask, SessionTaskStatusKind, TaskManager, TaskStore,
        TaskStoreHealth,
    };
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};
    use unicode_width::UnicodeWidthStr;

    type SingleLoadResult = Result<Vec<SessionTask>, String>;
    type MultiLoadResult = Result<Vec<(String, Vec<SessionTask>)>, String>;

    async fn wait_until<F: Fn() -> bool>(cond: F, timeout_ms: u64, pump: impl Fn()) {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while !cond() && Instant::now() < deadline {
            pump();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    struct DrawScriptStore {
        single: Mutex<VecDeque<SingleLoadResult>>,
        all: Mutex<VecDeque<MultiLoadResult>>,
        notify: tokio::sync::broadcast::Sender<String>,
    }

    impl DrawScriptStore {
        fn new(single: Vec<SingleLoadResult>, all: Vec<MultiLoadResult>) -> Arc<Self> {
            let (notify, _) = tokio::sync::broadcast::channel(8);
            Arc::new(Self {
                single: Mutex::new(single.into()),
                all: Mutex::new(all.into()),
                notify,
            })
        }

        fn mark_changed(&self, session_id: &str) {
            let _ = self.notify.send(session_id.to_string());
        }
    }

    #[async_trait]
    impl TaskStore for DrawScriptStore {
        async fn load(&self, _session_id: &str) -> Result<Vec<SessionTask>, String> {
            self.single
                .lock()
                .expect("single draw script lock")
                .pop_front()
                .unwrap_or_else(|| Err("single draw script exhausted".to_string()))
        }

        async fn load_open_sessions(
            &self,
            _limit: usize,
        ) -> Result<Vec<(String, Vec<SessionTask>)>, String> {
            self.all
                .lock()
                .expect("all-session draw script lock")
                .pop_front()
                .unwrap_or_else(|| Err("all-session draw script exhausted".to_string()))
        }

        async fn save(&self, _session_id: &str, _tasks: Vec<SessionTask>) -> Result<(), String> {
            Ok(())
        }

        async fn next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        async fn peek_next_task_id(&self, _session_id: &str) -> Result<u32, String> {
            Ok(1)
        }

        fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<String>> {
            Some(self.notify.subscribe())
        }
    }

    fn draw_task(id: &str, title: &str) -> SessionTask {
        SessionTask {
            archived_at: None,
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status: SessionTaskStatusKind::Pending,
            subtasks: Vec::new(),
            created_at: "2026-07-11T00:00:00Z".to_string(),
            updated_at: "2026-07-11T00:00:00Z".to_string(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    fn board_text(frame: &super::ViewportFrame) -> String {
        frame
            .task_board
            .as_ref()
            .into_iter()
            .flatten()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn wait_for_truth(
        observer: &crate::tui::task_board_observer::TaskBoardObserver,
        expected: crate::tui::task_board_observer::TaskBoardTruthState,
    ) {
        wait_until(
            || observer.truth_state() == expected,
            500,
            || observer.maybe_refresh(),
        )
        .await;
        assert_eq!(observer.truth_state(), expected);
    }

    #[test]
    fn task_truth_notices_are_single_line_and_terminal_width_bounded() {
        use crate::tui::task_board_observer::TaskBoardTruthState;

        for state in [
            TaskBoardTruthState::Loading,
            TaskBoardTruthState::Refreshing,
            TaskBoardTruthState::Stale,
            TaskBoardTruthState::Unavailable,
        ] {
            let line = task_board_truth_line(state, TaskStoreHealth::Unknown, 12)
                .expect("non-confirmed state notice");
            let text = line
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(text.width() <= 12, "state={state:?} text={text:?}");
            assert!(!text.contains('\n'));
        }
        assert!(
            task_board_truth_line(TaskBoardTruthState::Unbound, TaskStoreHealth::Unknown, 12)
                .is_none()
        );
        assert!(
            task_board_truth_line(TaskBoardTruthState::Confirmed, TaskStoreHealth::Ready, 12)
                .is_none()
        );
        assert!(
            task_board_truth_line(
                TaskBoardTruthState::Unavailable,
                TaskStoreHealth::Unknown,
                0
            )
            .is_none()
        );
    }

    #[test]
    fn stale_plan_projection_is_visible_but_absent_plan_is_quiet() {
        let stale = projected_task_truth_line(ProjectedTaskTruthState::Stale, 80)
            .expect("stale plan rows need an attribution notice");
        let text = stale
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("last confirmed steps"));
        assert!(projected_task_truth_line(ProjectedTaskTruthState::Loading, 80).is_none());
        assert!(projected_task_truth_line(ProjectedTaskTruthState::Unavailable, 80).is_none());
    }

    #[test]
    fn task_truth_notice_uses_structured_store_health() {
        let text = |health| {
            task_board_truth_line(TaskBoardTruthState::Unavailable, health, 100)
                .expect("notice")
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        let service = text(TaskStoreHealth::ServiceUnavailable);
        assert!(
            service.contains("Checklist storage unavailable"),
            "{service}"
        );
        assert!(service.contains("Ctrl+T → R refresh"), "{service}");

        let transport = text(TaskStoreHealth::TransportUnavailable);
        assert!(
            transport.contains("Checklist server unreachable"),
            "{transport}"
        );

        let auth = text(TaskStoreHealth::AuthenticationRequired);
        assert!(auth.contains("needs sign-in"), "{auth}");
        assert!(!auth.contains("R refresh"), "{auth}");

        let session = text(TaskStoreHealth::SessionUnavailable);
        assert!(session.contains("this session"), "{session}");
        assert!(!session.contains("R refresh"), "{session}");

        let protocol = text(TaskStoreHealth::ProtocolMismatch);
        assert!(protocol.contains("protocol mismatch"), "{protocol}");
        assert!(!protocol.contains("R refresh"), "{protocol}");
    }

    #[tokio::test]
    async fn first_read_failure_renders_unavailable_without_leaking_diagnostics() {
        use crate::tui::task_board_observer::{TaskBoardObserver, TaskBoardTruthState};

        let store = DrawScriptStore::new(
            vec![Err("SECRET database topology and credentials".to_string())],
            Vec::new(),
        );
        let observer = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "draw-failed");
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;

        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            false,
            None,
            80,
            24,
        );
        let text = board_text(&frame);
        assert!(text.is_empty(), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn unavailable_empty_checklist_is_explained_after_the_user_opens_the_board() {
        use crate::tui::task_board_observer::{TaskBoardObserver, TaskBoardTruthState};

        let store = DrawScriptStore::new(
            vec![Err("SECRET database topology and credentials".to_string())],
            Vec::new(),
        );
        let observer = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "draw-failed");
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;

        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            true,
            Some(true),
            80,
            24,
        );
        let text = board_text(&frame);
        assert!(text.contains("Checklist sync unavailable"), "{text}");
        assert!(text.contains("Ctrl+T → R refresh"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn confirmed_plan_work_remains_visible_when_checklist_sync_fails() {
        use crate::tui::task_board_observer::{TaskBoardObserver, TaskBoardTruthState};

        let store = DrawScriptStore::new(
            vec![Err("SECRET checklist endpoint failure".to_string())],
            Vec::new(),
        );
        let observer = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "draw-plan");
        observer.set_projected_task_projection(
            vec![draw_task("plan:plan-9:step-1", "verify durable plan work")],
            ProjectedTaskTruthState::Confirmed,
        );
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Unavailable).await;

        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            false,
            None,
            80,
            24,
        );
        let text = board_text(&frame);
        assert!(text.contains("verify durable plan work"), "{text}");
        assert!(text.contains("Checklist sync unavailable"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn stale_notice_keeps_last_confirmed_task_visible() {
        use crate::tui::task_board_observer::{TaskBoardObserver, TaskBoardTruthState};

        let store = DrawScriptStore::new(
            vec![
                Ok(vec![draw_task("task-1", "keep confirmed work visible")]),
                Err("SECRET refresh failure".to_string()),
            ],
            Vec::new(),
        );
        let observer = TaskBoardObserver::new(store.clone() as Arc<dyn TaskStore>, "draw-stale");
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;

        store.mark_changed("draw-stale");
        tokio::time::sleep(Duration::from_millis(20)).await;
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Stale).await;

        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            false,
            None,
            80,
            24,
        );
        let text = board_text(&frame);
        assert!(text.contains("Checklist sync delayed"), "{text}");
        assert!(text.contains("keep confirmed work visible"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn all_sessions_visibility_does_not_inherit_single_session_empty_hidden_state() {
        use crate::tui::task_board_observer::{TaskBoardObserver, TaskBoardTruthState};

        let store = DrawScriptStore::new(
            vec![Ok(Vec::new())],
            vec![Ok(vec![(
                "cloud-session".to_string(),
                vec![draw_task("task-cloud", "visible cross-session work")],
            )])],
        );
        let observer = TaskBoardObserver::new(store as Arc<dyn TaskStore>, "draw-single-empty");
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        assert!(
            observer.snapshot().hidden,
            "confirmed empty single lane hides itself"
        );

        observer.toggle_view_mode();
        observer.maybe_refresh();
        wait_for_truth(&observer, TaskBoardTruthState::Confirmed).await;
        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            false,
            None,
            80,
            24,
        );
        let text = board_text(&frame);
        assert!(text.contains("visible cross-session work"), "{text}");
    }

    #[tokio::test]
    async fn footer_keeps_completed_count_when_board_hidden() {
        let store = Arc::new(InMemoryTaskStore::new());
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new(
            store.clone() as Arc<dyn TaskStore>,
            "draw-hidden",
        );
        let mgr = TaskManager::new("draw-hidden", store as Arc<dyn TaskStore>);
        mgr.create(&serde_json::json!({"title": "done"})).await;
        mgr.update(&serde_json::json!({"task_id": "task-1", "new_status": "in_progress"}))
            .await;
        mgr.update(&serde_json::json!({"task_id": "task-1", "new_status": "completed"}))
            .await;
        wait_until(
            || obs.snapshot().tasks.len() == 1,
            500,
            || obs.maybe_refresh(),
        )
        .await;
        obs.hide_completed_after_review();

        let mut footer = Footer::new();
        sync_task_footer(&mut footer, &obs, false);
        assert_eq!(
            footer.task_counts,
            Some((0, 1)),
            "completed hidden boards should still advertise a Ctrl+T-visible task chip"
        );
    }

    #[tokio::test]
    async fn expanded_task_board_falls_back_to_hint_when_terminal_too_short() {
        let store = Arc::new(InMemoryTaskStore::new());
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new(
            store.clone() as Arc<dyn TaskStore>,
            "draw-short",
        );
        let mgr = TaskManager::new("draw-short", store as Arc<dyn TaskStore>);
        mgr.create(&serde_json::json!({"title": "fit me somewhere"}))
            .await;
        wait_until(
            || obs.snapshot().tasks.len() == 1,
            500,
            || obs.maybe_refresh(),
        )
        .await;
        // Let the create notification settle, then reconcile once so this
        // test measures the confirmed short-terminal layout rather than the
        // intentionally visible Refreshing state.
        tokio::time::sleep(Duration::from_millis(20)).await;
        obs.maybe_refresh();
        wait_for_truth(
            &obs,
            crate::tui::task_board_observer::TaskBoardTruthState::Confirmed,
        )
        .await;

        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&obs),
            true,
            None,
            80,
            10,
        );
        // Task-board slot is empty (rows<=10 → render() returns empty) so
        // we fall through to the next-hint on the ActiveView channel.
        assert!(
            frame.task_board.is_none(),
            "short terminal must NOT render board lines"
        );
        match frame.active {
            ActiveView::Status(line) => {
                let text = line
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>();
                assert!(
                    text.contains("Focus") && text.contains("fit me somewhere"),
                    "short expanded board should fall back to a useful next hint, got: {text}"
                );
            }
            other => panic!(
                "short expanded board should not render blank; got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[tokio::test]
    async fn active_cell_and_task_board_coexist_in_frame() {
        // Regression for the flicker bug: when the model is streaming a
        // tool cell AND an expanded board has rows, both must appear
        // in the ViewportFrame — the board is no longer on the
        // priority chain that `active` lives on.
        use crate::tui::chat_widget::WireEvent;
        let store = Arc::new(InMemoryTaskStore::new());
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new(
            store.clone() as Arc<dyn TaskStore>,
            "draw-coexist",
        );
        let mgr = TaskManager::new("draw-coexist", store as Arc<dyn TaskStore>);
        mgr.create(&serde_json::json!({"title": "pending task"}))
            .await;
        wait_until(
            || obs.snapshot().tasks.len() == 1,
            500,
            || obs.maybe_refresh(),
        )
        .await;

        // Parent-level streaming tool cell (what used to steal the slot).
        let mut widget = chat_widget::ChatWidget::new(String::new());
        widget.handle_event(chat_widget::AppEvent::wire(WireEvent::ToolStarted {
            name: "bash".into(),
            description: "ls /tmp".into(),
            tool_use_id: "tu_coexist".into(),
            parent_tool_use_id: None,
        }));

        let frame = active_viewport(
            &widget,
            &status_indicator::StatusIndicator::new(),
            Some(&obs),
            true,
            None,
            80,
            40,
        );

        assert!(
            matches!(frame.active, ActiveView::Active { .. }),
            "streaming tool must occupy the active slot"
        );
        assert!(
            frame.task_board.is_some(),
            "task board lines MUST coexist with streaming active cell — this was the flicker root cause"
        );
    }

    #[test]
    fn completed_logical_agents_linger_briefly_even_when_duration_exceeds_local_elapsed() {
        use crate::tui::chat_widget::WireEvent;

        let mut widget = chat_widget::ChatWidget::new(String::new());
        widget.handle_event(chat_widget::AppEvent::wire(
            WireEvent::AgentControlCompleted {
                action: "get_result".into(),
                label: "reviewer".into(),
                status: "completed".into(),
                duration_ms: 50,
                output: Some(
                    serde_json::json!({
                        "agent_id": "reviewer@abc",
                        "status": "completed",
                        "result": "done"
                    })
                    .to_string(),
                ),
                tool_use_id: "tu_get_result".into(),
                agent_id: Some("reviewer@abc".into()),
            },
        ));

        let frame = active_viewport(
            &widget,
            &status_indicator::StatusIndicator::new(),
            None,
            false,
            None,
            80,
            24,
        );

        assert!(
            frame.multi_agent.is_some(),
            "freshly completed logical agents should linger even when child duration exceeds the local registry elapsed time"
        );
    }

    #[test]
    fn cancelled_only_agent_strip_dismisses_after_linger_but_drilldown_remains() {
        use crate::tui::agent_run_projection::AgentRunStatus;
        use crate::tui::chat_widget::WireEvent;
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut widget = chat_widget::ChatWidget::new(String::new());
        widget.handle_event(chat_widget::AppEvent::wire(WireEvent::AgentLive(
            AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "reviewer@cancelled".into(),
                kind: AgentLiveEventKind::AgentTerminated {
                    termination: AgentLiveTermination::Cancelled,
                    duration_ms: 500,
                    reason: Some("user cancelled".into()),
                },
            },
        )));
        widget.set_agent_completed_at_for_test(
            "reviewer@cancelled",
            std::time::Instant::now() - std::time::Duration::from_secs(6),
        );

        let frame = active_viewport(
            &widget,
            &status_indicator::StatusIndicator::new(),
            None,
            false,
            None,
            80,
            24,
        );

        assert!(
            frame.multi_agent.is_none(),
            "cancelled-only agent strip must dismiss after the same short linger as completed-only"
        );
        let rows = widget.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Cancelled);
    }
}

#[cfg(test)]
mod multi_agent_strip_tests {
    //! Each row fits in one line and surfaces label, typed activity, and
    //! elapsed time, plus a status icon that distinguishes live
    //! agents from completed ones.
    use super::{
        LiveFramedCell, MultiAgentEntry, compact_agent_activity, format_short_elapsed,
        multi_agent_strip_header, render_primary_workspace, should_show_multi_agent_strip,
        truncate_label, workspace_agent_activity_line,
    };
    use crate::tui::agent_run_projection::{AgentRunState, AgentRunStatus};

    fn entry(live: bool, failed: bool) -> MultiAgentEntry {
        let status = if live {
            AgentRunStatus::Running
        } else if failed {
            AgentRunStatus::Failed
        } else {
            AgentRunStatus::Completed
        };
        MultiAgentEntry {
            agent_id: "a".into(),
            label: "agent".into(),
            activity: crate::tui::agent_run_projection::AgentActivityCounts::default(),
            elapsed_ms: 0,
            state: AgentRunState::observed(status),
        }
    }

    fn cancelling_entry() -> MultiAgentEntry {
        let mut e = entry(false, false);
        e.state = AgentRunState::local_intent(AgentRunStatus::Cancelling);
        e
    }

    fn cancelled_entry() -> MultiAgentEntry {
        let mut e = entry(false, false);
        e.state = AgentRunState::observed(AgentRunStatus::Cancelled);
        e
    }

    fn paused_entry() -> MultiAgentEntry {
        let mut e = entry(true, false);
        e.state = AgentRunState::confirmed_server(AgentRunStatus::Paused);
        e
    }

    fn unconfirmed_entry() -> MultiAgentEntry {
        let mut e = entry(true, false);
        e.state = AgentRunState::unconfirmed(AgentRunStatus::Running);
        e
    }

    #[test]
    fn header_only_shows_total_when_all_live() {
        let cells = vec![entry(true, false), entry(true, false)];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("2 parallel agents"));
        assert!(header.contains("2 live"));
        assert!(!header.contains("failed"));
        assert!(!header.contains("done"));
        assert!(header.contains("Ctrl+G manage"));
        assert!(!header.contains("X stop"));
    }

    #[test]
    fn header_surfaces_failed_count_when_any_failure() {
        // Regression: pre-fix the user only saw "▶ 3 parallel agents"
        // even though one was already dead. The breakdown must call out
        // failures so the user catches them at a glance.
        let cells = vec![entry(true, false), entry(false, true), entry(false, false)];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("3 parallel agents"));
        assert!(header.contains("1 live"));
        assert!(header.contains("1 failed"));
        assert!(header.contains("1 done"));
    }

    #[test]
    fn header_skips_zero_buckets() {
        let cells = vec![entry(false, false), entry(false, false)];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("2 parallel agents"));
        assert!(header.contains("2 done"));
        assert!(!header.contains("0 live"));
        assert!(!header.contains("0 failed"));
    }

    #[test]
    fn header_distinguishes_cancelled_from_failed() {
        // The user's complaint: cancelled rows looked identical to
        // failed rows. Splitting the buckets means a user-cancelled
        // agent shows in its own column and the user knows immediately
        // that nothing went wrong — they triggered it.
        let cells = vec![
            entry(false, true),  // 1 failed
            cancelled_entry(),   // 1 cancelled
            entry(false, false), // 1 done
        ];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("1 failed"));
        assert!(header.contains("1 cancelled"));
        assert!(header.contains("1 done"));
        assert!(!header.contains("2 failed"));
    }

    #[test]
    fn header_surfaces_cancelling_as_in_flight_status() {
        // While the kill is in flight the header reports "cancelling"
        // — distinct from cancelled — so the user sees the request is
        // being processed.
        let cells = vec![entry(true, false), cancelling_entry()];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("1 live"));
        assert!(header.contains("1 cancelling"));
        assert!(!header.contains("done"));
    }

    #[test]
    fn header_distinguishes_paused_from_waiting() {
        let mut waiting = entry(true, false);
        waiting.state = AgentRunState::confirmed_server(AgentRunStatus::Waiting);
        let header = multi_agent_strip_header(&[waiting, paused_entry()]);

        assert!(header.contains("1 waiting"), "{header}");
        assert!(header.contains("1 paused"), "{header}");
    }

    #[test]
    fn cancelled_only_strip_lingers_then_dismisses() {
        let cells = vec![cancelled_entry(), cancelled_entry()];
        assert!(
            should_show_multi_agent_strip(&cells, true),
            "freshly cancelled rows should linger long enough to confirm the kill landed"
        );
        assert!(
            !should_show_multi_agent_strip(&cells, false),
            "cancelled-only rows must dismiss after the same linger window as completed rows"
        );
    }

    #[test]
    fn failed_and_cancelling_rows_keep_strip_visible() {
        assert!(should_show_multi_agent_strip(&[entry(false, true)], false));
        assert!(should_show_multi_agent_strip(&[cancelling_entry()], false));
    }

    #[test]
    fn unconfirmed_rows_are_explicit_and_keep_strip_visible() {
        let cells = vec![unconfirmed_entry()];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("1 unconfirmed"), "{header}");
        assert!(!header.contains("1 live"), "{header}");
        assert!(should_show_multi_agent_strip(&cells, false));
    }

    #[test]
    fn header_routes_management_through_the_workbench() {
        let cells = vec![entry(true, false)];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("Ctrl+G manage"), "{header}");
        assert!(!header.contains("X stop"), "{header}");
    }

    #[test]
    fn workspace_activity_retains_live_agent_status_and_management_route() {
        let line = workspace_agent_activity_line(&[entry(true, false)], 100);
        let text: String = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();

        assert!(text.contains("1 parallel agent"), "{text}");
        assert!(text.contains("1 live"), "{text}");
        assert!(text.contains("Ctrl+G manage"), "{text}");
    }

    #[test]
    fn primary_transcript_workspace_renders_live_agent_activity_above_the_conversation() {
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::bottom_pane::transcript_view::{TranscriptSnapshot, TranscriptView};
        use crate::tui::testing::render::buffer_to_string;
        use ratatui::{buffer::Buffer, layout::Rect};

        let mut pane = BottomPane::new();
        pane.push_view(Box::new(
            TranscriptView::from_snapshot(TranscriptSnapshot::default(), 20, 80)
                .with_title("Main conversation · Transcript"),
        ));
        let activity = workspace_agent_activity_line(&[entry(true, false)], 80);
        let area = Rect::new(0, 0, 80, 20);
        let mut buffer = Buffer::empty(area);

        render_primary_workspace(area, &mut buffer, &mut pane, Some(&activity));
        let text = buffer_to_string(&buffer);

        assert!(text.contains("1 parallel agent"), "{text}");
        assert!(text.contains("Ctrl+G manage"), "{text}");
        assert!(text.contains("Main conversation · Transcript"), "{text}");
    }

    #[test]
    fn primary_workspace_makes_retained_agent_transcripts_visible_as_tabs() {
        use crate::tui::agent_run_projection::AgentTranscriptTarget;
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptView;
        use crate::tui::bottom_pane::transcript_view::{TranscriptSnapshot, TranscriptView};
        use crate::tui::testing::render::buffer_to_string;
        use ratatui::{buffer::Buffer, layout::Rect};

        let mut pane = BottomPane::new();
        pane.push_view(Box::new(TranscriptView::from_snapshot(
            TranscriptSnapshot::default(),
            20,
            100,
        )));
        pane.push_view(Box::new(AgentTranscriptView::live_unbound(
            "agent-reviewer".into(),
            "Reviewer".into(),
            "run-reviewer".into(),
            Some(AgentTranscriptTarget::LocalJournal),
            "agents",
            100,
            20,
        )));
        let area = Rect::new(0, 0, 100, 20);
        let mut buffer = Buffer::empty(area);

        render_primary_workspace(area, &mut buffer, &mut pane, None);
        let text = buffer_to_string(&buffer);

        assert!(text.contains("Main conversation"), "{text}");
        assert!(text.contains("Reviewer"), "{text}");
        assert!(text.contains("Shift+←/→ switch"), "{text}");
    }

    #[test]
    fn context_inspector_uses_the_full_primary_workspace() {
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::bottom_pane::context_panel_view::ContextPanelView;
        use crate::tui::context_panel::ContextBreakdown;
        use crate::tui::testing::render::buffer_to_string;
        use ratatui::{buffer::Buffer, layout::Rect};

        let mut pane = BottomPane::new();
        pane.push_view(Box::new(ContextPanelView::new(ContextBreakdown::empty())));
        assert!(pane.primary_workspace_is_open());

        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        render_primary_workspace(area, &mut buffer, &mut pane, None);
        let text = buffer_to_string(&buffer);

        assert!(text.contains("Context"), "{text}");
        assert!(text.contains("Tab focus"), "{text}");
    }

    #[test]
    fn evidence_report_uses_the_full_primary_workspace_only_when_requested() {
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::bottom_pane::info_view::InfoView;
        use crate::tui::testing::render::buffer_to_string;
        use ratatui::{buffer::Buffer, layout::Rect};

        let mut pane = BottomPane::new();
        let evidence = (0..20)
            .map(|index| format!("State evidence {index}"))
            .collect();
        pane.push_view(Box::new(
            InfoView::from_plain("Runtime Inspector", evidence).with_primary_workspace(),
        ));
        assert!(pane.primary_workspace_is_open());

        let area = Rect::new(0, 0, 100, 30);
        let mut buffer = Buffer::empty(area);
        render_primary_workspace(area, &mut buffer, &mut pane, None);
        let text = buffer_to_string(&buffer);

        assert!(text.contains("Runtime Inspector"), "{text}");
        assert!(text.contains("State evidence 19"), "{text}");
    }

    #[test]
    fn activity_gutter_uses_semantic_live_and_settled_colors() {
        use crate::tui::render::renderable::Renderable;
        use ratatui::{buffer::Buffer, layout::Rect, text::Line};

        let area = Rect::new(0, 0, 20, 2);
        let mut live_buffer = Buffer::empty(area);
        LiveFramedCell {
            lines: vec![Line::from("working")],
            live: true,
        }
        .render(area, &mut live_buffer);
        assert_eq!(live_buffer[(0, 0)].fg, crate::tui::theme::current().gutter);

        let mut settled_buffer = Buffer::empty(area);
        LiveFramedCell {
            lines: vec![Line::from("done")],
            live: false,
        }
        .render(area, &mut settled_buffer);
        assert_eq!(
            settled_buffer[(0, 0)].fg,
            crate::tui::theme::current().gutter_frozen
        );
    }

    #[test]
    fn active_edit_diff_paints_the_entire_available_row_surface() {
        use crate::tui::history_cell::{
            HistoryCell,
            tool::{ToolCell, ToolStatus},
        };
        use crate::tui::render::renderable::Renderable;
        use ratatui::{buffer::Buffer, layout::Rect};

        let area = Rect::new(0, 0, 82, 8);
        let mut edit = ToolCell::new_running("str_replace", "src/main.rs");
        edit.status = ToolStatus::Success;
        edit.duration_ms = Some(12);
        edit.output_summary = Some("@@ -1 +1 @@\n-fn old_name() {}\n+fn new_name() {}".to_string());

        let lines = edit.display_lines(area.width.saturating_sub(2));
        let mut buffer = Buffer::empty(area);
        LiveFramedCell { lines, live: false }.render(area, &mut buffer);

        let find_row = |needle: &str| {
            (0..area.height)
                .find(|&y| {
                    (0..area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect::<String>()
                        .contains(needle)
                })
                .expect("diff row must be rendered")
        };
        let theme = crate::tui::theme::current();
        for (needle, expected_background) in [
            ("old_name", theme.diff_del_bg),
            ("new_name", theme.diff_add_bg),
        ] {
            let row = find_row(needle);
            for x in area.x + 2..area.right() {
                assert_eq!(
                    buffer[(x, row)].bg,
                    expected_background,
                    "{needle} must retain its diff surface through the right edge at x={x}"
                );
            }
        }
    }

    #[test]
    fn elapsed_under_a_second_renders_in_milliseconds() {
        assert_eq!(format_short_elapsed(0), "0ms");
        assert_eq!(format_short_elapsed(150), "150ms");
        assert_eq!(format_short_elapsed(999), "999ms");
    }

    #[test]
    fn elapsed_seconds_render_compact_no_decimals() {
        // Sub-minute durations stay compact because the strip row has limited width.
        assert_eq!(format_short_elapsed(1_000), "1s");
        assert_eq!(format_short_elapsed(12_500), "12s");
        assert_eq!(format_short_elapsed(59_999), "59s");
    }

    #[test]
    fn elapsed_minutes_drop_zero_seconds() {
        assert_eq!(format_short_elapsed(60_000), "1m");
        assert_eq!(format_short_elapsed(90_000), "1m30s");
        assert_eq!(format_short_elapsed(120_000), "2m");
        assert_eq!(format_short_elapsed(125_000), "2m5s");
    }

    #[test]
    fn activity_copy_distinguishes_tools_children_and_partial_counts() {
        assert_eq!(
            compact_agent_activity(crate::tui::agent_run_projection::AgentActivityCounts {
                tool_calls: 3,
                child_agents: 1,
                messages_sent: 2,
                messages_received: 1,
                child_agents_partial: false,
            }),
            "3 tools · 1 child · ↑2 ↓1"
        );
        assert_eq!(
            compact_agent_activity(crate::tui::agent_run_projection::AgentActivityCounts {
                tool_calls: 0,
                child_agents: 4,
                messages_sent: 0,
                messages_received: 0,
                child_agents_partial: true,
            }),
            "≥4 children"
        );
    }

    /// Labels MUST be char-aware so a CJK label survives truncation
    /// without panicking on a multi-byte boundary.
    #[test]
    fn truncate_label_is_char_aware_for_cjk() {
        let label = "审查代码的正确性和并发安全";
        let truncated = truncate_label(label, 5);
        assert_eq!(
            truncated.chars().count(),
            5,
            "truncated CJK label must report exactly `max` chars"
        );
        assert!(
            truncated.ends_with('…'),
            "truncated label must end with the ellipsis: {truncated}"
        );
    }

    /// Short labels pass through unchanged — no spurious ellipsis.
    #[test]
    fn truncate_label_noop_when_fits() {
        assert_eq!(truncate_label("review_tui", 32), "review_tui");
        assert_eq!(truncate_label("", 10), "");
    }

    /// `max == 0` returns empty (defensive — caller computed a
    /// pathological budget).
    #[test]
    fn truncate_label_zero_budget_returns_empty() {
        assert_eq!(truncate_label("anything", 0), "");
    }
}
