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

#[derive(Debug, Clone)]
struct ToolCoverage {
    identity: ToolIdentity,
    compatible_result: bool,
    request_present: bool,
    result_index: Option<usize>,
    projection_index: usize,
    receipt: Option<crate::tui::agent_control_status::DelegationReceipt>,
}

#[derive(Default)]
struct CanonicalProjection {
    items: Vec<TranscriptItem>,
    tools: std::collections::HashMap<(Option<String>, String), ToolCoverage>,
    outputs: std::collections::HashSet<String>,
    reasoning: std::collections::HashSet<String>,
    evidence: std::collections::HashSet<String>,
}

#[derive(Debug, Clone)]
enum ToolIdentity {
    Unresolved,
    Known(String),
    Conflicting,
}

impl ToolIdentity {
    fn target(&self) -> Option<&str> {
        match self {
            Self::Known(target) => Some(target),
            _ => None,
        }
    }
    fn observe(&mut self, incoming: &str) -> bool {
        match self {
            Self::Unresolved => {
                *self = Self::Known(incoming.to_owned());
                true
            }
            Self::Known(previous) if previous != incoming => {
                *self = Self::Conflicting;
                false
            }
            _ => false,
        }
    }
}

fn compact_live_delegation(
    cell: &mut Option<Arc<ToolCell>>,
    receipt: &mut Option<crate::tui::agent_control_status::DelegationReceipt>,
    target: &str,
    request: &str,
) {
    if receipt.is_some() {
        return;
    }
    let Some(live) = cell.as_ref() else {
        return;
    };
    let status = match live.status {
        crate::tui::history_cell::tool::ToolStatus::Uncertain => Some("uncertain"),
        crate::tui::history_cell::tool::ToolStatus::Failed => Some("failed"),
        crate::tui::history_cell::tool::ToolStatus::Rejected => Some("rejected"),
        _ => None,
    };
    let projected = crate::tui::agent_control_status::delegation_receipt(
        status,
        live.output.as_deref().or(live.output_summary.as_deref()),
    );
    let live = Arc::make_mut(cell.as_mut().unwrap());
    live.name = target.to_owned();
    live.description = if request.is_empty() {
        "Agent control · Ctrl+G agents".into()
    } else {
        request.to_owned()
    };
    live.output = None;
    live.output_summary = Some(projected.text());
    *receipt = Some(projected);
}

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
        cell: Option<Arc<ToolCell>>,
        request: Arc<ToolCell>,
        identity: ToolIdentity,
        receipt: Option<crate::tui::agent_control_status::DelegationReceipt>,
    },
    Notice {
        text: String,
        evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
    },
}

#[derive(Debug)]
struct LiveTranscriptEntry {
    id: u64,
    item: LiveTranscriptItem,
}

#[derive(Debug, Default)]
struct LiveTranscript {
    items: Vec<LiveTranscriptEntry>,
    next_id: u64,
}

impl LiveTranscript {
    fn push(&mut self, item: LiveTranscriptItem) {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.items.push(LiveTranscriptEntry { id, item });
    }

    fn is_empty(&self) -> bool {
        self.items
            .iter()
            .all(|entry| matches!(entry.item, LiveTranscriptItem::Tool { cell: None, .. }))
    }

    fn reconcile_durable_items(
        &mut self,
        canonical: &CanonicalProjection,
        run_id: &str,
        items: &[astra_thin_client::SessionTranscriptItem],
        event: Option<&AgentLiveEventKind>,
    ) {
        self.items.retain_mut(|entry| match &mut entry.item {
            LiveTranscriptItem::Assistant(cell) => !cell
                .model_item_id()
                .is_some_and(|id| canonical.outputs.contains(id)),
            LiveTranscriptItem::Reasoning(cell) => !cell
                .model_item_id()
                .is_some_and(|id| canonical.reasoning.contains(id)),
            LiveTranscriptItem::Tool {
                tool_use_id,
                identity,
                cell,
                receipt,
                request,
            } => {
                // Compare large results only when this call or its loaded page changes.
                if let Some(event) = event {
                    match event {
                        AgentLiveEventKind::ToolStarted {
                            tool_use_id: id, ..
                        }
                        | AgentLiveEventKind::ToolCompleted {
                            tool_use_id: id, ..
                        } if id == tool_use_id => {}
                        _ => return true,
                    }
                }
                let Some(coverage) = canonical
                    .tools
                    .get(&(Some(run_id.to_owned()), tool_use_id.clone()))
                else {
                    return true;
                };
                match &coverage.identity {
                    ToolIdentity::Known(target) if identity.observe(target) => {
                        if is_delegation_name(target) {
                            compact_live_delegation(cell, receipt, target, &request.description);
                        } else if let Some(cell) = cell {
                            Arc::make_mut(cell).name = target.to_owned();
                        }
                    }
                    ToolIdentity::Conflicting => *identity = ToolIdentity::Conflicting,
                    _ => {}
                }
                if matches!(identity, ToolIdentity::Conflicting)
                    && cell.is_none()
                    && !request.description.is_empty()
                {
                    // A sticky conflict can no longer expose this request;
                    // retain only its identity sentinel across page changes.
                    *request = Arc::new(ToolCell::new_running("tool", String::new()));
                }
                let Some(identity) = identity.target() else {
                    return true;
                };
                let Some(live) = cell.as_ref() else {
                    return true;
                };
                if coverage.identity.target() != Some(identity) || !coverage.compatible_result {
                    return true;
                }
                let Some(result) = coverage.result_index.and_then(|index| items.get(index)) else {
                    return true;
                };
                let same_status = canonical
                    .items
                    .get(coverage.projection_index)
                    .and_then(TranscriptItem::tool_cell)
                    .is_some_and(|stored| stored.status == live.status);
                let covered = if live.status == crate::tui::history_cell::tool::ToolStatus::Running
                {
                    true
                } else if result.role == "tool_observation_preview" || !same_status {
                    false
                } else if is_delegation_name(identity) {
                    match (receipt.as_ref(), coverage.receipt.as_ref()) {
                        (Some(live), Some(stored)) => {
                            (live.outcome == stored.outcome)
                                && (live.detail.is_none() || live.detail == stored.detail)
                        }
                        _ => false,
                    }
                } else {
                    match live.output.as_deref() {
                        Some(output) => output == result.content,
                        None => live.output_summary.as_deref().is_none_or(|summary| {
                            result.content == summary
                                || result.content.lines().next() == Some(summary)
                        }),
                    }
                };
                let summary_covered = live.output_summary.as_deref().is_none_or(|summary| {
                    result.content == summary
                        || result.content.lines().next() == Some(summary)
                        || canonical
                            .items
                            .get(coverage.projection_index)
                            .and_then(TranscriptItem::tool_cell)
                            .and_then(|cell| cell.output_summary.as_deref())
                            == Some(summary)
                });
                if covered && !is_delegation_name(identity) && !summary_covered {
                    Arc::make_mut(cell.as_mut().unwrap()).output = None;
                } else if covered {
                    *cell = None;
                    *receipt = None;
                }
                true
            }
            LiveTranscriptItem::Notice {
                evidence: Some(evidence),
                ..
            } => !canonical.evidence.contains(&evidence.stable_key()),
            LiveTranscriptItem::Notice { evidence: None, .. } => true,
        });
        if event.is_none()
            || matches!(
                event,
                Some(
                    AgentLiveEventKind::ToolStarted { .. }
                        | AgentLiveEventKind::ToolCompleted { .. }
                )
            )
        {
            self.prune_covered_requests(canonical, run_id);
        }
    }

    fn prune_covered_requests(&mut self, canonical: &CanonicalProjection, run_id: &str) {
        const MAX_REQUESTS: usize = 128;
        const MAX_BYTES: usize = 512 * 1024;

        let mut kept = std::collections::HashSet::new();
        let mut bytes = 0usize;
        // Preserve the currently visible supplement first, then requests
        // already present in history, then the most recent off-page requests.
        for priority in 0..3 {
            for entry in self.items.iter().rev() {
                let LiveTranscriptItem::Tool {
                    tool_use_id,
                    cell: None,
                    request,
                    identity: ToolIdentity::Known(target),
                    ..
                } = &entry.item
                else {
                    continue;
                };
                let coverage = canonical
                    .tools
                    .get(&(Some(run_id.to_owned()), tool_use_id.clone()))
                    .filter(|coverage| coverage.identity.target() == Some(target));
                let item_priority = match coverage {
                    Some(coverage) if !coverage.request_present => 0,
                    Some(_) => 1,
                    None => 2,
                };
                if item_priority != priority {
                    continue;
                }
                let size = request
                    .description
                    .len()
                    .saturating_add(request.name.len())
                    .saturating_add(tool_use_id.len())
                    .saturating_add(target.len());
                if kept.len() < MAX_REQUESTS && size <= MAX_BYTES.saturating_sub(bytes) {
                    kept.insert(entry.id);
                    bytes += size;
                }
            }
        }
        self.items.retain(|entry| {
            !matches!(
                &entry.item,
                LiveTranscriptItem::Tool {
                    cell: None,
                    identity: ToolIdentity::Known(_),
                    ..
                }
            ) || kept.contains(&entry.id)
        });
    }

    fn finish_open_model_item(&mut self) {
        match self.items.last_mut().map(|entry| &mut entry.item) {
            Some(LiveTranscriptItem::Assistant(cell)) if cell.is_live() => cell.finalize(),
            Some(LiveTranscriptItem::Reasoning(cell)) if cell.is_live() => cell.finalize(),
            _ => {}
        }
    }

    fn finish_all_model_items(&mut self) {
        for entry in &mut self.items {
            match &mut entry.item {
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

    fn append_output(&mut self, text: &str, model_item_id: Option<&str>) {
        if text.is_empty() {
            return;
        }
        match self.items.last_mut().map(|entry| &mut entry.item) {
            Some(LiveTranscriptItem::Assistant(cell))
                if cell.is_live() && cell.model_item_id() == model_item_id =>
            {
                cell.push_delta(text)
            }
            _ => {
                self.finish_open_model_item();
                let mut cell = AssistantCell::new_streaming();
                cell.model_item_id = model_item_id.map(str::to_owned);
                cell.push_delta(text);
                self.push(LiveTranscriptItem::Assistant(cell));
            }
        }
    }

    fn append_reasoning(&mut self, text: &str, model_item_id: Option<&str>) {
        if text.is_empty() {
            return;
        }
        match self.items.last_mut().map(|entry| &mut entry.item) {
            Some(LiveTranscriptItem::Reasoning(cell))
                if cell.is_live() && cell.model_item_id() == model_item_id =>
            {
                cell.push_delta(text)
            }
            _ => {
                self.finish_open_model_item();
                let mut cell = ReasoningCell::new_streaming();
                cell.model_item_id = model_item_id.map(str::to_owned);
                cell.push_delta(text);
                self.push(LiveTranscriptItem::Reasoning(cell));
            }
        }
    }

    fn tool_started(&mut self, tool_use_id: String, name: String, description: String) {
        self.finish_open_model_item();
        let incoming = live_tool_identity(&name, &description);
        let request = match incoming.as_deref() {
            Some(target) if is_delegation_name(target) => {
                crate::tui::agent_control_status::compact_delegation_description(
                    &name,
                    &description,
                )
            }
            Some(_) => description,
            None => "Delegated or unresolved tool activity".into(),
        };
        if !tool_use_id.is_empty()
            && let Some(entry) = self.items.iter_mut().find(|entry| {
                matches!(&entry.item, LiveTranscriptItem::Tool { tool_use_id: id, .. } if id == &tool_use_id)
            })
            && let LiveTranscriptItem::Tool { identity, request: previous, cell, receipt, .. } = &mut entry.item
        {
            if let Some(target) = incoming.as_deref() {
                let resolved = identity.observe(target);
                if identity.target().is_some() && (resolved || previous.description.is_empty()) {
                    let source = Arc::make_mut(previous);
                    source.name = target.to_owned();
                    source.description = request.clone();
                    if let Some(cell) = cell {
                        let cell = Arc::make_mut(cell);
                        cell.name = target.to_owned();
                        cell.description = request;
                    }
                }
                if matches!(identity, ToolIdentity::Conflicting) && cell.is_none() {
                    *previous = Arc::new(ToolCell::new_running("tool", String::new()));
                }
                if identity.target().is_some_and(is_delegation_name) {
                    compact_live_delegation(cell, receipt, target, &previous.description);
                }
            }
            return;
        }
        let request = Arc::new(ToolCell::new_running(
            incoming.as_deref().unwrap_or(&name),
            request,
        ));
        self.push(LiveTranscriptItem::Tool {
            tool_use_id,
            cell: Some(request.clone()),
            request,
            identity: incoming.map_or(ToolIdentity::Unresolved, ToolIdentity::Known),
            receipt: None,
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
        known_target: Option<&str>,
    ) {
        let name = name.trim().to_owned();
        let incoming = (name != "invoke_tool")
            .then(|| live_tool_identity(&name, ""))
            .flatten();
        let index = (!tool_use_id.is_empty())
            .then(|| {
                self.items.iter().rposition(|entry|
            matches!(&entry.item, LiveTranscriptItem::Tool { tool_use_id: id, .. } if id == &tool_use_id)
        )
            })
            .flatten();
        let index = index.unwrap_or_else(|| {
            self.tool_started(
                tool_use_id,
                incoming
                    .as_ref()
                    .map(|_| name.as_str())
                    .or(known_target)
                    .unwrap_or(&name)
                    .to_owned(),
                String::new(),
            );
            if let Some(LiveTranscriptItem::Tool { request, .. }) =
                self.items.last_mut().map(|entry| &mut entry.item)
            {
                Arc::make_mut(request).description.clear();
            }
            self.items.len() - 1
        });
        let LiveTranscriptItem::Tool {
            cell,
            identity,
            receipt,
            request,
            ..
        } = &mut self.items[index].item
        else {
            unreachable!()
        };
        let delegated = identity.target().is_some_and(is_delegation_name)
            || incoming.as_deref().is_some_and(is_delegation_name)
            || known_target.is_some_and(is_delegation_name)
            || receipt.is_some();
        if let Some(target) = known_target {
            identity.observe(target);
        }
        if let Some(incoming) = incoming.as_deref() {
            identity.observe(incoming);
        }
        let cell = Arc::make_mut(cell.get_or_insert_with(|| {
            Arc::new(ToolCell::new_running(
                identity.target().unwrap_or(&name),
                request.description.clone(),
            ))
        }));
        if delegated {
            let projected_receipt = crate::tui::agent_control_status::delegation_receipt(
                Some(&status),
                output.as_deref().or(output_summary.as_deref()),
            );
            cell.complete(
                &status,
                duration_ms,
                String::new(),
                Some(projected_receipt.text()),
                None,
            );
            *receipt = Some(projected_receipt);
        } else {
            cell.complete(&status, duration_ms, description, output_summary, output);
        }
    }

    fn notice(
        &mut self,
        text: String,
        evidence: Option<astra_turn_types::AgentTranscriptEvidence>,
    ) {
        if !text.trim().is_empty() {
            self.finish_open_model_item();
            self.push(LiveTranscriptItem::Notice { text, evidence });
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
    ActivityLoaded {
        agent_id: String,
        run_id: String,
        page: Result<astra_thin_client::protocol::RunActivityPage, String>,
        replace: bool,
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
    activity: Option<astra_thin_client::protocol::RunActivityPage>,
    activity_items: Vec<TranscriptItem>,
    canonical: CanonicalProjection,
    activity_error: Option<String>,
    live: LiveTranscript,
    pending_transcript_commit: Option<PendingTranscriptCommit>,
    /// A terminal live event is emitted only after the local or server runner
    /// has attempted to persist its canonical transcript. Refresh that page
    /// once so an already-open conversation does not remain a live-only
    /// suffix after the run finishes. This is deliberately separate from
    /// live/durable item reconciliation: until both lanes share an item id,
    /// the view must not erase output based on equal text.
    terminal_refresh_requested: bool,
    local_root_live: Option<super::root_transcript_view::LocalLiveItem>,
    root_refresh_pending: bool,
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
            activity: None,
            activity_items: Vec::new(),
            canonical: CanonicalProjection::default(),
            activity_error: None,
            pending_transcript_commit: None,
            terminal_refresh_requested: false,
            local_root_live: None,
            root_refresh_pending: false,
            viewport_width,
            completed: false,
            reopen: Some(reopen.into()),
            return_action: BottomPaneViewAction::ReturnToConversationNavigator,
            pending_action: None,
            export_pending: false,
            export_seen_cursors: std::collections::HashSet::new(),
        };
        view.transcript
            .set_activity_status(Some("Syncing durable agent history…".into()));
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
            activity: None,
            activity_items: Vec::new(),
            canonical: CanonicalProjection::default(),
            activity_error: None,
            pending_transcript_commit: None,
            terminal_refresh_requested: false,
            local_root_live: None,
            root_refresh_pending: false,
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
        self.canonical = project_durable_transcript(&self.items, true);
        self.next_before_seq = page.next_before_seq;
        self.has_more = page.has_more;
        if self
            .local_root_live
            .as_ref()
            .is_some_and(|live| live.is_represented(&self.items))
        {
            self.local_root_live = None;
        }
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
        self.live
            .reconcile_durable_items(&self.canonical, &self.run_id, &self.items, None);
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
        if self.root_refresh_pending {
            self.root_refresh_pending = false;
            self.request_load(None);
        } else if self.export_pending {
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
        if self.items.is_empty()
            && self.live.is_empty()
            && self.local_root_live.is_none()
            && self.activity.is_none()
            && self.activity_error.is_none()
        {
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

        let mut projected = self.canonical.items.clone();
        if let Some(activity) = &self.activity {
            projected.push(TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(1 << 61),
                vec![Line::from(
                    "Recorded tool activity (partial coverage) · A older · R refresh",
                )],
                0,
            ));
            if activity.events.is_empty() {
                projected.push(TranscriptItem::rendered(
                    TranscriptItemId::from_widget_id((1 << 61) + 2),
                    vec![Line::from("No recorded activity in this range. Live-only or dropped events may be unavailable.")], 0));
            }
            projected.extend(self.activity_items.iter().cloned());
        }
        if let Some(error) = &self.activity_error {
            projected.push(TranscriptItem::rendered(
                TranscriptItemId::from_widget_id((1 << 61) + 1),
                vec![Line::from(error.clone())],
                0,
            ));
        }
        self.append_live_projection(&mut projected);
        if let Some(live) = &self.local_root_live {
            projected.push(TranscriptItem::rendered(
                TranscriptItemId::from_widget_id(u64::MAX - 1),
                vec![Line::from(
                    "Live local projection · awaiting durable reconciliation",
                )],
                0,
            ));
            projected.push(live.item.clone());
        }
        TranscriptSnapshot::new(projected)
    }

    fn append_live_projection(&self, projected: &mut Vec<TranscriptItem>) {
        const LIVE_ID_BASE: u64 = 1 << 63;
        let activity_events = self
            .activity
            .as_ref()
            .into_iter()
            .flat_map(|page| &page.events)
            .filter_map(|entry| entry.event.as_ref());
        let mut private_ids = std::collections::HashSet::new();
        for event in activity_events {
            if matches!(
                event.get("type").and_then(serde_json::Value::as_str),
                Some("stream_gap" | "durable_events_compacted")
            ) {
                continue;
            }
            let Some(id) = event
                .get("call_id")
                .or_else(|| event.get("tool_call_id"))
                .and_then(serde_json::Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            let (_, target, private) = activity_identity(event);
            if is_delegation_name(&target)
                || (private
                    && event.get("type").and_then(serde_json::Value::as_str)
                        == Some("tool_call_start"))
            {
                private_ids.insert(id);
            }
        }

        let mut live_items = Vec::new();
        let mut has_live_content = false;
        let mut has_live_tools = false;
        for entry in &self.live.items {
            let item = &entry.item;
            let before = live_items.len();
            let id = TranscriptItemId::from_widget_id(LIVE_ID_BASE.saturating_add(entry.id));
            match item {
                LiveTranscriptItem::Assistant(cell) if cell.is_live() => live_items.push(
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
                    live_items.push(TranscriptItem::committed(id, Arc::new(cell.clone()), 1))
                }
                LiveTranscriptItem::Reasoning(cell) => {
                    live_items.push(TranscriptItem::reasoning(id, cell.clone(), 1));
                }
                LiveTranscriptItem::Tool {
                    tool_use_id,
                    cell,
                    request,
                    identity,
                    receipt,
                } => {
                    let coverage = identity.target().and_then(|identity| {
                        self.canonical
                            .tools
                            .get(&(Some(self.run_id.clone()), tool_use_id.clone()))
                            .filter(|coverage| coverage.identity.target() == Some(identity))
                    });
                    let protected = identity.target().is_none()
                        || (private_ids.contains(tool_use_id.as_str())
                            && !identity.target().is_some_and(is_delegation_name));
                    let protected_message = if matches!(identity, ToolIdentity::Conflicting) {
                        "Conflicting tool identity · details unavailable"
                    } else {
                        "Delegated or unresolved tool activity"
                    };
                    if cell.is_none() {
                        // A request fallback supplements a currently loaded result;
                        // it does not bypass the conversation page window.
                        if coverage.is_none() {
                            continue;
                        }
                        if request.description.is_empty()
                            && coverage.is_some_and(|coverage| coverage.result_index.is_some())
                        {
                            continue;
                        }
                        if coverage.is_some_and(|coverage| coverage.request_present) {
                            continue;
                        }
                        if protected {
                            live_items.push(TranscriptItem::rendered_kind(
                                id,
                                TranscriptItemKind::Tool,
                                vec![Line::from(protected_message)],
                                1,
                            ));
                        } else {
                            live_items
                                .push(TranscriptItem::observed_tool_request(id, request.clone()));
                        }
                    } else {
                        if let Some(coverage) = coverage.filter(|coverage| {
                            coverage.request_present && coverage.result_index.is_none()
                        }) {
                            if !protected
                                && cell.as_ref().is_some_and(|cell| {
                                    cell.status
                                        == crate::tui::history_cell::tool::ToolStatus::Running
                                })
                                && let Some(running) = self
                                    .canonical
                                    .items
                                    .get(coverage.projection_index)
                                    .and_then(TranscriptItem::running_request)
                            {
                                projected[coverage.projection_index] = running;
                                continue;
                            }
                        }
                        if identity.target().is_none()
                            && let Some(receipt) = receipt
                        {
                            live_items.push(TranscriptItem::rendered_kind(
                                id,
                                TranscriptItemKind::Tool,
                                vec![Line::from(format!(
                                    "Conflicting tool identity · {}",
                                    receipt.text()
                                ))],
                                1,
                            ));
                        } else if identity.target().is_none()
                            || (private_ids.contains(tool_use_id.as_str())
                                && !identity.target().is_some_and(is_delegation_name))
                        {
                            live_items.push(TranscriptItem::rendered_kind(
                                id,
                                TranscriptItemKind::Tool,
                                vec![Line::from(protected_message)],
                                1,
                            ));
                        } else {
                            live_items.push(TranscriptItem::tool(
                                id,
                                cell.as_ref().unwrap().clone(),
                                1,
                            ));
                        }
                    }
                }
                LiveTranscriptItem::Notice {
                    text: _,
                    evidence: Some(evidence),
                } => live_items.push(TranscriptItem::rendered_kind(
                    id,
                    transcript_evidence_kind(evidence),
                    transcript_evidence_lines(evidence),
                    1,
                )),
                LiveTranscriptItem::Notice {
                    text,
                    evidence: None,
                } => live_items.push(TranscriptItem::rendered_kind(
                    id,
                    TranscriptItemKind::Agent,
                    vec![Line::from(format!("• {text}"))],
                    1,
                )),
            }
            if live_items.len() > before && !matches!(item, LiveTranscriptItem::Notice { .. }) {
                has_live_content = true;
                has_live_tools |= matches!(item, LiveTranscriptItem::Tool { .. });
            }
        }
        if has_live_content {
            projected.push(TranscriptItem::rendered_kind(
                TranscriptItemId::from_widget_id(LIVE_ID_BASE - 1),
                if has_live_tools {
                    TranscriptItemKind::Tool
                } else {
                    TranscriptItemKind::System
                },
                vec![Line::from("Live activity · not in loaded history")],
                0,
            ));
        }
        projected.extend(live_items);
    }

    fn rebuild_transcript(&mut self) {
        self.transcript
            .replace_with(self.transcript_snapshot(), self.viewport_width);
    }

    fn apply_live_event(&mut self, event: &AgentLiveEvent) -> bool {
        if event.run_id.trim().is_empty() {
            return false;
        }
        if self.run_id.trim().is_empty() && event.agent_id != self.agent_id {
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
            AgentLiveEventKind::OutputDelta { .. }
                | AgentLiveEventKind::ThinkingDelta { .. }
                | AgentLiveEventKind::ToolStarted { .. }
                | AgentLiveEventKind::ToolCompleted { .. }
        ) {
            // A resumed run can use the same durable run identity. Its next
            // terminal edge must request a fresh canonical page again.
            self.terminal_refresh_requested = false;
        }
        match &event.kind {
            AgentLiveEventKind::OutputDelta {
                model_item_id,
                text,
            } => self.live.append_output(text, model_item_id.as_deref()),
            AgentLiveEventKind::ThinkingDelta {
                model_item_id,
                text,
            } => self.live.append_reasoning(text, model_item_id.as_deref()),
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
                self.canonical
                    .tools
                    .get(&(Some(self.run_id.clone()), tool_use_id.clone()))
                    .and_then(|coverage| coverage.identity.target()),
            ),
            AgentLiveEventKind::Signal(signal) => {
                if matches!(signal, AgentLiveSignal::ExecutionWaiting { .. }) {
                    // The canonical run remains resumable, but this executor
                    // has released it. Freeze the current suffix so the UI
                    // does not keep animating output that can no longer
                    // arrive; a resumed delta opens a new streaming cell.
                    self.live.finish_all_model_items();
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
                    ..
                } = signal
                {
                    self.live.finish_all_model_items();
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
                    if self.session_id.is_some() {
                        self.terminal_refresh_requested = true;
                        if self.loading {
                            self.root_refresh_pending = true;
                        } else {
                            self.request_load(None);
                        }
                    }
                }
                if let Some((summary, evidence)) = live_signal_transcript_notice(signal) {
                    self.live.notice(summary, evidence);
                }
            }
            AgentLiveEventKind::Status { text } => self.live.notice(text.clone(), None),
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
                // Local sub-runners and the durable server attempt transcript
                // persistence before publishing this terminal lifecycle edge.
                // Refresh exactly once; the I/O remains an async view action
                // and a failed refresh preserves the visible live suffix.
                if !self.terminal_refresh_requested
                    && self.session_id.is_some()
                    && self.transcript_target.is_some()
                {
                    self.terminal_refresh_requested = true;
                    if self.loading {
                        self.root_refresh_pending = true;
                    } else {
                        self.request_load(None);
                    }
                }
            }
        }
        self.live.reconcile_durable_items(
            &self.canonical,
            &self.run_id,
            &self.items,
            Some(&event.kind),
        );
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

fn tool_display_content(item: &astra_thin_client::SessionTranscriptItem, compact: bool) -> String {
    let mut content = if compact {
        crate::tui::agent_control_status::compact_delegation_result(
            item.tool_result
                .as_ref()
                .and_then(|result| result.status.as_deref()),
            Some(&item.content),
        )
    } else {
        item.content.clone()
    };
    if let Some(result) = &item.tool_result {
        astra_turn_core::tool::result::advisory::append_display_guidance(
            &mut content,
            &result.runtime_advisories,
        );
    }
    content
}

/// Render durable conversation items for any root or delegated run through
/// the shared transcript browser. Identity comes from the typed item envelope;
/// this projection never compares visible text to reconcile records.
pub(crate) fn durable_transcript_items(
    items: &[astra_thin_client::SessionTranscriptItem],
) -> Vec<TranscriptItem> {
    project_durable_transcript(items, false).items
}

fn project_durable_transcript(
    items: &[astra_thin_client::SessionTranscriptItem],
    collect_coverage: bool,
) -> CanonicalProjection {
    let mut canonical = CanonicalProjection::default();
    if collect_coverage {
        for item in items {
            if let Some(evidence) = &item.evidence {
                canonical.evidence.insert(evidence.stable_key());
            }
            if item.role == "assistant"
                && let Some(id) = &item.model_item_id
                && !id.is_empty()
            {
                if !item.content.is_empty() {
                    canonical.outputs.insert(id.clone());
                }
                if item.reasoning.as_ref().is_some_and(|text| !text.is_empty()) {
                    canonical.reasoning.insert(id.clone());
                }
            }
        }
    }

    let tool_results = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let result = item.tool_result.as_ref()?;
            (!result.tool_use_id.is_empty()).then_some((
                (item.run_id.as_deref(), result.tool_use_id.as_str()),
                (index, item),
            ))
        })
        .collect::<std::collections::HashMap<_, _>>();
    let paired_tool_result_seqs = items
        .iter()
        .flat_map(|item| {
            item.tool_calls.iter().filter_map(|call| {
                tool_results
                    .get(&(item.run_id.as_deref(), call.tool_use_id.as_str()))
                    .map(|(_, result)| result.item_seq)
            })
        })
        .collect::<std::collections::HashSet<_>>();
    let mut projected = Vec::new();
    for (item_index, item) in items.iter().enumerate() {
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
            "assistant" | "tool_observation" | "tool_observation_preview"
                if item.role == "assistant" || !item.tool_calls.is_empty() =>
            {
                if item.role == "assistant" && !item.content.trim().is_empty() {
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
                    let result_entry = tool_results
                        .get(&(item.run_id.as_deref(), call.tool_use_id.as_str()))
                        .copied();
                    let result_item = result_entry.map(|(_, item)| item);
                    let target = live_tool_identity(&call.name, &call.arguments);
                    let conflict = result_item
                        .and_then(|item| item.tool_result.as_ref())
                        .and_then(|result| result.name.as_deref())
                        .is_some_and(|name| {
                            name.trim() != "invoke_tool" && target.as_deref() != Some(name.trim())
                        });
                    // A delegation's prompt and result belong to the child run.
                    // The invoking run needs a compact control receipt only.
                    let delegation = target
                        .as_deref()
                        .filter(|target| is_delegation_name(target));
                    let mut description = if delegation.is_some() {
                        crate::tui::agent_control_status::compact_delegation_description(
                            &call.name,
                            &call.arguments,
                        )
                    } else {
                        call.arguments.clone()
                    };
                    if item.role != "assistant" {
                        description = format!("Observed locally · {description}");
                    }
                    if let Some(result_item) = result_item {
                        if result_item.role == "tool_observation_preview" {
                            description = format!("Partial retained output · {description}");
                        }
                        if item.role == "assistant"
                            && matches!(
                                result_item.role.as_str(),
                                "tool_observation" | "tool_observation_preview"
                            )
                        {
                            description = format!("Observed locally · {description}");
                        }
                    }
                    let receipt = delegation.as_ref().and_then(|_| {
                        result_item.map(|item| {
                            crate::tui::agent_control_status::delegation_receipt(
                                item.tool_result
                                    .as_ref()
                                    .and_then(|result| result.status.as_deref()),
                                Some(&item.content),
                            )
                        })
                    });
                    let mut cell = crate::tui::history_cell::tool::ToolCell::new_running(
                        if result_item.is_none() {
                            target.as_deref().unwrap_or(&call.name)
                        } else {
                            &call.name
                        },
                        description.clone(),
                    );
                    if let Some(result_item) = result_item {
                        let result = result_item.tool_result.as_ref().unwrap();
                        cell.complete(
                            result.status.as_deref().unwrap_or("uncertain"),
                            result.duration_ms.unwrap_or_default(),
                            description,
                            if conflict {
                                Some("Conflicting tool identity · details unavailable".into())
                            } else if delegation.is_some() {
                                receipt.as_ref().map(|receipt| receipt.text())
                            } else {
                                result_item.content.lines().next().map(|line| {
                                    if result_item.role == "tool_observation_preview" {
                                        format!("Partial retained output · {line}")
                                    } else {
                                        line.to_string()
                                    }
                                })
                            },
                            (!conflict
                                && (delegation.is_none() || !result.runtime_advisories.is_empty()))
                            .then(|| tool_display_content(result_item, delegation.is_some())),
                        );
                    }
                    if collect_coverage && !call.tool_use_id.is_empty() {
                        let result = result_item.and_then(|item| item.tool_result.as_ref());
                        let compatible = target.is_some()
                            && result.is_some_and(|result| {
                                result.name.as_deref().is_none_or(|name| {
                                    name.trim() == "invoke_tool"
                                        || Some(name.trim()) == target.as_deref()
                                })
                            });
                        let mut identity =
                            target.map_or(ToolIdentity::Unresolved, ToolIdentity::Known);
                        if let Some(result_target) = result
                            .and_then(|result| result.name.as_deref())
                            .and_then(|name| live_tool_identity(name, ""))
                        {
                            identity.observe(&result_target);
                        }
                        let coverage =
                            ToolCoverage {
                                projection_index: projected.len()
                                    + usize::from(result_item.is_some_and(|item| {
                                        item.role == "tool_observation_preview"
                                    })),
                                request_present: true,
                                result_index: result_entry.map(|(index, _)| index),
                                identity,
                                compatible_result: compatible,
                                receipt,
                            };
                        canonical
                            .tools
                            .entry((item.run_id.clone(), call.tool_use_id.clone()))
                            .and_modify(|old| {
                                match &coverage.identity {
                                    ToolIdentity::Known(target) => {
                                        old.identity.observe(target);
                                    }
                                    ToolIdentity::Conflicting => {
                                        old.identity = ToolIdentity::Conflicting
                                    }
                                    ToolIdentity::Unresolved => {}
                                }
                                if matches!(old.identity, ToolIdentity::Conflicting) {
                                    old.compatible_result = false;
                                }
                            })
                            .or_insert(coverage);
                    }
                    let component = if call.tool_use_id.is_empty() {
                        format!("tool:{index}")
                    } else {
                        format!("tool:{}", call.tool_use_id)
                    };
                    if result_item.is_some_and(|item| item.role == "tool_observation_preview") {
                        projected.push(TranscriptItem::rendered_kind(
                            TranscriptItemId::from_canonical(
                                canonical_id.clone(),
                                format!("{component}:partial"),
                            ),
                            TranscriptItemKind::Tool,
                            vec![Line::from("Partial retained output")],
                            0,
                        ));
                    }
                    let tool = TranscriptItem::tool(
                        TranscriptItemId::from_canonical(canonical_id.clone(), component),
                        cell,
                        1,
                    );
                    projected.push(if result_item.is_none() {
                        tool.tool_request().expect("tool projection has a request")
                    } else {
                        tool
                    });
                }
            }
            "tool" | "tool_observation" | "tool_observation_preview" => {
                if paired_tool_result_seqs.contains(&item.item_seq) {
                    continue;
                }
                let result = item.tool_result.as_ref();
                let delegation = result
                    .and_then(|result| result.name.as_deref())
                    .map(str::trim)
                    .is_some_and(|name| matches!(name, "agent" | "agent_fanout"));
                // An unpaired deferred carrier cannot be resolved safely: its
                // initiating call may be on another page. Keep its payload out
                // of this transcript rather than guessing its logical target.
                let unresolved_carrier = result
                    .and_then(|result| result.name.as_deref())
                    .map(str::trim)
                    .is_none_or(|name| matches!(name, "invoke_tool" | "tool" | ""));
                if collect_coverage
                    && let Some(result) = result
                    && !result.tool_use_id.is_empty()
                    && !unresolved_carrier
                {
                    if let Some(target) = result
                        .name
                        .as_deref()
                        .and_then(|name| live_tool_identity(name, ""))
                    {
                        canonical
                            .tools
                            .entry((item.run_id.clone(), result.tool_use_id.clone()))
                            .or_insert(ToolCoverage {
                                projection_index: projected.len()
                                    + usize::from(item.role == "tool_observation_preview"),
                                request_present: false,
                                result_index: Some(item_index),
                                identity: ToolIdentity::Known(target),
                                compatible_result: true,
                                receipt: delegation.then(|| {
                                    crate::tui::agent_control_status::delegation_receipt(
                                        result.status.as_deref(),
                                        Some(&item.content),
                                    )
                                }),
                            });
                    }
                }

                let mut cell = crate::tui::history_cell::tool::ToolCell::new_running(
                    result
                        .and_then(|result| result.name.as_deref())
                        .unwrap_or("tool"),
                    "Tool result",
                );
                cell.complete(
                    result
                        .and_then(|result| result.status.as_deref())
                        .unwrap_or("uncertain"),
                    result
                        .and_then(|result| result.duration_ms)
                        .unwrap_or_default(),
                    match item.role.as_str() {
                        "tool_observation" => "Observed locally",
                        "tool_observation_preview" => "Observed locally · partial retained output",
                        _ => "Tool result · request not in loaded page",
                    }
                    .into(),
                    if unresolved_carrier {
                        Some("Deferred tool result · details unavailable on this page".into())
                    } else if delegation {
                        Some(crate::tui::agent_control_status::compact_delegation_result(
                            result.and_then(|result| result.status.as_deref()),
                            Some(&item.content),
                        ))
                    } else {
                        item.content.lines().next().map(ToString::to_string)
                    },
                    (!unresolved_carrier
                        && (!delegation
                            || result.is_some_and(|result| !result.runtime_advisories.is_empty())))
                    .then(|| tool_display_content(item, delegation)),
                );
                if item.role == "tool_observation_preview" {
                    projected.push(TranscriptItem::rendered_kind(
                        TranscriptItemId::from_canonical(canonical_id, "partial"),
                        TranscriptItemKind::Tool,
                        vec![Line::from("Partial retained output")],
                        0,
                    ));
                }
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
    canonical.items = projected;
    canonical
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
        if self.transcript.is_search_active() {
            self.transcript.handle_key(key);
            return;
        }
        match key.code {
            KeyCode::Left => {
                if !self.transcript.collapse_current_item() {
                    self.pending_action = Some(ViewActionRequest {
                        action: self.return_action.clone(),
                        disposition: ViewActionDisposition::KeepOpen,
                    });
                }
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
            KeyCode::Char('a' | 'A') if !self.loading => {
                if let (Some(session_id), Some(before)) = (
                    self.session_id.clone(),
                    self.activity
                        .as_ref()
                        .and_then(|page| page.next_before_event_idx),
                ) {
                    self.loading = true;
                    self.pending_action = Some(ViewActionRequest {
                        action: BottomPaneViewAction::LoadAgentActivity {
                            agent_id: self.agent_id.clone(),
                            session_id,
                            run_id: self.run_id.clone(),
                            before_event_idx: Some(before),
                        },
                        disposition: ViewActionDisposition::KeepOpen,
                    });
                }
            }
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

    fn refresh_root_transcript_live(
        &mut self,
        binding: Option<(&str, &str)>,
        item: Option<TranscriptItem>,
    ) -> bool {
        let Some((session_id, run_id)) = binding else {
            return false;
        };
        if self.session_id.as_deref() != Some(session_id)
            || self.run_id != run_id
            || self.transcript_target
                != Some(crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer)
        {
            return false;
        }
        match (item, self.local_root_live.as_mut()) {
            (Some(item), _) => {
                let settled = item.is_settled_tool();
                self.local_root_live =
                    Some(super::root_transcript_view::LocalLiveItem { item, settled });
                if !settled {
                    self.terminal_refresh_requested = false;
                } else if !self.terminal_refresh_requested {
                    self.terminal_refresh_requested = true;
                    if self.loading {
                        self.root_refresh_pending = true;
                    } else {
                        self.request_load(None);
                    }
                }
            }
            (None, Some(live)) if !live.settled => {
                live.settled = true;
                if !self.terminal_refresh_requested {
                    self.terminal_refresh_requested = true;
                    if self.loading {
                        self.root_refresh_pending = true;
                    } else {
                        self.request_load(None);
                    }
                }
            }
            _ => return false,
        }
        if self
            .local_root_live
            .as_ref()
            .is_some_and(|live| live.is_represented(&self.items))
        {
            self.local_root_live = None;
        }
        self.rebuild_transcript();
        true
    }

    fn refresh_agent_transcript(&mut self, update: AgentTranscriptUpdate) -> bool {
        match update {
            AgentTranscriptUpdate::ActivityLoaded {
                agent_id,
                run_id,
                page,
                replace,
            } if agent_id == self.agent_id && run_id == self.run_id => {
                self.loading = false;
                match page {
                    Ok(mut page)
                        if page.run_id == self.run_id
                            && self.session_id.as_deref() == Some(page.session_id.as_str())
                            && !page.source_run_id.is_empty()
                            && self
                                .activity
                                .as_ref()
                                .is_none_or(|old| old.source_run_id == page.source_run_id) =>
                    {
                        if !replace && let Some(old) = self.activity.take() {
                            page.events.extend(old.events);
                            page.events.sort_by_key(|entry| entry.event_idx);
                            page.events.dedup_by_key(|entry| entry.event_idx);
                        }
                        // Retain a bounded inspection window. Paging older drops
                        // the newest entries; R returns to the latest page.
                        page.events.truncate(128);
                        let mut starts = std::collections::HashMap::<String, (String, bool)>::new();
                        let mut seen = std::collections::HashMap::new();
                        let mut conflicts = std::collections::HashSet::new();
                        for event in page.events.iter().filter_map(|entry| entry.event.as_ref()) {
                            if matches!(
                                event.get("type").and_then(serde_json::Value::as_str),
                                Some("tool_call" | "tool_call_start")
                            ) {
                                if let Some(id) = event
                                    .get("call_id")
                                    .or_else(|| event.get("tool_call_id"))
                                    .or_else(|| event.pointer("/tool_call/id"))
                                    .and_then(serde_json::Value::as_str)
                                    .filter(|id| !id.is_empty())
                                {
                                    let (_, identity, _) = activity_identity(event);
                                    if seen
                                        .insert(id, identity.clone())
                                        .is_some_and(|old| old != identity)
                                    {
                                        conflicts.insert(id);
                                    }
                                }
                            }
                        }
                        self.activity_items = page
                            .events
                            .iter()
                            .map(|entry| {
                                let event = entry.event.as_ref();
                                let mut inferred_name = None;
                                let mut protected = false;
                                if let Some(event) = event {
                                    let kind = event
                                        .get("type")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default();
                                    if matches!(kind, "stream_gap" | "durable_events_compacted") {
                                        // Keep known delegation identities across a gap so a
                                        // conflicting result cannot expose child content.
                                        starts.retain(|_, (_, private)| *private);
                                    }
                                    let id = event
                                        .get("call_id")
                                        .or_else(|| event.get("tool_call_id"))
                                        .or_else(|| event.pointer("/tool_call/id"))
                                        .and_then(serde_json::Value::as_str)
                                        .filter(|id| !id.is_empty());
                                    if let Some(id) = id {
                                        let (_, identity, private) = activity_identity(event);
                                        if matches!(kind, "tool_call" | "tool_call_start") {
                                            starts
                                                .entry(id.to_string())
                                                .and_modify(|entry| {
                                                    if entry.0 != identity || entry.1 != private {
                                                        entry.1 = true;
                                                    }
                                                })
                                                .or_insert((identity.clone(), private));
                                            if let Some((start, private)) = starts.get(id) {
                                                protected = *start != identity
                                                    || (*private && !is_delegation_name(start));
                                            }
                                        } else if matches!(kind, "tool_result" | "tool_call_end") {
                                            if let Some((start, was_private)) = starts.get(id) {
                                                let unresolved_carrier = identity == "invoke_tool";
                                                if (identity == "tool" || unresolved_carrier)
                                                    && (!*was_private || is_delegation_name(start))
                                                {
                                                    inferred_name = Some(start.clone());
                                                } else {
                                                    protected = (*was_private
                                                        && !is_delegation_name(start))
                                                        || *start != identity;
                                                }
                                            }
                                        }
                                    }
                                }
                                protected |= event
                                    .and_then(|event| {
                                        event
                                            .get("call_id")
                                            .or_else(|| event.get("tool_call_id"))
                                            .or_else(|| event.pointer("/tool_call/id"))
                                    })
                                    .and_then(serde_json::Value::as_str)
                                    .is_some_and(|id| conflicts.contains(id));
                                let inferred = inferred_name.and_then(|name| {
                                    let mut event = event?.clone();
                                    event.as_object_mut()?.insert("tool".into(), name.into());
                                    Some(event)
                                });
                                let text = activity_event_text_with_protection(
                                    inferred.as_ref().or(event),
                                    protected,
                                );
                                TranscriptItem::activity(
                                    TranscriptItemId::from_canonical(
                                        format!(
                                            "activity:{}:{}",
                                            page.source_run_id, entry.event_idx
                                        ),
                                        "event",
                                    ),
                                    if entry
                                        .event
                                        .as_ref()
                                        .and_then(|event| event.get("type"))
                                        .and_then(serde_json::Value::as_str)
                                        .is_some_and(|kind| {
                                            matches!(
                                                kind,
                                                "stream_gap" | "durable_events_compacted"
                                            )
                                        })
                                    {
                                        TranscriptItemKind::System
                                    } else {
                                        TranscriptItemKind::Tool
                                    },
                                    text,
                                )
                            })
                            .collect();
                        self.activity = Some(page);
                        self.activity_error = None;
                    }
                    Ok(_) => {
                        self.activity_error =
                            Some("Activity identity mismatch; history was not replaced.".into())
                    }
                    Err(error) => {
                        self.activity_error = Some(format!("Tool activity unavailable: {error}"))
                    }
                }
                self.rebuild_transcript();
                if self.root_refresh_pending {
                    self.root_refresh_pending = false;
                    self.request_load(None);
                } else if self.export_pending {
                    self.continue_export();
                }
            }
            AgentTranscriptUpdate::Loaded {
                agent_id,
                run_id,
                page,
                replace,
                source,
            } if agent_id == self.agent_id && run_id == self.run_id => {
                self.apply_page(page, replace, source);
                if replace
                    && !self.loading
                    && self.pending_action.is_none()
                    && !self.export_pending
                    && self.transcript_target
                        == Some(
                            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                        )
                    && let Some(session_id) = self.session_id.clone()
                {
                    self.loading = true;
                    self.pending_action = Some(ViewActionRequest {
                        action: BottomPaneViewAction::LoadAgentActivity {
                            agent_id: self.agent_id.clone(),
                            session_id,
                            run_id: self.run_id.clone(),
                            before_event_idx: None,
                        },
                        disposition: ViewActionDisposition::KeepOpen,
                    });
                }
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
                if self.root_refresh_pending {
                    self.root_refresh_pending = false;
                    self.request_load(None);
                }
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
            if self.run_id.trim().is_empty() {
                row.agent_id == self.agent_id
                    || pending_tool_call_id.is_some_and(|tool_call_id| {
                        row.spawn_tool_call_id.as_deref() == Some(tool_call_id)
                    })
            } else {
                row.run_id.as_deref() == Some(self.run_id.as_str())
            }
        });
        let Some(row) = matched_row else {
            return false;
        };
        if self.run_id.trim().is_empty() && row.agent_id != self.agent_id {
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
        self.run_id.trim().is_empty()
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
        if !self.loading {
            self.transcript.set_activity_status(Some(
                "Live activity incomplete · R refresh durable history".to_string(),
            ));
        }
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

fn is_delegation_name(name: &str) -> bool {
    matches!(name, "agent" | "agent_fanout")
}

fn live_tool_identity(name: &str, arguments: &str) -> Option<String> {
    let (name, private) = activity_tool_identity(name, arguments);
    (!private || is_delegation_name(&name)).then_some(name)
}

fn activity_identity(event: &serde_json::Value) -> (String, String, bool) {
    let name = [
        event.get("tool"),
        event.get("tool_name"),
        event.pointer("/tool_call/function/name"),
    ]
    .into_iter()
    .flatten()
    .filter_map(serde_json::Value::as_str)
    .find(|name| !name.trim().is_empty())
    .unwrap_or("tool")
    .trim();
    // Delegation arguments/results belong to the delegated run. An unresolved
    // carrier is treated the same way until its target is known.
    let args = event
        .get("arguments")
        .filter(|value| !value.is_null())
        .or_else(|| event.pointer("/tool_call/function/arguments"))
        .or_else(|| event.get("description"));
    let args = args
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string())
        })
        .unwrap_or_default();
    let completion = matches!(
        event.get("type").and_then(serde_json::Value::as_str),
        Some("tool_result" | "tool_call_end")
    );
    let (identity, private) = activity_tool_identity(
        name,
        if completion && name == "invoke_tool" {
            ""
        } else {
            &args
        },
    );
    (name.to_string(), identity, private)
}

fn activity_tool_identity(name: &str, args: &str) -> (String, bool) {
    let name = name.trim();
    let deferred_target = (name == "invoke_tool")
        .then(|| {
            serde_json::from_str::<serde_json::Value>(args)
                .ok()
                .and_then(|value| {
                    value
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(|name| name.trim().to_owned())
                })
        })
        .flatten();
    let private = matches!(name, "agent" | "agent_fanout" | "tool" | "")
        || (name == "invoke_tool"
            && deferred_target.as_deref().is_none_or(|target| {
                target.is_empty() || matches!(target, "agent" | "agent_fanout")
            }));
    (deferred_target.unwrap_or_else(|| name.to_string()), private)
}

/// Render evidence without inferring success from a missing result or status.
fn activity_event_text_with_protection(
    event: Option<&serde_json::Value>,
    protected: bool,
) -> String {
    let Some(event) = event else {
        return "Activity unavailable: the stored event was malformed or exceeded the display limit.".into();
    };
    let kind = event
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if matches!(kind, "stream_gap" | "durable_events_compacted") {
        return "Recorded activity has a gap; some execution details are unavailable.".into();
    }
    let (name, target, private) = activity_identity(event);
    let status = event
        .get("status")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(
            || match event.get("success").and_then(serde_json::Value::as_bool) {
                Some(true) => "succeeded",
                Some(false) => "failed",
                None if matches!(kind, "tool_call" | "tool_call_start") => "requested",
                None => "outcome unknown",
            },
        );
    let execution = match event.get("executed") {
        Some(serde_json::Value::Bool(false))
            if event.get("disposition").and_then(serde_json::Value::as_str) == Some("reused") =>
        {
            " · reported reused; not executed"
        }
        Some(serde_json::Value::Bool(false)) => " · reported not executed",
        Some(serde_json::Value::Bool(true)) => " · reported executed",
        Some(serde_json::Value::Null) => " · execution unknown",
        _ => "",
    };
    if !protected && is_delegation_name(&target) {
        let args = event
            .get("arguments")
            .or_else(|| event.get("description"))
            .map(|value| {
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| value.to_string())
            })
            .unwrap_or_default();
        let receipt = if matches!(kind, "tool_call" | "tool_call_start") {
            crate::tui::agent_control_status::compact_delegation_description(&name, &args)
        } else {
            let payload = event
                .get("output")
                .filter(|v| !v.is_null())
                .or_else(|| event.get("result"))
                .or_else(|| event.get("output_summary"));
            let payload = payload.map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| v.to_string())
            });
            crate::tui::agent_control_status::compact_delegation_result(
                Some(
                    if status == "outcome unknown"
                        || event.get("executed").is_some_and(|value| value.is_null())
                    {
                        "uncertain"
                    } else {
                        status.strip_prefix("reported ").unwrap_or(status)
                    },
                ),
                payload.as_deref(),
            )
        };
        return format!("{name} · {status}{execution}\n{receipt}");
    }
    if protected || private {
        return format!(
            "{name} · {status}{execution}\nDelegated or unresolved tool details are available in the corresponding agent view."
        );
    }
    let detail = [
        event.get("output"),
        event.get("result"),
        event.get("output_summary"),
        event.get("error"),
        event.get("arguments"),
        event.pointer("/tool_call/function/arguments"),
        event.get("description"),
    ]
    .into_iter()
    .flatten()
    .find_map(|value| match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) if text.trim().is_empty() => None,
        serde_json::Value::String(text) => Some(text.clone()),
        value => Some(value.to_string()),
    })
    .unwrap_or_default();
    let summary = event
        .get("output_summary")
        .and_then(serde_json::Value::as_str)
        .filter(|summary| !summary.trim().is_empty() && *summary != detail);
    let detail = match summary {
        Some(summary) => format!("{summary}\n{detail}"),
        None => detail,
    };
    format!("{name} · {status}{execution}\n{detail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn delegation_history(
        count: usize,
    ) -> (
        Vec<astra_thin_client::SessionTranscriptItem>,
        LiveTranscript,
    ) {
        let mut items = Vec::new();
        let mut live = LiveTranscript::default();
        for n in 0..count {
            let id = format!("spawn-{n}");
            let mut call = page().items.remove(0);
            call.item_seq = (n * 2) as i64;
            call.reasoning = None;
            call.reasoning_status = None;
            call.content.clear();
            call.tool_calls = vec![astra_thin_client::SessionTranscriptToolCall {
                tool_use_id: id.clone(),
                name: "invoke_tool".into(),
                arguments: r#"{"name":"agent","arguments":{"action":"spawn"}}"#.into(),
            }];
            let payload = r#"{"status":"spawn_rejected","error":"selected model unavailable"}"#;
            let mut result = call.clone();
            result.item_seq += 1;
            result.role = "tool".into();
            result.content = payload.into();
            result.tool_calls.clear();
            result.tool_result = Some(astra_thin_client::SessionTranscriptToolResult {
                tool_use_id: id.clone(),
                name: None,
                status: Some("failed".into()),
                duration_ms: Some(1),
                runtime_advisories: vec![],
            });
            live.tool_started(id.clone(), "agent".into(), r#"{"action":"spawn"}"#.into());
            live.tool_completed(
                id,
                "agent".into(),
                String::new(),
                "failed".into(),
                1,
                None,
                Some(payload.into()),
                None,
            );
            items.extend([call, result]);
        }
        (items, live)
    }

    #[test]
    fn recorded_and_observed_requests_expand_without_inventing_running() {
        let argument = format!("{}\nTAIL-MARKER", "中".repeat(180));
        let (mut items, _) = delegation_history(1);
        items[0].tool_calls[0].name = "read_file".into();
        items[0].tool_calls[0].arguments = argument.clone();
        items[1].tool_result.as_mut().unwrap().name = Some("read_file".into());
        items[1].tool_result.as_mut().unwrap().status = Some("completed".into());
        items[1].content = "read result".into();

        let mut recorded = loading_view(80, 24);
        let mut call_only = page();
        call_only.items = vec![items[0].clone()];
        recorded.apply_page(
            call_only.clone(),
            true,
            AgentTranscriptSource::DurableServer,
        );
        recorded.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let collapsed = rendered(&recorded);
        assert!(collapsed.contains("result not loaded"), "{collapsed}");
        assert!(collapsed.contains("read_file"), "{collapsed}");
        assert!(!collapsed.contains("Running"), "{collapsed}");
        assert!(!collapsed.contains("TAIL-MARKER"), "{collapsed}");
        recorded.fit_conversation_workspace(20, 40);
        let area = Rect::new(0, 0, 40, 20);
        let mut buffer = Buffer::empty(area);
        recorded.render(area, &mut buffer);
        let narrow = crate::tui::testing::render::buffer_to_string(&buffer);
        assert!(
            narrow.contains("read_file") && narrow.contains("result not loaded"),
            "{narrow}"
        );
        recorded.fit_conversation_workspace(24, 80);
        recorded.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(rendered(&recorded).contains("TAIL-MARKER"));
        recorded.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(!rendered(&recorded).contains("TAIL-MARKER"));
        assert!(
            recorded
                .transcript
                .export_plain_lines()
                .join("\n")
                .contains(&argument)
        );

        let mut running = loading_view(80, 24);
        running.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "read_file".into(),
                description: "live display".into(),
                tool_use_id: "spawn-0".into(),
            },
        });
        let mut long_call = call_only.clone();
        long_call.items[0].tool_calls[0].arguments = format!("{}ACTIVE-TAIL", "x".repeat(600));
        running.apply_page(long_call, true, AgentTranscriptSource::DurableServer);
        running.handle_key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        let collapsed = rendered(&running);
        assert!(collapsed.contains("read_file · Running"), "{collapsed}");
        assert!(
            collapsed.contains("expand for full activity") && !collapsed.contains("ACTIVE-TAIL"),
            "{collapsed}"
        );
        running.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("next".into()),
                text: "next".into(),
            },
        });
        running.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(rendered(&running).contains("ACTIVE-TAIL"));

        let mut observed = loading_view(80, 24);
        for kind in [
            AgentLiveEventKind::ToolStarted {
                name: "read_file".into(),
                description: argument.clone(),
                tool_use_id: "spawn-0".into(),
            },
            AgentLiveEventKind::ToolCompleted {
                name: "read_file".into(),
                description: String::new(),
                status: "completed".into(),
                duration_ms: 1,
                output_summary: None,
                output: Some("read result".into()),
                tool_use_id: "spawn-0".into(),
            },
        ] {
            observed.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind,
            });
        }
        let mut result_only = page();
        result_only.items = vec![items[1].clone()];
        observed.apply_page(result_only, true, AgentTranscriptSource::DurableServer);
        observed.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(!rendered(&observed).contains("TAIL-MARKER"));
        observed.handle_key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert!(rendered(&observed).contains("TAIL-MARKER"));
        assert!(
            observed
                .transcript
                .export_plain_lines()
                .join("\n")
                .contains(&argument)
        );
        assert!(!rendered(&observed).contains("Running"));
    }

    #[test]
    fn request_heading_names_deferred_target_without_child_payload() {
        for (arguments, expected, hidden) in [
            (
                r#"{"name":"read_file","arguments":{"path":"notes.txt"}}"#,
                "read_file",
                None,
            ),
            (
                r#"{"name":"agent","arguments":{"action":"spawn","prompt":"private child prompt"}}"#,
                "agent",
                Some("private child prompt"),
            ),
        ] {
            let (mut items, _) = delegation_history(1);
            items.truncate(1);
            items[0].tool_calls[0].arguments = arguments.into();
            let mut view = loading_view(80, 24);
            let mut canonical = page();
            canonical.items = items;
            view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
            let text = view.transcript.export_plain_lines().join("\n");
            assert!(text.contains(expected), "{text}");
            if let Some(hidden) = hidden {
                assert!(!text.contains(hidden), "{text}");
            }
        }
    }

    #[test]
    fn covered_request_cache_is_bounded_without_dropping_live_evidence() {
        let (items, live) = delegation_history(300);
        let mut view = loading_view(80, 24);
        view.live = live;
        view.live
            .append_output("active reply", Some("active-output"));
        view.live.notice("active notice".into(), None);
        let later_ids: Vec<_> = view
            .live
            .items
            .iter()
            .rev()
            .take(2)
            .map(|entry| entry.id)
            .collect();
        let mut canonical = page();
        canonical.items = items;
        view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
        let cached: Vec<_> = view
            .live
            .items
            .iter()
            .filter_map(|entry| match &entry.item {
                LiveTranscriptItem::Tool {
                    cell: None,
                    request,
                    identity: ToolIdentity::Known(target),
                    tool_use_id,
                    ..
                } => Some(
                    request.description.len()
                        + request.name.len()
                        + target.len()
                        + tool_use_id.len(),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(cached.len(), 128);
        assert!(cached.iter().sum::<usize>() <= 512 * 1024);
        assert_eq!(
            view.live
                .items
                .iter()
                .rev()
                .take(2)
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            later_ids
        );
        let (request_id, request) = view
            .live
            .items
            .iter()
            .find_map(|entry| match &entry.item {
                LiveTranscriptItem::Tool {
                    cell: None,
                    request,
                    ..
                } => Some((entry.id, request.clone())),
                _ => None,
            })
            .unwrap();
        view.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("active-output".into()),
                text: " continues".into(),
            },
        });
        assert!(view.live.items.iter().any(|entry| {
            entry.id == request_id
                && matches!(&entry.item, LiveTranscriptItem::Tool { request: current, .. } if Arc::ptr_eq(current, &request))
        }));
        let output = view.transcript.export_plain_lines().join("\n");
        assert!(
            output.contains("active reply") && output.contains("active notice"),
            "{output}"
        );
    }

    #[test]
    fn oversized_covered_request_is_evicted_without_false_running() {
        let argument = "a".repeat(512 * 1024 + 1);
        let mut view = loading_view(80, 24);
        view.live
            .tool_started("large".into(), "read_file".into(), argument.clone());
        view.live.tool_completed(
            "large".into(),
            "read_file".into(),
            String::new(),
            "completed".into(),
            1,
            None,
            Some("done".into()),
            None,
        );
        let (mut items, _) = delegation_history(1);
        items[0].tool_calls[0].tool_use_id = "large".into();
        items[0].tool_calls[0].name = "read_file".into();
        items[0].tool_calls[0].arguments = argument;
        items[1].tool_result.as_mut().unwrap().tool_use_id = "large".into();
        items[1].tool_result.as_mut().unwrap().name = Some("read_file".into());
        items[1].tool_result.as_mut().unwrap().status = Some("completed".into());
        items[1].content = "done".into();
        let mut full = page();
        full.items = items.clone();
        view.apply_page(full, true, AgentTranscriptSource::DurableServer);
        assert!(view.live.items.is_empty());
        let mut call_only = page();
        call_only.items = vec![items[0].clone()];
        view.apply_page(call_only, true, AgentTranscriptSource::DurableServer);
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(text.contains("result not loaded") && !text.contains("Running"));
        let mut result_only = page();
        result_only.items = vec![items[1].clone()];
        view.apply_page(result_only, true, AgentTranscriptSource::DurableServer);
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(text.contains("request not in loaded page") && !text.contains("Running"));
    }

    #[test]
    fn fallback_budget_prefers_visible_evidence_and_preserves_uncovered_items() {
        let mut live = LiveTranscript::default();
        for index in 0..10 {
            live.push(LiveTranscriptItem::Tool {
                tool_use_id: format!("call-{index}"),
                cell: None,
                request: Arc::new(ToolCell::new_running("read_file", "x".repeat(100 * 1024))),
                identity: ToolIdentity::Known("read_file".into()),
                receipt: None,
            });
        }
        live.tool_started("active".into(), "read_file".into(), "active request".into());
        let mut canonical = CanonicalProjection::default();
        canonical.tools.insert(
            (Some("run-child".into()), "call-0".into()),
            ToolCoverage {
                identity: ToolIdentity::Known("read_file".into()),
                compatible_result: true,
                request_present: false,
                result_index: Some(0),
                projection_index: 0,
                receipt: None,
            },
        );
        live.prune_covered_requests(&canonical, "run-child");
        let ids: Vec<_> = live
            .items
            .iter()
            .filter_map(|entry| match &entry.item {
                LiveTranscriptItem::Tool { tool_use_id, .. } => Some(tool_use_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 6, "{ids:?}");
        assert!(ids.contains(&"call-0") && ids.contains(&"call-9") && ids.contains(&"active"));
        assert!(!ids.contains(&"call-1"));
    }

    #[test]
    fn call_only_page_keeps_canonical_arguments_with_live_result() {
        for (started, completed) in [(true, false), (true, true), (false, true)] {
            let mut view = loading_view(80, 24);
            if started {
                view.refresh_agent_live_event(&AgentLiveEvent {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    kind: AgentLiveEventKind::ToolStarted {
                        name: "read_file".into(),
                        description: "brief live display".into(),
                        tool_use_id: "spawn-0".into(),
                    },
                });
            }
            if completed {
                view.refresh_agent_live_event(&AgentLiveEvent {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    kind: AgentLiveEventKind::ToolCompleted {
                        name: "read_file".into(),
                        description: "brief live display".into(),
                        status: "completed".into(),
                        duration_ms: 1,
                        output_summary: None,
                        output: Some("unique live body".into()),
                        tool_use_id: "spawn-0".into(),
                    },
                });
            }
            if !started {
                let (mut result_items, _) = delegation_history(1);
                result_items.remove(0);
                result_items[0].tool_result.as_mut().unwrap().name = Some("read_file".into());
                result_items[0].tool_result.as_mut().unwrap().status = Some("completed".into());
                result_items[0].content = "unique live body".into();
                let mut latest = page();
                latest.items = result_items;
                view.apply_page(latest, true, AgentTranscriptSource::DurableServer);
            }
            let (mut items, _) = delegation_history(1);
            items.truncate(1);
            items[0].tool_calls[0].name = "read_file".into();
            items[0].tool_calls[0].arguments = r#"{"path":"canonical-path.txt"}"#.into();
            let mut canonical = page();
            canonical.items = items;
            view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
            let text = view.transcript.export_plain_lines().join("\n");
            assert!(text.contains("canonical-path.txt"), "{text}");
            assert_eq!(text.contains("Running"), !completed, "{text}");
            assert_eq!(
                text.matches("unique live body").count(),
                usize::from(completed && started),
                "{text}"
            );
        }
    }

    #[test]
    fn late_start_supplies_request_without_reopening_completed_tool() {
        for completion_name in ["read_file", "invoke_tool"] {
            let mut view = loading_view(80, 24);
            for kind in [
                AgentLiveEventKind::ToolCompleted {
                    name: completion_name.into(),
                    description: "opaque completion display".into(),
                    status: "completed".into(),
                    duration_ms: 1,
                    output_summary: None,
                    output: Some("body".into()),
                    tool_use_id: "read".into(),
                },
                AgentLiveEventKind::ToolStarted {
                    name: "read_file".into(),
                    description: "authoritative-request.txt".into(),
                    tool_use_id: "read".into(),
                },
            ] {
                view.refresh_agent_live_event(&AgentLiveEvent {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    kind,
                });
            }
            let text = view.transcript.export_plain_lines().join("\n");
            assert!(
                text.contains("authoritative-request.txt")
                    && text.contains("body")
                    && !text.contains("Running"),
                "{text}"
            );
        }
    }

    #[test]
    fn uncovered_result_is_shared_across_snapshot_refreshes() {
        let mut view = loading_view(80, 24);
        for kind in [
            AgentLiveEventKind::ToolStarted {
                name: "read_file".into(),
                description: "notes.txt".into(),
                tool_use_id: "read".into(),
            },
            AgentLiveEventKind::ToolCompleted {
                name: "read_file".into(),
                description: "notes.txt".into(),
                status: "completed".into(),
                duration_ms: 1,
                output_summary: None,
                output: Some("uncovered full result".repeat(128)),
                tool_use_id: "read".into(),
            },
        ] {
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind,
            });
        }
        let original = match &view.live.items[0].item {
            LiveTranscriptItem::Tool {
                cell: Some(cell), ..
            } => cell.clone(),
            _ => panic!("live result missing"),
        };
        view.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("answer".into()),
                text: "Next step".into(),
            },
        });
        let mut snapshot = Vec::new();
        view.append_live_projection(&mut snapshot);
        let projected = snapshot.iter().find_map(TranscriptItem::tool_cell).unwrap();
        assert!(
            std::ptr::eq(projected, original.as_ref()),
            "snapshot deep-copied the result"
        );
    }

    #[test]
    fn full_pair_deduplicates_even_after_result_only_page() {
        for (result_only_first, diagnostic) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let status = if diagnostic { "failed" } else { "completed" };
            let (mut items, _) = delegation_history(1);
            items[0].tool_calls[0].name = "read_file".into();
            items[0].tool_calls[0].arguments = r#"{"path":"notes.txt"}"#.into();
            items[1].tool_result.as_mut().unwrap().name = Some("read_file".into());
            items[1].tool_result.as_mut().unwrap().status = Some(status.into());
            items[1].content = "FULL_UNIQUE_BODY".into();
            let mut view = loading_view(80, 24);
            for kind in [
                AgentLiveEventKind::ToolStarted {
                    name: "read_file".into(),
                    description: r#"{"path":"notes.txt"}"#.into(),
                    tool_use_id: "spawn-0".into(),
                },
                AgentLiveEventKind::ToolCompleted {
                    name: "read_file".into(),
                    description: r#"{"path":"notes.txt"}"#.into(),
                    status: status.into(),
                    duration_ms: 1,
                    output_summary: diagnostic.then(|| r#"{"advisory":{"executed":false}}"#.into()),
                    output: Some("FULL_UNIQUE_BODY".into()),
                    tool_use_id: "spawn-0".into(),
                },
            ] {
                view.refresh_agent_live_event(&AgentLiveEvent {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    kind,
                });
            }
            if result_only_first {
                let mut latest = page();
                latest.items = vec![items[1].clone()];
                view.apply_page(latest, true, AgentTranscriptSource::DurableServer);
                let mut older = page();
                older.items = vec![items[0].clone()];
                view.apply_page(older, false, AgentTranscriptSource::DurableServer);
            } else {
                let mut canonical = page();
                canonical.items = items;
                view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
            }
            let text = view.transcript.export_plain_lines().join("\n");
            assert_eq!(
                text.matches("FULL_UNIQUE_BODY").count(),
                1,
                "result_only_first={result_only_first}: {text}"
            );
            if diagnostic {
                assert!(text.contains("Did not run"), "{text}");
            }
            assert!(view.live.items.iter().any(|entry| matches!(&entry.item,
                LiveTranscriptItem::Tool { cell, request, .. } if request.description.contains("notes.txt") && cell.as_ref().is_none_or(|cell| cell.output.is_none()))),
                "covered result body must be released");
            if !diagnostic {
                let result = view
                    .items
                    .iter()
                    .find(|item| item.tool_result.is_some())
                    .unwrap()
                    .clone();
                let mut empty = page();
                empty.items.clear();
                view.apply_page(empty, true, AgentTranscriptSource::DurableServer);
                let unloaded = view.transcript.export_plain_lines().join("\n");
                assert!(
                    !unloaded.contains("FULL_UNIQUE_BODY") && !unloaded.contains("notes.txt"),
                    "{unloaded}"
                );
                let mut latest = page();
                latest.items = vec![result.clone()];
                view.apply_page(latest, true, AgentTranscriptSource::DurableServer);
                let restored = view.transcript.export_plain_lines().join("\n");
                assert!(
                    restored.contains("notes.txt")
                        && restored.matches("FULL_UNIQUE_BODY").count() == 1,
                    "{restored}"
                );
                let mut conflicting = result.clone();
                conflicting.tool_result.as_mut().unwrap().name = Some("agent".into());
                conflicting.content = r#"{"status":"completed"}"#.into();
                let (mut conflict_items, _) = delegation_history(1);
                conflict_items.truncate(1);
                conflict_items[0].tool_calls[0].name = "read_file".into();
                conflict_items[0].tool_calls[0].arguments =
                    r#"{"path":"different-canonical-input"}"#.into();
                conflict_items.push(conflicting);
                let mut changed = page();
                changed.items = conflict_items;
                view.apply_page(changed, true, AgentTranscriptSource::DurableServer);
                assert!(
                    !view
                        .transcript
                        .export_plain_lines()
                        .join("\n")
                        .contains("notes.txt")
                );
                let mut restored_page = page();
                restored_page.items = vec![result];
                view.apply_page(restored_page, true, AgentTranscriptSource::DurableServer);
                assert!(
                    !view
                        .transcript
                        .export_plain_lines()
                        .join("\n")
                        .contains("notes.txt"),
                    "conflict cannot be silently forgotten"
                );
                assert!(view.live.items.iter().any(|entry| matches!(
                    &entry.item,
                    LiveTranscriptItem::Tool {
                        identity: ToolIdentity::Conflicting,
                        cell: None,
                        request,
                        ..
                    } if request.description.is_empty()
                )));
            }
        }
    }

    #[test]
    fn returned_spawn_control_is_not_a_running_tool() {
        for status in ["completed", "launched"] {
            let (mut items, _) = delegation_history(1);
            items[1].tool_result.as_mut().unwrap().status = Some(status.into());
            items[1].content = serde_json::json!({"result_family":"control_receipt","success":true,"action":"spawn","status":status,"parent_run_id":"run-child","agent_id":"launched-agent","run_id":"launched-child"}).to_string();
            let mut view = loading_view(80, 24);
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "agent".into(),
                    description: r#"{"action":"spawn"}"#.into(),
                    tool_use_id: "spawn-0".into(),
                },
            });
            let mut canonical = page();
            canonical.items = items;
            view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
            let text = view.transcript.export_plain_lines().join("\n");
            assert!(!text.contains("Running Agent"), "status={status}: {text}");
        }
    }

    #[test]
    fn unpaired_partial_result_preserves_known_invocation_arguments() {
        let mut view = loading_view(100, 24);
        view.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "read_file".into(),
                description: r#"{"path":"IMPORTANT_REQUEST_PATH"}"#.into(),
                tool_use_id: "spawn-0".into(),
            },
        });
        assert!(
            view.transcript
                .export_plain_lines()
                .join("\n")
                .contains("IMPORTANT_REQUEST_PATH")
        );
        let (mut items, _) = delegation_history(1);
        items.remove(0);
        items[0].role = "tool_observation_preview".into();
        items[0].content = "partial retained line".into();
        items[0].tool_result.as_mut().unwrap().name = Some("read_file".into());
        let mut canonical = page();
        canonical.items = items;
        view.apply_page(canonical, true, AgentTranscriptSource::LocalJournalOnly);
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(text.contains("IMPORTANT_REQUEST_PATH"), "{text}");
        assert!(!text.contains("Running"), "{text}");
        let latest = view.items.clone();
        let mut older = page();
        let mut call = older.items.remove(0);
        call.content.clear();
        call.reasoning = None;
        call.item_seq = -1;
        call.tool_calls = vec![astra_thin_client::SessionTranscriptToolCall {
            tool_use_id: "spawn-0".into(),
            name: "read_file".into(),
            arguments: r#"{"path":"IMPORTANT_REQUEST_PATH"}"#.into(),
        }];
        older.items = vec![call];
        view.apply_page(older, false, AgentTranscriptSource::LocalJournalOnly);
        let mut replacement = page();
        replacement.items = latest;
        view.apply_page(replacement, true, AgentTranscriptSource::LocalJournalOnly);
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(
            text.contains("IMPORTANT_REQUEST_PATH") && !text.contains("Running"),
            "{text}"
        );
    }

    #[test]
    fn named_result_on_latest_page_stops_running_indicator() {
        for (target, status) in [
            ("read_file", "failed"),
            ("agent", "failed"),
            ("agent_fanout", "failed"),
            ("read_file", "success"),
            ("agent", "completed"),
            ("read_file", "running"),
            ("read_file", "uncertain"),
            ("agent", "still_running"),
        ] {
            let (mut items, _) = delegation_history(1);
            items.remove(0);
            items[0].tool_result.as_mut().unwrap().name = Some(target.into());
            items[0].tool_result.as_mut().unwrap().status = Some(status.into());
            items[0].content = serde_json::json!({"status":status}).to_string();
            let mut view = loading_view(80, 24);
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: target.into(),
                    description: "{}".into(),
                    tool_use_id: "spawn-0".into(),
                },
            });
            let mut canonical = page();
            canonical.items = items;
            view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
            let text = view.transcript.export_plain_lines().join("\n");
            assert!(!text.contains("Running"), "target={target}: {text}");
        }
    }

    #[test]
    fn partial_output_does_not_resurrect_a_terminal_tool() {
        let (mut items, _) = delegation_history(1);
        items[1].role = "tool_observation_preview".into();
        let mut view = loading_view(80, 24);
        view.refresh_agent_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "agent".into(),
                description: r#"{"action":"spawn"}"#.into(),
                tool_use_id: "spawn-0".into(),
            },
        });
        let mut canonical = page();
        canonical.items = items;
        view.apply_page(canonical, true, AgentTranscriptSource::LocalJournalOnly);
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(!text.contains("Running Agent"), "{text}");
    }

    #[test]
    fn ordinary_carrier_completion_description_is_not_tool_identity() {
        let mut view = loading_view(80, 24);
        for kind in [
            AgentLiveEventKind::ToolStarted {
                name: "invoke_tool".into(),
                description: r#"{"name":"read_file","arguments":{"path":"manifest.json"}}"#.into(),
                tool_use_id: "read-manifest".into(),
            },
            AgentLiveEventKind::ToolCompleted {
                name: "invoke_tool".into(),
                description: r#"{"name":"agent"}"#.into(),
                status: "completed".into(),
                duration_ms: 1,
                output_summary: None,
                output: Some("ORDINARY_FILE_CONTENT".into()),
                tool_use_id: "read-manifest".into(),
            },
        ] {
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind,
            });
        }
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(text.contains("ORDINARY_FILE_CONTENT"), "{text}");
    }

    #[test]
    fn late_carrier_completion_uses_canonical_identity_and_keeps_stronger_evidence() {
        for (canonical_target, completion_name) in [
            ("agent", "invoke_tool"),
            ("read_file", "agent"),
            ("agent", "read_file"),
        ] {
            for (status, payload, expected) in [
                (
                    "failed",
                    r#"{"status":"failed","error":"late failure detail","output":"PRIVATE_CHILD_OUTPUT"}"#,
                    "late failure detail",
                ),
                (
                    "completed",
                    r#"{"status":"completed"}"#,
                    "Agent control: completed",
                ),
            ] {
                let (mut items, _) = delegation_history(1);
                if canonical_target == "read_file" {
                    items[0].tool_calls[0].name = "read_file".into();
                    items[0].tool_calls[0].arguments = "{}".into();
                }
                items[1].content = r#"{"status":"failed"}"#.into();
                let mut canonical = page();
                canonical.items = items;
                let mut view = loading_view(80, 24);
                view.apply_page(canonical, true, AgentTranscriptSource::LocalJournalOnly);
                view.refresh_agent_live_event(&AgentLiveEvent {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    kind: AgentLiveEventKind::ToolCompleted {
                        name: completion_name.into(),
                        description: r#"{"name":"agent","arguments":{"action":"spawn"}}"#.into(),
                        status: status.into(),
                        duration_ms: 1,
                        output: Some(payload.into()),
                        output_summary: None,
                        tool_use_id: "spawn-0".into(),
                    },
                });
                let text = view.transcript.export_plain_lines().join("\n");
                assert!(!text.contains("PRIVATE_CHILD_OUTPUT"), "{text}");
                assert!(
                    text.contains(expected)
                        && text.contains("Live activity · not in loaded history"),
                    "{text}"
                );
            }
        }
    }

    #[test]
    fn complete_history_retires_start_only_delegation_in_both_orders() {
        for (history_first, has_more) in [(false, false), (false, true), (true, true)] {
            let (items, _) = delegation_history(1);
            let mut canonical = page();
            canonical.items = items;
            canonical.has_more = has_more;
            let mut view = loading_view(80, 24);
            let load = |view: &mut AgentTranscriptView| {
                view.refresh_agent_transcript(AgentTranscriptUpdate::Loaded {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    page: canonical.clone(),
                    replace: true,
                    source: AgentTranscriptSource::LocalJournalOnly,
                });
            };
            if history_first {
                load(&mut view);
            }
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "agent".into(),
                    description: r#"{"action":"spawn"}"#.into(),
                    tool_use_id: "spawn-0".into(),
                },
            });
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::AgentTerminated {
                    termination: astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                    duration_ms: 1,
                    reason: None,
                },
            });
            load(&mut view);
            load(&mut view);
            let text = view.transcript.export_plain_lines().join("\n");
            assert_eq!(
                text.matches("selected model unavailable").count(),
                1,
                "{text}"
            );
            assert!(
                !text.contains("Running Agent")
                    && !text.contains("Live activity · not in loaded history"),
                "{text}"
            );
        }
    }

    #[test]
    fn projection_coverage_is_optional_without_changing_display() {
        let (items, _) = delegation_history(2);
        let render_only = project_durable_transcript(&items, false);
        let indexed = project_durable_transcript(&items, true);
        assert!(
            render_only.tools.is_empty()
                && render_only.outputs.is_empty()
                && render_only.evidence.is_empty()
        );
        let export = |items| {
            TranscriptView::from_snapshot(TranscriptSnapshot::new(items), 24, 80)
                .export_plain_lines()
        };
        assert_eq!(export(render_only.items), export(indexed.items));
    }

    #[test]
    fn incomplete_history_preserves_uncovered_or_conflicting_evidence() {
        for partial in ["result_only", "wrong_run", "wrong_target"] {
            let (mut items, _) = delegation_history(1);
            match partial {
                "result_only" => {
                    items.remove(0);
                }
                "wrong_run" => {
                    for item in &mut items {
                        item.run_id = Some("other-run".into());
                    }
                }
                "wrong_target" => {
                    items[0].tool_calls[0].arguments =
                        r#"{"name":"read_file","arguments":{}}"#.into()
                }
                _ => unreachable!(),
            }
            let mut view = loading_view(80, 24);
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "agent".into(),
                    description: r#"{"action":"spawn"}"#.into(),
                    tool_use_id: "spawn-0".into(),
                },
            });
            let mut canonical = page();
            canonical.items = items;
            view.apply_page(canonical, true, AgentTranscriptSource::LocalJournalOnly);
            let text = view.transcript.export_plain_lines().join("\n");
            if partial == "wrong_target" {
                assert!(
                    text.contains("Conflicting tool identity") && !text.contains("Running Agent"),
                    "{text}"
                );
            } else {
                assert!(text.contains("Running Agent"), "{partial}: {text}");
            }
        }
    }

    #[test]
    fn canonical_completion_preserves_distinct_live_status_evidence() {
        for (status, payload) in [
            ("uncertain", r#"{"status":"completed"}"#),
            (
                "failed",
                r#"{"status":"failed","error":"same child failure"}"#,
            ),
        ] {
            let (mut items, _) = delegation_history(1);
            items[1].content = payload.into();
            items[1].tool_result.as_mut().unwrap().status = Some("completed".into());
            let mut view = loading_view(80, 24);
            view.apply_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::ToolCompleted {
                    name: "agent".into(),
                    description: "{}".into(),
                    status: status.into(),
                    duration_ms: 0,
                    output: Some(payload.into()),
                    output_summary: None,
                    tool_use_id: "spawn-0".into(),
                },
            });
            let mut canonical = page();
            canonical.items = items;
            view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
            let text = view.transcript.export_plain_lines().join("\n");
            assert!(
                text.contains("Live activity · not in loaded history"),
                "{text}"
            );
            let expected = if status == "uncertain" {
                "Could not confirm Agent"
            } else {
                "Failed Agent"
            };
            assert!(
                text.contains(expected) && !text.contains("Running Agent"),
                "{text}"
            );
        }
    }

    #[test]
    fn empty_model_ids_do_not_retire_unrelated_live_output() {
        let mut view = loading_view(80, 24);
        view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some(String::new()),
                text: "uncorrelated live output".into(),
            },
        });
        let mut canonical = page();
        canonical.items[0].model_item_id = Some(String::new());
        view.apply_page(canonical, true, AgentTranscriptSource::DurableServer);
        assert!(
            view.transcript
                .export_plain_lines()
                .join("\n")
                .contains("uncorrelated live output")
        );
    }

    #[test]
    fn full_canonical_pair_retires_live_receipt() {
        let mut view = loading_view(80, 24);
        let (items, live) = delegation_history(1);
        view.live = live;
        let mut canonical = page();
        canonical.items = items;
        canonical.has_more = false;
        canonical.next_before_seq = None;
        view.apply_page(
            canonical.clone(),
            true,
            AgentTranscriptSource::LocalJournalOnly,
        );
        view.apply_page(canonical, true, AgentTranscriptSource::LocalJournalOnly);
        let exported = view.transcript.export_plain_lines().join("\n");
        assert_eq!(
            exported.matches("selected model unavailable").count(),
            1,
            "{exported}"
        );
        assert!(
            !exported.contains("Live activity · not in loaded history"),
            "{exported}"
        );
    }

    #[test]
    fn recorded_conflicting_starts_protect_both_orders() {
        for names in [["read_file", "agent"], ["agent", "read_file"]] {
            let mut view = loading_view(80, 24);
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind: AgentLiveEventKind::ToolStarted {
                    name: "read_file".into(),
                    description: "PRIVATE_CONFLICT_REQUEST".into(),
                    tool_use_id: "conflict".into(),
                },
            });
            let (mut result_items, _) = delegation_history(1);
            result_items.remove(0);
            let result = result_items[0].tool_result.as_mut().unwrap();
            result.tool_use_id = "conflict".into();
            result.name = Some("read_file".into());
            let mut latest = page();
            latest.items = result_items;
            view.apply_page(latest, true, AgentTranscriptSource::DurableServer);
            let events = names.iter().enumerate().map(|(index, name)| astra_thin_client::protocol::RunActivityEvent { event_idx: index as i64, event: Some(serde_json::json!({"type":"tool_call_start", "call_id":"conflict", "tool":name, "description":"PRIVATE_CONFLICT"})) }).collect();
            view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                replace: true,
                page: Ok(astra_thin_client::protocol::RunActivityPage {
                    session_id: "session-1".into(),
                    run_id: "run-child".into(),
                    source_run_id: "root".into(),
                    events,
                    next_before_event_idx: None,
                    has_more: false,
                }),
            });
            assert!(
                !view
                    .transcript
                    .export_plain_lines()
                    .join("\n")
                    .contains("PRIVATE_CONFLICT")
            );
        }
    }

    #[test]
    fn failed_delegation_keeps_failure_receipt() {
        let mut view = loading_view(80, 24);
        for kind in [
            AgentLiveEventKind::ToolStarted {
                name: "agent".into(),
                description: r#"{"action":"spawn"}"#.into(),
                tool_use_id: "spawn".into(),
            },
            AgentLiveEventKind::ToolCompleted {
                name: "agent".into(),
                description: r#"{"action":"spawn"}"#.into(),
                status: "failed".into(),
                duration_ms: 1,
                output_summary: Some("Agent spawn rejected: selected model unavailable".into()),
                output: Some(
                    r#"{"status":"spawn_rejected","error":"selected model unavailable"}"#.into(),
                ),
                tool_use_id: "spawn".into(),
            },
        ] {
            view.refresh_agent_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind,
            });
        }
        let exported = view.transcript.export_plain_lines().join("\n");
        assert!(
            exported.contains("selected model unavailable"),
            "{exported}"
        );
        let mut partial = page();
        let mut result = partial.items[0].clone();
        result.role = "tool".into();
        result.run_id = Some("run-child".into());
        result.content = "{}".into();
        result.tool_result = Some(
            serde_json::from_value(
                serde_json::json!({"tool_use_id":"spawn", "name":"invoke_tool", "status":"failed"}),
            )
            .unwrap(),
        );
        partial.items = vec![result];
        view.apply_page(partial, true, AgentTranscriptSource::DurableServer);
        assert!(
            view.transcript
                .export_plain_lines()
                .join("\n")
                .contains("selected model unavailable")
        );
    }

    #[test]
    fn resolved_deferred_live_completion_keeps_output() {
        for completion_name in ["read_file", "invoke_tool"] {
            let mut view = loading_view(80, 24);
            for kind in [
                AgentLiveEventKind::ToolStarted {
                    name: "invoke_tool".into(),
                    description: r#"{"name":"read_file","arguments":{"path":"notes.txt"}}"#.into(),
                    tool_use_id: "normal-read".into(),
                },
                AgentLiveEventKind::ToolCompleted {
                    name: completion_name.into(),
                    description: r#"{"name":"notes.txt"}"#.into(),
                    status: "completed".into(),
                    duration_ms: 1,
                    output_summary: Some("ordinary read completed".into()),
                    output: Some("UNIQUE_NORMAL_FILE_CONTENT".into()),
                    tool_use_id: "normal-read".into(),
                },
            ] {
                view.refresh_agent_live_event(&AgentLiveEvent {
                    agent_id: "agent-1".into(),
                    run_id: "run-child".into(),
                    kind,
                });
            }
            let exported = view.transcript.export_plain_lines().join("\n");
            assert!(
                exported.contains("UNIQUE_NORMAL_FILE_CONTENT"),
                "{exported}"
            );
        }
    }

    #[test]
    fn recorded_activity_uses_observed_details_without_showing_delegation_payloads() {
        let start = serde_json::json!({"type":"tool_call_start", "tool":"read_file", "description":"notes.txt"});
        assert!(activity_event_text_with_protection(Some(&start), false).contains("notes.txt"));
        let end = serde_json::json!({"type":"tool_call_end", "tool":"read_file", "output":null, "output_summary":"loaded"});
        let text = activity_event_text_with_protection(Some(&end), false);
        assert!(text.contains("loaded") && !text.contains("null"), "{text}");
        let carrier = serde_json::json!({"type":"tool_call_end", "tool":"invoke_tool", "description":{"name":"worker", "prompt":"PRIVATE_CHILD_RESULT"}, "output":"PRIVATE_CHILD_RESULT"});
        assert!(
            !activity_event_text_with_protection(Some(&carrier), false)
                .contains("PRIVATE_CHILD_RESULT")
        );
        for tool in ["agent", "invoke_tool", "tool"] {
            let event = serde_json::json!({"type":"tool_call_end", "tool":tool, "output":"PRIVATE_CHILD_RESULT"});
            assert!(
                !activity_event_text_with_protection(Some(&event), false)
                    .contains("PRIVATE_CHILD_RESULT")
            );
        }
    }

    #[test]
    fn activity_projection_distinguishes_requests_unknown_outcomes_and_gaps() {
        for (event, expected) in [
            (
                serde_json::json!({"type":"tool_call", "tool":"bash"}),
                "bash · requested",
            ),
            (
                serde_json::json!({"type":"tool_result", "tool":"bash"}),
                "bash · outcome unknown",
            ),
            (
                serde_json::json!({"type":"tool_result", "tool":"bash", "success":false}),
                "bash · failed",
            ),
            (
                serde_json::json!({"type":"tool_call_end", "tool":"bash", "status":"completed", "executed":false, "disposition":"reused"}),
                "bash · completed · reported reused; not executed",
            ),
            (
                serde_json::json!({"type":"tool_call_end", "tool":"bash", "status":"rejected", "executed":false}),
                "bash · rejected · reported not executed",
            ),
            (
                serde_json::json!({"type":"tool_call_end", "tool":"bash", "status":"failed", "executed":null}),
                "bash · failed · execution unknown",
            ),
            (
                serde_json::json!({"type":"tool_call_end", "tool":"bash", "status":"completed", "executed":true}),
                "bash · completed · reported executed",
            ),
            (
                serde_json::json!({"type":"stream_gap"}),
                "Recorded activity has a gap",
            ),
        ] {
            assert!(activity_event_text_with_protection(Some(&event), false).contains(expected));
        }
        assert!(activity_event_text_with_protection(None, false).contains("unavailable"));
        let absent =
            serde_json::json!({"type":"tool_call_end", "tool":"bash", "status":"completed"});
        assert!(!activity_event_text_with_protection(Some(&absent), false).contains("executed"));
        let unknown = serde_json::json!({"type":"tool_call_end", "tool":"agent", "executed":null, "status":"reported completed", "output":{"status":"completed"}});
        let text = activity_event_text_with_protection(Some(&unknown), false);
        assert!(
            text.contains("reported completed · execution unknown")
                && text.contains("Agent control: uncertain")
                && !text.contains("Agent control: completed"),
            "{text}"
        );
    }

    #[test]
    fn loaded_child_activity_keeps_tool_details_and_hides_delegation_content() {
        use astra_thin_client::protocol::{RunActivityEvent, RunActivityPage};
        let mut view = AgentTranscriptView::loading(
            "child".into(),
            "Child".into(),
            "session".into(),
            "run".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
            "agents",
            80,
            24,
        );
        let events = vec![
            RunActivityEvent {
                event_idx: 1,
                event: Some(
                    serde_json::json!({"type":"tool_call_start", "tool":"read_file", "description":"notes.txt"}),
                ),
            },
            RunActivityEvent {
                event_idx: 2,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "tool":"read_file", "output":null, "output_summary":"loaded"}),
                ),
            },
            RunActivityEvent {
                event_idx: 3,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "tool":"agent", "output":"PRIVATE_CHILD_RESULT"}),
                ),
            },
            RunActivityEvent {
                event_idx: 4,
                event: Some(
                    serde_json::json!({"type":"tool_call_start", "tool":"agent", "call_id":"same", "description":"PRIVATE_PROMPT"}),
                ),
            },
            RunActivityEvent {
                event_idx: 5,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "tool":"read_file", "call_id":"same", "output":"PRIVATE_CONFLICT_RESULT"}),
                ),
            },
            RunActivityEvent {
                event_idx: 6,
                event: Some(
                    serde_json::json!({"type":"tool_call_start", "tool":"read_file", "call_id":"same", "description":"PRIVATE_CONFLICT_START"}),
                ),
            },
            RunActivityEvent {
                event_idx: 7,
                event: Some(
                    serde_json::json!({"type":"tool_call_start", "tool":"read_file", "call_id":"ordinary", "description":"again"}),
                ),
            },
            RunActivityEvent {
                event_idx: 8,
                event: Some(
                    serde_json::json!({"type":"tool_call_start", "tool":"read_file", "call_id":"ordinary", "description":"again"}),
                ),
            },
            RunActivityEvent {
                event_idx: 9,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "call_id":"ordinary", "output":"body text", "output_summary":"short summary"}),
                ),
            },
            RunActivityEvent {
                event_idx: 10,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "call_id":"ordinary", "result":"second result"}),
                ),
            },
            RunActivityEvent {
                event_idx: 11,
                event: Some(
                    serde_json::json!({"type":"tool_call_start", "tool":"agent", "call_id":"gap-call"}),
                ),
            },
            RunActivityEvent {
                event_idx: 12,
                event: Some(serde_json::json!({"type":"stream_gap"})),
            },
            RunActivityEvent {
                event_idx: 13,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "tool":"read_file", "call_id":"gap-call", "output":"PRIVATE_AFTER_GAP"}),
                ),
            },
            RunActivityEvent {
                event_idx: 14,
                event: Some(
                    serde_json::json!({"type":"tool_call_end", "tool":"read_file", "call_id":"unrelated", "output":"ordinary after gap"}),
                ),
            },
        ];
        view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
            agent_id: "child".into(),
            run_id: "run".into(),
            replace: true,
            page: Ok(RunActivityPage {
                session_id: "session".into(),
                run_id: "run".into(),
                source_run_id: "root".into(),
                events,
                next_before_event_idx: None,
                has_more: false,
            }),
        });
        let text = view.transcript.export_plain_lines().join("\n");
        assert!(
            text.contains("notes.txt")
                && text.contains("loaded")
                && text.contains("short summary")
                && text.contains("body text")
                && text.contains("second result"),
            "{text}"
        );
        assert!(!text.contains("PRIVATE_AFTER_GAP"));
        assert!(text.contains("ordinary after gap"));
        assert!(
            !text.contains("PRIVATE_CHILD_RESULT")
                && !text.contains("PRIVATE_PROMPT")
                && !text.contains("PRIVATE_CONFLICT_RESULT")
                && !text.contains("PRIVATE_CONFLICT_START"),
            "{text}"
        );
    }

    #[test]
    fn recorded_activity_pages_are_bounded_deduplicated_and_identity_scoped() {
        use astra_thin_client::protocol::{RunActivityEvent, RunActivityPage};
        let mut view = AgentTranscriptView::loading(
            "child".into(),
            "Child".into(),
            "session".into(),
            "run".into(),
            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
            "agents",
            80,
            24,
        );
        let page = |session: &str, first: i64, last: i64| RunActivityPage {
            session_id: session.into(),
            run_id: "run".into(),
            source_run_id: "root".into(),
            events: (first..=last)
                .map(|event_idx| RunActivityEvent {
                    event_idx,
                    event: Some(serde_json::json!({"type":"tool_call_start","tool":"bash"})),
                })
                .collect(),
            next_before_event_idx: Some(first),
            has_more: true,
        };
        view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
            agent_id: "child".into(),
            run_id: "run".into(),
            page: Ok(page("session", 100, 163)),
            replace: true,
        });
        view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
            agent_id: "child".into(),
            run_id: "another-run".into(),
            page: Ok(page("session", 0, 63)),
            replace: true,
        });
        assert_eq!(view.activity.as_ref().unwrap().events[0].event_idx, 100);
        view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
            agent_id: "child".into(),
            run_id: "run".into(),
            page: Ok(page("other-session", 0, 63)),
            replace: true,
        });
        assert_eq!(view.activity.as_ref().unwrap().events[0].event_idx, 100);
        assert!(
            view.activity_error
                .as_deref()
                .unwrap()
                .contains("identity mismatch")
        );
        for (first, last) in [(50, 113), (0, 63)] {
            view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
                agent_id: "child".into(),
                run_id: "run".into(),
                page: Ok(page("session", first, last)),
                replace: false,
            });
        }
        let activity = view.activity.as_ref().unwrap();
        assert_eq!(activity.events.len(), 128);
        assert_eq!(activity.events.first().unwrap().event_idx, 0);
        assert_eq!(activity.events.last().unwrap().event_idx, 127);
        assert_eq!(activity.next_before_event_idx, Some(0));
        assert!(view.activity_error.is_none());
        view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
            agent_id: "child".into(),
            run_id: "run".into(),
            page: Err("connection lost".into()),
            replace: true,
        });
        assert_eq!(view.activity.as_ref().unwrap().events.len(), 128);
        assert!(
            view.activity_error
                .as_deref()
                .unwrap()
                .contains("connection lost")
        );
    }

    fn rendered(view: &AgentTranscriptView) -> String {
        let area = Rect::new(0, 0, 80, 20);
        let mut buffer = Buffer::empty(area);
        view.render(area, &mut buffer);
        crate::tui::testing::render::buffer_to_string(&buffer)
    }

    #[test]
    fn root_terminal_racing_initial_page_preserves_live_output_and_refreshes_once() {
        for initial_read_fails in [false, true] {
            let mut view = AgentTranscriptView::loading(
                "root".into(),
                "Main conversation".into(),
                "session-1".into(),
                "root".into(),
                crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                "agents",
                80,
                24,
            );
            let mut cell =
                crate::tui::history_cell::assistant::AssistantCell::from_markdown("root_live_body");
            cell.model_item_id = Some("root-model-item".into());
            let item = TranscriptItem::committed(
                TranscriptItemId::from_widget_id(1),
                std::sync::Arc::new(cell),
                0,
            );
            assert!(view.refresh_root_transcript_live(Some(("session-1", "root")), Some(item)));
            assert!(
                view.take_action_request().is_none(),
                "token projection does not fetch"
            );
            assert!(!view.refresh_root_transcript_live(None, None));
            assert!(!view.refresh_root_transcript_live(Some(("other-session", "root")), None));
            assert!(!view.refresh_root_transcript_live(Some(("session-1", "other-root")), None));
            assert!(view.refresh_root_transcript_live(Some(("session-1", "root")), None));
            assert!(view.root_refresh_pending);
            assert!(view.take_action_request().is_none());
            let update = if initial_read_fails {
                AgentTranscriptUpdate::Failed {
                    agent_id: "root".into(),
                    run_id: "root".into(),
                    message: "initial read failed".into(),
                }
            } else {
                let mut empty = page();
                empty.items.clear();
                AgentTranscriptUpdate::Loaded {
                    agent_id: "root".into(),
                    run_id: "root".into(),
                    page: empty,
                    replace: true,
                    source: AgentTranscriptSource::DurableServer,
                }
            };
            assert!(view.refresh_agent_transcript(update));
            assert!(
                matches!(view.take_action_request(), Some(ViewActionRequest { action: BottomPaneViewAction::LoadAgentTranscript { run_id, before_seq: None, .. }, .. }) if run_id == "root")
            );
            assert!(view.take_action_request().is_none());
            assert!(rendered(&view).contains("root_live_body"));
            view.refresh_agent_transcript(AgentTranscriptUpdate::Failed {
                agent_id: "root".into(),
                run_id: "root".into(),
                message: "terminal read failed".into(),
            });
            assert!(view.take_action_request().is_none());
            assert!(rendered(&view).contains("root_live_body"));
            let mut durable = page();
            durable.items[0].run_id = Some("root".into());
            durable.items[0].model_item_id = Some("root-model-item".into());
            durable.items[0].content = "root_live_body".into();
            view.refresh_agent_transcript(AgentTranscriptUpdate::Loaded {
                agent_id: "root".into(),
                run_id: "root".into(),
                page: durable,
                replace: true,
                source: AgentTranscriptSource::DurableServer,
            });
            assert!(view.local_root_live.is_none());
            assert_eq!(rendered(&view).matches("root_live_body").count(), 1);
        }
    }

    fn page() -> astra_thin_client::SessionTranscriptPage {
        astra_thin_client::SessionTranscriptPage {
            session_id: "session-1".into(),
            items: vec![astra_thin_client::SessionTranscriptItem {
                model_item_id: None,
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
                    model_item_id: None,
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
            next_before_seq: None,
            has_more: false,
        }
    }

    fn monitor_row_with_target(
        transcript_target: crate::tui::agent_run_projection::AgentTranscriptTarget,
    ) -> crate::tui::bottom_pane::in_flight_agents_view::AgentRow {
        crate::tui::bottom_pane::in_flight_agents_view::AgentRow {
            kind: crate::tui::agent_run_projection::AgentRunKind::Agent,
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
    fn durable_tool_guidance_is_rendered_without_mutating_result_evidence() {
        let mut item = page().items.remove(0);
        item.role = "tool".into();
        item.tool_calls.clear();
        item.content = r#"{"value":42}"#.into();
        item.tool_result = Some(astra_thin_client::SessionTranscriptToolResult {
            tool_use_id: "call".into(),
            name: Some("read_file".into()),
            status: Some("completed".into()),
            duration_ms: None,
            runtime_advisories: vec!["Read the remaining lines if needed.".into()],
        });
        let display = tool_display_content(&item, false);
        assert_eq!(display, tool_display_content(&item, false));
        let view = TranscriptView::from_snapshot(
            TranscriptSnapshot::new(durable_transcript_items(std::slice::from_ref(&item))),
            24,
            100,
        );
        let rendered = view.export_plain_lines().join("\n");
        assert!(
            rendered.contains("Read the remaining lines if needed."),
            "{rendered}"
        );
        assert_eq!(
            display
                .matches("Read the remaining lines if needed.")
                .count(),
            1
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&item.content).unwrap()["value"],
            42
        );
        item.content = "Literal [Runtime tool guidance] from the tool".into();
        assert!(tool_display_content(&item, false).starts_with(&item.content));
    }

    #[test]
    fn durable_tool_pairing_is_run_scoped_and_handles_local_observations() {
        let mut observed = page().items.remove(0);
        observed.item_seq = 1;
        observed.run_id = Some("root-a".into());
        observed.role = "tool_observation_preview".into();
        observed.content = "output-a".into();
        observed.reasoning = None;
        observed.tool_calls = vec![astra_thin_client::SessionTranscriptToolCall {
            tool_use_id: "same-call".into(),
            name: "write_file".into(),
            arguments: r#"{"path":"a.txt"}"#.into(),
        }];
        observed.tool_result = Some(astra_thin_client::SessionTranscriptToolResult {
            runtime_advisories: vec![],
            tool_use_id: "same-call".into(),
            name: Some("write_file".into()),
            status: Some("completed".into()),
            duration_ms: Some(1),
        });
        let mut result = observed.clone();
        result.item_seq = 2;
        result.run_id = Some("root-b".into());
        result.role = "tool".into();
        result.content = "output-b".into();
        result.tool_calls.clear();
        result.tool_result.as_mut().unwrap().status = None;
        let mut call = result.clone();
        call.item_seq = 3;
        call.role = "assistant".into();
        call.content.clear();
        call.tool_result = None;
        call.tool_calls = observed.tool_calls.clone();
        call.tool_calls[0].arguments = r#"{"path":"b.txt"}"#.into();
        let projected = durable_transcript_items(&[observed, result, call]);
        assert_eq!(
            projected.len(),
            3,
            "two tool rows and one partial-output marker"
        );
        let view = TranscriptView::from_snapshot(TranscriptSnapshot::new(projected), 24, 100);
        let text = view.export_plain_lines().join("\n");
        assert!(text.contains("Observed locally"), "{text}");
        assert!(text.contains("Partial retained output"), "{text}");
        assert!(
            text.contains("output-a") && text.contains("output-b"),
            "{text}"
        );
    }

    #[test]
    fn durable_delegation_row_keeps_child_payload_out_of_parent_view() {
        let mut view = loading_view(100, 24);
        let item = |item_seq, role: &str, content: &str| astra_thin_client::SessionTranscriptItem {
            model_item_id: None,
            session_id: "session-1".into(),
            item_seq,
            run_id: Some("run-child".into()),
            role: role.into(),
            content: content.into(),
            reasoning: None,
            reasoning_status: None,
            tool_calls: Vec::new(),
            tool_result: None,
            evidence: None,
            source_event_id: None,
            created_at: "2026-07-11T00:00:00".into(),
        };
        let mut call = item(1, "assistant", "");
        call.tool_calls
            .push(astra_thin_client::SessionTranscriptToolCall {
                tool_use_id: "spawn-1".into(),
                name: "agent".into(),
                arguments: r#"{"action":"spawn","prompt":"private child instructions"}"#.into(),
            });
        let mut result = item(2, "tool", r#"{"result":"private child result"}"#);
        result.tool_result = Some(astra_thin_client::SessionTranscriptToolResult {
            runtime_advisories: vec!["Check the member status before continuing.".into()],
            tool_use_id: "spawn-1".into(),
            name: Some("agent".into()),
            status: Some("launched".into()),
            duration_ms: Some(12),
        });
        let result_only = result.clone();
        assert_eq!(
            tool_display_content(&result, true)
                .matches("private child result")
                .count(),
            0
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result.content).unwrap()["result"],
            "private child result"
        );
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![call, result],
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::DurableServer,
        );
        let output = rendered(&view);
        assert!(output.contains("Agent spawn"), "{output}");
        assert!(!output.contains("private child instructions"), "{output}");
        assert!(!output.contains("private child result"), "{output}");
        let detailed = view.transcript.export_plain_lines().join("\n");
        assert!(
            !detailed.contains("private child instructions"),
            "{detailed}"
        );
        assert!(!detailed.contains("private child result"), "{detailed}");
        assert_eq!(
            view.transcript
                .export_plain_lines()
                .join("\n")
                .matches("Check the member status before continuing.")
                .count(),
            1
        );

        // A page boundary can leave the result without its initiating call.
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![result_only],
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::DurableServer,
        );
        assert!(!rendered(&view).contains("private child result"));
        let detailed = view.transcript.export_plain_lines().join("\n");
        assert!(
            !detailed.contains("private child instructions"),
            "{detailed}"
        );
        assert!(!detailed.contains("private child result"), "{detailed}");
        assert_eq!(
            view.transcript
                .export_plain_lines()
                .join("\n")
                .matches("Check the member status before continuing.")
                .count(),
            1
        );

        let mut deferred = item(3, "assistant", "");
        deferred.tool_calls.push(astra_thin_client::SessionTranscriptToolCall {
            tool_use_id: "deferred-spawn".into(),
            name: "invoke_tool".into(),
            arguments: r#"{"name":"agent","arguments":{"action":"spawn","prompt":"private deferred instructions"}}"#.into(),
        });
        let mut deferred_result = item(4, "tool", "private deferred result");
        deferred_result.tool_result = Some(astra_thin_client::SessionTranscriptToolResult {
            runtime_advisories: vec![],
            tool_use_id: "deferred-spawn".into(),
            name: Some("invoke_tool".into()),
            status: Some("launched".into()),
            duration_ms: Some(12),
        });
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![deferred, deferred_result],
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::DurableServer,
        );
        let output = rendered(&view);
        assert!(output.contains("Agent spawn"), "{output}");
        assert!(
            !output.contains("private deferred instructions"),
            "{output}"
        );
        assert!(!output.contains("private deferred result"), "{output}");

        let mut orphan = item(5, "tool", "private orphaned deferred result");
        orphan.tool_result = Some(astra_thin_client::SessionTranscriptToolResult {
            runtime_advisories: vec![],
            tool_use_id: "unknown-deferred".into(),
            name: Some("invoke_tool".into()),
            status: Some("success".into()),
            duration_ms: Some(1),
        });
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![orphan],
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::DurableServer,
        );
        assert!(!rendered(&view).contains("private orphaned deferred result"));
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
        assert!(
            matches!(view.take_action_request(), Some(ViewActionRequest { action: BottomPaneViewAction::LoadAgentActivity {
            run_id, session_id, before_event_idx: None, ..
        }, .. }) if run_id == "run-child" && session_id == "session-1")
        );
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        view.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert!(
            !view.is_complete(),
            "left while typing a transcript search must not leave the agent conversation"
        );
        assert!(view.cursor_pos(Rect::new(0, 0, 80, 20)).is_some());
        for ch in "qros".chars() {
            view.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        assert!(rendered(&view).contains("Search: /qros"));
        assert!(view.take_action_request().is_none());
        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!view.transcript.is_search_active());
        assert!(!view.is_complete());
        assert!(view.take_action_request().is_none());
    }

    #[test]
    fn agent_refresh_keeps_confirmed_history_and_reports_sync_progress_or_failure() {
        let mut view = loading_view(80, 24);
        assert!(
            view.refresh_agent_live_gap(&astra_turn_core::agent_live_event::AgentLiveGap {
                run_id: "run-child".into(),
                agent_id: "agent-1".into(),
                dropped_event_count: 2,
            })
        );
        let initial = rendered(&view);
        assert!(
            initial.contains("Syncing durable agent history…"),
            "{initial}"
        );
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "unreconciled live finding".into()
            },
        }));
        let live = rendered(&view);
        assert!(
            live.contains("Live activity · not in loaded history"),
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
            settled.contains("Live activity · not in loaded history"),
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
    fn durable_output_retires_only_its_item_and_represented_part() {
        let mut view = loading_view(80, 24);
        let event = |model_item_id: &str, text: &str, reasoning| AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: if reasoning {
                AgentLiveEventKind::ThinkingDelta {
                    model_item_id: Some(model_item_id.into()),
                    text: text.into(),
                }
            } else {
                AgentLiveEventKind::OutputDelta {
                    model_item_id: Some(model_item_id.into()),
                    text: text.into(),
                }
            },
        };
        let mut canonical = page();
        canonical.items[0].model_item_id = Some("accepted-A".into());
        canonical.items[0].reasoning = None;
        canonical.items[0].reasoning_status = None;
        canonical.has_more = false;
        canonical.next_before_seq = None;
        view.apply_live_event(&event("accepted-A", "unpersisted reasoning", true));
        view.apply_live_event(&event("accepted-A", "Found the race.", false));
        // Identical text from a resumed/unaccepted response is a different
        // message, not a duplicate that content matching may erase.
        view.apply_live_event(&event("partial-B", "Found the race.", false));
        view.apply_page(
            canonical.clone(),
            true,
            AgentTranscriptSource::DurableServer,
        );
        view.apply_page(
            canonical.clone(),
            true,
            AgentTranscriptSource::DurableServer,
        );
        // A page can win the race against queued live delivery.
        view.apply_live_event(&event("accepted-A", "Found the race.", false));
        assert_eq!(rendered(&view).matches("Found the race.").count(), 2);
        assert!(matches!(view.live.items.as_slice(), [
            LiveTranscriptEntry { item: LiveTranscriptItem::Reasoning(reasoning), .. },
            LiveTranscriptEntry { item: LiveTranscriptItem::Assistant(output), .. },
        ] if reasoning.model_item_id() == Some("accepted-A")
            && output.model_item_id() == Some("partial-B")));
    }

    #[test]
    fn committed_final_answer_is_not_rendered_twice() {
        let mut view = loading_view(80, 24);
        view.loading = false;
        let event = |kind| AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind,
        };
        let mut canonical = page();
        canonical.items[0].reasoning = None;
        canonical.items[0].reasoning_status = None;
        canonical.items[0].source_event_id = Some("accepted-final-answer".into());
        canonical.items[0].model_item_id = Some("model-answer".into());
        canonical.has_more = false;
        canonical.next_before_seq = None;
        assert!(
            view.apply_live_event(&event(AgentLiveEventKind::OutputDelta {
                model_item_id: Some("model-answer".into()),
                text: canonical.items[0].content.clone(),
            }))
        );
        assert!(view.apply_live_event(&event(AgentLiveEventKind::Signal(
            AgentLiveSignal::TranscriptCommitted {
                model_item_id: Some("model-answer".into()),
                source_event_id: "accepted-final-answer".into(),
                transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
            },
        ))));
        view.apply_page(
            canonical.clone(),
            true,
            AgentTranscriptSource::DurableServer,
        );
        assert!(
            view.apply_live_event(&event(AgentLiveEventKind::OutputDelta {
                model_item_id: Some("model-answer".into()),
                text: canonical.items[0].content.clone(),
            }))
        );

        // This is the actual user-visible oracle, not a comparison of the
        // live buffer with itself. Content equality must not implement the
        // repair: the producer must establish which message was accepted.
        let output = rendered(&view);
        assert_eq!(output.matches("Found the race.").count(), 1, "{output}");
        assert!(
            !output.contains("Live activity · not in loaded history"),
            "{output}"
        );
    }

    #[test]
    fn terminal_signal_during_activity_load_refreshes_transcript_once() {
        use astra_thin_client::protocol::RunActivityPage;
        for kind in [
            AgentLiveEventKind::Signal(AgentLiveSignal::TranscriptCommitted {
                model_item_id: None,
                source_event_id: "committed".into(),
                transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
            }),
            AgentLiveEventKind::AgentTerminated {
                termination: astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                duration_ms: 1,
                reason: None,
            },
        ] {
            let mut view = loading_view(80, 24);
            view.loading = true;
            let _ = view.take_action_request();
            assert!(view.apply_live_event(&AgentLiveEvent {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                kind
            }));
            assert!(view.take_action_request().is_none());
            view.refresh_agent_transcript(AgentTranscriptUpdate::ActivityLoaded {
                agent_id: "agent-1".into(),
                run_id: "run-child".into(),
                replace: true,
                page: Ok(RunActivityPage {
                    session_id: "session-1".into(),
                    run_id: "run-child".into(),
                    source_run_id: "root".into(),
                    events: Vec::new(),
                    next_before_event_idx: None,
                    has_more: false,
                }),
            });
            assert!(matches!(
                view.take_action_request(),
                Some(ViewActionRequest {
                    action: BottomPaneViewAction::LoadAgentTranscript {
                        before_seq: None,
                        ..
                    },
                    ..
                })
            ));
            assert!(view.take_action_request().is_none());
        }
    }

    #[test]
    fn committed_identity_confirms_page_without_guessing_live_model_mapping() {
        let mut view = loading_view(100, 30);
        view.loading = false;
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "old live fragment".into()
            },
        }));
        assert!(view.apply_live_event(&AgentLiveEvent {
            agent_id: "agent-1".into(),
            run_id: "run-child".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::TranscriptCommitted {
                model_item_id: None,
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "new resumed fragment".into()
            },
        }));
        view.apply_page(
            astra_thin_client::SessionTranscriptPage {
                session_id: "session-1".into(),
                items: vec![astra_thin_client::SessionTranscriptItem {
                    model_item_id: None,
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "final live finding".into()
            },
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
            kind: AgentLiveEventKind::ThinkingDelta {
                model_item_id: Some("test-model-item".into()),
                text: "Inspect the scheduler state.".into()
            },
        }));
        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-1".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "I found the race.".into()
            },
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
                        model_item_id: None,
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
                        model_item_id: None,
                        session_id: "session-1".into(),
                        item_seq: 8,
                        run_id: Some("run-child".into()),
                        role: "tool".into(),
                        content: "full scheduler source".into(),
                        reasoning: None,
                        reasoning_status: None,
                        tool_calls: Vec::new(),
                        tool_result: Some(astra_thin_client::SessionTranscriptToolResult {
                            runtime_advisories: vec![],
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
                        model_item_id: None,
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
                next_before_seq: None,
                has_more: false,
            },
            true,
            AgentTranscriptSource::LocalJournalOnly,
        );
        assert_eq!(
            view.live.items.len(),
            3,
            "two live objects and one lightweight request fallback remain"
        );
        let reconciled = view.transcript.export_plain_lines().join("\n");
        assert_eq!(reconciled.matches("I found the race.").count(), 2);
        assert!(reconciled.contains("src/scheduler.rs"));
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "must not leak".into()
            },
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
        };
        let item = |item_seq: i64,
                    source_event_id: &str,
                    evidence: astra_turn_types::AgentTranscriptEvidence| {
            astra_thin_client::SessionTranscriptItem {
                model_item_id: None,
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
            model_item_id: None,
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "live review finding".into()
            },
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "must not bind".into()
            },
        }));

        assert!(view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-pending".into(),
            agent_id: "agent-pending".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "first visible finding".into()
            },
        }));
        assert_eq!(view.run_id, "run-pending");
        let bound = rendered(&view);
        assert!(bound.contains("first visible finding"), "{bound}");
        assert!(!view.refresh_agent_live_event(&AgentLiveEvent {
            run_id: "run-reused-profile".into(),
            agent_id: "agent-pending".into(),
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "must not leak".into()
            },
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "child evidence".into()
            },
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
                kind: AgentLiveEventKind::OutputDelta {
                    model_item_id: Some("test-model-item".into()),
                    text: (0..2_000)
                        .map(|index| format!("agent-line-{index}\n"))
                        .collect(),
                },
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "live finding before receipt".into()
            },
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
            kind: AgentLiveEventKind::OutputDelta {
                model_item_id: Some("test-model-item".into()),
                text: "live review finding".into()
            },
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
                        model_item_id: None,
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
                        model_item_id: None,
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
                            runtime_advisories: vec![],
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
