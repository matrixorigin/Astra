pub(crate) mod agent_guide_view;
pub(crate) mod agent_transcript_view;
pub(crate) mod ask_user_view;
pub(crate) mod background_task_view;
pub(crate) mod busy_view;
pub(crate) mod chat_composer;
pub(crate) mod config_edit_view;
pub(crate) mod context_panel_view;
pub(crate) mod footer;
pub(crate) mod help_view;
pub(crate) mod in_flight_agents_view;
pub(crate) mod info_view;
pub(crate) mod list_selection_view;
pub(crate) mod login_view;
pub(crate) mod paste_burst;
pub(crate) mod plan_review_view;
pub(crate) mod root_transcript_view;
pub(crate) mod session_picker_view;
pub(crate) mod skill_popup;
pub(crate) mod task_board_view;
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
mod queue_preview_tests;
#[cfg(test)]
mod slash_integration_tests;

use chat_composer::{ChatComposer, ComposerAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use footer::Footer;
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Widget,
};
use skill_popup::SkillPopup;
use view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ConversationTabId,
    ViewActionDisposition,
};

use super::approval::{ApprovalQueue, ApprovalView, ButtonAction};
use super::mention_menu::{
    FileProvider, MentionMenu, extract_mention_at, popup as mention_popup_render,
};
use super::slash_menu::{SlashItem, SlashMenu, is_open_for, popup as slash_popup_render};
use super::task_status::TaskStatus;
use crate::cli::chat_stream::ApprovalResponse;
use ask_user_view::AskUserView;
use std::sync::Arc;
use tokio::sync::oneshot;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(crate) struct BottomPane {
    pub composer: ChatComposer,
    pub footer: Footer,
    view_stack: Vec<Box<dyn BottomPaneView>>,
    /// Stable browser-like order for retained conversation workspaces.
    ///
    /// `view_stack` is a focus stack: activating a tab intentionally moves it
    /// to the top, so deriving next/previous from that stack would make a
    /// three-tab cycle bounce between the two most recently focused tabs.
    /// Keep ordering separately, but retain the actual views in the stack so
    /// each transcript keeps its cursor, search, expansion, and live suffix.
    conversation_tab_order: Vec<ConversationTabId>,
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
    pending_user_intents: std::collections::VecDeque<PendingUserIntent>,
    /// User messages accepted after visible output ended but before the
    /// current turn has committed its canonical boundary. These are next-turn
    /// submissions, not guidance for the previous run.
    queued_next_turn_submissions: std::collections::VecDeque<String>,
    applied_user_intent_ids: std::collections::HashSet<String>,
    /// Typed actions emitted while a retained projection refreshes. These are
    /// not user-input events: for example, a live agent transcript upgrades
    /// itself once a later receipt supplies its durable history location.
    projection_actions: std::collections::VecDeque<BottomPaneViewAction>,
    /// True when the user pressed Esc/Ctrl+C to interrupt the current
    /// run and the cancel RPC is in flight. The queue panel reflects
    /// this intermediate state so the user isn't stuck wondering why
    /// "Esc sends now" isn't taking effect immediately.
    pub(crate) interrupt_pending: bool,
    /// Explicit permission selection made while a turn owns `SessionState`.
    /// It is a UI intent, not the active policy: the event loop applies it
    /// only after the current turn settles, so current tools and the footer
    /// cannot disagree about which policy actually governed execution.
    staged_permission_mode: Option<crate::cli::permission_manager::PermissionMode>,
}

/// Presentation-only entry in the conversation workspace tab strip. The
/// authoritative identity remains internal to [`BottomPane`]; callers receive
/// no routing data, so display text cannot become a control protocol.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ConversationTab {
    pub(crate) label: String,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingUserIntent {
    pub(crate) intent_id: String,
    pub(crate) delivery: astra_turn_types::UserIntentDelivery,
    pub(crate) status: astra_turn_types::UserIntentStatus,
    pub(crate) text: String,
    pub(crate) target: PendingUserIntentTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingUserIntentTarget {
    ActiveRun,
    AgentRun { run_id: String, agent_name: String },
}

fn pending_user_intent_title(intent: &PendingUserIntent, task_status: &TaskStatus) -> String {
    match (&intent.target, intent.status) {
        (
            PendingUserIntentTarget::AgentRun { agent_name, .. },
            astra_turn_types::UserIntentStatus::AcceptedLocal,
        ) => format!("Sending guidance to {agent_name}"),
        (
            PendingUserIntentTarget::AgentRun { agent_name, .. },
            astra_turn_types::UserIntentStatus::AcceptedRemote,
        ) => format!("Guidance delivered to {agent_name} · awaiting application"),
        (
            PendingUserIntentTarget::AgentRun { agent_name, .. },
            astra_turn_types::UserIntentStatus::Applied,
        ) => format!("Guidance applied by {agent_name}"),
        (
            PendingUserIntentTarget::ActiveRun,
            astra_turn_types::UserIntentStatus::AcceptedLocal
            | astra_turn_types::UserIntentStatus::AcceptedRemote,
        ) => match task_status {
            TaskStatus::ToolExecuting { .. } => {
                "Queued for current run · applies after current tool".to_string()
            }
            TaskStatus::WaitingApproval { .. } => {
                "Queued for current run · applies after approval".to_string()
            }
            TaskStatus::WaitingModel => {
                "Queued for current run · applies when model resumes".to_string()
            }
            TaskStatus::Idle | TaskStatus::Dispatching | TaskStatus::TurnRunning { .. } => {
                "Queued for current run · applies at next model boundary".to_string()
            }
        },
        (PendingUserIntentTarget::ActiveRun, astra_turn_types::UserIntentStatus::Applied) => {
            "Guidance applied to current run".to_string()
        }
    }
}

impl BottomPane {
    pub fn new() -> Self {
        Self {
            composer: ChatComposer::new(),
            footer: Footer::new(),
            view_stack: Vec::new(),
            conversation_tab_order: Vec::new(),
            task_status: TaskStatus::Idle,
            slash_menu: None,
            slash_items: Vec::new(),
            skill_popup: None,
            skill_items: Vec::new(),
            mention_menu: None,
            mention_range: None,
            file_provider: None,
            approval_queue: ApprovalQueue::new(),
            pending_user_intents: std::collections::VecDeque::new(),
            queued_next_turn_submissions: std::collections::VecDeque::new(),
            applied_user_intent_ids: std::collections::HashSet::new(),
            projection_actions: std::collections::VecDeque::new(),
            interrupt_pending: false,
            staged_permission_mode: None,
        }
    }

    pub fn set_skill_items(&mut self, items: Vec<skill_popup::SkillItem>) {
        self.skill_items = items;
    }

    /// Inject the slash-command catalog used by the inline menu.
    pub fn set_slash_items(&mut self, items: Vec<SlashItem>) {
        self.slash_items = items;
    }

    pub(crate) fn set_file_writer(&mut self, writer: crate::tui::file_writer::TuiFileWriter) {
        self.composer.set_file_writer(writer);
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
            .map(|i| i.name.as_ref())
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
        if self
            .active_view_mut()
            .is_some_and(|view| view.handle_paste(text))
        {
            return;
        }
        self.composer.handle_paste(text);
        self.sync_popups();
    }

    /// Project an intent that was accepted by the active run's control plane.
    pub fn accept_user_intent(
        &mut self,
        intent_id: impl Into<String>,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        text: impl Into<String>,
    ) -> bool {
        self.accept_user_intent_for_target(
            intent_id.into(),
            delivery,
            status,
            text.into(),
            PendingUserIntentTarget::ActiveRun,
        )
    }

    pub fn accept_agent_guide(
        &mut self,
        intent_id: String,
        run_id: String,
        agent_name: String,
        text: String,
    ) -> bool {
        self.accept_user_intent_for_target(
            intent_id,
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::AcceptedLocal,
            text,
            PendingUserIntentTarget::AgentRun { run_id, agent_name },
        )
    }

    fn accept_user_intent_for_target(
        &mut self,
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        text: String,
        target: PendingUserIntentTarget,
    ) -> bool {
        if intent_id.trim().is_empty()
            || text.trim().is_empty()
            || !matches!(
                status,
                astra_turn_types::UserIntentStatus::AcceptedLocal
                    | astra_turn_types::UserIntentStatus::AcceptedRemote
            )
            || self.applied_user_intent_ids.contains(&intent_id)
            || self
                .pending_user_intents
                .iter()
                .any(|pending| pending.intent_id == intent_id)
        {
            return false;
        }
        self.pending_user_intents.push_back(PendingUserIntent {
            intent_id,
            delivery,
            status,
            text,
            target,
        });
        true
    }

    pub fn promote_agent_guide_accepted(&mut self, intent_id: &str) -> bool {
        let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::AgentRun { .. })
        }) else {
            return false;
        };
        intent.status = astra_turn_types::UserIntentStatus::AcceptedRemote;
        true
    }

    pub fn remove_agent_guide(&mut self, intent_id: &str) -> Option<PendingUserIntent> {
        let index = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::AgentRun { .. })
        })?;
        self.pending_user_intents.remove(index)
    }

    /// Resolve an applied runtime intent by stable identity. Unknown IDs can
    /// come from another client attached to the same run, so their runtime
    /// content is still projected into history. Replayed applied events are
    /// idempotent and return `None`.
    pub fn apply_user_intent(
        &mut self,
        intent_id: &str,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        runtime_content: &str,
    ) -> Option<PendingUserIntent> {
        if intent_id.trim().is_empty()
            || status != astra_turn_types::UserIntentStatus::Applied
            || self.applied_user_intent_ids.contains(intent_id)
        {
            return None;
        }
        let runtime_content = runtime_content.trim();
        if runtime_content.is_empty() {
            return None;
        }
        let target = if let Some(index) = self
            .pending_user_intents
            .iter()
            .position(|pending| pending.intent_id == intent_id)
        {
            self.pending_user_intents
                .remove(index)
                .map(|pending| pending.target)
                .unwrap_or(PendingUserIntentTarget::ActiveRun)
        } else {
            PendingUserIntentTarget::ActiveRun
        };
        self.applied_user_intent_ids.insert(intent_id.to_string());
        Some(PendingUserIntent {
            intent_id: intent_id.to_string(),
            delivery,
            status,
            text: runtime_content.to_string(),
            target,
        })
    }

    /// Restore queued-but-unapplied input into the composer at run end,
    /// preserving any draft the user is currently editing by appending
    /// beneath it. Never silently drop user input.
    pub fn restore_into_composer(&mut self, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        if self.composer.is_empty() {
            self.composer.set_text(text);
        } else {
            let existing = self.composer.text();
            self.composer
                .set_text(&format!("{}\n\n{}", existing.trim_end(), text));
        }
    }

    pub fn take_unapplied_user_intents(&mut self) -> Vec<PendingUserIntent> {
        self.applied_user_intent_ids.clear();
        let mut active_run = Vec::new();
        let mut retained = std::collections::VecDeque::new();
        while let Some(intent) = self.pending_user_intents.pop_front() {
            if intent.target == PendingUserIntentTarget::ActiveRun {
                active_run.push(intent);
            } else {
                retained.push_back(intent);
            }
        }
        self.pending_user_intents = retained;
        active_run
    }

    /// Accept a message for the next turn once the current answer is visible.
    /// The event loop transfers this FIFO lane to its ordinary submit path as
    /// soon as the current canonical turn boundary is committed.
    pub fn queue_next_turn_submission(&mut self, text: String) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        self.queued_next_turn_submissions.push_back(text);
        true
    }

    pub fn take_queued_next_turn_submissions(&mut self) -> std::collections::VecDeque<String> {
        std::mem::take(&mut self.queued_next_turn_submissions)
    }

    fn has_pending_user_intents(&self) -> bool {
        !self.pending_user_intents.is_empty()
    }

    fn has_pending_composer_queue(&self) -> bool {
        self.has_pending_user_intents() || !self.queued_next_turn_submissions.is_empty()
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
        if let Some(tab_id) = view.conversation_tab_id() {
            // A re-created tab supersedes a closed instance with the same
            // identity. Existing active tabs are reactivated, never pushed,
            // so this does not reorder a live browser tab on focus changes.
            self.conversation_tab_order.retain(|tab| tab != &tab_id);
            self.conversation_tab_order.push(tab_id);
        }
        self.view_stack.push(view);
    }

    pub fn enqueue_ask_user(
        &mut self,
        prompt: crate::cli::chat_stream::AskUserPrompt,
        response_tx: oneshot::Sender<crate::cli::chat_stream::AskUserResponse>,
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
        response_tx: oneshot::Sender<crate::cli::chat_stream::PlanReviewDecision>,
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

    pub fn refresh_background_task_rows(
        &mut self,
        rows: Vec<background_task_view::BackgroundTaskRow>,
    ) -> bool {
        self.active_view_mut()
            .is_some_and(|view| view.refresh_background_task_rows(rows))
    }

    pub fn refresh_background_task_rows_selecting(
        &mut self,
        rows: Vec<background_task_view::BackgroundTaskRow>,
        selected_id: Option<&str>,
    ) -> bool {
        self.active_view_mut()
            .is_some_and(|view| view.refresh_background_task_rows_selecting(rows, selected_id))
    }

    pub fn accepts_background_task_rows(&self) -> bool {
        self.view_stack
            .last()
            .is_some_and(|view| view.accepts_background_task_rows())
    }

    pub fn refresh_agent_monitor(
        &mut self,
        snapshot: crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_agent_monitor(snapshot.clone());
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
        }
        refreshed
    }

    pub(crate) fn has_pending_agent_transcript_identity(&self) -> bool {
        self.view_stack
            .iter()
            .any(|view| view.has_pending_agent_transcript_identity())
    }

    /// Take an action emitted by a projection refresh rather than a keypress.
    /// The event loop dispatches it through the exact same typed effect path
    /// as a user-triggered view action.
    pub(crate) fn take_projection_action(&mut self) -> Option<BottomPaneViewAction> {
        self.projection_actions.pop_front()
    }

    pub(crate) fn refresh_task_board(
        &mut self,
        projection: &crate::tui::task_board_observer::TaskBoardProjection,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_task_board(projection);
        }
        refreshed
    }

    pub(crate) fn refresh_agent_transcript(
        &mut self,
        update: agent_transcript_view::AgentTranscriptUpdate,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_agent_transcript(update.clone());
        }
        refreshed
    }

    pub(crate) fn refresh_root_transcript(
        &mut self,
        update: root_transcript_view::RootTranscriptUpdate,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_root_transcript(update.clone());
        }
        refreshed
    }

    /// Keep a durable root transcript workspace live while its next canonical
    /// page is still being written. The view itself labels this as local
    /// evidence and never merges it into durable history by presentation text.
    pub(crate) fn refresh_root_transcript_live(
        &mut self,
        item: Option<transcript_view::TranscriptItem>,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_root_transcript_live(item.clone());
        }
        refreshed
    }

    pub(crate) fn refresh_root_transcript_context(
        &mut self,
        items: Vec<transcript_view::TranscriptItem>,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_root_transcript_context(items.clone());
        }
        refreshed
    }

    /// Notify retained root transcript tabs after canonical transcript items
    /// have reached the local journal. Projection-owned reload actions flow
    /// through the usual event-loop effect runner.
    pub(crate) fn refresh_root_transcript_committed(&mut self, session_id: &str) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_root_transcript_committed(session_id);
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
        }
        refreshed
    }

    pub(crate) fn refresh_agent_live_event(
        &mut self,
        event: &astra_turn_core::agent_live_event::AgentLiveEvent,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_agent_live_event(event);
        }
        refreshed
    }

    pub(crate) fn refresh_agent_live_gap(
        &mut self,
        gap: &astra_turn_core::agent_live_event::AgentLiveGap,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_agent_live_gap(gap);
        }
        refreshed
    }

    /// Bind the currently inspected live-only agent conversation when the
    /// parent session is first created. The returned action remains typed and
    /// uses the same dispatch path as an explicit refresh keypress.
    pub(crate) fn bind_open_agent_transcript_session(
        &mut self,
        session_id: &str,
    ) -> Option<BottomPaneViewAction> {
        let view = self.active_view_mut()?;
        if !view.bind_unbound_agent_transcript_session(session_id) {
            return None;
        }
        view.take_action_request().map(|request| request.action)
    }

    pub fn agent_monitor_is_open(&self) -> bool {
        self.view_stack
            .last()
            .is_some_and(|view| view.accepts_agent_rows())
    }

    /// A conversation tab can cover the navigator, but the retained tree must
    /// continue receiving truth updates so returning to it never displays an
    /// old run state.
    pub(crate) fn has_agent_monitor(&self) -> bool {
        self.view_stack.iter().any(|view| view.accepts_agent_rows())
    }

    #[allow(dead_code)]
    pub fn pop_view(&mut self) -> Option<Box<dyn BottomPaneView>> {
        self.pop_active_view()
    }

    pub(crate) fn dismiss_active_agent_monitor(&mut self) -> bool {
        if self
            .active_view()
            .is_some_and(BottomPaneView::accepts_agent_rows)
        {
            self.pop_active_view();
            true
        } else {
            false
        }
    }

    pub fn has_active_view(&self) -> bool {
        !self.view_stack.is_empty()
    }

    pub fn transcript_view_is_open(&self) -> bool {
        self.active_view()
            .is_some_and(|view| view.is_root_transcript_view())
    }

    /// Whether the root conversation is open anywhere in the conversation
    /// stack. It may be covered by an agent tab, but it must keep receiving
    /// the same live projection as the visible root conversation.
    pub(crate) fn has_root_transcript_tab(&self) -> bool {
        self.view_stack
            .iter()
            .any(|view| view.is_root_transcript_view())
    }

    /// Conversation tabs are navigable surfaces, not focus-stealing modals.
    /// Global workbench navigation may open over them and restore the exact
    /// tab afterward; approval/forms and other modal views still retain focus.
    pub(crate) fn conversation_tab_is_open(&self) -> bool {
        self.active_view()
            .is_some_and(|view| view.conversation_tab_id().is_some())
    }

    /// A primary workspace replaces the compact chat canvas. Conversation
    /// tabs and the task board use this same layout contract, while forms and
    /// pickers remain bounded overlays.
    pub(crate) fn primary_workspace_is_open(&self) -> bool {
        self.active_view()
            .is_some_and(|view| view.owns_primary_canvas())
    }

    /// A focused root or delegated transcript owns the primary terminal
    /// canvas. This is a conversation switch, not a taller detail pane.
    pub(crate) fn prepare_conversation_workspace(&mut self, terminal_height: u16, width: u16) {
        if let Some(view) = self.active_view_mut()
            && view.conversation_tab_id().is_some()
        {
            view.fit_conversation_workspace(terminal_height, width);
        }
    }

    #[cfg(test)]
    pub(crate) fn active_conversation_tab_id(&self) -> Option<ConversationTabId> {
        self.active_view()
            .and_then(BottomPaneView::conversation_tab_id)
    }

    /// Bring an already-open transcript scope to the front without losing
    /// its cursor, expanded thinking/tool cells, search state, or live suffix.
    /// This is the tab-switch operation for root and delegated conversations.
    fn activate_conversation_tab(&mut self, tab_id: &ConversationTabId) -> bool {
        let Some(index) = self
            .view_stack
            .iter()
            .rposition(|view| view.conversation_tab_id().as_ref() == Some(tab_id))
        else {
            return false;
        };
        let view = self.view_stack.remove(index);
        self.view_stack.push(view);
        true
    }

    /// Switch among retained root/agent conversations in their creation
    /// order. This is a workspace operation, not a reconstruction from the
    /// agent list, so all local transcript state stays intact.
    pub(crate) fn cycle_conversation_tab(&mut self, reverse: bool) -> bool {
        let Some(active_tab) = self
            .active_view()
            .and_then(BottomPaneView::conversation_tab_id)
        else {
            return false;
        };

        // Views can disappear through a completed overlay before their tab is
        // next selected. Prune from authoritative live views here rather than
        // carrying a second lifecycle state for an otherwise local UI detail.
        let open_tabs = self
            .view_stack
            .iter()
            .filter_map(|view| view.conversation_tab_id())
            .collect::<Vec<_>>();
        self.conversation_tab_order
            .retain(|tab| open_tabs.contains(tab));

        let tab_count = self.conversation_tab_order.len();
        if tab_count < 2 {
            return false;
        }
        let Some(active_index) = self
            .conversation_tab_order
            .iter()
            .position(|tab| tab == &active_tab)
        else {
            return false;
        };
        let target_index = if reverse {
            active_index.checked_sub(1).unwrap_or(tab_count - 1)
        } else {
            (active_index + 1) % tab_count
        };
        let target = self.conversation_tab_order[target_index].clone();
        self.activate_conversation_tab(&target)
    }

    /// Ordered, currently-open conversation tabs for the primary-workspace
    /// chrome. Views retain their own local state; this derives a small
    /// presentation projection and never becomes another transcript owner.
    pub(crate) fn conversation_tabs(&self) -> Vec<ConversationTab> {
        let active_tab = self
            .active_view()
            .and_then(BottomPaneView::conversation_tab_id);
        self.conversation_tab_order
            .iter()
            .filter_map(|tab_id| {
                self.view_stack
                    .iter()
                    .find(|view| view.conversation_tab_id().as_ref() == Some(tab_id))
                    .and_then(|view| {
                        view.conversation_tab_label().map(|label| ConversationTab {
                            label,
                            active: active_tab.as_ref() == Some(tab_id),
                        })
                    })
            })
            .collect()
    }

    pub(crate) fn activate_root_transcript(&mut self) -> bool {
        self.activate_conversation_tab(&ConversationTabId::Root)
    }

    /// Focus the durable root conversation for `session_id`.
    ///
    /// Before the first session is created, Ctrl+O can truthfully show only
    /// the in-memory transcript. That local browser has the same *tab*
    /// identity as the durable root conversation, but it is not an
    /// interchangeable data source. Once a session exists, replace the local
    /// browser in place and let the caller start a canonical page load. This
    /// keeps one root tab while preventing an early Ctrl+O from permanently
    /// hiding committed history behind a live-only view.
    ///
    /// Returns `true` exactly when the caller must load the initial durable
    /// page. Re-activating the same session preserves its cursor, detail
    /// state, pagination, and already-confirmed history.
    pub(crate) fn ensure_durable_root_transcript(
        &mut self,
        session_id: String,
        width: u16,
        terminal_height: u16,
    ) -> bool {
        let root_tab = ConversationTabId::Root;
        if let Some(index) = self
            .view_stack
            .iter()
            .rposition(|view| view.conversation_tab_id().as_ref() == Some(&root_tab))
        {
            if self.view_stack[index]
                .durable_root_transcript_session()
                .is_some_and(|bound_session| bound_session == session_id)
            {
                let view = self.view_stack.remove(index);
                self.view_stack.push(view);
                return false;
            }

            // Keep the existing Root entry in browser-tab order. Replacing the
            // implementation is a source-of-truth upgrade, not a newly-opened
            // conversation.
            self.view_stack.remove(index);
            self.view_stack
                .push(Box::new(root_transcript_view::RootTranscriptView::loading(
                    session_id,
                    width,
                    terminal_height,
                )));
            return true;
        }

        self.push_view(Box::new(root_transcript_view::RootTranscriptView::loading(
            session_id,
            width,
            terminal_height,
        )));
        true
    }

    /// Upgrade the visible-or-retained pre-session root browser when a
    /// session is first bound. Unlike [`Self::ensure_durable_root_transcript`],
    /// this must never open a workspace on its own: receiving a session id is
    /// not a request to steal the user's current focus.
    pub(crate) fn promote_open_root_transcript_to_durable(
        &mut self,
        session_id: String,
        width: u16,
        terminal_height: u16,
    ) -> bool {
        self.has_root_transcript_tab()
            && self.ensure_durable_root_transcript(session_id, width, terminal_height)
    }

    pub(crate) fn activate_agent_transcript(&mut self, agent_id: &str, run_id: &str) -> bool {
        self.activate_conversation_tab(&ConversationTabId::Run {
            agent_id: agent_id.to_string(),
            run_id: run_id.to_string(),
        })
    }

    /// Bring the existing run navigator back to the foreground so Ctrl+G
    /// always returns to the same conversation tree rather than creating a
    /// stack of duplicate navigator panes.
    pub(crate) fn activate_agent_monitor(&mut self) -> bool {
        let Some(index) = self
            .view_stack
            .iter()
            .rposition(|view| view.accepts_agent_rows())
        else {
            return false;
        };
        let view = self.view_stack.remove(index);
        self.view_stack.push(view);
        true
    }

    pub fn refresh_transcript_snapshot(
        &mut self,
        snapshot: transcript_view::TranscriptSnapshot,
        width: u16,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            if view.is_root_transcript_view() {
                refreshed |= view.refresh_transcript_snapshot(snapshot.clone(), width);
            }
        }
        refreshed
    }

    /// True only for the local fallback transcript. The durable root
    /// conversation has an independent canonical paging lane, so it must not
    /// cause a full in-memory history snapshot to be built for every token.
    pub(crate) fn uses_local_root_transcript_snapshot(&self) -> bool {
        self.view_stack.iter().any(|view| {
            view.is_root_transcript_view() && view.uses_local_root_transcript_snapshot()
        })
    }

    pub fn close_active_view(&mut self) -> bool {
        self.pop_active_view().is_some()
    }

    fn pop_active_view(&mut self) -> Option<Box<dyn BottomPaneView>> {
        let view = self.view_stack.pop()?;
        if let Some(tab_id) = view.conversation_tab_id()
            && !self
                .view_stack
                .iter()
                .any(|open| open.conversation_tab_id().as_ref() == Some(&tab_id))
        {
            self.conversation_tab_order.retain(|tab| tab != &tab_id);
        }
        Some(view)
    }

    fn active_view(&self) -> Option<&dyn BottomPaneView> {
        self.view_stack.last().map(|v| &**v)
    }

    fn active_view_mut(&mut self) -> Option<&mut Box<dyn BottomPaneView>> {
        self.view_stack.last_mut()
    }

    fn popup_height(&self) -> u16 {
        if let Some(m) = &self.slash_menu {
            return slash_popup_render::desired_composer_height(m);
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

    pub(crate) fn stage_permission_mode_for_next_turn(
        &mut self,
        mode: crate::cli::permission_manager::PermissionMode,
    ) {
        self.staged_permission_mode = Some(mode);
    }

    pub(crate) fn take_staged_permission_mode(
        &mut self,
    ) -> Option<crate::cli::permission_manager::PermissionMode> {
        self.staged_permission_mode.take()
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

    /// Re-evaluate every pending approval against `new_mode` and resolve the
    /// ones the new mode can decide without asking. Used when the user pivots
    /// permission modes (Shift+Tab, `/allow`, or the `exit_plan_mode`
    /// overlay) so the approval queue does not lag behind the chip.
    ///
    /// Returns the number of entries resolved. Footer counter is refreshed so
    /// the chip reads the new pending count.
    pub fn reevaluate_approvals_for_mode(
        &mut self,
        new_mode: crate::cli::permission_manager::PermissionMode,
    ) -> usize {
        use crate::cli::chat_stream::ApprovalResponse;
        use astra_turn_core::permission::engine::{HardDecision, evaluate_permission};
        use astra_turn_core::permission::types::{InheritedPermissions, PermissionSyncContext};

        let ctx = PermissionSyncContext::new(InheritedPermissions::new(new_mode));
        let released = self.approval_queue.drain_resolved(|entry| {
            // Legacy cloud / sandbox-expand entries can arrive
            // without args (Value::Null). We can't safely
            // re-evaluate those, so leave them in the queue and let
            // the original gate resolve them explicitly.
            if entry.args.is_null() {
                return None;
            }
            let envelope = evaluate_permission(&entry.tool, &entry.args, &ctx);
            match envelope.decision {
                HardDecision::Allow => Some(ApprovalResponse::AllowOnce),
                HardDecision::Deny { .. } => Some(ApprovalResponse::Deny),
                HardDecision::NeedExternal { .. } => None,
            }
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

    /// Explicitly reject the focused approval.
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

    /// Build a live `ApprovalCell` from the queue-selected entry. The card is
    /// visually focused only while the composer is empty; a draft keeps the
    /// approval observable without advertising that its action row owns keys.
    /// `None` when nothing is pending.
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
            self.composer.is_empty(),
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
            // A modal owns input focus, but pending approvals still need a
            // visible, non-interactive signal so they do not disappear from
            // the user's mental model while the card itself is hidden.
            if self.has_pending_approvals() {
                h = h.saturating_add(1);
            }
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
        let approval_h = self.focused_approval_height(width);
        let queue_h = self.user_intent_height();
        let popup_h = self.popup_height();
        content_h + approval_h + queue_h + popup_h + 1
    }

    /// Top-level key routing. Dispatches to named phase handlers so
    /// each concern reads as a single paragraph:
    ///
    /// 1. Ctrl+C / Ctrl+D (quit / interrupt / clear-draft)
    /// 2. Active overlay view (push_view'd full-screen widgets)
    /// 3. Visible approval queue (← → Enter Tab, Ctrl+D reject)
    /// 4. Popup dismissal (Esc)
    /// 5. Popup navigation (slash / skill / mention menus)
    /// 6. Composer (text input)
    pub fn handle_key(&mut self, key: KeyEvent) -> BottomPaneAction {
        if let Some(a) = self.handle_ctrl_keys(key) {
            return a;
        }
        if let Some(a) = self.handle_active_view_key(key) {
            return a;
        }
        if let Some(a) = self.handle_approval_keys(key) {
            return a;
        }
        if let Some(a) = self.handle_popup_dismiss(key) {
            return a;
        }
        // With no modal or popup owning Esc, a draft is the visible thing to
        // dismiss. Do this after popup dismissal so the first Esc closes the
        // popup and a later Esc clears the underlying composer text.
        if key.code == KeyCode::Esc && !self.composer.is_empty() {
            self.composer.clear_draft();
            self.sync_popups();
            return BottomPaneAction::Consumed;
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
            return BottomPaneAction::OpenPermissionModePicker;
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
                        // `Consumed` only means the focused object handled
                        // Ctrl+C. Closing is an independent state transition:
                        // a transcript search, for example, consumes Ctrl+C
                        // to cancel its query and must retain the same
                        // conversation canvas. Views that intentionally close
                        // already report `is_complete()` after handling it.
                        let completed = view.is_complete();
                        if completed {
                            self.pop_active_view();
                        }
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
        // 1. Active view → let that visible surface handle Ctrl+D.
        // 2. Pending approval + composer empty → Reject focused.
        //    The composer-empty guard means user typing "do this"
        //    pressing the wrong key never accidentally rejects;
        //    Ctrl+D is intentional.
        // 3. Otherwise composer empty (no approval) → quit.
        // 4. Composer not empty → consumed (no-op).
        if key.code == KeyCode::Char('d') && ctrl {
            // A visible modal owns every key other than the explicit global
            // Ctrl+C state machine above. Give Ctrl+D to the view instead of
            // applying an invisible approval/quit action underneath it.
            if !self.view_stack.is_empty() {
                return None;
            }
            if self.has_pending_approvals() && self.composer.is_empty() {
                if let Some(id) = self.reject_focused_approval() {
                    return Some(BottomPaneAction::ApprovalResolved { id });
                }
                return Some(BottomPaneAction::Consumed);
            }
            if self.composer.is_empty() {
                return Some(BottomPaneAction::Quit);
            }
            return Some(BottomPaneAction::Consumed);
        }
        None
    }

    /// When an approval card is visible, capture the narrow set of keys
    /// that drive it (← → Enter Tab, with Esc consumed). Intentionally does NOT map
    /// bare letters or Ctrl+Y/N — the Cursor-style button row already
    /// exposes every action, and a letter shortcut risks consuming
    /// text the user is still typing.
    fn handle_approval_keys(&mut self, key: KeyEvent) -> Option<BottomPaneAction> {
        if !self.has_pending_approvals()
            || !self.view_stack.is_empty()
            || self.slash_menu.is_some()
            || self.mention_menu.is_some()
            || self.skill_popup.is_some()
        {
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
        // A non-empty composer is the focused editing surface. Keep the
        // approval visible, but leave all ordinary navigation/submission
        // keys to normal composer routing.
        if !self.composer.is_empty() {
            return None;
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
            // Reject is intentionally explicit: choose the No button or use
            // Ctrl+D. Esc must never turn a dismiss gesture into a denial.
            KeyCode::Esc if self.composer.is_empty() => Some(BottomPaneAction::Consumed),
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
        if let Some(request) = view.take_action_request() {
            if request.disposition == ViewActionDisposition::Close {
                self.pop_active_view();
            }
            return Some(BottomPaneAction::ViewAction(request.action));
        }
        if view.is_complete() {
            let completion = view.completion();
            self.pop_active_view();
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
                if self.task_status.is_active() {
                    BottomPaneAction::ExternalEditorUnavailable
                } else {
                    BottomPaneAction::OpenExternalEditor(self.composer.text())
                }
            }
            ComposerAction::Interrupt => BottomPaneAction::Interrupt,
            ComposerAction::Quit => BottomPaneAction::Quit,
            ComposerAction::Consumed => BottomPaneAction::Consumed,
            ComposerAction::Unhandled => BottomPaneAction::Escalate(key),
        };
        self.sync_popups();
        action
    }

    pub fn pre_draw_tick(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;
        if let Some(view) = self.active_view_mut() {
            view.pre_draw_tick(now);
        }
        let dismiss_completed_view = self
            .active_view()
            .is_some_and(|view| view.is_complete() && view.completion().is_none());
        if dismiss_completed_view {
            self.pop_active_view();
            changed = true;
        }
        if self.task_status.is_active() {
            // The live status indicator already owns the "Thinking /
            // Running / Waiting" narrative above the composer. Duplicating
            // that same label + timer in the footer makes the bottom stack
            // feel cramped, so the footer stays focused on mode + context
            // while a turn is active.
            self.footer.current_objective = None;
            self.footer.turn_elapsed = None;
        } else {
            self.footer.current_objective = self.task_status.objective_label();
            self.footer.turn_elapsed = self.task_status.elapsed();
        }
        // Flush paste burst buffer when idle timeout expires.
        if self.composer.flush_paste_burst() {
            self.sync_popups();
            changed = true;
        }
        if self
            .mention_menu
            .as_mut()
            .is_some_and(MentionMenu::refresh_if_provider_changed)
        {
            changed = true;
        }
        changed
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

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        if let Some(view) = self.active_view() {
            // Stack optional footers under the view area:
            //   [view]
            //   [hint bar]        — when hint_keys() is Some
            //   [status line]     — when reserve_status_footer()
            let want_status = view.reserve_status_footer();
            let hint = view.hint_keys();
            let hidden_approvals = self.approval_queue.len();
            let footer_rows = u16::from(want_status)
                + u16::from(hint.is_some())
                + u16::from(hidden_approvals > 0);
            if footer_rows > 0 && area.height > footer_rows {
                let view_h = area.height - footer_rows;
                let view_rect = Rect::new(area.x, area.y, area.width, view_h);
                view.render(view_rect, buf);
                let mut y = area.y + view_h;
                if hidden_approvals > 0 {
                    render_hidden_approval_attention(
                        hidden_approvals,
                        Rect::new(area.x, y, area.width, 1),
                        buf,
                    );
                    y += 1;
                }
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
        let intent_queue_h = self.user_intent_height();
        let next_turn_queue_h = self.next_turn_submission_height();

        let approval_h = self.focused_approval_height(area.width);
        if popup_h > 0 {
            let chunks = Layout::vertical([
                Constraint::Length(approval_h),
                Constraint::Length(intent_queue_h),
                Constraint::Length(next_turn_queue_h),
                Constraint::Length(content_h),
                Constraint::Length(popup_h),
                Constraint::Length(1),
            ])
            .split(area);

            self.render_focused_approval(chunks[0], buf);
            self.render_pending_user_intents(chunks[1], buf);
            self.render_queued_next_turn_submissions(chunks[2], buf);
            self.composer.render(
                chunks[3],
                buf,
                self.task_status.is_active(),
                self.has_pending_composer_queue(),
            );
            if let Some(ref menu) = self.slash_menu {
                slash_popup_render::render_composer(menu, chunks[4], buf);
            } else if let Some(ref menu) = self.mention_menu {
                mention_popup_render::render(menu, chunks[4], buf);
            } else if let Some(ref popup) = self.skill_popup {
                popup.render(chunks[4], buf);
            }
            self.footer.render(chunks[5], buf);
        } else {
            let chunks = Layout::vertical([
                Constraint::Length(approval_h),
                Constraint::Length(intent_queue_h),
                Constraint::Length(next_turn_queue_h),
                Constraint::Length(content_h),
                Constraint::Length(1),
            ])
            .split(area);

            self.render_focused_approval(chunks[0], buf);
            self.render_pending_user_intents(chunks[1], buf);
            self.render_queued_next_turn_submissions(chunks[2], buf);
            self.composer.render(
                chunks[3],
                buf,
                self.task_status.is_active(),
                self.has_pending_composer_queue(),
            );
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

    fn user_intent_height(&self) -> u16 {
        if self.pending_user_intents.is_empty() {
            return 0;
        }
        let preview_rows = self.pending_user_intents.len().min(2) as u16;
        let more_row = u16::from(self.pending_user_intents.len() > 2);
        1 + preview_rows + more_row
    }

    fn next_turn_submission_height(&self) -> u16 {
        if self.queued_next_turn_submissions.is_empty() {
            return 0;
        }
        let preview_rows = self.queued_next_turn_submissions.len().min(2) as u16;
        let more_row = u16::from(self.queued_next_turn_submissions.len() > 2);
        1 + preview_rows + more_row
    }

    fn render_pending_user_intents(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || self.pending_user_intents.is_empty() {
            return;
        }
        let theme = crate::tui::theme::current();

        // Distinct panel surface: visibly darker (dark term) / darker still
        // (light term) than the composer surface so the queue reads as its
        // own region, not as more of the input box.
        let panel = crate::tui::style::queue_panel_style();
        for y in area.y..area.y + area.height {
            buf.set_string(area.x, y, " ".repeat(area.width as usize), panel);
        }
        let bg = panel.bg.unwrap_or(ratatui::style::Color::Reset);

        let Some(head) = self.pending_user_intents.front() else {
            return;
        };
        let title = if self.interrupt_pending {
            "Stopping · unapplied guidance returns to composer".to_string()
        } else {
            pending_user_intent_title(head, &self.task_status)
        };
        let title_style = Style::default().fg(theme.accent).bg(bg);
        Widget::render(
            Line::from(Span::styled(
                truncate_display(&title, area.width as usize),
                title_style,
            )),
            Rect::new(area.x, area.y, area.width, 1),
            buf,
        );

        // Visual hierarchy on the queue band:
        //   head — accent + bold, it's about to fire.
        //   tail — `fg` (not `dim`): queued entries are user content that
        //     must remain legible against the low-contrast queue surface;
        //     `dim` (DarkGray) sat too close to the panel bg.
        //   +N more — `accent_dim` keeps the count present without
        //     competing with the queued text above. Dropped the DIM
        //     modifier: it doubled up on a color already at low emphasis.
        let head_style = Style::default()
            .fg(theme.accent)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let tail_style = Style::default().fg(theme.fg).bg(bg);
        let more_style = Style::default().fg(theme.accent_dim()).bg(bg);

        let preview_rows = (area.height as usize).saturating_sub(1);
        for (idx, pending) in self
            .pending_user_intents
            .iter()
            .take(preview_rows)
            .take(2)
            .enumerate()
        {
            // Truncate by the actual column budget, not a hard-coded 100.
            let status = match pending.status {
                astra_turn_types::UserIntentStatus::AcceptedLocal => match &pending.target {
                    PendingUserIntentTarget::ActiveRun => "queued",
                    PendingUserIntentTarget::AgentRun { .. } => "sending",
                },
                astra_turn_types::UserIntentStatus::AcceptedRemote => "delivered to run",
                astra_turn_types::UserIntentStatus::Applied => "applied",
            };
            let prefix = format!("{}  {status} · ", idx + 1);
            let budget = area.width.saturating_sub(prefix.width() as u16) as usize;
            let preview = deferred_followup_preview(&pending.text, budget);
            let line = format!("{prefix}{preview}");
            let style = if idx == 0 { head_style } else { tail_style };
            Widget::render(
                Line::from(Span::styled(
                    truncate_display(&line, area.width as usize),
                    style,
                )),
                Rect::new(area.x, area.y + 1 + idx as u16, area.width, 1),
                buf,
            );
        }
        if self.pending_user_intents.len() > 2 && area.height >= 4 {
            let line = format!("  +{} more", self.pending_user_intents.len() - 2);
            Widget::render(
                Line::from(Span::styled(
                    truncate_display(&line, area.width as usize),
                    more_style,
                )),
                Rect::new(area.x, area.y + 3, area.width, 1),
                buf,
            );
        }
    }

    fn render_queued_next_turn_submissions(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || self.queued_next_turn_submissions.is_empty() {
            return;
        }
        let theme = crate::tui::theme::current();
        let panel = crate::tui::style::queue_panel_style();
        for y in area.y..area.y + area.height {
            buf.set_string(area.x, y, " ".repeat(area.width as usize), panel);
        }
        let bg = panel.bg.unwrap_or(ratatui::style::Color::Reset);
        let title_style = Style::default().fg(theme.accent).bg(bg);
        Widget::render(
            Line::from(Span::styled(
                truncate_display(
                    "Next message queued · starts after this reply is committed",
                    area.width as usize,
                ),
                title_style,
            )),
            Rect::new(area.x, area.y, area.width, 1),
            buf,
        );

        let head_style = Style::default()
            .fg(theme.accent)
            .bg(bg)
            .add_modifier(Modifier::BOLD);
        let tail_style = Style::default().fg(theme.fg).bg(bg);
        let more_style = Style::default().fg(theme.accent_dim()).bg(bg);
        let preview_rows = (area.height as usize).saturating_sub(1);
        for (idx, text) in self
            .queued_next_turn_submissions
            .iter()
            .take(preview_rows)
            .take(2)
            .enumerate()
        {
            let prefix = format!("{}  queued · ", idx + 1);
            let budget = area.width.saturating_sub(prefix.width() as u16) as usize;
            let line = format!("{prefix}{}", deferred_followup_preview(text, budget));
            let style = if idx == 0 { head_style } else { tail_style };
            Widget::render(
                Line::from(Span::styled(
                    truncate_display(&line, area.width as usize),
                    style,
                )),
                Rect::new(area.x, area.y + 1 + idx as u16, area.width, 1),
                buf,
            );
        }
        if self.queued_next_turn_submissions.len() > 2 && area.height >= 4 {
            let line = format!("  +{} more", self.queued_next_turn_submissions.len() - 2);
            Widget::render(
                Line::from(Span::styled(
                    truncate_display(&line, area.width as usize),
                    more_style,
                )),
                Rect::new(area.x, area.y + 3, area.width, 1),
                buf,
            );
        }
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if let Some(view) = self.active_view() {
            return view.cursor_pos(area);
        }

        let approval_h = self.focused_approval_height(area.width);
        let intent_queue_h = self.user_intent_height();
        let next_turn_queue_h = self.next_turn_submission_height();
        let content_h = self.composer.desired_height(area.width);
        let chunks = Layout::vertical([
            Constraint::Length(approval_h),
            Constraint::Length(intent_queue_h),
            Constraint::Length(next_turn_queue_h),
            Constraint::Length(content_h),
            Constraint::Min(0),
        ])
        .split(area);

        self.composer.cursor_position(chunks[3])
    }
}

fn deferred_followup_preview(text: &str, limit: usize) -> String {
    let single_line = text.trim().replace('\n', " ⏎ ");
    let mut preview: String = single_line.chars().take(limit).collect();
    if single_line.chars().count() > limit {
        preview.push('…');
    }
    preview
}

fn truncate_display(text: &str, max_width: usize) -> String {
    if text.width() <= max_width {
        return text.to_string();
    }
    if max_width <= 1 {
        return "…".to_string();
    }
    let keep = max_width - 1;
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if used + width > keep {
            break;
        }
        out.push(ch);
        used += width;
    }
    out.push('…');
    out
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

/// Keep hidden approvals observable without competing with the modal for
/// focus. This row deliberately contains no shortcut: Esc has different
/// meanings across Transcript, AskUser, and PlanReview.
fn render_hidden_approval_attention(count: usize, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::Widget;

    let theme = crate::tui::theme::current();
    let noun = if count == 1 {
        "approval request"
    } else {
        "approval requests"
    };
    Widget::render(
        Line::from(vec![
            Span::styled(
                format!("  ● {count} {noun} waiting"),
                Style::default().fg(theme.warn).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                " · review after this panel".to_string(),
                Style::default().fg(theme.dim),
            ),
        ]),
        area,
        buf,
    );
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
    /// The shortcut was pressed while a turn owns the event loop. Opening the
    /// editor here would stop polling that turn, so the dispatcher must show a
    /// non-blocking explanation instead.
    ExternalEditorUnavailable,
    /// Request the explicit permission-mode picker. Permission modes encode
    /// distinct capability/consent policies, so keyboard navigation must
    /// never silently advance through them as if they were one dial.
    OpenPermissionModePicker,
    ViewCompleted {
        result: Option<view::ViewResult>,
        reopen: Option<String>,
    },
    ViewAction(BottomPaneViewAction),
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
