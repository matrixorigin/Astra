//! ApprovalQueue behaviour contract (RED).

#![cfg(test)]

use super::ApprovalQueue;
use crate::chat_stream::ApprovalResponse;
use tokio::sync::oneshot;

fn channel() -> (
    oneshot::Sender<ApprovalResponse>,
    oneshot::Receiver<ApprovalResponse>,
) {
    oneshot::channel()
}

fn enqueue(q: &mut ApprovalQueue, tool: &str) -> (super::queue::ApprovalId, oneshot::Receiver<ApprovalResponse>) {
    let (tx, rx) = channel();
    let id = q.push(
        tool.to_string(),
        format!("{tool} needs approval"),
        None,
        "risk: unknown".into(),
        tx,
    );
    (id, rx)
}

// ─── Basic mechanics ──────────────────────────────────────────────

#[test]
fn new_queue_is_empty() {
    let q = ApprovalQueue::new();
    assert!(q.is_empty());
    assert_eq!(q.len(), 0);
    assert!(q.focused().is_none());
    assert_eq!(q.focus_index(), None);
    assert!(q.views().is_empty());
}

#[test]
fn push_adds_and_focuses_first_entry() {
    let mut q = ApprovalQueue::new();
    let (_id, _rx) = enqueue(&mut q, "bash");
    assert_eq!(q.len(), 1);
    assert!(!q.is_empty());
    assert_eq!(q.focus_index(), Some(0));
    let focused = q.focused().unwrap();
    assert_eq!(focused.tool, "bash");
}

#[test]
fn push_multiple_preserves_fifo_order() {
    let mut q = ApprovalQueue::new();
    enqueue(&mut q, "bash");
    enqueue(&mut q, "read");
    enqueue(&mut q, "edit");
    let tools: Vec<String> = q.views().iter().map(|v| v.tool.clone()).collect();
    assert_eq!(tools, vec!["bash", "read", "edit"]);
}

#[test]
fn push_assigns_monotonically_increasing_ids() {
    let mut q = ApprovalQueue::new();
    let (id_a, _) = enqueue(&mut q, "a");
    let (id_b, _) = enqueue(&mut q, "b");
    let (id_c, _) = enqueue(&mut q, "c");
    assert!(id_a < id_b);
    assert!(id_b < id_c);
}

// ─── Focus navigation ─────────────────────────────────────────────

#[test]
fn move_focus_down_advances_cursor() {
    let mut q = ApprovalQueue::new();
    enqueue(&mut q, "a");
    enqueue(&mut q, "b");
    enqueue(&mut q, "c");
    assert_eq!(q.focus_index(), Some(0));
    q.move_focus_down();
    assert_eq!(q.focus_index(), Some(1));
    q.move_focus_down();
    assert_eq!(q.focus_index(), Some(2));
}

#[test]
fn focus_wraps_at_the_ends() {
    let mut q = ApprovalQueue::new();
    enqueue(&mut q, "a");
    enqueue(&mut q, "b");
    q.move_focus_up();
    assert_eq!(q.focus_index(), Some(1), "up from 0 wraps to last");
    q.move_focus_down();
    assert_eq!(q.focus_index(), Some(0), "down from last wraps to first");
}

#[test]
fn move_focus_on_empty_queue_is_noop() {
    let mut q = ApprovalQueue::new();
    q.move_focus_down();
    q.move_focus_up();
    assert_eq!(q.focus_index(), None);
}

// ─── Responding ───────────────────────────────────────────────────

#[test]
fn respond_focused_sends_and_removes_entry() {
    let mut q = ApprovalQueue::new();
    let (_id, rx) = enqueue(&mut q, "bash");
    assert!(q.respond_focused(ApprovalResponse::AllowOnce));
    assert_eq!(q.len(), 0);
    assert_eq!(rx.blocking_recv().unwrap(), ApprovalResponse::AllowOnce);
}

#[test]
fn respond_focused_on_empty_returns_false() {
    let mut q = ApprovalQueue::new();
    assert!(!q.respond_focused(ApprovalResponse::Deny));
}

#[test]
fn respond_focused_advances_to_next_entry() {
    let mut q = ApprovalQueue::new();
    let (_id_a, _rx_a) = enqueue(&mut q, "a");
    let (_id_b, _rx_b) = enqueue(&mut q, "b");
    let (_id_c, _rx_c) = enqueue(&mut q, "c");
    assert!(q.respond_focused(ApprovalResponse::AllowOnce));
    // After resolving 'a', the new first entry 'b' becomes focused.
    assert_eq!(q.len(), 2);
    assert_eq!(q.focus_index(), Some(0));
    assert_eq!(q.focused().unwrap().tool, "b");
}

#[test]
fn respond_by_id_finds_nonfocused_entry() {
    let mut q = ApprovalQueue::new();
    let (id_a, rx_a) = enqueue(&mut q, "a");
    let (id_b, rx_b) = enqueue(&mut q, "b");
    let (id_c, _rx_c) = enqueue(&mut q, "c");
    // Focus is on a; resolve b out of order.
    assert!(q.respond_by_id(id_b, ApprovalResponse::Skip));
    assert_eq!(q.len(), 2);
    let remaining: Vec<_> = q.views().iter().map(|v| v.tool.clone()).collect();
    assert_eq!(remaining, vec!["a", "c"]);
    assert_eq!(rx_b.blocking_recv().unwrap(), ApprovalResponse::Skip);

    // rx_a still pending — the queue shouldn't have resolved it.
    // Drop q to release the sender without firing.
    drop(q);
    assert!(
        rx_a.blocking_recv().is_err(),
        "a's sender should be dropped unfired"
    );
    // id_c unused after drop, but capture to assert no confusion.
    let _ = id_c;
    let _ = id_a;
}

#[test]
fn respond_by_id_with_unknown_id_returns_false() {
    let mut q = ApprovalQueue::new();
    enqueue(&mut q, "only");
    assert!(!q.respond_by_id(9999, ApprovalResponse::Deny));
    assert_eq!(q.len(), 1, "queue unchanged");
}

#[test]
fn resolving_focused_when_focus_was_on_last_clamps() {
    let mut q = ApprovalQueue::new();
    let (_id_a, _rx_a) = enqueue(&mut q, "a");
    let (_id_b, _rx_b) = enqueue(&mut q, "b");
    q.move_focus_down();
    assert_eq!(q.focus_index(), Some(1));
    q.respond_focused(ApprovalResponse::AllowOnce);
    // 'b' removed — focus clamps to remaining last index.
    assert_eq!(q.len(), 1);
    assert_eq!(q.focus_index(), Some(0));
    assert_eq!(q.focused().unwrap().tool, "a");
}

// ─── ApprovalView projection ──────────────────────────────────────

#[test]
fn views_exclude_sender_and_are_cloneable() {
    let mut q = ApprovalQueue::new();
    enqueue(&mut q, "bash");
    let views = q.views();
    // ApprovalView is Clone + Debug — these calls exercise the bounds.
    let _v2 = views.clone();
    let _msg = format!("{views:?}");
}
