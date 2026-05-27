pub(crate) mod ask_user_view;
pub(crate) mod busy_view;
pub(crate) mod chat_composer;
pub(crate) mod config_edit_view;
pub(crate) mod context_panel_view;
pub(crate) mod footer;
pub(crate) mod help_view;
pub(crate) mod history_view;
pub(crate) mod in_flight_agents_view;
pub(crate) mod info_view;
pub(crate) mod list_selection_view;
pub(crate) mod login_view;
pub(crate) mod paste_burst;
pub(crate) mod plan_review_view;
pub(crate) mod session_picker_view;
pub(crate) mod skill_popup;
pub(crate) mod table_view;
pub(crate) mod task_detail_view;
pub(crate) mod textarea;
pub(crate) mod timeline_view;
pub(crate) mod transcript_view;
pub(crate) mod view;
pub(crate) mod worktrees_view;

#[cfg(test)]
mod approval_integration_tests;
#[cfg(test)]
mod ask_user_integration_tests;
#[cfg(test)]
mod config_edit_tests;
#[cfg(test)]
mod hint_tests;
#[cfg(test)]
mod keyboard_tests;
#[cfg(test)]
mod mention_integration_tests;
#[cfg(test)]
mod plan_review_integration_tests;
#[cfg(test)]
mod slash_integration_tests;

use chat_composer::{ChatComposer, ComposerAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use footer::Footer;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
};
use skill_popup::SkillPopup;
use view::{BottomPaneView, CancellationEvent};

use super::approval::{ApprovalQueue, ApprovalView, ButtonAction};
use super::mention_menu::{
    FileProvider, MentionMenu, extract_mention_at, popup as mention_popup_render,
};
use super::slash_menu::{SlashItem, SlashMenu, is_open_for, popup as slash_popup_render};
use super::task_status::TaskStatus;
use crate::chat_stream::ApprovalResponse;
use ask_user_view::AskUserView;
use std::sync::Arc;
use tokio::sync::oneshot;

pub(crate) struct BottomPane {
    pub composer: ChatComposer,
    pub footer: Footer,
    view_stack: Vec<Box<dyn BottomPaneView>>,
    task_status: TaskStatus,
    slash_menu: Option<SlashMenu>,
    slash_items: Vec<SlashItem>,
    skill_popup: Option<SkillPopup>,
    skill_items: Vec<skill_popup::SkillItem>,
    mention_menu: Option<MentionMenu>,
    /// Byte range `[at_byte, end_byte)` within the composer that the
    /// active mention covers, used for splicing on accept.
    mention_range: Option<(usize, usize)>,
    file_provider: Option<Arc<dyn FileProvider>>,
    approval_queue: ApprovalQueue,
    pub queued_messages: Vec<String>,
}

impl BottomPane {
    pub fn new() -> Self {
        Self {
            composer: ChatComposer::new(),
            footer: Footer::new(),
            view_stack: Vec::new(),
            task_status: TaskStatus::Idle,
            slash_menu: None,
            slash_items: Vec::new(),
            skill_popup: None,
            skill_items: Vec::new(),
            mention_menu: None,
            mention_range: None,
            file_provider: None,
            approval_queue: ApprovalQueue::new(),
            queued_messages: Vec::new(),
        }
    }

    /// Pop the last queued message back into composer for editing.
    pub fn edit_last_queued(&mut self) -> bool {
        if let Some(msg) = self.queued_messages.pop() {
            self.composer.set_text(&msg);
            true
        } else {
            false
        }
    }

    /// Take the first queued message for auto-dispatch.
    pub fn take_next_queued(&mut self) -> Option<String> {
        if self.queued_messages.is_empty() {
            None
        } else {
            Some(self.queued_messages.remove(0))
        }
    }

    pub fn set_skill_items(&mut self, items: Vec<skill_popup::SkillItem>) {
        self.skill_items = items;
    }

    /// Inject the slash-command catalog used by the inline menu.
    pub fn set_slash_items(&mut self, items: Vec<SlashItem>) {
        self.slash_items = items;
    }

    /// Refresh the dynamic completions injected into the `/mcp` slash item.
    ///
    /// Called whenever MCP servers connect or their tool lists change so that
    /// tab-completing `/mcp inspect`, `/mcp tools`, `/mcp ping`, etc. shows
    /// the live server and tool names.
    pub fn update_mcp_completions(&mut self, extras: Vec<(String, String)>) {
        if let Some(item) = self.slash_items.iter_mut().find(|i| i.name == "/mcp") {
            item.extra_subcommands = extras;
            // Drop any open menu so it re-builds with the new completions
            // next time the user types.
            self.slash_menu = None;
        }
    }

    /// Inject the [`FileProvider`] used by the `@`-mention menu.
    pub fn set_file_provider(&mut self, provider: Arc<dyn FileProvider>) {
        self.file_provider = Some(provider);
    }

    #[cfg(test)]
    pub(crate) fn mention_menu_is_open(&self) -> bool {
        self.mention_menu.is_some()
    }
    #[cfg(test)]
    pub(crate) fn mention_menu_names(&self) -> Vec<String> {
        self.mention_menu
            .as_ref()
            .map(|m| m.matches().iter().map(|e| e.path.clone()).collect())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn slash_menu_is_open(&self) -> bool {
        self.slash_menu.is_some()
    }
    #[cfg(test)]
    pub(crate) fn slash_menu_len(&self) -> usize {
        self.slash_menu.as_ref().map(|m| m.len()).unwrap_or(0)
    }
    #[cfg(test)]
    pub(crate) fn slash_menu_selected_name(&self) -> Option<&str> {
        self.slash_menu
            .as_ref()
            .and_then(|m| m.selected_item())
            .map(|i| i.name)
    }
    #[cfg(test)]
    pub(crate) fn slash_menu_names(&self) -> Vec<String> {
        self.slash_menu
            .as_ref()
            .map(|m| m.matches().iter().map(|i| i.name.to_string()).collect())
            .unwrap_or_default()
    }

    /// Handle a bracketed-paste payload. Multi-line pastes get
    /// collapsed into a `[Pasted #N · M lines]` placeholder; short
    /// pastes are inserted verbatim. After the paste lands, popup
    /// state is resynced because paste can newly trigger `/`, `@`, `$`.
    pub fn handle_paste(&mut self, text: &str) {
        self.composer.handle_paste(text);
        self.sync_popups();
    }

    pub fn set_task_status(&mut self, status: TaskStatus) {
        let was_active = self.task_status.is_active();
        self.task_status = status;
        let now_active = self.task_status.is_active();
        if was_active != now_active {
            self.footer.is_turn_active = now_active;
            // Refresh cwd + git branch only on the idle→active edge
            // (turn start). The active→idle edge would just repeat the
            // same probe a few seconds later, and intra-turn status
            // transitions (WaitingModel ↔ ToolExecuting) don't move
            // the branch. One gix discover per turn is the budget.
            if now_active {
                self.footer.refresh_env();
            }
        }
    }

    pub fn push_view(&mut self, view: Box<dyn BottomPaneView>) {
        self.view_stack.push(view);
    }

    pub fn enqueue_ask_user(
        &mut self,
        prompt: crate::chat_stream::AskUserPrompt,
        response_tx: oneshot::Sender<crate::chat_stream::AskUserResponse>,
    ) {
        self.view_stack
            .push(Box::new(AskUserView::new(prompt, response_tx)));
    }

    /// Surface the plan-review overlay used by `exit_plan_mode`.
    /// Pushes a dedicated `PlanReviewView` onto the view stack — the
    /// overlay self-resolves on submit/cancel and is popped via the
    /// usual `is_complete()` cleanup path.
    pub fn enqueue_plan_review(
        &mut self,
        plan_markdown: String,
        response_tx: oneshot::Sender<crate::chat_stream::PlanReviewDecision>,
    ) {
        self.view_stack
            .push(Box::new(plan_review_view::PlanReviewView::new(
                plan_markdown,
                response_tx,
            )));
    }

    pub fn refresh_task_detail(
        &mut self,
        id: &str,
        cell: &crate::tui::history_cell::task::TaskCell,
    ) -> bool {
        self.active_view_mut()
            .is_some_and(|view| view.refresh_task_cell(id, cell))
    }

    pub fn active_live_task_id(&self) -> Option<&str> {
        self.view_stack.last().and_then(|view| view.live_task_id())
    }

    pub fn refresh_agent_rows(
        &mut self,
        rows: Vec<crate::tui::bottom_pane::in_flight_agents_view::AgentRow>,
    ) -> bool {
        self.active_view_mut()
            .is_some_and(|view| view.refresh_agent_rows(rows))
    }

    pub fn agent_monitor_is_open(&self) -> bool {
        self.view_stack
            .last()
            .is_some_and(|view| view.accepts_agent_rows())
    }

    #[allow(dead_code)]
    pub fn pop_view(&mut self) -> Option<Box<dyn BottomPaneView>> {
        self.view_stack.pop()
    }

    pub fn has_active_view(&self) -> bool {
        !self.view_stack.is_empty()
    }

    #[allow(clippy::borrowed_box)]
    fn active_view(&self) -> Option<&Box<dyn BottomPaneView>> {
        self.view_stack.last()
    }

    fn active_view_mut(&mut self) -> Option<&mut Box<dyn BottomPaneView>> {
        self.view_stack.last_mut()
    }

    fn popup_height(&self) -> u16 {
        if let Some(m) = &self.slash_menu {
            return slash_popup_render::desired_height(m);
        }
        if let Some(m) = &self.mention_menu {
            return mention_popup_render::desired_height(m);
        }
        if let Some(p) = &self.skill_popup {
            return p.height();
        }
        0
    }

    pub fn sync_popups(&mut self) {
        let text = self.composer.text();

        // Suppress all popups while the user is browsing history with
        // Up/Down — otherwise a history entry starting with '/' or '@'
        // opens a menu that captures arrow keys and blocks further
        // history traversal.
        if self.composer.is_browsing_history() {
            self.slash_menu = None;
            self.close_mention();
            self.skill_popup = None;
            return;
        }

        // Slash menu: open whenever the first line starts with '/'. Empty
        // matches still keep the menu open so users see a "no matches"
        // message rather than silent closure.
        if self.view_stack.is_empty() && is_open_for(&text) && !self.slash_items.is_empty() {
            self.close_mention();
            self.skill_popup = None;
            match self.slash_menu.as_mut() {
                Some(menu) => menu.set_filter(&text),
                None => {
                    let mut menu = SlashMenu::new(self.slash_items.clone());
                    menu.set_filter(&text);
                    self.slash_menu = Some(menu);
                }
            }
            return;
        }

        // Mention menu: active when the cursor is inside an `@token`.
        if self.view_stack.is_empty() && self.file_provider.is_some() {
            let cursor = self.composer.cursor_byte();
            if let Some(token) = extract_mention_at(&text, cursor) {
                self.slash_menu = None;
                self.skill_popup = None;
                self.mention_range = Some((token.at_byte, token.end_byte));
                let provider = self.file_provider.clone().unwrap();
                match self.mention_menu.as_mut() {
                    Some(menu) => menu.set_token(&token),
                    None => {
                        let mut menu = MentionMenu::from_arc(provider);
                        menu.set_token(&token);
                        self.mention_menu = Some(menu);
                    }
                }
                return;
            }
        }

        if self.view_stack.is_empty() && text.starts_with('$') && !self.skill_items.is_empty() {
            self.slash_menu = None;
            self.close_mention();
            let popup = self
                .skill_popup
                .get_or_insert_with(|| SkillPopup::new(self.skill_items.clone()));
            popup.set_filter(&text);
            if popup.is_empty() {
                self.skill_popup = None;
            }
            return;
        }

        self.slash_menu = None;
        self.close_mention();
        self.skill_popup = None;
    }

    fn close_mention(&mut self) {
        self.mention_menu = None;
        self.mention_range = None;
    }

    // ── Approval queue public API ──────────────────────────────

    /// Enqueue a new pending approval. Also bumps the footer counter so
    /// the status line reflects the change on next draw.
    pub fn enqueue_approval(
        &mut self,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        args: serde_json::Value,
        response_tx: oneshot::Sender<ApprovalResponse>,
    ) -> u64 {
        let id = self
            .approval_queue
            .push(tool, header, detail, reason, args, response_tx);
        self.footer.pending_approvals = self.approval_queue.len();
        id
    }

    /// Issue #326 P3: enqueue with the full metadata bundle. Used
    /// by the stream-render gate when it has source_agent / risk
    /// tags / Will-save preview / host context to attach.
    #[allow(clippy::too_many_arguments)]
    pub fn enqueue_approval_with_metadata(
        &mut self,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        args: serde_json::Value,
        response_tx: oneshot::Sender<ApprovalResponse>,
        metadata: crate::tui::approval::queue::ApprovalMetadata,
    ) -> u64 {
        let id = self.approval_queue.push_with_metadata(
            tool,
            header,
            detail,
            reason,
            args,
            response_tx,
            metadata,
        );
        self.footer.pending_approvals = self.approval_queue.len();
        id
    }

    /// Re-evaluate every pending approval against `new_mode` and
    /// auto-approve the ones the new mode would not gate. Used when
    /// the user pivots permission modes (Shift+Tab, `/permissions`,
    /// or the `exit_plan_mode` overlay) so the approval queue does
    /// not lag behind the chip.
    ///
    /// Returns the number of entries auto-approved. Footer counter
    /// is refreshed so the chip reads the new pending count.
    pub fn reevaluate_approvals_for_mode(
        &mut self,
        new_mode: crate::permission_manager::PermissionMode,
    ) -> usize {
        use astra_turn_core::permission::engine::{HardDecision, evaluate_permission};
        use astra_turn_core::permission::types::{InheritedPermissions, PermissionSyncContext};

        let ctx = PermissionSyncContext::new(InheritedPermissions::new(new_mode));
        let released = self.approval_queue.drain_now_allowed(|entry| {
            // Cloud / sandbox-expand entries arrive without args
            // (Value::Null). We can't safely re-evaluate those —
            // the cloud path owns its own gate — so leave them in
            // the queue and let the user resolve them explicitly.
            if entry.args.is_null() {
                return true;
            }
            let envelope = evaluate_permission(&entry.tool, &entry.args, &ctx);
            // Keep the entry only if the new mode still needs an
            // external approval. Allow / Deny outcomes mean "no
            // user-facing prompt is required any more"; they are
            // dropped from the queue (Deny is the rarer path —
            // historically the approval card stayed open even for
            // a guaranteed-deny call which was just noise).
            matches!(envelope.decision, HardDecision::NeedExternal { .. })
        });
        self.footer.pending_approvals = self.approval_queue.len();
        released
    }

    /// Snapshot of pending approvals (safe to pass to rendering code).
    pub fn approval_views(&self) -> Vec<ApprovalView> {
        self.approval_queue.views()
    }

    /// Index of the currently focused approval, if any.
    pub fn focused_approval_index(&self) -> Option<usize> {
        self.approval_queue.focus_index()
    }

    /// True if at least one approval is pending.
    pub fn has_pending_approvals(&self) -> bool {
        !self.approval_queue.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn pending_approval_count(&self) -> usize {
        self.approval_queue.len()
    }

    /// Resolve the currently focused approval. Returns its id if one was
    /// resolved — callers pair this with an [`Action::ApprovalResolved`]
    /// dispatch to update reducer state.
    pub fn respond_focused_approval(&mut self, response: ApprovalResponse) -> Option<u64> {
        let focused_id = self.approval_queue.focused().map(|p| p.id);
        if self.approval_queue.respond_focused(response) {
            self.footer.pending_approvals = self.approval_queue.len();
            focused_id
        } else {
            None
        }
    }

    /// Resolve a specific approval by id. Used when the user clicks or
    /// otherwise targets a non-focused entry.
    pub fn respond_approval_by_id(&mut self, id: u64, response: ApprovalResponse) -> bool {
        let ok = self.approval_queue.respond_by_id(id, response);
        if ok {
            self.footer.pending_approvals = self.approval_queue.len();
        }
        ok
    }

    /// Move focus within the pending-approval queue.
    pub fn move_approval_focus_up(&mut self) {
        self.approval_queue.move_focus_up();
    }
    pub fn move_approval_focus_down(&mut self) {
        self.approval_queue.move_focus_down();
    }

    /// Move button focus **within** the currently focused approval row.
    pub fn move_approval_button_left(&mut self) {
        self.approval_queue.focused_button_move_left();
    }
    pub fn move_approval_button_right(&mut self) {
        self.approval_queue.focused_button_move_right();
    }

    /// Activate the currently focused button on the currently focused
    /// approval. Returns:
    /// - `Some((id, None))` if a single entry was resolved,
    /// - `Some((0, Some(n)))` if a batch action resolved `n` entries,
    /// - `None` if there's nothing to act on.
    pub fn activate_focused_approval_button(&mut self) -> Option<ApprovalActivation> {
        let action = self.approval_queue.focused_button_action()?;
        match action {
            ButtonAction::Respond(resp) => {
                let id = self.respond_focused_approval(resp.clone())?;
                Some(ApprovalActivation::Single { id, response: resp })
            }
            ButtonAction::RespondAll(resp) => {
                let n = self.approval_queue.respond_focused_group(resp.clone());
                self.footer.pending_approvals = self.approval_queue.len();
                Some(ApprovalActivation::Batch {
                    count: n,
                    response: resp,
                })
            }
        }
    }

    /// Default-reject the focused approval (Esc shortcut).
    pub fn reject_focused_approval(&mut self) -> Option<u64> {
        self.respond_focused_approval(ApprovalResponse::Deny)
    }

    /// Splice the selected mention entry into the composer, replacing
    /// the current mention range. Called by Tab/Enter handlers.
    /// Returns whether anything was spliced (selection must exist).
    fn accept_mention(&mut self) -> bool {
        let Some((at, end)) = self.mention_range else {
            return false;
        };
        let Some(entry) = self.mention_menu.as_ref().and_then(|m| m.selected_item()) else {
            return false;
        };

        let replacement = mention_popup_render::format_replacement(entry);
        let text = self.composer.text();
        // Guard against stale ranges if the buffer shrank.
        if end > text.len() || at > end {
            return false;
        }
        let mut new_text = String::with_capacity(text.len() + replacement.len());
        new_text.push_str(&text[..at]);
        new_text.push_str(&replacement);
        new_text.push_str(&text[end..]);
        self.composer.set_text(&new_text);
        self.close_mention();
        true
    }

    fn queue_preview_height(&self) -> u16 {
        if self.queued_messages.is_empty() {
            0
        } else {
            (self.queued_messages.len().min(3) + 1) as u16 // header + up to 3 messages
        }
    }

    /// Build a live `ApprovalCell` from the currently focused queue
    /// entry. `None` when nothing is pending.
    pub fn focused_approval_cell(
        &self,
    ) -> Option<crate::tui::history_cell::approval::ApprovalCell> {
        let view = self.approval_queue.focused_view()?;
        let buttons = self.approval_queue.focused_button_row()?.clone();
        let mut cell = crate::tui::history_cell::approval::ApprovalCell::new(
            view.id,
            view.tool,
            view.header,
            view.detail,
            view.reason,
            true,
        );
        cell.buttons = buttons;
        // Issue #326 P3: forward the view's metadata so the
        // approval card renders the source-agent / host /
        // risk-tag / remember-preview lines populated by
        // enqueue_approval_with_metadata.
        if let Some(agent) = view.source_agent {
            cell = cell.with_source_agent(agent);
        }
        if let Some(host) = view.host {
            cell = cell.with_host(host);
        }
        if !view.risk_tag_labels.is_empty() {
            cell = cell.with_risk_tag_labels(view.risk_tag_labels);
        }
        if let Some(preview) = view.remember_preview {
            cell = cell.with_remember_preview(preview);
        }
        if let Some(hint) = view.selection_hint {
            cell = cell.with_selection_hint(hint);
        }
        cell = cell.with_scope_context(
            view.workspace_untrusted,
            view.is_compound_command,
            view.has_dynamic_eval,
            view.unsafe_rule_shape,
        );
        Some(cell)
    }

    /// Height reserved for the focused approval widget (rendered above
    /// the composer so `←/→/Enter` feedback is visible).
    fn focused_approval_height(&self, width: u16) -> u16 {
        let Some(cell) = self.focused_approval_cell() else {
            return 0;
        };
        use crate::tui::history_cell::HistoryCell;
        cell.desired_height(width)
    }

    pub fn desired_height(&self, width: u16) -> u16 {
        if let Some(view) = self.active_view() {
            let mut h = view.desired_height(width);
            // Reserve a 1-row footer when the view advertises a hint.
            if view.hint_keys().is_some() {
                h = h.saturating_add(1);
            }
            // …and another row for the status line when the view
            // opts in (panels that want context preserved).
            if view.reserve_status_footer() {
                h = h.saturating_add(1);
            }
            return h;
        }
        let content_h = self.composer.desired_height(width);
        let queue_h = self.queue_preview_height();
        let approval_h = self.focused_approval_height(width);
        let popup_h = self.popup_height();
        if popup_h > 0 {
            content_h + queue_h + approval_h + 1 + popup_h
        } else {
            content_h + queue_h + approval_h + 1 + 1
        }
    }

    /// Top-level key routing. Dispatches to named phase handlers so
    /// each concern reads as a single paragraph:
    ///
    /// 1. Ctrl+C / Ctrl+D (quit / interrupt / clear-draft)
    /// 2. Approval queue (← → Enter Esc Tab when an approval is pending)
    /// 3. Active overlay view (push_view'd full-screen widgets)
    /// 4. Popup dismissal (Esc)
    /// 5. Popup navigation (slash / skill / mention menus)
    /// 6. Composer (text input)
    pub fn handle_key(&mut self, key: KeyEvent) -> BottomPaneAction {
        if let Some(a) = self.handle_ctrl_keys(key) {
            return a;
        }
        if let Some(a) = self.handle_approval_keys(key) {
            return a;
        }
        if let Some(a) = self.handle_active_view_key(key) {
            return a;
        }
        if let Some(a) = self.handle_popup_dismiss(key) {
            return a;
        }
        if let Some(a) = self.handle_slash_menu_key(key) {
            return a;
        }
        if let Some(a) = self.handle_skill_popup_key(key) {
            return a;
        }
        if let Some(a) = self.handle_mention_menu_key(key) {
            return a;
        }
        if key.code == KeyCode::BackTab {
            return BottomPaneAction::CyclePermissionMode;
        }
        self.route_to_composer(key)
    }

    fn handle_ctrl_keys(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+C state machine: view.on_ctrl_c → composer clear →
        // interrupt if task active → quit.
        if key.code == KeyCode::Char('c') && ctrl {
            if let Some(view) = self.active_view_mut() {
                match view.on_ctrl_c() {
                    CancellationEvent::Consumed => {
                        self.view_stack.pop();
                        return Some(BottomPaneAction::Consumed);
                    }
                    CancellationEvent::Escalate => {}
                }
            }
            if !self.composer.is_empty() {
                self.composer.clear_draft();
                self.sync_popups();
                return Some(BottomPaneAction::Consumed);
            }
            if self.task_status.is_active() {
                return Some(BottomPaneAction::Interrupt);
            }
            return Some(BottomPaneAction::Quit);
        }
        // Ctrl+D: route by context.
        //
        // Issue #326 P3 / R2 Major 6: plan v3 §P3 wants Reject to be
        // an explicit gesture — the "Esc rejects" shortcut from
        // earlier UI iterations was hostile when an approval popped
        // up while the user was mid-typing in the composer, because
        // pressing Esc to dismiss a popup would silently reject
        // the approval too.
        //
        // Routing:
        // 1. Pending approval + composer empty → Reject focused.
        //    The composer-empty guard means user typing "do this"
        //    pressing the wrong key never accidentally rejects;
        //    Ctrl+D is intentional.
        // 2. Otherwise composer empty (no approval) → quit.
        // 3. Composer not empty → consumed (no-op).
        if key.code == KeyCode::Char('d') && ctrl {
            if self.has_pending_approvals() && self.composer.is_empty() {
                if let Some(id) = self.reject_focused_approval() {
                    return Some(BottomPaneAction::ApprovalResolved { id });
                }
                return Some(BottomPaneAction::Consumed);
            }
            if self.composer.is_empty() && self.view_stack.is_empty() {
                return Some(BottomPaneAction::Quit);
            }
            return Some(BottomPaneAction::Consumed);
        }
        None
    }

    /// When an approval is pending, capture the narrow set of keys
    /// that drive it (← → Enter Esc Tab). Intentionally does NOT map
    /// bare letters or Ctrl+Y/N — the Cursor-style button row already
    /// exposes every action, and a letter shortcut risks consuming
    /// text the user is still typing.
    fn handle_approval_keys(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        if !self.has_pending_approvals() {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl+Enter → quick accept regardless of button focus.
        if key.code == KeyCode::Enter && ctrl {
            if let Some(id) = self.respond_focused_approval(ApprovalResponse::AllowOnce) {
                return Some(BottomPaneAction::ApprovalResolved { id });
            }
            return Some(BottomPaneAction::Consumed);
        }
        match key.code {
            KeyCode::Left | KeyCode::Up => {
                self.move_approval_button_left();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Right | KeyCode::Down => {
                self.move_approval_button_right();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Enter
                if self.composer.is_empty()
                    && self.slash_menu.is_none()
                    && self.mention_menu.is_none()
                    && self.skill_popup.is_none()
                    && self.view_stack.is_empty() =>
            {
                // Composer empty and no popup is capturing Enter —
                // activate the focused approval button. With text
                // in the composer, Enter submits the message
                // instead; `Ctrl+Enter` (handled above) is the
                // explicit approval shortcut.
                self.activate_focused_approval_button()
                    .map(|act| match act {
                        ApprovalActivation::Single { id, .. } => {
                            BottomPaneAction::ApprovalResolved { id }
                        }
                        ApprovalActivation::Batch { .. } => {
                            BottomPaneAction::ApprovalResolved { id: 0 }
                        }
                    })
            }
            KeyCode::Esc
                if self.slash_menu.is_none()
                    && self.mention_menu.is_none()
                    && self.skill_popup.is_none()
                    && self.view_stack.is_empty() =>
            {
                self.reject_focused_approval()
                    .map(|id| BottomPaneAction::ApprovalResolved { id })
            }
            KeyCode::Tab if self.slash_menu.is_none() && self.mention_menu.is_none() => {
                self.move_approval_focus_down();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::BackTab if self.slash_menu.is_none() && self.mention_menu.is_none() => {
                self.move_approval_focus_up();
                Some(BottomPaneAction::Consumed)
            }
            _ => None,
        }
    }

    fn handle_active_view_key(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        let view = self.active_view_mut()?;
        view.handle_key(key);
        if view.is_complete() {
            let completion = view.completion();
            self.view_stack.pop();
            return Some(match completion {
                Some(vc) => BottomPaneAction::ViewCompleted {
                    result: vc.result,
                    reopen: vc.reopen,
                },
                None => BottomPaneAction::ViewCompleted {
                    result: None,
                    reopen: None,
                },
            });
        }
        Some(BottomPaneAction::Consumed)
    }

    fn handle_popup_dismiss(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        if key.code != KeyCode::Esc {
            return None;
        }
        if self.slash_menu.is_some() {
            self.slash_menu = None;
            return Some(BottomPaneAction::Consumed);
        }
        if self.mention_menu.is_some() {
            self.close_mention();
            return Some(BottomPaneAction::Consumed);
        }
        if self.skill_popup.is_some() {
            self.skill_popup = None;
            return Some(BottomPaneAction::Consumed);
        }
        None
    }

    /// Slash menu navigation. Enter with no matches falls through to
    /// the composer so the raw draft gets submitted as-is (avoids a
    /// silent no-op).
    fn handle_slash_menu_key(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        self.slash_menu.as_mut()?;
        match key.code {
            KeyCode::Up => {
                self.slash_menu.as_mut().unwrap().move_up();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Down => {
                self.slash_menu.as_mut().unwrap().move_down();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::PageUp => {
                // Jump by a page-ish chunk. Keep in sync with popup's visible rows.
                self.slash_menu.as_mut().unwrap().page_up(5);
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::PageDown => {
                self.slash_menu.as_mut().unwrap().page_down(5);
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Home => {
                self.slash_menu.as_mut().unwrap().go_first();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::End => {
                self.slash_menu.as_mut().unwrap().go_last();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Tab => {
                if let Some(picked) = self
                    .slash_menu
                    .as_ref()
                    .and_then(|m| m.selected_item())
                    .map(|i| i.name.to_string())
                {
                    self.composer.set_text(&format!("{picked} "));
                    self.slash_menu = None;
                }
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Char(digit) if key.modifiers.is_empty() && digit.is_ascii_digit() => {
                if let Some(index) = digit.to_digit(10) {
                    if index > 0 {
                        if self
                            .slash_menu
                            .as_mut()
                            .is_some_and(|menu| menu.select(index as usize - 1))
                        {
                            if let Some(picked) = self
                                .slash_menu
                                .as_ref()
                                .and_then(|m| m.selected_item())
                                .map(|i| i.name.to_string())
                            {
                                self.composer.set_text(&format!("{picked} "));
                                self.slash_menu = None;
                            }
                        }
                    }
                }
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Enter => {
                if let Some(picked) = self
                    .slash_menu
                    .as_ref()
                    .and_then(|m| m.selected_item())
                    .map(|i| i.name.to_string())
                {
                    self.composer.clear_draft();
                    self.slash_menu = None;
                    return Some(BottomPaneAction::SubmitInput(picked));
                }
                None
            }
            _ => None,
        }
    }

    fn handle_skill_popup_key(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        self.skill_popup.as_mut()?;
        match key.code {
            KeyCode::Up => {
                self.skill_popup.as_mut().unwrap().move_up();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Down => {
                self.skill_popup.as_mut().unwrap().move_down();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Tab | KeyCode::Enter => {
                if let Some(name) = self.skill_popup.as_ref().and_then(|p| p.selected_name()) {
                    self.composer.set_text(&format!("${name} "));
                    self.skill_popup = None;
                }
                Some(BottomPaneAction::Consumed)
            }
            _ => None,
        }
    }

    /// Mention menu: Up/Down/Tab. Enter falls through to the composer
    /// so the user can submit the whole line with an inline mention.
    fn handle_mention_menu_key(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        self.mention_menu.as_mut()?;
        match key.code {
            KeyCode::Up => {
                self.mention_menu.as_mut().unwrap().move_up();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Down => {
                self.mention_menu.as_mut().unwrap().move_down();
                Some(BottomPaneAction::Consumed)
            }
            KeyCode::Tab => {
                if self.accept_mention() {
                    // After accept, re-sync in case the new draft
                    // (e.g. a directory `/`) keeps a menu open.
                    self.sync_popups();
                }
                Some(BottomPaneAction::Consumed)
            }
            _ => None,
        }
    }

    fn route_to_composer(&mut self, key: KeyEvent) -> BottomPaneAction {
        let action = match self.composer.handle_key(key) {
            ComposerAction::Submit => {
                let text = self.composer.clear_and_submit();
                self.slash_menu = None;
                self.close_mention();
                BottomPaneAction::SubmitInput(text)
            }
            ComposerAction::OpenExternalEditor => {
                BottomPaneAction::OpenExternalEditor(self.composer.text())
            }
            ComposerAction::Interrupt => BottomPaneAction::Interrupt,
            ComposerAction::Quit => BottomPaneAction::Quit,
            ComposerAction::Consumed => BottomPaneAction::Consumed,
            ComposerAction::Unhandled => BottomPaneAction::Escalate(key),
        };
        self.sync_popups();
        action
    }

    pub fn pre_draw_tick(&mut self, now: std::time::Instant) {
        if let Some(view) = self.active_view_mut() {
            view.pre_draw_tick(now);
        }
        self.footer.current_objective = self.task_status.objective_label();
        self.footer.turn_elapsed = self.task_status.elapsed();
        // Flush paste burst buffer when idle timeout expires.
        if self.composer.flush_paste_burst() {
            self.sync_popups();
        }
    }

    pub fn replace_composer_text(&mut self, text: &str) {
        self.composer.set_text(text);
        self.sync_popups();
    }

    /// True when something in the bottom pane is currently animating and
    /// needs a follow-up redraw (submit flash today; future tickers can
    /// piggyback here). The outer event loop's 50ms tick already gives
    /// plenty of redraws, so this mostly exists for test clarity.
    pub fn wants_redraw(&self) -> bool {
        self.composer.is_flashing()
    }

    fn render_queue_preview(&self, area: Rect, buf: &mut Buffer) {
        if self.queued_messages.is_empty() || area.height == 0 {
            return;
        }
        let dim = ratatui::style::Style::default().fg(ratatui::style::Color::DarkGray);
        let italic = ratatui::style::Style::default()
            .fg(ratatui::style::Color::DarkGray)
            .add_modifier(ratatui::style::Modifier::ITALIC);
        let mut y = area.y;

        // Header
        if y < area.bottom() {
            let hint = if self.queued_messages.len() == 1 {
                "  ⏳ Queued (↑ to edit):"
            } else {
                "  ⏳ Queued (↑ to edit last):"
            };
            ratatui::widgets::Widget::render(
                ratatui::text::Line::from(ratatui::text::Span::styled(hint, dim)),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }

        for msg in self.queued_messages.iter().take(3) {
            if y >= area.bottom() {
                break;
            }
            let preview: String = msg.chars().take(area.width as usize - 6).collect();
            ratatui::widgets::Widget::render(
                ratatui::text::Line::from(ratatui::text::Span::styled(
                    format!("    ↳ {preview}"),
                    italic,
                )),
                Rect::new(area.x, y, area.width, 1),
                buf,
            );
            y += 1;
        }
    }

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(view) = self.active_view() {
            // Stack optional footers under the view area:
            //   [view]
            //   [hint bar]        — when hint_keys() is Some
            //   [status line]     — when reserve_status_footer()
            let want_status = view.reserve_status_footer();
            let hint = view.hint_keys();
            let footer_rows = u16::from(want_status) + u16::from(hint.is_some());
            if footer_rows > 0 && area.height > footer_rows {
                let view_h = area.height - footer_rows;
                let view_rect = Rect::new(area.x, area.y, area.width, view_h);
                view.render(view_rect, buf);
                let mut y = area.y + view_h;
                if let Some(h) = hint {
                    render_hint_bar(&h, Rect::new(area.x, y, area.width, 1), buf);
                    y += 1;
                }
                if want_status {
                    self.footer.render(Rect::new(area.x, y, area.width, 1), buf);
                }
                return;
            }
            view.render(area, buf);
            return;
        }

        let popup_h = self.popup_height();
        let content_h = self.composer.desired_height(area.width);
        let queue_h = self.queue_preview_height();

        let approval_h = self.focused_approval_height(area.width);
        if popup_h > 0 {
            let chunks = Layout::vertical([
                Constraint::Length(approval_h),
                Constraint::Length(content_h),
                Constraint::Length(queue_h),
                Constraint::Length(1),
                Constraint::Length(popup_h),
            ])
            .split(area);

            self.render_focused_approval(chunks[0], buf);
            self.composer.render(chunks[1], buf);
            self.render_queue_preview(chunks[2], buf);
            if let Some(ref menu) = self.slash_menu {
                slash_popup_render::render(menu, chunks[4], buf);
            } else if let Some(ref menu) = self.mention_menu {
                mention_popup_render::render(menu, chunks[4], buf);
            } else if let Some(ref popup) = self.skill_popup {
                popup.render(chunks[4], buf);
            }
        } else {
            let chunks = Layout::vertical([
                Constraint::Length(approval_h),
                Constraint::Length(content_h),
                Constraint::Length(queue_h),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area);

            self.render_focused_approval(chunks[0], buf);
            self.composer.render(chunks[1], buf);
            self.render_queue_preview(chunks[2], buf);
            self.footer.render(chunks[4], buf);
        }
    }

    fn render_focused_approval(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 {
            return;
        }
        let Some(cell) = self.focused_approval_cell() else {
            return;
        };
        use crate::tui::history_cell::HistoryCell;
        use crate::tui::render::line_utils::sanitize_lines_for_terminal;
        use ratatui::widgets::{Paragraph, Widget, Wrap};
        let lines = sanitize_lines_for_terminal(cell.display_lines(area.width));
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if let Some(view) = self.active_view() {
            return view.cursor_pos(area);
        }

        let content_h = self.composer.desired_height(area.width);
        let chunks =
            Layout::vertical([Constraint::Length(content_h), Constraint::Min(0)]).split(area);

        self.composer.cursor_position(chunks[0])
    }
}

/// Render a one-row dim hint bar at the bottom of a view area.
fn render_hint_bar(hint: &str, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Widget;
    let styled = Line::from(vec![
        Span::raw("  "),
        Span::styled(hint.to_string(), Style::default().fg(Color::DarkGray)),
    ]);
    Widget::render(styled, area, buf);
}

/// Summary of what happened when the user activates a button on the
/// focused approval cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ApprovalActivation {
    /// Resolved a single entry via its button.
    Single { id: u64, response: ApprovalResponse },
    /// Resolved the whole queue via Accept-all / Reject-all.
    Batch {
        count: usize,
        response: ApprovalResponse,
    },
}

#[derive(Debug)]
pub(crate) enum BottomPaneAction {
    SubmitInput(String),
    OpenExternalEditor(String),
    CyclePermissionMode,
    ViewCompleted {
        result: Option<String>,
        reopen: Option<String>,
    },
    Interrupt,
    Quit,
    Consumed,
    Escalate(KeyEvent),
    /// An approval was resolved — the outer event loop should dispatch
    /// `Action::ApprovalResolved(id)` so `State::pending_approvals` stays
    /// in sync with the internal queue.
    ApprovalResolved {
        id: u64,
    },
}
