pub(crate) mod busy_view;
pub(crate) mod chat_composer;
pub(crate) mod config_edit_view;
pub(crate) mod context_panel_view;
pub(crate) mod footer;
pub(crate) mod help_view;
pub(crate) mod history_view;
pub(crate) mod info_view;
pub(crate) mod list_selection_view;
pub(crate) mod login_view;
pub(crate) mod paste_burst;
pub(crate) mod session_picker_view;
pub(crate) mod skill_popup;
pub(crate) mod table_view;
pub(crate) mod textarea;
pub(crate) mod timeline_view;
pub(crate) mod transcript_view;
pub(crate) mod view;
pub(crate) mod worktrees_view;

#[cfg(test)]
mod approval_integration_tests;
#[cfg(test)]
mod config_edit_tests;
#[cfg(test)]
mod hint_tests;
#[cfg(test)]
mod mention_integration_tests;
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
        response_tx: oneshot::Sender<ApprovalResponse>,
    ) -> u64 {
        let id = self
            .approval_queue
            .push(tool, header, detail, reason, response_tx);
        self.footer.pending_approvals = self.approval_queue.len();
        id
    }

    /// Issue #326 P3: enqueue with the full metadata bundle. Used
    /// by the stream-render gate when it has source_agent / risk
    /// tags / Will-save preview / host context to attach.
    pub fn enqueue_approval_with_metadata(
        &mut self,
        tool: String,
        header: String,
        detail: Option<String>,
        reason: String,
        response_tx: oneshot::Sender<ApprovalResponse>,
        metadata: crate::tui::approval::queue::ApprovalMetadata,
    ) -> u64 {
        let id = self.approval_queue.push_with_metadata(
            tool,
            header,
            detail,
            reason,
            response_tx,
            metadata,
        );
        self.footer.pending_approvals = self.approval_queue.len();
        id
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
            ButtonAction::SelectScope(scope) => {
                self.approval_queue.select_scope_for_focused(scope);
                None
            }
            ButtonAction::SelectMatch(target) => {
                let response = self.approval_queue.response_for_match_target(target)?;
                let id = self.respond_focused_approval(response.clone())?;
                Some(ApprovalActivation::Single { id, response })
            }
            ButtonAction::EditCustomPrefix => {
                self.approval_queue.enter_custom_prefix_for_focused();
                None
            }
            ButtonAction::BackToScopes => {
                self.approval_queue.back_to_scope_for_focused();
                None
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
        // risk-tag / will-save lines populated by
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
        if let Some(preview) = view.will_save_preview {
            cell = cell.with_will_save_preview(preview);
        }
        if let Some(hint) = view.selection_hint {
            cell = cell.with_selection_hint(hint);
        }
        if let Some(input) = view.custom_match_input {
            cell = cell.with_custom_match_input(input);
        }
        if let Some(source) = view.custom_match_source {
            cell = cell.with_custom_match_source(source);
        }
        cell = cell.with_scope_context(
            view.workspace_untrusted,
            view.is_compound_command,
            view.has_dynamic_eval,
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

    pub fn handle_key(&mut self, key: KeyEvent) -> BottomPaneAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Ctrl+C state machine
        if key.code == KeyCode::Char('c') && ctrl {
            if let Some(view) = self.active_view_mut() {
                match view.on_ctrl_c() {
                    CancellationEvent::Consumed => {
                        self.view_stack.pop();
                        return BottomPaneAction::Consumed;
                    }
                    CancellationEvent::Escalate => {}
                }
            }
            if !self.composer.is_empty() {
                self.composer.clear_draft();
                self.sync_popups();
                return BottomPaneAction::Consumed;
            }
            if self.task_status.is_active() {
                return BottomPaneAction::Interrupt;
            }
            return BottomPaneAction::Quit;
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
                    return BottomPaneAction::ApprovalResolved { id };
                }
                return BottomPaneAction::Consumed;
            }
            if self.composer.is_empty() && self.view_stack.is_empty() {
                return BottomPaneAction::Quit;
            }
            return BottomPaneAction::Consumed;
        }

        // ── Approval keys ────────────────────────────────────────
        //
        // The focused approval cell captures:
        //   • ← / →          — move button focus
        //   • Enter          — activate the focused button
        //   • Ctrl+Enter     — quick-Accept regardless of focus
        //   • Esc            — default-Reject the focused approval
        //   • Tab            — cycle focus to next pending (empty composer only)
        //
        // We intentionally do NOT map bare letters or Ctrl+Y/N: the
        // Cursor-style button row already exposes every action, and
        // any letter shortcut risks consuming text the user is still
        // typing.
        if self.has_pending_approvals() {
            if self.approval_queue.focused_custom_prefix_active() {
                match key.code {
                    KeyCode::Enter => {
                        if let Some(response) =
                            self.approval_queue.submit_custom_prefix_for_focused()
                        {
                            if let Some(id) = self.respond_focused_approval(response) {
                                return BottomPaneAction::ApprovalResolved { id };
                            }
                        }
                        return BottomPaneAction::Consumed;
                    }
                    KeyCode::Esc => {
                        self.approval_queue.cancel_custom_prefix_for_focused();
                        return BottomPaneAction::Consumed;
                    }
                    KeyCode::Backspace => {
                        self.approval_queue.pop_custom_prefix_char();
                        return BottomPaneAction::Consumed;
                    }
                    KeyCode::Char(ch) if !ctrl => {
                        self.approval_queue.push_custom_prefix_char(ch);
                        return BottomPaneAction::Consumed;
                    }
                    _ => {}
                }
            }

            // Ctrl+Enter → quick accept regardless of button focus.
            if key.code == KeyCode::Enter && ctrl {
                if let Some(id) = self.respond_focused_approval(ApprovalResponse::AllowOnce) {
                    return BottomPaneAction::ApprovalResolved { id };
                }
                return BottomPaneAction::Consumed;
            }

            match key.code {
                // Horizontal navigation: ←/→ move the focused-button
                // cursor within the approval cell's button row.
                // Up/Down intentionally mirror left/right. Many users
                // reach for the arrow keys without reading the hint
                // and expected a vertical menu; rather than force
                // them to learn an arrow-axis mapping, we accept all
                // four arrows as equivalent. No ambiguity: the
                // composer never consumes arrow keys while an
                // approval is pending (early-return below), so
                // there's no cost to this looseness.
                KeyCode::Left | KeyCode::Up => {
                    self.move_approval_button_left();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::Right | KeyCode::Down => {
                    self.move_approval_button_right();
                    return BottomPaneAction::Consumed;
                }
                // Tab cycles between PENDING approvals (not buttons).
                // Previously gated on `composer.is_empty()` so stray
                // whitespace in the composer would hand Tab to
                // completion instead — an accidental footgun during
                // an approval flow. Relaxed to: route Tab to the
                // approval queue UNLESS an inline menu (slash or
                // mention) is actively capturing Tab for completion.
                // That menu exception is what the
                // `slash_menu_open_with_approval_pending_routes_tab_
                // to_slash_selection` integration test enforces.
                KeyCode::Tab if self.slash_menu.is_none() && self.mention_menu.is_none() => {
                    self.move_approval_focus_down();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::BackTab if self.slash_menu.is_none() && self.mention_menu.is_none() => {
                    self.move_approval_focus_up();
                    return BottomPaneAction::Consumed;
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
                    if let Some(act) = self.activate_focused_approval_button() {
                        return match act {
                            ApprovalActivation::Single { id, .. } => {
                                BottomPaneAction::ApprovalResolved { id }
                            }
                            ApprovalActivation::Batch { .. } => {
                                BottomPaneAction::ApprovalResolved { id: 0 }
                            }
                        };
                    }
                }
                KeyCode::Esc
                    if self.slash_menu.is_none()
                        && self.mention_menu.is_none()
                        && self.skill_popup.is_none()
                        && self.view_stack.is_empty() =>
                {
                    if let Some(id) = self.reject_focused_approval() {
                        return BottomPaneAction::ApprovalResolved { id };
                    }
                }
                _ => {}
            }
        }

        // Route to active view first (view handles its own Esc)
        if let Some(view) = self.active_view_mut() {
            view.handle_key(key);
            if view.is_complete() {
                let completion = view.completion();
                self.view_stack.pop();
                if let Some(vc) = completion {
                    return BottomPaneAction::ViewCompleted {
                        result: vc.result,
                        reopen: vc.reopen,
                    };
                }
                return BottomPaneAction::ViewCompleted {
                    result: None,
                    reopen: None,
                };
            }
            return BottomPaneAction::Consumed;
        }

        // Esc: dismiss popup
        if key.code == KeyCode::Esc {
            if self.slash_menu.is_some() {
                self.slash_menu = None;
                return BottomPaneAction::Consumed;
            }
            if self.mention_menu.is_some() {
                self.close_mention();
                return BottomPaneAction::Consumed;
            }
            if self.skill_popup.is_some() {
                self.skill_popup = None;
                return BottomPaneAction::Consumed;
            }
        }

        // Popup key handling: Up/Down/Tab/Enter when slash menu is visible.
        //
        // Enter with no matches falls through so the composer handles it
        // (submits the raw draft), avoiding a silent no-op.
        if self.slash_menu.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.slash_menu.as_mut().unwrap().move_up();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::Down => {
                    self.slash_menu.as_mut().unwrap().move_down();
                    return BottomPaneAction::Consumed;
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
                    return BottomPaneAction::Consumed;
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
                        return BottomPaneAction::SubmitInput(picked);
                    }
                    // Empty matches: fall through to composer so the raw
                    // draft gets submitted as-is.
                }
                _ => {}
            }
        }

        if self.skill_popup.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.skill_popup.as_mut().unwrap().move_up();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::Down => {
                    self.skill_popup.as_mut().unwrap().move_down();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::Tab | KeyCode::Enter => {
                    if let Some(name) = self.skill_popup.as_ref().and_then(|p| p.selected_name()) {
                        self.composer.set_text(&format!("${name} "));
                        self.skill_popup = None;
                    }
                    return BottomPaneAction::Consumed;
                }
                _ => {}
            }
        }

        // Mention menu: Up/Down/Tab; Enter falls through to composer so
        // the user can submit the whole line with an inline mention.
        if self.mention_menu.is_some() {
            match key.code {
                KeyCode::Up => {
                    self.mention_menu.as_mut().unwrap().move_up();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::Down => {
                    self.mention_menu.as_mut().unwrap().move_down();
                    return BottomPaneAction::Consumed;
                }
                KeyCode::Tab => {
                    if self.accept_mention() {
                        // After accept, re-sync in case the new draft
                        // (e.g. a directory `/`) keeps a menu open.
                        self.sync_popups();
                    }
                    return BottomPaneAction::Consumed;
                }
                _ => {}
            }
        }

        // Route to composer
        let action = match self.composer.handle_key(key) {
            ComposerAction::Submit => {
                let text = self.composer.clear_and_submit();
                self.slash_menu = None;
                self.close_mention();
                BottomPaneAction::SubmitInput(text)
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
        // Flush paste burst buffer when idle timeout expires.
        self.composer.flush_paste_burst();
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
        use ratatui::widgets::{Paragraph, Widget, Wrap};
        let lines = cell.display_lines(area.width);
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
