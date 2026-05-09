//! Pure approval queue — RED phase stub.

#![allow(dead_code)]

use std::collections::VecDeque;
use tokio::sync::oneshot;

use super::button_row::ButtonRow;
use crate::chat_stream::ApprovalResponse;

/// Monotonic id assigned by the queue. Stable across the session so the
/// reducer and tool cells can refer to a pending approval without owning
/// the non-Clone `oneshot::Sender`.
pub(crate) type ApprovalId = u64;

/// One pending approval. The `response_tx` is `Option` so `respond_*`
/// can consume it exactly once without moving the whole struct.
pub(crate) struct PendingApproval {
    pub id: ApprovalId,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    pub response_tx: Option<oneshot::Sender<ApprovalResponse>>,
    /// Live button row owned per entry so arrow-key focus sticks
    /// through navigation even when focus cycles between entries.
    pub buttons: ButtonRow,
}

impl std::fmt::Debug for PendingApproval {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingApproval")
            .field("id", &self.id)
            .field("tool", &self.tool)
            .field("header", &self.header)
            .field("detail", &self.detail)
            .field("reason", &self.reason)
            .field("has_response_tx", &self.response_tx.is_some())
            .finish()
    }
}

/// View-only projection safe to store in `State` (no oneshot).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ApprovalView {
    pub id: ApprovalId,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
}

impl From<&PendingApproval> for ApprovalView {
    fn from(p: &PendingApproval) -> Self {
        Self {
            id: p.id,
            tool: p.tool.clone(),
            header: p.header.clone(),
            detail: p.detail.clone(),
            reason: p.reason.clone(),
        }
    }
}

/// FIFO queue of pending approvals with a focus cursor.
#[derive(Default)]
pub(crate) struct ApprovalQueue {
    next_id: ApprovalId,
    entries: VecDeque<PendingApproval>,
    focus: usize,
}

impl ApprovalQueue {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn push(
        &mut self,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        response_tx: oneshot::Sender<ApprovalResponse>,
    ) -> ApprovalId {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        // Promote to 6-button row when the queue already has entries:
        // the newcomer will coexist with others so batch actions are
        // useful. Otherwise the plain 4-button row suffices.
        let buttons = if self.entries.is_empty() {
            ButtonRow::primary()
        } else {
            ButtonRow::primary_with_batch()
        };
        self.entries.push_back(PendingApproval {
            id,
            tool,
            header,
            detail,
            reason,
            response_tx: Some(response_tx),
            buttons,
        });
        // Promote pre-existing entries too — they now share the queue
        // and should expose the batch buttons on their next focus.
        let total = self.entries.len();
        if total > 1 {
            for entry in self.entries.iter_mut().take(total - 1) {
                entry.buttons = ButtonRow::primary_with_batch();
            }
        }
        id
    }

    pub fn focused(&self) -> Option<&PendingApproval> {
        self.entries.get(self.focus)
    }

    pub fn focus_index(&self) -> Option<usize> {
        if self.entries.is_empty() {
            None
        } else {
            Some(self.focus)
        }
    }

    pub fn views(&self) -> Vec<ApprovalView> {
        self.entries.iter().map(ApprovalView::from).collect()
    }

    pub fn move_focus_up(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.focus = if self.focus == 0 {
            self.entries.len() - 1
        } else {
            self.focus - 1
        };
    }

    pub fn move_focus_down(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        self.focus = (self.focus + 1) % self.entries.len();
    }

    pub fn respond_focused(&mut self, response: ApprovalResponse) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let sent = self.send_at(self.focus, response);
        if sent {
            self.entries.remove(self.focus);
            self.clamp_focus();
        }
        sent
    }

    pub fn respond_by_id(&mut self, id: ApprovalId, response: ApprovalResponse) -> bool {
        let Some(idx) = self.entries.iter().position(|e| e.id == id) else {
            return false;
        };
        let sent = self.send_at(idx, response);
        if sent {
            self.entries.remove(idx);
            // If the removed entry was at or before focus, shift focus.
            if idx < self.focus {
                self.focus -= 1;
            }
            self.clamp_focus();
        }
        sent
    }

    fn send_at(&mut self, idx: usize, response: ApprovalResponse) -> bool {
        let Some(entry) = self.entries.get_mut(idx) else {
            return false;
        };
        match entry.response_tx.take() {
            Some(tx) => tx.send(response).is_ok(),
            None => false,
        }
    }

    fn clamp_focus(&mut self) {
        if self.entries.is_empty() {
            self.focus = 0;
        } else if self.focus >= self.entries.len() {
            self.focus = self.entries.len() - 1;
        }
    }

    /// Move button focus inside the currently focused entry.
    pub fn focused_button_move_left(&mut self) {
        if let Some(e) = self.entries.get_mut(self.focus) {
            e.buttons.move_left();
        }
    }
    pub fn focused_button_move_right(&mut self) {
        if let Some(e) = self.entries.get_mut(self.focus) {
            e.buttons.move_right();
        }
    }

    /// Action of the currently focused button on the focused entry.
    pub fn focused_button_action(&self) -> Option<super::button_row::ButtonAction> {
        self.entries
            .get(self.focus)
            .and_then(|e| e.buttons.activate())
    }

    /// Resolve every pending entry with the same response. Returns the
    /// count actually resolved (senders may have been dropped).
    pub fn respond_all(&mut self, response: ApprovalResponse) -> usize {
        let mut n = 0usize;
        while !self.entries.is_empty() {
            // Always target index 0 so focus ordering doesn't matter.
            if self.send_at(0, response) {
                n += 1;
            }
            self.entries.pop_front();
        }
        self.focus = 0;
        n
    }

    /// Button row of the currently focused entry (for rendering).
    pub fn focused_button_row(&self) -> Option<&super::button_row::ButtonRow> {
        self.entries.get(self.focus).map(|e| &e.buttons)
    }

    /// Button focus index of the currently focused entry.
    pub fn focused_button_index(&self) -> Option<usize> {
        self.entries.get(self.focus).map(|e| e.buttons.focus())
    }

    /// View projection of the focused entry (no oneshot).
    pub fn focused_view(&self) -> Option<ApprovalView> {
        self.entries.get(self.focus).map(ApprovalView::from)
    }
}
