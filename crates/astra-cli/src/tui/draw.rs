//! Frame composition for live content, activity, and the bottom pane.
//!
//! The TUI's draw cycle is: compute which source owns the active-cell
//! region (active cell / next-hint / empty), then paint the independent
//! activity line and bottom pane into the ratatui
//! frame. This file owns that pipeline so the outer event loop in
//! [`super::event_loop`] doesn't carry render details.
//!
//! See `ARCHITECTURE.md` for the visual-hierarchy grammar.

use super::work_board_projection::TaskStoreHealth;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::agent_run_projection::{AgentProjectionConfidence, AgentRunState, AgentRunStatus};
use super::bottom_pane::{BottomPane, ConversationTab, footer::Footer};
use super::history_cell::assistant::AssistantCell;
use super::render::line_utils::{history_cell_lines_for_buffer, sanitize_lines_for_buffer};
use super::render::renderable::{FlexRenderable, Renderable, RenderableItem};
use super::task_board_observer::{TaskBoardObserver, TaskBoardProjection, TaskBoardTruthState};
use super::terminal::TerminalGuard;
use super::{chat_widget, status_indicator, task_list};
#[cfg(test)]
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
///   left `▏` gutter whose semantic colour distinguishes live and
///   settled work without treating animation as progress evidence.
/// - **Hint** — an idle next-task cue when no live content is present.
pub(crate) enum ActiveView {
    Empty,
    Hint(Line<'static>),
    Active {
        lines: Vec<Line<'static>>,
        /// A parked tool is secondary to the live reply in a short viewport.
        parked: Vec<Line<'static>>,
        /// `true` while still streaming. `false` once finalised.
        live: bool,
        /// Streaming replies show their latest lines when space is limited.
        tail: bool,
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
/// Both canonical routes stay discoverable: Ctrl+G opens retained agent
/// conversations, while Shift+Down opens the typed background-task surface.
/// Individual rows belong on those surfaces, not in a second list that
/// competes with the conversation.
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

    let noun = if total == 1 { "agent" } else { "agents" };
    format!(
        "{} {total} {noun}{breakdown} · {} · {}",
        crate::tui::glyphs::current().agent_fanout,
        crate::tui::background_shortcut::agent_workbench_open_hint(),
        crate::tui::background_shortcut::background_task_open_hint(),
    )
}

/// The workbench keeps this one-line activity strip visible above a focused
/// root or agent transcript. It is intentionally a navigation/status cue,
/// not a second agent list: the background-work surface remains the place to
/// inspect and control individual runs.
fn workspace_agent_activity_line(cells: &[MultiAgentEntry], width: u16) -> Line<'static> {
    let text = super::history_cell::truncate_by_width(
        &format!("  {}", multi_agent_strip_header(cells)),
        usize::from(width),
    );
    Line::from(Span::styled(
        text,
        Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(ratatui::style::Modifier::BOLD),
    ))
}

/// The focused child must show its own run, even when a different agent is
/// live or the aggregate strip has already expired.
fn focused_agent_activity_line(
    widget: &chat_widget::ChatWidget,
    agent_id: &str,
    run_id: &str,
    width: u16,
) -> Line<'static> {
    use super::bottom_pane::background_task_view::types::format_elapsed;
    use super::bottom_pane::in_flight_agents_view::{state_color, state_phrase};
    let Some((cell, state, attention)) = widget.agent_run_for_workspace(agent_id, run_id) else {
        return Line::styled(
            super::history_cell::truncate_by_width(
                "  Agent status unconfirmed",
                usize::from(width),
            ),
            Style::default().fg(crate::tui::theme::current().dim),
        );
    };
    let theme = crate::tui::theme::current();
    let dim = Style::default().fg(theme.dim);
    let mut parts = vec![
        Span::styled("  ", dim),
        Span::styled(
            state_phrase(state).unwrap_or("working"),
            Style::default().fg(state_color(state)).bold(),
        ),
    ];
    let reliable = !matches!(
        state.confidence,
        AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
    );
    let elapsed = if state.status.is_terminal() {
        cell.duration_ms
    } else if reliable {
        Some(cell.started_at.elapsed().as_millis() as u64)
    } else {
        None
    };
    if let Some(elapsed) = elapsed {
        parts.push(Span::styled(format!(" · {}", format_elapsed(elapsed)), dim));
    }
    if let Some(attention) = attention {
        let attention = crate::tui::render::line_utils::sanitize_terminal_text(attention);
        let available =
            usize::from(width).saturating_sub(parts.iter().map(Span::width).sum::<usize>() + 3);
        if available > 0 {
            let brief = super::history_cell::truncate_by_width(&attention, available);
            parts.push(Span::styled(
                format!(" · {brief}"),
                Style::default().fg(theme.warn),
            ));
        }
    }
    Line::from(parts)
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
        .fg(theme.selected_fg)
        .bg(theme.selected_bg)
        .add_modifier(ratatui::style::Modifier::BOLD);
    let label_budget = usize::from(width);
    let labels = tabs
        .iter()
        .map(|tab| super::history_cell::truncate_by_width(&tab.label, label_budget))
        .collect::<Vec<_>>();
    let shortcut = " · Shift+←/→ switch";
    let full_width = 2
        + labels
            .iter()
            .map(|label| UnicodeWidthStr::width(label.as_str()) + 2)
            .sum::<usize>()
        + tabs.len().saturating_sub(1) * 2;

    if full_width <= usize::from(width) {
        let mut spans = vec![Span::styled("  ", dim)];
        for (index, (tab, label)) in tabs.iter().zip(labels).enumerate() {
            if index > 0 {
                spans.push(Span::raw("  "));
            }
            spans.push(Span::styled(
                format!(" {label} "),
                if tab.active { active_style } else { dim },
            ));
        }
        if full_width + shortcut.width() <= usize::from(width) {
            spans.push(Span::styled(shortcut, dim));
        }
        return Some(Line::from(spans));
    }

    let active_label = tabs
        .iter()
        .zip(labels)
        .find_map(|(tab, label)| tab.active.then_some(label))
        .unwrap_or_else(|| "conversation".to_string());
    let prefix = if width >= 16 { "  " } else { "" };
    let label = super::history_cell::truncate_by_width(
        &active_label,
        usize::from(width).saturating_sub(prefix.width()),
    );
    let mut spans = vec![Span::raw(prefix), Span::styled(label.clone(), active_style)];
    let suffix = format!(" · {} conversations{shortcut}", tabs.len());
    if prefix.width() + label.width() + suffix.width() <= usize::from(width) {
        spans.push(Span::styled(suffix, dim));
    }
    Some(Line::from(spans))
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
    let reserved = if bottom_pane.conversation_tab_is_open() {
        bottom_pane.conversation_workspace_min_height()
    } else {
        4
    };
    chrome.truncate(usize::from(area.height.saturating_sub(reserved)));
    let workspace_area = if !chrome.is_empty() {
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
    bottom_pane.prepare_conversation_workspace(workspace_area.height, workspace_area.width);
    bottom_pane.render(workspace_area, buf);
    bottom_pane.cursor_position(workspace_area)
}

/// One frame's presentation facts. Activity and task details have their own
/// slots so streaming content cannot replace the turn's elapsed time.
pub(crate) struct ViewportFrame {
    pub active: ActiveView,
    pub status: Option<super::status_indicator::StatusIndicator>,
    /// Parallel agents to render ABOVE the active cell. Rendered as
    /// a stack of `LiveFramedCell`s with a "▶ N parallel agents"
    /// header. Co-exists with `active` (which usually holds an
    /// AssistantCell/ReasoningCell during the parent turn) — they
    /// occupy independent strip rows so neither displaces the
    /// other. `None` when no parallel agents are live.
    pub multi_agent: Option<Vec<MultiAgentEntry>>,
    /// Pre-rendered task board. `None` when the board should not
    /// draw (terminal-history only, explicitly hidden, empty, etc.).
    pub task_board: Option<Vec<Line<'static>>>,
    /// Compact summary for height-constrained frames, without changing the
    /// user's expanded/collapsed preference.
    pub task_board_compact: Option<Vec<Line<'static>>>,
    /// The resolved board visibility state after evaluating the
    /// snapshot. Callers MUST write this back to their local
    /// `board_expanded` so that in-turn draws self-correct when
    /// tasks appear mid-turn (rather than waiting for the outer tick).
    pub resolved_board_expanded: bool,
}

fn task_board_truth_line(
    state: TaskBoardTruthState,
    _store_health: TaskStoreHealth,
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
        TaskBoardTruthState::Loading => ("Work · syncing", theme.dim),
        TaskBoardTruthState::Confirmed => return None,
        // A normal background refresh does not change the last confirmed
        // truth and must not add a transient row. Adding/removing this line on
        // every poll changed the viewport height and produced a blue flash.
        TaskBoardTruthState::Refreshing => return None,
        TaskBoardTruthState::Stale => (
            "Work state delayed · showing the last confirmed graph · Ctrl+T → R refresh",
            theme.warn,
        ),
        TaskBoardTruthState::Unavailable => {
            ("Work state unavailable · Ctrl+T → R refresh", theme.warn)
        }
    };
    Some(Line::from(Span::styled(
        truncate_state_line(text, width as usize),
        Style::default().fg(color),
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
    _board_expanded: bool,
    board_user_pin: Option<bool>,
    width: u16,
    rows: u16,
) -> ViewportFrame {
    // Reserve 2 cols for the `▏ ` gutter.
    let inner_w = width.saturating_sub(2).max(20);

    // Multi-agent strip: one compact row per logical background
    // agent, not per control-plane spawn/get_result action
    // call. Co-exists with `active_cell` (assistant streaming WHILE
    // sub-agents run shows both).
    //
    // Visibility rule: show the strip while ANY agent is still live
    // (running/cancelling). Completed-only and cancelled-only strips
    // linger for `STRIP_LINGER` so the user can glance at the final
    // state, then dismiss. Every confirmed terminal state follows the same
    // rule: failures and interruptions remain available in history and via
    // Ctrl+G, but must not permanently occupy the live viewport.
    const STRIP_LINGER: std::time::Duration = std::time::Duration::from_secs(5);
    let _ = inner_w; // retained for future per-row layout decisions
    let agent_ids = chat_widget.agent_status_strip_ids();
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
        // A terminal-only strip is a brief within-turn transition cue, not
        // durable workspace chrome. Once the parent turn settles, retained
        // conversations remain available through Ctrl+G/Shift+Down but the
        // active viewport must return to idle immediately. This also avoids a
        // timer-only row becoming stuck when no event schedules a frame after
        // the linger deadline.
        let any_recently_terminal = status.turn_is_open()
            && !any_live
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
        let mut tail = cell.as_any_ref().is::<AssistantCell>() && cell.is_live();
        let mut lines = if let Some(assistant) = cell.as_any_ref().downcast_ref::<AssistantCell>()
            && cell.is_live()
        {
            sanitize_lines_for_buffer(
                assistant.live_reply_lines(width.saturating_sub(2).max(1), live_assistant_rows),
            )
        } else {
            history_cell_lines_for_buffer(cell, inner_w)
        };
        let mut parked = if let Some(tool) = chat_widget.latest_parked_tool() {
            let mut tool_lines = history_cell_lines_for_buffer(tool, inner_w);
            // Keep the reply visible even in a short viewport; the compact
            // tool cue remains below it while the invocation is still live.
            tool_lines.truncate(2);
            tool_lines
        } else {
            Vec::new()
        };
        if lines.is_empty() {
            std::mem::swap(&mut lines, &mut parked);
            tail = false;
        }
        if lines.is_empty() {
            None
        } else {
            Some(ActiveView::Active {
                lines,
                parked,
                live: cell.is_live(),
                tail,
            })
        }
    });
    // Rows and confidence are cloned under one observer lock. Reading them
    // separately could pair a pre-refresh cache with post-refresh confidence.
    let projection = board.map(TaskBoardObserver::active_projection);
    let truth_state = projection.as_ref().map(TaskBoardProjection::truth_state);
    let has_tasks = projection
        .as_ref()
        .is_some_and(TaskBoardProjection::has_tasks);
    let has_open_work = projection
        .as_ref()
        .is_some_and(TaskBoardProjection::has_open_work);
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
    // stale window state from turn start. Now every draw self-corrects — if
    // a Work task lands mid-turn, the board opens
    // on the very next frame (≤50ms).
    let (resolved_expanded, _reset_pin) =
        super::board_pin::resolve_board_visibility(board_user_pin, has_tasks, has_open_work);

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
    let mut state_lines = Vec::with_capacity(1);
    // Keep an unrequested, empty Work lane quiet during startup. Once the user
    // opens it or real rows exist, surface its exact observation state.
    let work_lane_is_relevant = has_open_work || resolved_expanded || board_user_pin == Some(true);
    if work_lane_is_relevant
        && let Some(line) =
            truth_state.and_then(|state| task_board_truth_line(state, store_health, width))
    {
        state_lines.push(line);
    }
    let row_budget = rows.saturating_sub(state_lines.len() as u16);
    let mut cached_lines = if has_tasks && !hidden {
        match projection.as_ref() {
            Some(TaskBoardProjection::Single { snapshot, .. }) if resolved_expanded => {
                // Status/title changes are already visible in the canonical
                // row. Do not add a transient accent glyph: it makes ordinary
                // observer reconciliation look like unstable content.
                task_list::render(&snapshot.tasks, width, row_budget, true)
            }
            Some(TaskBoardProjection::Single { snapshot, .. }) if has_open_work => {
                task_list::render_collapsed_active_summary(&snapshot.tasks, width)
                    .into_iter()
                    .collect()
            }
            Some(_) | None => Vec::new(),
        }
    } else {
        Vec::new()
    };
    let task_board_compact = match projection.as_ref() {
        Some(TaskBoardProjection::Single { snapshot, .. })
            if has_open_work && !hidden && resolved_expanded =>
        {
            let mut lines = state_lines.clone();
            lines.extend(task_list::render_collapsed_active_summary(
                &snapshot.tasks,
                width,
            ));
            (!lines.is_empty()).then_some(lines)
        }
        _ => None,
    };
    let task_board = if state_lines.is_empty() && cached_lines.is_empty() {
        None
    } else {
        let mut lines = Vec::with_capacity(cached_lines.len() + state_lines.len());
        lines.append(&mut state_lines);
        lines.append(&mut cached_lines);
        Some(lines)
    };
    let status_line = status.is_visible().then(|| status.clone());
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
    let active = pick_active_view(active, next_hint.filter(|_| status_line.is_none()));
    ViewportFrame {
        active,
        status: status_line,
        multi_agent: multi_agent_active,
        task_board,
        task_board_compact,
        resolved_board_expanded: resolved_expanded,
    }
}

fn should_show_multi_agent_strip(cells: &[MultiAgentEntry], any_recently_terminal: bool) -> bool {
    let any_live = cells.iter().any(|entry| entry.state.is_actionable_active());
    let any_uncertain = cells.iter().any(|entry| {
        matches!(
            entry.state.confidence,
            AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
        )
    });
    any_live || any_uncertain || any_recently_terminal
}

/// Live content takes precedence over the optional idle next-task hint.
pub(crate) fn pick_active_view(
    active: Option<ActiveView>,
    next_hint: Option<Line<'static>>,
) -> ActiveView {
    active.unwrap_or_else(|| next_hint.map_or(ActiveView::Empty, ActiveView::Hint))
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

/// Paint one frame without mixing the root status into a focused workspace.
pub(crate) fn do_draw(
    guard: &mut TerminalGuard,
    viewport: ViewportFrame,
    chat_widget: &chat_widget::ChatWidget,
    bottom_pane: &mut BottomPane,
    task_board: Option<(&TaskBoardObserver, bool)>,
) -> Result<(), String> {
    guard
        .ensure_tui_modes()
        .map_err(|e| format!("failed to restore terminal input mode: {e}"))?;
    if let Some((board, expanded)) = task_board {
        sync_task_footer(&mut bottom_pane.footer, board, expanded);
    }
    bottom_pane.pre_draw_session_tick(std::time::Instant::now(), chat_widget.history());

    let terminal_size = guard.terminal.size().unwrap_or(ratatui::layout::Size {
        width: 80,
        height: 24,
    });
    let width = terminal_size.width;
    let workspace_agent_activity = match bottom_pane.active_conversation_tab_id() {
        Some(super::bottom_pane::view::ConversationTabId::Run { agent_id, run_id }) => Some(
            focused_agent_activity_line(chat_widget, &agent_id, &run_id, width),
        ),
        _ if bottom_pane.agent_monitor_is_open() => None,
        _ => viewport
            .multi_agent
            .as_deref()
            .map(|cells| workspace_agent_activity_line(cells, width)),
    };

    // Root and delegated conversations are peers. Once one is focused, it
    // replaces the root chat canvas rather than rendering as another pane
    // below it. The run navigator remains reachable through Ctrl+G and
    // Left/Esc returns to it, giving this surface browser-tab semantics.
    if bottom_pane.primary_workspace_is_open() {
        return guard
            .draw_workspace(|frame| {
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

    let flex = compact_view(viewport, bottom_pane, width, terminal_size.height);

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

/// Allocate the focused bottom pane first, then the turn status and at least
/// one live-content row. Auxiliary surfaces use only the remaining space.
fn compact_view<'a>(
    viewport: ViewportFrame,
    bottom_pane: &'a mut BottomPane,
    width: u16,
    height: u16,
) -> FlexRenderable<'a> {
    let ViewportFrame {
        active,
        status,
        multi_agent,
        task_board,
        task_board_compact,
        ..
    } = viewport;
    // The composer frame is optional chrome. Reserve the rows needed by live
    // content before allowing it to consume an extra row on a short terminal.
    let required_rows = u16::from(status.is_some() && !bottom_pane.has_active_view())
        .saturating_add(u16::from(!matches!(active, ActiveView::Empty)))
        .saturating_add(
            task_board
                .as_ref()
                .map(Vec::len)
                .or_else(|| task_board_compact.as_ref().map(Vec::len))
                .unwrap_or(0) as u16,
        )
        .saturating_add(u16::from(multi_agent.is_some()));
    let extra_picker_rows = bottom_pane.extra_picker_rows();
    let optional_composer_rows =
        extra_picker_rows + u16::from(bottom_pane.has_composer_hints(width));
    bottom_pane.set_composer_frame_allowed(true);
    let framed_bottom_h = bottom_pane
        .desired_height(width)
        .saturating_sub(optional_composer_rows);
    // Keep a useful slice of the candidate list ahead of the optional frame.
    let minimum_picker_rows = extra_picker_rows.saturating_sub(1).min(2);
    bottom_pane.set_composer_frame_allowed(
        height
            >= framed_bottom_h
                .saturating_add(required_rows)
                .saturating_add(minimum_picker_rows),
    );

    let desired = bottom_pane.desired_height(width);
    let bottom_h = height
        .saturating_sub(required_rows)
        .clamp(desired.saturating_sub(optional_composer_rows), desired);
    let available = height.saturating_sub(bottom_h);
    let margin = super::style::conversation_margin(width);
    let status = status
        .filter(|_| available > 0 && !bottom_pane.has_active_view())
        .and_then(|indicator| {
            let mut line = indicator.render_with_context(
                width - margin * 2,
                bottom_pane.has_pending_approvals(),
                bottom_pane.can_interrupt_turn(),
            )?;
            if margin > 0 {
                line.spans.insert(0, Span::raw("  "));
            }
            Some(line)
        });
    let status_rows = u16::from(status.is_some());
    let available = available.saturating_sub(status_rows);
    let active_min = u16::from(available > 0 && !matches!(active, ActiveView::Empty));
    let mut auxiliary = available.saturating_sub(active_min);

    let multi_agent = multi_agent.filter(|_| {
        if auxiliary == 0 {
            false
        } else {
            auxiliary -= 1;
            true
        }
    });
    let fits =
        |lines: &Vec<Line<'static>>| !lines.is_empty() && lines.len() <= usize::from(auxiliary);
    let task_board = task_board.filter(&fits).or_else(|| {
        if matches!(active, ActiveView::Hint(_)) {
            None
        } else {
            task_board_compact.filter(fits)
        }
    });
    if let Some(lines) = &task_board {
        auxiliary -= lines.len() as u16;
    }
    let spacer = u16::from((status.is_some() || task_board.is_some()) && auxiliary > 0);
    auxiliary -= spacer;
    let active_rows = usize::from(active_min + auxiliary);
    let active: RenderableItem<'_> = match active {
        ActiveView::Empty => RenderableItem::Owned(Box::new(())),
        ActiveView::Hint(line) => RenderableItem::Owned(Box::new(line)),
        ActiveView::Active {
            lines,
            parked,
            live,
            tail,
        } => {
            let lines = fit_live_lines(lines, parked, tail, active_rows, width.saturating_sub(2));
            RenderableItem::Owned(Box::new(LiveFramedCell { lines, live }))
        }
    };

    let mut flex = FlexRenderable::new();
    flex.push(1, active);
    if let Some(cells) = multi_agent {
        let line = Line::from(Span::styled(
            multi_agent_strip_header(&cells),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));
        flex.push(
            0,
            RenderableItem::Owned(Box::new(LiveFramedCell {
                lines: vec![line],
                live: true,
            })),
        );
    }
    if let Some(lines) = task_board {
        flex.push(
            0,
            RenderableItem::Owned(Box::new(Paragraph::new(ratatui::text::Text::from(lines)))),
        );
    }
    if spacer > 0 {
        flex.push(0, RenderableItem::Owned(Box::new(Line::default())));
    }
    if let Some(status) = status {
        flex.push(0, RenderableItem::Owned(Box::new(status)));
    }
    flex.push(
        0,
        RenderableItem::Owned(Box::new(BottomPaneRenderable {
            pane: bottom_pane,
            height: bottom_h,
        })),
    );
    flex
}

fn fit_live_lines(
    mut lines: Vec<Line<'static>>,
    parked: Vec<Line<'static>>,
    tail: bool,
    limit: usize,
    width: u16,
) -> Vec<Line<'static>> {
    if lines.len() > limit {
        if tail && limit > 0 {
            let keep = limit.saturating_sub(usize::from(limit > 1));
            let mut visible = lines.split_off(lines.len() - keep);
            if limit > 1 {
                let mark = crate::tui::glyphs::current().truncation;
                let notice = format!("{mark} earlier text hidden");
                let notice = if notice.width() <= usize::from(width) {
                    notice
                } else {
                    mark.to_string()
                };
                visible.insert(
                    0,
                    Line::styled(
                        notice,
                        Style::default().fg(crate::tui::theme::current().dim),
                    ),
                );
            }
            lines = visible;
        } else {
            lines.truncate(limit);
        }
    }
    let spare = limit.saturating_sub(lines.len());
    lines.extend(parked.into_iter().take(spare));
    lines
}

struct BottomPaneRenderable<'a> {
    pane: &'a BottomPane,
    height: u16,
}

impl<'a> Renderable for BottomPaneRenderable<'a> {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        self.pane.render(area, buf);
    }
    fn desired_height(&self, _width: u16) -> u16 {
        self.height
    }
    fn cursor_pos(&self, area: ratatui::layout::Rect) -> Option<(u16, u16)> {
        self.pane.cursor_position(area)
    }
}

// ───────────────────────────────────────────────────────────────────────
// LiveFramedCell — quiet gutter for active content
// ───────────────────────────────────────────────────────────────────────

/// Left-gutter renderable: a slim line on the left edge. Its semantic
/// color communicates whether the cell is live or settled; motion is not
/// treated as evidence that useful work is happening.
struct LiveFramedCell {
    lines: Vec<Line<'static>>,
    live: bool,
}

impl super::render::renderable::Renderable for LiveFramedCell {
    fn render(&self, area: ratatui::layout::Rect, buf: &mut ratatui::buffer::Buffer) {
        use ratatui::widgets::Widget;

        // Need at least 3 cols: gutter + space + 1 col of content.
        if area.width < 3 || area.height == 0 {
            return;
        }

        // Inner paragraph area: 2 cols reserved for the gutter and a space.
        let inner = ratatui::layout::Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: area.height,
        };

        let para = crate::tui::render::line_utils::FullRowParagraph::new(self.lines.clone());
        Widget::render(para, inner, buf);

        let theme = crate::tui::theme::current();
        let color = if self.live {
            theme.gutter
        } else {
            theme.gutter_frozen
        };
        for row in 0..area.height as usize {
            set_char(
                buf,
                area.x,
                area.y + row as u16,
                crate::tui::glyphs::current().live_gutter,
                color,
            );
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

// ───────────────────────────────────────────────────────────────────────
// Priority resolver tests
// ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod active_view_priority_tests {
    //! Live content takes precedence over an idle next-task hint.
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
            ActiveView::Hint(_) => "Hint",
            ActiveView::Active { .. } => "Active",
        }
    }
    fn first_text(v: &ActiveView) -> String {
        match v {
            ActiveView::Empty => String::new(),
            ActiveView::Hint(l) => l
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
    fn active_beats_hint() {
        let v = pick_active_view(
            Some(ActiveView::Active {
                lines: lines("active-tool"),
                parked: Vec::new(),
                live: true,
                tail: false,
            }),
            Some(line("next-hint")),
        );
        assert_eq!(discriminate(&v), "Active");
        assert_eq!(first_text(&v), "active-tool");
    }

    #[test]
    fn hint_used_when_alone() {
        let v = pick_active_view(None, Some(line("next-hint")));
        assert_eq!(discriminate(&v), "Hint");
        assert_eq!(first_text(&v), "next-hint");
    }

    #[test]
    fn empty_when_all_none() {
        let v = pick_active_view(None, None);
        assert_eq!(discriminate(&v), "Empty");
    }
}

#[cfg(test)]
mod compact_surface_tests {
    use super::{ActiveView, ViewportFrame, compact_view};
    use crate::tui::bottom_pane::BottomPane;
    use crate::tui::render::renderable::Renderable;
    use crate::tui::status_indicator::{IndicatorState, StatusIndicator};
    use crate::tui::testing::render::buffer_to_string;
    use ratatui::{buffer::Buffer, layout::Rect, text::Line};
    use std::time::Instant;

    fn frame(_width: u16) -> ViewportFrame {
        let now = Instant::now();
        let mut status = StatusIndicator::new();
        status.begin_turn(now);
        status.set_state(IndicatorState::Thinking { started_at: now });
        ViewportFrame {
            active: ActiveView::Active {
                lines: vec![Line::from("old reply"), Line::from("latest reply")],
                parked: vec![Line::from("parked tool")],
                live: true,
                tail: true,
            },
            status: Some(status),
            multi_agent: None,
            task_board: None,
            task_board_compact: None,
            resolved_board_expanded: false,
        }
    }

    #[test]
    fn streaming_reply_and_turn_status_remain_visible_together() {
        for height in [5, 6, 8, 12] {
            let area = Rect::new(0, 0, 40, height);
            let mut pane = BottomPane::new();
            let flex = compact_view(frame(area.width), &mut pane, area.width, area.height);
            let mut buffer = Buffer::empty(area);
            flex.render(area, &mut buffer);
            let text = buffer_to_string(&buffer);
            assert!(text.contains("Working"), "{height}: {text}");
            assert!(text.contains("latest reply"), "{height}: {text}");
            if height == 12 {
                let rows = text.lines().collect::<Vec<_>>();
                let status = rows.iter().position(|row| row.contains("Working")).unwrap();
                assert!(rows[status - 1].trim().is_empty(), "{text}");
                assert!(
                    rows[..status - 1]
                        .iter()
                        .any(|row| row.contains("latest reply"))
                );
            }
            let cursor = flex.cursor_pos(area).expect("composer remains usable");
            assert!(cursor.0 < area.right() && cursor.1 < area.bottom());
        }
    }

    #[test]
    fn working_tips_share_the_input_row_and_yield_to_drafts() {
        use crate::tui::task_status::TaskStatus;
        let mut pane = BottomPane::new();
        let started_at = Instant::now();
        pane.set_task_status(TaskStatus::TurnRunning { started_at });
        pane.enable_tips(started_at);
        let draw = |pane: &mut BottomPane, width, height| {
            let area = Rect::new(0, 0, width, height);
            let mut buf = Buffer::empty(area);
            let flex = compact_view(frame(area.width), pane, width, height);
            let viewport_height = flex.desired_height(width);
            flex.render(area, &mut buf);
            (buffer_to_string(&buf), viewport_height)
        };

        let (first, height_with_tip) = draw(&mut pane, 80, 24);
        assert!(
            first.lines().any(|row| {
                row.contains("Message Astra") && row.contains("Tip  Ctrl+Q queues the next turn")
            }),
            "{first}"
        );

        pane.set_tips_enabled(false);
        let (disabled, height_without_tip) = draw(&mut pane, 80, 24);
        assert!(!disabled.contains("Tip  "), "{disabled}");
        assert_eq!(height_with_tip, height_without_tip);

        pane.enable_tips(started_at);
        assert!(pane.pre_draw_tick(started_at + std::time::Duration::from_secs(24)));
        let (second, _) = draw(&mut pane, 80, 24);
        assert!(
            second.contains("Tip  Ctrl+O opens the live transcript"),
            "{second}"
        );
        let (narrow, _) = draw(&mut pane, 40, 8);
        assert!(!narrow.contains("Tip  "), "{narrow}");
        assert!(
            narrow.contains("Working") && narrow.contains("latest reply"),
            "{narrow}"
        );

        pane.composer.set_text("unsent draft");
        let (editing, _) = draw(&mut pane, 80, 24);
        assert!(
            editing.contains("unsent draft") && !editing.contains("Tip  "),
            "{editing}"
        );
    }

    #[test]
    fn history_suggestions_fit_below_input_without_hiding_live_output() {
        use crate::tui::history_cell::{HistoryCell, user::UserCell};
        use std::sync::Arc;

        let history: Vec<Arc<dyn HistoryCell>> = vec![
            Arc::new(UserCell::new("ship staging")),
            Arc::new(UserCell::new("ship production")),
        ];
        for height in [6, 8] {
            let area = Rect::new(0, 0, 40, height);
            let mut pane = BottomPane::new();
            pane.handle_session_paste("shi", &history);
            let flex = compact_view(frame(area.width), &mut pane, area.width, area.height);
            let mut buffer = Buffer::empty(area);
            flex.render(area, &mut buffer);
            let text = buffer_to_string(&buffer);
            assert!(
                text.contains("Working") && text.contains("latest reply"),
                "{text}"
            );
            let (x, y) = flex.cursor_pos(area).expect("input cursor");
            assert_eq!(buffer[(x - 1, y)].symbol(), "i");
            if height == 8 {
                assert!(
                    text.contains("ship staging") && text.contains("› ship production"),
                    "{text}"
                );
            } else {
                assert!(text.contains("ship production"), "{text}");
                assert!(!text.contains("ship staging"), "{text}");
            }
        }
    }

    #[test]
    fn history_tab_only_accepts_a_match_visible_in_the_last_frame() {
        use crate::tui::history_cell::{HistoryCell, user::UserCell};
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use std::sync::Arc;

        let draw = |pane: &mut BottomPane, height| {
            let area = Rect::new(0, 0, 40, height);
            let mut buffer = Buffer::empty(area);
            compact_view(frame(area.width), pane, area.width, area.height)
                .render(area, &mut buffer);
            buffer_to_string(&buffer)
        };
        for (query, candidate) in [
            (
                "deploy production canary before frame",
                "deploy production canary _before frame",
            ),
            (
                "deploy production canary before fr",
                "deploy production canary _before fr",
            ),
        ] {
            let history: Vec<Arc<dyn HistoryCell>> = vec![Arc::new(UserCell::new(candidate))];
            let mut pane = BottomPane::new();
            pane.handle_session_paste(query, &history);
            let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);
            pane.handle_session_key_at(tab, Instant::now(), &history);
            assert_eq!(pane.composer.text(), query);

            // The unframed composer leaves a row for suggestions at six
            // rows. At five it preserves the input, live reply and status.
            assert!(draw(&mut pane, 6).contains(candidate));
            let short = draw(&mut pane, 5);
            assert!(
                short.contains(query) && short.contains("Working"),
                "{short}"
            );
            assert!(
                !short.contains("_before") && !short.contains(" → …"),
                "{short}"
            );
            pane.handle_session_key_at(tab, Instant::now(), &history);
            assert_eq!(pane.composer.text(), query);

            assert!(draw(&mut pane, 6).contains(candidate));
            pane.handle_session_key_at(tab, Instant::now(), &history);
            assert_eq!(pane.composer.text(), candidate);
        }
    }

    #[test]
    fn tiny_terminal_keeps_input_and_cursor_within_the_canvas() {
        for (width, height) in [(40, 5), (24, 4), (12, 3), (1, 1), (0, 0)] {
            let area = Rect::new(0, 0, width, height);
            let mut pane = BottomPane::new();
            let flex = compact_view(frame(width), &mut pane, width, height);
            let mut buffer = Buffer::empty(area);
            flex.render(area, &mut buffer);
            if let Some((x, y)) = flex.cursor_pos(area) {
                assert!(
                    x < area.right() && y < area.bottom(),
                    "{width}x{height}: {x},{y}"
                );
            }
            if width >= 12 && height >= 3 {
                assert!(flex.cursor_pos(area).is_some(), "{width}x{height}");
            }
        }
    }

    #[test]
    fn status_matches_actual_cancel_and_approval_focus() {
        use crate::tui::task_status::TaskStatus;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

        let area = Rect::new(0, 0, 80, 24);
        let mut pane = BottomPane::new();
        pane.set_task_status(TaskStatus::TurnRunning {
            started_at: Instant::now(),
        });
        pane.composer.set_text("unsent draft");
        let render = |pane: &mut BottomPane| {
            let flex = compact_view(frame(area.width), pane, area.width, area.height);
            let mut buffer = Buffer::empty(area);
            flex.render(area, &mut buffer);
            buffer_to_string(&buffer)
        };
        let text = render(&mut pane);
        assert!(!text.contains("Ctrl+C to stop"), "{text}");
        pane.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(pane.composer.is_empty());
        let text = render(&mut pane);
        assert!(text.contains("Ctrl+C to stop"), "{text}");

        let (response, _receiver) = tokio::sync::oneshot::channel();
        pane.enqueue_approval(
            "bash".into(),
            "Run command".into(),
            None,
            "approval".into(),
            serde_json::Value::Null,
            response,
        );
        let text = render(&mut pane);
        assert!(text.contains("Approval needed"), "{text}");
    }

    #[test]
    fn approval_and_queued_messages_keep_the_editing_cursor_on_short_terminals() {
        use crate::tui::task_status::TaskStatus;
        for height in [6, 8] {
            let area = Rect::new(0, 0, 40, height);
            let mut pane = BottomPane::new();
            pane.set_task_status(TaskStatus::TurnRunning {
                started_at: Instant::now(),
            });
            pane.queue_next_turn_submission("first queued message".into());
            pane.queue_next_turn_submission("second queued message".into());
            pane.composer.set_text("draft");
            let (response, _receiver) = tokio::sync::oneshot::channel();
            pane.enqueue_approval(
                "bash".into(),
                "Run command".into(),
                Some("$ echo hello".into()),
                "reason".into(),
                serde_json::Value::Null,
                response,
            );
            let flex = compact_view(frame(area.width), &mut pane, area.width, area.height);
            let mut buffer = Buffer::empty(area);
            flex.render(area, &mut buffer);
            let text = buffer_to_string(&buffer);
            assert!(text.contains("draft"), "{height}: {text}");
            assert!(text.contains("Approval"), "{height}: {text}");
            assert!(
                !text.contains("←→ · Enter"),
                "draft owns Enter: {height}: {text}"
            );
            let (x, y) = flex.cursor_pos(area).expect("editor cursor");
            assert!(x < area.right() && y < area.bottom(), "{height}: {x},{y}");
            assert_eq!(buffer[(x - 1, y)].symbol(), "t");
        }

        // With no draft, compact the action row only after the request and
        // its reason remain visible and the approval actually owns Enter.
        let area = Rect::new(0, 0, 40, 6);
        let mut pane = BottomPane::new();
        let (response, _receiver) = tokio::sync::oneshot::channel();
        pane.enqueue_approval(
            "bash".into(),
            "Run command".into(),
            Some("$ echo hello".into()),
            "reason".into(),
            serde_json::Value::Null,
            response,
        );
        let flex = compact_view(frame(area.width), &mut pane, area.width, area.height);
        let mut buffer = Buffer::empty(area);
        flex.render(area, &mut buffer);
        let text = buffer_to_string(&buffer);
        assert!(
            text.contains("Approval") && text.contains("Run command") && text.contains("reason"),
            "{text}"
        );
        assert!(
            text.contains("Yes") && text.contains("←→ · Enter"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod task_board_draw_tests {
    use super::super::work_board_projection::{
        SessionTask, SessionTaskStatusKind, TaskStoreHealth,
    };
    use super::{ActiveView, active_viewport, sync_task_footer, task_board_truth_line};
    use crate::tui::{
        bottom_pane::footer::Footer,
        chat_widget, status_indicator,
        task_board_observer::{
            LiveWorkTaskBoardUpdate, ProjectedTaskTruthState, TaskBoardTruthState, WorkBoardContext,
        },
    };
    use unicode_width::UnicodeWidthStr;

    fn draw_task(id: &str, title: &str) -> SessionTask {
        SessionTask {
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

    #[tokio::test]
    async fn completed_board_uses_the_spacer_row_when_the_terminal_is_short() {
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::render::renderable::Renderable;
        use crate::tui::testing::render::buffer_to_string;
        use ratatui::{buffer::Buffer, layout::Rect};

        let observer = crate::tui::task_board_observer::TaskBoardObserver::new("short-completed");
        let tasks = (1..=4)
            .map(|index| {
                let mut task =
                    draw_task(&format!("done-{index}"), &format!("Completed item {index}"));
                task.status = SessionTaskStatusKind::Completed;
                task
            })
            .collect();
        observer.set_projected_task_projection(tasks, ProjectedTaskTruthState::Confirmed);
        let area = Rect::new(0, 0, 80, 11);
        let mut pane = BottomPane::new();
        pane.composer.set_text("one\ntwo\nthree");
        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            true,
            Some(true),
            area.width,
            area.height,
        );
        assert_eq!(frame.task_board.as_ref().map(Vec::len), Some(5));
        let flex = super::compact_view(frame, &mut pane, area.width, area.height);
        let mut buffer = Buffer::empty(area);
        flex.render(area, &mut buffer);
        let text = buffer_to_string(&buffer);
        assert!(text.contains("Completed item"), "{text}");
        assert!(text.contains("more"), "{text}");
        assert!(text.contains("three"), "{text}");
        assert!(flex.cursor_pos(area).is_some());
    }

    fn draw_work_task(id: &str, title: &str, execution: &str) -> SessionTask {
        let mut task = draw_task(&format!("work:work-1:main:{id}"), title);
        task.status = match execution {
            "running" => SessionTaskStatusKind::InProgress,
            _ => SessionTaskStatusKind::Pending,
        };
        task.metadata = Some(serde_json::Map::from_iter([
            ("source".into(), serde_json::json!("work_task_graph")),
            ("declaration_state".into(), serde_json::json!("active")),
            ("execution_status".into(), serde_json::json!(execution)),
            ("delivery_status".into(), serde_json::json!("unreported")),
            ("verification_status".into(), serde_json::json!("unknown")),
        ]));
        task
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

    #[test]
    fn task_truth_notices_are_single_line_and_terminal_width_bounded() {
        use crate::tui::task_board_observer::TaskBoardTruthState;

        for state in [
            TaskBoardTruthState::Loading,
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
            task_board_truth_line(TaskBoardTruthState::Refreshing, TaskStoreHealth::Ready, 12)
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
    fn unavailable_work_notice_is_bounded_and_actionable() {
        let line =
            task_board_truth_line(TaskBoardTruthState::Unavailable, TaskStoreHealth::Ready, 48)
                .expect("notice");
        let text = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.width() <= 48, "{text}");
        assert!(text.contains("Work state unavailable"), "{text}");
    }

    #[tokio::test]
    async fn first_read_failure_renders_unavailable_without_leaking_diagnostics() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-failed");
        observer.set_projected_task_projection(Vec::new(), ProjectedTaskTruthState::Unavailable);

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
    async fn unavailable_empty_work_is_explained_after_the_user_opens_the_board() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-failed");
        observer.set_projected_task_projection(Vec::new(), ProjectedTaskTruthState::Unavailable);

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
        assert!(text.contains("Work state unavailable"), "{text}");
        assert!(text.contains("Ctrl+T → R refresh"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn stale_work_remains_visible_without_leaking_diagnostics() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-plan");
        observer.set_projected_task_projection(
            vec![draw_task("work:work-9:main:item-1", "verify durable work")],
            ProjectedTaskTruthState::Stale,
        );

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
        assert!(text.contains("verify durable work"), "{text}");
        assert!(text.contains("Work state delayed"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn stale_notice_keeps_last_confirmed_task_visible() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-stale");
        observer.set_projected_task_projection(
            vec![draw_task(
                "work:work-1:main:item-1",
                "keep confirmed work visible",
            )],
            ProjectedTaskTruthState::Confirmed,
        );
        observer.set_projected_task_projection(
            vec![draw_task(
                "work:work-1:main:item-1",
                "keep confirmed work visible",
            )],
            ProjectedTaskTruthState::Stale,
        );

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
        assert!(text.contains("Work state delayed"), "{text}");
        assert!(text.contains("keep confirmed work visible"), "{text}");
        assert!(!text.contains("SECRET"), "{text}");
    }

    #[tokio::test]
    async fn active_canonical_work_auto_expands_the_task_board() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-single-empty");
        observer.set_projected_task_projection(
            vec![draw_work_task("item-1", "visible Work item", "not_started")],
            ProjectedTaskTruthState::Confirmed,
        );
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
        assert!(frame.resolved_board_expanded);
        assert!(text.contains("Tasks"), "{text}");
        assert!(text.contains("visible Work item"), "{text}");
    }

    #[test]
    fn live_receipt_auto_expands_before_a_remote_graph_read() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-live-receipt");
        let mut active = draw_work_task("item-1", "execute without polling", "running");
        active.status = SessionTaskStatusKind::InProgress;
        assert!(
            observer.apply_live_work_update(LiveWorkTaskBoardUpdate::Snapshot {
                work: WorkBoardContext {
                    work_id: "work-1".into(),
                    branch_id: "main".into(),
                    goal: "Show durable work immediately".into(),
                    graph_revision: 1,
                    criteria_member_count: 0,
                    milestone_count: 0,
                },
                tasks: vec![active],
            })
        );

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
        assert!(frame.resolved_board_expanded);
        assert!(text.contains("execute without polling"), "{text}");
        assert!(!text.contains("Work state unavailable"), "{text}");
    }

    #[test]
    fn inline_board_tracks_canonical_graph_growth_and_execution_changes() {
        use crate::tui::task_board_observer::TaskBoardObserver;

        let observer = TaskBoardObserver::new("draw-dynamic-work");
        observer.set_projected_task_projection(
            vec![
                draw_work_task("inspect", "Inspect the failing path", "not_started"),
                draw_work_task("repair", "Repair the invariant", "not_started"),
            ],
            ProjectedTaskTruthState::Confirmed,
        );
        let initial = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            true,
            Some(true),
            100,
            32,
        );
        let initial_text = board_text(&initial);
        assert!(initial_text.contains("2 queued"), "{initial_text}");
        assert!(
            initial_text.contains("Inspect the failing path"),
            "{initial_text}"
        );
        assert!(
            initial_text.contains("Repair the invariant"),
            "{initial_text}"
        );

        observer.set_projected_task_projection(
            vec![
                draw_work_task("inspect", "Inspect the failing path", "running"),
                draw_work_task("repair", "Repair the invariant", "not_started"),
                draw_work_task("verify", "Verify the repaired behavior", "not_started"),
            ],
            ProjectedTaskTruthState::Confirmed,
        );
        let advanced = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&observer),
            true,
            Some(true),
            100,
            32,
        );
        let advanced_text = board_text(&advanced);
        assert!(advanced_text.contains("1 working"), "{advanced_text}");
        assert!(advanced_text.contains("2 queued"), "{advanced_text}");
        assert!(
            advanced_text.contains("Verify the repaired behavior"),
            "{advanced_text}"
        );
    }

    #[tokio::test]
    async fn terminal_history_does_not_keep_compact_surface_alive() {
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new("draw-hidden");
        let mut done = draw_task("work:work-1:main:done", "done");
        done.status = SessionTaskStatusKind::Completed;
        obs.set_projected_task_projection(vec![done], ProjectedTaskTruthState::Confirmed);
        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&obs),
            false,
            Some(false),
            80,
            24,
        );
        assert!(
            frame.task_board.is_none(),
            "collapsed terminal history should not occupy the active viewport: {:?}",
            frame.task_board
        );

        let mut footer = Footer::new();
        sync_task_footer(&mut footer, &obs, false);
        assert_eq!(
            footer.task_counts, None,
            "terminal history remains available through Ctrl+T without a sticky footer chip"
        );
    }

    #[tokio::test]
    async fn footer_retains_lifecycle_progress_while_open_work_remains() {
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new("draw-mixed-progress");
        let mut done = draw_task("work:work-1:main:done", "done");
        done.status = SessionTaskStatusKind::Completed;
        obs.set_projected_task_projection(
            vec![done, draw_task("work:work-1:main:open", "still open")],
            ProjectedTaskTruthState::Confirmed,
        );

        let mut footer = Footer::new();
        sync_task_footer(&mut footer, &obs, false);
        assert_eq!(
            footer.task_counts,
            Some((1, 2)),
            "active progress should retain completed lifecycle context"
        );
    }

    #[tokio::test]
    async fn expanded_task_board_falls_back_to_hint_when_terminal_too_short() {
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new("draw-short");
        obs.set_projected_task_projection(
            vec![draw_task("work:work-1:main:item-1", "fit me somewhere")],
            ProjectedTaskTruthState::Confirmed,
        );

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
            ActiveView::Hint(line) => {
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
        let obs = crate::tui::task_board_observer::TaskBoardObserver::new("draw-coexist");
        obs.set_projected_task_projection(
            vec![draw_task("work:work-1:main:item-1", "pending task")],
            ProjectedTaskTruthState::Confirmed,
        );

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
    fn live_reply_stays_visible_while_a_tool_is_running() {
        use crate::tui::chat_widget::WireEvent;
        let mut widget = chat_widget::ChatWidget::new(String::new());
        widget.handle_event(chat_widget::AppEvent::wire(WireEvent::ToolStarted {
            name: "bash".into(),
            description: "long task".into(),
            tool_use_id: "running-task".into(),
            parent_tool_use_id: None,
        }));
        widget.handle_event(chat_widget::AppEvent::wire(WireEvent::AnswerDelta {
            model_item_id: None,
            text: "Here is the status".into(),
        }));
        let frame = active_viewport(
            &widget,
            &status_indicator::StatusIndicator::new(),
            None,
            false,
            None,
            80,
            12,
        );
        let ActiveView::Active { lines, parked, .. } = frame.active else {
            panic!("reply must be rendered in the active viewport");
        };
        let rendered = lines
            .iter()
            .chain(&parked)
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let answer = rendered.find("Here is the status").expect("visible reply");
        let tool = rendered.find("Bash").expect("running tool remains visible");
        assert!(
            answer < tool,
            "the reply takes priority in a short viewport"
        );
        assert!(widget.has_live_tool_projection());
    }

    #[test]
    fn completed_logical_agents_linger_briefly_even_when_duration_exceeds_local_elapsed() {
        use crate::tui::chat_widget::WireEvent;

        let mut widget = chat_widget::ChatWidget::new(String::new());
        let mut status = status_indicator::StatusIndicator::new();
        status.begin_turn(std::time::Instant::now());
        widget.handle_event(chat_widget::AppEvent::wire(
            WireEvent::AgentControlCompleted {
                action: "get_result".into(),
                label: "reviewer".into(),
                status: "completed".into(),
                duration_ms: 50,
                output: Some(
                    serde_json::json!({
                        "agent_id": "reviewer@abc",
                        "run_id": "reviewer-run",
                        "result_family": "child_result",
                        "status": "completed",
                        "result": "done"
                    })
                    .to_string(),
                ),
                tool_use_id: "tu_get_result".into(),
                agent_id: Some("reviewer@abc".into()),
            },
        ));

        let rows = widget.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id.as_deref(), Some("reviewer-run"));
        assert_eq!(
            rows[0].state.status,
            crate::tui::agent_run_projection::AgentRunStatus::Completed
        );
        let frame = active_viewport(&widget, &status, None, false, None, 80, 24);

        assert!(
            frame.multi_agent.is_some(),
            "freshly completed logical agents should linger even when child duration exceeds the local registry elapsed time"
        );
    }

    #[test]
    fn completed_agent_strip_dismisses_when_the_parent_turn_settles() {
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
                        "agent_id": "reviewer@settled",
                        "run_id": "settled-run",
                        "result_family": "child_result",
                        "status": "completed",
                        "result": "done"
                    })
                    .to_string(),
                ),
                tool_use_id: "tu_get_result_settled".into(),
                agent_id: Some("reviewer@settled".into()),
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
            frame.multi_agent.is_none(),
            "terminal agent history remains in Ctrl+G and must not pin the idle viewport"
        );
        let rows = widget.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].run_id.as_deref(), Some("settled-run"));
        assert_eq!(
            rows[0].state.status,
            crate::tui::agent_run_projection::AgentRunStatus::Completed
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
    //! The chat surface owns one stable aggregate line. Per-agent activity and
    //! controls live on the background-work surface.
    use super::{
        LiveFramedCell, MultiAgentEntry, focused_agent_activity_line, multi_agent_strip_header,
        render_primary_workspace, should_show_multi_agent_strip, truncate_label,
        workspace_agent_activity_line, workspace_conversation_tab_line,
    };
    use crate::tui::agent_run_projection::{AgentRunState, AgentRunStatus};

    #[test]
    fn narrow_tabs_prioritize_the_active_name_and_display_cell_width() {
        use crate::tui::bottom_pane::ConversationTab;
        let tabs = vec![
            ConversationTab {
                label: "Main conversation".into(),
                active: false,
            },
            ConversationTab {
                label: "审查代理甲".into(),
                active: true,
            },
        ];
        for width in [8, 12, 16, 24, 80] {
            let line = workspace_conversation_tab_line(&tabs, width).unwrap();
            let text = line.to_string();
            assert!(line.width() <= usize::from(width), "{width}: {text}");
            assert!(
                text.contains("审查"),
                "active identity missing at {width}: {text}"
            );
        }
    }

    #[test]
    fn child_workspace_uses_the_selected_run_instead_of_the_live_aggregate() {
        use crate::tui::chat_widget::{AppEvent, ChatWidget, WireEvent};
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal, AgentLiveTermination,
        };
        let mut widget = ChatWidget::new(String::new());
        for run in ["run-a", "run-b"] {
            widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
                agent_id: "reviewer".into(),
                run_id: run.into(),
                kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                    parent_run_id: Some("root".into()),
                    depth: 1,
                    spawn_tool_call_id: None,
                    transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
                }),
            })));
        }
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer".into(),
            run_id: "run-a".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::ModelResponding),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer".into(),
            run_id: "run-b".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Completed,
                duration_ms: 1500,
                reason: None,
            },
        })));
        let running = focused_agent_activity_line(&widget, "reviewer", "run-a", 80).to_string();
        let completed = focused_agent_activity_line(&widget, "reviewer", "run-b", 80).to_string();
        assert!(running.contains("working"), "{running}");
        assert!(
            completed.contains("done") && completed.contains("1.5s"),
            "{completed}"
        );
        let unknown =
            focused_agent_activity_line(&widget, "reviewer", "different-run", 80).to_string();
        assert!(unknown.contains("unconfirmed"), "{unknown}");
    }

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
        assert!(header.contains("2 agents"));
        assert!(header.contains("2 live"));
        assert!(!header.contains("failed"));
        assert!(!header.contains("done"));
        assert!(header.contains("Shift+↓ manage"));
        assert!(header.contains("Ctrl+G agents"));
        assert!(!header.contains("X stop"));
    }

    #[test]
    fn header_surfaces_failed_count_when_any_failure() {
        // Regression: pre-fix the user only saw "▶ 3 parallel agents"
        // even though one was already dead. The breakdown must call out
        // failures so the user catches them at a glance.
        let cells = vec![entry(true, false), entry(false, true), entry(false, false)];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("3 agents"));
        assert!(header.contains("1 live"));
        assert!(header.contains("1 failed"));
        assert!(header.contains("1 done"));
    }

    #[test]
    fn header_skips_zero_buckets() {
        let cells = vec![entry(false, false), entry(false, false)];
        let header = multi_agent_strip_header(&cells);
        assert!(header.contains("2 agents"));
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
    fn terminal_failure_dismisses_but_cancelling_stays_visible() {
        assert!(
            !should_show_multi_agent_strip(&[entry(false, true)], false),
            "confirmed terminal failures remain in history and must not pin the live viewport"
        );
        assert!(should_show_multi_agent_strip(&[cancelling_entry()], false));
    }

    #[test]
    fn interrupted_only_strip_lingers_then_dismisses() {
        let mut interrupted = entry(false, false);
        interrupted.state = AgentRunState::observed(AgentRunStatus::Interrupted);
        assert!(should_show_multi_agent_strip(
            std::slice::from_ref(&interrupted),
            true
        ));
        assert!(
            !should_show_multi_agent_strip(&[interrupted], false),
            "interrupted agents remain available through Ctrl+G without pinning the live strip"
        );
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
        assert!(header.contains("Shift+↓ manage"), "{header}");
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

        assert!(text.contains("1 agent"), "{text}");
        assert!(text.contains("1 live"), "{text}");
        assert!(text.contains("Shift+↓ manage"), "{text}");
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

        assert!(text.contains("1 agent"), "{text}");
        assert!(text.contains("Shift+↓ manage"), "{text}");
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
    fn short_child_workspace_keeps_transcript_and_approval_notice_visible() {
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptView;
        use crate::tui::bottom_pane::transcript_view::{TranscriptSnapshot, TranscriptView};
        use crate::tui::testing::render::buffer_to_string;
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use ratatui::{buffer::Buffer, layout::Rect};

        let mut pane = BottomPane::new();
        pane.push_view(Box::new(TranscriptView::from_snapshot(
            TranscriptSnapshot::default(),
            24,
            80,
        )));
        pane.push_view(Box::new(AgentTranscriptView::live_unbound(
            "reviewer".into(),
            "Reviewer".into(),
            "run-1".into(),
            None,
            "agents",
            80,
            24,
        )));
        assert!(pane.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "reviewer".into(),
            run_id: "run-1".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: None,
                text: "visible child evidence".into()
            },
        }));
        let (response, _receiver) = tokio::sync::oneshot::channel();
        pane.enqueue_approval(
            "bash".into(),
            "Run command".into(),
            None,
            "approval".into(),
            serde_json::Value::Null,
            response,
        );
        let activity = workspace_agent_activity_line(&[entry(true, false)], 80);
        for height in [5, 6, 8] {
            let area = Rect::new(0, 0, 80, height);
            let mut buffer = Buffer::empty(area);
            render_primary_workspace(area, &mut buffer, &mut pane, Some(&activity));
            let text = buffer_to_string(&buffer);
            assert!(text.contains("Transcript · Reviewer"), "{height}: {text}");
            assert!(text.contains("visible child evidence"), "{height}: {text}");
            assert!(text.to_lowercase().contains("approval"), "{height}: {text}");
        }
    }

    #[test]
    fn child_search_cursor_follows_the_painted_search_row_above_footers() {
        use crate::tui::bottom_pane::BottomPane;
        use crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptView;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        use ratatui::{buffer::Buffer, layout::Rect};

        let mut pane = BottomPane::new();
        pane.push_view(Box::new(AgentTranscriptView::live_unbound(
            "reviewer".into(),
            "Reviewer".into(),
            "run-1".into(),
            None,
            "agents",
            80,
            24,
        )));
        let (response, _receiver) = tokio::sync::oneshot::channel();
        pane.enqueue_approval(
            "bash".into(),
            "Run".into(),
            None,
            "approval".into(),
            serde_json::Value::Null,
            response,
        );
        pane.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        pane.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let area = Rect::new(0, 0, 80, 8);
        let mut buffer = Buffer::empty(area);
        let cursor =
            render_primary_workspace(area, &mut buffer, &mut pane, None).expect("search cursor");
        let row: String = (0..area.width)
            .map(|x| buffer[(x, cursor.1)].symbol())
            .collect();
        assert!(row.contains("Search: /x"), "cursor at {cursor:?}: {row:?}");
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
    fn live_and_frozen_tool_titles_render_in_the_same_terminal_column() {
        use crate::tui::history_cell::{HistoryCell, tool::ToolCell};
        use crate::tui::render::{line_utils::FullRowParagraph, renderable::Renderable};
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

        let area = Rect::new(0, 0, 40, 2);
        let mut tool = ToolCell::new_running("bash", "$ cargo test");

        let mut live_buffer = Buffer::empty(area);
        LiveFramedCell {
            lines: tool.display_lines(area.width.saturating_sub(2)),
            live: true,
        }
        .render(area, &mut live_buffer);

        tool.complete("success", 42, "$ cargo test".into(), None, None);
        let mut frozen_buffer = Buffer::empty(area);
        Widget::render(
            FullRowParagraph::new(tool.display_lines(area.width)),
            area,
            &mut frozen_buffer,
        );

        let title_column = |buffer: &Buffer| {
            (area.x..area.right())
                .find(|&x| buffer[(x, area.y)].symbol() == "R")
                .expect("tool title must contain its lifecycle verb")
        };
        assert_eq!(
            live_buffer[(0, 0)].symbol(),
            crate::tui::glyphs::current().live_gutter.to_string()
        );
        assert_eq!(frozen_buffer[(0, 0)].symbol(), "●");
        assert_eq!(title_column(&live_buffer), title_column(&frozen_buffer));
    }

    #[test]
    fn live_and_frozen_thought_titles_render_in_the_same_terminal_column() {
        use crate::tui::history_cell::{HistoryCell, reasoning::ReasoningCell};
        use crate::tui::render::{line_utils::FullRowParagraph, renderable::Renderable};
        use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};

        let area = Rect::new(0, 0, 60, 2);
        let mut thought = ReasoningCell::new_streaming();
        thought.push_delta("inspect the state");

        let mut live_buffer = Buffer::empty(area);
        LiveFramedCell {
            lines: thought.display_lines(area.width.saturating_sub(2)),
            live: true,
        }
        .render(area, &mut live_buffer);

        thought.finalize();
        let mut frozen_buffer = Buffer::empty(area);
        Widget::render(
            FullRowParagraph::new(thought.display_lines(area.width)),
            area,
            &mut frozen_buffer,
        );

        let title_column = |buffer: &Buffer| {
            (area.x..area.right())
                .find(|&x| buffer[(x, area.y)].symbol() == "T")
                .expect("thought title must be rendered")
        };
        assert_eq!(
            live_buffer[(0, 0)].symbol(),
            crate::tui::glyphs::current().live_gutter.to_string()
        );
        assert_eq!(
            frozen_buffer[(0, 0)].symbol(),
            crate::tui::glyphs::current().agent_completed
        );
        assert_eq!(title_column(&live_buffer), title_column(&frozen_buffer));
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
