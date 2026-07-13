use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
};

use super::view::{BottomPaneView, CancellationEvent};
use crate::tui::history_cell::task::{ChildStatus, TaskCell, TaskStatus};
use astra_tools::task_mgmt::SessionTask;
use astra_tools::task_mgmt::SessionTaskStatusKind;

/// Detail drill-in view for a TaskCell. Shows header + full children
/// list with descriptions, durations, and output. Scrollable.
pub(crate) struct TaskDetailView {
    kind: DetailKind,
    title: String,
    lines: Vec<Line<'static>>,
    scroll: usize,
    completed: bool,
    reopen: Option<String>,
    live_task_id: Option<String>,
}

#[derive(Clone, Copy)]
enum DetailKind {
    Task,
    Agent,
}

impl DetailKind {
    fn title(self, description: &str) -> String {
        let prefix = match self {
            Self::Task => "Task details",
            Self::Agent => "Agent inspector",
        };
        format!("{prefix} · {}", truncate_label(description, 50))
    }
}

impl TaskDetailView {
    pub fn from_task_cell(cell: &TaskCell) -> Self {
        Self::from_cell(cell, DetailKind::Task)
    }

    pub fn from_agent_cell(cell: &TaskCell) -> Self {
        Self::from_cell(cell, DetailKind::Agent)
    }

    fn from_cell(cell: &TaskCell, kind: DetailKind) -> Self {
        let title = kind.title(&cell.description);
        let lines = build_detail_lines(cell);
        Self {
            kind,
            title,
            lines,
            scroll: 0,
            completed: false,
            reopen: None,
            live_task_id: None,
        }
    }

    pub fn from_session_task(task: &SessionTask) -> Self {
        let title = format!("Task details · {}", truncate_label(&task.title, 50));
        let lines = build_session_task_lines(task);
        Self {
            kind: DetailKind::Task,
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
        self.title = self.kind.title(&cell.description);
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
    let theme = crate::tui::theme::current();
    let mut out = Vec::new();
    let dim = Style::default().fg(theme.dim);
    let bold = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);

    let status_color = match task.status {
        SessionTaskStatusKind::InProgress => theme.warn,
        SessionTaskStatusKind::Completed => theme.success,
        SessionTaskStatusKind::Failed => theme.error,
        SessionTaskStatusKind::Cancelled => theme.warn,
        _ => theme.fg,
    };
    out.push(Line::from(vec![
        Span::styled(task.status.as_str(), Style::default().fg(status_color)),
        Span::styled(" · ".to_string(), dim),
        Span::styled(task.id.clone(), dim),
    ]));

    let plan_step = durable_plan_step_provenance(task);
    if let Some((plan_id, version, step_id)) = plan_step {
        out.push(Line::from(vec![
            Span::styled("Kind · ".to_string(), dim),
            Span::styled("Durable plan step", bold),
        ]));
        let mut plan_spans = vec![
            Span::styled("Plan · ".to_string(), dim),
            Span::styled(plan_id.to_string(), dim),
        ];
        if let Some(version) = version {
            plan_spans.push(Span::styled(format!(" · v{version}"), dim));
        }
        out.push(Line::from(plan_spans));
        out.push(Line::from(vec![
            Span::styled("Step · ".to_string(), dim),
            Span::styled(step_id.to_string(), dim),
        ]));
    }

    if let Some(ref owner) = task.owner {
        out.push(Line::from(vec![
            Span::styled("Owner · ".to_string(), dim),
            Span::styled(owner.clone(), dim),
        ]));
    }

    if let Some(ref desc) = task.description {
        out.push(Line::default());
        out.push(Line::from(Span::styled("Description", bold)));
        for line in desc.lines().take(10) {
            out.push(Line::from(Span::styled(format!("  {line}"), dim)));
        }
    }

    if !task.blocked_by.is_empty() {
        out.push(Line::default());
        out.push(Line::from(vec![
            Span::styled("Depends on · ".to_string(), dim),
            Span::styled(task.blocked_by.join(", "), dim),
        ]));
    }

    if !task.subtasks.is_empty() {
        out.push(Line::default());
        out.push(Line::from(Span::styled(
            format!("Checklist · {}", task.subtasks.len()),
            bold,
        )));
        for sub in &task.subtasks {
            let (icon, icon_color) = match sub.status {
                SessionTaskStatusKind::Completed => ("✓", theme.success),
                SessionTaskStatusKind::InProgress => ("◦", theme.warn),
                SessionTaskStatusKind::Paused => ("⏸", theme.warn),
                SessionTaskStatusKind::Pending => ("·", theme.dim),
                SessionTaskStatusKind::Failed => ("✗", theme.error),
                SessionTaskStatusKind::Cancelled => ("⏹", theme.warn),
                SessionTaskStatusKind::Archived
                | SessionTaskStatusKind::Deleted
                | SessionTaskStatusKind::Migrated
                | SessionTaskStatusKind::Other => ("·", theme.dim),
            };
            out.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(icon, Style::default().fg(icon_color)),
                Span::raw(format!(" {}", sub.title)),
            ]));
        }
    }

    out.push(Line::default());
    if !task.created_at.trim().is_empty() {
        out.push(Line::from(vec![
            Span::styled("Created · ".to_string(), dim),
            Span::styled(task.created_at.clone(), dim),
        ]));
    }
    out.push(Line::from(vec![
        Span::styled(
            if plan_step.is_some() {
                "Revision · "
            } else {
                "Updated · "
            }
            .to_string(),
            dim,
        ),
        Span::styled(task.updated_at.clone(), dim),
    ]));

    out
}

/// Proves plan provenance from both the reserved row namespace and typed
/// metadata. User-authored checklist metadata alone must never make a row
/// masquerade as a durable plan step.
fn durable_plan_step_provenance(task: &SessionTask) -> Option<(&str, Option<u64>, &str)> {
    if !task.id.starts_with("plan:") {
        return None;
    }
    let metadata = task.metadata.as_ref()?;
    if metadata.get("source")?.as_str()?.trim() != "plan" {
        return None;
    }
    let plan_id = metadata.get("plan_id")?.as_str()?.trim();
    let step_id = metadata.get("step_id")?.as_str()?.trim();
    if plan_id.is_empty() || step_id.is_empty() {
        return None;
    }
    Some((
        plan_id,
        metadata
            .get("plan_version")
            .and_then(serde_json::Value::as_u64),
        step_id,
    ))
}

use crate::cli::effects::truncate_label;

fn status_style(status: &TaskStatus) -> Style {
    let theme = crate::tui::theme::current();
    match status {
        TaskStatus::Running => Style::default().fg(theme.warn),
        TaskStatus::Waiting => Style::default().fg(theme.warn),
        TaskStatus::Completed => Style::default().fg(theme.success),
        TaskStatus::Interrupted => Style::default().fg(theme.warn),
        TaskStatus::Failed => Style::default().fg(theme.error),
        TaskStatus::Cancelled => Style::default().fg(theme.dim),
        TaskStatus::Unconfirmed => Style::default().fg(theme.dim),
    }
}

fn child_status_style(status: &ChildStatus) -> Style {
    let theme = crate::tui::theme::current();
    match status {
        ChildStatus::Running => Style::default().fg(theme.warn),
        ChildStatus::Success => Style::default().fg(theme.success),
        ChildStatus::Failed => Style::default().fg(theme.error),
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
    let theme = crate::tui::theme::current();
    let mut out = Vec::new();
    let dim = Style::default().fg(theme.dim);
    let bold = Style::default()
        .fg(theme.accent)
        .add_modifier(Modifier::BOLD);
    // Header
    let status_text = match cell.status {
        TaskStatus::Running => "Running",
        TaskStatus::Waiting => "Waiting",
        TaskStatus::Completed => "Completed",
        TaskStatus::Interrupted => "Interrupted",
        TaskStatus::Failed => "Failed",
        TaskStatus::Cancelled => "Cancelled",
        TaskStatus::Unconfirmed => "Status unconfirmed",
    };
    let status_style = status_style(&cell.status);
    out.push(Line::from(vec![
        Span::styled(status_text, status_style),
        Span::styled(" · ".to_string(), dim),
        Span::styled(
            if cell.status.is_active() {
                "live"
            } else {
                "snapshot"
            },
            dim,
        ),
    ]));
    if matches!(cell.status, TaskStatus::Unconfirmed) {
        out.push(Line::from(Span::styled(
            "  Live updates ended before a terminal event was confirmed",
            dim,
        )));
    } else if !cell.status.is_active() {
        out.push(Line::from(Span::styled("  Frozen at completion", dim)));
    }

    if let Some(ms) = cell.duration_ms {
        out.push(Line::from(vec![
            Span::styled("Duration · ".to_string(), dim),
            Span::styled(format_duration(ms), dim),
        ]));
    }

    if let Some(ref err) = cell.error {
        let err_style = if cell.status == TaskStatus::Interrupted {
            Style::default().fg(theme.warn)
        } else {
            Style::default().fg(theme.error)
        };
        out.push(Line::from(vec![
            Span::styled("Error · ".to_string(), err_style),
            Span::styled(err.clone(), dim),
        ]));
    }

    out.push(Line::default());

    // Children
    if cell.children.is_empty() {
        out.push(Line::from(Span::styled("No steps yet.", dim)));
    } else {
        out.push(Line::from(Span::styled(
            format!("Steps · {}", cell.children.len()),
            bold,
        )));
        out.push(Line::default());

        for child in &cell.children {
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(
                    child_status_icon(&child.status),
                    child_status_style(&child.status),
                ),
                Span::raw(" "),
                Span::styled(child.name.clone(), bold),
            ];
            if !child.description.is_empty() {
                spans.push(Span::styled(
                    format!(" · {}", truncate_label(&child.description, 54)),
                    dim,
                ));
            }
            if let Some(ms) = child.duration_ms {
                spans.push(Span::styled(format!(" · {}", format_duration(ms)), dim));
            }
            out.push(Line::from(spans));
        }
    }

    if let Some(ref summary) = cell.output_summary {
        out.push(Line::default());
        out.push(Line::from(Span::styled("Output", bold)));
        let mut truncated = false;
        for (idx, line) in summary.lines().enumerate() {
            if idx >= MAX_OUTPUT_LINES {
                truncated = true;
                break;
            }
            out.push(Line::from(Span::styled(format!("  {line}"), dim)));
        }
        if truncated {
            out.push(Line::from(Span::styled(
                "  ... output truncated in detail view",
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
        Some(match self.kind {
            DetailKind::Agent if self.live_task_id.is_some() => {
                "↑↓/Pg scroll · Following live · Esc back".into()
            }
            DetailKind::Agent => "↑↓/Pg scroll · Esc back".into(),
            DetailKind::Task => "↑↓/Pg scroll · Esc close".into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::history_cell::task::TaskCell;
    use astra_tools::task_mgmt::SessionTask;

    fn mk_session_task(id: &str, title: &str) -> SessionTask {
        SessionTask {
            archived_at: None,
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
    fn durable_plan_step_detail_keeps_provenance_and_dependencies_distinct() {
        let mut task = mk_session_task("plan:plan-7:verify", "Verify release");
        task.created_at.clear();
        task.updated_at = "plan-v12".into();
        task.blocked_by = vec!["plan:plan-7:build".into()];
        task.metadata = Some(serde_json::Map::from_iter([
            ("source".into(), serde_json::json!("plan")),
            ("plan_id".into(), serde_json::json!("plan-7")),
            ("plan_version".into(), serde_json::json!(12)),
            ("step_id".into(), serde_json::json!("verify")),
        ]));

        let view = TaskDetailView::from_session_task(&task);
        let rendered = view
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("Durable plan step"), "{rendered}");
        assert!(rendered.contains("Plan · plan-7 · v12"), "{rendered}");
        assert!(rendered.contains("Step · verify"), "{rendered}");
        assert!(
            rendered.contains("Depends on · plan:plan-7:build"),
            "{rendered}"
        );
        assert!(rendered.contains("Revision · plan-v12"), "{rendered}");
        assert!(!rendered.contains("Created ·"), "{rendered}");
    }

    #[test]
    fn from_task_cell_with_emoji_description_does_not_panic() {
        let cell = TaskCell::new_running("tool-1", "🚀🔥💥".repeat(20));
        let view = TaskDetailView::from_task_cell(&cell);
        assert!(view.title.starts_with("Task details ·"));
    }

    #[test]
    fn explicit_interrupted_task_shows_interrupted_status() {
        let mut cell = TaskCell::new_running("tool-1", "reviewer");
        cell.complete(
            "interrupted",
            25,
            Some("partial findings".into()),
            Some("budget exhausted".into()),
        );

        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.to_string()))
            .collect::<Vec<_>>()
            .join("");

        assert!(rendered.contains("Interrupted"), "{rendered}");
        assert!(!rendered.contains("Failed"), "{rendered}");
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

    #[test]
    fn truncate_handles_exact_byte_boundary() {
        // 50-byte string of mixed ASCII + multi-byte
        let s = "abcd日本語abcd日本語abcd日本語";
        // Should not panic regardless of max value
        for max in [10, 25, 50, 100] {
            let result = truncate_label(s, max);
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
        cell.complete("completed", 1, Some(output), None);

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
        cell.complete("completed", 300_000, Some("done".into()), None);
        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(rendered.contains("Completed · snapshot"));
        assert!(rendered.contains("Frozen at completion"));
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
    fn agent_inspector_identifies_live_follow_and_keeps_identity_after_refresh() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        let mut view = TaskDetailView::from_agent_cell(&cell).with_live_task_id("agent-1");

        assert_eq!(view.title, "Agent inspector · reviewer");
        assert_eq!(
            view.hint_keys().as_deref(),
            Some("↑↓/Pg scroll · Following live · Esc back")
        );
        cell.description = "reviewer · updated objective".into();
        assert!(view.refresh_task_cell("agent-1", &cell));
        assert_eq!(view.title, "Agent inspector · reviewer · updated objective");
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

    #[test]
    fn child_steps_render_as_single_compact_rows() {
        let mut cell = TaskCell::new_running("agent-1", "reviewer");
        cell.push_child_started("a", "Read", "src/main.rs");
        cell.push_child_completed("a", "completed", 20);

        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            rendered.contains("Read · src/main.rs · 20ms"),
            "child rows should render as one compact summary line: {rendered}"
        );
    }

    #[test]
    fn empty_steps_use_calm_copy() {
        let cell = TaskCell::new_running("agent-1", "reviewer");
        let view = TaskDetailView::from_task_cell(&cell);
        let rendered = view
            .lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");

        assert!(rendered.contains("No steps yet."), "{rendered}");
        assert!(!rendered.contains("No sub-operations."), "{rendered}");
    }
}
