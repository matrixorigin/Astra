//! Integration tests for the non-blocking approval queue wired through
//! BottomPane. Verifies the user can keep typing, pending count drives
//! the status line, Ctrl+Y/N resolve without clearing the draft, and
//! the queue sequences multiple pendings FIFO.

#![cfg(test)]

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::oneshot;

use super::{BottomPane, BottomPaneAction};
use crate::chat_stream::ApprovalResponse;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn ctrl_char(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn special(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
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

#[test]
fn resolving_decrements_footer_counter() {
    let mut bp = BottomPane::new();
    let _rx_a = enqueue(&mut bp, "a");
    let _rx_b = enqueue(&mut bp, "b");
    assert_eq!(bp.footer.pending_approvals, 2);
    let id = bp
        .respond_focused_approval(ApprovalResponse::AllowOnce)
        .expect("resolved");
    assert!(id > 0);
    assert_eq!(bp.footer.pending_approvals, 1);
}

// ─── Composer remains live ────────────────────────────────────────

#[test]
fn composer_accepts_text_while_approval_pending() {
    let mut bp = BottomPane::new();
    let _rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "hello");
    assert_eq!(bp.composer.text(), "hello");
    assert_eq!(bp.footer.pending_approvals, 1);
}

#[test]
fn typing_y_in_composer_is_text_not_approval() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "yes please");
    assert_eq!(bp.composer.text(), "yes please");
    assert_eq!(bp.footer.pending_approvals, 1, "approval still pending");
    drop(rx);
}

#[test]
fn bare_y_does_not_resolve_even_when_composer_empty() {
    // Safety: users often start typing "yes" when a question comes up.
    // Require Ctrl+Y so we never consume that first keystroke.
    let mut bp = BottomPane::new();
    let _rx = enqueue(&mut bp, "bash");
    let action = bp.handle_key(key('y'));
    assert!(
        !matches!(action, BottomPaneAction::ApprovalResolved { .. }),
        "bare 'y' must NOT resolve approvals"
    );
    assert_eq!(bp.footer.pending_approvals, 1);
    assert_eq!(bp.composer.text(), "y", "letter should reach composer");
}

// ─── Ctrl-combo resolution while typing ───────────────────────────

#[test]
fn ctrl_y_resolves_even_with_nonempty_composer() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    type_string(&mut bp, "still typing...");

    let action = bp.handle_key(ctrl_char('y'));
    assert!(matches!(action, BottomPaneAction::ApprovalResolved { .. }));
    assert_eq!(
        rx.blocking_recv().unwrap(),
        ApprovalResponse::AllowOnce
    );
    assert_eq!(bp.composer.text(), "still typing...", "draft preserved");
    assert_eq!(bp.footer.pending_approvals, 0);
}

#[test]
fn ctrl_n_resolves_as_deny() {
    let mut bp = BottomPane::new();
    let rx = enqueue(&mut bp, "bash");
    let action = bp.handle_key(ctrl_char('n'));
    assert!(matches!(action, BottomPaneAction::ApprovalResolved { .. }));
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::Deny);
}

// ─── Multi-entry FIFO ─────────────────────────────────────────────

#[test]
fn fifo_order_across_responses() {
    let mut bp = BottomPane::new();
    let rx_a = enqueue(&mut bp, "a");
    let rx_b = enqueue(&mut bp, "b");
    let rx_c = enqueue(&mut bp, "c");
    assert_eq!(bp.footer.pending_approvals, 3);

    bp.handle_key(ctrl_char('y'));
    assert_eq!(rx_a.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
    assert_eq!(bp.footer.pending_approvals, 2);

    bp.handle_key(ctrl_char('n'));
    assert_eq!(rx_b.blocking_recv().unwrap(), ApprovalResponse::Deny);
    assert_eq!(bp.footer.pending_approvals, 1);

    bp.handle_key(ctrl_char('y'));
    assert_eq!(rx_c.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
    assert_eq!(bp.footer.pending_approvals, 0);
}

// ─── Tab cycles focus ─────────────────────────────────────────────

#[test]
fn tab_cycles_focus_when_composer_empty() {
    let mut bp = BottomPane::new();
    let _rx_a = enqueue(&mut bp, "a");
    let _rx_b = enqueue(&mut bp, "b");
    assert_eq!(bp.focused_approval_index(), Some(0));

    let _ = bp.handle_key(special(KeyCode::Tab));
    assert_eq!(bp.focused_approval_index(), Some(1));

    let _ = bp.handle_key(special(KeyCode::Tab));
    assert_eq!(bp.focused_approval_index(), Some(0), "wraps back");
}

#[test]
fn tab_is_not_stolen_by_approval_when_slash_menu_open() {
    let mut bp = BottomPane::new();
    use crate::tui::slash_menu::SlashItem;
    bp.set_slash_items(vec![SlashItem {
        name: "/help",
        description: "show help",
    }]);
    let _rx = enqueue(&mut bp, "bash");

    // Open slash menu via '/', then press Tab — should accept slash,
    // NOT cycle approval focus.
    let _ = bp.handle_key(key('/'));
    assert!(bp.slash_menu_is_open());
    let _ = bp.handle_key(special(KeyCode::Tab));
    // Slash accept consumes and closes the menu, inserting the command.
    assert!(!bp.slash_menu_is_open(), "slash menu closes on Tab accept");
    assert_eq!(bp.focused_approval_index(), Some(0), "approval focus unchanged");
}
