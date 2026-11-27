pub(crate) mod agent_transcript_view;
pub(crate) mod ask_user_view;
pub(crate) mod background_task_view;
pub(crate) mod chat_composer;
pub(crate) mod config_edit_view;
pub(crate) mod context_panel_view;
pub(crate) mod footer;
pub(crate) mod form_view;
pub(crate) mod help_view;
pub(crate) mod in_flight_agents_view;
pub(crate) mod info_view;
pub(crate) mod list_selection_view;
pub(crate) mod local_output_view;
pub(crate) mod login_view;
pub(crate) mod paste_burst;
pub(crate) mod plan_review_view;
pub(crate) mod root_transcript_view;
pub(crate) mod session_picker_view;
pub(crate) mod skill_popup;
pub(crate) mod task_input_view;
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
#[cfg(test)]
mod update_notice_tests;

use chat_composer::{ChatComposer, ComposerAction};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use footer::Footer;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
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
use unicode_width::UnicodeWidthStr;

const MAX_PENDING_INTENTS: usize = 64;
const MAX_PENDING_BYTES: usize = 1024 * 1024;

enum UnsentDraft {
    Returned(String),
    Parked(String),
}

impl UnsentDraft {
    fn text(&self) -> &str {
        match self {
            Self::Returned(text) | Self::Parked(text) => text,
        }
    }

    fn into_text(self) -> String {
        match self {
            Self::Returned(text) | Self::Parked(text) => text,
        }
    }
}

enum ModelCatalogState {
    Ready(Vec<astra_services::ModelListItemResponse>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NextTurnSubmission {
    Interactive(String),
    Guidance(PendingUserIntent),
    Conversation {
        target: crate::tui::event_loop::SessionContinuationTarget,
        content: String,
    },
}

impl NextTurnSubmission {
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Interactive(text) => text,
            Self::Guidance(intent) => &intent.text,
            Self::Conversation { content, .. } => content,
        }
    }
}

pub(crate) struct BottomPane {
    pub composer: ChatComposer,
    pub footer: Footer,
    pub update_notice: Option<String>,
    detached_editor_limit_notice: bool,
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
    /// Presentation scope supplied by the session owner; never authorizes a send.
    session_attachment: Option<(Option<String>, u64)>,
    active_root_scope: Option<Arc<RootIntentScope>>,
    /// User messages accepted after visible output ended but before the
    /// current turn has committed its canonical boundary. These are next-turn
    /// submissions, not guidance for the previous run.
    queued_next_turn_submissions: std::collections::VecDeque<NextTurnSubmission>,
    /// Recovery is editable input, not authority for automatic next-turn replay.
    restored_drafts: std::collections::VecDeque<UnsentDraft>,
    applied_user_intent_ids: std::collections::HashSet<String>,
    /// Typed actions emitted while a retained projection refreshes. These are
    /// not user-input events: for example, a live agent transcript upgrades
    /// itself once a later receipt supplies its durable history location.
    projection_actions: std::collections::VecDeque<BottomPaneViewAction>,
    model_catalog: Option<ModelCatalogState>,
    /// One-shot model source/management read, dispatched by the existing setup task owner.
    pub(crate) pending_model_setup_read: Option<crate::tui::model_setup::Action>,
    /// True when the user pressed Esc/Ctrl+C to interrupt the current
    /// run and terminal settlement is pending. The queue panel reflects
    /// this intermediate state so the user isn't stuck wondering why
    /// "Esc sends now" isn't taking effect immediately.
    pub(crate) interrupt_pending: bool,
    /// Explicit permission selection made while a turn owns `SessionState`.
    /// It is a UI intent, not the active policy: the event loop applies it
    /// only after the current turn settles, so current tools and the footer
    /// cannot disagree about which policy actually governed execution.
    staged_permission_mode: Option<crate::cli::permission_manager::PermissionMode>,
    /// A local presentation clock. No tip is written to the transcript.
    tip_rotation: Option<(std::time::Instant, u64)>,
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
    /// Model-facing text when a local command needs a different wire form.
    /// The original text remains available for explicit user recovery.
    pub(crate) wire_text: Option<String>,
    /// An unsent revision of a returned child message, scoped to its owner.
    pub(crate) retry_draft: Option<String>,
    pub(crate) target: PendingUserIntentTarget,
    /// Physical turn that owns an active-run submission. Child messages keep
    /// their own captured target and do not use this field.
    turn_id: Option<String>,
    pub(crate) next_turn_position: usize,
    custody: PendingUserIntentCustody,
    root_scope: Option<Arc<RootIntentScope>>,
}

/// Reuses the foreground turn's owner and first-wins session binding.
#[derive(Debug)]
pub(crate) struct RootIntentScope {
    owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
    attachment_epoch: u64,
    session_binding: Arc<std::sync::Mutex<Option<String>>>,
    remote_run: Option<tokio::sync::watch::Receiver<Option<String>>>,
    recheck_in_flight: std::sync::atomic::AtomicBool,
    started_unbound: bool,
}

pub(crate) struct RootIntentRecheck(Arc<RootIntentScope>);

impl Drop for RootIntentRecheck {
    fn drop(&mut self) {
        self.0
            .recheck_in_flight
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

impl RootIntentScope {
    pub(crate) fn new(
        owner: crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
        attachment_epoch: u64,
        session_binding: Arc<std::sync::Mutex<Option<String>>>,
    ) -> Self {
        let started_unbound =
            astra_core::sync_poison::recover_mutex_lock(&session_binding).is_none();
        Self {
            owner,
            attachment_epoch,
            session_binding,
            remote_run: None,
            recheck_in_flight: std::sync::atomic::AtomicBool::new(false),
            started_unbound,
        }
    }

    /// Retain the existing first-wins Run binding for later receipt lookup.
    pub(crate) fn with_remote_run(
        mut self,
        remote_run: tokio::sync::watch::Receiver<Option<String>>,
    ) -> Self {
        self.remote_run = Some(remote_run);
        self
    }

    pub(crate) fn recovery_context(
        &self,
    ) -> Option<(
        crate::cli::cli_config::cli_utils::CliOwnerAuthSnapshot,
        String,
        String,
    )> {
        let owner = crate::cli::cli_config::cli_utils::cli_owner_auth_snapshot();
        if !owner.is_current() || !self.owner.same_presentation_owner(&owner) {
            return None;
        }
        let session = astra_core::sync_poison::recover_mutex_lock(&self.session_binding).clone()?;
        let remote_run = self.remote_run.as_ref()?;
        // While the originating turn is active, its ordered stream owns
        // settlement and its FIFO wakeup. Recheck only after that owner exits.
        if remote_run.has_changed().is_ok() {
            return None;
        }
        let run = remote_run.borrow().clone()?;
        (!run.trim().is_empty()).then_some((owner, session, run))
    }

    pub(crate) fn begin_recheck(self: &Arc<Self>) -> Option<RootIntentRecheck> {
        self.recheck_in_flight
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()
            .map(|_| RootIntentRecheck(self.clone()))
    }

    pub(crate) fn visible_in(&self, session_id: Option<&str>, epoch: u64) -> bool {
        if !self.owner.can_view_local_draft() {
            return false;
        }
        match astra_core::sync_poison::recover_mutex_lock(&self.session_binding).as_deref() {
            Some(bound) => session_id == Some(bound),
            None => session_id.is_none() && epoch == self.attachment_epoch,
        }
    }

    pub(crate) fn owner_can_view(&self) -> bool {
        self.owner.can_view_local_draft()
    }

    fn returned_unbound_visible_in(&self, session_id: Option<&str>) -> bool {
        session_id.is_none()
            && self.owner.can_view_local_draft()
            && astra_core::sync_poison::recover_mutex_lock(&self.session_binding).is_none()
    }

    pub(crate) fn attached_in(&self, session_id: Option<&str>, epoch: u64) -> bool {
        if !self.owner.is_current() {
            return false;
        }
        match astra_core::sync_poison::recover_mutex_lock(&self.session_binding).as_deref() {
            Some(bound) => {
                session_id == Some(bound)
                    && epoch
                        == if self.started_unbound {
                            self.attachment_epoch.saturating_add(1)
                        } else {
                            self.attachment_epoch
                        }
            }
            None => session_id.is_none() && epoch == self.attachment_epoch,
        }
    }

    fn same_session(&self, other: &Self) -> bool {
        if !self.owner.same_presentation_owner(&other.owner) {
            return false;
        }
        if Arc::ptr_eq(&self.session_binding, &other.session_binding) {
            return true;
        }
        let first = astra_core::sync_poison::recover_mutex_lock(&self.session_binding).clone();
        let second = astra_core::sync_poison::recover_mutex_lock(&other.session_binding).clone();
        first == second
    }
}

impl PartialEq for RootIntentScope {
    fn eq(&self, other: &Self) -> bool {
        self.attachment_epoch == other.attachment_epoch
            && self.owner == other.owner
            && Arc::ptr_eq(&self.session_binding, &other.session_binding)
            && self.started_unbound == other.started_unbound
    }
}

impl Eq for RootIntentScope {}

impl PendingUserIntent {
    fn retained_bytes(&self) -> usize {
        self.text
            .len()
            .saturating_add(self.wire_text.as_ref().map_or(0, String::len))
            .saturating_add(self.retry_draft.as_ref().map_or(0, String::len))
    }

    pub(crate) fn root_scope(&self) -> Option<Arc<RootIntentScope>> {
        self.root_scope.clone()
    }

    pub(crate) fn wire_text(&self) -> &str {
        self.wire_text.as_deref().unwrap_or(&self.text)
    }

    pub(crate) fn composer_text(&self) -> std::borrow::Cow<'_, str> {
        if self.wire_text.is_some() {
            std::borrow::Cow::Borrowed(&self.text)
        } else {
            super::slash_dispatch::conversation_for_composer(&self.text)
        }
    }

    pub(crate) fn delivery_is_unconfirmed(&self) -> bool {
        self.custody == PendingUserIntentCustody::Unconfirmed
    }
}

/// Why a local guidance intent was not admitted into the pending queue.
///
/// Admission is a local input/state check. Keeping the reason typed prevents
/// callers from presenting an input error as an ownership-transfer failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UserIntentRejectReason {
    EmptyIntentId,
    EmptyText,
    InvalidStatus,
    IdentityAlreadyKnown,
    QueueFull,
}

impl UserIntentRejectReason {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::EmptyIntentId => "Could not queue guidance: missing intent identity.",
            // Empty input is intentionally silent at the event-loop boundary.
            Self::EmptyText => "",
            Self::InvalidStatus => "Could not queue guidance: invalid local acceptance state.",
            Self::IdentityAlreadyKnown => {
                "Could not queue guidance: this intent is already recorded locally."
            }
            Self::QueueFull => "Too many messages are waiting. Your draft was not sent.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PendingUserIntentCustody {
    /// No request can have committed; the client may safely recover the text.
    Client,
    /// A request crossed the transport boundary but no authoritative receipt
    /// arrived. Keep its stable identity visible and never auto-replay it.
    Unconfirmed,
    /// The durable run acknowledged ownership.
    Run,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingUserIntentTarget {
    ActiveRun,
    AgentRun {
        target: crate::tui::event_loop::AgentGuideTarget,
    },
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AgentDeliveryBarrier {
    Sending,
    Unconfirmed(Vec<String>),
}

fn pending_user_intent_title(intent: &PendingUserIntent, task_status: &TaskStatus) -> String {
    if intent.custody == PendingUserIntentCustody::Unconfirmed {
        return match &intent.target {
            PendingUserIntentTarget::ActiveRun => {
                if intent.status == astra_turn_types::UserIntentStatus::AcceptedRemote {
                    "Message received · whether it was used is unknown".to_string()
                } else {
                    "Message delivery is unknown".to_string()
                }
            }
            PendingUserIntentTarget::AgentRun { target } => {
                let agent_name = target.agent_name();
                let action = if intent.status == astra_turn_types::UserIntentStatus::AcceptedRemote
                {
                    "used"
                } else {
                    "received"
                };
                format!("Can't confirm {agent_name} {action} your message")
            }
        };
    }
    match (&intent.target, intent.status) {
        (
            PendingUserIntentTarget::AgentRun { target },
            astra_turn_types::UserIntentStatus::AcceptedLocal,
        ) => format!("Sending message to {}", target.agent_name()),
        (
            PendingUserIntentTarget::AgentRun { target },
            astra_turn_types::UserIntentStatus::AcceptedRemote,
        ) => format!(
            "Message sent to {} · waiting to take effect",
            target.agent_name()
        ),
        (
            PendingUserIntentTarget::AgentRun { target },
            astra_turn_types::UserIntentStatus::Applied,
        ) => format!("Message added for {}", target.agent_name()),
        (
            PendingUserIntentTarget::AgentRun { target },
            astra_turn_types::UserIntentStatus::Returned,
        ) => format!("Message returned from {}", target.agent_name()),
        (PendingUserIntentTarget::ActiveRun, astra_turn_types::UserIntentStatus::AcceptedLocal) => {
            "Guidance pending · current run".to_string()
        }
        (
            PendingUserIntentTarget::ActiveRun,
            astra_turn_types::UserIntentStatus::AcceptedRemote,
        ) => match task_status {
            TaskStatus::Cancelling => "Stopping · checking message delivery".to_string(),
            TaskStatus::Exiting => "Exiting · checking message delivery".to_string(),
            TaskStatus::Idle => "Checking whether your message was used".to_string(),
            _ => "Guidance received · current run".to_string(),
        },
        (PendingUserIntentTarget::ActiveRun, astra_turn_types::UserIntentStatus::Applied) => {
            "Message added to the conversation".to_string()
        }
        (PendingUserIntentTarget::ActiveRun, astra_turn_types::UserIntentStatus::Returned) => {
            "Unsent message · Alt+U to restore".to_string()
        }
    }
}

impl BottomPane {
    pub fn new() -> Self {
        Self {
            composer: ChatComposer::new(),
            footer: Footer::new(),
            update_notice: None,
            detached_editor_limit_notice: false,
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
            session_attachment: None,
            active_root_scope: None,
            queued_next_turn_submissions: std::collections::VecDeque::new(),
            restored_drafts: std::collections::VecDeque::new(),
            applied_user_intent_ids: std::collections::HashSet::new(),
            projection_actions: std::collections::VecDeque::new(),
            model_catalog: None,
            pending_model_setup_read: None,
            interrupt_pending: false,
            staged_permission_mode: None,
            tip_rotation: None,
        }
    }

    pub fn set_skill_items(&mut self, items: Vec<skill_popup::SkillItem>) {
        self.skill_items = items;
    }

    pub(crate) fn set_session_attachment(&mut self, session_id: Option<&str>, epoch: u64) {
        if self
            .session_attachment
            .as_ref()
            .is_some_and(|(id, previous_epoch)| {
                id.as_deref() != session_id || *previous_epoch != epoch
            })
        {
            self.composer.cancel_history_search();
        }
        let mut count = 0usize;
        let mut bytes = 0usize;
        let mut lost_edits = false;
        self.view_stack.retain_mut(|view| {
            if view.visible_in_attachment(session_id, epoch) {
                return true;
            }
            let Some(size) = view.prepare_detached_editor() else {
                return false;
            };
            if count < MAX_PENDING_INTENTS && bytes.saturating_add(size) <= MAX_PENDING_BYTES {
                count += 1;
                bytes += size;
                true
            } else {
                lost_edits = true;
                false
            }
        });
        self.detached_editor_limit_notice |= lost_edits;
        self.session_attachment = Some((session_id.map(str::to_owned), epoch));
    }

    /// Check an explicit switch before changing sessions. None denotes a
    /// newly created session whose identity is not yet known.
    pub(crate) fn can_park_editors_for(&mut self, session_id: Option<&str>, epoch: u64) -> bool {
        if self
            .session_attachment
            .as_ref()
            .is_some_and(|(current, current_epoch)| {
                self.pending_user_intents.iter().any(|intent| {
                    matches!(&intent.target, PendingUserIntentTarget::AgentRun { target }
                        if !target.has_bound_session()
                            && target.can_view_draft(current.as_deref(), *current_epoch))
                })
            })
        {
            return false;
        }
        let mut count = 0usize;
        let mut bytes = 0usize;
        for view in &mut self.view_stack {
            if (session_id.is_none() || !view.visible_in_attachment(session_id, epoch))
                && let Some(size) = view.prepare_detached_editor()
            {
                if !view.can_park_for_explicit_switch() {
                    return false;
                }
                count += 1;
                bytes = bytes.saturating_add(size);
                if count > MAX_PENDING_INTENTS || bytes > MAX_PENDING_BYTES {
                    return false;
                }
            }
        }
        true
    }

    pub(crate) fn bind_initial_agent_session(&mut self, session_id: &str, epoch: u64) {
        for intent in &self.pending_user_intents {
            if let PendingUserIntentTarget::AgentRun { target } = &intent.target {
                target.bind_initial_session(session_id, epoch);
            }
        }
        for view in &mut self.view_stack {
            view.bind_initial_agent_session(session_id, epoch);
        }
    }

    pub(crate) fn root_scope_visible(&self, scope: &RootIntentScope) -> bool {
        self.session_attachment
            .as_ref()
            .is_some_and(|(session_id, epoch)| scope.visible_in(session_id.as_deref(), *epoch))
    }

    pub(crate) fn root_guidance_copy_scope(&self, intent_id: &str) -> Option<Arc<RootIntentScope>> {
        let scope = self.active_view()?.root_guidance_copy_scope(intent_id)?;
        self.root_scope_visible(&scope).then_some(scope)
    }

    pub(crate) fn set_root_intent_scope(&mut self, scope: Arc<RootIntentScope>) {
        self.active_root_scope = Some(scope);
    }

    fn intent_is_visible(&self, intent: &PendingUserIntent) -> bool {
        match &intent.target {
            PendingUserIntentTarget::ActiveRun => intent.root_scope.as_ref().is_none_or(|scope| {
                self.session_attachment
                    .as_ref()
                    .is_some_and(|(session_id, epoch)| {
                        scope.visible_in(session_id.as_deref(), *epoch)
                            || (intent.status == astra_turn_types::UserIntentStatus::Returned
                                && scope.returned_unbound_visible_in(session_id.as_deref()))
                    })
            }),
            PendingUserIntentTarget::AgentRun { target } => self
                .session_attachment
                .as_ref()
                .is_some_and(|(session_id, epoch)| {
                    target.can_view_draft(session_id.as_deref(), *epoch)
                }),
        }
    }

    fn visible_pending_intents(&self) -> impl Iterator<Item = &PendingUserIntent> {
        self.pending_user_intents
            .iter()
            .filter(|intent| self.intent_is_visible(intent))
    }

    pub(crate) fn next_recoverable_guidance(&self, after: &str) -> Option<PendingUserIntent> {
        let candidates: Vec<_> = self
            .visible_pending_intents()
            .filter(|intent| match &intent.target {
                PendingUserIntentTarget::ActiveRun => {
                    intent.custody == PendingUserIntentCustody::Unconfirmed
                }
                PendingUserIntentTarget::AgentRun { .. } => {
                    intent.custody == PendingUserIntentCustody::Unconfirmed
                        || intent.status == astra_turn_types::UserIntentStatus::Returned
                }
            })
            .collect();
        if candidates.is_empty() {
            return None;
        }
        let next = candidates
            .iter()
            .position(|intent| intent.intent_id == after)
            .map_or(0, |index| (index + 1) % candidates.len());
        Some(candidates[next].clone())
    }

    pub(crate) fn activate_recoverable_guidance(&mut self, intent: &PendingUserIntent) -> bool {
        let Some(index) = self
            .view_stack
            .iter()
            .rposition(|view| view.is_recoverable_guidance(intent))
        else {
            return false;
        };
        let view = self.view_stack.remove(index);
        self.view_stack.push(view);
        true
    }

    pub(crate) fn update_root_guidance_view(
        &mut self,
        intent: &PendingUserIntent,
        status: astra_turn_types::UserIntentStatus,
    ) {
        let terminal = matches!(
            status,
            astra_turn_types::UserIntentStatus::Applied
                | astra_turn_types::UserIntentStatus::Returned
        );
        let focused = self.active_view_index();
        let mut index = 0;
        self.view_stack.retain_mut(|view| {
            let is_focused = Some(index) == focused;
            index += 1;
            if view.is_recoverable_guidance(intent) {
                if terminal && !is_focused {
                    return false;
                }
                view.update_root_guidance_status(&intent.intent_id, status);
            }
            true
        });
    }

    fn visible_next_turn_submissions(&self) -> impl Iterator<Item = &NextTurnSubmission> {
        self.queued_next_turn_submissions
            .iter()
            .filter(|entry| match entry {
                NextTurnSubmission::Guidance(intent) => self.intent_is_visible(intent),
                _ => true,
            })
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

    #[cfg(test)]
    pub fn handle_paste(&mut self, text: &str) {
        self.handle_session_paste(text, &[]);
    }

    /// Handle a bracketed-paste payload. Multi-line pastes get
    /// collapsed into a `[Pasted #N · M lines]` placeholder; short
    /// pastes are inserted verbatim. After the paste lands, popup
    /// state is resynced because paste can newly trigger `/`, `@`, `$`.
    pub(crate) fn handle_session_paste(
        &mut self,
        text: &str,
        history: &[Arc<dyn crate::tui::history_cell::HistoryCell>],
    ) {
        let now = std::time::Instant::now();
        self.dismiss_closed_views_at(now);
        if self.has_active_view() {
            // The explicit paste belongs to the focused view; a parked
            // composer's old unbracketed tail must not capture later keys.
            self.composer.release_paste_focus_at(now);
        }
        if self
            .active_view_mut()
            .is_some_and(|view| view.handle_paste(text))
        {
            return;
        }
        self.composer.handle_paste(text);
        self.sync_popups();
        self.refresh_history_suggestions(history);
    }

    /// Project an intent that was accepted by the active run's control plane.
    #[cfg(test)]
    pub fn accept_user_intent(
        &mut self,
        intent_id: impl Into<String>,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        text: impl Into<String>,
    ) -> bool {
        self.try_accept_user_intent(intent_id, delivery, status, text)
            .is_ok()
    }

    /// Admit an active-run guidance intent and retain the precise local
    /// rejection reason for the event loop. The bool API above remains the
    /// compact projection used by existing callers and tests.
    #[cfg(test)]
    pub(crate) fn try_accept_user_intent(
        &mut self,
        intent_id: impl Into<String>,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        text: impl Into<String>,
    ) -> Result<(), UserIntentRejectReason> {
        self.accept_user_intent_for_target(
            intent_id.into(),
            delivery,
            status,
            (text.into(), None),
            PendingUserIntentTarget::ActiveRun,
            None,
        )
    }

    pub(crate) fn accept_turn_user_intent(
        &mut self,
        turn_id: &str,
        intent_id: String,
        text: String,
    ) -> Result<(), UserIntentRejectReason> {
        self.accept_user_intent_for_target(
            intent_id,
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::AcceptedLocal,
            (text, None),
            PendingUserIntentTarget::ActiveRun,
            Some(turn_id.to_owned()),
        )
    }

    pub(crate) fn accept_turn_plan_intent(
        &mut self,
        turn_id: &str,
        intent_id: String,
        command: String,
        wire_text: String,
    ) -> Result<(), UserIntentRejectReason> {
        self.accept_user_intent_for_target(
            intent_id,
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::AcceptedLocal,
            (command, Some(wire_text)),
            PendingUserIntentTarget::ActiveRun,
            Some(turn_id.to_owned()),
        )
    }

    pub fn accept_agent_guide(
        &mut self,
        intent_id: String,
        target: crate::tui::event_loop::AgentGuideTarget,
        text: String,
    ) -> bool {
        self.accept_user_intent_for_target(
            intent_id,
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::AcceptedLocal,
            (text, None),
            PendingUserIntentTarget::AgentRun { target },
            None,
        )
        .is_ok()
    }

    pub(crate) fn agent_delivery_barrier(
        &self,
        target: &crate::tui::event_loop::AgentGuideTarget,
        acknowledged_unknown: &[String],
    ) -> Option<AgentDeliveryBarrier> {
        let acknowledged = acknowledged_unknown
            .iter()
            .map(String::as_str)
            .collect::<std::collections::HashSet<_>>();
        let mut unknown = Vec::new();
        for intent in &self.pending_user_intents {
            let PendingUserIntentTarget::AgentRun { target: previous } = &intent.target else {
                continue;
            };
            if !previous.same_recipient(target) {
                continue;
            }
            match (intent.custody, intent.status) {
                (PendingUserIntentCustody::Client, _) => {
                    return Some(AgentDeliveryBarrier::Sending);
                }
                (
                    PendingUserIntentCustody::Unconfirmed,
                    astra_turn_types::UserIntentStatus::AcceptedLocal,
                ) => unknown.push(intent.intent_id.clone()),
                _ => {}
            }
        }
        unknown
            .iter()
            .any(|id| !acknowledged.contains(id.as_str()))
            .then_some(AgentDeliveryBarrier::Unconfirmed(unknown))
    }

    fn accept_user_intent_for_target(
        &mut self,
        intent_id: String,
        delivery: astra_turn_types::UserIntentDelivery,
        status: astra_turn_types::UserIntentStatus,
        text: (String, Option<String>),
        target: PendingUserIntentTarget,
        turn_id: Option<String>,
    ) -> Result<(), UserIntentRejectReason> {
        let (text, wire_text) = text;
        let counts_toward_limit = |intent: &&PendingUserIntent| match &target {
            PendingUserIntentTarget::ActiveRun => {
                matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                    && match (&self.active_root_scope, &intent.root_scope) {
                        (Some(current), Some(previous)) => current.same_session(previous),
                        _ => intent.custody == PendingUserIntentCustody::Client,
                    }
            }
            PendingUserIntentTarget::AgentRun { target } => matches!(&intent.target,
                PendingUserIntentTarget::AgentRun { target: owner } if owner.same_session(target)),
        };
        let rejection = if intent_id.trim().is_empty() {
            Some(UserIntentRejectReason::EmptyIntentId)
        } else if text.trim().is_empty() {
            Some(UserIntentRejectReason::EmptyText)
        } else if !matches!(
            status,
            astra_turn_types::UserIntentStatus::AcceptedLocal
                | astra_turn_types::UserIntentStatus::AcceptedRemote
        ) {
            Some(UserIntentRejectReason::InvalidStatus)
        } else if self.applied_user_intent_ids.contains(&intent_id)
            || self
                .pending_user_intents
                .iter()
                .any(|pending| pending.intent_id == intent_id)
        {
            Some(UserIntentRejectReason::IdentityAlreadyKnown)
        } else if self
            .pending_user_intents
            .iter()
            .filter(counts_toward_limit)
            .count()
            >= MAX_PENDING_INTENTS
            || self
                .pending_user_intents
                .iter()
                .filter(counts_toward_limit)
                .map(|intent| intent.retained_bytes())
                .sum::<usize>()
                .saturating_add(text.len())
                .saturating_add(wire_text.as_ref().map_or(0, String::len))
                > MAX_PENDING_BYTES
        {
            Some(UserIntentRejectReason::QueueFull)
        } else {
            None
        };
        if let Some(reason) = rejection {
            return Err(reason);
        }
        let root_scope = matches!(target, PendingUserIntentTarget::ActiveRun)
            .then(|| self.active_root_scope.clone())
            .flatten();
        self.pending_user_intents.push_back(PendingUserIntent {
            intent_id,
            delivery,
            status,
            text,
            wire_text,
            retry_draft: None,
            target,
            turn_id,
            next_turn_position: self.queued_next_turn_submissions.len(),
            root_scope,
            custody: if status == astra_turn_types::UserIntentStatus::AcceptedRemote {
                PendingUserIntentCustody::Run
            } else {
                PendingUserIntentCustody::Client
            },
        });
        Ok(())
    }

    /// Select only an unsent entry belonging to this physical turn. Older
    /// unconfirmed entries remain visible without blocking a new turn.
    pub(crate) fn next_unsent_guidance(&self, turn_id: &str) -> Option<&PendingUserIntent> {
        for intent in &self.pending_user_intents {
            if intent.turn_id.as_deref() != Some(turn_id)
                || !matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                || intent.status == astra_turn_types::UserIntentStatus::Returned
            {
                continue;
            }
            // An explicit next-turn submission entered earlier is a barrier.
            if intent.next_turn_position > 0 {
                return None;
            }
            match intent.custody {
                PendingUserIntentCustody::Unconfirmed => return None,
                PendingUserIntentCustody::Client => return Some(intent),
                // Later messages cannot overtake a plan already accepted by
                // the run but not yet applied.
                PendingUserIntentCustody::Run if intent.wire_text.is_some() => return None,
                PendingUserIntentCustody::Run => {}
            }
        }
        None
    }

    /// A plan may change permission only after all earlier guidance was applied.
    pub(crate) fn next_startable_plan(&self, turn_id: &str) -> Option<&PendingUserIntent> {
        let first = self.pending_user_intents.iter().find(|intent| {
            intent.turn_id.as_deref() == Some(turn_id)
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.status != astra_turn_types::UserIntentStatus::Returned
        })?;
        (first.custody == PendingUserIntentCustody::Client
            && first.wire_text.is_some()
            && first.next_turn_position == 0)
            .then_some(first)
    }

    pub(crate) fn is_plan_user_intent(&self, intent_id: &str) -> bool {
        self.pending_user_intents
            .iter()
            .any(|intent| intent.intent_id == intent_id && intent.wire_text.is_some())
    }

    pub(crate) fn has_turn_guidance_needing_attention(&self, turn_id: &str) -> bool {
        self.pending_user_intents.iter().any(|intent| {
            intent.turn_id.as_deref() == Some(turn_id)
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && (intent.custody == PendingUserIntentCustody::Unconfirmed
                    || intent.status == astra_turn_types::UserIntentStatus::Returned)
        })
    }

    pub(crate) fn has_pending_turn_guidance(&self, turn_id: &str) -> bool {
        self.pending_user_intents.iter().any(|intent| {
            intent.turn_id.as_deref() == Some(turn_id)
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
        })
    }

    pub(crate) fn accepted_turn_guidance(&self, turn_id: &str) -> Vec<PendingUserIntent> {
        self.pending_user_intents
            .iter()
            .filter(|intent| {
                intent.turn_id.as_deref() == Some(turn_id)
                    && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                    && intent.status == astra_turn_types::UserIntentStatus::AcceptedRemote
            })
            .cloned()
            .collect()
    }

    pub fn promote_agent_guide_accepted(&mut self, intent_id: &str) -> bool {
        let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::AgentRun { .. })
                && intent.status != astra_turn_types::UserIntentStatus::Returned
        }) else {
            return false;
        };
        intent.status = astra_turn_types::UserIntentStatus::AcceptedRemote;
        intent.custody = PendingUserIntentCustody::Run;
        true
    }

    /// Stop detached observation; an in-flight submission remains uncertain.
    pub(crate) fn retire_detached_agent_observations(
        &mut self,
        session_id: Option<&str>,
        epoch: u64,
    ) {
        self.set_session_attachment(session_id, epoch);
        for intent in &mut self.pending_user_intents {
            if let PendingUserIntentTarget::AgentRun { target } = &intent.target
                && !target.observation_is_attached(session_id, epoch)
            {
                target.retire_observation();
                if intent.status != astra_turn_types::UserIntentStatus::Returned
                    && matches!(
                        intent.custody,
                        PendingUserIntentCustody::Client | PendingUserIntentCustody::Run
                    )
                {
                    intent.custody = PendingUserIntentCustody::Unconfirmed;
                }
            }
        }
    }

    /// Record transport ambiguity without reclaiming or resending the input.
    pub fn mark_user_intent_unconfirmed(&mut self, intent_id: &str) -> bool {
        let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && intent.status != astra_turn_types::UserIntentStatus::Returned
                && matches!(
                    intent.custody,
                    PendingUserIntentCustody::Client | PendingUserIntentCustody::Run
                )
        }) else {
            return false;
        };
        intent.custody = PendingUserIntentCustody::Unconfirmed;
        true
    }

    pub(crate) fn mark_root_guidance_received(&mut self, intent_id: &str) {
        let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Unconfirmed
        }) else {
            return;
        };
        intent.status = astra_turn_types::UserIntentStatus::AcceptedRemote;
        for view in &mut self.view_stack {
            view.update_root_guidance_status(
                intent_id,
                astra_turn_types::UserIntentStatus::AcceptedRemote,
            );
        }
    }

    pub(crate) fn root_guidance_check_notice(&mut self, intent_id: &str, message: Option<&str>) {
        for view in &mut self.view_stack {
            view.root_guidance_check_notice(intent_id, message);
        }
    }

    pub(crate) fn agent_guidance_check_notice(
        &mut self,
        original: &crate::tui::event_loop::AgentGuideTarget,
        requester: &crate::tui::event_loop::AgentGuideTarget,
        intent_id: &str,
        message: Option<&str>,
    ) {
        for view in &mut self.view_stack {
            view.agent_guidance_check_notice(original, requester, intent_id, message);
        }
    }

    pub(crate) fn mark_agent_guidance_received(
        &mut self,
        intent_id: &str,
        target: &crate::tui::event_loop::AgentGuideTarget,
    ) {
        if let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && intent.custody == PendingUserIntentCustody::Unconfirmed
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target: owner }
                    if owner.same_execution(target))
        }) {
            intent.status = astra_turn_types::UserIntentStatus::AcceptedRemote;
        }
    }

    /// Transfer an active-run intent from local custody to the durable run.
    ///
    /// The local entry is created before the HTTP submission starts so a
    /// lost acknowledgement cannot turn the same bytes back into an
    /// apparently unsent draft. A matching durable Applied/Returned event can
    /// therefore settle the identity even when the POST response is lost.
    pub fn promote_user_intent_accepted(&mut self, intent_id: &str) -> bool {
        let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Client
        }) else {
            return false;
        };
        intent.status = astra_turn_types::UserIntentStatus::AcceptedRemote;
        intent.custody = PendingUserIntentCustody::Run;
        true
    }

    pub(crate) fn user_intent_is_resolved(&self, intent_id: &str) -> bool {
        self.applied_user_intent_ids.contains(intent_id)
    }

    /// Remove a locally-owned active-run intent after a definitive rejection.
    /// Ambiguous transport failures deliberately do not use this path: the
    /// run may already own the idempotent intent.
    pub fn remove_local_user_intent(&mut self, intent_id: &str) -> Option<PendingUserIntent> {
        let index = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Client
        })?;
        self.pending_user_intents.remove(index)
    }

    /// Claim one member intent in its original attachment. Mailbox receipts
    /// additionally have to identify the exact run that observed the message.
    pub fn remove_agent_guide(
        &mut self,
        intent_id: &str,
        session_id: Option<&str>,
        attachment_epoch: u64,
        receiver_run_id: Option<&str>,
    ) -> Option<PendingUserIntent> {
        let index = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && intent.status != astra_turn_types::UserIntentStatus::Returned
                && matches!(&intent.target,
                    PendingUserIntentTarget::AgentRun { target }
                        if target.observation_is_attached(session_id, attachment_epoch)
                            && receiver_run_id.is_none_or(|receiver| receiver == target.run_id())
                )
        })?;
        self.pending_user_intents.remove(index)
    }

    /// Settle only the exact attached child receipt named by the live event.
    pub(crate) fn settle_agent_guide_signal(
        &mut self,
        intent_id: &str,
        run_id: &str,
        epoch: u64,
        returned: bool,
    ) -> Option<crate::tui::event_loop::AgentGuideTarget> {
        let (session, attached_epoch) = self.session_attachment.as_ref()?;
        if *attached_epoch != epoch {
            return None;
        }
        let index = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && intent.status != astra_turn_types::UserIntentStatus::Returned
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target }
                    if target.observation_is_attached(session.as_deref(), epoch) && target.run_id() == run_id)
        })?;
        let intent = self.pending_user_intents.get_mut(index)?;
        let PendingUserIntentTarget::AgentRun { target } = &intent.target else {
            return None;
        };
        let target = target.clone();
        target.retire_observation();
        if returned {
            intent.status = astra_turn_types::UserIntentStatus::Returned;
            intent.custody = PendingUserIntentCustody::Run;
        } else {
            self.pending_user_intents.remove(index);
        }
        Some(target)
    }

    pub(crate) fn resolve_agent_guide_view(
        &mut self,
        target: &crate::tui::event_loop::AgentGuideTarget,
        intent_id: &str,
        status: astra_turn_types::UserIntentStatus,
    ) {
        for view in &mut self.view_stack {
            view.resolve_agent_guide(target, intent_id, status);
        }
    }

    pub(crate) fn refresh_agent_guide_recipient(
        &mut self,
        target: &crate::tui::event_loop::AgentGuideTarget,
        intent_id: &str,
        recipient: &crate::tui::event_loop::AgentGuideTarget,
    ) {
        for view in &mut self.view_stack {
            view.refresh_agent_guide_recipient(target, intent_id, recipient);
        }
    }

    pub(crate) fn remove_detached_agent_guide(
        &mut self,
        intent_id: &str,
        target: &crate::tui::event_loop::AgentGuideTarget,
    ) -> bool {
        let Some(index) = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && intent.status != astra_turn_types::UserIntentStatus::Returned
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target: owner }
                    if owner.same_execution(target))
        }) else {
            return false;
        };
        self.pending_user_intents.remove(index);
        true
    }

    pub(crate) fn mark_agent_guide_returned(
        &mut self,
        intent_id: &str,
        target: &crate::tui::event_loop::AgentGuideTarget,
    ) -> bool {
        let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target: owner }
                    if owner.same_execution(target))
        }) else {
            return false;
        };
        if intent.status == astra_turn_types::UserIntentStatus::Returned {
            return false;
        }
        intent.status = astra_turn_types::UserIntentStatus::Returned;
        intent.custody = PendingUserIntentCustody::Run;
        true
    }

    pub(crate) fn mark_agent_guide_rejected(
        &mut self,
        intent_id: &str,
        target: &crate::tui::event_loop::AgentGuideTarget,
    ) -> bool {
        if !self.pending_user_intents.iter().any(|intent| {
            intent.intent_id == intent_id
                && intent.status == astra_turn_types::UserIntentStatus::AcceptedLocal
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target: owner }
                    if owner.same_execution(target))
        }) {
            return false;
        }
        self.mark_agent_guide_returned(intent_id, target)
    }

    pub(crate) fn returned_agent_guide(
        &self,
        session_id: Option<&str>,
        attachment_epoch: u64,
        intent_id: Option<&str>,
    ) -> Option<&PendingUserIntent> {
        self.pending_user_intents.iter().find(|intent| {
            (intent.status == astra_turn_types::UserIntentStatus::Returned
                || intent.custody == PendingUserIntentCustody::Unconfirmed)
                && intent_id.is_none_or(|id| id == intent.intent_id)
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target }
                    if target.can_view_draft(session_id, attachment_epoch))
        })
    }

    pub(crate) fn take_returned_agent_guide(
        &mut self,
        intent_id: &str,
        target: &crate::tui::event_loop::AgentGuideTarget,
        session_id: Option<&str>,
        attachment_epoch: u64,
    ) -> Option<PendingUserIntent> {
        let index = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && (intent.status == astra_turn_types::UserIntentStatus::Returned
                    || intent.custody == PendingUserIntentCustody::Unconfirmed)
                && matches!(&intent.target, PendingUserIntentTarget::AgentRun { target: owner }
                    if owner.same_execution(target) && owner.can_view_draft(session_id, attachment_epoch))
        })?;
        self.pending_user_intents.remove(index)
    }

    pub(crate) fn restore_returned_agent_guide(&mut self, intent: PendingUserIntent) {
        self.pending_user_intents.push_back(intent);
    }

    /// Save a revised retry without changing the original delivery fact.
    pub(crate) fn save_agent_retry_draft(
        &self,
        intent: &mut PendingUserIntent,
        draft: &str,
    ) -> bool {
        let PendingUserIntentTarget::AgentRun { target } = &intent.target else {
            return false;
        };
        let other_bytes = self
            .pending_user_intents
            .iter()
            .filter(|pending| {
                matches!(&pending.target, PendingUserIntentTarget::AgentRun { target: other }
                    if other.same_session(target))
            })
            .map(PendingUserIntent::retained_bytes)
            .fold(0usize, usize::saturating_add);
        let original_bytes = intent
            .text
            .len()
            .saturating_add(intent.wire_text.as_ref().map_or(0, String::len));
        let revised_bytes = if draft == intent.text { 0 } else { draft.len() };
        if other_bytes
            .saturating_add(original_bytes)
            .saturating_add(revised_bytes)
            > MAX_PENDING_BYTES
        {
            return false;
        }
        intent.retry_draft = (draft != intent.text).then(|| draft.to_owned());
        true
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
            if matches!(
                self.pending_user_intents[index].target,
                PendingUserIntentTarget::AgentRun { .. }
            ) {
                return None;
            }
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
            wire_text: None,
            retry_draft: None,
            target,
            turn_id: None,
            next_turn_position: 0,
            custody: PendingUserIntentCustody::Run,
            root_scope: None,
        })
    }

    /// A late disposition must not be reduced into a different physical turn.
    pub(crate) fn user_intent_belongs_to_other_turn(&self, intent_id: &str, turn_id: &str) -> bool {
        self.pending_user_intents.iter().any(|intent| {
            intent.intent_id == intent_id
                && intent
                    .turn_id
                    .as_deref()
                    .is_some_and(|owner| owner != turn_id)
        })
    }

    pub(crate) fn root_intent_is_visible(&self, intent_id: &str) -> bool {
        self.visible_pending_intents().any(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.root_scope.as_ref().is_none_or(|scope| {
                    self.session_attachment
                        .as_ref()
                        .is_some_and(|(session_id, epoch)| {
                            scope.attached_in(session_id.as_deref(), *epoch)
                        })
                })
        })
    }

    /// Settle an old owner's disposition without projecting it into the
    /// currently attached conversation. A returned message stays recoverable.
    pub(crate) fn settle_detached_root_intent(&mut self, intent_id: &str, returned: bool) -> bool {
        let Some(index) = self.pending_user_intents.iter().position(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.root_scope.as_ref().is_some_and(|scope| {
                    !self
                        .session_attachment
                        .as_ref()
                        .is_some_and(|(session_id, epoch)| {
                            scope.attached_in(session_id.as_deref(), *epoch)
                        })
                })
        }) else {
            return false;
        };
        if returned {
            self.pending_user_intents[index].status = astra_turn_types::UserIntentStatus::Returned;
            self.pending_user_intents[index].custody = PendingUserIntentCustody::Run;
        } else {
            self.pending_user_intents.remove(index);
            self.applied_user_intent_ids.insert(intent_id.to_owned());
        }
        true
    }

    pub(crate) fn retain_unsent_detached_turn(&mut self, turn_id: &str) {
        for intent in &mut self.pending_user_intents {
            if intent.turn_id.as_deref() == Some(turn_id)
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Client
            {
                intent.status = astra_turn_types::UserIntentStatus::Returned;
                intent.custody = PendingUserIntentCustody::Run;
            }
        }
    }

    pub(crate) fn retain_unsent_detached_plan(&mut self, intent_id: &str) {
        if let Some(intent) = self.pending_user_intents.iter_mut().find(|intent| {
            intent.intent_id == intent_id
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Client
        }) {
            intent.status = astra_turn_types::UserIntentStatus::Returned;
            intent.custody = PendingUserIntentCustody::Run;
        }
    }

    pub(crate) fn recover_returned_root_guidance(&mut self) -> bool {
        let Some(index) = self.pending_user_intents.iter().position(|intent| {
            self.intent_is_visible(intent)
                && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.status == astra_turn_types::UserIntentStatus::Returned
        }) else {
            return false;
        };
        let Some(intent) = self.pending_user_intents.remove(index) else {
            return false;
        };
        self.start_composer_draft(&intent.composer_text());
        true
    }

    pub(crate) fn unconfirmed_root_guidance(
        &self,
        intent_id: Option<&str>,
    ) -> Option<&PendingUserIntent> {
        self.visible_pending_intents().find(|intent| {
            matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Unconfirmed
                && intent_id.is_none_or(|id| intent.intent_id == id)
        })
    }

    pub(crate) fn can_restore_root_guidance(&self, intent: &PendingUserIntent) -> bool {
        intent.root_scope.as_ref().is_none_or(|scope| {
            scope.owner.is_current()
                && self
                    .session_attachment
                    .as_ref()
                    .is_some_and(|(session_id, epoch)| {
                        scope.visible_in(session_id.as_deref(), *epoch)
                    })
        })
    }

    pub(crate) fn retain_returned_root_guidance(&mut self, mut intent: PendingUserIntent) {
        intent.status = astra_turn_types::UserIntentStatus::Returned;
        intent.custody = PendingUserIntentCustody::Run;
        self.pending_user_intents.push_back(intent);
    }

    pub(crate) fn dismiss_unconfirmed_root_guidance(&mut self, intent_id: &str) -> bool {
        if self.unconfirmed_root_guidance(Some(intent_id)).is_none() {
            return false;
        }
        let Some(index) = self
            .pending_user_intents
            .iter()
            .position(|intent| intent.intent_id == intent_id)
        else {
            return false;
        };
        self.pending_user_intents.remove(index);
        true
    }

    /// Consume a durable ownership return. Only the client that still owns a
    /// matching pending identity restores the content; other attached clients
    /// must not duplicate it into their composer.
    pub fn return_user_intent(
        &mut self,
        intent_id: &str,
        status: astra_turn_types::UserIntentStatus,
        runtime_content: &str,
    ) -> bool {
        if intent_id.trim().is_empty()
            || status != astra_turn_types::UserIntentStatus::Returned
            || self.applied_user_intent_ids.contains(intent_id)
        {
            return false;
        }
        let Some(index) = self.pending_user_intents.iter().position(|pending| {
            pending.intent_id == intent_id
                && matches!(pending.target, PendingUserIntentTarget::ActiveRun)
        }) else {
            return false;
        };
        if self.pending_user_intents[index].root_scope.is_some() {
            self.pending_user_intents[index].status = astra_turn_types::UserIntentStatus::Returned;
            self.pending_user_intents[index].custody = PendingUserIntentCustody::Run;
            self.applied_user_intent_ids.insert(intent_id.to_string());
            return true;
        }
        let intent = self
            .pending_user_intents
            .remove(index)
            .expect("matched intent");
        self.applied_user_intent_ids.insert(intent_id.to_string());
        let restored = if intent.wire_text.is_some() {
            std::borrow::Cow::Borrowed(intent.text.as_str())
        } else {
            super::slash_dispatch::conversation_for_composer(runtime_content)
        };
        self.restore_into_composer(&restored);
        true
    }

    /// Return unapplied input for editing, without authorizing a new submission.
    pub fn restore_into_composer(&mut self, text: &str) {
        self.restore_inputs_into_composer(std::iter::once(text));
    }

    /// Merge only adjacent conversation fragments. Commands and shell drafts
    /// retain their own submission boundary and require a fresh gesture each.
    pub(crate) fn restore_inputs_into_composer(
        &mut self,
        inputs: impl IntoIterator<Item = impl AsRef<str>>,
    ) {
        for input in inputs {
            let text = input.as_ref();
            if !text.trim().is_empty() {
                let draft = if self.composer.is_history_search_active() {
                    UnsentDraft::Parked(text.to_owned())
                } else {
                    UnsentDraft::Returned(text.to_owned())
                };
                self.restored_drafts.push_back(draft);
            }
        }
        self.restore_next_draft();
    }

    fn restore_next_draft(&mut self) -> bool {
        use super::slash_dispatch::parse_submission;
        let conversation = |text: &str| {
            !text.trim().is_empty()
                && !parse_submission(text).is_command()
                && crate::cli::turn::turn_entry::shell_input_body(text).is_none()
        };
        if self.restored_drafts.is_empty()
            || self.composer.paste_burst_active()
            || self.composer.is_history_search_active()
        {
            return false;
        }
        let mut raw = self.composer.text();
        let had_visible_draft = !raw.is_empty();
        let mut changed = false;
        let compatible = if !had_visible_draft {
            raw = self
                .restored_drafts
                .pop_front()
                .expect("nonempty drafts")
                .into_text();
            changed = true;
            conversation(&raw)
        } else {
            conversation(&self.composer.expanded_raw_text())
        };
        if compatible {
            // Keep the first fragment's composer encoding and paste placeholders.
            // Only later fragments lose their leading literal escape; they no
            // longer occupy a command boundary in the combined conversation.
            while self.restored_drafts.front().is_some_and(|draft| {
                (!had_visible_draft || matches!(draft, UnsentDraft::Returned(_)))
                    && conversation(draft.text())
            }) {
                let next = self.restored_drafts.pop_front().expect("checked front");
                raw.push_str("\n\n");
                raw.push_str(parse_submission(next.text()).text());
                changed = true;
            }
        }
        if changed {
            self.replace_composer_text(&raw);
        }
        changed
    }

    /// Drain active-run intents when the run settles.
    ///
    /// Only locally accepted intents are returned to the caller for recovery.
    /// An `AcceptedRemote` acknowledgement transfers delivery ownership to the
    /// server; replaying it as a new turn merely because the local `Applied`
    /// projection raced with run settlement violates exactly-once user intent.
    fn take_client_recoverable_for_turn(
        &mut self,
        turn_id: Option<&str>,
    ) -> Vec<PendingUserIntent> {
        let mut locally_owned = Vec::new();
        let mut retained = std::collections::VecDeque::new();
        while let Some(mut intent) = self.pending_user_intents.pop_front() {
            let visible_here = intent.root_scope.as_ref().is_none_or(|scope| {
                self.session_attachment
                    .as_ref()
                    .is_some_and(|(session_id, epoch)| {
                        scope.attached_in(session_id.as_deref(), *epoch)
                    })
            });
            match (&intent.target, intent.custody, intent.status) {
                (
                    PendingUserIntentTarget::ActiveRun,
                    PendingUserIntentCustody::Client,
                    astra_turn_types::UserIntentStatus::AcceptedLocal,
                ) if intent.turn_id.as_deref() == turn_id => {
                    if visible_here {
                        locally_owned.push(intent);
                    } else {
                        // Settlement has already resolved any in-flight POST;
                        // only definitely unsent local messages reach here.
                        intent.status = astra_turn_types::UserIntentStatus::Returned;
                        intent.custody = PendingUserIntentCustody::Run;
                        retained.push_back(intent);
                    }
                }
                (
                    PendingUserIntentTarget::ActiveRun,
                    PendingUserIntentCustody::Run | PendingUserIntentCustody::Unconfirmed,
                    astra_turn_types::UserIntentStatus::AcceptedRemote,
                )
                | (
                    PendingUserIntentTarget::ActiveRun,
                    PendingUserIntentCustody::Unconfirmed,
                    astra_turn_types::UserIntentStatus::AcceptedLocal,
                ) => {
                    // The remote run owns delivery, but terminal cancellation
                    // can beat its Applied acknowledgement. Retain the stable
                    // identity as visible unresolved state; never turn it into
                    // a second canonical submission automatically.
                    retained.push_back(intent);
                }
                (
                    PendingUserIntentTarget::ActiveRun,
                    _,
                    astra_turn_types::UserIntentStatus::Applied,
                ) => {}
                (
                    PendingUserIntentTarget::ActiveRun,
                    _,
                    astra_turn_types::UserIntentStatus::Returned,
                ) => retained.push_back(intent),
                (PendingUserIntentTarget::AgentRun { .. }, _, _) => retained.push_back(intent),
                // Construction rejects every other status/custody pairing.
                (PendingUserIntentTarget::ActiveRun, _, _) => retained.push_back(intent),
            }
        }
        self.pending_user_intents = retained;
        locally_owned
    }

    pub(crate) fn finish_turn_user_intents(&mut self, turn_id: &str) -> Vec<PendingUserIntent> {
        self.applied_user_intent_ids.clear();
        self.take_client_recoverable_for_turn(Some(turn_id))
    }

    #[cfg(test)]
    pub fn take_client_recoverable_user_intents(&mut self) -> Vec<PendingUserIntent> {
        self.applied_user_intent_ids.clear();
        self.take_client_recoverable_for_turn(None)
    }

    /// Accept a message for the next safe conversational turn. The event loop
    /// transfers this FIFO lane to its ordinary submit path once the current
    /// run or Session lifecycle action has reached a safe boundary.
    pub fn queue_next_turn_submission(&mut self, text: String) -> bool {
        if text.trim().is_empty() {
            return false;
        }
        self.queued_next_turn_submissions
            .push_back(NextTurnSubmission::Interactive(text));
        true
    }

    pub(crate) fn queue_session_continuation(
        &mut self,
        target: crate::tui::event_loop::SessionContinuationTarget,
        content: String,
    ) -> bool {
        if content.trim().is_empty() {
            return false;
        }
        self.queued_next_turn_submissions
            .push_back(NextTurnSubmission::Conversation { target, content });
        true
    }

    pub(crate) fn queued_next_turn_submission_count(&self) -> usize {
        self.queued_next_turn_submissions.len()
    }

    /// Insert at the position this text had when the user submitted it.
    /// Entries queued later stay behind it. An index past the end appends.
    pub(crate) fn insert_next_turn_guidance(
        &mut self,
        index: usize,
        intent: PendingUserIntent,
    ) -> bool {
        if intent.text.trim().is_empty() {
            return false;
        }
        let index = index.min(self.queued_next_turn_submissions.len());
        self.queued_next_turn_submissions
            .insert(index, NextTurnSubmission::Guidance(intent));
        for intent in &mut self.pending_user_intents {
            if matches!(intent.target, PendingUserIntentTarget::ActiveRun)
                && intent.custody == PendingUserIntentCustody::Client
                && intent.next_turn_position >= index
            {
                intent.next_turn_position += 1;
            }
        }
        true
    }

    pub(crate) fn take_queued_next_turn_submissions(
        &mut self,
    ) -> std::collections::VecDeque<NextTurnSubmission> {
        std::mem::take(&mut self.queued_next_turn_submissions)
    }

    #[cfg(test)]
    pub(crate) fn pending_user_intent_count(&self) -> usize {
        self.pending_user_intents.len()
    }

    #[cfg(test)]
    fn has_pending_user_intents(&self) -> bool {
        !self.pending_user_intents.is_empty()
    }

    fn has_pending_composer_queue(&self) -> bool {
        self.visible_pending_intents()
            .any(|intent| matches!(intent.target, PendingUserIntentTarget::ActiveRun))
            || self.visible_next_turn_submissions().next().is_some()
    }

    pub fn set_task_status(&mut self, status: TaskStatus) {
        // Cancellation is a monotonic foreground projection. Provider/tool
        // events already queued before Ctrl+C may still arrive, but they do
        // not revoke the user's stop intent. Only terminal settlement may
        // return the pane to Idle.
        if matches!(
            self.task_status,
            TaskStatus::Cancelling | TaskStatus::Exiting
        ) && !matches!(
            status,
            TaskStatus::Cancelling | TaskStatus::Exiting | TaskStatus::Idle
        ) {
            return;
        }
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

    /// A delayed control receipt must not steal focus from another form.
    pub(crate) fn push_workbench_input(&mut self, view: Box<dyn BottomPaneView>) {
        if self.has_active_view() && !self.agent_monitor_is_open() {
            self.view_stack.insert(0, view);
        } else {
            self.push_view(view);
        }
    }

    pub(crate) fn current_model_catalog(&self) -> &[astra_services::ModelListItemResponse] {
        match &self.model_catalog {
            Some(ModelCatalogState::Ready(rows)) => rows,
            _ => &[],
        }
    }

    pub(crate) fn clear_model_catalog(&mut self) {
        self.model_catalog = None;
    }

    pub(crate) fn cache_model_catalog(&mut self, rows: Vec<astra_services::ModelListItemResponse>) {
        self.model_catalog = Some(ModelCatalogState::Ready(rows));
    }

    pub fn enqueue_ask_user(
        &mut self,
        prompt: crate::cli::chat_stream::AskUserPrompt,
        response_tx: oneshot::Sender<crate::cli::chat_stream::AskUserResponse>,
    ) {
        if response_tx.is_closed() {
            return;
        }
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
        self.active_view()
            .is_some_and(|view| view.accepts_background_task_rows())
    }

    pub fn refresh_agent_monitor(
        &mut self,
        snapshot: crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            let previous_tab = view.conversation_tab_id();
            refreshed |= view.refresh_agent_monitor(snapshot.clone());
            reconcile_conversation_tab(
                &mut self.conversation_tab_order,
                previous_tab,
                view.conversation_tab_id(),
            );
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

    pub(crate) fn refresh_agent_transcript(
        &mut self,
        update: agent_transcript_view::AgentTranscriptUpdate,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_agent_transcript(update.clone());
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
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
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
        }
        refreshed
    }

    /// Keep a durable root transcript workspace live while its next canonical
    /// page is still being written. The view itself labels this as local
    /// evidence and never merges it into durable history by presentation text.
    pub(crate) fn refresh_root_transcript_live(
        &mut self,
        binding: Option<(&str, &str)>,
        item: Option<transcript_view::TranscriptItem>,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            refreshed |= view.refresh_root_transcript_live(binding, item.clone());
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
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
            let previous_tab = view.conversation_tab_id();
            refreshed |= view.refresh_agent_live_event(event);
            reconcile_conversation_tab(
                &mut self.conversation_tab_order,
                previous_tab,
                view.conversation_tab_id(),
            );
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
        }
        refreshed
    }

    pub(crate) fn refresh_agent_live_events(
        &mut self,
        events: &[astra_turn_core::agent_live_event::AgentLiveEvent],
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            let previous_tab = view.conversation_tab_id();
            refreshed |= view.refresh_agent_live_events(events);
            reconcile_conversation_tab(
                &mut self.conversation_tab_order,
                previous_tab,
                view.conversation_tab_id(),
            );
            if let Some(request) = view.take_action_request()
                && request.disposition == ViewActionDisposition::KeepOpen
            {
                self.projection_actions.push_back(request.action);
            }
        }
        refreshed
    }

    pub(crate) fn refresh_agent_live_gap(
        &mut self,
        gap: &astra_turn_core::agent_live_event::AgentLiveGap,
    ) -> bool {
        let mut refreshed = false;
        for view in self.view_stack.iter_mut().rev() {
            let previous_tab = view.conversation_tab_id();
            refreshed |= view.refresh_agent_live_gap(gap);
            reconcile_conversation_tab(
                &mut self.conversation_tab_order,
                previous_tab,
                view.conversation_tab_id(),
            );
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
        self.active_view()
            .is_some_and(|view| view.accepts_agent_rows())
    }

    /// A conversation tab can cover the navigator, but the retained tree must
    /// continue receiving truth updates so returning to it never displays an
    /// old run state.
    pub(crate) fn has_agent_monitor(&self) -> bool {
        self.view_stack.iter().any(|view| view.accepts_agent_rows())
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
        self.active_view_index().is_some()
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

    pub(crate) fn has_agent_transcript_tab(&self) -> bool {
        self.view_stack.iter().any(|view| {
            matches!(
                view.conversation_tab_id(),
                Some(view::ConversationTabId::Run { .. })
            )
        })
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

    /// Fit a focused transcript or report to its actual content area.
    pub(crate) fn prepare_conversation_workspace(&mut self, terminal_height: u16, width: u16) {
        let has_approvals = self.has_pending_approvals();
        if let Some(view) = self.active_view_mut()
            && view.is_transcript_view()
        {
            let (approval, hint, status) = visible_view_footers(
                terminal_height,
                view.conversation_tab_id().is_some(),
                has_approvals,
                view.hint_keys().is_some(),
                view.reserve_status_footer(),
            );
            let content_height = terminal_height
                .saturating_sub(u16::from(approval) + u16::from(hint) + u16::from(status));
            view.fit_conversation_workspace(content_height, width);
        }
    }

    /// Leave enough space for transcript content and its currently requested
    /// footer rows before spending height on optional workspace chrome.
    pub(crate) fn conversation_workspace_min_height(&self) -> u16 {
        self.active_view().map_or(1, |view| {
            if view.conversation_tab_id().is_none() {
                return 1;
            }
            3 + u16::from(self.has_pending_approvals())
                + u16::from(view.hint_keys().is_some())
                + u16::from(view.reserve_status_footer())
        })
    }

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
        let Some(index) = self.view_stack.iter().rposition(|view| {
            view.conversation_tab_id().as_ref() == Some(&ConversationTabId::Root)
        }) else {
            return false;
        };
        if self.view_stack[index].durable_root_transcript_session() == Some(session_id.as_str()) {
            return false;
        }
        self.view_stack[index] = Box::new(root_transcript_view::RootTranscriptView::loading(
            session_id,
            width,
            terminal_height,
        ));
        true
    }

    pub(crate) fn activate_agent_transcript(&mut self, agent_id: &str, run_id: &str) -> bool {
        if !run_id.is_empty() && let Some(tab_id) = self.view_stack.iter().find_map(|view| {
            let tab_id = view.conversation_tab_id()?;
            matches!(&tab_id, ConversationTabId::Run { run_id: open_run, .. } if open_run == run_id)
                .then_some(tab_id)
        }) {
            return self.activate_conversation_tab(&tab_id);
        }
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
        self.pop_active_view_at(std::time::Instant::now())
    }

    fn pop_active_view_at(&mut self, now: std::time::Instant) -> Option<Box<dyn BottomPaneView>> {
        let mut view = self.view_stack.remove(self.active_view_index()?);
        if let Some(tail) = view.pending_paste_tail_at(now) {
            self.composer.discard_paste_tail_at(now, tail);
        }
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

    fn active_view_index(&self) -> Option<usize> {
        self.view_stack.iter().rposition(|view| {
            self.session_attachment
                .as_ref()
                .is_none_or(|(session, epoch)| {
                    view.visible_in_attachment(session.as_deref(), *epoch)
                })
        })
    }

    fn active_view(&self) -> Option<&dyn BottomPaneView> {
        self.active_view_index()
            .map(|index| &*self.view_stack[index])
    }

    fn active_view_mut(&mut self) -> Option<&mut Box<dyn BottomPaneView>> {
        let index = self.active_view_index()?;
        self.view_stack.get_mut(index)
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

    pub(super) fn has_composer_hints(&self, width: u16) -> bool {
        width >= 34
            && !self.has_active_view()
            && (!self.composer.is_empty() || !self.has_pending_approvals())
            && self.mention_menu.is_none()
            && self.skill_popup.is_none()
    }

    fn composer_hints(&self) -> &'static str {
        if let Some(menu) = &self.slash_menu {
            if menu.is_empty() {
                "Esc close menu"
            } else if matches!(
                super::slash_dispatch::parse_submission(&self.composer.text()),
                super::slash_dispatch::SubmissionInput::Command(_)
            ) {
                "↑↓ select · Tab insert · Enter run · Esc close menu"
            } else {
                "↑↓ select · Tab insert · Esc close menu"
            }
        } else if matches!(
            self.task_status,
            TaskStatus::Cancelling | TaskStatus::Exiting
        ) {
            "Ctrl+G agents · Ctrl+O transcript · Shift+↓ background"
        } else if self.task_status.is_active() {
            "Enter guide · Ctrl+Q queue · Ctrl+G agents · Ctrl+O transcript · Ctrl+T tasks · Shift+Enter newline"
        } else {
            "Ctrl+G agents · Ctrl+O transcript · Ctrl+T tasks · / commands · Shift+Enter newline"
        }
    }

    pub fn sync_popups(&mut self) {
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

        let text = self.composer.text();

        // Completion owns command names, never their arguments. Otherwise
        // selecting a menu item would discard the user's remaining input.
        if !self.has_active_view()
            && is_open_for(&text, &self.slash_items)
            && !self.slash_items.is_empty()
        {
            self.composer.dismiss_history_suggestions();
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
        if !self.has_active_view() && self.file_provider.is_some() {
            let cursor = self.composer.cursor_byte();
            if let Some(token) = extract_mention_at(&text, cursor) {
                self.composer.dismiss_history_suggestions();
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

        if !self.has_active_view()
            && text.starts_with('$')
            && !text.chars().any(char::is_whitespace)
            && !self.skill_items.is_empty()
        {
            self.composer.dismiss_history_suggestions();
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

    pub(crate) fn stage_permission_mode_for_next_round(
        &mut self,
        mode: crate::cli::permission_manager::PermissionMode,
    ) {
        self.staged_permission_mode = Some(mode);
        self.footer.set_pending_permission_mode(mode);
    }

    /// Clear a pending selection after acknowledgement or response settlement.
    pub(crate) fn clear_staged_permission_mode(&mut self) {
        self.staged_permission_mode = None;
        self.footer.clear_pending_permission_mode();
    }

    /// Peek at the requested policy without consuming it.
    /// Repeated runtime shortcuts use this value as their cycle cursor while
    /// the currently executing round keeps its captured policy.
    pub(crate) fn staged_permission_mode(
        &self,
    ) -> Option<crate::cli::permission_manager::PermissionMode> {
        self.staged_permission_mode
    }

    pub(crate) fn take_staged_permission_mode(
        &mut self,
    ) -> Option<crate::cli::permission_manager::PermissionMode> {
        let mode = self.staged_permission_mode.take();
        self.footer.clear_pending_permission_mode();
        mode
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
            if entry.args.is_null() || entry.runtime_dependencies.is_some() {
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

    /// Match the existing Ctrl+C routing before advertising an interrupt.
    pub(crate) fn can_interrupt_turn(&self) -> bool {
        !self.has_active_view() && self.composer.is_empty() && self.task_status.is_active()
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
        cell.invocation_scoped = view.invocation_scoped;
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

    pub(crate) fn set_composer_frame_allowed(&mut self, allowed: bool) {
        self.composer.set_frame_allowed(allowed);
    }

    pub(crate) fn history_suggestions_height(&self) -> u16 {
        if self.has_active_view() {
            return 0;
        }
        self.composer.history_suggestions_height()
    }

    pub(crate) fn extra_picker_rows(&self) -> u16 {
        if self.has_active_view() {
            0
        } else {
            self.popup_height().saturating_sub(1) + self.history_suggestions_height()
        }
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
        let next_turn_queue_h = self.next_turn_submission_height();
        let popup_h = self.popup_height();
        // One quiet row above the composer is reserved for the context rail.
        // Keeping it stable avoids shifting the input when the first usage
        // measurement arrives; before that it simply acts as breathing room.
        content_h
            + approval_h
            + queue_h
            + next_turn_queue_h
            + popup_h
            + self.history_suggestions_height()
            + 2
            + u16::from(self.has_composer_hints(width))
            + self.notice_lines(width).len() as u16
    }

    /// The composer and its cursor must use the same constrained rectangles,
    /// including when the terminal cannot fit every queue and popup row.
    fn composer_areas(&self, area: Rect) -> [Rect; 8] {
        let mut remaining = area.height;
        let mut take = |desired: u16| {
            let height = desired.min(remaining);
            remaining -= height;
            height
        };
        // Keep an insertion point, then the focused approval. Optional queues,
        // chrome and menus cannot consume the only editable row on a small
        // terminal. Both paint and cursor positioning use these same areas.
        let mut composer = take(1);
        let approval = take(self.focused_approval_height(area.width));
        composer += take(
            self.composer
                .desired_height(area.width)
                .saturating_sub(composer),
        );
        let popup_rows = self.popup_height();
        let popup = take(popup_rows.min(1));
        let queue = take(self.user_intent_height());
        let next_turn_queue = take(self.next_turn_submission_height());
        let context = take(1);
        let footer = take(1);
        // Ordinary history suggestions yield to the queue and status rows.
        let popup = popup
            + take(popup_rows.saturating_sub(popup))
            + take(self.history_suggestions_height());
        let hints = take(u16::from(self.has_composer_hints(area.width)));
        let mut y = area.y;
        let mut row = |height| {
            let rect = Rect::new(area.x, y, area.width, height);
            y += height;
            rect
        };
        [
            row(approval),
            row(queue),
            row(next_turn_queue),
            row(context),
            row(composer),
            row(popup),
            row(footer),
            row(hints),
        ]
    }

    // Ctrl+R stays inside the composer; it never opens a view stack overlay.

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
        self.handle_key_at(key, std::time::Instant::now())
    }

    pub(crate) fn handle_key_at(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
    ) -> BottomPaneAction {
        self.handle_session_key_at(key, now, &[])
    }

    pub(crate) fn handle_session_key_at(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
        history: &[Arc<dyn crate::tui::history_cell::HistoryCell>],
    ) -> BottomPaneAction {
        self.dismiss_closed_views_at(now);
        if self.composer.is_empty() {
            self.restore_next_draft();
        }
        let ordinary_char = super::event::is_text_key(&key);
        let paste_control =
            key.modifiers.is_empty() && matches!(key.code, KeyCode::Enter | KeyCode::Tab);
        if ordinary_char {
            self.composer.flush_paste_burst_at(now);
            match self.composer.plain_paste_input_at(now) {
                Some(paste_burst::GuardedInput::Discard) => return BottomPaneAction::Consumed,
                Some(paste_burst::GuardedInput::Insert) if self.has_active_view() => {
                    return self.route_to_composer(key, now, history);
                }
                None if self.has_active_view() && self.composer.release_paste_focus_at(now) => {
                    self.sync_popups();
                }
                _ => {}
            }
        }
        if self.has_active_view() && paste_control {
            self.composer.flush_paste_burst_at(now);
            if self.composer.paste_input_pending_at(now) {
                let consumed = match key.code {
                    KeyCode::Enter => self.composer.consume_guarded_enter_at(now),
                    KeyCode::Tab => self.composer.consume_guarded_tab_at(now),
                    _ => false,
                };
                if consumed {
                    return BottomPaneAction::Consumed;
                }
            }
        }
        let cancel = key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL));
        if !self.has_active_view()
            && !ordinary_char
            && !cancel
            && !chat_composer::ChatComposer::is_explicit_followup_key(key)
            && self.composer.flush_pending_paste_at(now)
        {
            self.sync_popups();
            self.refresh_history_suggestions(history);
        }
        let action = self.handle_key_inner(key, now, history);
        if self.composer.is_empty() {
            self.restore_next_draft();
        }
        action
    }

    fn handle_key_inner(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
        history: &[Arc<dyn crate::tui::history_cell::HistoryCell>],
    ) -> BottomPaneAction {
        if let Some(a) = self.handle_ctrl_keys(key, now) {
            return a;
        }
        // A slash is an explicit command-palette gesture. Let it reclaim
        // focus from a completed picker (for example `/work`'s catalog) so a
        // user can immediately type `/model` or any other command without
        // first guessing which close key the overlay expects. The picker is
        // already a read-only action; closing it does not mutate Work.
        if matches!(key.code, KeyCode::Char('/'))
            && key.modifiers.is_empty()
            && self
                .active_view()
                .is_some_and(|view| view.slash_reclaims_focus())
        {
            self.pop_active_view_at(now);
            self.start_composer_draft("/");
            return BottomPaneAction::Consumed;
        }
        if let Some(a) = self.handle_active_view_key(key, now) {
            return a;
        }
        if key.code == KeyCode::Char('r')
            && key.modifiers == KeyModifiers::CONTROL
            && !self.composer.is_history_search_active()
        {
            self.composer.start_history_search(now, history);
            self.sync_popups();
            return BottomPaneAction::Consumed;
        }
        if key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::ALT {
            return BottomPaneAction::ViewAction(BottomPaneViewAction::OpenAppliedRootGuidance {
                after_cell_id: None,
            });
        }
        if key.code == KeyCode::Char('u') && key.modifiers == KeyModifiers::ALT {
            if self.visible_pending_intents().any(|intent| {
                intent.status == astra_turn_types::UserIntentStatus::Returned
                    && matches!(intent.target, PendingUserIntentTarget::ActiveRun)
            }) {
                return BottomPaneAction::ViewAction(
                    BottomPaneViewAction::RecoverReturnedRootGuidance,
                );
            }
            if self.unconfirmed_root_guidance(None).is_some() {
                return BottomPaneAction::ViewAction(
                    BottomPaneViewAction::OpenUnconfirmedRootGuidance,
                );
            }
            if self.visible_pending_intents().any(|intent| {
                (intent.status == astra_turn_types::UserIntentStatus::Returned
                    || intent.custody == PendingUserIntentCustody::Unconfirmed)
                    && matches!(intent.target, PendingUserIntentTarget::AgentRun { .. })
            }) {
                return BottomPaneAction::ViewAction(BottomPaneViewAction::OpenReturnedAgentGuide);
            }
        }
        // A trailing pasted newline belongs to editing, before a popup or
        // approval can interpret Enter. The textarea consumes it atomically.
        if key.code == KeyCode::Enter
            && key.modifiers.is_empty()
            && self.composer.consume_guarded_enter_at(now)
        {
            self.sync_popups();
            return BottomPaneAction::Consumed;
        }
        if key.code == KeyCode::Tab
            && key.modifiers.is_empty()
            && self.composer.consume_guarded_tab_at(now)
        {
            self.sync_popups();
            return BottomPaneAction::Consumed;
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
        if key.code == KeyCode::Esc
            && !self.composer.is_empty()
            && !self.composer.is_history_search_active()
        {
            // A burst hides the preview. Esc cancels the pending input,
            // not a suggestion the user cannot see.
            if self.composer.was_buffering_at(now) || !self.composer.dismiss_history_suggestions() {
                self.composer.clear_draft_at(now);
                self.sync_popups();
            }
            return BottomPaneAction::Consumed;
        }
        if let Some(a) = self.handle_slash_menu_key(key, now) {
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
        self.route_to_composer(key, now, history)
    }

    fn handle_ctrl_keys(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
    ) -> Option<BottomPaneAction> {
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
                            self.pop_active_view_at(now);
                        }
                        return Some(BottomPaneAction::Consumed);
                    }
                    CancellationEvent::Escalate => {}
                }
            }
            if self.composer.cancel_history_search_at(now) {
                return Some(BottomPaneAction::Consumed);
            }
            if !self.composer.is_empty() {
                self.composer.clear_draft_at(now);
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
            if self.has_active_view() {
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
            || self.has_active_view()
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
                    && !self.has_active_view() =>
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
            _ => None,
        }
    }

    fn handle_active_view_key(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
    ) -> Option<BottomPaneAction> {
        let view = self.active_view_mut()?;
        view.handle_key_at(key, now);
        if let Some(request) = view.take_action_request() {
            if request.disposition == ViewActionDisposition::Close {
                self.pop_active_view_at(now);
            }
            return Some(BottomPaneAction::ViewAction(request.action));
        }
        if view.is_complete() {
            let completion = view.completion();
            self.pop_active_view_at(now);
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

    /// Slash menu navigation. Submit keys with no matches fall through to
    /// the composer so the raw draft gets submitted as-is (avoids a
    /// silent no-op).
    fn handle_slash_menu_key(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
    ) -> Option<BottomPaneAction> {
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
                // Numbered selection is a shortcut for the *bare* palette.
                // In a typed slash path, digits belong to the composer.
                if self.composer.text() != "/" {
                    return None;
                }
                // An out-of-range number is still a palette shortcut:
                // ignore it without appending the digit to the draft.
                let Some(index) = digit.to_digit(10).filter(|index| *index > 0) else {
                    return Some(BottomPaneAction::Consumed);
                };
                if !self
                    .slash_menu
                    .as_mut()
                    .is_some_and(|menu| menu.select(index as usize - 1))
                {
                    return Some(BottomPaneAction::Consumed);
                }
                if let Some(picked) = self
                    .slash_menu
                    .as_ref()
                    .and_then(|m| m.selected_item())
                    .map(|i| i.name.to_string())
                {
                    self.composer.set_text(&format!("{picked} "));
                    self.slash_menu = None;
                    Some(BottomPaneAction::Consumed)
                } else {
                    None
                }
            }
            _ if (key.code == KeyCode::Enter && key.modifiers.is_empty())
                || ChatComposer::is_explicit_followup_key(key) =>
            {
                let explicit_followup = ChatComposer::is_explicit_followup_key(key);
                if explicit_followup {
                    // Apply the same paste guard as ordinary composer queueing.
                    // A due burst can change the completion or add arguments;
                    // refresh the menu before choosing a command from it.
                    if self.composer.handle_key_at(key, now) != ComposerAction::QueueFollowUp {
                        return Some(BottomPaneAction::Consumed);
                    }
                    self.sync_popups();
                }
                // Prospective fuzzy matches are suggestions, not implicit
                // commands. An unregistered path must reach the agent even
                // when the menu happens to highlight a similar command.
                if matches!(
                    crate::tui::slash_dispatch::parse_submission(&self.composer.text()),
                    crate::tui::slash_dispatch::SubmissionInput::Conversation(_)
                ) {
                    if explicit_followup {
                        let text = self.composer.clear_and_submit();
                        self.slash_menu = None;
                        return Some(BottomPaneAction::QueueFollowUp(text));
                    }
                    return None; // Let the composer submit the exact text.
                }
                if let Some(picked) = self
                    .slash_menu
                    .as_ref()
                    .and_then(|m| m.selected_item())
                    .map(|i| i.name.to_string())
                {
                    self.composer.clear_draft();
                    self.slash_menu = None;
                    return Some(if explicit_followup {
                        BottomPaneAction::QueueFollowUp(picked)
                    } else {
                        BottomPaneAction::SubmitInput(picked)
                    });
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
            KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
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

    fn route_to_composer(
        &mut self,
        key: KeyEvent,
        now: std::time::Instant,
        history: &[Arc<dyn crate::tui::history_cell::HistoryCell>],
    ) -> BottomPaneAction {
        let composer_action = self.composer.handle_key_at(key, now);
        let edited = composer_action == ComposerAction::Edited;
        let action = match composer_action {
            ComposerAction::Recall(drafts) => {
                self.park_drafts(drafts);
                BottomPaneAction::Consumed
            }
            ComposerAction::Submit | ComposerAction::QueueFollowUp => {
                let text = self.composer.clear_and_submit();
                self.slash_menu = None;
                self.close_mention();
                if composer_action == ComposerAction::QueueFollowUp {
                    BottomPaneAction::QueueFollowUp(text)
                } else {
                    BottomPaneAction::SubmitInput(text)
                }
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
            ComposerAction::Consumed | ComposerAction::Edited => BottomPaneAction::Consumed,
            ComposerAction::Unhandled => BottomPaneAction::Escalate(key),
        };
        self.sync_popups();
        if edited {
            self.refresh_history_suggestions(history);
        }
        action
    }

    fn refresh_history_suggestions(
        &mut self,
        history: &[Arc<dyn crate::tui::history_cell::HistoryCell>],
    ) {
        if !self.has_active_view()
            && self.slash_menu.is_none()
            && self.skill_popup.is_none()
            && self.mention_menu.is_none()
        {
            self.composer.refresh_history_suggestions(history);
        }
    }

    fn dismiss_closed_views_at(&mut self, now: std::time::Instant) -> bool {
        let mut changed = false;
        while self
            .active_view()
            .is_some_and(|view| view.is_complete() && view.completion().is_none())
        {
            self.pop_active_view_at(now);
            changed = true;
        }
        changed
    }

    #[cfg(test)]
    pub fn pre_draw_tick(&mut self, now: std::time::Instant) -> bool {
        self.pre_draw_session_tick(now, &[])
    }

    pub(crate) fn set_tips_enabled(&mut self, enabled: bool) {
        if enabled {
            self.tip_rotation
                .get_or_insert_with(|| (std::time::Instant::now(), 0));
        } else {
            self.tip_rotation = None;
        }
    }

    #[cfg(test)]
    pub(crate) fn enable_tips(&mut self, now: std::time::Instant) {
        self.tip_rotation = Some((now, 0));
    }

    fn tips_visible(&self) -> bool {
        self.composer.is_empty()
            && !self.has_active_view()
            && !self.has_pending_approvals()
            && !self.has_pending_composer_queue()
            && !self.interrupt_pending
            && !matches!(
                self.task_status,
                TaskStatus::Exiting | TaskStatus::Cancelling
            )
    }

    fn composer_tip(&self) -> Option<&'static str> {
        const IDLE: &[&str] = &[
            "Ctrl+R searches prompt history.",
            "Ctrl+O opens the transcript.",
            "/explain shows timing and tokens.",
            "Ctrl+G opens the run navigator.",
            "/context shows the request budget.",
            "/help keys lists the shortcuts.",
        ];
        const WORKING: &[&str] = &[
            "Enter guides this run. Ctrl+Q queues for the next turn.",
            "Ctrl+O opens the live transcript.",
            "Ctrl+G inspects active runs.",
        ];
        let (_, index) = self.tip_rotation?;
        if !self.tips_visible() {
            return None;
        }
        let working = self.task_status.is_active();
        let tips = if working { WORKING } else { IDLE };
        if !working
            && index.is_multiple_of(3)
            && self
                .footer
                .bg_task_counts
                .is_some_and(|counts| !counts.is_empty())
        {
            Some("Shift+↓ opens background tasks.")
        } else {
            Some(tips[(index % tips.len() as u64) as usize])
        }
    }

    pub(crate) fn pre_draw_session_tick(
        &mut self,
        now: std::time::Instant,
        history: &[Arc<dyn crate::tui::history_cell::HistoryCell>],
    ) -> bool {
        let mut changed = self.approval_queue.prune_closed() > 0;
        if let Some((started_at, index)) = &mut self.tip_rotation {
            let current = now.saturating_duration_since(*started_at).as_secs() / 24;
            if current != *index {
                *index = current;
                changed |= self.tips_visible();
            }
        }
        self.footer.pending_approvals = self.approval_queue.len();
        if let Some(view) = self.active_view_mut() {
            changed |= view.is_in_paste_burst();
            view.pre_draw_tick(now);
        }
        changed |= self.dismiss_closed_views_at(now);
        // Flush paste burst buffer when idle timeout expires.
        if self.composer.flush_paste_burst_at(now) {
            self.sync_popups();
            self.restore_next_draft();
            self.refresh_history_suggestions(history);
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

    /// A command/view starts another draft without discarding ongoing editing.
    pub(crate) fn start_composer_draft(&mut self, text: &str) {
        let drafts = self.composer.take_recoverable_drafts();
        self.park_drafts(drafts);
        self.replace_composer_text(text);
    }

    fn park_drafts(&mut self, drafts: Vec<String>) {
        for draft in drafts.into_iter().rev() {
            self.restored_drafts.push_front(UnsentDraft::Parked(draft));
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

    pub fn render(&self, area: Rect, buf: &mut Buffer) {
        self.composer.clear_painted_history_suggestions();
        if let Some(view) = self.active_view() {
            // Stack optional footers under the view area:
            //   [view]
            //   [status line]     — when reserve_status_footer()
            //   [hint bar]        — when hint_keys() is Some
            let want_status = view.reserve_status_footer();
            let hint = view.hint_keys();
            let hidden_approvals = self.approval_queue.len();
            let (show_approval, show_hint, show_status) = visible_view_footers(
                area.height,
                view.conversation_tab_id().is_some(),
                hidden_approvals > 0,
                hint.is_some(),
                want_status,
            );
            let footer_rows =
                u16::from(show_approval) + u16::from(show_hint) + u16::from(show_status);
            if footer_rows > 0 {
                let view_h = area.height - footer_rows;
                let view_rect = Rect::new(area.x, area.y, area.width, view_h);
                view.render(view_rect, buf);
                let mut y = area.y + view_h;
                if show_approval {
                    render_hidden_approval_attention(
                        hidden_approvals,
                        Rect::new(area.x, y, area.width, 1),
                        buf,
                    );
                    y += 1;
                }
                if show_status {
                    self.footer.render(Rect::new(area.x, y, area.width, 1), buf);
                    y += 1;
                }
                if let Some(h) = hint.filter(|_| show_hint) {
                    render_hint_bar(&h, Rect::new(area.x, y, area.width, 1), buf);
                }
                return;
            }
            view.render(area, buf);
            return;
        }

        // Keep installation notices below the composer/status strip, outside
        // conversation history. Modal views retain their existing layout.
        let notice_lines = self.notice_lines(area.width);
        let notice_h = (notice_lines.len() as u16).min(area.height.saturating_sub(3));
        let content_area = Rect::new(area.x, area.y, area.width, area.height - notice_h);
        ratatui::widgets::Paragraph::new(notice_lines).render(
            Rect::new(area.x, content_area.bottom(), area.width, notice_h),
            buf,
        );
        let area = content_area;
        let [
            approval,
            guidance,
            queued,
            context,
            composer,
            popup,
            footer,
            hints,
        ] = self.composer_areas(area);
        self.render_focused_approval(approval, buf);
        self.render_pending_user_intents(guidance, buf);
        self.render_queued_next_turn_submissions(queued, buf);
        self.footer.render_context_bar(context, buf);
        self.composer.render(
            composer,
            buf,
            self.has_pending_composer_queue(),
            self.composer_tip(),
        );
        if let Some(ref menu) = self.slash_menu {
            slash_popup_render::render_composer(menu, popup, buf);
        } else if let Some(ref menu) = self.mention_menu {
            mention_popup_render::render(menu, popup, buf);
        } else if let Some(ref skills) = self.skill_popup {
            skills.render(popup, buf);
        } else if popup.is_empty() {
            self.composer
                .render_compact_history_suggestion(composer, buf);
        } else {
            self.composer.render_history_suggestions(popup, buf);
        }
        self.footer.render(footer, buf);
        if !hints.is_empty() {
            render_hint_bar(self.composer_hints(), hints, buf);
        }
    }

    fn notice_lines(&self, width: u16) -> Vec<Line<'static>> {
        let style = Style::default().fg(super::theme::current().warn);
        let mut lines = Vec::new();
        if let Some(notice) = &self.update_notice {
            lines.push(Line::styled(notice.clone(), style));
        }
        if self.detached_editor_limit_notice {
            lines.push(Line::styled(
                "Some unsent edits were discarded because the local draft limit was reached; original messages are unchanged.",
                style,
            ));
        }
        if self.visible_pending_intents().any(|intent| {
            intent.status == astra_turn_types::UserIntentStatus::Returned
                || intent.custody == PendingUserIntentCustody::Unconfirmed
        }) {
            lines.push(Line::styled(
                "Message needs attention · Alt+U to view",
                style,
            ));
        }
        if let Some(next) = self.restored_drafts.front() {
            let preview: String = next
                .text()
                .chars()
                .take(60)
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .collect();
            lines.push(Line::styled(
                format!(
                    "Unsent drafts ({}) · send or clear this draft to edit next: {preview}",
                    self.restored_drafts.len(),
                ),
                style,
            ));
        }
        super::wrapping::word_wrap_lines(lines, width.max(1) as usize)
    }

    fn render_focused_approval(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let Some(cell) = self.focused_approval_cell() else {
            return;
        };
        use crate::tui::history_cell::HistoryCell;
        use crate::tui::render::line_utils::{FullRowParagraph, sanitize_lines_for_buffer};
        use ratatui::widgets::{Paragraph, Widget, Wrap};
        let mut lines = sanitize_lines_for_buffer(cell.display_lines(area.width));
        if cell.desired_height(area.width) > area.height
            && cell.focused
            && self.slash_menu.is_none()
            && self.mention_menu.is_none()
            && self.skill_popup.is_none()
        {
            // The final three rows are breathing space, buttons and key hints.
            // Compact them only if all request, risk and reason rows still fit.
            let mut compact = lines[..lines.len().saturating_sub(3)].to_vec();
            compact.push(cell.compact_focused_button_line());
            if Paragraph::new(compact.clone())
                .wrap(Wrap { trim: false })
                .line_count(area.width)
                <= usize::from(area.height)
            {
                lines = compact;
            }
        }
        FullRowParagraph::new(lines)
            .wrap(Wrap { trim: false })
            .render(area, buf);
    }

    fn user_intent_height(&self) -> u16 {
        input_notice_height(self.visible_pending_intents().take(3).count())
    }

    fn next_turn_submission_height(&self) -> u16 {
        input_notice_height(self.visible_next_turn_submissions().take(3).count())
    }

    fn render_pending_user_intents(&self, area: Rect, buf: &mut Buffer) {
        use astra_turn_types::UserIntentStatus::{
            AcceptedLocal, AcceptedRemote, Applied, Returned,
        };

        if area.is_empty() {
            return;
        }
        let mut intents = self.visible_pending_intents();
        let Some(first) = intents.next() else { return };
        let title = if self.interrupt_pending {
            "Stopping · checking message delivery".into()
        } else {
            pending_user_intent_title(first, &self.task_status)
        };
        let theme = super::theme::current();
        let color = if self.interrupt_pending || first.delivery_is_unconfirmed() {
            theme.warn
        } else {
            theme.accent
        };
        let previews = std::iter::once(("", first.text.as_str())).chain(intents.map(|pending| {
            let prefix = if pending.delivery_is_unconfirmed() {
                "Delivery unknown · "
            } else if pending.target == first.target
                && pending.status == first.status
                && pending.custody == first.custody
            {
                ""
            } else {
                match pending.status {
                    AcceptedLocal => "Pending · ",
                    AcceptedRemote => "Received · ",
                    Applied => "Applied · ",
                    Returned => "Returned · ",
                }
            };
            (prefix, pending.text.as_str())
        }));
        render_input_notice(area, buf, &title, color, previews);
    }

    fn render_queued_next_turn_submissions(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }
        let mut submissions = self.visible_next_turn_submissions();
        let Some(first) = submissions.next() else {
            return;
        };
        let command = matches!(first, NextTurnSubmission::Interactive(text)
            if super::slash_dispatch::parse_submission(text).is_command());
        let title = if command {
            "Command queued · after current work"
        } else {
            "Message queued · next turn"
        };
        render_input_notice(
            area,
            buf,
            title,
            super::theme::current().accent,
            std::iter::once(first)
                .chain(submissions)
                .map(|entry| ("", entry.text())),
        );
    }

    pub fn cursor_position(&self, area: Rect) -> Option<(u16, u16)> {
        if let Some(view) = self.active_view() {
            let (approval, hint, status) = visible_view_footers(
                area.height,
                view.conversation_tab_id().is_some(),
                self.has_pending_approvals(),
                view.hint_keys().is_some(),
                view.reserve_status_footer(),
            );
            let footer_rows = u16::from(approval) + u16::from(hint) + u16::from(status);
            let content = Rect::new(
                area.x,
                area.y,
                area.width,
                area.height.saturating_sub(footer_rows),
            );
            return view.cursor_pos(content);
        }

        let notice_h =
            (self.notice_lines(area.width).len() as u16).min(area.height.saturating_sub(3));
        let area = Rect::new(area.x, area.y, area.width, area.height - notice_h);

        let [_, _, _, _, composer, ..] = self.composer_areas(area);
        self.composer.cursor_position(composer)
    }
}

fn visible_view_footers(
    height: u16,
    conversation: bool,
    approval: bool,
    hint: bool,
    status: bool,
) -> (bool, bool, bool) {
    let mut remaining = height.saturating_sub(if conversation { 3 } else { 1 });
    let mut take = |wanted: bool| {
        if wanted && remaining > 0 {
            remaining -= 1;
            true
        } else {
            false
        }
    };
    (take(approval), take(hint), take(status))
}

/// A pending conversation can acquire its canonical run identity while its
/// view stays open. Preserve its existing position in the workspace tab order.
fn reconcile_conversation_tab(
    order: &mut Vec<ConversationTabId>,
    previous: Option<ConversationTabId>,
    current: Option<ConversationTabId>,
) {
    let (Some(previous), Some(current)) = (previous, current) else {
        return;
    };
    if previous == current {
        return;
    }
    if let Some(index) = order.iter().position(|tab| *tab == previous) {
        if order.contains(&current) {
            order.remove(index);
        } else {
            order[index] = current;
        }
    }
}

fn input_notice_height(count: usize) -> u16 {
    if count == 0 {
        0
    } else {
        1 + count.min(3) as u16
    }
}

fn render_input_notice<'a>(
    area: Rect,
    buf: &mut Buffer,
    title: &str,
    color: ratatui::style::Color,
    mut previews: impl Iterator<Item = (&'a str, &'a str)>,
) {
    let theme = super::theme::current();
    let rail = super::glyphs::current().live_gutter.to_string();
    let dim = Style::default().fg(theme.dim);
    let indent = 2.min(area.width.saturating_sub(1));
    let width = area.width - indent;
    let row_area = |row| Rect::new(area.x + indent, area.y + row, width, 1);
    let heading = Line::styled(title, Style::default().fg(color).bold());
    Widget::render(heading, row_area(0), buf);
    if indent > 0 {
        for y in area.y..area.bottom() {
            buf.set_string(area.x, y, &rail, dim);
        }
    }
    let rows = usize::from(area.height.saturating_sub(1).min(2));
    for (row, (prefix, text)) in previews.by_ref().take(rows).enumerate() {
        let budget = usize::from(width).saturating_sub(prefix.width());
        let preview = chat_composer::input_preview(text, budget);
        let line = Line::from(vec![
            Span::styled(prefix, dim),
            Span::styled(preview, Style::default().fg(theme.fg)),
        ]);
        Widget::render(line, row_area(row as u16 + 1), buf);
    }
    if area.height >= 4 {
        let remaining = previews.count();
        if remaining > 0 {
            Widget::render(
                Line::styled(format!("+{remaining} more"), dim),
                row_area(3),
                buf,
            );
        }
    }
}

/// Fit whole shortcuts and keep key chords distinct from their labels.
fn render_hint_bar(hint: &str, area: Rect, buf: &mut Buffer) {
    if area.is_empty() || area.width <= 2 {
        return;
    }
    use ratatui::widgets::Widget;
    use unicode_width::UnicodeWidthStr;

    let theme = super::theme::current();
    let dim = Style::default().fg(theme.dim);
    let key = Style::default().fg(theme.accent_dim());
    let mut remaining = usize::from(area.width) - 2;
    let mut spans = vec![Span::raw("  ")];
    for item in hint.split(" · ") {
        let gap = if spans.len() > 1 { " · " } else { "" };
        let width = gap.width() + item.width();
        if width > remaining {
            continue;
        }
        remaining -= width;
        if !gap.is_empty() {
            spans.push(Span::styled(gap, dim));
        }
        if let Some((chord, label)) = item.split_once(' ') {
            spans.push(Span::styled(chord, key));
            spans.push(Span::styled(format!(" {label}"), dim));
        } else {
            spans.push(Span::styled(item, key));
        }
    }
    Widget::render(Line::from(spans), area, buf);
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
    /// A composer-only delivery choice. The active loop retains conversational
    /// input in its existing local FIFO; idle input and local controls use the
    /// ordinary dispatcher. This does not transfer custody to the Server.
    QueueFollowUp(String),
    OpenExternalEditor(String),
    /// The shortcut was pressed while a turn owns the event loop. Opening the
    /// editor here would stop polling that turn, so the dispatcher must show a
    /// non-blocking explanation instead.
    ExternalEditorUnavailable,
    /// Cycle the everyday permission policies. The event loop applies the
    /// action immediately while idle, or stages it for the next safe turn
    /// boundary while a turn is running.
    CyclePermissionMode,
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
