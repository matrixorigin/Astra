#![cfg(test)]

use super::BottomPane;
use super::{DeferredFollowupPop, deferred_input_preview_fingerprint};
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
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
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
        rendered.contains("Enter queues follow-up") || rendered.contains("Ctrl+C stops"),
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
        rendered.contains("Ctrl+C stops") || rendered.contains("Enter queues"),
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
        rendered.contains("Enter queues follow-up") || rendered.contains("Ctrl+C stops"),
        "the helper row should stay visible after typing begins; got {rendered:?}"
    );
}

#[test]
fn pop_applied_deferred_followup_returns_head_on_matching_preview() {
    // Contract: the server emits `__deferred_input_applied__:<preview>`
    // per dequeued item, where `<preview>` is the server's own truncation
    // of the dequeued text. The client recomputes the fingerprint from
    // its head and compares — a match means it is safe to commit the
    // head verbatim. This test pins both halves of the contract: the
    // fingerprint algorithm must match the server's, and a match must
    // pop the head regardless of how the server's truncation behaves
    // for long / multi-line / wide input.
    let mut pane = BottomPane::new();
    pane.queue_deferred_followup("first queued message");
    pane.queue_deferred_followup(
        "a very long second message whose preview would truncate differently \
         than the server's 80-char status line and therefore never match under \
         the old implementation, causing it to be dropped out of order",
    );

    // First applied signal: preview must match "first queued message".
    let first_preview = deferred_input_preview_fingerprint("first queued message");
    assert_eq!(
        pane.pop_applied_deferred_followup(&first_preview),
        DeferredFollowupPop::Applied("first queued message".to_string()),
        "matching preview must pop the head, independent of preview text"
    );
    // Second applied signal: preview recomputed from the actual head text.
    let long_text = "a very long second message whose preview would truncate differently \
         than the server's 80-char status line and therefore never match under \
         the old implementation, causing it to be dropped out of order";
    let second_preview = deferred_input_preview_fingerprint(long_text);
    match pane.pop_applied_deferred_followup(&second_preview) {
        DeferredFollowupPop::Applied(text) => assert!(
            text.starts_with("a very long second"),
            "second applied signal must pop the next head when preview matches; got {text:?}"
        ),
        other => panic!("expected Applied, got {other:?}"),
    }
    // Popping an empty queue yields Empty, not a panic or a phantom drop.
    let stray_preview = deferred_input_preview_fingerprint("stray");
    assert!(
        matches!(
            pane.pop_applied_deferred_followup(&stray_preview),
            DeferredFollowupPop::Empty
        ),
        "popping an empty queue yields Empty, not a panic or a phantom drop"
    );
}

#[test]
fn pop_applied_deferred_followup_surfaces_desync_instead_of_corrupting_history() {
    // Regression: a bare FIFO pop trusted the server's "exactly one status
    // line per item, in strict order" invariant. If the server ever drops
    // or reorders an event, every subsequent pop commits the *wrong* text
    // as the user's input. The fingerprint check turns that silent
    // corruption into a visible, recoverable failure: on mismatch the
    // entire local queue is dropped and returned as `Desync`.
    let mut pane = BottomPane::new();
    pane.queue_deferred_followup("head that should have been applied");
    pane.queue_deferred_followup("second item, now orphaned by desync");
    pane.queue_deferred_followup("third item, also orphaned");

    // Server preview does NOT match the local head — desync.
    let mismatched_preview = "something the server dequeued that we never queued";
    match pane.pop_applied_deferred_followup(&mismatched_preview) {
        DeferredFollowupPop::Desync { dropped } => {
            assert_eq!(
                dropped.len(),
                3,
                "desync must drop the entire local queue so no wrong text is committed; got {dropped:?}"
            );
            assert_eq!(dropped[0], "head that should have been applied");
            assert_eq!(dropped[2], "third item, also orphaned");
        }
        other => panic!("expected Desync, got {other:?}"),
    }

    // After desync, the queue is empty — a stray applied signal yields Empty.
    let stray_preview = deferred_input_preview_fingerprint("stray");
    assert!(
        matches!(
            pane.pop_applied_deferred_followup(&stray_preview),
            DeferredFollowupPop::Empty
        ),
        "post-desync queue must be empty"
    );
}

#[test]
fn restore_into_composer_never_drops_user_input() {
    // Regression: the old run-end restore path only wrote queued input into
    // the composer when `composer.is_empty()`; otherwise it logged a banner
    // and discarded the queued text entirely. User input must never vanish
    // silently — append beneath an existing draft instead.
    let mut pane = BottomPane::new();
    pane.composer.set_text("draft in progress");

    pane.restore_into_composer("queued follow-up");

    let text = pane.composer.text();
    assert!(
        text.contains("draft in progress"),
        "existing draft must be preserved; got {text:?}"
    );
    assert!(
        text.contains("queued follow-up"),
        "queued input must be appended, not dropped; got {text:?}"
    );
}

#[test]
fn restore_into_composer_replaces_empty_draft() {
    let mut pane = BottomPane::new();
    pane.restore_into_composer("only queued");
    assert_eq!(
        pane.composer.text().trim(),
        "only queued",
        "empty composer should receive the restored text verbatim"
    );
}

#[test]
fn restore_into_composer_ignores_blank_input() {
    let mut pane = BottomPane::new();
    pane.composer.set_text("untouched draft");
    pane.restore_into_composer("   \n  ");
    assert_eq!(
        pane.composer.text().trim(),
        "untouched draft",
        "blank restored input must not perturb the composer"
    );
}

#[test]
fn queued_followup_panel_renders_immediate_feedback() {
    let mut pane = BottomPane::new();
    pane.set_task_status(TaskStatus::TurnRunning {
        started_at: Instant::now(),
    });
    pane.queue_deferred_followup("hi from the user");

    let rendered = render_text(&pane, Rect::new(0, 0, 90, 8));
    assert!(
        rendered.contains("Esc sends now"),
        "queued follow-up panel should lead with the action verb; got {rendered:?}"
    );
    assert!(
        rendered.contains("next tool"),
        "queued follow-up panel should explain the accurate trigger; got {rendered:?}"
    );
    assert!(
        rendered.contains("hi from the user"),
        "queued follow-up panel should show the queued message preview; got {rendered:?}"
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
