use crate::tui::agent_run_projection::AgentControlTarget;
use crossterm::event::KeyEvent;
use ratatui::{buffer::Buffer, layout::Rect};

/// Semantic result emitted by a completed view. Display labels must never be
/// used as control data: the same text can occur in unrelated menus, change
/// with localisation, or be supplied by a remote source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ViewResult {
    Login {
        username: String,
        password: String,
    },
    Register {
        username: String,
        email: String,
        password: String,
    },
    ConfigEdit {
        disposition: ConfigEditDisposition,
        toml_body: String,
    },
    Model {
        name: String,
    },
    ModelThinking {
        base_model: String,
        config: astra_turn_core::thinking_config::ThinkingConfig,
    },
    Session {
        session_id: String,
        intent: SessionSelectionIntent,
    },
    WorkspaceTrust(WorkspaceTrustChoice),
    Stats(StatsPanel),
    Instructions(ProjectInstructionsAction),
    Permission(crate::cli::permission_manager::PermissionMode),
    /// Resolution of a high-impact permission change. The original selected
    /// mode stays typed across the confirmation view; no display label is
    /// used to decide whether bypass was approved.
    PermissionConfirmation {
        mode: crate::cli::permission_manager::PermissionMode,
        confirmed: bool,
    },
    Memory(MemorySelection),
    InsertCommand(String),
}

/// The only terminal outcomes of the config editor. The editor's internal
/// states (for example, a visible save prompt) never cross the view boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfigEditDisposition {
    SaveUser,
    SaveProject,
    Discard,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionSelectionIntent {
    Resume,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkspaceTrustChoice {
    Trust,
    ContinueUntrusted,
    MarkUntrusted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StatsPanel {
    Overview,
    History,
    Tools,
    Cost,
    Health,
    Learn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProjectInstructionsAction {
    Show,
    Reload,
    Disable,
}

/// Immutable preview retained from the typed memory-search response. Selecting
/// it opens the exact observed record; it does not re-query by a presentation
/// string or mutate memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemorySelection {
    pub memory_id: String,
    pub content: String,
}

/// Stable identity of a full conversation tab. Root and delegated runs share
/// the same transcript surface; this identity only selects whose canonical
/// history and live suffix the surface projects.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ConversationTabId {
    Root,
    Run { agent_id: String, run_id: String },
}

#[derive(Debug)]
pub(crate) enum CancellationEvent {
    Consumed,
    Escalate,
}

pub(crate) struct ViewCompletion {
    pub result: Option<ViewResult>,
    pub reopen: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BottomPaneViewAction {
    /// Open the root conversation in the same transcript browser used for
    /// delegated runs. The run navigator owns selection only; it never
    /// substitutes its summary for this conversation.
    OpenRootTranscript,
    /// Return from a focused conversation to the run navigator while keeping
    /// the conversation tab alive. If no navigator exists, the dispatcher
    /// closes the current transcript instead of inventing a parent surface.
    ReturnToConversationNavigator,
    InspectAgent {
        agent_id: String,
        agent_name: String,
        run_id: Option<String>,
        transcript_target: Option<crate::tui::agent_run_projection::AgentTranscriptTarget>,
    },
    ControlAgent {
        agent_id: String,
        target: AgentControlTarget,
        action: astra_thin_client::SessionRunAction,
    },
    BeginAgentGuide {
        agent_id: String,
        agent_name: String,
        run_id: String,
        target: AgentControlTarget,
    },
    SubmitAgentGuide {
        agent_id: String,
        agent_name: String,
        run_id: String,
        target: AgentControlTarget,
        content: String,
    },
    LoadAgentTranscript {
        agent_id: String,
        session_id: String,
        run_id: String,
        transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget,
        before_seq: Option<i64>,
    },
    LoadRootTranscript {
        session_id: String,
        transcript_target: crate::tui::bottom_pane::root_transcript_view::RootTranscriptTarget,
        before_seq: Option<i64>,
    },
    /// Persist the transcript projection without blocking the UI loop. The
    /// originating view owns pagination and content assembly; the event loop
    /// owns the file effect through the bounded TUI writer.
    ExportTranscript {
        path: std::path::PathBuf,
        lines: Vec<String>,
    },
    /// Copy a transcript selection without running a subprocess on the input
    /// or render path.
    CopyToClipboard {
        text: String,
        success_message: String,
    },
    /// Ask the durable run-tree observer to bypass its current retry backoff.
    /// The request stays in the view/action boundary; the observer owns the
    /// eventual network effect and authoritative projection update.
    RefreshAgentMonitor,
    /// Ask every canonical source behind the task board to bypass its current
    /// observation backoff. The task board remains read-only; this only
    /// refreshes its typed projections.
    RefreshTaskBoard,
    StopBackgroundTask {
        task_id: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ViewActionDisposition {
    KeepOpen,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ViewActionRequest {
    pub action: BottomPaneViewAction,
    pub disposition: ViewActionDisposition,
}

pub(crate) trait BottomPaneView: Send {
    fn render(&self, area: Rect, buf: &mut Buffer);
    fn desired_height(&self, width: u16) -> u16;
    fn handle_key(&mut self, key: KeyEvent);
    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)>;

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        CancellationEvent::Escalate
    }

    fn is_complete(&self) -> bool {
        false
    }

    fn completion(&self) -> Option<ViewCompletion> {
        None
    }

    fn prefer_esc_to_handle_key_event(&self) -> bool {
        false
    }

    fn dismiss_after_child_accept(&self) -> bool {
        false
    }

    fn is_in_paste_burst(&self) -> bool {
        false
    }

    /// Route bracketed paste to the focused view. Returning `true` means the
    /// paste was consumed and must not mutate the composer behind the view.
    fn handle_paste(&mut self, _text: &str) -> bool {
        false
    }

    fn pre_draw_tick(&mut self, _now: std::time::Instant) {}

    fn refresh_task_cell(
        &mut self,
        _id: &str,
        _cell: &crate::tui::history_cell::task::TaskCell,
    ) -> bool {
        false
    }

    fn live_task_id(&self) -> Option<&str> {
        None
    }

    fn refresh_agent_monitor(
        &mut self,
        _snapshot: crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot,
    ) -> bool {
        false
    }

    fn refresh_task_board(
        &mut self,
        _projection: &crate::tui::task_board_observer::TaskBoardProjection,
    ) -> bool {
        false
    }

    fn accepts_agent_rows(&self) -> bool {
        false
    }

    fn refresh_agent_transcript(
        &mut self,
        _update: crate::tui::bottom_pane::agent_transcript_view::AgentTranscriptUpdate,
    ) -> bool {
        false
    }

    fn refresh_root_transcript(
        &mut self,
        _update: crate::tui::bottom_pane::root_transcript_view::RootTranscriptUpdate,
    ) -> bool {
        false
    }

    /// Refresh the current local root suffix without promoting it to durable
    /// history. Only the durable root transcript browser consumes this; the
    /// local fallback browser already receives the full live snapshot.
    fn refresh_root_transcript_live(
        &mut self,
        _item: Option<crate::tui::bottom_pane::transcript_view::TranscriptItem>,
    ) -> bool {
        false
    }

    /// Refresh non-conversational runtime facts that are currently visible in
    /// the root workbench but have not reached the durable transcript yet.
    /// Unlike the live assistant suffix, these items disappear when the
    /// underlying runtime condition resolves and never trigger a durable-page
    /// reconciliation on their own.
    fn refresh_root_transcript_context(
        &mut self,
        _items: Vec<crate::tui::bottom_pane::transcript_view::TranscriptItem>,
    ) -> bool {
        false
    }

    /// A root transcript sidecar became durable. The root browser may request
    /// an ordinary typed reload; no caller is allowed to synthesize or merge
    /// transcript rows from display text.
    fn refresh_root_transcript_committed(&mut self, _session_id: &str) -> bool {
        false
    }

    /// Deliver a typed live event directly to an open agent transcript. This
    /// keeps the transcript's live conversation faithful instead of
    /// reconstructing it from a task-card summary.
    fn refresh_agent_live_event(
        &mut self,
        _event: &astra_turn_core::agent_live_event::AgentLiveEvent,
    ) -> bool {
        false
    }

    /// Mark an open agent conversation as incomplete after its bounded live
    /// transport dropped progress. The view keeps confirmed history intact and
    /// asks the user to refresh durable history; it never invents a message to
    /// fill the missing interval.
    fn refresh_agent_live_gap(
        &mut self,
        _gap: &astra_turn_core::agent_live_event::AgentLiveGap,
    ) -> bool {
        false
    }

    /// Bind an already-open live agent conversation to the session that was
    /// just created for its parent turn. Implementations return `true` only
    /// when this binding produced a typed follow-up view action.
    ///
    /// This is deliberately not a general session-rebind hook: a view must
    /// never guess that a different session owns an existing run.
    fn bind_unbound_agent_transcript_session(&mut self, _session_id: &str) -> bool {
        false
    }

    fn refresh_background_task_rows(
        &mut self,
        _rows: Vec<crate::tui::bottom_pane::background_task_view::BackgroundTaskRow>,
    ) -> bool {
        false
    }

    fn refresh_background_task_rows_selecting(
        &mut self,
        rows: Vec<crate::tui::bottom_pane::background_task_view::BackgroundTaskRow>,
        _selected_id: Option<&str>,
    ) -> bool {
        self.refresh_background_task_rows(rows)
    }

    fn accepts_background_task_rows(&self) -> bool {
        false
    }

    fn refresh_transcript_snapshot(
        &mut self,
        _snapshot: crate::tui::bottom_pane::transcript_view::TranscriptSnapshot,
        _width: u16,
    ) -> bool {
        false
    }

    /// Whether this root-conversation view consumes the in-memory transcript
    /// snapshot. The canonical durable transcript owns its own paged history
    /// and only needs the current live suffix; rebuilding the whole local
    /// history for it on every stream event would put unbounded work on the
    /// keyboard/render path.
    fn uses_local_root_transcript_snapshot(&self) -> bool {
        false
    }

    /// Short key-binding hint rendered as a 1-row footer at the bottom
    /// of the view. Return `None` to suppress (no hint bar reserved).
    ///
    /// The expected style is dim, space-separated, `·` as a separator
    /// between groups:
    ///
    /// ```text
    /// ↑↓ navigate · Enter resume · Esc close
    /// ```
    fn hint_keys(&self) -> Option<String> {
        None
    }

    /// Drain a typed action requested by this view. The disposition makes
    /// view ownership explicit: navigation actions can close the current
    /// view while control actions keep selection and scroll state intact.
    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        None
    }

    /// Opt in to having `BottomPane`'s status-line footer (model ·
    /// cost · token budget · permission mode · git branch · pending
    /// approvals) rendered under this view. `false` keeps the view
    /// occupying the entire bottom pane area — appropriate for
    /// dismissable dialogs that should feel like pop-ups, not
    /// embedded side panels.
    fn reserve_status_footer(&self) -> bool {
        false
    }

    fn is_transcript_view(&self) -> bool {
        false
    }

    /// The root conversation is the target of the global Ctrl+O action.
    /// Delegated runs use the same transcript browser, but are a distinct
    /// navigation scope and must not make Ctrl+O behave like a close key.
    fn is_root_transcript_view(&self) -> bool {
        false
    }

    /// Session identity for a root transcript backed by the canonical durable
    /// conversation store. `None` means this is a local, pre-session view and
    /// must be replaced when a session becomes available.
    fn durable_root_transcript_session(&self) -> Option<&str> {
        None
    }

    /// A full transcript can be re-activated like a browser tab instead of
    /// being rebuilt from a task summary. Non-conversation overlays return
    /// `None` and retain their ordinary stack semantics.
    fn conversation_tab_id(&self) -> Option<ConversationTabId> {
        None
    }

    /// Human-facing label for the retained conversation workspace. Identity
    /// remains the typed [`ConversationTabId`]; labels are presentation only
    /// and must never be used to select, deduplicate, or route a run.
    fn conversation_tab_label(&self) -> Option<String> {
        self.conversation_tab_id().map(|tab| match tab {
            ConversationTabId::Root => "Main conversation".to_string(),
            ConversationTabId::Run { agent_id, .. } => agent_id,
        })
    }

    /// Whether this view replaces the compact-chat canvas. Transcript tabs
    /// opt in through their conversation identity; other workspaces may opt
    /// in explicitly without pretending to be a conversation.
    fn owns_primary_canvas(&self) -> bool {
        self.conversation_tab_id().is_some()
    }

    /// Resize a focused conversation to own the terminal's primary canvas.
    /// Root and delegated transcripts are peer conversations; dialogs and
    /// task cards remain bounded overlays.
    fn fit_conversation_workspace(&mut self, _terminal_height: u16, _width: u16) {}
}
