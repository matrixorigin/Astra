#![cfg(test)]
//! End-to-end keyboard flow for the plan-review overlay surfaced
//! when the model calls `exit_plan_mode`. These tests drive the
//! whole stack (`BottomPane::enqueue_plan_review` → key events →
//! decision channel) the way the real TUI does, so a regression in
//! any of: view stack push, key dispatch, decision delivery, or
//! ViewCompleted bookkeeping shows up here.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use tokio::sync::oneshot;

use super::{BottomPane, BottomPaneAction};
use crate::cli::chat_stream::PlanReviewDecision;
use crate::cli::permission_manager::PermissionMode;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn shift(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

fn enqueue(bp: &mut BottomPane, plan: &str) -> oneshot::Receiver<PlanReviewDecision> {
    let (tx, rx) = oneshot::channel();
    bp.enqueue_plan_review(plan.to_string(), tx);
    rx
}

fn render_text(bp: &BottomPane, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    bp.render(area, &mut buf);
    let mut text = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            text.push_str(buf[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

#[test]
fn enqueue_pushes_view_and_renders_plan_body_plus_choices() {
    let mut bp = BottomPane::new();
    let _rx = enqueue(
        &mut bp,
        "1. Read the auth module\n2. Add unit tests\n3. Open a PR",
    );
    let frame = render_text(&bp, 80, 14);

    // Plan body visible.
    assert!(
        frame.contains("Read the auth module"),
        "rendered frame must show plan markdown body. Got:\n{frame}"
    );
    // All four choices present in the radio block.
    assert!(
        frame.contains("Approve · auto"),
        "choice 1 missing:\n{frame}"
    );
    assert!(
        frame.contains("Approve · edit"),
        "choice 2 missing:\n{frame}"
    );
    assert!(
        frame.contains("Approve · default"),
        "choice 3 missing:\n{frame}"
    );
    assert!(
        frame.contains("Keep planning"),
        "choice 4 missing:\n{frame}"
    );
    // Default selection marker on the first option.
    assert!(
        frame.contains("● Approve · auto"),
        "the active choice marker should sit on Approve · auto initially:\n{frame}"
    );
}

#[test]
fn enter_on_default_selection_approves_with_auto_mode() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, "1. Plan");

    let action = bp.handle_key(key(KeyCode::Enter));

    assert!(
        matches!(action, BottomPaneAction::ViewCompleted { .. }),
        "Enter on the default selection must mark the view as completed; got: {action:?}"
    );
    assert_eq!(
        rx.try_recv().unwrap(),
        PlanReviewDecision::Approve {
            mode: PermissionMode::Auto
        },
        "default selection corresponds to Approve · auto"
    );
}

#[test]
fn tab_walks_through_each_choice_in_order() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, "1. Plan");

    // Default → Tab → Approve · edit → Enter
    bp.handle_key(key(KeyCode::Tab));
    bp.handle_key(key(KeyCode::Enter));
    assert_eq!(
        rx.try_recv().unwrap(),
        PlanReviewDecision::Approve {
            mode: PermissionMode::AcceptEdits
        }
    );

    // Re-open and walk twice to land on default.
    let mut rx = enqueue(&mut bp, "1. Plan");
    bp.handle_key(key(KeyCode::Tab));
    bp.handle_key(key(KeyCode::Tab));
    bp.handle_key(key(KeyCode::Enter));
    assert_eq!(
        rx.try_recv().unwrap(),
        PlanReviewDecision::Approve {
            mode: PermissionMode::Prompt
        }
    );

    // Re-open and walk three times for Keep planning.
    let mut rx = enqueue(&mut bp, "1. Plan");
    bp.handle_key(key(KeyCode::Tab));
    bp.handle_key(key(KeyCode::Tab));
    bp.handle_key(key(KeyCode::Tab));
    bp.handle_key(key(KeyCode::Enter));
    assert_eq!(rx.try_recv().unwrap(), PlanReviewDecision::KeepPlanning);
}

#[test]
fn shift_tab_moves_selection_backwards() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, "1. Plan");

    // From default (auto), Shift+Tab wraps to Keep planning.
    bp.handle_key(shift(KeyCode::BackTab));
    bp.handle_key(key(KeyCode::Enter));
    assert_eq!(rx.try_recv().unwrap(), PlanReviewDecision::KeepPlanning);
}

#[test]
fn number_keys_jump_directly_to_choice() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, "1. Plan");

    bp.handle_key(key(KeyCode::Char('3')));
    bp.handle_key(key(KeyCode::Enter));
    assert_eq!(
        rx.try_recv().unwrap(),
        PlanReviewDecision::Approve {
            mode: PermissionMode::Prompt
        },
        "number 3 must select Approve · default"
    );
}

#[test]
fn esc_cancels_and_returns_cancelled_decision() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, "1. Plan");

    let action = bp.handle_key(key(KeyCode::Esc));

    assert!(
        matches!(action, BottomPaneAction::ViewCompleted { .. }),
        "Esc must complete the view (overlay dismissable); got: {action:?}"
    );
    assert_eq!(
        rx.try_recv().unwrap(),
        PlanReviewDecision::Cancelled,
        "Esc maps to Cancelled, never to silent KeepPlanning"
    );
}

#[test]
fn scroll_keys_do_not_dispatch_a_decision() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(
        &mut bp,
        "Long plan:\n1. step one\n2. step two\n3. step three\n4. step four",
    );

    // Drive the body scroller — must not submit.
    bp.handle_key(key(KeyCode::Char('j')));
    bp.handle_key(key(KeyCode::Char('j')));
    bp.handle_key(key(KeyCode::Char('j')));
    bp.handle_key(key(KeyCode::Char('k')));
    bp.handle_key(key(KeyCode::PageDown));
    bp.handle_key(key(KeyCode::PageUp));

    assert!(
        rx.try_recv().is_err(),
        "scroll keystrokes must not dispatch a PlanReviewDecision"
    );

    // The view must still be live for a follow-up Enter to commit.
    bp.handle_key(key(KeyCode::Enter));
    assert_eq!(
        rx.try_recv().unwrap(),
        PlanReviewDecision::Approve {
            mode: PermissionMode::Auto
        }
    );
}

#[test]
fn ctrl_c_cancels_overlay_without_escalating_to_quit() {
    let mut bp = BottomPane::new();
    let mut rx = enqueue(&mut bp, "1. Plan");

    let action = bp.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    // The overlay consumes Ctrl+C (CancellationEvent::Consumed) and the
    // BottomPane reports ViewCompleted, NOT Quit. This protects the
    // user from accidentally killing the session while reviewing a plan.
    assert!(
        matches!(
            action,
            BottomPaneAction::ViewCompleted { .. } | BottomPaneAction::Consumed
        ),
        "Ctrl+C inside plan-review overlay must not escalate to Quit; got: {action:?}"
    );
    // Decision must be Cancelled, since on_ctrl_c calls cancel().
    assert_eq!(rx.try_recv().unwrap(), PlanReviewDecision::Cancelled);
}

#[test]
fn long_plan_remains_navigable_with_scroll() {
    // Smoke test: a plan that overflows the rendered area must still
    // render its first lines and not corrupt the choice block.
    let plan = (1..=40)
        .map(|n| format!("{n}. step number {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut bp = BottomPane::new();
    let _rx = enqueue(&mut bp, &plan);

    let frame = render_text(&bp, 80, 14);

    // Top of the body is visible.
    assert!(
        frame.contains("step number 1"),
        "first plan line must be visible at scroll=0:\n{frame}"
    );
    // Choice block is not eaten by the body.
    assert!(
        frame.contains("Approve · auto"),
        "choice block must remain rendered even with a 40-line plan:\n{frame}"
    );
}
