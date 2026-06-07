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

#[test]
fn active_turn_placeholder_explains_queue_vs_interrupt() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 4));
    assert!(
        rendered.contains("Send follow-up"),
        "active composer should use a short primary prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Queued after next tool call"),
        "active composer should explain queued delivery boundary in the helper row; got {rendered:?}"
    );
    assert!(
        rendered.contains("Ctrl+C stops"),
        "active composer should advertise the real interrupt gesture; got {rendered:?}"
    );
}

#[test]
fn idle_composer_uses_clean_prompt_and_editor_hint() {
    let pane = BottomPane::new();

    let rendered = render_text(&pane, Rect::new(0, 0, 80, 4));
    assert!(
        rendered.contains("Message astra"),
        "idle composer should render a short prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Ctrl+E editor"),
        "idle helper row should carry editor guidance; got {rendered:?}"
    );
}

#[test]
fn narrow_active_composer_degrades_helper_without_losing_stop_hint() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });

    let rendered = render_text(&pane, Rect::new(0, 0, 42, 4));
    assert!(
        rendered.contains("Send follow-up"),
        "narrow composer should keep the primary prompt; got {rendered:?}"
    );
    assert!(
        rendered.contains("Ctrl+C stops"),
        "narrow helper should preserve the stop gesture; got {rendered:?}"
    );
}
