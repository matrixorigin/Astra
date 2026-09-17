//! Pixel-ish render snapshots for the task board widget.
//!
//! Snapshots live in `snapshots/` and are locked in by `insta`. Any
//! change to glyphs, spacing, ordering, truncation, or the header
//! template shows up as a reviewable diff here. These are the
//! load-bearing "what the user actually sees" artefacts — if the
//! diff looks wrong to a human, the widget changed in a way the
//! unit tests didn't catch.

#![cfg(test)]

use super::super::work_board_projection::SessionTask;
use super::render;
use crate::tui::testing::render::{buffer_to_string, draw_widget};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Widget};

struct LinesWidget(Vec<Line<'static>>);
impl Widget for LinesWidget {
    fn render(self, area: Rect, buf: &mut Buffer) {
        Paragraph::new(ratatui::text::Text::from(self.0)).render(area, buf);
    }
}

fn mk_task(id: &str, title: &str, status: &str) -> SessionTask {
    SessionTask {
        id: id.into(),
        title: title.into(),
        description: None,
        status: status.into(),
        subtasks: vec![],
        created_at: "2026-05-10T00:00:00Z".into(),
        updated_at: "2026-05-10T00:00:00Z".into(),
        active_form: None,
        owner: None,
        metadata: None,
        blocks: vec![],
        blocked_by: vec![],
    }
}

fn with_blockers(mut t: SessionTask, blockers: &[&str]) -> SessionTask {
    t.blocked_by = blockers.iter().map(|s| (*s).to_string()).collect();
    t
}

/// Render `tasks` into a buffer of the given size and stringify. The
/// buffer height is sized so truncation/`maxDisplay` behaviour shows
/// up in the snapshot (e.g. 24 rows → max 10 task lines + header).
fn draw(tasks: &[SessionTask], w: u16, h: u16, standalone: bool) -> String {
    let lines = render(tasks, w, h, standalone);
    let buf = draw_widget(LinesWidget(lines), w, h);
    buffer_to_string(&buf)
}

// ─── Empty + hidden ───────────────────────────────────────────────

#[test]
fn snapshot_empty_list_renders_nothing() {
    crate::tui::testing::assert_tui_snapshot!("task_board_empty_80", draw(&[], 80, 24, true));
}

#[test]
fn snapshot_hidden_when_rows_too_small() {
    // rows=10 triggers the hidden guard → zero output.
    let tasks = vec![mk_task("task-1", "probe", "in_progress")];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_rows_10_hidden_80",
        draw(&tasks, 80, 10, true)
    );
}

// ─── Standalone header ────────────────────────────────────────────

#[test]
fn snapshot_standalone_header_counts_open_done_inprogress() {
    let tasks = vec![
        mk_task("task-1", "refactor auth module", "completed"),
        mk_task("task-2", "write unit tests", "in_progress"),
        mk_task("task-3", "update the docs", "pending"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_standalone_mixed_80",
        draw(&tasks, 80, 24, true)
    );
}

#[test]
fn snapshot_non_standalone_omits_header() {
    // Same data, `standalone=false` — header line gone, icons only.
    let tasks = vec![
        mk_task("task-1", "refactor auth module", "completed"),
        mk_task("task-2", "write unit tests", "in_progress"),
        mk_task("task-3", "update the docs", "pending"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_inline_mixed_80",
        draw(&tasks, 80, 24, false)
    );
}

// ─── Priority ordering ────────────────────────────────────────────

#[test]
fn snapshot_priority_orders_in_progress_first() {
    // Input is deliberately disorder; output must put in_progress on
    // top, pending in the middle, completed last, each group sorted
    // by numeric id.
    let tasks = vec![
        mk_task("task-3", "first-completed", "completed"),
        mk_task("task-1", "first-pending", "pending"),
        mk_task("task-2", "first-in-progress", "in_progress"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_priority_order_80",
        draw(&tasks, 80, 24, false)
    );
}

#[test]
fn snapshot_blocked_pending_sorts_after_unblocked_pending() {
    // Within pending, blocked tasks sort last. #2 is blocked by #3;
    // #1 and #4 are free. Expected order: #1, #4, #2 (then #3 lives
    // in the in_progress bucket above).
    let mut b = mk_task("task-2", "blocked-work", "pending");
    b.blocked_by = vec!["task-3".into()];
    let tasks = vec![
        mk_task("task-1", "free-work-a", "pending"),
        b,
        mk_task("task-3", "blocker-running", "in_progress"),
        mk_task("task-4", "free-work-b", "pending"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_blocked_last_within_pending_80",
        draw(&tasks, 80, 24, false)
    );
}

// ─── Blocked-by badge ─────────────────────────────────────────────

#[test]
fn snapshot_blocked_by_badge_lists_blocker_ids() {
    let blocker_a = mk_task("task-1", "prep-a", "in_progress");
    let blocked = with_blockers(
        mk_task("task-2", "downstream-work", "pending"),
        &["task-1", "task-3"],
    );
    let blocker_c = mk_task("task-3", "prep-c", "pending");
    let tasks = vec![blocker_a, blocked, blocker_c];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_blocked_by_badge_80",
        draw(&tasks, 80, 24, false)
    );
}

// ─── Truncation ───────────────────────────────────────────────────

#[test]
fn snapshot_truncation_appends_hidden_summary() {
    // 15 pending tasks, rows=24 → maxDisplay = min(10, max(3, 10)) = 10
    // → 10 visible + "… +5 pending" summary line.
    let tasks: Vec<_> = (1..=15)
        .map(|i| mk_task(&format!("task-{i}"), &format!("task number {i}"), "pending"))
        .collect();
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_truncated_15_in_24_rows_80",
        draw(&tasks, 80, 24, false)
    );
}

#[test]
fn snapshot_truncation_at_narrow_height_shows_only_three() {
    // rows=17 → maxDisplay = max(3, rows - 14) = 3. Five tasks → 3
    // visible + summary.
    let tasks = vec![
        mk_task("task-1", "one", "in_progress"),
        mk_task("task-2", "two", "pending"),
        mk_task("task-3", "three", "pending"),
        mk_task("task-4", "four", "pending"),
        mk_task("task-5", "five", "completed"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_truncated_5_in_17_rows_80",
        draw(&tasks, 80, 17, false)
    );
}

// ─── Responsive subject truncation ────────────────────────────────

#[test]
fn snapshot_narrow_width_truncates_long_subjects() {
    // 40-col terminal. Long subjects should get "…" suffix.
    let tasks = vec![
        mk_task(
            "task-1",
            "refactor the entire authentication module and write tests",
            "in_progress",
        ),
        mk_task(
            "task-2",
            "another very long task title that won't fit on a narrow screen",
            "pending",
        ),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_subject_truncated_40",
        draw(&tasks, 40, 24, true)
    );
}

// ─── CJK width ────────────────────────────────────────────────────

#[test]
fn snapshot_cjk_subjects_width_accounted() {
    // CJK chars are 2 cols wide. Ensure they render without layout
    // corruption at a medium terminal width.
    let tasks = vec![
        mk_task("task-1", "重构认证模块", "in_progress"),
        mk_task("task-2", "编写单元测试", "pending"),
        mk_task("task-3", "更新文档并提交", "completed"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_cjk_subjects_60",
        draw(&tasks, 60, 24, true)
    );
}

// ─── Next-hint (collapsed state) ──────────────────────────────────

#[test]
fn snapshot_next_hint_picks_in_progress_first() {
    let tasks = vec![
        mk_task("task-1", "pending-work", "pending"),
        mk_task("task-2", "running-work", "in_progress"),
    ];
    let hint = super::render_next_hint(&tasks, 80).expect("hint");
    let buf = draw_widget(LinesWidget(vec![hint]), 80, 1);
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_next_hint_in_progress_80",
        buffer_to_string(&buf)
    );
}

#[test]
fn snapshot_next_hint_falls_back_to_pending() {
    let tasks = vec![mk_task("task-1", "waiting-work", "pending")];
    let hint = super::render_next_hint(&tasks, 80).expect("hint");
    let buf = draw_widget(LinesWidget(vec![hint]), 80, 1);
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_next_hint_pending_80",
        buffer_to_string(&buf)
    );
}

#[test]
fn snapshot_narrow_header_drops_ctrlt_hint() {
    // Header counts already consume most of a 45-col line; the Ctrl+T
    // hint must NOT render here (would wrap + look like garbage).
    let tasks = vec![
        mk_task("task-1", "a", "completed"),
        mk_task("task-2", "b", "in_progress"),
        mk_task("task-3", "c", "pending"),
    ];
    crate::tui::testing::assert_tui_snapshot!(
        "task_board_narrow_header_45",
        draw(&tasks, 45, 24, true)
    );
}
