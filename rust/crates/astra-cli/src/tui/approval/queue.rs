//! Pure approval queue — RED phase stub.

#![allow(dead_code)]

use std::collections::VecDeque;
use tokio::sync::oneshot;

use super::button_row::ButtonRow;
use crate::cli::chat_stream::ApprovalResponse;
use astra_turn_core::permission::scope::AllowScope;

/// Monotonic id assigned by the queue. Stable across the session so the
/// reducer and tool cells can refer to a pending approval without owning
/// the non-Clone `oneshot::Sender`.
pub(crate) type ApprovalId = u64;

/// One pending approval. The `response_txs` vec lets dedup
/// merge multiple in-flight requests with byte-identical
/// `ApprovalRequestKey`s under one user-facing prompt — when
/// the user resolves, all stored senders receive the same
/// response (issue #326 P4 / R2 Critical 1).
pub(crate) struct PendingApproval {
    pub id: ApprovalId,
    pub tool: String,
    pub header: String,
    pub detail: Option<String>,
    pub reason: String,
    /// All response channels waiting on this prompt. Empty
    /// after the prompt resolves (each sender takes itself out
    /// when broadcasting).
    pub response_txs: Vec<oneshot::Sender<ApprovalResponse>>,
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
    pub mcp_capability: Option<astra_turn_core::permission::engine::ToolCapabilityMetadata>,
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
    pub risk_tags: Vec<astra_turn_core::permission::engine::RiskTag>,
    /// Human-readable preview of what pressing Always will remember.
    /// This must not expose permission-rule DSL.
    pub remember_preview: Option<String>,
    /// Workspace trust gate for Project scope. When true, Project
    /// is rendered but cannot be activated; project allow rules are
    /// not trusted for this session.
    pub workspace_untrusted: bool,
    pub is_compound_command: bool,
    pub has_dynamic_eval: bool,
    pub unsafe_rule_shape: bool,
    /// Issue #326 P3 / P5f / R2 Major 3: host-computed digest of
    /// the file the tool will mutate, snapshotted at the moment
    /// the approval enters the queue. The executor compares this
    /// against a fresh digest right before running the tool; a
    /// mismatch surfaces a stale-approval reject and a re-prompt
    /// with the new diff. `None` for non-file tools or for
    /// brand-new writes (where the file doesn't exist yet).
    ///
    /// Crucially: this is set by the host, NOT by the LLM. R2
    /// Major 3 explicitly forbids trusting an `expected_base_sha`
    /// arg from the model.
    pub base_digest: Option<astra_turn_core::approval_base_digest::BaseDigest>,
    /// Issue #326 P4 / R2 Critical 1: the strict request
    /// identity used for queue dedup. Two pending entries with
    /// equal request_keys are merged into one prompt (their
    /// senders unioned into `response_txs`); the user's choice
    /// broadcasts to all waiting senders. None for legacy
    /// callers that don't compute the key.
    pub request_key: Option<astra_turn_core::approval_request_key::ApprovalRequestKey>,
    /// Original tool-call arguments. Carried so the queue can
    /// re-evaluate the request through `permission_engine` after
    /// the user pivots permission modes (e.g. Edit → Auto). Empty
    /// `Value::Null` means "args weren't propagated by this caller"
    /// and re-evaluation will keep the entry pending unchanged
    /// (conservative — better to ask twice than to silently allow).
    pub args: serde_json::Value,
    /// Issue #326 P4 / R2 Major 1: the wide UI-grouping key.
    /// Entries that share `batch_group_key` render as one
    /// batch card with per-item rows. None means "render as a
    /// solo card".
    pub batch_group_key: Option<astra_turn_core::approval_batch_group::ApprovalBatchGroupKey>,
}

/// Metadata attached to a [`PendingApproval`] beyond the basic
/// header/detail/reason. Aggregated into a struct so callers can
/// extend without churning the whole call signature.
#[derive(Default, Debug, Clone)]
pub(crate) struct ApprovalMetadata {
    pub source_agent: Option<String>,
    pub mcp_capability: Option<astra_turn_core::permission::engine::ToolCapabilityMetadata>,
    pub host: Option<String>,
    pub risk_tags: Vec<astra_turn_core::permission::engine::RiskTag>,
    pub remember_preview: Option<String>,
    pub workspace_untrusted: bool,
    pub is_compound_command: bool,
    pub has_dynamic_eval: bool,
    pub unsafe_rule_shape: bool,
    /// Host-computed snapshot of the file the tool will mutate;
    /// see [`PendingApproval::base_digest`].
    pub base_digest: Option<astra_turn_core::approval_base_digest::BaseDigest>,
    /// Issue #326 P4: strict request identity for queue dedup.
    pub request_key: Option<astra_turn_core::approval_request_key::ApprovalRequestKey>,
    /// Issue #326 P4: UI grouping key.
    pub batch_group_key: Option<astra_turn_core::approval_batch_group::ApprovalBatchGroupKey>,
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
        tags: Vec<astra_turn_core::permission::engine::RiskTag>,
    ) -> Self {
        self.risk_tags = tags;
        self
    }

    #[must_use]
    pub fn with_remember_preview(mut self, preview: impl Into<String>) -> Self {
        self.remember_preview = Some(preview.into());
        self
    }

    #[must_use]
    pub fn with_workspace_untrusted(mut self, workspace_untrusted: bool) -> Self {
        self.workspace_untrusted = workspace_untrusted;
        self
    }

    #[must_use]
    pub fn with_scope_shape(mut self, is_compound_command: bool, has_dynamic_eval: bool) -> Self {
        self.is_compound_command = is_compound_command;
        self.has_dynamic_eval = has_dynamic_eval;
        self
    }

    #[must_use]
    pub fn with_unsafe_rule_shape(mut self, unsafe_rule_shape: bool) -> Self {
        self.unsafe_rule_shape = unsafe_rule_shape;
        self
    }

    #[must_use]
    pub fn with_mcp_capability(
        mut self,
        meta: astra_turn_core::permission::engine::ToolCapabilityMetadata,
    ) -> Self {
        self.mcp_capability = Some(meta);
        self
    }

    /// Issue #326 P3 / P5f: snapshot the file's current digest at
    /// approval enqueue time. The executor uses this to detect
    /// stale approvals (file modified between approval and
    /// execution) and re-prompt with the new diff.
    #[must_use]
    pub fn with_base_digest(
        mut self,
        digest: astra_turn_core::approval_base_digest::BaseDigest,
    ) -> Self {
        self.base_digest = Some(digest);
        self
    }

    /// Issue #326 P4 / R2 Critical 1: attach the strict
    /// request-identity key. When the queue receives an
    /// equal key it merges the new sender into the existing
    /// entry's `response_txs` — no second prompt fires.
    #[must_use]
    pub fn with_request_key(
        mut self,
        key: astra_turn_core::approval_request_key::ApprovalRequestKey,
    ) -> Self {
        self.request_key = Some(key);
        self
    }

    /// Issue #326 P4 / R2 Major 1: attach the wide UI
    /// grouping key. Same-group entries can be batch-resolved
    /// together; cross-group "Yes to all" is rejected by
    /// `ApprovalBatchGroupKey::allows_accept_all` for
    /// destructive groups.
    #[must_use]
    pub fn with_batch_group_key(
        mut self,
        key: astra_turn_core::approval_batch_group::ApprovalBatchGroupKey,
    ) -> Self {
        self.batch_group_key = Some(key);
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
            .field("response_txs_len", &self.response_txs.len())
            .field("source_agent", &self.source_agent)
            .field("host", &self.host)
            .field("risk_tag_count", &self.risk_tags.len())
            .field("remember_preview", &self.remember_preview)
            .field(
                "base_digest",
                &self.base_digest.as_ref().map(|d| d.short_display()),
            )
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
    /// Always-remembers preview, mirrored from
    /// [`PendingApproval::remember_preview`].
    pub remember_preview: Option<String>,
    pub selection_hint: Option<String>,
    pub workspace_untrusted: bool,
    pub is_compound_command: bool,
    pub has_dynamic_eval: bool,
    pub unsafe_rule_shape: bool,
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
            risk_tag_labels: p.risk_tags.iter().map(|tag| format!("{tag:?}")).collect(),
            remember_preview: p.remember_preview.clone(),
            selection_hint: p.selection_hint(),
            workspace_untrusted: p.workspace_untrusted,
            is_compound_command: p.is_compound_command,
            has_dynamic_eval: p.has_dynamic_eval,
            unsafe_rule_shape: p.unsafe_rule_shape,
        }
    }
}

impl PendingApproval {
    fn scope_context(&self) -> astra_turn_core::permission::scope::ScopeAvailabilityContext {
        astra_turn_core::permission::scope::ScopeAvailabilityContext {
            risk_tags: self.risk_tags.clone(),
            source_agent_present: self.source_agent.is_some(),
            mcp_unknown_capability: self
                .mcp_capability
                .as_ref()
                .is_some_and(|meta| !meta.is_known())
                || self
                    .risk_tags
                    .contains(&astra_turn_core::permission::engine::RiskTag::MCPUnknownCapability),
            workspace_untrusted: self.workspace_untrusted,
            is_compound_command: self.is_compound_command,
            has_dynamic_eval: self.has_dynamic_eval,
            unsafe_rule_shape: self.unsafe_rule_shape,
        }
    }

    fn scope_available(&self, scope: astra_turn_core::permission::scope::AllowScope) -> bool {
        astra_turn_core::permission::scope::permitted_scopes(&self.scope_context())
            .into_iter()
            .any(|entry| entry.scope == scope && entry.available)
    }

    fn always_action_disabled(&self) -> bool {
        use astra_turn_core::permission::engine::RiskTag;

        if self.source_agent.is_some()
            || self.is_compound_command
            || self.has_dynamic_eval
            || self.unsafe_rule_shape
            || self.risk_tags.contains(&RiskTag::WritesSensitiveFile)
            || self.risk_tags.contains(&RiskTag::GitDestructive)
            || self.risk_tags.contains(&RiskTag::WritesOutsideWorkspace)
            || self.risk_tags.contains(&RiskTag::CredentialAccess)
            || self.risk_tags.contains(&RiskTag::MCPUnknownCapability)
        {
            return true;
        }

        !self.workspace_untrusted && !self.scope_available(AllowScope::Project)
    }

    fn always_uses_session_fallback(&self) -> bool {
        self.workspace_untrusted
            && !self.always_action_disabled()
            && !self.scope_available(AllowScope::Project)
    }

    fn selection_hint(&self) -> Option<String> {
        if self.always_uses_session_fallback() {
            return Some(
                "Don't ask again stays session-only until you trust this workspace. Choose Trust Workspace or run `/allow trust` to save workspace rules."
                    .to_string(),
            );
        }
        None
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
        args: serde_json::Value,
        response_tx: oneshot::Sender<ApprovalResponse>,
    ) -> ApprovalId {
        self.push_with_metadata(
            tool,
            header,
            detail,
            reason,
            args,
            response_tx,
            ApprovalMetadata::default(),
        )
    }

    /// Issue #326 P3 / R1 Major 11 / P4 R2 Critical 1: same as
    /// [`push`] but lets callers attach extended metadata. When
    /// `metadata.request_key` is `Some` and equals the
    /// request_key of an entry already in the queue, this entry
    /// is **deduplicated**: the new sender is appended to the
    /// existing entry's `response_txs` and no new prompt is
    /// rendered. Returns the surviving entry's id.
    #[allow(clippy::too_many_arguments)]
    pub fn push_with_metadata(
        &mut self,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        args: serde_json::Value,
        response_tx: oneshot::Sender<ApprovalResponse>,
        metadata: ApprovalMetadata,
    ) -> ApprovalId {
        // Issue #326 P4 / R2 Critical 1: dedup on byte-equal
        // ApprovalRequestKey. Senders waiting on the same
        // request all get the same answer.
        if let Some(ref rkey) = metadata.request_key {
            for entry in self.entries.iter_mut() {
                if entry.request_key.as_ref() == Some(rkey) {
                    entry.response_txs.push(response_tx);
                    return entry.id;
                }
            }
        }

        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        // Promote to the batch-capable row when the queue already has entries:
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
            response_txs: vec![response_tx],
            buttons,
            source_agent: metadata.source_agent,
            mcp_capability: metadata.mcp_capability,
            host: metadata.host,
            risk_tags: metadata.risk_tags,
            remember_preview: metadata.remember_preview,
            workspace_untrusted: metadata.workspace_untrusted,
            is_compound_command: metadata.is_compound_command,
            has_dynamic_eval: metadata.has_dynamic_eval,
            unsafe_rule_shape: metadata.unsafe_rule_shape,
            base_digest: metadata.base_digest,
            request_key: metadata.request_key,
            args,
            batch_group_key: metadata.batch_group_key,
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

    /// Re-evaluate every pending entry against the predicate `still_needs_approval`.
    /// For each entry the predicate returns `false` for, drain it from the
    /// queue and broadcast `ApprovalResponse::AllowOnce` to all of its
    /// waiting senders. The focus cursor is normalised so it points at a
    /// surviving entry (or to 0 if the queue is empty afterwards).
    ///
    /// This is the post-mode-pivot cleanup path: when the user flips
    /// permission modes (e.g. Edit → Auto), pending entries that the
    /// new mode would auto-approve must be released immediately.
    /// Without this, the chip flips to Auto but the approval card
    /// lingers and the model stalls waiting for the user.
    ///
    /// Returns the number of entries auto-approved.
    pub fn drain_now_allowed<F>(&mut self, mut still_needs_approval: F) -> usize
    where
        F: FnMut(&PendingApproval) -> bool,
    {
        let mut released = 0usize;
        let mut idx = 0usize;
        while idx < self.entries.len() {
            if still_needs_approval(&self.entries[idx]) {
                idx += 1;
                continue;
            }
            // Take this entry out and broadcast Allow to its waiters.
            let mut entry = self.entries.remove(idx).expect("index in range");
            for tx in entry.response_txs.drain(..) {
                let _ = tx.send(ApprovalResponse::AllowOnce);
            }
            released += 1;
            // Don't advance idx: the next entry slid into this slot.
        }
        // Normalise focus cursor to a valid index.
        if self.focus >= self.entries.len() {
            self.focus = self.entries.len().saturating_sub(1);
        }
        released
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
        // Issue #326 P4 / R2 Critical 1: broadcast the
        // response to every waiting sender. Drop dead
        // senders silently — recv-side may have cancelled
        // the future.
        let txs = std::mem::take(&mut entry.response_txs);
        let mut any_ok = false;
        for tx in txs {
            if tx.send(response.clone()).is_ok() {
                any_ok = true;
            }
        }
        any_ok
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
        let entry = self.entries.get(self.focus)?;
        let action = entry.buttons.activate()?;
        if matches!(
            action,
            super::button_row::ButtonAction::Respond(ApprovalResponse::AlwaysAllow)
        ) && entry.always_action_disabled()
        {
            return None;
        }
        match action {
            super::button_row::ButtonAction::RespondAll(ref response) if response.is_approved() => {
                match entry.batch_group_key.as_ref() {
                    Some(group) if group.allows_accept_all() => Some(action.clone()),
                    Some(_) => None,
                    // Legacy/ungrouped approvals are not batchable,
                    // but the queue resolves only the focused entry
                    // for safety. Keep the action available so the
                    // row does not dead-end when older senders omit
                    // metadata.
                    None => Some(action.clone()),
                }
            }
            _ => Some(action),
        }
    }

    /// Resolve every pending entry in the focused entry's batch group.
    ///
    /// `None` group keys are intentionally not batchable: pressing a
    /// batch button on a legacy/ungrouped queue resolves only the
    /// focused entry, never the whole queue. For grouped approvals,
    /// `Yes to all` is further gated by
    /// [`ApprovalBatchGroupKey::allows_accept_all`]; destructive groups
    /// must be approved item-by-item.
    ///
    /// Returns the count that received a response. Entries whose
    /// receiver was already dropped are still removed from the queue.
    pub fn respond_focused_group(&mut self, response: ApprovalResponse) -> usize {
        if self.entries.is_empty() {
            return 0;
        }

        let focused_group = self.entries[self.focus].batch_group_key.clone();
        let Some(group) = focused_group else {
            let focused = self.focus;
            let sent = self.send_at(focused, response);
            self.entries.remove(focused);
            self.clamp_focus();
            return usize::from(sent);
        };

        if response.is_approved() && !group.allows_accept_all() {
            return 0;
        }

        let mut n = 0usize;
        let mut idx = 0usize;
        while idx < self.entries.len() {
            let same_group = self.entries[idx].batch_group_key.as_ref() == Some(&group);
            if same_group {
                if self.send_at(idx, response.clone()) {
                    n += 1;
                }
                self.entries.remove(idx);
            } else {
                idx += 1;
            }
        }
        self.clamp_focus();
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

    /// Issue #326 P5f: stale-revalidate the focused entry's
    /// approval against the file's current bytes.
    ///
    /// Returns `None` when the entry doesn't carry a base_digest
    /// (e.g. non-file tools, brand-new writes). Otherwise reads
    /// the file at `path` and returns the [`StaleCheck`] outcome.
    pub fn focused_stale_check(
        &self,
        path: &std::path::Path,
    ) -> Option<std::io::Result<astra_turn_core::approval_base_digest::StaleCheck>> {
        let entry = self.entries.get(self.focus)?;
        Some(astra_turn_core::approval_base_digest::stale_check(
            path,
            entry.base_digest.clone(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_records_no_source_agent_by_default() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push(
            "bash".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx,
        );
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
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_source_agent("review-subagent"),
        );
        let view = q.focused_view().unwrap();
        assert_eq!(view.source_agent.as_deref(), Some("review-subagent"));
    }

    #[test]
    fn push_with_metadata_carries_mcp_capability() {
        use astra_turn_core::permission::engine::ToolCapabilityMetadata;
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
            serde_json::Value::Null,
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
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_host("ssh:bastion-prod"),
        );
        let view = q.focused_view().unwrap();
        assert_eq!(view.host.as_deref(), Some("ssh:bastion-prod"));
    }

    #[test]
    fn push_with_metadata_carries_risk_tags_and_remember_preview() {
        // Risk tags and the human-readable remember preview must
        // reach the view layer.
        use astra_turn_core::permission::engine::RiskTag;
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default()
                .with_risk_tags(vec![RiskTag::BashExecute])
                .with_remember_preview("similar `npm test` commands in this workspace"),
        );
        let view = q.focused_view().unwrap();
        assert_eq!(view.risk_tag_labels, vec!["BashExecute"]);
        assert_eq!(
            view.remember_preview.as_deref(),
            Some("similar `npm test` commands in this workspace")
        );
    }

    // ── Issue #326 P3 / P5f / R2 Major 3: base digest ──

    #[test]
    fn focused_stale_check_handles_request_without_digest() {
        // For non-file tools (no base_digest set), focused_stale_check
        // still returns Some(...) — the underlying stale_check
        // helper interprets None previous as "file should be
        // brand-new". This is the right behaviour because edit-vs-
        // not-edit decisions live higher up; the queue accessor
        // just runs the comparison.
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push(
            "write_file".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx,
        );
        let path = std::env::temp_dir().join("definitely-does-not-exist-326-test");
        let _ = std::fs::remove_file(&path); // best-effort
        let result = q.focused_stale_check(&path).unwrap().unwrap();
        // No previous, no current → StillAbsent (Fresh).
        assert!(result.is_fresh());
    }

    #[test]
    fn focused_stale_check_returns_none_when_queue_empty() {
        // Returns None only when the focus index has no entry —
        // the empty-queue case.
        let q = ApprovalQueue::new();
        let path = std::env::temp_dir();
        assert!(q.focused_stale_check(&path).is_none());
    }

    #[test]
    fn focused_stale_check_detects_modified_file() {
        // Take a digest, modify the file, ensure stale_check
        // surfaces the change. Locks the contract that approvals
        // bound to a digest can see "the file is no longer the
        // file you approved".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"baseline").unwrap();
        let digest = astra_turn_core::approval_base_digest::compute_file_digest(&path)
            .unwrap()
            .unwrap();

        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "write_file".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_base_digest(digest),
        );

        // Mutate the file behind the queue's back.
        std::fs::write(&path, b"changed").unwrap();
        let result = q.focused_stale_check(&path).unwrap().unwrap();
        match result {
            astra_turn_core::approval_base_digest::StaleCheck::Stale { .. } => {}
            other => panic!("expected Stale, got {other:?}"),
        }
    }

    #[test]
    fn focused_stale_check_fresh_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, b"baseline").unwrap();
        let digest = astra_turn_core::approval_base_digest::compute_file_digest(&path)
            .unwrap()
            .unwrap();

        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "write_file".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_base_digest(digest),
        );

        let result = q.focused_stale_check(&path).unwrap().unwrap();
        assert!(result.is_fresh());
    }

    // ── Issue #326 P4 / R2 Critical 1: dedup ─────────────────

    fn fixed_request_key(
        args_seed: &str,
    ) -> astra_turn_core::approval_request_key::ApprovalRequestKey {
        let args = serde_json::json!({"seed": args_seed});
        astra_turn_core::approval_request_key::ApprovalRequestKey::new(
            "bash",
            std::env::temp_dir(),
            &args,
            None,
            uuid::Uuid::nil(),
        )
    }

    fn fixed_batch_group(
        tool_family: &str,
        risk_tags: &[&str],
    ) -> astra_turn_core::approval_batch_group::ApprovalBatchGroupKey {
        astra_turn_core::approval_batch_group::ApprovalBatchGroupKey::new(
            tool_family,
            "ReadOnly",
            risk_tags.iter().map(|tag| (*tag).to_string()),
            uuid::Uuid::nil(),
        )
        .with_scope_root("/repo")
    }

    #[test]
    fn dedup_merges_equal_request_keys_into_one_entry() {
        let mut q = ApprovalQueue::new();
        let key = fixed_request_key("npm-test");

        let (tx_a, _rx_a) = oneshot::channel();
        let id_a = q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_request_key(key.clone()),
        );
        assert_eq!(q.len(), 1);

        // Second push with the SAME request_key must NOT
        // create a new entry — the sender is appended to the
        // existing one's response_txs.
        let (tx_b, _rx_b) = oneshot::channel();
        let id_b = q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_request_key(key.clone()),
        );
        assert_eq!(q.len(), 1, "dedup must keep queue length at 1");
        assert_eq!(id_a, id_b, "dedup returns the existing entry's id");
    }

    #[test]
    fn dedup_does_not_merge_different_request_keys() {
        let mut q = ApprovalQueue::new();

        let (tx_a, _rx_a) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_request_key(fixed_request_key("a")),
        );
        let (tx_b, _rx_b) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_request_key(fixed_request_key("b")),
        );
        assert_eq!(q.len(), 2, "different keys must NOT collapse");
    }

    #[test]
    fn dedup_broadcasts_response_to_all_senders() {
        // Critical contract: when the user resolves the merged
        // entry, every dedup'd sender receives the same answer.
        let mut q = ApprovalQueue::new();
        let key = fixed_request_key("npm-test");

        let (tx_a, mut rx_a) = oneshot::channel();
        let (tx_b, mut rx_b) = oneshot::channel();
        let (tx_c, mut rx_c) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_request_key(key.clone()),
        );
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_request_key(key.clone()),
        );
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_c,
            ApprovalMetadata::default().with_request_key(key),
        );
        assert_eq!(q.len(), 1);

        assert!(q.respond_focused(ApprovalResponse::AllowOnce));
        assert_eq!(rx_a.try_recv().unwrap(), ApprovalResponse::AllowOnce);
        assert_eq!(rx_b.try_recv().unwrap(), ApprovalResponse::AllowOnce);
        assert_eq!(rx_c.try_recv().unwrap(), ApprovalResponse::AllowOnce);
    }

    #[test]
    fn batch_group_key_is_carried_to_pending() {
        use astra_turn_core::approval_batch_group::ApprovalBatchGroupKey;
        let mut q = ApprovalQueue::new();
        let group = ApprovalBatchGroupKey::new(
            "Read",
            "ReadOnly",
            ["BashExecute".to_string()],
            uuid::Uuid::nil(),
        );

        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "read_file".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_batch_group_key(group.clone()),
        );

        let entry = q.entries.front().unwrap();
        assert_eq!(entry.batch_group_key.as_ref(), Some(&group));
    }

    #[test]
    fn respond_focused_group_resolves_only_matching_batch_group() {
        let mut q = ApprovalQueue::new();
        let group_a = fixed_batch_group("Read(src)", &["BashExecute"]);
        let group_b = fixed_batch_group("Read(tests)", &["BashExecute"]);

        let (tx_a, mut rx_a) = oneshot::channel();
        q.push_with_metadata(
            "read_file".into(),
            "a".into(),
            None,
            "read".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_batch_group_key(group_a.clone()),
        );
        let (tx_b, mut rx_b) = oneshot::channel();
        q.push_with_metadata(
            "read_file".into(),
            "b".into(),
            None,
            "read".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_batch_group_key(group_b),
        );
        let (tx_c, mut rx_c) = oneshot::channel();
        q.push_with_metadata(
            "read_file".into(),
            "c".into(),
            None,
            "read".into(),
            serde_json::Value::Null,
            tx_c,
            ApprovalMetadata::default().with_batch_group_key(group_a),
        );

        assert_eq!(q.respond_focused_group(ApprovalResponse::AllowOnce), 2);
        assert_eq!(q.len(), 1);
        assert_eq!(q.focused().unwrap().header, "b");
        assert_eq!(rx_a.try_recv().unwrap(), ApprovalResponse::AllowOnce);
        assert!(
            rx_b.try_recv().is_err(),
            "cross-group entry must stay pending"
        );
        assert_eq!(rx_c.try_recv().unwrap(), ApprovalResponse::AllowOnce);
    }

    #[test]
    fn respond_focused_group_without_key_resolves_only_focused_entry() {
        let mut q = ApprovalQueue::new();
        let (tx_a, mut rx_a) = oneshot::channel();
        q.push(
            "bash".into(),
            "a".into(),
            None,
            "run".into(),
            serde_json::Value::Null,
            tx_a,
        );
        let (tx_b, mut rx_b) = oneshot::channel();
        q.push(
            "bash".into(),
            "b".into(),
            None,
            "run".into(),
            serde_json::Value::Null,
            tx_b,
        );
        let (tx_c, mut rx_c) = oneshot::channel();
        q.push(
            "bash".into(),
            "c".into(),
            None,
            "run".into(),
            serde_json::Value::Null,
            tx_c,
        );

        assert_eq!(q.respond_focused_group(ApprovalResponse::AllowOnce), 1);
        assert_eq!(q.len(), 2);
        assert_eq!(rx_a.try_recv().unwrap(), ApprovalResponse::AllowOnce);
        assert!(
            rx_b.try_recv().is_err(),
            "ungrouped entry must stay pending"
        );
        assert!(
            rx_c.try_recv().is_err(),
            "ungrouped entry must stay pending"
        );
    }

    #[test]
    fn respond_focused_group_rejects_accept_all_for_destructive_group() {
        let mut q = ApprovalQueue::new();
        let group = fixed_batch_group("Bash(rm)", &["GitDestructive"]);

        let (tx_a, mut rx_a) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "rm a".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_batch_group_key(group.clone()),
        );
        let (tx_b, mut rx_b) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "rm b".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_batch_group_key(group),
        );

        assert_eq!(q.respond_focused_group(ApprovalResponse::AllowOnce), 0);
        assert_eq!(q.len(), 2);
        assert!(
            rx_a.try_recv().is_err(),
            "dangerous Yes to all must not send"
        );
        assert!(
            rx_b.try_recv().is_err(),
            "dangerous Yes to all must not send"
        );

        assert_eq!(q.respond_focused_group(ApprovalResponse::Deny), 2);
        assert_eq!(q.len(), 0);
        assert_eq!(rx_a.try_recv().unwrap(), ApprovalResponse::Deny);
        assert_eq!(rx_b.try_recv().unwrap(), ApprovalResponse::Deny);
    }

    #[test]
    fn dangerous_group_accept_all_button_has_no_action() {
        let mut q = ApprovalQueue::new();
        let group = fixed_batch_group("Bash(rm)", &["GitDestructive"]);

        let (tx_a, _rx_a) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "rm a".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_batch_group_key(group.clone()),
        );
        let (tx_b, _rx_b) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "rm b".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_batch_group_key(group),
        );

        // Move from Yes to Yes to all (index 3).
        for _ in 0..3 {
            q.focused_button_move_right();
        }
        assert!(
            q.focused_button_action().is_none(),
            "dangerous groups must not activate Yes to all"
        );

        q.focused_button_move_right();
        assert_eq!(
            q.focused_button_action(),
            Some(super::super::button_row::ButtonAction::RespondAll(
                ApprovalResponse::Deny
            )),
            "No to all remains available for dangerous groups"
        );
    }

    #[test]
    fn focused_button_action_blocks_unavailable_workspace_always() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "npm test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_workspace_untrusted(true),
        );

        // Yes -> don't ask again.
        q.focused_button_move_right();
        assert_eq!(
            q.focused_button_action(),
            Some(super::super::button_row::ButtonAction::Respond(
                ApprovalResponse::AlwaysAllow
            )),
            "benign untrusted-workspace request should keep don't-ask-again available via session fallback"
        );

        q.focused_button_move_left();
        assert_eq!(
            q.focused_button_action(),
            Some(super::super::button_row::ButtonAction::Respond(
                ApprovalResponse::AllowOnce
            )),
            "Yes remains available for untrusted workspaces"
        );
    }

    #[test]
    fn focused_button_action_blocks_always_for_compound_commands() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "cd rust && cargo test".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_scope_shape(true, false),
        );

        q.focused_button_move_right();
        assert!(
            q.focused_button_action().is_none(),
            "compound shell commands must keep Always disabled"
        );
    }

    #[test]
    fn focused_button_action_blocks_always_for_unsafe_rule_shape() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "bash".into(),
            "bash".into(),
            None,
            "execute".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_unsafe_rule_shape(true),
        );

        q.focused_button_move_right();
        assert!(
            q.focused_button_action().is_none(),
            "requests without a safe match target must not offer Always"
        );
    }

    #[test]
    fn untrusted_workspace_view_explains_how_to_trust() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push_with_metadata(
            "git".into(),
            "Git show HEAD".into(),
            None,
            "This command needs your approval before it runs.".into(),
            serde_json::Value::Null,
            tx,
            ApprovalMetadata::default().with_workspace_untrusted(true),
        );

        let view = q
            .views()
            .into_iter()
            .next()
            .expect("pending approval should exist");
        assert_eq!(
            view.selection_hint.as_deref(),
            Some(
                "Don't ask again stays session-only until you trust this workspace. Choose Trust Workspace or run `/allow trust` to save workspace rules."
            )
        );
    }

    // ── drain_now_allowed: post-mode-pivot cleanup ────────────────
    //
    // Regression for session 6953d1da: pending approvals from a
    // restrictive mode (Edit / Plan) hung around in the queue after
    // the user pivoted to Auto, leaving the "auto · ⏸ 1 pending"
    // visual contradiction on the status line. Mode pivots must
    // re-evaluate every entry and drain those the new mode would
    // not gate.

    #[test]
    fn drain_now_allowed_releases_entries_predicate_drops() {
        let mut q = ApprovalQueue::new();
        let (tx_keep, _rx_keep) = oneshot::channel();
        let (tx_drop, mut rx_drop) = oneshot::channel();
        q.push(
            "write_file".into(),
            "h1".into(),
            None,
            "r1".into(),
            serde_json::Value::Null,
            tx_keep,
        );
        q.push(
            "bash".into(),
            "h2".into(),
            None,
            "r2".into(),
            serde_json::Value::Null,
            tx_drop,
        );

        // Predicate: keep write_file, drop bash.
        let released = q.drain_now_allowed(|entry| entry.tool == "write_file");

        assert_eq!(released, 1, "exactly one entry should be released");
        assert_eq!(q.len(), 1, "the kept entry must remain in the queue");
        assert_eq!(
            rx_drop.try_recv().unwrap(),
            crate::cli::chat_stream::ApprovalResponse::AllowOnce,
            "released entries must broadcast AllowOnce to their senders"
        );
    }

    #[test]
    fn drain_now_allowed_normalises_focus_when_focused_entry_drops() {
        let mut q = ApprovalQueue::new();
        let (tx_a, _rx_a) = oneshot::channel();
        let (tx_b, _rx_b) = oneshot::channel();
        q.push(
            "a".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx_a,
        );
        q.push(
            "b".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx_b,
        );
        // Focus the second (b)
        q.move_focus_down();
        assert_eq!(q.focus_index(), Some(1));

        // Drop b; focus should re-anchor onto a.
        q.drain_now_allowed(|entry| entry.tool == "a");
        assert_eq!(q.len(), 1);
        assert_eq!(
            q.focus_index(),
            Some(0),
            "focus must clamp into the surviving entries after a drop"
        );
    }

    #[test]
    fn drain_now_allowed_zero_when_predicate_keeps_everything() {
        let mut q = ApprovalQueue::new();
        let (tx, _rx) = oneshot::channel();
        q.push(
            "bash".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx,
        );
        let released = q.drain_now_allowed(|_| true);
        assert_eq!(released, 0);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn drain_now_allowed_broadcasts_to_every_sender_on_a_deduped_entry() {
        let mut q = ApprovalQueue::new();
        let key = astra_turn_core::approval_request_key::ApprovalRequestKey {
            tool: "bash".into(),
            args_hash: [9; 32],
            payload_hash: None,
            canonical_cwd: "/tmp".into(),
            source_agent: None,
            turn_id: uuid::Uuid::nil(),
        };
        let (tx_a, mut rx_a) = oneshot::channel();
        let (tx_b, mut rx_b) = oneshot::channel();
        // Same request_key triggers dedup → both senders ride a single entry.
        q.push_with_metadata(
            "bash".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx_a,
            ApprovalMetadata::default().with_request_key(key.clone()),
        );
        q.push_with_metadata(
            "bash".into(),
            "h".into(),
            None,
            "r".into(),
            serde_json::Value::Null,
            tx_b,
            ApprovalMetadata::default().with_request_key(key),
        );
        assert_eq!(q.len(), 1, "dedup should keep one entry");

        let released = q.drain_now_allowed(|_| false);
        assert_eq!(released, 1);
        // Both waiters get the same Allow.
        assert_eq!(
            rx_a.try_recv().unwrap(),
            crate::cli::chat_stream::ApprovalResponse::AllowOnce,
        );
        assert_eq!(
            rx_b.try_recv().unwrap(),
            crate::cli::chat_stream::ApprovalResponse::AllowOnce,
        );
    }
}
