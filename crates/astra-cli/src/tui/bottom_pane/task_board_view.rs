//! Primary-canvas browser for the canonical task-board projection.
//!
//! Compact chat keeps a small task summary near the composer. Once the user
//! is inspecting a root or agent transcript, that location is intentionally
//! absent; this view lets Ctrl+T expose the exact same typed projection
//! without reconstructing task state from chat text or opening a second store.

use std::cell::Cell;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};

use super::task_detail_view::TaskDetailView;
use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ViewActionDisposition,
    ViewActionRequest, ViewCompletion,
};
use crate::tui::task_board_observer::{
    ProjectedTaskTruthState, TaskBoardProjection, TaskBoardTruthState,
};
use astra_tools::task_mgmt::SessionTask;

pub(crate) struct TaskBoardView {
    projection: TaskBoardProjection,
    /// Line offset into the complete, already-fetched projection. The view
    /// owns this instead of asking the observer to mutate its canonical data.
    scroll_offset: Cell<usize>,
    /// Armed only by task focus navigation. Render consumes it once to reveal
    /// the selected stable row; manual scroll immediately disarms it so the
    /// viewport never snaps away from what the user is reviewing.
    follow_focused_task: Cell<bool>,
    /// Stable task identity, rather than a render-row index. A refresh can
    /// reorder or prune rows without making focus point at a different task.
    focused_task_id: Option<String>,
    /// Detail is available only for the current session's canonical task
    /// records. The all-sessions rollup deliberately carries summaries, not
    /// fabricated details.
    detail: Option<(String, TaskDetailView)>,
    pending_action: Option<ViewActionRequest>,
    completed: bool,
}

struct RenderedTaskLines {
    lines: Vec<Line<'static>>,
    focused_line_index: Option<usize>,
}

impl RenderedTaskLines {
    fn plain(lines: Vec<Line<'static>>) -> Self {
        Self {
            lines,
            focused_line_index: None,
        }
    }
}

impl TaskBoardView {
    pub(crate) fn new(projection: TaskBoardProjection) -> Self {
        Self {
            projection,
            scroll_offset: Cell::new(0),
            follow_focused_task: Cell::new(false),
            focused_task_id: None,
            detail: None,
            pending_action: None,
            completed: false,
        }
    }

    fn title(&self) -> &'static str {
        match &self.projection {
            TaskBoardProjection::Single { .. } => "Task board · This session",
            TaskBoardProjection::All { .. } => "Task board · All sessions",
        }
    }

    fn truth_line(&self) -> (&'static str, Color) {
        let theme = crate::tui::theme::current();
        let projected_truth_state = match &self.projection {
            TaskBoardProjection::Single {
                projected_truth_state,
                ..
            }
            | TaskBoardProjection::All {
                projected_truth_state,
                ..
            } => *projected_truth_state,
        };
        match (self.projection.truth_state(), projected_truth_state) {
            (TaskBoardTruthState::Refreshing, ProjectedTaskTruthState::Confirmed) => (
                "Checklist sync is refreshing · plan work remains confirmed",
                theme.dim,
            ),
            (TaskBoardTruthState::Stale, ProjectedTaskTruthState::Confirmed) => (
                "Checklist sync is delayed · plan work remains confirmed · R refresh",
                theme.warn,
            ),
            (TaskBoardTruthState::Unavailable, ProjectedTaskTruthState::Confirmed) => (
                "Checklist service is unavailable · plan work remains confirmed · R refresh",
                theme.warn,
            ),
            (TaskBoardTruthState::Unavailable, ProjectedTaskTruthState::Stale) => (
                "Checklist service is unavailable · showing last confirmed plan work · R refresh",
                theme.warn,
            ),
            (TaskBoardTruthState::Unbound, _) => {
                ("No session is bound to this task board.", theme.dim)
            }
            (TaskBoardTruthState::Loading, _) => ("Loading canonical task state…", theme.accent),
            (TaskBoardTruthState::Confirmed, _) => ("Canonical task state", theme.dim),
            (TaskBoardTruthState::Refreshing, _) => ("Refreshing task state…", theme.dim),
            (TaskBoardTruthState::Stale, _) => (
                "Checklist state is delayed · showing last confirmed checklist · R refresh",
                theme.warn,
            ),
            (TaskBoardTruthState::Unavailable, _) => (
                "Checklist service is unavailable · no checklist state is inferred · R refresh",
                theme.warn,
            ),
        }
    }

    fn projected_truth_line(&self) -> Option<(&'static str, Color)> {
        let theme = crate::tui::theme::current();
        match &self.projection {
            TaskBoardProjection::Single {
                projected_truth_state,
                ..
            }
            | TaskBoardProjection::All {
                projected_truth_state,
                ..
            } => match projected_truth_state {
                ProjectedTaskTruthState::NotConfigured => None,
                ProjectedTaskTruthState::Loading => Some(("Plan work is loading…", theme.dim)),
                ProjectedTaskTruthState::Confirmed => Some(("Plan work is confirmed", theme.dim)),
                ProjectedTaskTruthState::Stale => Some((
                    "Plan work is delayed · showing last confirmed plan work",
                    theme.warn,
                )),
                ProjectedTaskTruthState::Unavailable => Some((
                    "Plan work is unavailable · no plan work is inferred",
                    theme.warn,
                )),
            },
        }
    }

    fn task_lines(&self, width: u16) -> RenderedTaskLines {
        match &self.projection {
            TaskBoardProjection::Single { snapshot, .. } if snapshot.tasks.is_empty() => {
                RenderedTaskLines::plain(vec![self.empty_task_state_line(false)])
            }
            TaskBoardProjection::Single { snapshot, .. } => {
                let rendered = crate::tui::task_list::render_full_focused(
                    &snapshot.tasks,
                    width,
                    true,
                    self.focused_task_id.as_deref(),
                );
                RenderedTaskLines {
                    lines: rendered.lines,
                    focused_line_index: rendered.selected_line_index,
                }
            }
            TaskBoardProjection::All { snapshot, .. }
                if snapshot
                    .per_session
                    .iter()
                    .all(|(_, tasks)| tasks.is_empty()) =>
            {
                RenderedTaskLines::plain(vec![self.empty_task_state_line(true)])
            }
            TaskBoardProjection::All { snapshot, .. } => RenderedTaskLines::plain(
                crate::tui::task_list::render_multi_full(&snapshot.per_session, width),
            ),
        }
    }

    /// An empty projection is only useful if it says what was actually
    /// observed. In particular, a failed or in-flight provider read must
    /// never masquerade as "no tasks".
    fn empty_task_state_line(&self, all_sessions: bool) -> Line<'static> {
        let theme = crate::tui::theme::current();
        let (text, color) = match self.projection.truth_state() {
            TaskBoardTruthState::Unbound => ("No session is bound to this task board.", theme.dim),
            TaskBoardTruthState::Loading | TaskBoardTruthState::Refreshing => (
                "Task state is syncing; no empty result is inferred yet.",
                theme.dim,
            ),
            TaskBoardTruthState::Stale => (
                "Task sync is delayed; the last confirmed task state was empty.",
                theme.warn,
            ),
            TaskBoardTruthState::Unavailable => (
                "Task state is unavailable; no tasks are inferred.",
                theme.warn,
            ),
            TaskBoardTruthState::Confirmed => {
                let projected_truth = match &self.projection {
                    TaskBoardProjection::Single {
                        projected_truth_state,
                        ..
                    }
                    | TaskBoardProjection::All {
                        projected_truth_state,
                        ..
                    } => *projected_truth_state,
                };
                match projected_truth {
                    ProjectedTaskTruthState::Loading => {
                        ("No checklist tasks yet; plan work is syncing.", theme.dim)
                    }
                    ProjectedTaskTruthState::Unavailable => (
                        "No checklist tasks; plan work is currently unavailable.",
                        theme.warn,
                    ),
                    _ if all_sessions => ("No open tasks across available sessions.", theme.dim),
                    _ => ("No tasks in this session.", theme.dim),
                }
            }
        };
        Line::from(Span::styled(text, Style::default().fg(color)))
    }

    fn current_session_tasks(&self) -> Option<&[SessionTask]> {
        match &self.projection {
            TaskBoardProjection::Single { snapshot, .. } => Some(&snapshot.tasks),
            TaskBoardProjection::All { .. } => None,
        }
    }

    fn reconcile_focus(&mut self) {
        let still_present = self.focused_task_id.as_deref().is_some_and(|task_id| {
            self.current_session_tasks()
                .is_some_and(|tasks| tasks.iter().any(|task| task.id == task_id))
        });
        if !still_present {
            self.focused_task_id = None;
        }
    }

    fn focus_next_task(&mut self, reverse: bool) {
        let Some(tasks) = self.current_session_tasks() else {
            return;
        };
        if tasks.is_empty() {
            self.focused_task_id = None;
            return;
        }
        let current = self
            .focused_task_id
            .as_deref()
            .and_then(|id| tasks.iter().position(|task| task.id == id));
        let next = match (current, reverse) {
            (Some(index), false) => (index + 1) % tasks.len(),
            (Some(0), true) => tasks.len() - 1,
            (Some(index), true) => index - 1,
            (None, _) => 0,
        };
        self.focused_task_id = Some(tasks[next].id.clone());
        self.follow_focused_task.set(true);
    }

    fn open_focused_task_detail(&mut self) {
        let Some(task_id) = self.focused_task_id.as_deref() else {
            return;
        };
        let Some(task) = self
            .current_session_tasks()
            .and_then(|tasks| tasks.iter().find(|task| task.id == task_id))
        else {
            return;
        };
        self.detail = Some((task.id.clone(), TaskDetailView::from_session_task(task)));
    }

    fn reconcile_detail(&mut self) {
        let Some(task_id) = self.detail.as_ref().map(|(task_id, _)| task_id.clone()) else {
            return;
        };
        let refreshed = self.current_session_tasks().and_then(|tasks| {
            tasks
                .iter()
                .find(|task| task.id == task_id)
                .map(TaskDetailView::from_session_task)
        });
        self.detail = refreshed.map(|detail| (task_id, detail));
    }
}

impl BottomPaneView for TaskBoardView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some((_, detail)) = &self.detail {
            detail.render(area, buf);
            return;
        }
        if area.width < 10 || area.height == 0 {
            return;
        }
        let theme = crate::tui::theme::current();
        let (truth, color) = self.truth_line();
        let mut next_row = 1;
        let projected_truth = self.projected_truth_line();
        if area.height > next_row {
            next_row = next_row.saturating_add(1);
        }
        if projected_truth.is_some() && area.height > next_row {
            next_row = next_row.saturating_add(1);
        }

        let task_render = self.task_lines(area.width);
        let task_lines = task_render.lines;
        let task_y = area.y.saturating_add(next_row.saturating_add(1));
        let viewport_rows = area.bottom().saturating_sub(task_y) as usize;
        let max_scroll = task_lines.len().saturating_sub(viewport_rows);
        let mut start = self.scroll_offset.get().min(max_scroll);
        if self.follow_focused_task.replace(false)
            && let Some(focused_line_index) = task_render.focused_line_index
        {
            if focused_line_index < start {
                start = focused_line_index;
            } else if viewport_rows > 0 && focused_line_index >= start + viewport_rows {
                start = focused_line_index + 1 - viewport_rows;
            }
            start = start.min(max_scroll);
            self.scroll_offset.set(start);
        }
        let end = start.saturating_add(viewport_rows).min(task_lines.len());
        let title_suffix = (viewport_rows > 0 && task_lines.len() > viewport_rows)
            .then(|| format!(" · {}–{} of {}", start + 1, end, task_lines.len()));
        let focused_suffix = self
            .focused_task_id
            .as_deref()
            .map(|task_id| format!(" · focus {task_id}"))
            .unwrap_or_default();
        Line::from(Span::styled(
            format!(
                "  {}{}{}",
                self.title(),
                focused_suffix,
                title_suffix.unwrap_or_default()
            ),
            Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
        ))
        .render(Rect::new(area.x, area.y, area.width, 1), buf);

        if area.height > 1 {
            Line::from(Span::styled(
                format!("  {truth}"),
                Style::default().fg(color),
            ))
            .render(Rect::new(area.x, area.y + 1, area.width, 1), buf);
        }
        if let Some((projected_truth, projected_color)) = projected_truth
            && area.height > 2
        {
            Line::from(Span::styled(
                format!("  {projected_truth}"),
                Style::default().fg(projected_color),
            ))
            .render(Rect::new(area.x, area.y + 2, area.width, 1), buf);
        }

        let mut y = task_y;
        for line in task_lines.into_iter().skip(start).take(viewport_rows) {
            line.render(Rect::new(area.x, y, area.width, 1), buf);
            y = y.saturating_add(1);
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        if let Some((_, detail)) = &self.detail {
            return detail.desired_height(_width);
        }
        16
    }

    fn handle_key(&mut self, key: KeyEvent) {
        if let Some((_, detail)) = &mut self.detail {
            detail.handle_key(key);
            if detail.is_complete() {
                self.detail = None;
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => self.completed = true,
            KeyCode::Tab => self.focus_next_task(false),
            KeyCode::BackTab => self.focus_next_task(true),
            KeyCode::Enter => self.open_focused_task_detail(),
            KeyCode::Up | KeyCode::Char('k') => {
                self.follow_focused_task.set(false);
                self.scroll_offset
                    .set(self.scroll_offset.get().saturating_sub(1));
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.follow_focused_task.set(false);
                self.scroll_offset
                    .set(self.scroll_offset.get().saturating_add(1));
            }
            KeyCode::PageUp => {
                self.follow_focused_task.set(false);
                self.scroll_offset
                    .set(self.scroll_offset.get().saturating_sub(8));
            }
            KeyCode::PageDown => {
                self.follow_focused_task.set(false);
                self.scroll_offset
                    .set(self.scroll_offset.get().saturating_add(8));
            }
            KeyCode::Home => {
                self.follow_focused_task.set(false);
                self.scroll_offset.set(0);
            }
            KeyCode::End => {
                self.follow_focused_task.set(false);
                self.scroll_offset.set(usize::MAX);
            }
            KeyCode::Char('r') | KeyCode::Char('R')
                if self.projection.truth_state() != TaskBoardTruthState::Unbound =>
            {
                self.pending_action = Some(ViewActionRequest {
                    action: BottomPaneViewAction::RefreshTaskBoard,
                    disposition: ViewActionDisposition::KeepOpen,
                });
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        if let Some((_, detail)) = &mut self.detail {
            let event = detail.on_ctrl_c();
            self.detail = None;
            return event;
        }
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action.take()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.completed.then_some(ViewCompletion {
            result: None,
            reopen: None,
        })
    }

    fn refresh_task_board(&mut self, projection: &TaskBoardProjection) -> bool {
        if self.projection.same_render_state(projection) {
            return false;
        }
        self.projection = projection.clone();
        self.reconcile_focus();
        self.reconcile_detail();
        true
    }

    fn hint_keys(&self) -> Option<String> {
        if let Some((_, detail)) = &self.detail {
            return detail.hint_keys();
        }
        match self.projection {
            TaskBoardProjection::Single { .. } => Some(
                "↑↓ scroll · Tab focus · Enter details · R refresh · PgUp/PgDn page · Home/End · ←/Esc return"
                    .into(),
            ),
            TaskBoardProjection::All { .. } => {
                Some("↑↓ scroll · R refresh · PgUp/PgDn page · Home/End · ←/Esc return".into())
            }
        }
    }

    fn owns_primary_canvas(&self) -> bool {
        true
    }

    fn is_task_board_view(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::TaskBoardView;
    use crate::tui::bottom_pane::view::{
        BottomPaneView, BottomPaneViewAction, ViewActionDisposition,
    };
    use crate::tui::task_board_observer::{
        ProjectedTaskTruthState, TaskBoardProjection, TaskBoardSnapshot, TaskBoardTruthState,
    };
    use astra_tools::task_mgmt::{SessionTask, TaskStoreHealth};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{buffer::Buffer, layout::Rect};

    fn task(id: usize) -> SessionTask {
        SessionTask {
            archived_at: None,
            id: format!("task-{id}"),
            title: format!("row-{id:02}"),
            description: None,
            status: "pending".into(),
            subtasks: Vec::new(),
            created_at: "now".into(),
            updated_at: "now".into(),
            active_form: None,
            owner: None,
            metadata: None,
            blocks: Vec::new(),
            blocked_by: Vec::new(),
        }
    }

    fn render(view: &TaskBoardView) -> String {
        let area = Rect::new(0, 0, 80, 8);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        crate::tui::testing::render::buffer_to_string(&buffer)
    }

    #[test]
    fn empty_projection_explains_absence_without_claiming_a_failure() {
        let view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot::default(),
        });
        let area = Rect::new(0, 0, 80, 16);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);

        assert!(text.contains("Task board · This session"), "{text}");
        assert!(text.contains("Canonical task state"), "{text}");
        assert!(text.contains("No tasks in this session."), "{text}");
    }

    #[test]
    fn empty_loading_or_unavailable_projection_never_claims_no_tasks() {
        let loading = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Loading,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot::default(),
        });
        let loading_text = render(&loading);
        assert!(
            loading_text.contains("Task state is syncing"),
            "{loading_text}"
        );
        assert!(
            !loading_text.contains("No tasks in this session."),
            "{loading_text}"
        );

        let unavailable = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Unavailable,
            store_health: TaskStoreHealth::ServiceUnavailable,
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot::default(),
        });
        let unavailable_text = render(&unavailable);
        assert!(
            unavailable_text.contains("Task state is unavailable; no tasks are inferred."),
            "{unavailable_text}"
        );
        assert!(
            !unavailable_text.contains("No tasks in this session."),
            "{unavailable_text}"
        );
    }

    #[test]
    fn refresh_replaces_the_visible_truth_projection() {
        let mut view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot::default(),
        });
        let unavailable = TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Unavailable,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot::default(),
        };
        assert!(view.refresh_task_board(&unavailable));
        assert!(
            !view.refresh_task_board(&unavailable),
            "an unchanged projection must not schedule a redundant redraw"
        );

        let area = Rect::new(0, 0, 80, 16);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(text.contains("Checklist service is unavailable"), "{text}");
    }

    #[test]
    fn degraded_board_exposes_a_typed_manual_refresh_action() {
        let mut view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Unavailable,
            store_health: TaskStoreHealth::ServiceUnavailable,
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot::default(),
        });

        let rendered = render(&view);
        assert!(rendered.contains("R refresh"), "{rendered}");
        assert!(
            view.hint_keys()
                .expect("task board hints")
                .contains("R refresh")
        );

        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(super::ViewActionRequest {
                action: BottomPaneViewAction::RefreshTaskBoard,
                disposition: ViewActionDisposition::KeepOpen,
            })
        ));
    }

    #[test]
    fn confirmed_plan_work_is_not_relabeled_as_an_unavailable_task_board() {
        let view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Unavailable,
            store_health: TaskStoreHealth::ServiceUnavailable,
            projected_truth_state: ProjectedTaskTruthState::Confirmed,
            snapshot: TaskBoardSnapshot {
                tasks: vec![task(1)],
                hidden: false,
            },
        });

        let area = Rect::new(0, 0, 120, 16);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);

        assert!(
            text.contains("Checklist service is unavailable · plan work remains confirmed"),
            "{text}"
        );
        assert!(!text.contains("no checklist state is inferred"), "{text}");
        assert!(text.contains("row-01"), "{text}");
    }

    #[test]
    fn stale_plan_projection_is_distinguished_from_checklist_truth() {
        let view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::Stale,
            snapshot: TaskBoardSnapshot::default(),
        });
        let area = Rect::new(0, 0, 100, 16);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        let text = crate::tui::testing::render::buffer_to_string(&buffer);

        assert!(text.contains("Canonical task state"), "{text}");
        assert!(text.contains("showing last confirmed plan work"), "{text}");
    }

    #[test]
    fn primary_board_scrolls_the_complete_fetched_projection() {
        let mut view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot {
                tasks: (1..=14).map(task).collect(),
                hidden: false,
            },
        });

        let initial = render(&view);
        assert!(initial.contains("row-01"), "{initial}");
        assert!(!initial.contains("row-12"), "{initial}");

        view.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));
        let paged = render(&view);
        assert!(paged.contains("row-12"), "{paged}");
        assert!(!paged.contains("row-01"), "{paged}");
        assert!(paged.contains("9–13 of 15"), "{paged}");

        view.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        let at_end = render(&view);
        assert!(at_end.contains("row-14"), "{at_end}");
        assert!(at_end.contains("11–15 of 15"), "{at_end}");
    }

    #[test]
    fn current_session_focus_opens_details_by_stable_task_identity() {
        let mut view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot {
                tasks: vec![task(1), task(2)],
                hidden: false,
            },
        });

        view.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(view.focused_task_id.as_deref(), Some("task-1"));
        let focused = render(&view);
        assert!(
            focused.contains("› • row-01") || focused.contains("› ◻ row-01"),
            "{focused}"
        );

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            view.detail.as_ref().map(|(task_id, _)| task_id.as_str()),
            Some("task-1")
        );

        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(view.detail.is_none());
        assert!(
            !view.completed,
            "Esc returns from task detail before closing board"
        );

        assert!(view.refresh_task_board(&TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot {
                tasks: vec![task(2)],
                hidden: false,
            },
        }));
        assert!(
            view.focused_task_id.is_none(),
            "a refresh must not transfer focus to a different task row"
        );
    }

    #[test]
    fn terminal_cancelled_task_remains_visible_and_inspectable() {
        let mut cancelled = task(1);
        cancelled.title = "cancelled deployment".into();
        cancelled.status = "cancelled".into();
        let mut view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot {
                tasks: vec![cancelled],
                hidden: false,
            },
        });

        let rendered = render(&view);
        assert!(rendered.contains("cancelled deployment"), "{rendered}");

        view.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let detail = view.detail.as_ref().expect("cancelled task detail opens");
        assert_eq!(detail.0, "task-1");
    }

    #[test]
    fn keyboard_focus_reveals_the_selected_task_without_overriding_manual_scroll() {
        let mut view = TaskBoardView::new(TaskBoardProjection::Single {
            truth_state: TaskBoardTruthState::Confirmed,
            store_health: TaskStoreHealth::default(),
            projected_truth_state: ProjectedTaskTruthState::NotConfigured,
            snapshot: TaskBoardSnapshot {
                tasks: (1..=14).map(task).collect(),
                hidden: false,
            },
        });

        for _ in 0..14 {
            view.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        }
        let followed = render(&view);
        assert!(followed.contains("› ◻ row-14"), "{followed}");
        assert!(followed.contains("11–15 of 15"), "{followed}");

        view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let manually_scrolled = render(&view);
        assert!(manually_scrolled.contains("row-01"), "{manually_scrolled}");
        assert!(
            !manually_scrolled.contains("› ◻ row-14"),
            "manual scroll must not be stolen by a previously focused row: {manually_scrolled}"
        );
    }
}
