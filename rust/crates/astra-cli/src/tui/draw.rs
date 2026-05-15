//! Frame composition: `ActiveView` priority resolver + `do_draw`.
//!
//! The TUI's draw cycle is: compute which source owns the active-cell
//! region (active cell / task board / status line / next-hint / empty),
//! then paint `active | separator | bottom pane` into the ratatui
//! frame. This file owns that pipeline so the outer event loop in
//! [`super::event_loop`] doesn't carry render details.
//!
//! See `ARCHITECTURE.md` for the visual-hierarchy grammar.

use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph, Widget};

use super::bottom_pane::{BottomPane, footer::Footer};
use super::render::renderable::{FlexRenderable, Renderable, RenderableItem};
use super::task_board_observer::TaskBoardObserver;
use super::terminal::TerminalGuard;
use super::{chat_widget, status_indicator, task_list};

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
///   left `█` gutter whose colour gradient flows while live and
///   freezes in place on completion.
/// - **Status** — a one-line indicator (`✶ Thinking …`) when we
///   have a turn in flight but no cell content yet. No border —
///   the cue is the spinner, not the frame.
pub(crate) enum ActiveView {
    Empty,
    Status(Line<'static>),
    Active {
        lines: Vec<Line<'static>>,
        /// `true` while still streaming — gradient flows. `false`
        /// once finalized — gradient freezes in place.
        live: bool,
        /// Process-relative seconds at which the underlying cell
        /// finalized. Mirrors `HistoryCell::frozen_phase` so the
        /// active-slot gutter can lock its phase on freeze.
        freeze_phase: Option<f32>,
    },
}

/// Single live agent's render data inside `ViewportFrame.multi_agent`.
///
/// Compact per-agent row (claudecode/Kiro-style): `{status_icon}
/// {name}  {child_count} steps · {elapsed}`. The whole strip renders
/// inside ONE `LiveFramedCell` with a single gradient gutter so the
/// user sees N agents as a tidy panel, not N stacked frames.
pub(crate) struct MultiAgentEntry {
    pub agent_id: String,
    /// Display label — name from `agent.spawn` if set, falling back
    /// to description. Building it lives in `agents_drilldown_rows`-
    /// adjacent territory; here we just render the prepared string.
    pub label: String,
    /// Number of child tool calls observed under this agent. Mirrors
    /// the count shown by Kiro's monitor.
    pub child_count: usize,
    pub elapsed_ms: u64,
    pub live: bool,
    pub failed: bool,
}

/// Pair of (ActiveView, TaskBoard lines) produced by
/// [`active_viewport`]. The task board is its OWN slot — not an
/// `ActiveView` variant — so a streaming active cell (tool /
/// assistant) no longer replaces the board mid-frame. Matches
/// claude-code's Ink `<Static>` / panel split.
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
}

pub(crate) fn active_viewport(
    chat_widget: &chat_widget::ChatWidget,
    status: &status_indicator::StatusIndicator,
    board: Option<&TaskBoardObserver>,
    board_expanded: bool,
    width: u16,
    rows: u16,
) -> ViewportFrame {
    // Reserve 2 cols for the `█ ` gutter.
    let inner_w = width.saturating_sub(2).max(20);

    // Multi-agent strip: one compact row per logical background
    // agent, not per control-plane `agent.spawn/get_result` tool
    // call. Co-exists with `active_cell` (assistant streaming WHILE
    // sub-agents run shows both).
    //
    // Visibility rule: show the strip while ANY agent is still live
    // (running). Completed-only strips linger for `STRIP_LINGER` so
    // the user can glance at the final state, then dismiss. Failed
    // entries stay visible until a new turn state replaces them, so
    // failures are not silently missed after a short linger timeout.
    // Users can still drill in via Ctrl+G.
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
                    child_count: tc.children.len(),
                    elapsed_ms: tc
                        .duration_ms
                        .unwrap_or_else(|| tc.started_at.elapsed().as_millis() as u64),
                    live: matches!(
                        tc.status,
                        crate::tui::history_cell::task::TaskStatus::Running
                    ),
                    failed: matches!(
                        tc.status,
                        crate::tui::history_cell::task::TaskStatus::Failed
                    ),
                })
            })
            .collect();

        let any_live = cells.iter().any(|entry| entry.live);
        let any_failed = cells.iter().any(|entry| entry.failed);
        let any_recently_terminal = !any_live
            && agent_ids.iter().any(|id| {
                chat_widget.agent_run_cell(id).is_some_and(|tc| {
                    tc.completed_at
                        .is_some_and(|completed_at| completed_at.elapsed() < STRIP_LINGER)
                })
            });
        if any_live || any_failed || any_recently_terminal {
            Some(cells)
        } else {
            None
        }
    } else {
        None
    };

    let active = chat_widget.active_cell().and_then(|cell| {
        let lines = cell.display_lines(inner_w);
        if lines.is_empty() {
            None
        } else {
            let live = cell.is_live();
            let freeze_phase = cell.frozen_phase();
            Some((lines, live, freeze_phase))
        }
    });
    // Pump the observer before reading its snapshot. Without this,
    // mid-turn `task.create` / `task.update` calls land in
    // `session_todos` but never propagate into the snapshot until
    // the outer-tick branch fires `maybe_refresh()` — which can
    // be 30+ seconds for a long agentic loop. The observer's own
    // dirty/window gating makes per-frame calls cheap (no fetch
    // unless something actually changed).
    if let Some(b) = board {
        b.maybe_refresh();
    }
    let snap = board.map(|b| b.snapshot()).unwrap_or_default();
    // In multi-session mode the standard `snap.tasks` is empty by
    // design (observer populates `multi_snapshot` instead). Pick
    // the right render path so the expanded board honors whichever
    // view mode the user toggled via Ctrl+Shift+T.
    let task_board = if board_expanded && !snap.hidden {
        let mode = board
            .map(|b| b.view_mode())
            .unwrap_or(super::task_board_observer::ViewMode::SingleSession);
        match mode {
            super::task_board_observer::ViewMode::AllSessions => {
                let multi = board.map(|b| b.multi_snapshot()).unwrap_or_default();
                if multi.per_session.is_empty() {
                    None
                } else {
                    let lines = task_list::render_multi(&multi.per_session, width, rows);
                    if lines.is_empty() { None } else { Some(lines) }
                }
            }
            super::task_board_observer::ViewMode::SingleSession => {
                if snap.tasks.is_empty() {
                    None
                } else {
                    let fresh_task_ids = board
                        .map(|observer| observer.fresh_task_id_set())
                        .unwrap_or_default();
                    let lines = task_list::render_with_fresh_predicate(
                        &snap.tasks,
                        width,
                        rows,
                        true,
                        |task_id| fresh_task_ids.contains(task_id),
                    );
                    if lines.is_empty() { None } else { Some(lines) }
                }
            }
        }
    } else {
        None
    };
    let status_line = status.render();
    // Hint fallback still runs — but ONLY when we have no board to
    // render (collapsed or empty). When the board IS visible, the
    // hint is redundant noise.
    let next_hint = if task_board.is_none() && !snap.hidden {
        task_list::render_next_hint(&snap.tasks, width)
    } else {
        None
    };
    let active = pick_active_view(active, status_line, next_hint);
    ViewportFrame {
        active,
        multi_agent: multi_agent_active,
        task_board,
    }
}

/// Pure priority resolver. Priority: **Active > Status > NextHint >
/// Empty**. The task board is NOT on this chain anymore — it has its
/// own sibling slot in the frame so a streaming active cell cannot
/// flicker-replace it. See [`ViewportFrame`].
pub(crate) fn pick_active_view(
    active: Option<(Vec<Line<'static>>, bool, Option<f32>)>,
    status_line: Option<Line<'static>>,
    next_hint: Option<Line<'static>>,
) -> ActiveView {
    // Priority: Active > Status > NextHint > Empty. Multi-agent
    // strip is now a SIBLING field on ViewportFrame (rendered
    // above the active slot) so this resolver no longer needs to
    // arbitrate between them — both can co-exist.
    if let Some((lines, live, freeze_phase)) = active {
        return ActiveView::Active {
            lines,
            live,
            freeze_phase,
        };
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
    }
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
        // Active cell renders with a single-column gradient gutter
        // on the left. While live the gradient flows over time; once
        // finalized it freezes at the phase captured by the cell so
        // there's no visual jump on completion. (PR #335.)
        ActiveView::Active {
            lines,
            live,
            freeze_phase,
        } => {
            let framed = LiveFramedCell {
                lines,
                live,
                freeze_phase,
            };
            RenderableItem::Owned(Box::new(framed))
        }
    };

    // Multi-agent strip: claudecode/Kiro-style compact panel. ONE
    // gradient-gutter frame containing a header and one short row
    // per live agent. Renders as e.g.:
    //
    //   █ ▶ 3 parallel agents · Ctrl+G to drill in
    //   █ ◦ review_tui      · 2 steps · 12s
    //   █ ◦ review_fixes    · 0 steps · 8s
    //   █ ✓ review_refactor · 4 steps · 18s
    //
    // (status: ◦ live / ✓ completed / ✗ failed)
    let multi_agent_renderable: Option<RenderableItem<'_>> = multi_agent.map(|cells| {
        let count = cells.len();
        let theme = crate::tui::theme::current();
        let header_line = Line::from(ratatui::text::Span::styled(
            format!("▶ {count} parallel agents · Ctrl+G to drill in"),
            ratatui::style::Style::default()
                .fg(theme.accent)
                .add_modifier(ratatui::style::Modifier::BOLD),
        ));

        // Width budget for the label — leave room for status icon (2),
        // " · " separator (3), "N steps" (~10), " · " (3), "elapsed"
        // (~6) ≈ 24 chars of overhead.
        let total_overhead = 24;
        let label_budget = (width as usize).saturating_sub(2 + total_overhead).max(10);

        let dim = ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(cells.len() + 1);
        lines.push(header_line);
        for entry in cells {
            let (icon, icon_color) = if entry.failed {
                ("✗", ratatui::style::Color::Red)
            } else if entry.live {
                ("◦", ratatui::style::Color::Yellow)
            } else {
                ("✓", ratatui::style::Color::Green)
            };
            let label = truncate_label(&entry.label, label_budget);
            let suffix = format!(
                " · {} steps · {}",
                entry.child_count,
                format_short_elapsed(entry.elapsed_ms),
            );
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

        let framed = LiveFramedCell {
            lines,
            live: true,
            freeze_phase: None,
        };
        RenderableItem::Owned(Box::new(framed) as Box<dyn Renderable>)
    });

    // Thin dim separator between scrollback area and composer
    let sep_line = Line::from(ratatui::text::Span::styled(
        "─".repeat(width as usize),
        ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray),
    ));
    let sep_renderable = RenderableItem::Owned(Box::new(sep_line));

    let bp_renderable = BottomPaneRenderable(bottom_pane);
    let bp_item = RenderableItem::Owned(Box::new(bp_renderable) as Box<dyn Renderable>);

    let mut flex = FlexRenderable::new();
    // Task board FIRST so it sits above the active cell — claude-code
    // stacks its live-panels-as-static on top of the streaming region.
    // Flex weight 0 = "take only what you need"; the board has a
    // deterministic row count so weight 0 is correct, stream content
    // gets the remainder.
    if let Some(lines) = task_board_lines
        && !lines.is_empty()
    {
        let para = Paragraph::new(ratatui::text::Text::from(lines));
        flex.push(0, RenderableItem::Owned(Box::new(para)));
    }
    // Multi-agent strip BETWEEN task board and active cell. So the
    // user sees: board (Tier 1 todos) → parallel agents (Tier 2 sub
    // -agents) → active cell (assistant streaming) → composer.
    if let Some(item) = multi_agent_renderable {
        flex.push(0, item);
    }
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

/// Left-gutter renderable: a solid `█` bar on the left edge with a
/// top-to-bottom colour gradient. While the cell is still streaming
/// (`live == true`) the gradient flows downward over time; once
/// finalized (`live == false`) the gradient freezes in place so there
/// is no visual jump or flash when output completes. PR #335.
struct LiveFramedCell {
    lines: Vec<Line<'static>>,
    #[allow(dead_code)]
    live: bool,
    /// Process-relative seconds at which the underlying cell finalized.
    /// `Some` once frozen — fed into `gradient_color_at_t` so the bar
    /// stops at the exact phase it had on the final live frame.
    /// `None` while live, in which case the renderer reads `now` so
    /// the gradient flows.
    freeze_phase: Option<f32>,
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

        // Single formula for live and frozen — only the time component
        // differs. Live reads `now`; frozen pins the captured value so
        // colors don't snap on transition.
        let height = area.height as usize;
        let period = super::shimmer::LIVE_GUTTER_PERIOD_SECS;
        let t = match self.freeze_phase {
            Some(t) => t,
            None => super::shimmer::elapsed_since_start().as_secs_f32(),
        };

        for row in 0..height {
            let (r, g, b) = super::shimmer::gradient_color_at_t(row, height.max(1), period, t);
            let color = ratatui::style::Color::Rgb(r, g, b);
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

/// Char-aware label truncation with a single-character ellipsis.
/// Multi-byte safe (CJK label like "审查代码" stays valid). When
/// `max == 0` returns empty string. When the label fits, returned
/// as-is.
pub(crate) fn truncate_label(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{truncated}…")
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
            Some((lines("active-tool"), true, None)),
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
                if a {
                    Some((lines("a"), true, None))
                } else {
                    None
                },
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
    use super::{ActiveView, active_viewport, sync_task_footer};
    use crate::tui::{bottom_pane::footer::Footer, chat_widget, status_indicator};
    use astra_tools::task_mgmt::{InMemoryTaskStore, TaskManager, TaskStore};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    async fn wait_until<F: Fn() -> bool>(cond: F, timeout_ms: u64, pump: impl Fn()) {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while !cond() && Instant::now() < deadline {
            pump();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
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
        mgr.update(&serde_json::json!({"task_id": "task-1", "status": "completed"}))
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

        let frame = active_viewport(
            &chat_widget::ChatWidget::new(String::new()),
            &status_indicator::StatusIndicator::new(),
            Some(&obs),
            true,
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
                    text.contains("Next:") && text.contains("fit me somewhere"),
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
        widget.handle_event(chat_widget::AppEvent::Wire(WireEvent::ToolStarted {
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
        widget.handle_event(chat_widget::AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: reviewer@abc".into(),
            status: "success".into(),
            duration_ms: 50,
            output_summary: None,
            output: Some(
                serde_json::json!({
                    "agent_id": "reviewer@abc",
                    "status": "completed",
                    "result": "done"
                })
                .to_string(),
            ),
            tool_use_id: "tu_get_result".into(),
            parent_tool_use_id: None,
        }));

        let frame = active_viewport(
            &widget,
            &status_indicator::StatusIndicator::new(),
            None,
            false,
            80,
            24,
        );

        assert!(
            frame.multi_agent.is_some(),
            "freshly completed logical agents should linger even when child duration exceeds the local registry elapsed time"
        );
    }
}

#[cfg(test)]
mod multi_agent_strip_tests {
    //! Pin the compact per-agent row format (claudecode/Kiro-style).
    //!
    //! Each row fits in one line and surfaces label, step count, and
    //! elapsed time, plus a status icon that distinguishes live
    //! agents from completed ones.
    use super::{format_short_elapsed, truncate_label};

    #[test]
    fn elapsed_under_a_second_renders_in_milliseconds() {
        assert_eq!(format_short_elapsed(0), "0ms");
        assert_eq!(format_short_elapsed(150), "150ms");
        assert_eq!(format_short_elapsed(999), "999ms");
    }

    #[test]
    fn elapsed_seconds_render_compact_no_decimals() {
        // Matches Kiro's "0m 10s" inspiration but compacts to "10s"
        // for sub-minute durations — the strip row has limited width.
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

    /// REGRESSION (session 8ca96f0f): when N parallel agents share
    /// the strip, the panel had ONE header + N stacked frames each
    /// holding the full TaskCell display_lines. With description-only
    /// rendering, all rows looked alike. The new shape is a single
    /// frame containing the header + one short row per agent. Pin the
    /// per-row data shape — the renderer assembles those rows into
    /// `█ ◦ name · 2 steps · 12s` lines.
    #[test]
    fn multi_agent_entry_shape_carries_the_compact_row_data() {
        // This test pins the public field set on `MultiAgentEntry`
        // — if anyone resurrects the old `lines: Vec<Line>` shape
        // (which embedded each agent's full TaskCell) this build
        // breaks at the constructor below.
        let entry = super::MultiAgentEntry {
            agent_id: "agent-A".into(),
            label: "review_tui".into(),
            child_count: 4,
            elapsed_ms: 12_000,
            live: true,
            failed: false,
        };
        assert_eq!(entry.label, "review_tui");
        assert_eq!(entry.child_count, 4);
        assert_eq!(format_short_elapsed(entry.elapsed_ms), "12s");
        assert!(entry.live);
        assert!(!entry.failed);
    }
}
