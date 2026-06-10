//! Background task switcher.
//!
//! Opened from the status-line task chip. It gives the user a typed,
//! keyboard-driven view over local background shell tasks promoted by
//! Ctrl+B, including detail/tail inspection and stop actions.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::view::{BottomPaneView, CancellationEvent};
use crate::cli::effects::truncate_label;

pub(crate) const BACKGROUND_TASK_STOP_SENTINEL: &str = "__background_task_stop__\n";
pub(crate) const BACKGROUND_TASK_OUTPUT_SENTINEL: &str = "__background_task_output__\n";

pub(crate) fn parse_stop_sentinel(s: &str) -> Option<&str> {
    s.strip_prefix(BACKGROUND_TASK_STOP_SENTINEL)
        .map(|rest| {
            let rest = rest.trim_start_matches('\n');
            rest.split_once('\n').map(|(id, _)| id).unwrap_or(rest)
        })
        .filter(|id| !id.is_empty())
}

pub(crate) fn parse_output_sentinel(s: &str) -> Option<&str> {
    s.strip_prefix(BACKGROUND_TASK_OUTPUT_SENTINEL)
        .map(|rest| {
            let rest = rest.trim_start_matches('\n');
            rest.split_once('\n').map(|(id, _)| id).unwrap_or(rest)
        })
        .filter(|id| !id.is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundTaskKind {
    Shell,
    LocalAgent,
    CloudSession,
    MainSession,
    Monitor,
}

impl BackgroundTaskKind {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::LocalAgent => "local agent",
            Self::CloudSession => "cloud session",
            Self::MainSession => "main session",
            Self::Monitor => "monitor",
        }
    }

    fn supports_output_action(self) -> bool {
        matches!(self, Self::Shell)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BackgroundTaskStatus {
    Pending,
    WaitingForInput,
    Failed,
    Running,
    Killed,
    Completed,
    Unavailable,
}

impl BackgroundTaskStatus {
    pub(crate) fn from_str(value: &str) -> Self {
        match value {
            "pending" => Self::Pending,
            "waiting_for_input" => Self::WaitingForInput,
            "failed" => Self::Failed,
            "running" => Self::Running,
            "killed" => Self::Killed,
            "completed" => Self::Completed,
            "unavailable" => Self::Unavailable,
            _ => Self::Unavailable,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForInput => "waiting_for_input",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Killed => "killed",
            Self::Completed => "completed",
            Self::Unavailable => "unavailable",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::WaitingForInput => "needs input",
            Self::Failed => "failed",
            Self::Running => "running",
            Self::Killed => "stopped",
            Self::Completed => "completed",
            Self::Unavailable => "unavailable",
        }
    }

    fn color(self) -> Color {
        match self {
            Self::Pending => Color::Blue,
            Self::WaitingForInput => Color::Yellow,
            Self::Failed => Color::Red,
            Self::Running => Color::Cyan,
            Self::Completed => Color::Green,
            Self::Killed | Self::Unavailable => Color::DarkGray,
        }
    }

    fn is_killable(self) -> bool {
        matches!(self, Self::Running | Self::WaitingForInput)
    }

    fn attention_rank(self) -> u8 {
        match self {
            Self::WaitingForInput | Self::Failed => 0,
            Self::Running | Self::Pending => 1,
            Self::Killed => 2,
            Self::Completed => 3,
            Self::Unavailable => 4,
        }
    }

    fn empty_output_state(self) -> &'static str {
        match self {
            Self::Pending => "Pending · no output yet",
            Self::WaitingForInput => "Waiting for input · no output yet",
            Self::Running => "No output yet · still running",
            Self::Completed => "Completed with no output",
            Self::Failed => "Failed with no output",
            Self::Killed => "Stopped with no output",
            Self::Unavailable => "Unavailable · stale handle or unsupported runner",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LiveControlState {
    Available,
    StaleHandle,
    UnsupportedInMode,
}

impl LiveControlState {
    fn label(self) -> Option<&'static str> {
        match self {
            Self::Available => None,
            Self::StaleHandle => Some("stale handle"),
            Self::UnsupportedInMode => Some("control unavailable"),
        }
    }

    fn can_stop(self) -> bool {
        matches!(self, Self::Available)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundTaskFanoutMembership {
    pub group_id: String,
    pub group_title: String,
    pub target_count: usize,
    pub slot_index: usize,
    pub slot_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BackgroundTaskRow {
    pub id: String,
    pub kind: BackgroundTaskKind,
    pub status: BackgroundTaskStatus,
    pub live_control: LiveControlState,
    pub elapsed_ms: u64,
    pub started_at_ms: Option<u64>,
    pub ended_at_ms: Option<u64>,
    pub title: String,
    pub output_ref: Option<String>,
    pub output_tail: Option<String>,
    pub output_offset: Option<u64>,
    pub total_bytes: Option<u64>,
    pub total_lines: Option<u64>,
    pub exit_code: Option<i32>,
    pub terminal_reason: Option<String>,
    pub fanout: Option<BackgroundTaskFanoutMembership>,
}

impl BackgroundTaskRow {
    pub(crate) fn new(
        id: impl Into<String>,
        kind: BackgroundTaskKind,
        status: impl AsRef<str>,
        elapsed_ms: u64,
        title: impl Into<String>,
        output_ref: Option<String>,
        output_tail: Option<String>,
        total_bytes: Option<u64>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            status: BackgroundTaskStatus::from_str(status.as_ref()),
            live_control: LiveControlState::Available,
            elapsed_ms,
            started_at_ms: None,
            ended_at_ms: None,
            title: title.into(),
            output_ref,
            output_tail,
            output_offset: None,
            total_bytes,
            total_lines: None,
            exit_code: None,
            terminal_reason: None,
            fanout: None,
        }
    }

    pub(crate) fn shell(
        id: impl Into<String>,
        status: impl AsRef<str>,
        elapsed_ms: u64,
        title: impl Into<String>,
        output_ref: Option<String>,
        output_tail: Option<String>,
        total_bytes: Option<u64>,
    ) -> Self {
        Self::new(
            id,
            BackgroundTaskKind::Shell,
            status,
            elapsed_ms,
            title,
            output_ref,
            output_tail,
            total_bytes,
        )
    }

    pub(crate) fn with_live_control(mut self, live_control: LiveControlState) -> Self {
        self.live_control = live_control;
        if !live_control.can_stop()
            && matches!(
                self.status,
                BackgroundTaskStatus::Pending
                    | BackgroundTaskStatus::Running
                    | BackgroundTaskStatus::WaitingForInput
            )
        {
            self.status = BackgroundTaskStatus::Unavailable;
        }
        self
    }

    pub(crate) fn with_output_stats(
        mut self,
        output_offset: Option<u64>,
        total_lines: Option<u64>,
    ) -> Self {
        self.output_offset = output_offset;
        self.total_lines = total_lines;
        self
    }

    pub(crate) fn with_terminal(
        mut self,
        exit_code: Option<i32>,
        terminal_reason: Option<String>,
    ) -> Self {
        self.exit_code = exit_code;
        self.terminal_reason = terminal_reason;
        self
    }

    pub(crate) fn with_timing(
        mut self,
        started_at_ms: Option<u64>,
        ended_at_ms: Option<u64>,
    ) -> Self {
        self.started_at_ms = started_at_ms;
        self.ended_at_ms = ended_at_ms;
        self
    }

    pub(crate) fn with_fanout(mut self, fanout: BackgroundTaskFanoutMembership) -> Self {
        self.fanout = Some(fanout);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Detail,
}

pub(crate) struct BackgroundTaskView {
    rows: Vec<BackgroundTaskRow>,
    selected: usize,
    completed: bool,
    mode: Mode,
    pending_action: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FanoutHeader {
    title: String,
    target_count: usize,
    running: usize,
    done: usize,
    failed: usize,
    stopped: usize,
    unavailable: usize,
}

enum BackgroundTaskListEntry<'a> {
    FanoutHeader(FanoutHeader),
    Row {
        row_idx: usize,
        row: &'a BackgroundTaskRow,
        grouped: bool,
    },
}

impl BackgroundTaskListEntry<'_> {
    fn row_index(&self) -> Option<usize> {
        match self {
            Self::FanoutHeader(_) => None,
            Self::Row { row_idx, .. } => Some(*row_idx),
        }
    }
}

fn background_task_list_entries(rows: &[BackgroundTaskRow]) -> Vec<BackgroundTaskListEntry<'_>> {
    let mut entries = Vec::with_capacity(rows.len());
    let mut rendered = vec![false; rows.len()];

    for idx in 0..rows.len() {
        if rendered[idx] {
            continue;
        }

        let Some(fanout) = rows[idx].fanout.as_ref() else {
            rendered[idx] = true;
            entries.push(BackgroundTaskListEntry::Row {
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
        entries.push(BackgroundTaskListEntry::FanoutHeader(fanout_header(
            fanout,
            &member_indices,
            rows,
        )));
        for member_idx in member_indices {
            rendered[member_idx] = true;
            entries.push(BackgroundTaskListEntry::Row {
                row_idx: member_idx,
                row: &rows[member_idx],
                grouped: true,
            });
        }
    }

    entries
}

fn fanout_header(
    fanout: &BackgroundTaskFanoutMembership,
    member_indices: &[usize],
    rows: &[BackgroundTaskRow],
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
        unavailable: 0,
    };

    for row in member_indices.iter().filter_map(|idx| rows.get(*idx)) {
        match row.status {
            BackgroundTaskStatus::Pending
            | BackgroundTaskStatus::Running
            | BackgroundTaskStatus::WaitingForInput => header.running += 1,
            BackgroundTaskStatus::Completed => header.done += 1,
            BackgroundTaskStatus::Failed => header.failed += 1,
            BackgroundTaskStatus::Killed => header.stopped += 1,
            BackgroundTaskStatus::Unavailable => header.unavailable += 1,
        }
    }

    header
}

impl BackgroundTaskView {
    pub(crate) fn new(rows: Vec<BackgroundTaskRow>) -> Self {
        let mut view = Self {
            rows: sort_rows(rows),
            selected: 0,
            completed: false,
            mode: Mode::List,
            pending_action: None,
        };
        view.clamp_selection();
        view
    }

    pub(crate) fn new_with_selected(
        rows: Vec<BackgroundTaskRow>,
        selected_id: Option<&str>,
    ) -> Self {
        let mut view = Self::new(rows);
        if let Some(selected_id) = selected_id
            && let Some(idx) = view.rows.iter().position(|row| row.id == selected_id)
        {
            view.selected = idx;
        }
        view
    }

    pub(crate) fn replace_rows(&mut self, rows: Vec<BackgroundTaskRow>) {
        self.replace_rows_with_selected(rows, None);
    }

    pub(crate) fn replace_rows_with_selected(
        &mut self,
        rows: Vec<BackgroundTaskRow>,
        selected_id: Option<&str>,
    ) {
        let current_selected_id = self.rows.get(self.selected).map(|row| row.id.clone());
        let rows = sort_rows(rows);
        let selected_idx = selected_id.and_then(|id| rows.iter().position(|row| row.id == id));
        let fallback_idx = current_selected_id
            .and_then(|id| rows.iter().position(|row| row.id == id))
            .or_else(|| rows.first().map(|_| 0));
        self.selected = selected_idx
            .or(fallback_idx)
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        self.rows = rows;
        self.clamp_selection();
    }

    fn clamp_selection(&mut self) {
        if self.rows.is_empty() {
            self.selected = 0;
        } else {
            self.selected = self.selected.min(self.rows.len() - 1);
        }
    }

    fn selected_row(&self) -> Option<&BackgroundTaskRow> {
        self.rows.get(self.selected)
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

    fn request_stop(&mut self) {
        if let Some(row) = self.selected_row()
            && row.status.is_killable()
            && row.live_control.can_stop()
        {
            self.pending_action = Some(format!("{BACKGROUND_TASK_STOP_SENTINEL}{}", row.id));
        }
    }

    fn request_output(&mut self) {
        if let Some(row) = self.selected_row()
            && row.kind.supports_output_action()
        {
            self.pending_action = Some(format!("{BACKGROUND_TASK_OUTPUT_SENTINEL}{}", row.id));
        }
    }

    fn render_list(&self, area: Rect, buf: &mut Buffer) {
        let dim = Style::default().fg(Color::DarkGray);
        let title_style = Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
        let running = self
            .rows
            .iter()
            .filter(|row| row.status == BackgroundTaskStatus::Running)
            .count();
        let waiting = self
            .rows
            .iter()
            .filter(|row| row.status == BackgroundTaskStatus::WaitingForInput)
            .count();
        let failed = self
            .rows
            .iter()
            .filter(|row| row.status == BackgroundTaskStatus::Failed)
            .count();
        let header = if self.rows.is_empty() {
            "  Background tasks".to_string()
        } else {
            let mut parts = vec![format!("{} total", self.rows.len())];
            if running > 0 {
                parts.push(format!("{running} running"));
            }
            if waiting > 0 {
                parts.push(pluralize_with_count(waiting, "needs input", "need input"));
            }
            if failed > 0 {
                parts.push(format!("{failed} failed"));
            }
            format!("  Background tasks · {}", parts.join(" · "))
        };
        buf.set_line(
            area.x,
            area.y,
            &Line::from(Span::styled(header, title_style)),
            area.width,
        );

        if self.rows.is_empty() {
            if area.height >= 2 {
                buf.set_line(
                    area.x,
                    area.y + 1,
                    &Line::from(Span::styled("  No background tasks.", dim)),
                    area.width,
                );
            }
            return;
        }

        let body_y = area.y + 1;
        let body_h = area.height.saturating_sub(1) as usize;
        let entries = background_task_list_entries(&self.rows);
        let selected_entry = entries
            .iter()
            .position(|entry| entry.row_index() == Some(self.selected))
            .unwrap_or(0);
        let window_start = selected_entry.saturating_add(1).saturating_sub(body_h);
        for (i, entry) in entries.iter().skip(window_start).take(body_h).enumerate() {
            let line = match entry {
                BackgroundTaskListEntry::FanoutHeader(header) => fanout_header_line(header, dim),
                BackgroundTaskListEntry::Row {
                    row_idx,
                    row,
                    grouped,
                } => {
                    let selected = *row_idx == self.selected;
                    let marker_style = if selected {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        dim
                    };
                    let row_style = if selected {
                        Style::default()
                            .fg(row.status.color())
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(row.status.color())
                    };
                    let marker = if selected { "› " } else { "  " };
                    let index = format!("{}. ", row_idx + 1);
                    let meta = format!(
                        "{}  {}  {}  ",
                        row.kind.as_str(),
                        row.status.label(),
                        format_elapsed(row.elapsed_ms)
                    );
                    let title = if *grouped {
                        fanout_slot_title(row)
                    } else {
                        row.title.clone()
                    };
                    let (id, title) = compact_list_row_text(
                        row,
                        title.as_str(),
                        area.width as usize,
                        marker,
                        &index,
                        &meta,
                    );
                    Line::from(vec![
                        Span::styled(marker, marker_style),
                        Span::styled(index, row_style),
                        Span::styled(id, row_style),
                        Span::styled("  ", dim),
                        Span::styled(meta, dim),
                        Span::styled(title, row_style),
                    ])
                }
            };
            buf.set_line(area.x, body_y + i as u16, &line, area.width);
        }
    }

    fn render_detail(&self, area: Rect, buf: &mut Buffer) {
        let Some(row) = self.selected_row() else {
            self.render_list(area, buf);
            return;
        };
        let dim = Style::default().fg(Color::DarkGray);
        let title_style = Style::default()
            .fg(row.status.color())
            .add_modifier(Modifier::BOLD);
        let mut timing_parts = Vec::new();
        if let Some(started_at_ms) = row.started_at_ms {
            timing_parts.push(format!("started {}", format_timestamp_ms(started_at_ms)));
        }
        if let Some(ended_at_ms) = row.ended_at_ms {
            timing_parts.push(format!("ended {}", format_timestamp_ms(ended_at_ms)));
        }
        timing_parts.push(format!("elapsed {}", format_elapsed(row.elapsed_ms)));

        let mut lines = vec![
            Line::from(Span::styled(
                format!("  {} · {}", row.id, row.status.label()),
                title_style,
            )),
            Line::from(vec![
                Span::styled("  kind ", dim),
                Span::raw(row.kind.as_str()),
                Span::styled(" · ", dim),
                Span::raw(timing_parts.join(" · ")),
            ]),
            Line::from(vec![
                Span::styled("  command ", dim),
                Span::raw(row.title.clone()),
            ]),
        ];
        if let Some(total_bytes) = row.total_bytes {
            let mut output_parts = Vec::new();
            if let Some(offset) = row.output_offset {
                output_parts.push(format!("offset {offset} -> {total_bytes}"));
            }
            output_parts.push(format!("{total_bytes} bytes"));
            if let Some(total_lines) = row.total_lines {
                output_parts.push(pluralize_with_count(total_lines as usize, "line", "lines"));
            }
            lines.push(Line::from(vec![
                Span::styled("  output ", dim),
                Span::raw(output_parts.join(" · ")),
            ]));
        }
        if let Some(exit_code) = row.exit_code {
            lines.push(Line::from(vec![
                Span::styled("  exit ", dim),
                Span::raw(exit_code.to_string()),
            ]));
        }
        if let Some(reason) = row.terminal_reason.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("  reason ", dim),
                Span::raw(reason.to_string()),
            ]));
        }
        if let Some(label) = row.live_control.label() {
            lines.push(Line::from(vec![
                Span::styled("  control ", dim),
                Span::raw(label.to_string()),
            ]));
        }
        if let Some(output_ref) = row.output_ref.as_deref() {
            lines.push(Line::from(vec![
                Span::styled("  ref ", dim),
                Span::raw(output_ref.to_string()),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled("  Tail", dim)));
        let tail = row
            .output_tail
            .as_deref()
            .map(str::trim_end)
            .filter(|tail| !tail.is_empty())
            .unwrap_or_else(|| row.status.empty_output_state());
        for line in tail.lines().take(DETAIL_TAIL_LINES) {
            lines.push(Line::from(format!("  {}", line)));
        }
        if row.status.is_killable() && row.live_control.can_stop() {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                detail_actions_label(row.kind.supports_output_action(), true, area.width as usize),
                dim,
            )));
        } else {
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                detail_actions_label(
                    row.kind.supports_output_action(),
                    false,
                    area.width as usize,
                ),
                dim,
            )));
        }

        for (i, line) in lines.into_iter().take(area.height as usize).enumerate() {
            buf.set_line(area.x, area.y + i as u16, &line, area.width);
        }
    }
}

impl BottomPaneView for BackgroundTaskView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        match self.mode {
            Mode::List => self.render_list(area, buf),
            Mode::Detail => self.render_detail(area, buf),
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        match self.mode {
            Mode::List => (background_task_list_entries(&self.rows).len().max(1) as u16)
                .saturating_add(1)
                .min(10),
            Mode::Detail => 14,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match self.mode {
            Mode::List => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_up(),
                KeyCode::Down | KeyCode::Char('j') => self.move_down(),
                KeyCode::PageUp => self.move_page_up(),
                KeyCode::PageDown => self.move_page_down(),
                KeyCode::Home => self.selected = 0,
                KeyCode::End if !self.rows.is_empty() => self.selected = self.rows.len() - 1,
                KeyCode::Char(ch) if ('1'..='9').contains(&ch) => {
                    let idx = ch as usize - '1' as usize;
                    if idx < self.rows.len() {
                        self.selected = idx;
                    }
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('o') | KeyCode::Char('O') => {
                    if !self.rows.is_empty() {
                        self.mode = Mode::Detail;
                    }
                }
                KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('x') | KeyCode::Delete => {
                    self.request_stop();
                }
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => self.completed = true,
                _ => {}
            },
            Mode::Detail => match key.code {
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('o') | KeyCode::Char('O') => {
                    self.request_output();
                }
                KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Char('x') | KeyCode::Delete => {
                    self.request_stop();
                }
                KeyCode::Esc | KeyCode::Left => self.mode = Mode::List,
                KeyCode::Char('q') => self.completed = true,
                _ => {}
            },
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn take_pending_action(&mut self) -> Option<String> {
        self.pending_action.take()
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn refresh_background_task_rows(&mut self, rows: Vec<BackgroundTaskRow>) -> bool {
        self.replace_rows(rows);
        true
    }

    fn refresh_background_task_rows_selecting(
        &mut self,
        rows: Vec<BackgroundTaskRow>,
        selected_id: Option<&str>,
    ) -> bool {
        self.replace_rows_with_selected(rows, selected_id);
        true
    }

    fn accepts_background_task_rows(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        match self.mode {
            Mode::List => Some("↑↓ move · Enter details · S stop · Esc close".into()),
            Mode::Detail => Some("S stop · Esc list · Q close".into()),
        }
    }

    fn reserve_status_footer(&self) -> bool {
        true
    }
}

fn sort_rows(mut rows: Vec<BackgroundTaskRow>) -> Vec<BackgroundTaskRow> {
    rows.sort_by_key(|row| (row.status.attention_rank(), row.elapsed_ms));
    rows
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
    if header.unavailable > 0 {
        parts.push(format!("{} unavailable", header.unavailable));
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

fn fanout_slot_title(row: &BackgroundTaskRow) -> String {
    let Some(fanout) = row.fanout.as_ref() else {
        return row.title.clone();
    };
    let label = if fanout.slot_label.trim().is_empty() {
        row.title.as_str()
    } else {
        fanout.slot_label.as_str()
    };
    format!("slot {}: {}", fanout.slot_index + 1, label)
}

fn compact_list_row_text(
    row: &BackgroundTaskRow,
    title: &str,
    width: usize,
    marker: &str,
    index: &str,
    meta: &str,
) -> (String, String) {
    let min_title = if width >= 44 {
        10
    } else if width >= 34 {
        6
    } else {
        0
    };
    let fixed_without_id_or_title =
        marker.chars().count() + index.chars().count() + 2 + meta.chars().count();
    let max_id = if width >= 72 {
        24
    } else if width >= 44 {
        14
    } else {
        9
    };
    let id_budget = width
        .saturating_sub(fixed_without_id_or_title + min_title)
        .min(max_id);
    let id = truncate_label(&row.id, id_budget);
    let used = fixed_without_id_or_title + id.chars().count();
    let title_budget = width.saturating_sub(used);
    let title = truncate_label(title, title_budget);
    (id, title)
}

fn detail_actions_label(can_output: bool, can_stop: bool, width: usize) -> &'static str {
    match (can_output, can_stop, width) {
        (true, true, 34..) => "  actions: output · stop · return",
        (true, true, 24..) => "  actions: output · stop",
        (true, true, _) => "  actions: output",
        (true, false, 26..) => "  actions: output · return",
        (true, false, _) => "  actions: output",
        (false, true, 26..) => "  actions: stop · return",
        (false, true, _) => "  actions: stop",
        (false, false, _) => "  actions: return",
    }
}

fn pluralize_with_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
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

fn format_timestamp_ms(ms: u64) -> String {
    let Ok(ms) = i64::try_from(ms) else {
        return format!("{ms}ms");
    };
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|dt| dt.format("%Y-%m-%d %H:%M:%SZ").to_string())
        .unwrap_or_else(|| format!("{ms}ms"))
}

const PAGE_STEP: usize = 8;
const DETAIL_TAIL_LINES: usize = 8;

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

    fn row(id: &str, status: &str, title: &str) -> BackgroundTaskRow {
        BackgroundTaskRow::shell(
            id,
            status,
            1500,
            title,
            Some(format!("/tmp/{id}.stdout")),
            Some("line one\nline two".to_string()),
            Some(17),
        )
    }

    fn row_without_output(id: &str, status: &str, title: &str) -> BackgroundTaskRow {
        BackgroundTaskRow::shell(
            id,
            status,
            1500,
            title,
            Some(format!("/tmp/{id}.stdout")),
            None,
            None,
        )
    }

    fn typed_row(
        id: &str,
        kind: BackgroundTaskKind,
        status: &str,
        title: &str,
    ) -> BackgroundTaskRow {
        BackgroundTaskRow::new(
            id,
            kind,
            status,
            1500,
            title,
            Some(format!("/tmp/{id}.stdout")),
            Some("line one\nline two".to_string()),
            Some(17),
        )
    }

    fn fanout(
        group_id: &str,
        target_count: usize,
        slot_index: usize,
    ) -> BackgroundTaskFanoutMembership {
        BackgroundTaskFanoutMembership {
            group_id: group_id.to_string(),
            group_title: "review fanout".to_string(),
            target_count,
            slot_index,
            slot_label: format!("slot task {slot_index}"),
        }
    }

    fn render(view: &BackgroundTaskView, width: u16, height: u16) -> String {
        struct ViewWidget<'a>(&'a BackgroundTaskView);
        impl Widget for ViewWidget<'_> {
            fn render(self, area: Rect, buf: &mut Buffer) {
                self.0.render(area, buf);
            }
        }
        buffer_to_string(&draw_widget(ViewWidget(view), width, height))
    }

    #[test]
    fn list_orders_attention_rows_before_running_and_done() {
        let view = BackgroundTaskView::new(vec![
            row("done", "completed", "done task"),
            row("run", "running", "running task"),
            row("wait", "waiting_for_input", "waiting task"),
            row("fail", "failed", "failed task"),
        ]);

        assert_eq!(view.rows[0].id, "wait");
        assert_eq!(view.rows[1].id, "fail");
        assert_eq!(view.rows[2].id, "run");
        assert_eq!(view.rows[3].id, "done");
    }

    #[test]
    fn new_with_selected_selects_matching_row_after_sort() {
        let view = BackgroundTaskView::new_with_selected(
            vec![
                row("done", "completed", "done task"),
                row("run", "running", "running task"),
                row("wait", "waiting_for_input", "waiting task"),
            ],
            Some("run"),
        );

        assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("run"));
    }

    #[test]
    fn list_renders_typed_task_kinds_beyond_shell() {
        let view = BackgroundTaskView::new(vec![
            typed_row(
                "agent-1",
                BackgroundTaskKind::LocalAgent,
                "waiting_for_input",
                "review auth flow",
            ),
            typed_row(
                "cloud-1",
                BackgroundTaskKind::CloudSession,
                "running",
                "remote plan run",
            ),
            typed_row(
                "main-1",
                BackgroundTaskKind::MainSession,
                "failed",
                "main turn",
            ),
            typed_row(
                "mon-1",
                BackgroundTaskKind::Monitor,
                "running",
                "watch tests",
            ),
        ]);

        let text = render(&view, 120, 8);
        assert!(text.contains("local agent"), "{text}");
        assert!(text.contains("cloud session"), "{text}");
        assert!(text.contains("main session"), "{text}");
        assert!(text.contains("monitor"), "{text}");
        assert!(
            text.find("agent-1").unwrap() < text.find("cloud-1").unwrap(),
            "attention rows must still sort above plain running rows: {text}"
        );
        assert!(
            text.find("main-1").unwrap() < text.find("cloud-1").unwrap(),
            "failed rows must still sort above plain running rows: {text}"
        );
    }

    #[test]
    fn list_groups_fanout_local_agents_under_target_header() {
        let view = BackgroundTaskView::new(vec![
            typed_row(
                "agent-auth",
                BackgroundTaskKind::LocalAgent,
                "running",
                "auth review",
            )
            .with_fanout(fanout("review-1", 3, 0)),
            typed_row(
                "agent-storage",
                BackgroundTaskKind::LocalAgent,
                "killed",
                "storage review",
            )
            .with_fanout(fanout("review-1", 3, 1)),
            typed_row(
                "agent-api",
                BackgroundTaskKind::LocalAgent,
                "running",
                "API review",
            )
            .with_fanout(fanout("review-1", 3, 2)),
        ]);

        let text = render(&view, 120, 6);

        assert!(text.contains("review fanout"), "{text}");
        assert!(text.contains("3 target"), "{text}");
        assert!(text.contains("2 running"), "{text}");
        assert!(text.contains("1 stopped"), "{text}");
        assert!(text.contains("slot 1: slot task 0"), "{text}");
        assert!(text.contains("slot 2: slot task 1"), "{text}");
        assert!(text.contains("slot 3: slot task 2"), "{text}");
    }

    #[test]
    fn narrow_list_preserves_status_and_command_preview_for_long_ids() {
        let view = BackgroundTaskView::new(vec![row(
            "bg-shell-1234567890abcdef",
            "running",
            "cargo test -p astra-cli --all-targets --all-features",
        )]);

        let text = render(&view, 48, 3);

        assert!(text.contains("shell"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("cargo"), "{text}");
        assert!(
            !text.contains("bg-shell-1234567890abcdef"),
            "long id should be compacted before it hides the command: {text}"
        );
    }

    #[test]
    fn narrow_detail_actions_do_not_render_partial_return_word() {
        let mut view = BackgroundTaskView::new(vec![row("bg-shell-1", "running", "long command")]);
        view.handle_key(key(KeyCode::Enter));

        let text = render(&view, 24, 14);

        assert!(text.contains("actions: output"), "{text}");
        assert!(text.contains("stop"), "{text}");
        assert!(!text.contains("retu"), "{text}");
    }

    #[test]
    fn enter_opens_detail_for_selected_row() {
        let mut view = BackgroundTaskView::new(vec![
            row("first", "running", "first command"),
            row("second", "running", "second command"),
        ]);
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));

        let text = render(&view, 90, 12);
        assert!(text.contains("second"));
        assert!(text.contains("shell"));
        assert!(text.contains("running"));
        assert!(text.contains("second command"));
        assert!(text.contains("/tmp/second.stdout"));
        assert!(text.contains("line one"));
        assert!(text.contains("actions: output"));
    }

    #[test]
    fn detail_renders_output_offsets_lines_and_terminal_reason() {
        let mut view = BackgroundTaskView::new(vec![
            BackgroundTaskRow::shell(
                "fail",
                "failed",
                41_200,
                "npm test",
                Some("/tmp/fail.stdout".to_string()),
                Some("test failed".to_string()),
                Some(13_244),
            )
            .with_output_stats(Some(8192), Some(312))
            .with_terminal(Some(1), Some("exit code 1".to_string())),
        ]);

        view.handle_key(key(KeyCode::Enter));
        let text = render(&view, 100, 14);

        assert!(text.contains("offset 8192 -> 13244"), "{text}");
        assert!(text.contains("13244 bytes"), "{text}");
        assert!(text.contains("312 lines"), "{text}");
        assert!(text.contains("exit 1"), "{text}");
        assert!(text.contains("reason exit code 1"), "{text}");
    }

    #[test]
    fn detail_renders_started_ended_and_elapsed() {
        let mut view = BackgroundTaskView::new(vec![
            BackgroundTaskRow::shell("timed", "completed", 2_000, "cargo test", None, None, None)
                .with_timing(Some(0), Some(2_000)),
        ]);

        view.handle_key(key(KeyCode::Enter));
        let text = render(&view, 120, 10);

        assert!(text.contains("started 1970-01-01 00:00:00Z"), "{text}");
        assert!(text.contains("ended 1970-01-01 00:00:02Z"), "{text}");
        assert!(text.contains("elapsed 2.0s"), "{text}");
    }

    #[test]
    fn waiting_for_input_renders_as_needs_input_not_internal_state() {
        let mut view =
            BackgroundTaskView::new(vec![row("wait", "waiting_for_input", "prompting command")]);

        let list = render(&view, 100, 5);
        assert!(list.contains("1 needs input"), "{list}");
        assert!(list.contains("needs input"), "{list}");
        assert!(!list.contains("waiting_for_input"), "{list}");

        view.handle_key(key(KeyCode::Enter));
        let detail = render(&view, 100, 8);
        assert!(detail.contains("wait · needs input"), "{detail}");
        assert!(!detail.contains("waiting_for_input"), "{detail}");
    }

    #[test]
    fn pending_status_renders_explicitly() {
        let mut view =
            BackgroundTaskView::new(vec![row_without_output("queued", "pending", "queued task")]);

        let list = render(&view, 100, 4);
        assert!(list.contains("pending"), "{list}");
        assert!(!list.contains("unknown"), "{list}");

        view.handle_key(key(KeyCode::Enter));
        let detail = render(&view, 100, 10);
        assert!(detail.contains("Pending · no output yet"), "{detail}");
        assert!(!detail.contains("No output captured yet"), "{detail}");
    }

    #[test]
    fn stale_live_handle_renders_unavailable_and_disables_stop() {
        let mut view = BackgroundTaskView::new(vec![
            row_without_output("stale", "running", "restored long command")
                .with_live_control(LiveControlState::StaleHandle),
        ]);

        let list = render(&view, 100, 5);
        assert!(list.contains("unavailable"), "{list}");
        assert!(list.contains("restored long command"), "{list}");

        view.handle_key(key(KeyCode::Enter));
        let detail = render(&view, 100, 12);
        assert!(detail.contains("stale · unavailable"), "{detail}");
        assert!(detail.contains("control stale handle"), "{detail}");
        assert!(
            detail.contains("Unavailable · stale handle or unsupported runner"),
            "{detail}"
        );
        assert!(!detail.contains("No output captured yet"), "{detail}");
        assert!(detail.contains("actions: output · return"), "{detail}");
        assert!(!detail.contains("stop"), "{detail}");

        view.handle_key(key(KeyCode::Char('s')));
        assert!(
            view.take_pending_action().is_none(),
            "stale handles must not emit stop actions"
        );
    }

    #[test]
    fn stop_action_emits_selected_task_id_and_keeps_view_open() {
        let mut view = BackgroundTaskView::new(vec![
            row("first", "running", "first command"),
            row("second", "running", "second command"),
        ]);
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char('s')));

        assert_eq!(
            view.take_pending_action().as_deref(),
            Some("__background_task_stop__\nsecond")
        );
        assert!(view.take_pending_action().is_none());
        assert!(!view.is_complete());
    }

    #[test]
    fn stop_is_inert_for_terminal_rows() {
        let mut view = BackgroundTaskView::new(vec![row("done", "completed", "done task")]);
        view.handle_key(key(KeyCode::Char('s')));

        assert!(view.take_pending_action().is_none());
        assert!(!view.is_complete());
    }

    #[test]
    fn refresh_preserves_selection_by_id() {
        let mut view = BackgroundTaskView::new(vec![
            row("first", "running", "first command"),
            row("second", "running", "second command"),
        ]);
        view.handle_key(key(KeyCode::Down));
        view.replace_rows(vec![
            row("second", "waiting_for_input", "second command"),
            row("first", "running", "first command"),
        ]);

        assert_eq!(
            view.selected_row().map(|row| row.id.as_str()),
            Some("second")
        );
    }

    #[test]
    fn refresh_can_select_explicit_id_after_sort() {
        let mut view = BackgroundTaskView::new(vec![
            row("first", "running", "first command"),
            row("second", "running", "second command"),
        ]);

        view.replace_rows_with_selected(
            vec![
                row("first", "running", "first command"),
                row("new", "waiting_for_input", "new command"),
                row("second", "running", "second command"),
            ],
            Some("new"),
        );

        assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("new"));
    }

    #[test]
    fn esc_from_detail_returns_to_list_and_q_closes() {
        let mut view = BackgroundTaskView::new(vec![row("task", "running", "cmd")]);
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Esc));

        let text = render(&view, 80, 5);
        assert!(text.contains("Background tasks"));
        assert!(!view.is_complete());

        view.handle_key(key(KeyCode::Char('q')));
        assert!(view.is_complete());
    }

    #[test]
    fn parse_stop_sentinel_extracts_id() {
        assert_eq!(
            parse_stop_sentinel("__background_task_stop__\nbg-shell-1"),
            Some("bg-shell-1")
        );
        assert_eq!(parse_stop_sentinel("not it"), None);
        assert_eq!(parse_stop_sentinel(BACKGROUND_TASK_STOP_SENTINEL), None);
    }

    #[test]
    fn detail_output_action_emits_selected_task_id_and_keeps_view_open() {
        let mut view = BackgroundTaskView::new(vec![
            row("first", "running", "first command"),
            row("second", "running", "second command"),
        ]);
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Char('o')));

        assert_eq!(
            view.take_pending_action().as_deref(),
            Some("__background_task_output__\nsecond")
        );
        assert!(view.take_pending_action().is_none());
        assert!(!view.is_complete());
    }

    #[test]
    fn parse_output_sentinel_extracts_id() {
        assert_eq!(
            parse_output_sentinel("__background_task_output__\nbg-shell-1"),
            Some("bg-shell-1")
        );
        assert_eq!(parse_output_sentinel("not it"), None);
        assert_eq!(parse_output_sentinel(BACKGROUND_TASK_OUTPUT_SENTINEL), None);
    }

    #[test]
    fn local_agent_detail_does_not_offer_shell_output_action() {
        let mut view = BackgroundTaskView::new(vec![typed_row(
            "agent-1",
            BackgroundTaskKind::LocalAgent,
            "running",
            "review auth flow",
        )]);
        view.handle_key(key(KeyCode::Enter));

        let detail = render(&view, 80, 12);
        assert!(detail.contains("actions: stop · return"), "{detail}");
        assert!(!detail.contains("actions: output"), "{detail}");

        view.handle_key(key(KeyCode::Char('o')));
        assert!(
            view.take_pending_action().is_none(),
            "local agent output must not emit shell task_output sentinel"
        );

        view.handle_key(key(KeyCode::Char('s')));
        assert_eq!(
            view.take_pending_action().as_deref(),
            Some("__background_task_stop__\nagent-1")
        );
    }
}
