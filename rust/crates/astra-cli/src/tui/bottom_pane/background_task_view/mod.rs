//! Background task switcher.
//!
//! Opened from the status-line task chip. It gives the user a typed,
//! keyboard-driven view over local background shell tasks promoted by
//! Ctrl+B, including detail/tail inspection and stop actions.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{buffer::Buffer, layout::Rect};

pub(crate) mod detail_render;
pub(crate) mod fanout_header;
pub(crate) mod list_render;
pub(crate) mod types;

pub(crate) use types::{
    parse_output_sentinel, parse_stop_sentinel, BackgroundTaskFanoutMembership,
    BackgroundTaskKind, BackgroundTaskRow, BackgroundTaskRowInit, BackgroundTaskStatus,
    LiveControlState, Mode, BACKGROUND_TASK_OUTPUT_SENTINEL, BACKGROUND_TASK_STOP_SENTINEL,
};

use list_render::background_task_list_entries;
use types::{sort_rows, PAGE_STEP};

use super::view::{BottomPaneView, CancellationEvent};

pub(crate) struct BackgroundTaskView {
    rows: Vec<BackgroundTaskRow>,
    selected: usize,
    completed: bool,
    mode: Mode,
    pending_action: Option<String>,
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
        let indices = self.selectable_row_indices();
        if indices.is_empty() {
            return;
        }
        let pos = self.selected_visual_position(&indices).unwrap_or(0);
        let next_pos = if pos == 0 { indices.len() - 1 } else { pos - 1 };
        self.selected = indices[next_pos];
    }

    fn move_down(&mut self) {
        let indices = self.selectable_row_indices();
        if indices.is_empty() {
            return;
        }
        let pos = self.selected_visual_position(&indices).unwrap_or(0);
        self.selected = indices[(pos + 1) % indices.len()];
    }

    fn move_page_up(&mut self) {
        let indices = self.selectable_row_indices();
        if indices.is_empty() {
            return;
        }
        let pos = self.selected_visual_position(&indices).unwrap_or(0);
        self.selected = indices[pos.saturating_sub(PAGE_STEP)];
    }

    fn move_page_down(&mut self) {
        let indices = self.selectable_row_indices();
        if indices.is_empty() {
            return;
        }
        let pos = self.selected_visual_position(&indices).unwrap_or(0);
        self.selected = indices[pos.saturating_add(PAGE_STEP).min(indices.len() - 1)];
    }

    fn select_home(&mut self) {
        if let Some(idx) = self.selectable_row_indices().first() {
            self.selected = *idx;
        }
    }

    fn select_end(&mut self) {
        if let Some(idx) = self.selectable_row_indices().last() {
            self.selected = *idx;
        }
    }

    fn select_visible_ordinal(&mut self, ordinal: usize) {
        if let Some(idx) = self.selectable_row_indices().get(ordinal) {
            self.selected = *idx;
        }
    }

    fn selectable_row_indices(&self) -> Vec<usize> {
        background_task_list_entries(&self.rows)
            .into_iter()
            .filter_map(|entry| entry.row_index())
            .collect()
    }

    fn selected_visual_position(&self, indices: &[usize]) -> Option<usize> {
        indices.iter().position(|idx| *idx == self.selected)
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
}

impl BottomPaneView for BackgroundTaskView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        match self.mode {
            Mode::List => list_render::render_list(&self.rows, self.selected, area, buf),
            Mode::Detail => detail_render::render_detail(
                self.selected_row(),
                area,
                buf,
                |fallback_area, fallback_buf| {
                    list_render::render_list(&self.rows, self.selected, fallback_area, fallback_buf);
                },
            ),
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
                KeyCode::Home => self.select_home(),
                KeyCode::End if !self.rows.is_empty() => self.select_end(),
                KeyCode::Char(ch) if ('1'..='9').contains(&ch) => {
                    self.select_visible_ordinal(ch as usize - '1' as usize);
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('o') | KeyCode::Char('O')
                    if !self.rows.is_empty() =>
                {
                    self.mode = Mode::Detail;
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
            BackgroundTaskRowInit::new(id, kind, status, 1500, title).with_output(
                Some(format!("/tmp/{id}.stdout")),
                Some("line one\nline two".to_string()),
                Some(17),
            ),
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
            typed_row("agent-1", BackgroundTaskKind::LocalAgent, "waiting_for_input", "review auth flow"),
            typed_row("cloud-1", BackgroundTaskKind::CloudSession, "running", "remote plan run"),
            typed_row("main-1", BackgroundTaskKind::MainSession, "failed", "main turn"),
            typed_row("mon-1", BackgroundTaskKind::Monitor, "running", "watch tests"),
        ]);

        let text = render(&view, 120, 8);
        assert!(text.contains("local agent"), "{text}");
        assert!(text.contains("cloud session"), "{text}");
        assert!(text.contains("main session"), "{text}");
        assert!(text.contains("monitor"), "{text}");
        assert!(text.find("agent-1").unwrap() < text.find("cloud-1").unwrap(),
            "attention rows must still sort above plain running rows: {text}");
        assert!(text.find("main-1").unwrap() < text.find("cloud-1").unwrap(),
            "failed rows must still sort above plain running rows: {text}");
    }

    #[test]
    fn list_groups_fanout_local_agents_under_target_header() {
        let view = BackgroundTaskView::new(vec![
            typed_row("agent-auth", BackgroundTaskKind::LocalAgent, "running", "auth review")
                .with_fanout(fanout("review-1", 3, 0)),
            typed_row("agent-storage", BackgroundTaskKind::LocalAgent, "killed", "storage review")
                .with_fanout(fanout("review-1", 3, 1)),
            typed_row("agent-api", BackgroundTaskKind::LocalAgent, "running", "API review")
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
    fn list_navigation_follows_visible_fanout_grouping_order() {
        let mut view = BackgroundTaskView::new(vec![
            typed_row("agent-failed", BackgroundTaskKind::LocalAgent, "failed", "failed review")
                .with_fanout(fanout("review-1", 2, 0)),
            typed_row("standalone", BackgroundTaskKind::Shell, "running", "cargo test"),
            typed_row("agent-done", BackgroundTaskKind::LocalAgent, "completed", "completed review")
                .with_fanout(fanout("review-1", 2, 1)),
        ]);

        assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("agent-failed"));

        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("agent-done"));

        view.handle_key(key(KeyCode::Char('3')));
        assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("standalone"));
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
        assert!(!text.contains("bg-shell-1234567890abcdef"),
            "long id should be compacted before it hides the command: {text}");
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
            BackgroundTaskRow::shell("fail", "failed", 41_200, "npm test",
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
        let mut view = BackgroundTaskView::new(vec![row("wait", "waiting_for_input", "prompting command")]);

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
        let mut view = BackgroundTaskView::new(vec![row_without_output("queued", "pending", "queued task")]);

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
        assert!(detail.contains("Unavailable · stale handle or unsupported runner"), "{detail}");
        assert!(!detail.contains("No output captured yet"), "{detail}");
        assert!(detail.contains("actions: output · return"), "{detail}");
        assert!(!detail.contains("stop"), "{detail}");

        view.handle_key(key(KeyCode::Char('s')));
        assert!(view.take_pending_action().is_none(),
            "stale handles must not emit stop actions");
    }

    #[test]
    fn stop_action_emits_selected_task_id_and_keeps_view_open() {
        let mut view = BackgroundTaskView::new(vec![
            row("first", "running", "first command"),
            row("second", "running", "second command"),
        ]);
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char('s')));

        assert_eq!(view.take_pending_action().as_deref(), Some("__background_task_stop__\nsecond"));
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

        assert_eq!(view.selected_row().map(|row| row.id.as_str()), Some("second"));
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
        assert_eq!(parse_stop_sentinel("__background_task_stop__\nbg-shell-1"), Some("bg-shell-1"));
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

        assert_eq!(view.take_pending_action().as_deref(), Some("__background_task_output__\nsecond"));
        assert!(view.take_pending_action().is_none());
        assert!(!view.is_complete());
    }

    #[test]
    fn parse_output_sentinel_extracts_id() {
        assert_eq!(parse_output_sentinel("__background_task_output__\nbg-shell-1"), Some("bg-shell-1"));
        assert_eq!(parse_output_sentinel("not it"), None);
        assert_eq!(parse_output_sentinel(BACKGROUND_TASK_OUTPUT_SENTINEL), None);
    }

    #[test]
    fn local_agent_detail_offers_typed_output_action() {
        let mut view = BackgroundTaskView::new(vec![
            typed_row("agent-1", BackgroundTaskKind::LocalAgent, "running", "review auth flow"),
        ]);
        view.handle_key(key(KeyCode::Enter));

        let detail = render(&view, 80, 12);
        assert!(detail.contains("actions: output · stop · return"), "{detail}");
        assert!(detail.contains("task review auth flow"), "{detail}");

        view.handle_key(key(KeyCode::Char('o')));
        assert!(matches!(view.take_pending_action().as_deref(),
            Some("__background_task_output__\nagent-1")),
            "local agent output should emit typed task_output sentinel");

        view.handle_key(key(KeyCode::Char('s')));
        assert_eq!(view.take_pending_action().as_deref(),
            Some("__background_task_stop__\nagent-1"));
    }
}
