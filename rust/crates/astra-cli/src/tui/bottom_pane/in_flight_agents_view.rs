//! In-flight agents drill-in list.
//!
//! When multiple sub-agents are running in parallel (the model spawned
//! N agent spawn actions in one turn), the user presses `Ctrl+G` to
//! open this view: a vertical list of every live TaskCell with its
//! description, child count, and elapsed time. ↑↓ navigates, Enter
//! drills into a `TaskDetailView` for the selected agent, Esc/← closes.
//!
//! Rows are a snapshot supplied by `ChatWidget`, and the outer event
//! loop refreshes the snapshot while the monitor is open whenever an
//! agent lifecycle event can affect row state. The view itself stays
//! ownership-only: it never holds a reference back into `ChatWidget`.
//!
//! Result-prefix sentinel: `__agent_drilldown__\n<agent_id>`. The
//! outer event loop strips the prefix and pushes a `TaskDetailView`
//! built from the matching live TaskCell.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::view::{BottomPaneView, CancellationEvent};

pub(crate) const AGENT_DRILLDOWN_SENTINEL: &str = "__agent_drilldown__\n";
const AUTO_DISMISS_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Sentinel emitted when the user presses `x` (or Delete) on a live row in
/// the in-flight agents drill view. The outer event loop strips the
/// prefix and routes to the spawner's `cancel_agent` API.
///
/// Why a separate sentinel from drilldown: cancellation is irreversible
/// — surfacing it through the same channel as drilldown ("open this
/// agent") would conflate intent and risk a UI race where the user's
/// cursor was off-by-one when they pressed Enter on a row that was
/// about to fail. Distinct sentinels make the intent legible at the
/// dispatch layer and at the test boundary.
pub(crate) const AGENT_KILL_SENTINEL: &str = "__agent_kill__\n";

/// Strip the kill sentinel and return the agent_id, with the same
/// trailing-newline defensive trim as `parse_drilldown_sentinel`.
pub(crate) fn parse_kill_sentinel(s: &str) -> Option<&str> {
    s.strip_prefix(AGENT_KILL_SENTINEL)
        .map(|rest| {
            let rest = rest.trim_start_matches('\n');
            rest.split_once('\n').map(|(id, _)| id).unwrap_or(rest)
        })
        .filter(|id| !id.is_empty())
}

/// Strip the drill-in sentinel and return the agent_id, defensively
/// trimming after the first newline so a malformed id can't carry
/// trailing garbage. Returns `None` when the input doesn't match.
///
/// The runtime spawner's agent_ids follow the `<name>@<uuid_prefix>`
/// format which is newline-free in practice, but this stays as a
/// safety net so a future code path that builds the sentinel
/// incorrectly can't silently dispatch a wrong id.
pub(crate) fn parse_drilldown_sentinel(s: &str) -> Option<&str> {
    s.strip_prefix(AGENT_DRILLDOWN_SENTINEL)
        .map(|rest| {
            let rest = rest.trim_start_matches('\n');
            rest.split_once('\n').map(|(id, _)| id).unwrap_or(rest)
        })
        .filter(|id| !id.is_empty())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentRowStatus {
    Live,
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

impl AgentRowStatus {
    fn color(self) -> Color {
        match self {
            AgentRowStatus::Live | AgentRowStatus::Cancelling => Color::Yellow,
            AgentRowStatus::Completed => Color::Green,
            AgentRowStatus::Failed => Color::Red,
            AgentRowStatus::Cancelled => Color::DarkGray,
        }
    }

    fn is_live(self) -> bool {
        matches!(self, AgentRowStatus::Live | AgentRowStatus::Cancelling)
    }

    fn is_failed(self) -> bool {
        matches!(self, AgentRowStatus::Failed)
    }

    fn phrase(self) -> Option<&'static str> {
        match self {
            AgentRowStatus::Live => None,
            AgentRowStatus::Cancelling => Some("stopping"),
            AgentRowStatus::Completed => Some("done"),
            AgentRowStatus::Failed => Some("failed"),
            AgentRowStatus::Cancelled => Some("stopped"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentFanoutMembership {
    pub group_id: String,
    pub group_title: String,
    pub target_count: usize,
    pub slot_index: usize,
    pub slot_label: String,
}

#[derive(Clone)]
pub(crate) struct AgentRow {
    pub agent_id: String,
    pub name: String,
    pub child_count: usize,
    pub elapsed_ms: u64,
    pub status: AgentRowStatus,
    pub fanout: Option<AgentFanoutMembership>,
}

/// What action the view emitted on completion.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AcceptedAction {
    /// User pressed Enter — drill into the agent's detail view.
    Drilldown(String),
}

pub(crate) struct InFlightAgentsView {
    rows: Vec<AgentRow>,
    live_count: usize,
    failed_count: usize,
    selected: usize,
    completed: bool,
    accepted: Option<AcceptedAction>,
    /// Sentinel queued by a non-terminating action (currently `x`/Delete →
    /// kill). Drained by `take_pending_action`. The view stays open after
    /// emitting so the user observes the row transition Live →
    /// Cancelling → Cancelled in real time and can kill additional rows
    /// without re-opening Ctrl+G.
    pending_action: Option<String>,
    /// When set, the view will auto-dismiss after this instant.
    /// Armed when all rows become terminal (no live agents). Gives the
    /// user ~3 seconds to see final status before the view closes itself.
    /// Reset to None if the user interacts (key press) or if new live
    /// rows appear.
    auto_dismiss_at: Option<std::time::Instant>,
}

impl InFlightAgentsView {
    pub fn new(rows: Vec<AgentRow>) -> Self {
        let (live_count, failed_count) = count_rows(&rows);
        let auto_dismiss_at = terminal_auto_dismiss_at(live_count, !rows.is_empty());
        Self {
            rows,
            live_count,
            failed_count,
            selected: 0,
            completed: false,
            accepted: None,
            pending_action: None,
            auto_dismiss_at,
        }
    }

    fn replace_rows(&mut self, rows: Vec<AgentRow>) {
        let selected_id = self.rows.get(self.selected).map(|row| row.agent_id.clone());
        let (live_count, failed_count) = count_rows(&rows);
        self.selected = selected_id
            .and_then(|id| rows.iter().position(|row| row.agent_id == id))
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.live_count = live_count;
        self.failed_count = failed_count;
        // Arm auto-dismiss when all rows become terminal. Reset if live
        // rows reappear (e.g. user spawns a new agent while the view is
        // still open in the grace period).
        if live_count == 0 && !self.rows.is_empty() {
            if self.auto_dismiss_at.is_none() {
                self.auto_dismiss_at = terminal_auto_dismiss_at(live_count, true);
            }
        } else {
            self.auto_dismiss_at = None;
        }
    }

    fn postpone_auto_dismiss_if_terminal(&mut self) {
        if self.live_count == 0 && !self.rows.is_empty() {
            self.auto_dismiss_at = terminal_auto_dismiss_at(self.live_count, true);
        }
    }

    fn move_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.rows.len() - 1
        } else {
            self.selected - 1
        };
    }

    fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.rows.len();
    }

    fn move_page_up(&mut self) {
        self.selected = self.selected.saturating_sub(PAGE_STEP);
    }

    fn move_page_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = self
            .selected
            .saturating_add(PAGE_STEP)
            .min(self.rows.len().saturating_sub(1));
    }

    fn select_number(&mut self, n: u8) {
        let idx = usize::from(n.saturating_sub(1));
        if idx < self.rows.len() {
            self.selected = idx;
        }
    }

    fn accept(&mut self) {
        if let Some(row) = self.rows.get(self.selected) {
            self.accepted = Some(AcceptedAction::Drilldown(row.agent_id.clone()));
            self.completed = true;
        }
        // Enter hands ownership to the drilldown completion path.
        self.auto_dismiss_at = None;
    }

    /// User pressed `x` (or Delete) on the selected row.
    ///
    /// Queues a kill sentinel as a *pending action* — the dispatcher
    /// drains it via `take_pending_action` and routes to the spawner /
    /// task service, but the view STAYS OPEN. That way the user sees
    /// the row transition Live → Cancelling → Cancelled in real time
    /// and can kill additional rows without re-opening Ctrl+G.
    ///
    /// Only fires when the row is actually killable (Live or already
    /// Cancelling). Terminal rows (Completed / Failed / Cancelled) do
    /// nothing — there's nothing to kill, and an inert keypress should
    /// not silently dismiss the view.
    fn request_kill(&mut self) {
        if let Some(row) = self.rows.get(self.selected)
            && row.status.is_live()
        {
            self.pending_action = Some(format!("{AGENT_KILL_SENTINEL}{}", row.agent_id));
        }
        self.postpone_auto_dismiss_if_terminal();
    }
}

fn count_rows(rows: &[AgentRow]) -> (usize, usize) {
    let live_count = rows.iter().filter(|row| row.status.is_live()).count();
    let failed_count = rows.iter().filter(|row| row.status.is_failed()).count();
    (live_count, failed_count)
}

fn terminal_auto_dismiss_at(live_count: usize, has_rows: bool) -> Option<std::time::Instant> {
    (has_rows && live_count == 0).then(|| std::time::Instant::now() + AUTO_DISMISS_GRACE)
}

#[derive(Clone)]
struct FanoutHeader {
    title: String,
    target_count: usize,
    running: usize,
    done: usize,
    failed: usize,
    stopped: usize,
}

enum AgentListEntry<'a> {
    FanoutHeader(FanoutHeader),
    Row {
        row_idx: usize,
        row: &'a AgentRow,
        grouped: bool,
    },
}

impl AgentListEntry<'_> {
    fn row_index(&self) -> Option<usize> {
        match self {
            AgentListEntry::FanoutHeader(_) => None,
            AgentListEntry::Row { row_idx, .. } => Some(*row_idx),
        }
    }
}

fn agent_list_entries(rows: &[AgentRow]) -> Vec<AgentListEntry<'_>> {
    let mut entries = Vec::with_capacity(rows.len());
    let mut rendered = vec![false; rows.len()];

    for idx in 0..rows.len() {
        if rendered[idx] {
            continue;
        }

        let Some(fanout) = rows[idx].fanout.as_ref() else {
            rendered[idx] = true;
            entries.push(AgentListEntry::Row {
                row_idx: idx,
                row: &rows[idx],
                grouped: false,
            });
            continue;
        };

        let member_indices = rows
            .iter()
            .enumerate()
            .filter_map(|(member_idx, row)| {
                row.fanout
                    .as_ref()
                    .is_some_and(|member| member.group_id == fanout.group_id)
                    .then_some(member_idx)
            })
            .collect::<Vec<_>>();
        entries.push(AgentListEntry::FanoutHeader(fanout_header(
            fanout,
            &member_indices,
            rows,
        )));
        for member_idx in member_indices {
            rendered[member_idx] = true;
            entries.push(AgentListEntry::Row {
                row_idx: member_idx,
                row: &rows[member_idx],
                grouped: true,
            });
        }
    }

    entries
}

fn fanout_header(
    fanout: &AgentFanoutMembership,
    member_indices: &[usize],
    rows: &[AgentRow],
) -> FanoutHeader {
    let mut header = FanoutHeader {
        title: if fanout.group_title.trim().is_empty() {
            fanout.group_id.clone()
        } else {
            fanout.group_title.clone()
        },
        target_count: fanout.target_count,
        running: 0,
        done: 0,
        failed: 0,
        stopped: 0,
    };

    for row in member_indices.iter().filter_map(|idx| rows.get(*idx)) {
        match row.status {
            AgentRowStatus::Live | AgentRowStatus::Cancelling => header.running += 1,
            AgentRowStatus::Completed => header.done += 1,
            AgentRowStatus::Failed => header.failed += 1,
            AgentRowStatus::Cancelled => header.stopped += 1,
        }
    }

    header
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs}s")
    }
}

const PAGE_STEP: usize = 8;

impl BottomPaneView for InFlightAgentsView {
    fn pre_draw_tick(&mut self, now: std::time::Instant) {
        // Auto-dismiss: if all rows are terminal and the grace period
        // has elapsed, mark the view complete so the event loop pops it.
        if let Some(dismiss_at) = self.auto_dismiss_at {
            if now >= dismiss_at && self.live_count == 0 {
                self.completed = true;
                self.auto_dismiss_at = None;
            }
        }
    }

    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let dim = Style::default().fg(Color::DarkGray);
        let title_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);

        // Header
        let live = self.live_count;
        let failed = self.failed_count;
        let done = self.rows.len().saturating_sub(live + failed);
        let header_text = if self.rows.is_empty() {
            "  Agents".to_string()
        } else if failed > 0 {
            format!("  Agents · {live} working · {done} done · {failed} failed")
        } else if done > 0 {
            format!("  Agents · {live} working · {done} done")
        } else {
            format!("  Agents · {live} working")
        };
        let header = Line::from(Span::styled(header_text, title_style));
        buf.set_line(area.x, area.y, &header, area.width);

        if self.rows.is_empty() {
            let empty = Line::from(Span::styled("  No agents working.".to_string(), dim));
            if area.height >= 2 {
                buf.set_line(area.x, area.y + 1, &empty, area.width);
            }
            return;
        }

        let body_y = area.y + 1;
        let body_h = area.height.saturating_sub(1) as usize;
        let entries = agent_list_entries(&self.rows);
        let selected_entry = entries
            .iter()
            .position(|entry| entry.row_index() == Some(self.selected))
            .unwrap_or(0);
        let window_start = selected_entry.saturating_add(1).saturating_sub(body_h);
        for (i, entry) in entries.iter().skip(window_start).take(body_h).enumerate() {
            let line = match entry {
                AgentListEntry::FanoutHeader(header) => fanout_header_line(header, dim),
                AgentListEntry::Row {
                    row_idx,
                    row,
                    grouped,
                } => {
                    let selected = *row_idx == self.selected;
                    let marker = if selected { "› " } else { "  " };
                    let status_color = row.status.color();
                    let meta = row_meta(row);
                    let label = if *grouped {
                        fanout_slot_row_label(*row_idx, row)
                    } else {
                        format!("{}. {}", row_idx + 1, truncate_label(&row.name, 38))
                    };
                    let mut spans = vec![
                        Span::styled(
                            marker.to_string(),
                            if selected {
                                Style::default()
                                    .fg(Color::Cyan)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                dim
                            },
                        ),
                        Span::styled(
                            label,
                            if selected {
                                Style::default()
                                    .fg(status_color)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(status_color)
                            },
                        ),
                        Span::styled(format!("  · {meta}"), dim),
                    ];
                    if *grouped {
                        spans.push(Span::styled(
                            format!(" · {}", truncate_label(&row.agent_id, 18)),
                            dim,
                        ));
                    }
                    Line::from(spans)
                }
            };
            buf.set_line(area.x, body_y + i as u16, &line, area.width);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let rows = agent_list_entries(&self.rows).len().max(1);
        (rows as u16).saturating_add(1).min(10)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        // Interaction should give the user another short look, not pin
        // a terminal board forever. Live boards stay open until rows
        // settle; terminal boards disappear shortly after input stops.
        self.postpone_auto_dismiss_if_terminal();
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::PageUp => self.move_page_up(),
            KeyCode::PageDown => self.move_page_down(),
            KeyCode::Home => self.selected = 0,
            KeyCode::End if !self.rows.is_empty() => self.selected = self.rows.len() - 1,
            KeyCode::Char(ch) if ('1'..='9').contains(&ch) => self.select_number(ch as u8 - b'0'),
            KeyCode::Enter => self.accept(),
            // Kill the selected live agent. `x` is the conventional
            // "kill"/"close" gesture in dashboard-style TUIs; `Delete`
            // is the keyboard-discoverable equivalent.
            KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Delete => self.request_kill(),
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                self.completed = true;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn completion(&self) -> Option<super::view::ViewCompletion> {
        if !self.completed {
            return None;
        }
        match self.accepted.as_ref()? {
            AcceptedAction::Drilldown(id) => Some(super::view::ViewCompletion {
                result: Some(format!("{AGENT_DRILLDOWN_SENTINEL}{id}")),
                reopen: None,
            }),
        }
    }

    fn take_pending_action(&mut self) -> Option<String> {
        self.pending_action.take()
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn refresh_agent_rows(&mut self, rows: Vec<AgentRow>) -> bool {
        self.replace_rows(rows);
        true
    }

    fn accepts_agent_rows(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        Some("↑↓ move · Enter open · X stop · Esc close".into())
    }
}

use crate::cli::effects::truncate_label;

fn step_label(child_count: usize) -> String {
    if child_count == 0 {
        String::new()
    } else if child_count == 1 {
        "1 step · ".to_string()
    } else {
        format!("{child_count} steps · ")
    }
}

fn row_meta(row: &AgentRow) -> String {
    let mut parts = Vec::new();
    if let Some(status) = row.status.phrase() {
        parts.push(status.to_string());
    }
    let steps = step_label(row.child_count);
    if !steps.is_empty() {
        parts.push(steps.trim_end_matches(" · ").to_string());
    }
    parts.push(format_elapsed(row.elapsed_ms));
    parts.join(" · ")
}

fn fanout_header_line(header: &FanoutHeader, dim: Style) -> Line<'static> {
    let mut parts = vec![format!("{} target", header.target_count)];
    if header.running > 0 {
        parts.push(format!("{} running", header.running));
    }
    if header.done > 0 {
        parts.push(format!("{} done", header.done));
    }
    if header.failed > 0 {
        parts.push(format!("{} failed", header.failed));
    }
    if header.stopped > 0 {
        parts.push(format!("{} stopped", header.stopped));
    }

    Line::from(vec![
        Span::styled("  ▣ ".to_string(), dim),
        Span::styled(
            truncate_label(&header.title, 30),
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", parts.join(" · ")), dim),
    ])
}

fn fanout_slot_row_label(row_idx: usize, row: &AgentRow) -> String {
    let Some(fanout) = row.fanout.as_ref() else {
        return format!("{}. {}", row_idx + 1, truncate_label(&row.name, 38));
    };
    let label = if fanout.slot_label.trim().is_empty() {
        row.name.as_str()
    } else {
        fanout.slot_label.as_str()
    };
    format!(
        "{}. slot {}: {}",
        row_idx + 1,
        fanout.slot_index + 1,
        truncate_label(label, 30)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::widgets::Widget;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn rows(n: usize) -> Vec<AgentRow> {
        (0..n)
            .map(|i| AgentRow {
                agent_id: format!("agent-{i}"),
                name: format!("task {i}"),
                child_count: i,
                elapsed_ms: 1000 * (i as u64 + 1),
                status: AgentRowStatus::Live,
                fanout: None,
            })
            .collect()
    }

    fn fanout(group_id: &str, target_count: usize, slot_index: usize) -> AgentFanoutMembership {
        AgentFanoutMembership {
            group_id: group_id.to_string(),
            group_title: "review fanout".to_string(),
            target_count,
            slot_index,
            slot_label: format!("slot task {slot_index}"),
        }
    }

    fn render(view: &InFlightAgentsView, width: u16, height: u16) -> String {
        struct ViewWidget<'a>(&'a InFlightAgentsView);
        impl Widget for ViewWidget<'_> {
            fn render(self, area: Rect, buf: &mut Buffer) {
                self.0.render(area, buf);
            }
        }
        buffer_to_string(&draw_widget(ViewWidget(view), width, height))
    }

    /// Empty agent list: must not panic, must not select anything.
    #[test]
    fn empty_list_is_inert() {
        let mut v = InFlightAgentsView::new(vec![]);
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Up));
        v.handle_key(key(KeyCode::Enter));
        // Enter on empty must not complete the view with a result.
        assert!(v.completion().is_none());
        // Esc completes without a result (just dismisses).
        v.handle_key(key(KeyCode::Esc));
        assert!(v.is_complete());
        assert!(v.completion().is_none());
    }

    /// Down/Up arrow navigation wraps correctly in both directions.
    #[test]
    fn navigation_wraps() {
        let mut v = InFlightAgentsView::new(rows(3));
        assert_eq!(v.selected, 0);
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.selected, 1);
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Down)); // wraps to 0
        assert_eq!(v.selected, 0);
        v.handle_key(key(KeyCode::Up)); // wraps to 2
        assert_eq!(v.selected, 2);
    }

    /// Enter on a row produces the sentinel-prefixed agent_id.
    #[test]
    fn enter_emits_sentinel_with_agent_id() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Enter));
        assert!(v.is_complete());
        let completion = v.completion().unwrap();
        assert_eq!(
            completion.result.as_deref(),
            Some("__agent_drilldown__\nagent-1")
        );
    }

    /// `x` on a Live row emits the kill sentinel as a *pending action*
    /// — the view STAYS OPEN so the user keeps watching the row
    /// transition Live → Cancelling → Cancelled, and can kill more
    /// agents in the same Ctrl+G session without re-opening the view.
    ///
    /// Pre-fix: `x` set `completed=true`, dropping the user back into
    /// the chat with no visibility into whether the cancel landed.
    #[test]
    fn x_on_live_row_emits_kill_sentinel_via_pending_action_and_stays_open() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char('x')));

        let pending = v.take_pending_action();
        assert_eq!(
            pending.as_deref(),
            Some("__agent_kill__\nagent-1"),
            "x on live row must emit kill sentinel as a pending action"
        );
        // Drained: a second poll returns None.
        assert!(v.take_pending_action().is_none());
        // Critical: the view is NOT complete — the user keeps watching.
        assert!(
            !v.is_complete(),
            "x on live row must NOT close the drill view"
        );
        assert!(
            v.completion().is_none(),
            "view must not produce a completion until the user explicitly dismisses"
        );
    }

    #[test]
    fn delete_key_also_emits_kill_sentinel_via_pending_action() {
        let mut v = InFlightAgentsView::new(rows(2));
        v.handle_key(key(KeyCode::Delete));
        let pending = v.take_pending_action();
        assert_eq!(pending.as_deref(), Some("__agent_kill__\nagent-0"));
        assert!(!v.is_complete());
    }

    #[test]
    fn x_on_terminal_row_is_inert_and_keeps_view_open() {
        // Pressing x on a row that already finished (Completed/Failed/
        // Cancelled) must NOT emit a kill AND must not close the view.
        let mut rows = rows(3);
        rows[0].status = AgentRowStatus::Completed;
        rows[1].status = AgentRowStatus::Failed;
        rows[2].status = AgentRowStatus::Cancelled;
        let mut v = InFlightAgentsView::new(rows);
        for _ in 0..3 {
            v.handle_key(key(KeyCode::Char('x')));
            v.handle_key(key(KeyCode::Down));
        }
        assert!(
            !v.is_complete(),
            "x on terminal rows must not complete view"
        );
        assert!(v.take_pending_action().is_none());
    }

    #[test]
    fn x_on_cancelling_row_re_issues_kill() {
        // Re-pressing x while a row is mid-cancel is harmless and gives
        // the user a way to nudge a stuck cancel — emit again, view
        // stays open.
        let mut rs = rows(2);
        rs[0].status = AgentRowStatus::Cancelling;
        let mut v = InFlightAgentsView::new(rs);
        v.handle_key(key(KeyCode::Char('x')));
        let pending = v.take_pending_action();
        assert_eq!(pending.as_deref(), Some("__agent_kill__\nagent-0"));
        assert!(!v.is_complete());
    }

    #[test]
    fn x_can_be_invoked_repeatedly_in_the_same_session() {
        // Multiple kills in one Ctrl+G session.
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Char('x'))); // selected=0
        let p1 = v.take_pending_action();
        assert_eq!(p1.as_deref(), Some("__agent_kill__\nagent-0"));
        assert!(!v.is_complete());

        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char('x'))); // selected=1
        let p2 = v.take_pending_action();
        assert_eq!(p2.as_deref(), Some("__agent_kill__\nagent-1"));
        assert!(!v.is_complete());
    }

    #[test]
    fn esc_closes_view_after_kill() {
        // After a kill, Esc still cleanly dismisses the view.
        let mut v = InFlightAgentsView::new(rows(2));
        v.handle_key(key(KeyCode::Char('x')));
        let _ = v.take_pending_action();
        v.handle_key(key(KeyCode::Esc));
        assert!(v.is_complete());
        assert!(v.completion().is_none());
    }

    #[test]
    fn enter_after_kill_drills_into_selected_row() {
        // Enter after kill must still open detail view.
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char('x')));
        let _ = v.take_pending_action();
        v.handle_key(key(KeyCode::Enter));
        assert!(v.is_complete());
        let completion = v.completion().unwrap();
        assert_eq!(
            completion.result.as_deref(),
            Some("__agent_drilldown__\nagent-1")
        );
    }

    #[test]
    fn parse_kill_sentinel_extracts_id() {
        let s = format!("{AGENT_KILL_SENTINEL}reviewer@abc12345");
        assert_eq!(parse_kill_sentinel(&s), Some("reviewer@abc12345"));
        assert_eq!(parse_kill_sentinel("not a sentinel"), None);
        assert_eq!(parse_kill_sentinel(AGENT_KILL_SENTINEL), None);
    }

    #[test]
    fn drilldown_and_kill_sentinels_are_disjoint() {
        // The two sentinels MUST share no prefix — otherwise a parser
        // bug could route a kill to drilldown or vice versa.
        assert!(!AGENT_DRILLDOWN_SENTINEL.starts_with(AGENT_KILL_SENTINEL));
        assert!(!AGENT_KILL_SENTINEL.starts_with(AGENT_DRILLDOWN_SENTINEL));
    }

    /// Ctrl+C dismisses the view without producing a selection.
    #[test]
    fn ctrl_c_dismisses() {
        let mut v = InFlightAgentsView::new(rows(2));
        let ev = v.on_ctrl_c();
        assert!(matches!(ev, CancellationEvent::Consumed));
        assert!(v.is_complete());
        // Dismissed: no result emitted.
        assert!(v.completion().is_none());
    }

    /// hjkl vim-style nav also works (alias for arrow keys).
    #[test]
    fn vim_keys_navigate() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Char('j')));
        assert_eq!(v.selected, 1);
        v.handle_key(key(KeyCode::Char('k')));
        assert_eq!(v.selected, 0);
    }

    #[test]
    fn paging_and_number_jump_navigate_long_agent_lists() {
        let mut v = InFlightAgentsView::new(rows(12));
        v.handle_key(key(KeyCode::PageDown));
        assert_eq!(v.selected, 8);
        v.handle_key(key(KeyCode::PageDown));
        assert_eq!(v.selected, 11);
        v.handle_key(key(KeyCode::PageUp));
        assert_eq!(v.selected, 3);
        v.handle_key(key(KeyCode::Char('7')));
        assert_eq!(v.selected, 6);
        v.handle_key(key(KeyCode::Char('9')));
        assert_eq!(v.selected, 8);
    }

    #[test]
    fn refresh_agent_rows_recomputes_counts_and_preserves_selection() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.rows[v.selected].agent_id, "agent-1");

        let mut updated = rows(3);
        updated[0].status = AgentRowStatus::Completed;
        updated[1].status = AgentRowStatus::Failed;
        updated[2].status = AgentRowStatus::Cancelled;
        assert!(v.refresh_agent_rows(updated));

        assert_eq!(v.rows[v.selected].agent_id, "agent-1");
        assert_eq!(v.live_count, 0);
        assert_eq!(v.failed_count, 1);
    }

    #[test]
    fn render_groups_fanout_rows_under_header() {
        let mut rows = rows(4);
        rows[0].fanout = Some(fanout("review-1", 3, 0));
        rows[1].fanout = Some(fanout("review-1", 3, 1));
        rows[2].fanout = Some(fanout("review-1", 3, 2));
        rows[1].status = AgentRowStatus::Failed;
        rows[2].status = AgentRowStatus::Completed;

        let out = render(&InFlightAgentsView::new(rows), 100, 7);

        assert!(out.contains("review fanout"), "{out}");
        assert!(out.contains("3 target"), "{out}");
        assert!(out.contains("1 running"), "{out}");
        assert!(out.contains("1 done"), "{out}");
        assert!(out.contains("1 failed"), "{out}");
        assert!(out.contains("1. slot 1: slot task 0"), "{out}");
        assert!(out.contains("2. slot 2: slot task 1"), "{out}");
        assert!(out.contains("3. slot 3: slot task 2"), "{out}");
        assert!(out.contains("agent-0"), "{out}");
        assert!(out.contains("4. task 3"), "{out}");
    }

    #[test]
    fn fanout_group_header_is_not_selectable() {
        let mut rows = rows(2);
        rows[0].fanout = Some(fanout("review-1", 2, 0));
        rows[1].fanout = Some(fanout("review-1", 2, 1));

        let mut v = InFlightAgentsView::new(rows);
        let out = render(&v, 100, 4);
        assert!(out.contains("review fanout"), "{out}");
        assert!(out.contains("› 1. slot 1"), "{out}");

        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(
            v.completion().and_then(|completion| completion.result),
            Some("__agent_drilldown__\nagent-1".to_string())
        );
    }

    #[test]
    fn refresh_agent_rows_preserves_selection_for_grouped_rows() {
        let mut initial_rows = rows(3);
        initial_rows[0].fanout = Some(fanout("review-1", 3, 0));
        initial_rows[1].fanout = Some(fanout("review-1", 3, 1));
        initial_rows[2].fanout = Some(fanout("review-1", 3, 2));
        let mut v = InFlightAgentsView::new(initial_rows);
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.rows[v.selected].agent_id, "agent-1");

        let mut updated = rows(3);
        updated[0].fanout = Some(fanout("review-1", 3, 0));
        updated[1].fanout = Some(fanout("review-1", 3, 1));
        updated[2].fanout = Some(fanout("review-1", 3, 2));
        updated[1].status = AgentRowStatus::Completed;
        assert!(v.refresh_agent_rows(updated));

        assert_eq!(v.rows[v.selected].agent_id, "agent-1");
        let out = render(&v, 100, 5);
        assert!(out.contains("1 done"), "{out}");
        assert!(out.contains("› 2. slot 2"), "{out}");
    }

    /// Truncation: char-aware, multi-byte safe.
    #[test]
    fn truncate_handles_cjk() {
        // Should not panic on multi-byte boundaries.
        let s = "日本語のとても長いタスク説明".repeat(3);
        let result = truncate_label(&s, 10);
        assert!(result.chars().count() <= 10);
    }

    #[test]
    fn parse_drilldown_sentinel_extracts_id() {
        let s = format!("{AGENT_DRILLDOWN_SENTINEL}reviewer@abc12345");
        assert_eq!(parse_drilldown_sentinel(&s), Some("reviewer@abc12345"));
    }

    #[test]
    fn parse_drilldown_sentinel_rejects_unprefixed() {
        assert_eq!(parse_drilldown_sentinel("not a sentinel"), None);
    }

    /// Defensive: if a malformed caller embeds a newline AFTER the
    /// id (carrying trailing garbage), only the first segment is
    /// dispatched. Prevents a hostile or buggy producer from feeding
    /// the dispatcher unexpected payload.
    #[test]
    fn parse_drilldown_sentinel_strips_trailing_newline_garbage() {
        let s = format!("{AGENT_DRILLDOWN_SENTINEL}reviewer@abc\nGARBAGE\nMORE");
        assert_eq!(parse_drilldown_sentinel(&s), Some("reviewer@abc"));
    }

    #[test]
    fn parse_drilldown_sentinel_tolerates_separator_newline() {
        let s = format!("{AGENT_DRILLDOWN_SENTINEL}\nreviewer@abc\nGARBAGE");
        assert_eq!(parse_drilldown_sentinel(&s), Some("reviewer@abc"));
    }

    #[test]
    fn parse_drilldown_sentinel_rejects_empty_id() {
        let s = AGENT_DRILLDOWN_SENTINEL.to_string();
        assert_eq!(parse_drilldown_sentinel(&s), None);
    }

    #[test]
    fn render_uses_calmer_agents_header_and_hint_copy() {
        let v = InFlightAgentsView::new(rows(2));
        let out = render(&v, 80, 4);
        assert!(out.contains("Agents · 2 working"), "{out}");
        assert!(!out.contains("SUBAGENTS"), "{out}");
        assert!(!out.contains("0 steps"), "{out}");
        assert_eq!(
            v.hint_keys().as_deref(),
            Some("↑↓ move · Enter open · X stop · Esc close")
        );
    }

    #[test]
    fn render_uses_meta_words_for_terminal_and_cancelling_rows() {
        let mut rows = rows(4);
        rows[0].status = AgentRowStatus::Cancelling;
        rows[1].status = AgentRowStatus::Completed;
        rows[2].status = AgentRowStatus::Failed;
        rows[3].status = AgentRowStatus::Cancelled;
        let out = render(&InFlightAgentsView::new(rows), 80, 6);
        assert!(out.contains("stopping"), "{out}");
        assert!(out.contains("done"), "{out}");
        assert!(out.contains("failed"), "{out}");
        assert!(out.contains("stopped"), "{out}");
    }
}
