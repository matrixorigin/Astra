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
    /// Issue #326 P3 / R1 Major 11 / scenarios #21-#25: when a sub-agent
    /// owns this request, the agent identifier is recorded here.
    /// `None` for requests originating in the main TUI session.
    ///
    /// The TUI:
    /// 1. Renders a `[agent: <id>]` chip in the header so the
    ///    user knows whose request they're approving.
    /// 2. Disables the persistent-scope buttons (Project / User)
    ///    when this is `Some` — a child agent shouldn't be able
    ///    to extend the project rule file behind the user's back.
    pub source_agent: Option<String>,
    /// Issue #326 P3 / R1 Major 11 / scenarios #15/#23/#25: when this
    /// approval is for an MCP tool call, the server-supplied
    /// capability metadata (destructiveHint / readOnlyHint /
    /// openWorldHint). The TUI uses this to render a precise
    /// risk badge and to decide whether to disable persistent
    /// scopes for unknown-capability tools.
    pub mcp_capability:
        Option<astra_turn_core::permission_engine::ToolCapabilityMetadata>,
    /// Issue #326 P3 / scenario #39: when the agent runs against a
    /// remote host (SSH session, dev container, sandbox VM), this
    /// records the host label so the UI can prefix paths with
    /// `host:path`. `None` means "local". The label is purely
    /// display-side; the gate doesn't change behaviour based on
    /// it (remote-vs-local is the sub-run / capability metadata's
    /// job).
    pub host: Option<String>,
    /// Issue #326 P3 / R1 Major 7: multi-tag risk classification
    /// computed by the engine. Empty means "no specific risk
    /// tags emitted by the engine"; the UI falls back to the
    /// existing reason text.
    pub risk_tags: Vec<astra_turn_core::permission_engine::RiskTag>,
    /// Issue #326 P3: precomputed "Will save" preview — what
    /// would be persisted if the user pressed Always with the
    /// default Project scope. The TUI renders this verbatim in
    /// the approval card so users see exactly what
    /// `permissions.json` would gain. `None` means the request
    /// is not eligible for Always (e.g. compound shell command,
    /// MCP unknown capability).
    pub will_save_preview: Option<String>,
}

/// Metadata attached to a [`PendingApproval`] beyond the basic
/// header/detail/reason. Aggregated into a struct so callers can
/// extend without churning the whole call signature.
#[derive(Default, Debug, Clone)]
pub(crate) struct ApprovalMetadata {
    pub source_agent: Option<String>,
    pub mcp_capability:
        Option<astra_turn_core::permission_engine::ToolCapabilityMetadata>,
    pub host: Option<String>,
    pub risk_tags: Vec<astra_turn_core::permission_engine::RiskTag>,
    pub will_save_preview: Option<String>,
}

impl ApprovalMetadata {
    /// Convenience for callers that only want one field.
    #[must_use]
    pub fn with_source_agent(mut self, agent: impl Into<String>) -> Self {
        self.source_agent = Some(agent.into());
        self
    }

    #[must_use]
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = Some(host.into());
        self
    }

    #[must_use]
    pub fn with_risk_tags(
        mut self,
        tags: Vec<astra_turn_core::permission_engine::RiskTag>,
    ) -> Self {
        self.risk_tags = tags;
        self
    }

    #[must_use]
    pub fn with_will_save_preview(mut self, preview: impl Into<String>) -> Self {
        self.will_save_preview = Some(preview.into());
        self
    }

    #[must_use]
    pub fn with_mcp_capability(
        mut self,
        meta: astra_turn_core::permission_engine::ToolCapabilityMetadata,
    ) -> Self {
        self.mcp_capability = Some(meta);
        self
    }
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
            .field("source_agent", &self.source_agent)
            .field("host", &self.host)
            .field("risk_tag_count", &self.risk_tags.len())
            .field("will_save_preview", &self.will_save_preview)
            .field(
                "mcp_capability_known",
                &self.mcp_capability.as_ref().map(|m| m.is_known()),
            )
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
    /// Sub-agent owner, if any. Mirrored from
    /// [`PendingApproval::source_agent`].
    pub source_agent: Option<String>,
    /// Remote host, if any. Mirrored from
    /// [`PendingApproval::host`].
    pub host: Option<String>,
    /// Risk tags as their snake_case names so the view stays
    /// `Eq` / `Hash`-friendly (the engine's `RiskTag` enum is
    /// not currently `Eq`-derived for use as a HashSet key).
    pub risk_tag_labels: Vec<String>,
    /// "Will save: …" preview, mirrored from
    /// [`PendingApproval::will_save_preview`].
    pub will_save_preview: Option<String>,
}

impl From<&PendingApproval> for ApprovalView {
    fn from(p: &PendingApproval) -> Self {
        Self {
            id: p.id,
            tool: p.tool.clone(),
            header: p.header.clone(),
            detail: p.detail.clone(),
            reason: p.reason.clone(),
            source_agent: p.source_agent.clone(),
            host: p.host.clone(),
            risk_tag_labels: p
                .risk_tags
                .iter()
                .map(|tag| format!("{tag:?}"))
                .collect(),
            will_save_preview: p.will_save_preview.clone(),
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
        self.push_with_metadata(
            tool,
            header,
            detail,
            reason,
            response_tx,
            ApprovalMetadata::default(),
        )
    }

    /// Issue #326 P3 / R1 Major 11: same as [`push`] but lets callers
    /// attach extended metadata: source agent, MCP capability,
    /// remote host, risk tags, and a "Will save" preview. See
    /// [`ApprovalMetadata`] for the shape.
    pub fn push_with_metadata(
        &mut self,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        response_tx: oneshot::Sender<ApprovalResponse>,
        metadata: ApprovalMetadata,
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
            source_agent: metadata.source_agent,
            mcp_capability: metadata.mcp_capability,
            host: metadata.host,
            risk_tags: metadata.risk_tags,
            will_save_preview: metadata.will_save_preview,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_records_no_source_agent_by_default() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push("bash".into(), "h".into(), None, "r".into(), tx);
        let view = q.focused_view().unwrap();
        assert!(view.source_agent.is_none());
    }

    #[test]
    fn push_with_metadata_carries_source_agent_to_view() {
        // Issue #326 P3 / R1 Major 11: a sub-agent's request must
        // surface its identity in the ApprovalView so the TUI
        // can render the [agent: ...] chip.
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "h".into(),
            None,
            "r".into(),
            tx,
            ApprovalMetadata::default().with_source_agent("review-subagent"),
        );
        let view = q.focused_view().unwrap();
        assert_eq!(view.source_agent.as_deref(), Some("review-subagent"));
    }

    #[test]
    fn push_with_metadata_carries_mcp_capability() {
        use astra_turn_core::permission_engine::ToolCapabilityMetadata;
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        let meta = ToolCapabilityMetadata {
            destructive_hint: Some(true),
            server_name: Some("github".into()),
            ..Default::default()
        };
        q.push_with_metadata(
            "mcp_github_delete_issue".into(),
            "h".into(),
            None,
            "r".into(),
            tx,
            ApprovalMetadata::default().with_mcp_capability(meta.clone()),
        );
        let entries_dbg = format!("{:?}", &q.entries);
        assert!(entries_dbg.contains("mcp_capability_known: Some(true)"));
    }

    #[test]
    fn push_with_metadata_carries_host_to_view() {
        // Issue #326 P3 / scenario #39: SSH / dev-container
        // approvals must surface the host so the TUI can prefix
        // path strings with `host:path`.
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "edit_file".into(),
            "edit /etc/hosts".into(),
            None,
            "write".into(),
            tx,
            ApprovalMetadata::default().with_host("ssh:bastion-prod"),
        );
        let view = q.focused_view().unwrap();
        assert_eq!(view.host.as_deref(), Some("ssh:bastion-prod"));
    }

    #[test]
    fn push_with_metadata_carries_risk_tags_and_will_save() {
        // Issue #326 P3 / R1 Major 7: risk tags and the will-save
        // preview must reach the view layer.
        use astra_turn_core::permission_engine::RiskTag;
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            tx,
            ApprovalMetadata::default()
                .with_risk_tags(vec![RiskTag::BashExecute])
                .with_will_save_preview("Bash(npm test:*)"),
        );
        let view = q.focused_view().unwrap();
        assert_eq!(view.risk_tag_labels, vec!["BashExecute"]);
        assert_eq!(view.will_save_preview.as_deref(), Some("Bash(npm test:*)"));
    }
}
