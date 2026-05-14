//! Integration tests for the Cursor-style non-blocking approval queue
//! wired through BottomPane.

#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

use super::{ApprovalActivation, BottomPane, BottomPaneAction};
use crate::chat_stream::ApprovalResponse;
use crate::tui::slash_menu::SlashItem;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ctrl_special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::CONTROL)
}

fn enqueue(bp: &mut BottomPane, tool: &str) -> oneshot::Receiver<ApprovalResponse> {
    let (tx, rx) = oneshot::channel();
    bp.enqueue_approval(
        tool.to_string(),
        format!("{tool} needs approval"),
        None,
        "unknown".into(),
        tx,
    );
    rx
}

fn slash_items() -> Vec<SlashItem> {
    vec![
        SlashItem::simple("/help", "show help"),
        SlashItem::simple("/history", "browse history"),
    ]
}

fn type_string(bp: &mut BottomPane, s: &str) {
    for c in s.chars() {
        let _ = bp.handle_key(key(c));
    }
}

// ─── Footer counter ────────────────────────────────────────────────

#[test]
fn enqueue_increments_footer_counter() {
    let mut bp = BottomPane::new();
    assert_eq!(bp.footer.pending_approvals, 0);
    let _rx = enqueue(&mut bp, "bash");
    assert_eq!(bp.footer.pending_approvals, 1);
    let _rx2 = enqueue(&mut bp, "read");
    assert_eq!(bp.footer.pending_approvals, 2);
}

// ─── Button focus + Enter ─────────────────────────────────────────

#[test]
fn enter_activates_accept_by_default() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    let action = bp.handle_key(special(KeyCode::Enter));
    match action {
        BottomPaneAction::ApprovalResolved { id } => assert!(id > 0),
        other => panic!("expected ApprovalResolved, got {other:?}"),
    }
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
    assert_eq!(bp.footer.pending_approvals, 0);
}

#[test]
fn right_moves_focus_then_enter_rejects() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");

    // Accept → Reject
    let _ = bp.handle_key(special(KeyCode::Right));
    let _ = bp.handle_key(special(KeyCode::Enter));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::Deny);
}

#[test]
fn left_from_accept_wraps_to_skip() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    let _ = bp.handle_key(special(KeyCode::Left));
    let _ = bp.handle_key(special(KeyCode::Enter));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::Skip);
}

#[test]
fn esc_rejects_focused_approval() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    let action = bp.handle_key(special(KeyCode::Esc));
    assert!(matches!(action, BottomPaneAction::ApprovalResolved { .. }));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::Deny);
}

// ─── Ctrl+Enter quick accept ─────────────────────────────────────

#[test]
fn ctrl_enter_accepts_regardless_of_button_focus() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    // Move to Reject first.
    let _ = bp.handle_key(special(KeyCode::Right));
    // Ctrl+Enter should still Accept.
    let action = bp.handle_key(ctrl_special(KeyCode::Enter));
    assert!(matches!(action, BottomPaneAction::ApprovalResolved { .. }));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
}

#[test]
fn ctrl_enter_accepts_even_with_nonempty_composer() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "hello");
    let _ = bp.handle_key(special(KeyCode::Right));

    let action = bp.handle_key(ctrl_special(KeyCode::Enter));

    assert!(matches!(action, BottomPaneAction::ApprovalResolved { .. }));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
    assert_eq!(
        bp.composer.text(),
        "hello",
        "quick approval should not destroy the user's draft"
    );
}

// ─── Composer stays live ──────────────────────────────────────────

#[test]
fn letter_keys_still_reach_composer_while_pending() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "yes please");
    assert_eq!(bp.composer.text(), "yes please", "letters not stolen");
    assert_eq!(bp.footer.pending_approvals, 1, "approval still pending");
    drop(rx);
}

#[test]
fn enter_with_text_in_composer_submits_instead_of_approving() {
    // When the composer has text, Enter should submit the message.
    // Approvals are resolved with Ctrl+Enter (quick accept) or by
    // explicitly clearing the composer and pressing Enter.
    let mut bp = BottomPane::new();
    let _rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "hello world");
    let action = bp.handle_key(special(KeyCode::Enter));
    match action {
        BottomPaneAction::SubmitInput(text) => {
            assert_eq!(text, "hello world");
            assert_eq!(bp.footer.pending_approvals, 1, "approval untouched");
        }
        other => panic!("expected SubmitInput, got {other:?}"),
    }
}

#[test]
fn slash_menu_open_with_approval_pending_routes_enter_to_slash_selection() {
    let mut bp = BottomPane::new();
    bp.set_slash_items(slash_items());
    let _rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "/he");
    assert!(bp.slash_menu_is_open());

    let action = bp.handle_key(special(KeyCode::Enter));

    match action {
        BottomPaneAction::SubmitInput(text) => assert_eq!(text, "/help"),
        other => panic!("expected slash command submission, got {other:?}"),
    }
    assert_eq!(
        bp.footer.pending_approvals, 1,
        "Enter in an open slash menu should not accidentally approve"
    );
}

#[test]
fn slash_menu_open_with_approval_pending_routes_tab_to_slash_selection() {
    let mut bp = BottomPane::new();
    bp.set_slash_items(slash_items());
    let _rx_a = enqueue(&mut bp, "a");
    let _rx_b = enqueue(&mut bp, "b");
    type_string(&mut bp, "/he");
    assert_eq!(bp.focused_approval_index(), Some(0));

    let action = bp.handle_key(special(KeyCode::Tab));

    assert!(matches!(action, BottomPaneAction::Consumed));
    assert_eq!(
        bp.composer.text(),
        "/help ",
        "Tab should complete the slash menu while it is open"
    );
    assert_eq!(
        bp.focused_approval_index(),
        Some(0),
        "slash-menu Tab must not cycle approvals underneath the popup"
    );
}

// ─── Multi-entry: Tab cycles, batch buttons work ──────────────────

#[test]
fn tab_cycles_between_pendings_when_composer_empty() {
    let mut bp = BottomPane::new();
    let _rx_a = enqueue(&mut bp, "a");
    let _rx_b = enqueue(&mut bp, "b");
    assert_eq!(bp.focused_approval_index(), Some(0));

    let _ = bp.handle_key(special(KeyCode::Tab));
    assert_eq!(bp.focused_approval_index(), Some(1));
}

#[test]
fn batch_button_resolves_all_pendings() {
    let mut bp = BottomPane::new();
    let rx_a = enqueue(&mut bp, "a");
    let rx_b = enqueue(&mut bp, "b");
    let rx_c = enqueue(&mut bp, "c");
    assert_eq!(bp.footer.pending_approvals, 3);

    // Navigate to Accept-all (index 4 in the 6-button row).
    for _ in 0..4 {
        bp.handle_key(special(KeyCode::Right));
    }
    let action = bp.handle_key(special(KeyCode::Enter));
    assert!(matches!(action, BottomPaneAction::ApprovalResolved { .. }));
    assert_eq!(bp.footer.pending_approvals, 0);
    assert_eq!(rx_a.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
    assert_eq!(rx_b.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
    assert_eq!(rx_c.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
}

// ─── Preview of focused approval is rendered inside BottomPane ────

#[test]
fn focused_approval_cell_available_for_render() {
    let mut bp = BottomPane::new();
    let _rx = enqueue(&mut bp, "bash");
    let cell = bp
        .focused_approval_cell()
        .expect("focused approval should produce a cell");
    assert_eq!(cell.tool, "bash");
    assert!(cell.focused);
}

#[test]
fn focused_approval_cell_is_none_when_empty() {
    let bp = BottomPane::new();
    assert!(bp.focused_approval_cell().is_none());
}

#[test]
fn activation_single_vs_batch_reports_correct_variant() {
    let mut bp = BottomPane::new();
    let _rx1 = enqueue(&mut bp, "a");
    let _rx2 = enqueue(&mut bp, "b");

    // Single: default-focus Accept.
    let act = bp.activate_focused_approval_button();
    assert!(matches!(
        act,
        Some(ApprovalActivation::Single {
            response: ApprovalResponse::AllowOnce,
            ..
        })
    ));
    assert_eq!(bp.footer.pending_approvals, 1);

    // Navigate to Accept-all (index 4).
    for _ in 0..4 {
        bp.handle_key(special(KeyCode::Right));
    }
    let act = bp.activate_focused_approval_button();
    assert!(matches!(
        act,
        Some(ApprovalActivation::Batch {
            count: 1,
            response: ApprovalResponse::AllowOnce
        })
    ));
    assert_eq!(bp.footer.pending_approvals, 0);
}

// ─── Arrow-key parity (user-reported UX bug) ─────────────────────
//
// Users reported pressing Up/Down on the approval card and seeing
// no movement — they were stuck on Accept. The prior mapping only
// bound Left/Right, which isn't a discoverable default when the
// buttons render horizontally. Fix: accept Up/Down as aliases of
// Left/Right. These tests lock the parity in.

#[test]
fn down_moves_focus_like_right() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    // Down from Accept → Reject. Enter rejects.
    let _ = bp.handle_key(special(KeyCode::Down));
    let _ = bp.handle_key(special(KeyCode::Enter));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::Deny);
}

#[test]
fn up_moves_focus_like_left_wrapping_to_skip() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    // Up from Accept wraps to the last button (Skip).
    let _ = bp.handle_key(special(KeyCode::Up));
    let _ = bp.handle_key(special(KeyCode::Enter));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::Skip);
}

#[test]
fn up_down_reach_always_button() {
    // End-to-end: the user wants to pick "Always" via Up/Down only.
    // From Accept (index 0), Down twice lands on Always (index 2).
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    let _ = bp.handle_key(special(KeyCode::Down));
    let _ = bp.handle_key(special(KeyCode::Down));
    let _ = bp.handle_key(special(KeyCode::Enter));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::AlwaysAllow);
}

#[test]
fn tab_cycles_pending_approvals_even_with_composer_text() {
    // The old guard required `composer.is_empty()` so a stray
    // character in the composer would hand Tab to completion and
    // the approval queue would never cycle. Users reported "Tab
    // did nothing" in exactly this scenario.
    let mut bp = BottomPane::new();
    let _rx1 = enqueue(&mut bp, "alpha");
    let _rx2 = enqueue(&mut bp, "beta");

    // Stray whitespace in the composer — user may have typed
    // ahead, or an accidental paste.
    type_string(&mut bp, " ");
    assert_eq!(bp.focused_approval_index(), Some(0));
    let _ = bp.handle_key(special(KeyCode::Tab));
    assert_eq!(
        bp.focused_approval_index(),
        Some(1),
        "Tab must cycle to the next approval regardless of composer content"
    );
}

#[test]
fn shift_tab_cycles_pending_approvals_backward() {
    let mut bp = BottomPane::new();
    let _rx1 = enqueue(&mut bp, "alpha");
    let _rx2 = enqueue(&mut bp, "beta");

    // Move to second, then BackTab should go back to first.
    let _ = bp.handle_key(special(KeyCode::Tab));
    assert_eq!(bp.focused_approval_index(), Some(1));
    let _ = bp.handle_key(special(KeyCode::BackTab));
    assert_eq!(bp.focused_approval_index(), Some(0));
}
