//! Agent-scoped loading and pagination around the shared transcript surface.

use std::sync::Arc;

use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal};
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{buffer::Buffer, layout::Rect, text::Line};

use super::transcript_view::{
    TranscriptItem, TranscriptItemId, TranscriptItemKind, TranscriptSnapshot, TranscriptView,
};
use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ConversationTabId,
    ViewActionDisposition, ViewActionRequest, ViewCompletion,
};
use crate::tui::history_cell::{
    HistoryCell, assistant::AssistantCell, reasoning::ReasoningCell, tool::ToolCell,
};

/// The live, non-durable suffix of an agent conversation. It deliberately
/// preserves the same content boundaries as the canonical transcript: output,
/// reasoning, tool calls, and high-value attention evidence are distinct
/// objects rather than lines collapsed into a task summary.
#[derive(Debug)]
enum LiveTranscriptItem {
    Assistant(AssistantCell),
    Reasoning(ReasoningCell),
    Tool {
        tool_use_id: String,
        cell: ToolCell,
    },
    Notice {
        text: String,
        evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
    },
}

#[derive(Debug, Default)]
struct LiveTranscript {
    items: Vec<LiveTranscriptItem>,
    /// A terminal live signal settles the suffix, but does not make it
    /// durable. Assistant/reasoning deltas currently lack a shared canonical
    /// item identity, so they remain visibly attributable until a future
    /// envelope can reconcile them without text matching.
    settled: bool,
}

impl LiveTranscript {
    fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    fn mark_active(&mut self) {
        self.settled = false;
    }

    fn mark_settled(&mut self) {
        self.settled = true;
    }

    fn reconcile_durable_items(
        &mut self,
        durable_items: &[astra_thin_client::SessionTranscriptItem],
    ) {
        let durable_tool_ids = durable_items
            .iter()
            .flat_map(|item| {
                item.tool_calls
                    .iter()
                    .map(|call| call.tool_use_id.as_str())
                    .chain(
                        item.tool_result
                            .iter()
                            .map(|result| result.tool_use_id.as_str()),
                    )
            })
            .collect::<std::collections::HashSet<_>>();
        let durable_evidence = durable_items
            .iter()
            .filter_map(|item| item.evidence.as_ref())
            .map(astra_turn_types::AgentTranscriptEvidence::stable_key)
            .collect::<std::collections::HashSet<_>>();

        self.items.retain(|item| match item {
            // Output deltas and reasoning chunks do not yet carry a durable
            // item identity. Never use equal text as a surrogate: repeated
            // findings, retries, and identical short answers are distinct
            // conversation objects. Until the event envelope supplies a
            // shared stable id, preserve this live suffix rather than
            // silently deleting a potentially different message.
            LiveTranscriptItem::Assistant(_) | LiveTranscriptItem::Reasoning(_) => true,
            LiveTranscriptItem::Tool { tool_use_id, .. } => {
                !durable_tool_ids.contains(tool_use_id.as_str())
            }
            // Only typed evidence can reconcile a live attention notice with
            // durable history. Plain status text remains view-local so a
            // similar-looking message can never erase an unrelated event.
            LiveTranscriptItem::Notice {
                evidence: Some(evidence),
                ..
            } => !durable_evidence.contains(&evidence.stable_key()),
            LiveTranscriptItem::Notice { evidence: None, .. } => true,
        });
    }

    fn finish_open_model_item(&mut self) {
        match self.items.last_mut() {
            Some(LiveTranscriptItem::Assistant(cell)) if cell.is_live() => cell.finalize(),
            Some(LiveTranscriptItem::Reasoning(cell)) if cell.is_live() => cell.finalize(),
            _ => {}
        }
    }

    fn finish_all_model_items(&mut self) {
        for item in &mut self.items {
            match item {
                LiveTranscriptItem::Assistant(cell) if cell.is_live() => {
                    cell.finalize();
                }
                LiveTranscriptItem::Reasoning(cell) if cell.is_live() => {
                    cell.finalize();
                }
                _ => {}
            }
        }
    }

    fn append_output(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.mark_active();
        match self.items.last_mut() {
            Some(LiveTranscriptItem::Assistant(cell)) if cell.is_live() => cell.push_delta(text),
            _ => {
                self.finish_open_model_item();
                let mut cell = AssistantCell::new_streaming();
                cell.push_delta(text);
                self.items.push(LiveTranscriptItem::Assistant(cell));
            }
        }
    }

    fn append_reasoning(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.mark_active();
        match self.items.last_mut() {
            Some(LiveTranscriptItem::Reasoning(cell)) if cell.is_live() => cell.push_delta(text),
            _ => {
                self.finish_open_model_item();
                let mut cell = ReasoningCell::new_streaming();
                cell.push_delta(text);
                self.items.push(LiveTranscriptItem::Reasoning(cell));
            }
        }
    }

    fn tool_started(&mut self, tool_use_id: String, name: String, description: String) {
        self.mark_active();
        self.finish_open_model_item();
        if self
            .items
            .iter()
            .any(|item| matches!(item, LiveTranscriptItem::Tool { tool_use_id: id, .. } if id == &tool_use_id))
        {
            return;
        }
        self.items.push(LiveTranscriptItem::Tool {
            tool_use_id,
            cell: ToolCell::new_running(name, description),
        });
    }

    // Mirrors the typed ToolCompleted wire payload at the reducer boundary.
    #[allow(clippy::too_many_arguments)]
    fn tool_completed(
        &mut self,
        tool_use_id: String,
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
    ) {
        self.mark_active();
        let tool = self.items.iter_mut().rev().find_map(|item| match item {
            LiveTranscriptItem::Tool {
                tool_use_id: id,
                cell,
            } if id == &tool_use_id => Some(cell),
            _ => None,
        });
        let cell = match tool {
            Some(cell) => cell,
            None => {
                self.finish_open_model_item();
                self.items.push(LiveTranscriptItem::Tool {
                    tool_use_id,
                    cell: ToolCell::new_running(name, description.clone()),
                });
                match self.items.last_mut() {
                    Some(LiveTranscriptItem::Tool { cell, .. }) => cell,
                    _ => unreachable!("newly appended live item must be a tool"),
                }
            }
        };
        cell.complete(&status, duration_ms, description, output_summary, output);
    }

    fn notice(
        &mut self,
        text: String,
        evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
    ) {
        if !text.trim().is_empty() {
            self.mark_active();
            self.finish_open_model_item();
            self.items
                .push(LiveTranscriptItem::Notice { text, evidence });
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum AgentTranscriptUpdate {
    Loaded {
        agent_id: String,
        run_id: String,
        page: astra_thin_client::SessionTranscriptPage,
        replace: bool,
        source: AgentTranscriptSource,
    },
    Failed {
        agent_id: String,
        run_id: String,
        message: String,
    },
}

/// The authority that supplied the visible initial page for an agent run.
///
/// A local journal can temporarily lead the server projection in Edge+Server,
/// but pages from those cursor domains must never be interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AgentTranscriptSource {
    DurableServer,
    LocalJournalOnly,
    LocalJournalWhileServerCatchesUp,
    LocalJournalWithBroaderHistory,
    LocalJournalWhileServerUnavailable,
}

pub(crate) struct AgentTranscriptView {
    agent_id: String,
    agent_name: String,
    /// A local/edge child can begin producing typed live events before the
    /// parent session is durably bound. The conversation remains viewable in
    /// that interval; this only gates durable history pagination.
    session_id: Option<String>,
    run_id: String,
    /// The authoritative history source. A launch receipt can legitimately
    /// arrive after typed live events, so a run remains inspectable before
    /// this location is known; only durable paging is gated on it.
    transcript_target: Option<crate::tui::agent_run_projection::AgentTranscriptTarget>,
    items: Vec<astra_thin_client::SessionTranscriptItem>,
    next_before_seq: Option<i64>,
    has_more: bool,
    loading: bool,
    error: Option<String>,
    source: Option<AgentTranscriptSource>,
    live: LiveTranscript,
    pending_transcript_commit: Option<PendingTranscriptCommit>,
    /// A terminal live event is emitted only after the local or server runner
    /// has attempted to persist its canonical transcript. Refresh that page
    /// once so an already-open conversation does not remain a live-only
    /// suffix after the run finishes. This is deliberately separate from
    /// live/durable item reconciliation: until both lanes share an item id,
    /// the view must not erase output based on equal text.
    terminal_refresh_requested: bool,
    /// The transcript projection is built for the same viewport that opened
    /// it. A delegated run is a first-class conversation, not a compact task
    /// detail panel with a different layout budget than another run.
    viewport_width: u16,
    transcript: TranscriptView,
    completed: bool,
    reopen: Option<String>,
    return_action: BottomPaneViewAction,
    pending_action: Option<ViewActionRequest>,
    export_pending: bool,
    export_seen_cursors: std::collections::HashSet<i64>,
}

#[derive(Debug, Clone)]
struct PendingTranscriptCommit {
    source_event_id: String,
}

impl AgentTranscriptView {
    // View identity, data source, and viewport are intentionally explicit;
    // none is an optional builder concern after construction.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn loading(
        agent_id: String,
        agent_name: String,
        session_id: String,
        run_id: String,
        transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget,
        reopen: impl Into<String>,
        viewport_width: u16,
        terminal_height: u16,
    ) -> Self {
        let mut view = Self {
            agent_id,
            agent_name: agent_name.clone(),
            transcript: TranscriptView::from_snapshot(
                TranscriptSnapshot::default(),
                terminal_height,
                viewport_width,
            )
            .with_title(format!("{agent_name} · Transcript")),
            session_id: Some(session_id),
            run_id,
            transcript_target: Some(transcript_target),
            items: Vec::new(),
            next_before_seq: None,
            has_more: false,
            loading: true,
            error: None,
            source: None,
            live: LiveTranscript::default(),
            pending_transcript_commit: None,
            terminal_refresh_requested: false,
            viewport_width,
            completed: false,
            reopen: Some(reopen.into()),
            return_action: BottomPaneViewAction::ReturnToConversationNavigator,
            pending_action: None,
            export_pending: false,
            export_seen_cursors: std::collections::HashSet::new(),
        };
        view.rebuild_transcript();
        view
    }

    /// Open the same conversation browser before the durable session and/or
    /// run identity has arrived. An empty `run_id` is an explicit pending
    /// identity, never a guessed address; the first typed event or monitor
    /// row for this stable `agent_id` binds it.
    pub(crate) fn live_unbound(
        agent_id: String,
        agent_name: String,
        run_id: String,
        transcript_target: Option<crate::tui::agent_run_projection::AgentTranscriptTarget>,
        reopen: impl Into<String>,
        viewport_width: u16,
        terminal_height: u16,
    ) -> Self {
        let mut view = Self {
            agent_id,
            agent_name: agent_name.clone(),
            transcript: TranscriptView::from_snapshot(
                TranscriptSnapshot::default(),
                terminal_height,
                viewport_width,
            )
            .with_title(format!("{agent_name} · Transcript")),
            session_id: None,
            run_id,
            transcript_target,
            items: Vec::new(),
            next_before_seq: None,
            has_more: false,
            loading: false,
            error: None,
            source: None,
            live: LiveTranscript::default(),
            pending_transcript_commit: None,
            terminal_refresh_requested: false,
            viewport_width,
            completed: false,
            reopen: Some(reopen.into()),
            return_action: BottomPaneViewAction::ReturnToConversationNavigator,
            pending_action: None,
            export_pending: false,
            export_seen_cursors: std::collections::HashSet::new(),
        };
        view.rebuild_transcript();
        view
    }

    fn request_load(&mut self, before_seq: Option<i64>) {
        if self.loading {
            return;
        }
        let Some(session_id) = self.session_id.clone() else {
            self.error = Some(
                "History is waiting for this run's session binding; live activity remains available."
                    .into(),
            );
            self.rebuild_transcript();
            return;
        };
        let Some(transcript_target) = self.transcript_target else {
            self.error = Some(
                "Canonical transcript location is still pending; live activity remains available."
                    .into(),
            );
            self.rebuild_transcript();
            return;
        };
        // The server and the edge journal have independent cursor domains.
        // When the first durable page is empty/unavailable we deliberately
        // show the exact local run history, but older pages must continue from
        // that same journal. `R` (no cursor) is the explicit user action that
        // retries the durable server projection.
        let target = if before_seq.is_some()
            && matches!(
                self.source,
                Some(
                    AgentTranscriptSource::LocalJournalOnly
                        | AgentTranscriptSource::LocalJournalWhileServerCatchesUp
                        | AgentTranscriptSource::LocalJournalWithBroaderHistory
                        | AgentTranscriptSource::LocalJournalWhileServerUnavailable
                )
            ) {
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal
        } else {
            transcript_target
        };
        self.loading = true;
        self.error = None;
        self.transcript.set_activity_status(Some(
            match (before_seq, target) {
                (
                    Some(_),
                    crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
                ) => "Loading older local agent history…",
                (
                    Some(_),
                    crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                ) => "Loading older durable agent history…",
                (None, _) => "Syncing durable agent history…",
            }
            .to_string(),
        ));
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::LoadAgentTranscript {
                agent_id: self.agent_id.clone(),
                session_id,
                run_id: self.run_id.clone(),
                transcript_target: target,
                before_seq,
            },
            disposition: ViewActionDisposition::KeepOpen,
        });
        self.rebuild_transcript();
    }

    /// Promote a live-only conversation once its parent session has been
    /// created. The run id stays unchanged, so live events continue to land
    /// in the same view while the durable prefix is fetched.
    fn bind_session(&mut self, session_id: &str) -> bool {
        if self.session_id.is_some() || session_id.trim().is_empty() {
            return false;
        }
        self.session_id = Some(session_id.to_string());
        if self.transcript_target.is_some() {
            self.request_load(None);
            true
        } else {
            self.transcript.set_activity_status(Some(
                "Canonical transcript location pending · live activity remains available".into(),
            ));
            self.rebuild_transcript();
            false
        }
    }

    fn apply_page(
        &mut self,
        page: astra_thin_client::SessionTranscriptPage,
        replace: bool,
        source: AgentTranscriptSource,
    ) {
        if replace {
            self.items = page.items;
            self.source = Some(source);
        } else {
            let known = self
                .items
                .iter()
                .map(transcript_item_identity)
                .collect::<std::collections::HashSet<_>>();
            let mut older: Vec<_> = page
                .items
                .into_iter()
                .filter(|item| !known.contains(&transcript_item_identity(item)))
                .collect();
            older.append(&mut self.items);
            self.items = older;
        }
        self.next_before_seq = page.next_before_seq;
        self.has_more = page.has_more;
        self.loading = false;
        self.error = None;
        self.transcript
            .set_activity_status(match (self.source, self.items.is_empty()) {
                (_, true) | (Some(AgentTranscriptSource::DurableServer) | None, false) => None,
                (Some(AgentTranscriptSource::LocalJournalOnly), false) => {
                    Some("Local agent history".into())
                }
                (Some(AgentTranscriptSource::LocalJournalWhileServerCatchesUp), false) => Some(
                    "Local agent history · server transcript is still syncing · R refresh".into(),
                ),
                (Some(AgentTranscriptSource::LocalJournalWithBroaderHistory), false) => {
                    Some("Local agent history · server page is incomplete · R refresh".into())
                }
                (Some(AgentTranscriptSource::LocalJournalWhileServerUnavailable), false) => {
                    Some("Local agent history · server transcript unavailable · R refresh".into())
                }
            });
        self.live.reconcile_durable_items(&self.items);
        if let Some(commit) = self.pending_transcript_commit.as_ref()
            && self.items.iter().any(|item| {
                item.source_event_id.as_deref() == Some(commit.source_event_id.as_str())
            })
        {
            // The id proves that the canonical page caught up, but legacy
            // model deltas do not carry it. Preserve them until both sides
            // share an item identity; deleting by text or ordinal corrupts
            // multi-round and reasoning-only histories.
            self.pending_transcript_commit = None;
        }
        if self.pending_transcript_commit.is_some() && !self.terminal_refresh_requested {
            // The commit can race an older page already in flight. One typed
            // follow-up read is sufficient for local journals and a committed
            // MatrixOne transaction; never spin on eventual visibility.
            self.terminal_refresh_requested = true;
            self.request_load(None);
        }
        self.rebuild_transcript();
        if self.export_pending {
            self.continue_export();
        }
    }

    fn request_export(&mut self) {
        self.export_pending = true;
        self.export_seen_cursors.clear();
        if self.loading {
            self.transcript
                .set_activity_status(Some("Preparing complete transcript export…".into()));
            return;
        }
        self.continue_export();
    }

    fn continue_export(&mut self) {
        if self.has_more {
            let Some(cursor) = self.next_before_seq else {
                self.fail_export("Transcript source reported older history without a cursor");
                return;
            };
            if !self.export_seen_cursors.insert(cursor) {
                self.fail_export("Transcript pagination stopped at a repeated cursor");
                return;
            }
            self.request_load(Some(cursor));
            self.transcript.set_activity_status(Some(
                "Loading complete agent conversation for export…".into(),
            ));
            return;
        }

        self.export_pending = false;
        let identity = if self.run_id.trim().is_empty() {
            self.agent_id.as_str()
        } else {
            self.run_id.as_str()
        };
        let path = super::root_transcript_view::transcript_export_path("agent", identity);
        let mut lines = vec![
            "# Astra agent transcript".to_string(),
            String::new(),
            format!("- Agent: {}", self.agent_name),
            format!("- Agent ID: {}", self.agent_id),
            format!("- Run: {}", self.run_id),
            String::new(),
        ];
        lines.extend(self.transcript.export_plain_lines());
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::ExportTranscript {
                path: path.clone(),
                lines,
            },
            disposition: ViewActionDisposition::KeepOpen,
        });
        self.transcript.set_activity_status(Some(format!(
            "Transcript export queued → {}",
            path.display()
        )));
    }

    fn fail_export(&mut self, message: &str) {
        self.export_pending = false;
        self.export_seen_cursors.clear();
        self.transcript
            .set_activity_status(Some(format!("Export stopped · {message} · R retry")));
    }

    fn transcript_snapshot(&self) -> TranscriptSnapshot {
        if self.items.is_empty() && self.live.is_empty() {
            let message = if self.loading {
                "Loading durable conversation…"
            } else if let Some(error) = self.error.as_deref() {
                error
            } else if self.run_id.trim().is_empty() {
                "Waiting for this agent's run identity; live activity will appear here."
            } else if self.transcript_target.is_none() {
                "Canonical transcript location pending; live activity will appear here."
            } else if self.session_id.is_none() {
                "Waiting for session binding; live activity will appear here."
            } else {
                match self.source {
                    Some(AgentTranscriptSource::LocalJournalOnly) => {
                        "No local agent history exists for this run yet."
                    }
                    Some(AgentTranscriptSource::LocalJournalWhileServerCatchesUp)
                    | Some(AgentTranscriptSource::LocalJournalWithBroaderHistory)
                    | Some(AgentTranscriptSource::LocalJournalWhileServerUnavailable)
                    | Some(AgentTranscriptSource::DurableServer)
                    | None => "No canonical conversation items have synced for this agent yet.",
                }
            };
            return TranscriptSnapshot::new(vec![TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(0),
                vec![Line::from(message.to_string())],
                0,
            )]);
        }

        let mut projected = durable_transcript_items(&self.items);
        self.append_live_projection(&mut projected);
        TranscriptSnapshot::new(projected)
    }

    fn append_live_projection(&self, projected: &mut Vec<TranscriptItem>) {
        const LIVE_ID_BASE: u64 = 1 << 63;
        let durable_tool_ids = self
            .items
            .iter()
            .flat_map(|item| {
                item.tool_calls
                    .iter()
                    .map(|call| call.tool_use_id.as_str())
                    .chain(
                        item.tool_result
                            .iter()
                            .map(|result| result.tool_use_id.as_str()),
                    )
            })
            .collect::<std::collections::HashSet<_>>();

        if !self.live.is_empty() {
            let state = if self.live.settled {
                "Local agent result · awaiting durable reconciliation"
            } else {
                "Live agent projection · awaiting durable reconciliation"
            };
            projected.push(TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(LIVE_ID_BASE - 1),
                vec![Line::from(state)],
                0,
            ));
        }

        for (index, item) in self.live.items.iter().enumerate() {
            let id = TranscriptItemId::from_widget_id(
                LIVE_ID_BASE.saturating_add(u64::try_from(index).unwrap_or(u64::MAX - 2)),
            );
            match item {
                LiveTranscriptItem::Assistant(cell) if cell.is_live() => projected.push(
                    // This is the same mutable suffix contract as the root
                    // transcript: preserve current content and provenance,
                    // but never rebuild a growing Markdown document on every
                    // live event while the user is inspecting a child run.
                    TranscriptItem::rendered_kind(
                        id,
                        TranscriptItemKind::Assistant,
                        cell.live_viewport_lines(self.viewport_width, 48),
                        1,
                    ),
                ),
                LiveTranscriptItem::Assistant(cell) => {
                    projected.push(TranscriptItem::committed(id, Arc::new(cell.clone()), 1))
                }
                LiveTranscriptItem::Reasoning(cell) => {
                    projected.push(TranscriptItem::reasoning(id, cell.clone(), 1));
                }
                LiveTranscriptItem::Tool { tool_use_id, cell }
                    if !durable_tool_ids.contains(tool_use_id.as_str()) =>
                {
                    projected.push(TranscriptItem::tool(id, cell.clone(), 1));
                }
                LiveTranscriptItem::Notice {
                    text: _,
                    evidence: Some(evidence),
                } => projected.push(TranscriptItem::rendered_kind(
                    id,
                    transcript_evidence_kind(evidence),
                    transcript_evidence_lines(evidence),
                    1,
                )),
                LiveTranscriptItem::Notice {
                    text,
                    evidence: None,
                } => projected.push(TranscriptItem::rendered_kind(
                    id,
                    TranscriptItemKind::Agent,
                    vec![Line::from(format!("• {text}"))],
                    1,
                )),
                LiveTranscriptItem::Tool { .. } => {}
            }
        }
    }

    fn rebuild_transcript(&mut self) {
        self.transcript
            .replace_with(self.transcript_snapshot(), self.viewport_width);
    }

    fn apply_live_event(&mut self, event: &AgentLiveEvent) -> bool {
        if event.run_id.trim().is_empty() {
            return false;
        }
        if event.agent_id != self.agent_id {
            let launched_from_this_view = matches!(
                &event.kind,
                AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                    spawn_tool_call_id: Some(tool_call_id),
                    ..
                }) if self.agent_id == format!("pending:{tool_call_id}")
            );
            if !launched_from_this_view {
                return false;
            }
            self.agent_id = event.agent_id.clone();
            self.error = None;
        }
        if self.run_id.trim().is_empty() {
            self.run_id = event.run_id.clone();
            self.error = None;
        } else if event.run_id != self.run_id {
            return false;
        }
        if matches!(
            &event.kind,
            AgentLiveEventKind::OutputDelta(_)
                | AgentLiveEventKind::ThinkingDelta(_)
                | AgentLiveEventKind::ToolStarted { .. }
                | AgentLiveEventKind::ToolCompleted { .. }
        ) {
            // A resumed run can use the same durable run identity. Its next
            // terminal edge must request a fresh canonical page again.
            self.terminal_refresh_requested = false;
        }
        match &event.kind {
            AgentLiveEventKind::OutputDelta(text) => self.live.append_output(text),
            AgentLiveEventKind::ThinkingDelta(text) => self.live.append_reasoning(text),
            AgentLiveEventKind::ToolStarted {
                name,
                description,
                tool_use_id,
            } => self
                .live
                .tool_started(tool_use_id.clone(), name.clone(), description.clone()),
            AgentLiveEventKind::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
            } => self.live.tool_completed(
                tool_use_id.clone(),
                name.clone(),
                description.clone(),
                status.clone(),
                *duration_ms,
                output_summary.clone(),
                output.clone(),
            ),
            AgentLiveEventKind::Signal(signal) => {
                if matches!(signal, AgentLiveSignal::ExecutionWaiting { .. }) {
                    // The canonical run remains resumable, but this executor
                    // has released it. Freeze the current suffix so the UI
                    // does not keep animating output that can no longer
                    // arrive; a later resumed delta calls `mark_active`.
                    self.live.finish_all_model_items();
                    self.live.mark_settled();
                }
                if let AgentLiveSignal::RunStarted {
                    transcript_location,
                    ..
                } = signal
                {
                    self.transcript_target = Some(match transcript_location {
                        astra_turn_types::AgentTranscriptLocation::LocalJournal => {
                            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal
                        }
                        astra_turn_types::AgentTranscriptLocation::DurableServer => {
                            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer
                        }
                    });
                }
                if let AgentLiveSignal::TranscriptCommitted {
                    source_event_id,
                    transcript_location,
                } = signal
                {
                    self.live.finish_all_model_items();
                    self.live.mark_settled();
                    self.transcript_target = Some(match transcript_location {
                        astra_turn_types::AgentTranscriptLocation::LocalJournal => {
                            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal
                        }
                        astra_turn_types::AgentTranscriptLocation::DurableServer => {
                            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer
                        }
                    });
                    self.pending_transcript_commit = Some(PendingTranscriptCommit {
                        source_event_id: source_event_id.clone(),
                    });
                    if self.session_id.is_some() && !self.loading {
                        self.terminal_refresh_requested = true;
                        self.request_load(None);
                    }
                }
                if let Some((summary, evidence)) = live_signal_transcript_notice(signal) {
                    self.live.notice(summary, evidence);
                }
            }
            AgentLiveEventKind::Status(text) => self.live.notice(text.clone(), None),
            AgentLiveEventKind::AgentTerminated {
                termination,
                reason,
                ..
            } => {
                self.live.finish_all_model_items();
                let reason = reason.as_deref().filter(|reason| !reason.trim().is_empty());
                let status = match termination {
                    astra_turn_core::agent_live_event::AgentLiveTermination::Completed => {
                        "completed"
                    }
                    astra_turn_core::agent_live_event::AgentLiveTermination::Delegated => {
                        "delegated"
                    }
                    astra_turn_core::agent_live_event::AgentLiveTermination::Failed => "failed",
                    astra_turn_core::agent_live_event::AgentLiveTermination::Interrupted => {
                        "interrupted"
                    }
                    astra_turn_core::agent_live_event::AgentLiveTermination::Cancelled => {
                        "cancelled"
                    }
                };
                let notice = if matches!(
                    termination,
                    astra_turn_core::agent_live_event::AgentLiveTermination::Interrupted
                ) {
                    reason
                        .and_then(astra_turn_core::interruption::InterruptionKind::from_label)
                        .map_or_else(
                            || {
                                "Agent needs continuation · The run stopped before it completed."
                                    .to_string()
                            },
                            |kind| {
                                format!(
                                    "Agent {} · {}",
                                    kind.user_status().to_ascii_lowercase(),
                                    kind.user_description()
                                )
                            },
                        )
                } else {
                    match reason {
                        Some(reason) => format!("Agent {status} · {reason}"),
                        None => format!("Agent {status}"),
                    }
                };
                self.live.notice(notice, None);
                self.live.mark_settled();
                // Local sub-runners and the durable server attempt transcript
                // persistence before publishing this terminal lifecycle edge.
                // Refresh exactly once; the I/O remains an async view action
                // and a failed refresh preserves the visible live suffix.
                if !self.terminal_refresh_requested
                    && self.session_id.is_some()
                    && self.transcript_target.is_some()
                    && !self.loading
                {
                    self.terminal_refresh_requested = true;
                    self.request_load(None);
                }
            }
        }
        self.rebuild_transcript();
        true
    }
}

fn live_signal_transcript_notice(
    signal: &AgentLiveSignal,
) -> Option<(String, Option<astra_turn_types::AgentTranscriptEvidence>)> {
    match signal {
        // These are transient activity indicators, not durable transcript
        // objects. Keeping them out avoids a noisy, misleading event log.
        AgentLiveSignal::RunStarted { .. }
        | AgentLiveSignal::WaitingForModel
        | AgentLiveSignal::ModelResponding
        | AgentLiveSignal::OutputSettled
        | AgentLiveSignal::TranscriptCommitted { .. }
        | AgentLiveSignal::ToolProgress { .. } => None,
        AgentLiveSignal::AgentCommunication(event) => Some((
            crate::tui::chat_widget::agent_live_signal_summary(signal),
            Some(
                astra_turn_types::AgentTranscriptEvidence::AgentCommunication {
                    event: event.clone(),
                },
            ),
        )),
        AgentLiveSignal::ApprovalRequired {
            request_id,
            tool,
            approval_kind,
            detail,
            display_label,
            ..
        } => Some((
            crate::tui::chat_widget::agent_live_signal_summary(signal),
            Some(
                astra_turn_types::AgentTranscriptEvidence::ApprovalRequired {
                    request_id: request_id.clone(),
                    tool: tool.clone(),
                    approval_kind: approval_kind.clone(),
                    display_label: display_label.clone(),
                    detail: detail.clone(),
                },
            ),
        )),
        _ => Some((
            crate::tui::chat_widget::agent_live_signal_summary(signal),
            None,
        )),
    }
}

fn transcript_evidence_lines(
    evidence: &astra_turn_types::AgentTranscriptEvidence,
) -> Vec<Line<'static>> {
    match evidence {
        astra_turn_types::AgentTranscriptEvidence::ApprovalRequired {
            tool,
            approval_kind,
            display_label,
            detail,
            ..
        } => {
            let subject = display_label
                .as_deref()
                .or(detail.as_deref())
                .unwrap_or(tool);
            vec![
                Line::from(format!("Permission required · {subject}")),
                Line::from(format!("Tool · {tool} · {approval_kind}")),
            ]
        }
        astra_turn_types::AgentTranscriptEvidence::AgentCommunication { event } => {
            let (direction, peer) = match event.direction {
                astra_turn_types::AgentCommunicationDirection::Sent => {
                    let peer = match &event.to {
                        astra_turn_types::AgentCommunicationTarget::Direct { address } => {
                            address.agent_id.as_str()
                        }
                        astra_turn_types::AgentCommunicationTarget::Broadcast { delegation_id } => {
                            delegation_id.as_str()
                        }
                        astra_turn_types::AgentCommunicationTarget::Parent => "parent",
                    };
                    ("sent to", peer)
                }
                astra_turn_types::AgentCommunicationDirection::Received => {
                    ("received from", event.from.agent_id.as_str())
                }
            };
            let mut lines = vec![Line::from(format!(
                "Message {direction} {peer} · {}",
                event.payload_kind
            ))];
            if let Some(summary) = event
                .summary
                .as_deref()
                .filter(|text| !text.trim().is_empty())
            {
                lines.push(Line::from(summary.to_string()));
            }
            lines
        }
    }
}

fn transcript_evidence_kind(
    evidence: &astra_turn_types::AgentTranscriptEvidence,
) -> TranscriptItemKind {
    match evidence {
        astra_turn_types::AgentTranscriptEvidence::ApprovalRequired { .. } => {
            TranscriptItemKind::System
        }
        astra_turn_types::AgentTranscriptEvidence::AgentCommunication { .. } => {
            TranscriptItemKind::Agent
        }
    }
}

/// Render durable conversation items for any root or delegated run through
/// the shared transcript browser. Identity comes from the typed item envelope;
/// this projection never compares visible text to reconcile records.
pub(crate) fn durable_transcript_items(
    items: &[astra_thin_client::SessionTranscriptItem],
) -> Vec<TranscriptItem> {
    let tool_results = items
        .iter()
        .filter_map(|item| {
            let result = item.tool_result.as_ref()?;
            (!result.tool_use_id.is_empty()).then_some((result.tool_use_id.as_str(), item))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let mut paired_tool_result_seqs = std::collections::HashSet::new();
    let mut projected = Vec::new();
    for item in items {
        let canonical_id = transcript_item_identity(item);
        if let Some(reasoning) = item.reasoning.as_deref() {
            projected.push(TranscriptItem::reasoning(
                TranscriptItemId::from_canonical(canonical_id.clone(), "reasoning"),
                crate::tui::history_cell::reasoning::ReasoningCell::from_text(reasoning, None),
                1,
            ));
        }
        let id = TranscriptItemId::from_canonical(canonical_id.clone(), "content");
        if let Some(evidence) = item.evidence.as_ref() {
            projected.push(TranscriptItem::rendered_kind(
                id,
                transcript_evidence_kind(evidence),
                transcript_evidence_lines(evidence),
                1,
            ));
            continue;
        }
        match item.role.as_str() {
            "user" => projected.push(TranscriptItem::committed(
                id,
                Arc::new(crate::tui::history_cell::user::UserCell::new(
                    item.content.clone(),
                )),
                1,
            )),
            "assistant" => {
                if !item.content.trim().is_empty() {
                    projected.push(TranscriptItem::committed(
                        id,
                        Arc::new(
                            crate::tui::history_cell::assistant::AssistantCell::from_markdown(
                                item.content.clone(),
                            ),
                        ),
                        1,
                    ));
                }
                for (index, call) in item.tool_calls.iter().enumerate() {
                    let result_item = tool_results.get(call.tool_use_id.as_str()).copied();
                    let mut cell = crate::tui::history_cell::tool::ToolCell::new_running(
                        call.name.clone(),
                        call.arguments.clone(),
                    );
                    if let Some(result_item) = result_item {
                        paired_tool_result_seqs.insert(result_item.item_seq);
                        let result = result_item.tool_result.as_ref().unwrap();
                        cell.complete(
                            result.status.as_deref().unwrap_or("success"),
                            result.duration_ms.unwrap_or_default(),
                            call.arguments.clone(),
                            result_item.content.lines().next().map(ToString::to_string),
                            Some(result_item.content.clone()),
                        );
                    }
                    let component = if call.tool_use_id.is_empty() {
                        format!("tool:{index}")
                    } else {
                        format!("tool:{}", call.tool_use_id)
                    };
                    projected.push(TranscriptItem::tool(
                        TranscriptItemId::from_canonical(canonical_id.clone(), component),
                        cell,
                        1,
                    ));
                }
            }
            "tool" => {
                if paired_tool_result_seqs.contains(&item.item_seq) {
                    continue;
                }
                let result = item.tool_result.as_ref();
                let mut cell = crate::tui::history_cell::tool::ToolCell::new_running(
                    result
                        .and_then(|result| result.name.as_deref())
                        .unwrap_or("tool"),
                    "Tool result",
                );
                cell.complete(
                    result
                        .and_then(|result| result.status.as_deref())
                        .unwrap_or("success"),
                    result
                        .and_then(|result| result.duration_ms)
                        .unwrap_or_default(),
                    "Tool result".into(),
                    item.content.lines().next().map(ToString::to_string),
                    Some(item.content.clone()),
                );
                projected.push(TranscriptItem::tool(id, cell, 1));
            }
            role => projected.push(TranscriptItem::rendered_kind(
                id,
                transcript_role_kind(role),
                vec![Line::from(format!("{role} · {}", item.content))],
                1,
            )),
        }
    }
    projected
}

fn transcript_role_kind(role: &str) -> TranscriptItemKind {
    match role {
        "user" => TranscriptItemKind::User,
        "assistant" => TranscriptItemKind::Assistant,
        "tool" => TranscriptItemKind::Tool,
        "agent" => TranscriptItemKind::Agent,
        "error" => TranscriptItemKind::Error,
        _ => TranscriptItemKind::System,
    }
}

fn transcript_item_identity(item: &astra_thin_client::SessionTranscriptItem) -> String {
    item.source_event_id
        .as_deref()
        .map(|event_id| format!("event:{event_id}"))
        .unwrap_or_else(|| format!("item:{}", item.item_seq))
}

impl BottomPaneView for AgentTranscriptView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        self.transcript.render(area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.transcript.desired_height(width)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Left if self.transcript.is_search_active() => self.transcript.handle_key(key),
            KeyCode::Left if !self.transcript.collapse_current_item() => {
                self.pending_action = Some(ViewActionRequest {
                    action: self.return_action.clone(),
                    disposition: ViewActionDisposition::KeepOpen,
                });
            }
            KeyCode::Esc | KeyCode::Char('q') => {
                self.pending_action = Some(ViewActionRequest {
                    action: self.return_action.clone(),
                    disposition: ViewActionDisposition::KeepOpen,
                });
            }
            KeyCode::Right => {
                self.transcript.expand_current_item();
            }
            KeyCode::Char('r' | 'R') => self.request_load(None),
            KeyCode::Char('o' | 'O') if self.has_more => self.request_load(self.next_before_seq),
            KeyCode::Char('s' | 'S') => self.request_export(),
            _ => self.transcript.handle_key(key),
        }
    }

    fn cursor_pos(&self, area: Rect) -> Option<(u16, u16)> {
        self.transcript.cursor_pos(area)
    }

    fn is_complete(&self) -> bool {
        self.completed || self.transcript.is_complete()
    }

    fn completion(&self) -> Option<ViewCompletion> {
        self.is_complete().then(|| ViewCompletion {
            result: None,
            reopen: self.reopen.clone(),
        })
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.transcript.on_ctrl_c()
    }

    fn refresh_agent_transcript(&mut self, update: AgentTranscriptUpdate) -> bool {
        match update {
            AgentTranscriptUpdate::Loaded {
                agent_id,
                run_id,
                page,
                replace,
                source,
            } if agent_id == self.agent_id && run_id == self.run_id => {
                self.apply_page(page, replace, source)
            }
            AgentTranscriptUpdate::Failed {
                agent_id,
                run_id,
                message,
            } if agent_id == self.agent_id && run_id == self.run_id => {
                self.loading = false;
                self.export_pending = false;
                self.error = Some(message);
                let status = match self.source {
                    Some(AgentTranscriptSource::LocalJournalWhileServerCatchesUp) => {
                        "Could not sync durable agent history · showing local history · R retry"
                    }
                    Some(AgentTranscriptSource::LocalJournalWithBroaderHistory) => {
                        "Could not refresh server transcript · showing broader local history · R retry"
                    }
                    Some(AgentTranscriptSource::LocalJournalWhileServerUnavailable) => {
                        "Server transcript unavailable · showing local history · R retry"
                    }
                    Some(AgentTranscriptSource::LocalJournalOnly) => {
                        "Could not load local agent history · R retry"
                    }
                    Some(AgentTranscriptSource::DurableServer) | None => {
                        "Could not sync durable agent history · R retry"
                    }
                };
                self.transcript
                    .set_activity_status(Some(status.to_string()));
                self.rebuild_transcript();
            }
            _ => return false,
        }
        true
    }

    fn refresh_agent_monitor(
        &mut self,
        snapshot: crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot,
    ) -> bool {
        let mut changed = false;
        let pending_tool_call_id = self.agent_id.strip_prefix("pending:");
        let matched_row = snapshot.rows.iter().find(|row| {
            row.agent_id == self.agent_id
                || pending_tool_call_id.is_some_and(|tool_call_id| {
                    row.spawn_tool_call_id.as_deref() == Some(tool_call_id)
                })
        });
        let Some(row) = matched_row else {
            return false;
        };
        if row.agent_id != self.agent_id {
            self.agent_id = row.agent_id.clone();
            self.agent_name = row.name.clone();
            self.error = None;
            changed = true;
        }
        if self.run_id.trim().is_empty()
            && let Some(run_id) = row
                .run_id
                .as_deref()
                .filter(|run_id| !run_id.trim().is_empty())
        {
            self.run_id = run_id.to_string();
            self.error = None;
            changed = true;
        }
        let Some(target) = row.transcript_target.filter(|_| {
            self.run_id.trim().is_empty() || row.run_id.as_deref() == Some(self.run_id.as_str())
        }) else {
            if changed {
                self.rebuild_transcript();
            }
            return changed;
        };
        if self.transcript_target == Some(target) {
            if changed {
                self.rebuild_transcript();
            }
            return changed;
        }

        self.transcript_target = Some(target);
        if self.session_id.is_some() {
            self.request_load(None);
        } else if self.items.is_empty() {
            self.error = None;
            self.transcript.set_activity_status(Some(
                "Canonical transcript location available · awaiting session binding".into(),
            ));
            self.rebuild_transcript();
        }
        true
    }

    fn has_pending_agent_transcript_identity(&self) -> bool {
        self.run_id.trim().is_empty() || self.agent_id.starts_with("pending:")
    }

    fn refresh_agent_live_event(&mut self, event: &AgentLiveEvent) -> bool {
        self.apply_live_event(event)
    }

    fn refresh_agent_live_gap(
        &mut self,
        gap: &astra_turn_core::agent_live_event::AgentLiveGap,
    ) -> bool {
        if gap.run_id.trim().is_empty() {
            return false;
        }
        if self.run_id.trim().is_empty() {
            self.run_id = gap.run_id.clone();
            self.error = None;
        } else if gap.run_id != self.run_id {
            return false;
        }
        let count = gap.dropped_event_count;
        self.live.notice(
            format!(
                "Live activity incomplete · {count} update{} skipped · R refresh durable history",
                if count == 1 { "" } else { "s" }
            ),
            None,
        );
        self.transcript.set_activity_status(Some(
            "Live activity incomplete · R refresh durable history".to_string(),
        ));
        self.rebuild_transcript();
        true
    }

    fn bind_unbound_agent_transcript_session(&mut self, session_id: &str) -> bool {
        self.bind_session(session_id)
    }

    fn hint_keys(&self) -> Option<String> {
        let mut hints = Vec::new();
        if self.session_id.is_some() && self.transcript_target.is_some() {
            hints.push("R refresh");
        } else if self.transcript_target.is_none() {
            hints.push("live only");
        }
        if self.has_more {
            hints.push("O older");
        }
        hints.push("Ctrl+E toggle");
        hints.push("S export");
        hints.push("Ctrl+G conversations");
        hints.push("Shift+←/→ switch");
        hints.push("←/Esc agents");
        Some(hints.join(" · "))
    }

    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action
            .take()
            .or_else(|| self.transcript.take_action_request())
    }

    fn handle_paste(&mut self, text: &str) -> bool {
        self.transcript.handle_paste(text)
    }

    fn is_transcript_view(&self) -> bool {
        true
    }

    fn conversation_tab_id(&self) -> Option<ConversationTabId> {
        Some(ConversationTabId::Run {
            agent_id: self.agent_id.clone(),
            run_id: self.run_id.clone(),
        })
    }

    fn conversation_tab_label(&self) -> Option<String> {
        Some(self.agent_name.clone())
    }

    fn fit_conversation_workspace(&mut self, terminal_height: u16, width: u16) {
        self.viewport_width = width;
        self.transcript.fit_workspace(terminal_height, width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn rendered(view: &AgentTranscriptView) -> String {
        let area = Rect::new(0, 0, 80, 20);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        crate::tui::testing::render::buffer_to_string(&buffer)
    }

    fn page() -> astra_thin_client::SessionTranscriptPage {
        astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![astra_thin_client::SessionTranscriptItem {
                session_id: "session-1".into(),
                item_seq: 7,
                run_id: Some("run-child".into()),
                role: "assistant".into(),
                content: "Found the race.".into(),
                reasoning: Some("Inspect state ownership".into()),
                reasoning_status: Some("done".into()),
                tool_calls: Vec::new(),
                tool_result: None,
                evidence: None,
                source_event_id: None,
                created_at: "2026-07-11T00:00:00".into(),
            }],
            page_refs: vec![],
            next_before_seq: Some(7),
            has_more: true,
        }
    }

    fn transcript_page_with_assistant_lines(
        count: i64,
    ) -> astra_thin_client::SessionTranscriptPage {
        astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: (0..count)
                .map(|item_seq| astra_thin_client::SessionTranscriptItem {
                    session_id: "session-1".into(),
                    item_seq,
                    run_id: Some("run-child".into()),
                    role: "assistant".into(),
                    content: format!("Finding {item_seq}"),
                    reasoning: None,
                    reasoning_status: None,
                    tool_calls: Vec::new(),
                    tool_result: None,
                    evidence: None,
                    source_event_id: None,
                    created_at: "2026-07-11T00:00:00".into(),
                })
                .collect(),
            page_refs: Vec::new(),
            next_before_seq: None,
            has_more: false,
        }
    }

    fn monitor_row_with_target(
        transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget,
    ) -> crate::tui::bottom_pane::in_flight_agents_view::AgentRow {
        crate::tui::bottom_pane::in_flight_agents_view::AgentRow {
            agent_id: "agent-1".into(),
            name: "Reviewer".into(),
            spawn_tool_call_id: None,
            activity: Default::default(),
            run_id: Some("run-child".into()),
            parent_run_id: None,
            depth: 1,
            provenance: crate::tui::agent_run_projection::AgentProjectionSource::LiveStream,
            elapsed_ms: 0,
            state: crate::tui::agent_run_projection::AgentRunState::observed(
                crate::tui::agent_run_projection::AgentRunStatus::Running,
            ),
            attention_summary: None,
            fanout: None,
            control_target: None,
            transcript_target: Some(transcript_target),
            available_actions: Vec::new(),
            runtime: Default::default(),
        }
    }

    fn loading_view(viewport_width: u16, terminal_height: u16) -> AgentTranscriptView {
        AgentTranscriptView::loading(
            "agent-1".into(),
            "Reviewer".into(),
            "session-1".into(),
            "run-child".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
            "agents",
            viewport_width,
            terminal_height,
        )
    }

    #[test]
    fn agent_transcript_uses_the_same_viewport_budget_as_a_root_transcript() {
        let page = transcript_page_with_assistant_lines(40);
        let mut compact = loading_view(32, 24);
        compact.apply_page(page.clone(), true, AgentTranscriptSource::DurableServer);
        let mut spacious = loading_view(120, 120);
        spacious.apply_page(page, true, AgentTranscriptSource::DurableServer);

        assert!(
            spacious.desired_height(120) > compact.desired_height(32),
            "a delegated transcript must use its opening terminal budget rather than the old fixed detail-panel budget"
        );
    }

    #[test]
    fn durable_payload_drives_conversation_and_reasoning_projection() {
        let mut view = AgentTranscriptView::loading(
            "agent-1".into(),
            "Reviewer".into(),
            "session-1".into(),
            "run-child".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
            "agents",
            80,
            0,
        );
        assert!(
            view.refresh_agent_transcript(AgentTranscriptUpdate::Loaded {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                page: page(),
                replace: true,
                source: AgentTranscriptSource::DurableServer,
            })
        );
        let collapsed = rendered(&view);
        assert!(collapsed.contains("Found the race."));
        assert!(collapsed.contains("Thought"));
        assert!(!collapsed.contains("Inspect state ownership"));

        view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        let expanded = rendered(&view);
        assert!(expanded.contains("Inspect state ownership"));
        view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !view.is_complete(),
            "left first collapses the selected detail"
        );
        assert!(!rendered(&view).contains("Inspect state ownership"));
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !view.is_complete(),
            "left while typing a transcript search must not leave the agent conversation"
        );
        assert!(view.cursor_pos(Rect::new(0, 0, 80, 20)).is_some());
    }

    #[test]
    fn agent_refresh_keeps_confirmed_history_and_reports_sync_progress_or_failure() {
        let mut view = loading_view(80, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);

        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        let syncing = rendered(&view);
        assert!(syncing.contains("Found the race."), "{syncing}");
        assert!(
            syncing.contains("Syncing durable agent history…"),
            "{syncing}"
        );

        assert!(
            view.refresh_agent_transcript(AgentTranscriptUpdate::Failed {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                message: "unreachable internal endpoint with credentials".into(),
            })
        );
        let failed = rendered(&view);
        assert!(failed.contains("Found the race."), "{failed}");
        assert!(
            failed.contains("Could not sync durable agent history · R retry"),
            "{failed}"
        );
        assert!(!failed.contains("credentials"), "{failed}");
    }

    #[test]
    fn local_agent_history_remains_visible_while_server_projection_catches_up() {
        let mut view = loading_view(100, 24);
        view.refresh_agent_transcript(AgentTranscriptUpdate::Loaded {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            page: page(),
            replace: true,
            source: AgentTranscriptSource::LocalJournalWhileServerCatchesUp,
        });

        let initial = rendered(&view);
        assert!(initial.contains("Found the race."), "{initial}");
        assert!(
            initial
                .contains("Local agent history · server transcript is still syncing · R refresh"),
            "{initial}"
        );

        view.refresh_agent_transcript(AgentTranscriptUpdate::Failed {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            message: "server unavailable".into(),
        });
        let failed = rendered(&view);
        assert!(failed.contains("Found the race."), "{failed}");
        assert!(
            failed
                .contains("Could not sync durable agent history · showing local history · R retry"),
            "{failed}"
        );
    }

    #[test]
    fn live_agent_suffix_has_explicit_provenance_until_a_canonical_identity_exists() {
        let mut view = loading_view(80, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta("unreconciled live finding".into()),
        }));
        let live = rendered(&view);
        assert!(
            live.contains("Live agent projection · awaiting durable reconciliation"),
            "{live}"
        );
        assert!(live.contains("unreconciled live finding"), "{live}");

        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                duration_ms: 1_000,
                reason: None,
            },
        }));
        let settled = rendered(&view);
        assert!(
            settled.contains("Local agent result · awaiting durable reconciliation"),
            "{settled}"
        );
        assert!(settled.contains("unreconciled live finding"), "{settled}");
    }

    #[test]
    fn empty_completion_termination_renders_human_state_not_wire_reason() {
        let mut view = loading_view(100, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);

        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: astra_turn_core::agent_live_event::AgentLiveTermination::Interrupted,
                duration_ms: 1_000,
                reason: Some("empty_completion".into()),
            },
        }));

        let output = rendered(&view);
        assert!(output.contains("Agent needs final answer"), "{output}");
        assert!(
            output.contains("stopped before producing a final answer"),
            "{output}"
        );
        assert!(!output.contains("empty_completion"), "{output}");
    }

    #[test]
    fn committed_identity_confirms_page_without_guessing_live_model_mapping() {
        let mut view = loading_view(100, 30);
        view.loading = false;
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta("old live fragment".into()),
        }));
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::TranscriptCommitted {
                source_event_id: "assistant-committed-1".into(),
                transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
            }),
        }));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    transcript_target:
                        crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                    before_seq: None,
                    ..
                },
                ..
            })
        ));

        // A resumed model can start before the committed page returns. It is
        // beyond the acknowledgement boundary and must remain live.
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta("new resumed fragment".into()),
        }));
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![astra_thin_client::SessionTranscriptItem {
                    session_id: "session-1".into(),
                    item_seq: 11,
                    run_id: Some("run-child".into()),
                    role: "assistant".into(),
                    content: "canonical old answer".into(),
                    reasoning_status: None,
                    reasoning: None,
                    tool_calls: Vec::new(),
                    tool_result: None,
                    evidence: None,
                    source_event_id: Some("assistant-committed-1".into()),
                    created_at: "2026-07-13T00:00:00Z".into(),
                }],
                page_refs: Vec::new(),
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::DurableServer,
        );

        let reconciled = rendered(&view);
        assert!(reconciled.contains("canonical old answer"), "{reconciled}");
        assert!(reconciled.contains("new resumed fragment"), "{reconciled}");
        assert!(reconciled.contains("old live fragment"), "{reconciled}");
        assert!(view.pending_transcript_commit.is_none());
    }

    #[test]
    fn terminal_live_event_refreshes_canonical_history_once_without_text_reconciliation() {
        let mut view = loading_view(80, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta("final live finding".into()),
        }));

        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                duration_ms: 1_000,
                reason: None,
            },
        }));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    agent_id,
                    session_id,
                    run_id,
                    transcript_target:
                        crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                    before_seq: None,
                },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1" && session_id == "session-1" && run_id == "run-child"
        ));

        // The refresh has completed, but a duplicate terminal delivery still
        // must not keep issuing new reads. The live suffix stays visible:
        // without a shared item identity it cannot be deduplicated by text.
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                duration_ms: 1_000,
                reason: None,
            },
        }));
        assert!(view.take_action_request().is_none());
        assert!(rendered(&view).contains("final live finding"));
    }

    #[test]
    fn live_gap_preserves_confirmed_history_and_marks_the_suffix_incomplete() {
        let mut view = loading_view(80, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);

        assert!(
            view.refresh_agent_live_gap(&astra_turn_core::agent_live_event::AgentLiveGap {
                run_id: "run-child".into(),
                agent_id: "agent-1".into(),
                dropped_event_count: 2,
            })
        );
        let rendered = rendered(&view);
        assert!(rendered.contains("Found the race."), "{rendered}");
        assert!(
            rendered.contains(
                "Live activity incomplete · 2 updates skipped · R refresh durable history"
            ),
            "{rendered}"
        );
        assert!(
            !view.refresh_agent_live_gap(&astra_turn_core::agent_live_event::AgentLiveGap {
                run_id: "other-run".into(),
                agent_id: "agent-1".into(),
                dropped_event_count: 1,
            }),
            "a reused profile must not leak a gap into another run's transcript"
        );
    }

    #[test]
    fn delayed_page_for_reused_agent_profile_cannot_replace_another_run() {
        let mut view = loading_view(80, 24);
        let mut delayed_page = page();
        delayed_page.items[0].content = "wrong run".into();

        assert!(
            !view.refresh_agent_transcript(AgentTranscriptUpdate::Loaded {
                agent_id: "agent-1".into(),
                run_id: "run-older-attempt".into(),
                page: delayed_page,
                replace: true,
                source: AgentTranscriptSource::DurableServer,
            })
        );
        assert!(rendered(&view).contains("Loading durable conversation…"));
    }

    #[test]
    fn agent_transcript_advertises_global_detail_and_conversation_navigation() {
        let view = loading_view(80, 24);
        let hints = view.hint_keys().expect("agent transcript hints");
        assert!(hints.contains("Ctrl+E toggle"), "{hints}");
        assert!(hints.contains("Ctrl+G conversations"), "{hints}");
    }

    #[test]
    fn older_page_request_retains_exact_run_target() {
        let mut view = AgentTranscriptView::loading(
            "agent-1".into(),
            "Reviewer".into(),
            "session-1".into(),
            "run-child".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
            "agents",
            80,
            0,
        );
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);
        view.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    agent_id,
                    session_id,
                    run_id,
                    transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                    before_seq: Some(7),
                    ..
                },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1" && session_id == "session-1" && run_id == "run-child"
        ));
    }

    #[test]
    fn older_page_after_local_server_fallback_keeps_the_local_cursor_domain() {
        let mut view = loading_view(80, 24);
        view.apply_page(
            page(),
            true,
            AgentTranscriptSource::LocalJournalWhileServerCatchesUp,
        );

        view.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    transcript_target:
                        crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
                    before_seq: Some(7),
                    ..
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        ));

        // Refresh is intentionally distinct from pagination: it retries the
        // durable projection from its own initial cursor domain.
        view.loading = false;
        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    transcript_target:
                        crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                    before_seq: None,
                    ..
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        ));
    }

    #[test]
    fn typed_live_events_preserve_agent_transcript_boundaries() {
        let mut view = AgentTranscriptView::loading(
            "agent-1".into(),
            "Reviewer".into(),
            "session-1".into(),
            "run-child".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
            "agents",
            80,
            0,
        );

        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::ThinkingDelta("Inspect the scheduler state.".into()),
        }));
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::OutputDelta("I found the race.".into()),
        }));
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "read_file".into(),
                description: "src/scheduler.rs".into(),
                tool_use_id: "tool-1".into(),
            },
        }));
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::ToolCompleted {
                name: "read_file".into(),
                description: "src/scheduler.rs".into(),
                status: "success".into(),
                duration_ms: 12,
                output_summary: Some("scheduler source loaded".into()),
                output: Some("full scheduler source".into()),
                tool_use_id: "tool-1".into(),
            },
        }));
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::ApprovalRequired {
                request_id: "approval-1".into(),
                tool: "bash".into(),
                approval_kind: "explicit".into(),
                path: None,
                detail: Some("git status".into()),
                display_label: None,
            }),
        }));

        // The live tool supersedes the same child in the coarse TaskCell;
        // one provenance row plus its separate reasoning/output/tool/attention
        // objects must remain visible.
        assert_eq!(view.transcript_snapshot().item_count(), 5);
        let output = rendered(&view);
        assert!(output.contains("Thought"));
        assert!(output.contains("I found the race."));
        assert!(output.contains("scheduler source loaded"));
        assert!(output.contains("Permission required"), "{output}");
        assert_eq!(output.matches("src/scheduler.rs").count(), 1);

        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![
                    astra_thin_client::SessionTranscriptItem {
                        session_id: "session-1".into(),
                        item_seq: 7,
                        run_id: Some("run-child".into()),
                        role: "assistant".into(),
                        content: "I found the race.".into(),
                        reasoning: Some("Inspect the scheduler state.".into()),
                        reasoning_status: Some("done".into()),
                        tool_calls: vec![astra_thin_client::SessionTranscriptToolCall {
                            tool_use_id: "tool-1".into(),
                            name: "read_file".into(),
                            arguments: "src/scheduler.rs".into(),
                        }],
                        tool_result: None,
                        evidence: None,
                        source_event_id: Some("call-1".into()),
                        created_at: "2026-07-12T00:00:00".into(),
                    },
                    astra_thin_client::SessionTranscriptItem {
                        session_id: "session-1".into(),
                        item_seq: 8,
                        run_id: Some("run-child".into()),
                        role: "tool".into(),
                        content: "full scheduler source".into(),
                        reasoning: None,
                        reasoning_status: None,
                        tool_calls: Vec::new(),
                        tool_result: Some(astra_thin_client::SessionTranscriptToolResult {
                            tool_use_id: "tool-1".into(),
                            name: Some("read_file".into()),
                            status: Some("success".into()),
                            duration_ms: Some(12),
                        }),
                        evidence: None,
                        source_event_id: Some("result-1".into()),
                        created_at: "2026-07-12T00:00:01".into(),
                    },
                    astra_thin_client::SessionTranscriptItem {
                        session_id: "session-1".into(),
                        item_seq: 9,
                        run_id: Some("run-child".into()),
                        role: "event".into(),
                        content: String::new(),
                        reasoning: None,
                        reasoning_status: None,
                        tool_calls: Vec::new(),
                        tool_result: None,
                        evidence: Some(
                            astra_turn_types::AgentTranscriptEvidence::ApprovalRequired {
                                request_id: "approval-1".into(),
                                tool: "bash".into(),
                                approval_kind: "explicit".into(),
                                display_label: None,
                                detail: Some("git status".into()),
                            },
                        ),
                        source_event_id: Some("approval-1".into()),
                        created_at: "2026-07-12T00:00:02".into(),
                    },
                ],
                page_refs: Vec::new(),
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::LocalJournalOnly,
        );
        assert_eq!(
            view.live.items.len(),
            2,
            "only live objects with a stable durable identity may reconcile"
        );
        let reconciled = rendered(&view);
        assert_eq!(reconciled.matches("I found the race.").count(), 2);
        assert_eq!(reconciled.matches("src/scheduler.rs").count(), 1);
        assert_eq!(
            reconciled
                .matches("Permission required · git status")
                .count(),
            1
        );

        assert!(!view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "other-run".into(),
            // A reused profile is a distinct conversation when its run id
            // differs; its live suffix must never bleed into this transcript.
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::OutputDelta("must not leak".into()),
        }));
        assert!(!rendered(&view).contains("must not leak"));
    }

    #[test]
    fn durable_agent_evidence_uses_shared_typed_filter_semantics() {
        let communication = astra_turn_types::AgentCommunicationEvent {
            schema_version: astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION.into(),
            observed_by: astra_turn_types::AgentCommunicationParty {
                run_id: "run-review".into(),
                agent_id: "reviewer".into(),
            },
            direction: astra_turn_types::AgentCommunicationDirection::Received,
            message_id: "message-1".into(),
            from: astra_turn_types::AgentCommunicationParty {
                run_id: "run-code".into(),
                agent_id: "coder".into(),
            },
            to: astra_turn_types::AgentCommunicationTarget::Direct {
                address: astra_turn_types::AgentCommunicationParty {
                    run_id: "run-review".into(),
                    agent_id: "reviewer".into(),
                },
            },
            payload_kind: astra_turn_types::AgentCommunicationPayloadKind::Text,
            summary: Some("lock ownership is unsafe".into()),
            response_accepted: None,
            related_message_id: None,
            timestamp_ms: 42,
            correlation_id: None,
            requires_ack: false,
        };
        let item = |item_seq: i64,
                    source_event_id: &str,
                    evidence: astra_turn_types::AgentTranscriptEvidence| {
            astra_thin_client::SessionTranscriptItem {
                session_id: "session-1".into(),
                item_seq,
                run_id: Some("run-review".into()),
                role: "event".into(),
                content: String::new(),
                reasoning: None,
                reasoning_status: None,
                tool_calls: Vec::new(),
                tool_result: None,
                evidence: Some(evidence),
                source_event_id: Some(source_event_id.into()),
                created_at: "2026-07-13T00:00:00Z".into(),
            }
        };
        let items = vec![
            item(
                1,
                "communication-1",
                astra_turn_types::AgentTranscriptEvidence::AgentCommunication {
                    event: communication,
                },
            ),
            item(
                2,
                "approval-1",
                astra_turn_types::AgentTranscriptEvidence::ApprovalRequired {
                    request_id: "approval-1".into(),
                    tool: "bash".into(),
                    approval_kind: "explicit".into(),
                    display_label: None,
                    detail: Some("git status".into()),
                },
            ),
        ];
        let mut transcript = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(durable_transcript_items(&items)),
            24,
            80,
        );
        let render = |view: &TranscriptView| {
            let area = Rect::new(0, 0, 80, 20);
            let mut buffer = Buffer::empty(area);
            view.render(area, &mut buffer);
            crate::tui::testing::render::buffer_to_string(&buffer)
        };

        for _ in 0..6 {
            transcript.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        }
        let agents = render(&transcript);
        assert!(agents.contains("lock ownership is unsafe"), "{agents}");
        assert!(!agents.contains("Permission required"), "{agents}");

        transcript.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE));
        let system = render(&transcript);
        assert!(
            system.contains("Permission required · git status"),
            "{system}"
        );
        assert!(!system.contains("lock ownership is unsafe"), "{system}");
    }

    #[test]
    fn durable_projection_retains_canonical_event_and_component_identity() {
        let items = vec![astra_thin_client::SessionTranscriptItem {
            session_id: "session-1".into(),
            item_seq: i64::MAX,
            run_id: Some("run-review".into()),
            role: "assistant".into(),
            content: "review complete".into(),
            reasoning: Some("compare both implementations".into()),
            reasoning_status: Some("done".into()),
            tool_calls: vec![
                astra_thin_client::SessionTranscriptToolCall {
                    tool_use_id: "call-read".into(),
                    name: "read_file".into(),
                    arguments: "src/lib.rs".into(),
                },
                astra_thin_client::SessionTranscriptToolCall {
                    tool_use_id: "call-test".into(),
                    name: "bash".into(),
                    arguments: "cargo test".into(),
                },
            ],
            tool_result: None,
            evidence: None,
            source_event_id: Some("assistant-event-with-lossless-identity".into()),
            created_at: "2026-07-13T00:00:00Z".into(),
        }];

        let projected = durable_transcript_items(&items);
        let ids = projected
            .iter()
            .map(TranscriptItem::id)
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec![
                TranscriptItemId::from_canonical(
                    "event:assistant-event-with-lossless-identity",
                    "reasoning"
                ),
                TranscriptItemId::from_canonical(
                    "event:assistant-event-with-lossless-identity",
                    "content"
                ),
                TranscriptItemId::from_canonical(
                    "event:assistant-event-with-lossless-identity",
                    "tool:call-read"
                ),
                TranscriptItemId::from_canonical(
                    "event:assistant-event-with-lossless-identity",
                    "tool:call-test"
                ),
            ]
        );
    }

    #[test]
    fn unbound_live_run_opens_conversation_before_session_binding() {
        let mut view = AgentTranscriptView::live_unbound(
            "agent-1".into(),
            "Reviewer".into(),
            "run-child".into(),
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
            "agents",
            80,
            24,
        );
        assert!(rendered(&view).contains("Waiting for session binding"));

        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::OutputDelta("live review finding".into()),
        }));
        let output = rendered(&view);
        assert!(output.contains("live review finding"), "{output}");

        view.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE));
        assert!(view.take_action_request().is_none());
    }

    #[test]
    fn pending_run_identity_binds_only_from_a_typed_event_for_the_same_agent() {
        let mut view = AgentTranscriptView::live_unbound(
            "agent-pending".into(),
            "Reviewer".into(),
            String::new(),
            None,
            "agents",
            80,
            24,
        );
        assert!(
            rendered(&view).contains("Waiting for this agent's run identity"),
            "{}",
            rendered(&view)
        );
        assert!(!view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-other".into(),
            agent_id: "other-agent".into(),
            kind: AgentLiveEventKind::OutputDelta("must not bind".into()),
        }));

        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-pending".into(),
            agent_id: "agent-pending".into(),
            kind: AgentLiveEventKind::OutputDelta("first visible finding".into()),
        }));
        assert_eq!(view.run_id, "run-pending");
        let bound = rendered(&view);
        assert!(bound.contains("first visible finding"), "{bound}");
        assert!(!view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-reused-profile".into(),
            agent_id: "agent-pending".into(),
            kind: AgentLiveEventKind::OutputDelta("must not leak".into()),
        }));
        assert!(!rendered(&view).contains("must not leak"));
    }

    #[test]
    fn open_provisional_transcript_rebinds_to_its_typed_spawn_run() {
        let mut view = AgentTranscriptView::live_unbound(
            "pending:call-spawn-1".into(),
            "Mock child review".into(),
            String::new(),
            None,
            "agents",
            80,
            24,
        );
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "reviewer@run-child".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                parent_run_id: Some("run-root".into()),
                depth: 1,
                spawn_tool_call_id: Some("call-spawn-1".into()),
                transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
            }),
        }));
        assert_eq!(view.agent_id, "reviewer@run-child");
        assert_eq!(view.run_id, "run-child");
        assert_eq!(
            view.transcript_target,
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal)
        );
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "reviewer@run-child".into(),
            kind: AgentLiveEventKind::OutputDelta("child evidence".into()),
        }));
        assert!(rendered(&view).contains("child evidence"));
    }

    #[test]
    fn live_agent_transcript_keeps_a_long_reply_to_its_visible_tail() {
        let mut view = AgentTranscriptView::live_unbound(
            "agent-1".into(),
            "Reviewer".into(),
            "run-child".into(),
            None,
            "agents",
            80,
            120,
        );
        assert!(
            view.refresh_agent_live_event(&AgentLiveEvent {
                run_id: "run-child".into(),
                agent_id: "agent-1".into(),
                kind: AgentLiveEventKind::OutputDelta(
                    (0..2_000)
                        .map(|index| format!("agent-line-{index}\n"))
                        .collect(),
                ),
            })
        );

        let output = rendered(&view);
        assert!(output.contains("agent-line-1999"), "{output}");
        assert!(
            !output.contains("agent-line-0\n"),
            "live suffix must stay bounded: {output}"
        );
    }

    #[test]
    fn live_run_remains_inspectable_before_transcript_location_arrives() {
        let mut view = AgentTranscriptView::live_unbound(
            "agent-1".into(),
            "Reviewer".into(),
            "run-child".into(),
            None,
            "agents",
            80,
            24,
        );
        assert!(
            rendered(&view).contains("Canonical transcript location pending"),
            "{}",
            rendered(&view)
        );

        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::OutputDelta("live finding before receipt".into()),
        }));
        assert!(
            rendered(&view).contains("live finding before receipt"),
            "{}",
            rendered(&view)
        );
    }

    #[test]
    fn receipt_location_automatically_loads_durable_history_for_an_open_live_transcript() {
        let mut view = AgentTranscriptView::live_unbound(
            "agent-1".into(),
            "Reviewer".into(),
            String::new(),
            None,
            "agents",
            80,
            24,
        );
        assert!(!view.bind_session("session-1"));
        assert!(view.refresh_agent_monitor(
            crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot::complete(vec![
                monitor_row_with_target(
                    crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                ),
            ]),
        ));
        assert_eq!(view.run_id, "run-child");
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    agent_id,
                    session_id,
                    run_id,
                    transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                    before_seq: None,
                },
                ..
            }) if agent_id == "agent-1" && session_id == "session-1" && run_id == "run-child"
        ));
    }

    #[test]
    fn session_binding_upgrades_the_open_live_transcript_without_changing_run() {
        let mut view = AgentTranscriptView::live_unbound(
            "agent-1".into(),
            "Reviewer".into(),
            "run-child".into(),
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal),
            "agents",
            80,
            24,
        );
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::OutputDelta("live review finding".into()),
        }));

        assert!(view.bind_session("session-1"));
        assert!(rendered(&view).contains("live review finding"));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    agent_id,
                    session_id,
                    run_id,
                    transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
                    before_seq: None,
                },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1" && session_id == "session-1" && run_id == "run-child"
        ));
        assert!(
            !view.bind_session("session-other"),
            "a transcript must never rebind a run to a different session"
        );
    }

    #[test]
    fn canonical_tool_call_and_result_pair_into_one_expandable_tool_cell() {
        let mut view = AgentTranscriptView::loading(
            "agent-1".into(),
            "Reviewer".into(),
            "session-1".into(),
            "run-child".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
            "agents",
            80,
            0,
        );
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![
                    astra_thin_client::SessionTranscriptItem {
                        session_id: "session-1".into(),
                        item_seq: -101,
                        run_id: Some("run-child".into()),
                        role: "assistant".into(),
                        content: String::new(),
                        reasoning: None,
                        reasoning_status: None,
                        tool_calls: vec![astra_thin_client::SessionTranscriptToolCall {
                            tool_use_id: "call-1".into(),
                            name: "read_file".into(),
                            arguments: "{\"path\":\"src/lib.rs\"}".into(),
                        }],
                        tool_result: None,
                        evidence: None,
                        source_event_id: Some("event-call-1".into()),
                        created_at: "2026-07-12T00:00:00".into(),
                    },
                    astra_thin_client::SessionTranscriptItem {
                        session_id: "session-1".into(),
                        item_seq: -102,
                        run_id: Some("run-child".into()),
                        role: "tool".into(),
                        content: "line one\nline two\nline three\nline four\nline five\nline six"
                            .into(),
                        reasoning: None,
                        reasoning_status: None,
                        tool_calls: Vec::new(),
                        tool_result: Some(astra_thin_client::SessionTranscriptToolResult {
                            tool_use_id: "call-1".into(),
                            name: Some("read_file".into()),
                            status: Some("success".into()),
                            duration_ms: Some(12),
                        }),
                        evidence: None,
                        source_event_id: Some("event-result-1".into()),
                        created_at: "2026-07-12T00:00:01".into(),
                    },
                ],
                page_refs: Vec::new(),
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::LocalJournalOnly,
        );

        assert_eq!(view.transcript_snapshot().item_count(), 1);
        let collapsed = rendered(&view);
        assert!(collapsed.contains("src/lib.rs"));
        assert_eq!(collapsed.matches("src/lib.rs").count(), 1);
        assert!(!collapsed.contains("line six"));

        view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(rendered(&view).contains("line six"));
    }

    #[test]
    fn agent_export_paginates_and_keeps_agent_identity_in_the_artifact() {
        let mut view = loading_view(100, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::LoadAgentTranscript {
                    before_seq: Some(7),
                    ..
                },
                ..
            })
        ));

        let older = transcript_page_with_assistant_lines(2);
        view.refresh_agent_transcript(AgentTranscriptUpdate::Loaded {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            page: older,
            replace: false,
            source: AgentTranscriptSource::DurableServer,
        });
        let Some(ViewActionRequest {
            action: BottomPaneViewAction::ExportTranscript { path, lines },
            ..
        }) = view.take_action_request()
        else {
            panic!("complete agent history must emit one export effect");
        };
        let body = lines.join("\n");
        assert!(body.contains("- Agent: Reviewer"), "{body}");
        assert!(body.contains("- Run: run-child"), "{body}");
        assert!(body.contains("Finding 0"), "{body}");
        assert!(body.contains("Found the race."), "{body}");
        assert!(path.ends_with("agent-run-child.md"), "{}", path.display());
    }

    #[test]
    fn agent_view_forwards_transcript_clipboard_actions() {
        let mut view = loading_view(100, 24);
        view.apply_page(page(), true, AgentTranscriptSource::DurableServer);
        view.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));

        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::CopyToClipboard { text, .. },
                ..
            }) if !text.is_empty()
        ));
    }
}
