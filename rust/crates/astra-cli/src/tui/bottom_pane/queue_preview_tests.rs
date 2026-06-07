#![cfg(test)]

use super::BottomPane;
use crate::tui::task_status::TaskStatus;
use ratatui::{buffer::Buffer, layout::Rect};
use std::time::Instant;

fn render_text(pane: &BottomPane, area: Rect) -> String {
    let mut buf = Buffer::empty(area);
    pane.render(area, &mut buf);
    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

fn snapshot_text(pane: &BottomPane, area: Rect) -> String {
    render_text(pane, area)
}

fn seed_footer(pane: &mut BottomPane) {
    pane.footer.model = Some("sonnet-4.6".into());
    pane.footer.cwd = Some("~/github/astra".into());
    pane.footer.git_branch = Some("enqueue_new_after_next_call".into());
}

#[test]
fn active_turn_placeholder_explains_queue_vs_interrupt() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    seed_footer(&mut pane);

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("Message astra"),
        "active composer should keep the same primary prompt as idle; got {rendered:?}"
    );
    assert!(
        rendered.contains("Queued until next tool call") || rendered.contains("Ctrl+C stops"),
        "active composer should keep an in-panel helper hint; got {rendered:?}"
    );
    assert!(
        rendered.contains("sonnet-4.6")
            || rendered.contains("github/astra")
            || rendered.contains("enqueue"),
        "status footer should remain visible under the composer; got {rendered:?}"
    );
}

#[test]
fn idle_composer_uses_clean_prompt_and_editor_hint() {
    let pane = BottomPane::new();

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("Message astra"),
        "idle composer should render a short prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Ctrl+E opens editor") || rendered.contains("Shift+Enter newline"),
        "idle composer should keep an in-panel helper hint; got {rendered:?}"
    );
}

#[test]
fn narrow_active_composer_degrades_helper_without_losing_stop_hint() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });

    let rendered = render_text(&pane, Rect::new(0, 0, 42, 5));
    assert!(
        rendered.contains("Message astra"),
        "narrow composer should keep the primary prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Ctrl+C stops") || rendered.contains("next tool"),
        "narrow composer should keep a compressed helper hint; got {rendered:?}"
    );
    assert!(
        rendered
            .lines()
            .any(|line| line.contains("~/") || line.contains("sonnet-4.6")),
        "narrow footer should stay visible under the composer; got {rendered:?}"
    );
}

#[test]
fn helper_row_stays_visible_after_typing_begins() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    pane.composer.set_text("review the latest diff");

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 5));
    assert!(
        rendered.contains("review the latest diff"),
        "typed text should remain visible inside the composer; got {rendered:?}"
    );
    assert!(
        rendered.contains("Queued until next tool call") || rendered.contains("Ctrl+C stops"),
        "the helper row should stay visible after typing begins; got {rendered:?}"
    );
}

#[test]
fn snapshot_idle_bottom_surface_80() {
    let mut pane = BottomPane::new();
    seed_footer(&mut pane);
    crate::tui::testing::assert_tui_snapshot!(
        "bottom_surface_idle_80",
        snapshot_text(&pane, Rect::new(0, 0, 80, 5))
    );
}

#[test]
fn snapshot_active_bottom_surface_80() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    seed_footer(&mut pane);
    crate::tui::testing::assert_tui_snapshot!(
        "bottom_surface_active_80",
        snapshot_text(&pane, Rect::new(0, 0, 80, 5))
    );
}

#[test]
fn snapshot_active_bottom_surface_narrow_42() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    seed_footer(&mut pane);
    crate::tui::testing::assert_tui_snapshot!(
        "bottom_surface_active_42",
        snapshot_text(&pane, Rect::new(0, 0, 42, 5))
    );
}
