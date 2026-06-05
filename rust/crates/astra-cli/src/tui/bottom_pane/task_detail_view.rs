use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

use super::view::{BottomPaneView, CancellationEvent};
use crate::cli::surface::session_task_surface::SessionTaskStatusKind;
use crate::tui::history_cell::task::{ChildStatus, TaskCell, TaskStatus};
use astra_tools::task_mgmt::SessionTask;

/// Detail drill-in view for a TaskCell. Shows header + full children
/// list with descriptions, durations, and output. Scrollable.
pub(crate) struct TaskDetailView {
    title: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
    completed: bool,
    reopen: Option<String>,
    live_task_id: Option<String>,
}

impl TaskDetailView {
    pub fn from_task_cell(cell: &TaskCell) -> Self {
        let title = format!("Task: {}", truncate(&cell.description, 50));
        let lines = build_detail_lines(cell);
        Self {
            title,
            lines,
            scroll: 0,
            completed: false,
            reopen: None,
            live_task_id: None,
        }
    }

    pub fn from_session_task(task: &SessionTask) -> Self {
        let title = format!("#{} — {}", task.id, truncate(&task.title, 50));
        let lines = build_session_task_lines(task);
        Self {
            title,
            lines,
            scroll: 0,
            completed: false,
            reopen: None,
            live_task_id: None,
        }
    }

    pub fn with_reopen(mut self, reopen: impl Into<String>) -> Self {
        self.reopen = Some(reopen.into());
        self
    }

    pub fn with_live_task_id(mut self, id: impl Into<String>) -> Self {
        self.live_task_id = Some(id.into());
        self
    }

    fn refresh_from_task_cell(&mut self, cell: &TaskCell) {
        let old_max_scroll = self.lines.len().saturating_sub(MAX_VISIBLE);
        let was_pinned_to_bottom = self.scroll >= old_max_scroll;
        self.title = format!("Task: {}", truncate(&cell.description, 50));
        self.lines = build_detail_lines(cell);
        let new_max_scroll = self.lines.len().saturating_sub(MAX_VISIBLE);
        self.scroll = if was_pinned_to_bottom {
            new_max_scroll
        } else {
            self.scroll.min(new_max_scroll)
        };
    }
}

fn build_session_task_lines(task: &SessionTask) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let status_color = match task.status {
        SessionTaskStatusKind::InProgress => Color::Yellow,
        SessionTaskStatusKind::Completed => Color::Green,
        SessionTaskStatusKind::Failed => Color::Red,
        SessionTaskStatusKind::Cancelled => Color::Yellow,
        _ => Color::White,
    };
    out.push(Line::from(vec![
        Span::styled("  Status: ", dim),
        Span::styled(task.status.as_str(), Style::default().fg(status_color)),
    ]));

    if let Some(ref owner) = task.owner {
        out.push(Line::from(vec![
            Span::styled("  Owner: ", dim),
            Span::raw(owner.clone()),
        ]));
    }

    if let Some(ref desc) = task.description {
        out.push(Line::default());
        out.push(Line::from(Span::styled("  Description:", bold)));
        for line in desc.lines().take(10) {
            out.push(Line::from(Span::styled(format!("    {line}"), dim)));
        }
    }

    if !task.blocked_by.is_empty() {
        out.push(Line::default());
        out.push(Line::from(vec![
            Span::styled("  Blocked by: ", dim),
            Span::raw(task.blocked_by.join(", ")),
        ]));
    }

    if !task.subtasks.is_empty() {
        out.push(Line::default());
        out.push(Line::from(Span::styled(
            format!("  Subtasks ({}):", task.subtasks.len()),
            bold,
        )));
        for (i, sub) in task.subtasks.iter().enumerate() {
            let is_last = i + 1 == task.subtasks.len();
            let connector = if is_last { "└" } else { "├" };
            let (icon, icon_color) = match sub.status {
                SessionTaskStatusKind::Completed => ("✓", Color::Green),
                SessionTaskStatusKind::InProgress => ("◦", Color::Yellow),
                SessionTaskStatusKind::Pending => ("·", Color::DarkGray),
                SessionTaskStatusKind::Failed => ("✗", Color::Red),
                SessionTaskStatusKind::Cancelled => ("⏹", Color::Yellow),
                SessionTaskStatusKind::Archived
                | SessionTaskStatusKind::Deleted
                | SessionTaskStatusKind::Other => ("·", Color::DarkGray),
            };
            out.push(Line::from(vec![
                Span::styled(format!("  {connector}─ "), dim),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::raw(format!(" {}", sub.title)),
            ]));
        }
    }

    out.push(Line::default());
    out.push(Line::from(vec![
        Span::styled("  Created: ", dim),
        Span::raw(task.created_at.clone()),
    ]));
    out.push(Line::from(vec![
        Span::styled("  Updated: ", dim),
        Span::raw(task.updated_at.clone()),
    ]));

    out
}

fn truncate(s: &str, max: usize) -> String {
    // Char-aware truncation: avoids slicing mid-codepoint for CJK / emoji.
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}

fn status_style(status: &TaskStatus) -> Style {
    match status {
        TaskStatus::Running => Style::default().fg(Color::Yellow),
        TaskStatus::Completed => Style::default().fg(Color::Green),
        TaskStatus::Failed => Style::default().fg(Color::Red),
    }
}

fn child_status_style(status: &ChildStatus) -> Style {
    match status {
        ChildStatus::Running => Style::default().fg(Color::Yellow),
        ChildStatus::Success => Style::default().fg(Color::Green),
        ChildStatus::Failed => Style::default().fg(Color::Red),
    }
}

fn child_status_icon(status: &ChildStatus) -> &'static str {
    match status {
        ChildStatus::Running => "◦",
        ChildStatus::Success => "✓",
        ChildStatus::Failed => "✗",
    }
}

fn build_detail_lines(cell: &TaskCell) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let dim = Style::default().fg(Color::DarkGray);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let interrupted_wait = cell.is_interrupted_wait();

    // Header
    let status_text = match cell.status {
        TaskStatus::Running => "Running",
        TaskStatus::Completed => "Completed",
        TaskStatus::Failed if interrupted_wait => "Interrupted",
        TaskStatus::Failed => "Failed",
    };
    let status_style = if interrupted_wait {
        Style::default().fg(Color::Yellow)
    } else {
        status_style(&cell.status)
    };
    out.push(Line::from(vec![
        Span::styled("  Status: ", dim),
        Span::styled(status_text, status_style),
    ]));
    if !matches!(cell.status, TaskStatus::Running) {
        out.push(Line::from(Span::styled(
            "  Snapshot: frozen at completion",
            dim,
        )));
    }

    if let Some(ms) = cell.duration_ms {
        out.push(Line::from(vec![
            Span::styled("  Duration: ", dim),
            Span::raw(format_duration(ms)),
        ]));
    }

    if let Some(ref err) = cell.error {
        let err_style = if interrupted_wait {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::Red)
        };
        out.push(Line::from(vec![
            Span::styled("  Error: ", err_style),
            Span::raw(err.clone()),
        ]));
    }

    out.push(Line::default());

    // Children
    if cell.children.is_empty() {
        out.push(Line::from(Span::styled("  No sub-operations.", dim)));
    } else {
        out.push(Line::from(Span::styled(
            format!("  Operations ({}):", cell.children.len()),
            bold,
        )));
        out.push(Line::default());

        for (i, child) in cell.children.iter().enumerate() {
            let is_last = i + 1 == cell.children.len();
            let connector = if is_last { "└" } else { "├" };
            let icon = child_status_icon(&child.status);

            out.push(Line::from(vec![
                Span::styled(format!("  {connector}─ "), dim),
                Span::styled(icon, child_status_style(&child.status)),
                Span::raw(" "),
                Span::styled(child.name.clone(), bold),
            ]));

            // Description line
            if !child.description.is_empty() {
                let indent = if is_last { "   " } else { "│  " };
                out.push(Line::from(vec![
                    Span::styled(format!("  {indent}  "), dim),
                    Span::styled(truncate(&child.description, 70), dim),
                ]));
            }

            // Duration
            if let Some(ms) = child.duration_ms {
                let indent = if is_last { "   " } else { "│  " };
                out.push(Line::from(vec![
                    Span::styled(format!("  {indent}  "), dim),
                    Span::styled(format_duration(ms), dim),
                ]));
            }
        }
    }

    if let Some(ref summary) = cell.output_summary {
        out.push(Line::default());
        out.push(Line::from(Span::styled("  Output:", bold)));
        let mut truncated = false;
        for (idx, line) in summary.lines().enumerate() {
            if idx >= MAX_OUTPUT_LINES {
                truncated = true;
                break;
            }
            out.push(Line::from(Span::styled(format!("    {line}"), dim)));
        }
        if truncated {
            out.push(Line::from(Span::styled(
                "    ... output truncated in detail view",
                dim,
            )));
        }
    }

    out
}

fn format_duration(ms: u64) -> String {
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

const MAX_VISIBLE: usize = 20;
const MAX_OUTPUT_LINES: usize = 500;

impl BottomPaneView for TaskDetailView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let title_style = Style::default()
            .fg(crate::tui::theme::current().accent)
            .add_modifier(Modifier::BOLD);
        let title_line = Line::from(Span::styled(format!("  {}", self.title), title_style));
        buf.set_line(area.x, area.y, &title_line, area.width);

        let body_y = area.y.saturating_add(1);
        let body_h = area.height.saturating_sub(1) as usize;
        let max_y = area.y.saturating_add(area.height);
        for (i, line) in self.lines.iter().skip(self.scroll).take(body_h).enumerate() {
            // Saturating add; clamp to area bottom so we never write past the buffer.
            let y = body_y.saturating_add(u16::try_from(i).unwrap_or(u16::MAX));
            if y >= max_y {
                break;
            }
            buf.set_line(area.x, y, line, area.width);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let body = self.lines.len().min(MAX_VISIBLE);
        (body as u16).saturating_add(1) // +1 for title
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.scroll = self.scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') if self.scroll + MAX_VISIBLE < self.lines.len() => {
                self.scroll += 1;
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(MAX_VISIBLE);
            }
            KeyCode::PageDown => {
                self.scroll =
                    (self.scroll + MAX_VISIBLE).min(self.lines.len().saturating_sub(MAX_VISIBLE));
            }
            KeyCode::Home => {
                self.scroll = 0;
            }
            KeyCode::End => {
                self.scroll = self.lines.len().saturating_sub(MAX_VISIBLE);
            }
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
        if self.completed {
            Some(super::view::ViewCompletion {
                result: None,
                reopen: self.reopen.clone(),
            })
        } else {
            None
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn refresh_task_cell(&mut self, id: &str, cell: &TaskCell) -> bool {
        if self.live_task_id.as_deref() != Some(id) {
            return false;
        }
        self.refresh_from_task_cell(cell);
        true
    }

    fn live_task_id(&self) -> Option<&str> {
        self.live_task_id.as_deref()
    }

    fn hint_keys(&self) -> Option<String> {
        Some("↑↓/Pg scroll · Esc/←/q back".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::history_cell::HistoryCell;
    use crate::tui::history_cell::task::TaskCell;
    use astra_tools::task_mgmt::SessionTask;

    fn mk_session_task(id: &str, title: &str) -> SessionTask {
        SessionTask {
            id: id.into(),
            title: title.into(),
            description: None,
            status: "pending".into(),
            subtasks: Vec::new(),
            created_at: "2026-01-01T00:00:00Z".into(),
            updated_at: "2026-01-01T00:00:00Z".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    /// C5 regression: multi-byte char title longer than 50 chars must
    /// not panic. Old impl used `&s[..max - 1]` which slices bytes and
    /// crashes mid-codepoint for CJK / emoji.
    #[test]
    fn from_session_task_with_long_cjk_title_does_not_panic() {
        let long_cjk = "日本語のとても長いタスク名前です本当に長いんですよ".repeat(3);
        let task = mk_session_task("1", &long_cjk);
        // Just constructing it must not panic.
        let view = TaskDetailView::from_session_task(&task);
        assert!(!view.title.is_empty());
    }

    #[test]
    fn from_task_cell_with_emoji_description_does_not_panic() {
        let cell = TaskCell::new_running("tool-1", "🚀🔥💥".repeat(20));
        let view = TaskDetailView::from_task_cell(&cell);
        assert!(view.title.starts_with("Task:"));
    }

    #[test]
    fn interrupted_get_agent_result_shows_interrupted_status() {
        let mut cell = TaskCell::new_running("tool-1", "Get agent result: reviewer@abc");
        cell.finalize();

        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("Interrupted"), "{rendered}");
        assert!(!rendered.contains("Status: Failed"), "{rendered}");
    }

    /// C-TUI-1 regression: large scroll offset must not write past the
    /// area's bottom edge or wrap around `u16`. Pre-fix used unchecked
    /// `body_y + i as u16` arithmetic.
    #[test]
    fn render_with_huge_scroll_offset_does_not_panic_or_overflow() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let cell = TaskCell::new_running("tool-1", "demo".to_string());
        let mut view = TaskDetailView::from_task_cell(&cell);
        // Force scroll well past content length.
        view.scroll = 10_000;

        // Tiny area; render must clamp inside it.
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        // Should not panic regardless of scroll.
        view.render(area, &mut buf);
    }

    /// C-TUI-1 regression: render near the top of the u16 coordinate
    /// space must not wrap. Use `area.y` close to u16::MAX and ensure
    /// no panic and no out-of-area writes.
    #[test]
    fn render_near_u16_max_y_does_not_wrap() {
        use ratatui::buffer::Buffer;
        use ratatui::layout::Rect;

        let cell = TaskCell::new_running("tool-1", "demo".to_string());
        let mut lines = Vec::new();
        for _ in 0..50 {
            lines.push(Line::from(""));
        }
        let mut view = TaskDetailView::from_task_cell(&cell);
        view.lines = lines;
        view.scroll = 0;

        // Buffer area must include the high y-range we render into.
        let area = Rect::new(0, u16::MAX - 6, 40, 5);
        let mut buf = Buffer::empty(area);
        view.render(area, &mut buf);
        // The render must not have wrapped to y=0..; that would be an overflow.
    }

    /// C-TUI-2 regression: title must use the theme accent rather than a
    /// hardcoded `Color::Cyan`. We assert the source no longer carries
    /// the literal so light-terminal users get a readable colour.
    #[test]
    fn title_does_not_hardcode_cyan() {
        let src = include_str!("task_detail_view.rs");
        // Walk the render() body specifically; other Color::Cyan refs
        // (e.g. in tests) would be fine, but render() must not have one.
        let render_start = src.find("fn render(&self").expect("render fn");
        let render_end = src[render_start..]
            .find("\n    }")
            .map(|i| render_start + i)
            .unwrap_or(src.len());
        let render_src = &src[render_start..render_end];
        assert!(
            !render_src.contains("Color::Cyan"),
            "render() still uses hardcoded Color::Cyan; should go through theme::current().accent"
        );
    }

    #[test]
    fn truncate_handles_exact_byte_boundary() {
        // 50-byte string of mixed ASCII + multi-byte
        let s = "abcd日本語abcd日本語abcd日本語";
        // Should not panic regardless of max value
        for max in [10, 25, 50, 100] {
            let result = truncate(s, max);
            assert!(!result.is_empty() || s.is_empty());
        }
    }

    #[test]
    fn task_detail_keeps_more_than_twenty_agent_output_lines() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        let output = (0..30)
            .map(|i| format!("line-{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        cell.complete("success", 1, Some(output), None);

        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("line-29"));
    }

    #[test]
    fn task_detail_formats_long_duration_as_minutes() {
        assert_eq!(format_duration(999), "999ms");
        assert_eq!(format_duration(12_500), "12.5s");
        assert_eq!(format_duration(300_000), "5m0s");
        assert_eq!(format_duration(305_000), "5m5s");
    }

    #[test]
    fn completed_task_detail_shows_snapshot_hint() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        cell.complete("success", 300_000, Some("done".into()), None);
        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Snapshot: frozen at completion"));
        assert!(rendered.contains("5m0s"));
    }

    #[test]
    fn task_detail_refreshes_live_task_cell() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        cell.output_summary = Some("before".into());
        let mut view = TaskDetailView::from_task_cell(&cell).with_live_task_id("agent-1");

        cell.output_summary = Some("after".into());
        assert!(view.refresh_task_cell("agent-1", &cell));
        let rendered = view
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("after"));
        assert!(!view.refresh_task_cell("other-agent", &cell));
    }

    #[test]
    fn refresh_task_cell_preserves_user_scroll_when_scrolled_up() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        cell.output_summary = Some(
            (0..80)
                .map(|i| format!("before-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut view = TaskDetailView::from_task_cell(&cell).with_live_task_id("agent-1");
        view.scroll = 5;

        cell.output_summary = Some(
            (0..120)
                .map(|i| format!("after-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert!(view.refresh_task_cell("agent-1", &cell));

        assert_eq!(view.scroll, 5);
    }

    #[test]
    fn refresh_task_cell_follows_bottom_when_pinned() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        cell.output_summary = Some(
            (0..80)
                .map(|i| format!("before-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let mut view = TaskDetailView::from_task_cell(&cell).with_live_task_id("agent-1");
        view.scroll = view.lines.len().saturating_sub(MAX_VISIBLE);

        cell.output_summary = Some(
            (0..120)
                .map(|i| format!("after-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        assert!(view.refresh_task_cell("agent-1", &cell));

        assert_eq!(view.scroll, view.lines.len().saturating_sub(MAX_VISIBLE));
    }
}
