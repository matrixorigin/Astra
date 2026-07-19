//! `ChatWidget` — the single event router for the refactored TUI.
//!
//! Owns everything related to the scrollback + active stream:
//! the committed history (`Vec<Arc<dyn HistoryCell>>`), the
//! `active_cell: Option<Box<dyn HistoryCell>>` slot, and the
//! session identity. Does NOT own the composer / bottom pane /
//! popup menus — those stay in `BottomPane` because they're a
//! separate concern (input vs. output).
//!
//! The event flow is:
//!
//! ```text
//! AppEvent ──▶ ChatWidget::handle_event ──▶ mutate history/active_cell
//!                                         ──▶ (outer draws on next frame)
//! ```
//!
//! `handle_event` is deliberately one big `match` (§3.2 of the
//! design doc). A reducer abstraction was tried and failed — the
//! async HTTP stream + direct terminal IO don't map cleanly to pure
//! `State, Action -> State`. One readable match beats a reducer that
//! leaks `Effect`s everywhere.
//!
//! The event loop owns I/O and forwards typed view events here; this module
//! owns the resulting conversation projection.

mod agent_control_surface;
mod bridge;
mod resume;
#[cfg(test)]
mod turn_driver;

use self::agent_control_surface::{AgentControlOutcome, AgentControlSurface};
pub(crate) use bridge::{TurnContext, translate};
pub(crate) use resume::load as load_resume;

use std::{collections::HashSet, sync::Arc};

use super::agent_run_projection::{
    AgentProjectionConfidence, AgentProjectionSource, AgentRunState, AgentRunStatus,
};
use super::history_cell::{
    HistoryCell, assistant::AssistantCell, reasoning::ReasoningCell, system::SystemCell,
    task::TaskCell, tool::ToolCell, turn_summary::TurnSummaryCell, user::UserCell,
};
use super::turn_event::TurnEvent;
use crate::VerdictEvent;
use astra_turn_core::compaction_types::CompactionEvent;

/// Events the ChatWidget knows how to route. Grouped by origin so
/// `handle_event` can scale to more variants without bloating a
/// single match: user-originated input vs wire-originated streaming.
///
/// Self-contained — no borrowed references, no lifetimes — so it's
/// easy to buffer, replay in tests, and cross thread boundaries.
#[derive(Debug, Clone)]
pub(crate) enum AppEvent {
    /// User-originated events (composer submit, future edits, …).
    User(UserEvent),
    /// Wire-originated events from the SSE host / model stream.
    Wire(Box<WireEvent>),
}

impl AppEvent {
    pub(crate) fn wire(event: WireEvent) -> Self {
        Self::Wire(Box::new(event))
    }
}

/// User-side sources. Today just composer submit; future candidates:
/// Edit, Cancel, Retry.
#[derive(Debug, Clone)]
pub(crate) enum UserEvent {
    /// User pressed Enter in the composer. Opens a new turn.
    Submit(String),
}

/// Wire-side sources. Streaming tokens, tool lifecycle, turn end.
#[derive(Debug, Clone)]
pub(crate) enum WireEvent {
    /// Token streamed as part of the model's final reply body.
    AnswerDelta(String),

    /// Chunk of reasoning / thinking content. Separate from
    /// `AnswerDelta` so the cell types don't get muddled —
    /// ReasoningCell vs AssistantCell are different things.
    ReasoningDelta(String),

    /// Server/host tells us reasoning has ended. Cells collapse
    /// into their finalised form on this signal.
    ReasoningDone,

    /// Server announced a new tool invocation starting.
    ToolStarted {
        name: String,
        description: String,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    },
    AgentControlStarted {
        action: String,
        label: String,
        tool_use_id: String,
        agent_id: Option<String>,
        fanout_slot: Option<astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity>,
        fanout_title: Option<String>,
    },

    /// Tool finished. `status` mirrors the canonical string we receive on
    /// the wire (`"completed"`, `"failed"`, or `"skipped"`).
    ToolCompleted {
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    },
    AgentControlCompleted {
        action: String,
        label: String,
        status: String,
        duration_ms: u64,
        output: Option<String>,
        tool_use_id: String,
        agent_id: Option<String>,
    },

    /// Mid-flight progress signal for the active ToolCell —
    /// `lines`/`bytes` are cumulative counters since the tool
    /// started. Used to render real "streaming · N lines · K KB"
    /// status on long-running cells; the cell falls back to an
    /// indeterminate animation when this event never arrives (non-
    /// streaming tools like `read_file` / `git(action=log)`).
    ToolOutput {
        name: String,
        lines: u64,
        bytes: u64,
    },
    AgentLive(astra_turn_core::agent_live_event::AgentLiveEvent),
    AgentLiveBatch(Vec<astra_turn_core::agent_live_event::AgentLiveEvent>),
    AgentLiveGap(astra_turn_core::agent_live_event::AgentLiveGap),
    AgentCommunication(astra_turn_types::AgentCommunicationEvent),

    /// Turn ended cleanly; ChatWidget should emit a summary cell.
    TurnComplete(Box<TurnStats>),

    /// Turn ended with an error. Error text gets humanised by
    /// `SystemCell::error` before storage.
    TurnError(String),
    /// System-level warning (not necessarily turn-scoped). Rendered as `SystemCell::warning` in scrollback.
    SystemWarning(String),
    /// System-level informational message. Rendered as `SystemCell::info` in scrollback.
    SystemInfo(String),
    ExplainReport(Vec<serde_json::Value>),
    VerdictReport(Vec<crate::VerdictEvent>),
    /// Structured compaction event — renders as a system info cell
    /// so the user sees live context-health feedback in scrollback.
    Compaction(CompactionEvent),
}

/// Per-turn metrics the outer loop collects and hands to
/// `TurnComplete` for the summary band. Boxed in the event enum
/// so the enum stays small (clippy::large_enum_variant guard).
#[derive(Debug, Clone, Default)]
pub(crate) struct TurnStats {
    pub elapsed_ms: Option<u64>,
    pub ttft_ms: Option<u64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    /// Of the `tokens_in` total, how many were served from the
    /// provider's prompt cache. Drives the `💾 N%` segment in the
    /// per-turn summary band. `None` when the provider didn't
    /// report cache stats this turn (e.g. first turn, no cache
    /// participation, DeepSeek with cache disabled).
    pub cache_read_tokens: Option<u64>,
    pub tools: u32,
    pub cumulative_tokens: Option<u64>,
    pub cumulative_cost_usd: Option<f64>,
}

/// Single source of truth for the chat-view scrollback.
///
/// `history` holds **committed** cells (finalised, persistable,
/// immutable). `active_cell` holds the **live** non-Task cell
/// (assistant streaming, reasoning, single-tool execution) — these
/// are mutually exclusive within a turn so one slot suffices.
///
struct AgentRunProjection {
    detail: Box<TaskCell>,
    state: AgentRunState,
    state_observed_at: std::time::Instant,
    terminal_at: Option<std::time::Instant>,
    reported_tool_calls: usize,
    reported_child_agents: usize,
    messages_sent: usize,
    messages_received: usize,
    run_id: Option<String>,
    parent_run_id: Option<String>,
    depth: u32,
    metadata_source: Option<AgentProjectionSource>,
    runtime_facts: astra_thin_client::SessionRunRuntimeFacts,
    runtime_sources: RuntimeFactSources,
    control_target: Option<crate::tui::agent_run_projection::AgentControlTarget>,
    available_actions: Vec<astra_thin_client::SessionRunAction>,
    control_source: Option<AgentProjectionSource>,
    transcript_target: Option<crate::tui::agent_run_projection::AgentTranscriptTarget>,
    transcript_source: Option<AgentProjectionSource>,
    durable_event_high_watermark: Option<i64>,
    control_requested_from: Option<AgentRunState>,
    /// Latest structured reason why this run needs attention. This is a
    /// projection field, never a prompt-facing transcript substitute: the
    /// full event remains in the run transcript with its stable identity.
    attention_summary: Option<String>,
    live_transcript_events:
        std::collections::VecDeque<astra_turn_core::agent_live_event::AgentLiveEvent>,
    live_transcript_bytes: usize,
    live_transcript_dropped: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RuntimeFactSources {
    runtime_profile: Option<AgentProjectionSource>,
    offering_id: Option<AgentProjectionSource>,
    model_name: Option<AgentProjectionSource>,
    agent_binding_id: Option<AgentProjectionSource>,
    agent_binding_name: Option<AgentProjectionSource>,
    agent_binding_schema_version: Option<AgentProjectionSource>,
    capability_server_refs: Option<AgentProjectionSource>,
    background: Option<AgentProjectionSource>,
    permission: Option<AgentProjectionSource>,
}

impl AgentRunProjection {
    fn new(id: String, label: String, state: AgentRunState) -> Self {
        let mut projection = Self {
            detail: Box::new(TaskCell::new_running(id, label)),
            state,
            state_observed_at: std::time::Instant::now(),
            terminal_at: None,
            reported_tool_calls: 0,
            reported_child_agents: 0,
            messages_sent: 0,
            messages_received: 0,
            run_id: None,
            parent_run_id: None,
            depth: 1,
            metadata_source: None,
            runtime_facts: Default::default(),
            runtime_sources: Default::default(),
            control_target: None,
            available_actions: Vec::new(),
            control_source: None,
            transcript_target: None,
            transcript_source: None,
            durable_event_high_watermark: None,
            control_requested_from: None,
            attention_summary: None,
            live_transcript_events: std::collections::VecDeque::new(),
            live_transcript_bytes: 0,
            live_transcript_dropped: 0,
        };
        projection.sync_detail_status();
        projection
    }

    fn set_state(&mut self, state: AgentRunState) -> bool {
        if state.source != AgentProjectionSource::LocalIntent
            && matches!(
                self.state.status,
                AgentRunStatus::Pausing | AgentRunStatus::Resuming | AgentRunStatus::Cancelling
            )
            && let Some(requested_from) = self.control_requested_from
        {
            if state.status == requested_from.status {
                // A poll that still reports the pre-request state is older
                // than the pending local control operation. Keep the overlay
                // until an authoritative transition or rejection arrives.
                return false;
            }
            self.control_requested_from = None;
        }
        if !should_accept_agent_state(self.state, state) {
            return false;
        }
        self.state = state;
        if !matches!(
            state.status,
            AgentRunStatus::Waiting | AgentRunStatus::Paused
        ) {
            self.attention_summary = None;
        }
        self.state_observed_at = std::time::Instant::now();
        if state.status.is_terminal() {
            self.terminal_at.get_or_insert(self.state_observed_at);
        } else {
            self.terminal_at = None;
        }
        self.sync_detail_status();
        true
    }

    fn set_attention_summary(&mut self, summary: Option<String>) {
        self.attention_summary = summary.filter(|summary| !summary.trim().is_empty());
    }

    fn record_live_transcript_event(
        &mut self,
        event: &astra_turn_core::agent_live_event::AgentLiveEvent,
    ) {
        const MAX_LIVE_TRANSCRIPT_BYTES: usize = 512 * 1024;
        use astra_turn_core::agent_live_event::AgentLiveEventKind;

        let appended = match (
            self.live_transcript_events
                .back_mut()
                .map(|event| &mut event.kind),
            &event.kind,
        ) {
            (
                Some(AgentLiveEventKind::OutputDelta(previous)),
                AgentLiveEventKind::OutputDelta(next),
            )
            | (
                Some(AgentLiveEventKind::ThinkingDelta(previous)),
                AgentLiveEventKind::ThinkingDelta(next),
            ) => {
                previous.push_str(next);
                self.live_transcript_bytes = self.live_transcript_bytes.saturating_add(next.len());
                true
            }
            _ => false,
        };
        if !appended {
            let bytes = agent_live_event_payload_bytes(event);
            self.live_transcript_events.push_back(event.clone());
            self.live_transcript_bytes = self.live_transcript_bytes.saturating_add(bytes);
        }
        while self.live_transcript_bytes > MAX_LIVE_TRANSCRIPT_BYTES {
            let Some(evicted) = self.live_transcript_events.pop_front() else {
                break;
            };
            self.live_transcript_bytes = self
                .live_transcript_bytes
                .saturating_sub(agent_live_event_payload_bytes(&evicted));
            self.live_transcript_dropped = self.live_transcript_dropped.saturating_add(1);
        }
    }

    fn set_controls(
        &mut self,
        source: AgentProjectionSource,
        target: crate::tui::agent_run_projection::AgentControlTarget,
        available_actions: Vec<astra_thin_client::SessionRunAction>,
    ) {
        if self.control_source.is_some_and(|current| {
            agent_projection_source_rank(current) > agent_projection_source_rank(source)
        }) {
            return;
        }
        self.control_source = Some(source);
        self.control_target = Some(target);
        self.available_actions = available_actions;
    }

    fn set_transcript_target(
        &mut self,
        source: AgentProjectionSource,
        target: crate::tui::agent_run_projection::AgentTranscriptTarget,
    ) {
        if self.transcript_source.is_some_and(|current| {
            agent_projection_source_rank(current) > agent_projection_source_rank(source)
        }) {
            return;
        }
        self.transcript_source = Some(source);
        self.transcript_target = Some(target);
    }

    fn set_runtime_metadata(
        &mut self,
        source: AgentProjectionSource,
        run_id: String,
        parent_run_id: Option<String>,
        depth: u32,
        child_agents: usize,
    ) {
        if self.metadata_source.is_some_and(|current| {
            agent_projection_source_rank(current) > agent_projection_source_rank(source)
        }) {
            // A lower-confidence source must not replace durable/local
            // metadata, but a typed live event still carries the immutable
            // execution identity. Preserve that identity if the earlier
            // snapshot omitted it; otherwise the transcript cannot be opened.
            if self.run_id.is_none() {
                self.run_id = Some(run_id);
            }
            return;
        }
        self.metadata_source = Some(source);
        self.run_id = Some(run_id);
        self.parent_run_id = parent_run_id;
        self.depth = depth.max(1);
        self.reported_child_agents = child_agents;
    }

    fn set_runtime_facts(
        &mut self,
        source: AgentProjectionSource,
        mut facts: astra_thin_client::SessionRunRuntimeFacts,
    ) {
        fn accepts(
            current: Option<AgentProjectionSource>,
            incoming: AgentProjectionSource,
        ) -> bool {
            current.is_none_or(|current| {
                agent_projection_source_rank(incoming) >= agent_projection_source_rank(current)
            })
        }

        macro_rules! merge_fact {
            ($field:ident) => {
                if facts.$field.is_some() && accepts(self.runtime_sources.$field, source) {
                    self.runtime_facts.$field = facts.$field.take();
                    self.runtime_sources.$field = Some(source);
                }
            };
        }
        merge_fact!(runtime_profile);
        merge_fact!(offering_id);
        merge_fact!(model_name);
        merge_fact!(agent_binding_id);
        merge_fact!(agent_binding_name);
        merge_fact!(agent_binding_schema_version);
        merge_fact!(capability_server_refs);
        merge_fact!(background);
        merge_fact!(permission);
    }

    fn activity_counts(
        &self,
        durable_snapshot_truncated: bool,
    ) -> crate::tui::agent_run_projection::AgentActivityCounts {
        crate::tui::agent_run_projection::AgentActivityCounts {
            tool_calls: self.detail.children.len().max(self.reported_tool_calls),
            child_agents: self.reported_child_agents,
            messages_sent: self.messages_sent,
            messages_received: self.messages_received,
            child_agents_partial: durable_snapshot_truncated
                && self.metadata_source == Some(AgentProjectionSource::DurableServer),
        }
    }

    fn begin_control(&mut self, action: astra_thin_client::SessionRunAction) -> bool {
        if !self.available_actions.contains(&action) {
            return false;
        }
        if self.control_requested_from.is_none() {
            self.control_requested_from = Some(self.state);
        }
        let status = match action {
            astra_thin_client::SessionRunAction::Pause => AgentRunStatus::Pausing,
            astra_thin_client::SessionRunAction::Resume
            | astra_thin_client::SessionRunAction::ContinueSession => AgentRunStatus::Resuming,
            astra_thin_client::SessionRunAction::Cancel => AgentRunStatus::Cancelling,
        };
        self.set_state(AgentRunState::local_intent(status))
    }

    fn reject_control(&mut self) -> bool {
        let Some(mut previous) = self.control_requested_from.take() else {
            return false;
        };
        if previous.status.is_active() {
            previous.confidence = AgentProjectionConfidence::Stale;
        }
        self.state = previous;
        self.state_observed_at = std::time::Instant::now();
        self.sync_detail_status();
        true
    }

    fn mark_unconfirmed_if_active(&mut self) {
        if matches!(
            self.state.source,
            AgentProjectionSource::LiveStream | AgentProjectionSource::LocalIntent
        ) && self.state.mark_unconfirmed_if_active()
        {
            self.state_observed_at = std::time::Instant::now();
            self.sync_detail_status();
        }
    }

    fn mark_stale_if_active(&mut self) {
        if self.state.mark_stale_if_active() {
            self.state_observed_at = std::time::Instant::now();
            self.sync_detail_status();
        }
    }

    fn sync_detail_status(&mut self) {
        use crate::tui::history_cell::task::TaskStatus;

        self.detail.status = if self.state.status.is_active()
            && matches!(
                self.state.confidence,
                AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
            ) {
            if self.detail.duration_ms.is_none() {
                self.detail.duration_ms = Some(self.detail.started_at.elapsed().as_millis() as u64);
            }
            TaskStatus::Unconfirmed
        } else {
            match self.state.status {
                AgentRunStatus::Starting
                | AgentRunStatus::Running
                | AgentRunStatus::Pausing
                | AgentRunStatus::Resuming
                | AgentRunStatus::Cancelling => {
                    self.detail.completed_at = None;
                    self.detail.duration_ms = None;
                    TaskStatus::Running
                }
                AgentRunStatus::Waiting | AgentRunStatus::Paused => {
                    self.detail.completed_at = None;
                    self.detail.duration_ms = None;
                    TaskStatus::Waiting
                }
                AgentRunStatus::Completed | AgentRunStatus::Delegated => TaskStatus::Completed,
                AgentRunStatus::Interrupted => TaskStatus::Interrupted,
                AgentRunStatus::Failed => TaskStatus::Failed,
                AgentRunStatus::Cancelled => TaskStatus::Cancelled,
            }
        };
    }
}

fn should_accept_agent_state(current: AgentRunState, incoming: AgentRunState) -> bool {
    let current_rank = current.source_rank();
    let incoming_rank = incoming.source_rank();

    // A fresh owning source can temporarily take over a stale projection even
    // when the durable server normally has the higher authority rank.
    if current.confidence == AgentProjectionConfidence::Stale
        && incoming.confidence == AgentProjectionConfidence::Confirmed
        && current.status == incoming.status
    {
        return true;
    }

    // Repeated lower-authority observations must not erase a confirmation.
    if current.status == incoming.status && incoming_rank < current_rank {
        return false;
    }
    if current.status.is_terminal() {
        // Confirmed terminal facts are monotonic for one immutable run id.
        // An observed live terminal may still be corrected by an owning
        // runtime before durable settlement (for example an interrupted live
        // stream that the local runtime proves is still running).
        return if incoming.status.is_terminal() {
            incoming_rank >= current_rank
        } else {
            current.confidence != AgentProjectionConfidence::Confirmed
                && incoming.confidence == AgentProjectionConfidence::Confirmed
        };
    }

    // A newer terminal stream event is useful immediately even before the
    // next runtime snapshot confirms it. Active-to-active observations are
    // also allowed to reflect newer work/wait/cancel activity.
    true
}

fn agent_projection_source_rank(source: AgentProjectionSource) -> u8 {
    AgentRunState {
        status: AgentRunStatus::Running,
        confidence: AgentProjectionConfidence::Observed,
        source,
    }
    .source_rank()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentControlAction {
    Spawn,
    GetResult,
}

impl AgentControlAction {
    fn from_wire(value: &str) -> Option<Self> {
        match value {
            "spawn" => Some(Self::Spawn),
            "get_result" => Some(Self::GetResult),
            _ => None,
        }
    }
}

struct AgentControlBinding {
    run_key: String,
    action: AgentControlAction,
}

#[derive(PartialEq, Eq)]
struct AgentRunSignature {
    id: String,
    state: AgentRunState,
    description: String,
    reported_tool_calls: usize,
    reported_child_agents: usize,
    messages_sent: usize,
    messages_received: usize,
    run_id: Option<String>,
    parent_run_id: Option<String>,
    depth: u32,
    metadata_source: Option<AgentProjectionSource>,
    runtime_facts: astra_thin_client::SessionRunRuntimeFacts,
    runtime_sources: RuntimeFactSources,
    output: Option<(usize, u64)>,
    error: Option<(usize, u64)>,
    fanout: Option<crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership>,
    control_target: Option<crate::tui::agent_run_projection::AgentControlTarget>,
    transcript_target: Option<crate::tui::agent_run_projection::AgentTranscriptTarget>,
    available_actions: Vec<astra_thin_client::SessionRunAction>,
    durable_event_high_watermark: Option<i64>,
    attention: Option<(usize, u64)>,
}

fn agent_text_fingerprint(value: Option<&str>) -> Option<(usize, u64)> {
    use std::hash::{Hash, Hasher};

    value.map(|value| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        value.hash(&mut hasher);
        (value.len(), hasher.finish())
    })
}

#[derive(Default)]
struct AgentRunRegistry {
    runs: std::collections::HashMap<String, AgentRunProjection>,
    order: Vec<String>,
    fanout_membership: std::collections::HashMap<
        String,
        crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership,
    >,
    /// Structured control-call identity. Generic tool descriptions are never
    /// parsed to recover either the action or run identity.
    control_bindings: std::collections::HashMap<String, AgentControlBinding>,
    /// A bounded durable snapshot proves the included rows, but it cannot
    /// prove that absent runs or child counts are complete.
    durable_snapshot_truncated: bool,
    server_truth_state: crate::tui::server_agent_observer::ServerAgentTruthState,
}

impl AgentRunRegistry {
    fn prune_terminal_history(&mut self, max_recent_terminal: usize) {
        let mut keep = self
            .order
            .iter()
            .filter(|key| {
                self.runs
                    .get(*key)
                    .is_some_and(|projection| projection.state.status.is_active())
            })
            .cloned()
            .collect::<std::collections::HashSet<_>>();
        keep.extend(
            self.order
                .iter()
                .rev()
                .filter(|key| {
                    self.runs
                        .get(*key)
                        .is_some_and(|projection| projection.state.status.is_terminal())
                })
                .take(max_recent_terminal)
                .cloned(),
        );

        // Preserve ancestors of retained rows so the workbench tree never
        // turns recent grandchildren into unexplained roots.
        loop {
            let parents = keep
                .iter()
                .filter_map(|key| self.runs.get(key))
                .filter_map(|projection| projection.parent_run_id.as_deref())
                .filter_map(|parent_run_id| self.key_for_run_id(parent_run_id))
                .map(str::to_owned)
                .collect::<Vec<_>>();
            let before = keep.len();
            keep.extend(parents);
            if keep.len() == before {
                break;
            }
        }

        let evicted = self
            .order
            .iter()
            .filter(|key| !keep.contains(*key))
            .cloned()
            .collect::<Vec<_>>();
        for key in evicted {
            self.remove(&key);
        }
    }

    fn ids(&self) -> Vec<String> {
        self.order.clone()
    }

    /// The compact status strip represents one current work surface, not the
    /// session archive. Prefer the newest live fanout and include its settled
    /// siblings; when no fanout is live, briefly retain only the newest group.
    fn status_strip_ids(&self) -> Vec<String> {
        let live_group = self.order.iter().rev().find_map(|id| {
            self.runs
                .get(id)
                .filter(|projection| projection.state.status.is_active())
                .and_then(|_| self.fanout_membership.get(id))
                .map(|fanout| fanout.group_id.as_str())
        });
        if let Some(group_id) = live_group {
            return self
                .order
                .iter()
                .filter(|id| {
                    self.fanout_membership
                        .get(*id)
                        .is_some_and(|fanout| fanout.group_id == group_id)
                })
                .cloned()
                .collect();
        }

        let standalone_live = self
            .order
            .iter()
            .filter(|id| {
                self.runs
                    .get(*id)
                    .is_some_and(|projection| projection.state.status.is_active())
                    && !self.fanout_membership.contains_key(*id)
            })
            .cloned()
            .collect::<Vec<_>>();
        if !standalone_live.is_empty() {
            return standalone_live;
        }

        let newest_group = self
            .order
            .iter()
            .rev()
            .find_map(|id| self.fanout_membership.get(id))
            .map(|fanout| fanout.group_id.as_str());
        match newest_group {
            Some(group_id) => self
                .order
                .iter()
                .filter(|id| {
                    self.fanout_membership
                        .get(*id)
                        .is_some_and(|fanout| fanout.group_id == group_id)
                })
                .cloned()
                .collect(),
            None => self.order.last().cloned().into_iter().collect(),
        }
    }

    fn get(&self, id: &str) -> Option<&AgentRunProjection> {
        self.runs.get(id)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut AgentRunProjection> {
        self.runs.get_mut(id)
    }

    fn record_communication(&mut self, event: &astra_turn_types::AgentCommunicationEvent) -> bool {
        let key = self
            .order
            .iter()
            .find(|key| {
                self.runs.get(*key).is_some_and(|projection| {
                    projection.run_id.as_deref() == Some(event.observed_by.run_id.as_str())
                })
            })
            .cloned()
            .or_else(|| {
                self.runs
                    .contains_key(&event.observed_by.agent_id)
                    .then(|| event.observed_by.agent_id.clone())
            });
        let Some(projection) = key.and_then(|key| self.runs.get_mut(&key)) else {
            return false;
        };
        match event.direction {
            astra_turn_types::AgentCommunicationDirection::Sent => {
                projection.messages_sent = projection.messages_sent.saturating_add(1);
            }
            astra_turn_types::AgentCommunicationDirection::Received => {
                projection.messages_received = projection.messages_received.saturating_add(1);
            }
        }
        true
    }

    fn fanout_membership(
        &self,
        id: &str,
    ) -> Option<&crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership> {
        self.fanout_membership.get(id)
    }

    fn set_fanout_membership(
        &mut self,
        id: &str,
        fanout: Option<crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership>,
    ) {
        if let Some(fanout) = fanout {
            self.fanout_membership.insert(id.to_string(), fanout);
        }
    }

    fn reconcile_fanout_membership(
        &mut self,
        id: &str,
        fanout: Option<crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership>,
    ) {
        match fanout {
            Some(fanout) => {
                self.fanout_membership.insert(id.to_string(), fanout);
            }
            None => {
                self.fanout_membership.remove(id);
            }
        }
    }

    fn contains_key(&self, id: &str) -> bool {
        self.runs.contains_key(id)
    }

    /// Fetch the registry key bound to `tool_use_id`, if any.
    fn key_for_tool_use(&self, tool_use_id: &str) -> Option<&str> {
        self.control_bindings
            .get(tool_use_id)
            .map(|binding| binding.run_key.as_str())
    }

    /// A profile can own several concurrent or retried executions. Live
    /// streams therefore resolve by their immutable execution id, never by
    /// their display/profile id.
    fn key_for_run_id(&self, run_id: &str) -> Option<&str> {
        self.order.iter().find_map(|key| {
            self.runs
                .get(key)
                .is_some_and(|projection| projection.run_id.as_deref() == Some(run_id))
                .then_some(key.as_str())
        })
    }

    fn key_for_live_event(&self, agent_id: &str, run_id: &str) -> String {
        if let Some(key) = self.key_for_run_id(run_id) {
            return key.to_owned();
        }

        // Preserve an established local key when it is already bound to this
        // run. A second execution of the same profile gets its own run key,
        // rather than overwriting the first projection.
        match self.runs.get(agent_id) {
            None => agent_id.to_owned(),
            Some(projection)
                if projection.run_id.is_none() || projection.run_id.as_deref() == Some(run_id) =>
            {
                agent_id.to_owned()
            }
            Some(_) => run_id.to_owned(),
        }
    }

    fn action_for_tool_use(&self, tool_use_id: &str) -> Option<AgentControlAction> {
        self.control_bindings
            .get(tool_use_id)
            .map(|binding| binding.action)
    }

    fn tool_uses_for_key(&self, key: &str) -> Vec<String> {
        self.control_bindings
            .iter()
            .filter(|(_, binding)| binding.run_key == key)
            .map(|(tool_use_id, _)| tool_use_id.clone())
            .collect()
    }

    fn spawn_tool_use_for_key(&self, key: &str) -> Option<String> {
        self.control_bindings
            .iter()
            .find(|(_, binding)| {
                binding.run_key == key && binding.action == AgentControlAction::Spawn
            })
            .map(|(tool_use_id, _)| tool_use_id.clone())
    }

    fn ensure_for_tool_use(
        &mut self,
        id: String,
        label: String,
        state: AgentRunState,
        tool_use_id: &str,
        action: AgentControlAction,
    ) {
        self.ensure(id.clone(), label, state);
        self.bind_tool_use(tool_use_id, id, action);
    }

    fn bind_tool_use(&mut self, tool_use_id: &str, id: String, action: AgentControlAction) {
        self.control_bindings
            .entry(tool_use_id.to_string())
            .or_insert(AgentControlBinding {
                run_key: id,
                action,
            });
    }

    fn ensure(&mut self, id: String, label: String, state: AgentRunState) -> bool {
        if let Some(projection) = self.runs.get_mut(&id) {
            if state.source_rank() >= projection.state.source_rank()
                || projection.detail.description == projection.detail.tool_use_id
            {
                projection.detail.description = label;
            }
            return projection.set_state(state);
        }

        self.order.push(id.clone());
        self.runs
            .insert(id.clone(), AgentRunProjection::new(id, label, state));
        true
    }

    fn rename(&mut self, old: &str, new: String) {
        if old == new || !self.runs.contains_key(old) {
            return;
        }
        if let Some(projection) = self.runs.remove(old) {
            if let Some(existing) = self.runs.get_mut(&new) {
                merge_agent_projections(existing, projection);
            } else {
                self.runs.insert(new.clone(), projection);
            }
        }
        if let Some(fanout) = self.fanout_membership.remove(old) {
            self.fanout_membership.entry(new.clone()).or_insert(fanout);
        }
        for id in &mut self.order {
            if id == old {
                *id = new.clone();
            }
        }
        let mut seen = std::collections::HashSet::new();
        self.order.retain(|id| seen.insert(id.clone()));
        // Re-point any tool_use_id bindings from `old` to `new` so
        // follow-up events still resolve.
        for binding in self.control_bindings.values_mut() {
            if binding.run_key == old {
                binding.run_key = new.clone();
            }
        }
    }

    fn remove(&mut self, id: &str) {
        self.runs.remove(id);
        self.order.retain(|candidate| candidate != id);
        self.fanout_membership.remove(id);
        self.control_bindings
            .retain(|_, binding| binding.run_key != id);
    }

    fn mark_active_unconfirmed(&mut self) {
        for projection in self.runs.values_mut() {
            projection.mark_unconfirmed_if_active();
        }
    }

    fn signature(
        &self,
    ) -> (
        Vec<AgentRunSignature>,
        bool,
        crate::tui::server_agent_observer::ServerAgentTruthState,
    ) {
        (
            self.order
                .iter()
                .filter_map(|id| {
                    self.runs.get(id).map(|projection| AgentRunSignature {
                        id: id.clone(),
                        state: projection.state,
                        description: projection.detail.description.clone(),
                        reported_tool_calls: projection.reported_tool_calls,
                        reported_child_agents: projection.reported_child_agents,
                        messages_sent: projection.messages_sent,
                        messages_received: projection.messages_received,
                        run_id: projection.run_id.clone(),
                        parent_run_id: projection.parent_run_id.clone(),
                        depth: projection.depth,
                        metadata_source: projection.metadata_source,
                        runtime_facts: projection.runtime_facts.clone(),
                        runtime_sources: projection.runtime_sources,
                        output: agent_text_fingerprint(projection.detail.output_summary.as_deref()),
                        error: agent_text_fingerprint(projection.detail.error.as_deref()),
                        fanout: self.fanout_membership.get(id).cloned(),
                        control_target: projection.control_target.clone(),
                        transcript_target: projection.transcript_target,
                        available_actions: projection.available_actions.clone(),
                        durable_event_high_watermark: projection.durable_event_high_watermark,
                        attention: agent_text_fingerprint(projection.attention_summary.as_deref()),
                    })
                })
                .collect(),
            self.durable_snapshot_truncated,
            self.server_truth_state,
        )
    }
}

fn merge_agent_projections(target: &mut AgentRunProjection, source: AgentRunProjection) {
    let source_state = source.state;
    let source_state_observed_at = source.state_observed_at;
    let source_terminal_at = source.terminal_at;
    let source_reported_tool_calls = source.reported_tool_calls;
    let source_reported_child_agents = source.reported_child_agents;
    let source_messages_sent = source.messages_sent;
    let source_messages_received = source.messages_received;
    let source_run_id = source.run_id.clone();
    let source_parent_run_id = source.parent_run_id.clone();
    let source_depth = source.depth;
    let source_metadata_source = source.metadata_source;
    let source_runtime_facts = source.runtime_facts;
    let source_runtime_sources = source.runtime_sources;
    let source_control_target = source.control_target;
    let source_available_actions = source.available_actions;
    let source_control_source = source.control_source;
    let source_transcript_target = source.transcript_target;
    let source_transcript_source = source.transcript_source;
    let source_durable_event_high_watermark = source.durable_event_high_watermark;
    let source_attention_summary = source.attention_summary;
    let source_is_stronger = source_state.source_rank() > target.state.source_rank()
        || (!target.state.status.is_terminal() && source_state.status.is_terminal())
        || (matches!(
            target.state.confidence,
            AgentProjectionConfidence::Unconfirmed
        ) && !matches!(
            source_state.confidence,
            AgentProjectionConfidence::Unconfirmed
        ));
    let accept_lifecycle = should_accept_agent_state(target.state, source_state)
        && (source_is_stronger || source_state_observed_at > target.state_observed_at);
    let accept_content = accept_lifecycle || target.state.status == source_state.status;
    merge_agent_task_cells(
        target.detail.as_mut(),
        *source.detail,
        accept_lifecycle,
        accept_content,
    );
    target.reported_tool_calls = target.reported_tool_calls.max(source_reported_tool_calls);
    target.messages_sent = target.messages_sent.max(source_messages_sent);
    target.messages_received = target.messages_received.max(source_messages_received);

    if let (Some(metadata_source), Some(run_id)) = (source_metadata_source, source_run_id) {
        target.set_runtime_metadata(
            metadata_source,
            run_id,
            source_parent_run_id,
            source_depth,
            source_reported_child_agents,
        );
    }
    macro_rules! merge_runtime_fact_from_projection {
        ($field:ident) => {
            if let Some(runtime_source) = source_runtime_sources.$field {
                let mut facts = astra_thin_client::SessionRunRuntimeFacts::default();
                facts.$field = source_runtime_facts.$field.clone();
                target.set_runtime_facts(runtime_source, facts);
            }
        };
    }
    merge_runtime_fact_from_projection!(runtime_profile);
    merge_runtime_fact_from_projection!(offering_id);
    merge_runtime_fact_from_projection!(model_name);
    merge_runtime_fact_from_projection!(agent_binding_id);
    merge_runtime_fact_from_projection!(agent_binding_name);
    merge_runtime_fact_from_projection!(agent_binding_schema_version);
    merge_runtime_fact_from_projection!(capability_server_refs);
    merge_runtime_fact_from_projection!(background);
    merge_runtime_fact_from_projection!(permission);

    if let (Some(control_source), Some(control_target)) =
        (source_control_source, source_control_target)
    {
        target.set_controls(control_source, control_target, source_available_actions);
    }
    if let (Some(transcript_source), Some(transcript_target)) =
        (source_transcript_source, source_transcript_target)
    {
        target.set_transcript_target(transcript_source, transcript_target);
    }
    target.durable_event_high_watermark = target
        .durable_event_high_watermark
        .max(source_durable_event_high_watermark);

    if accept_lifecycle {
        target.state = source_state;
        target.state_observed_at = source_state_observed_at;
        target.terminal_at = source_terminal_at;
    }
    if accept_content {
        target.attention_summary = source_attention_summary;
    }
    target.sync_detail_status();
}

fn merge_agent_task_cells(
    target: &mut TaskCell,
    source: crate::tui::history_cell::task::TaskCell,
    accept_lifecycle: bool,
    accept_content: bool,
) {
    use crate::tui::history_cell::task::ChildStatus;

    let crate::tui::history_cell::task::TaskCell {
        tool_use_id,
        description,
        status,
        started_at,
        completed_at,
        duration_ms,
        output_summary,
        children,
        error,
        ctrl_b_background_hint,
    } = source;

    if started_at < target.started_at {
        target.started_at = started_at;
    }
    if target.description == target.tool_use_id && description != tool_use_id {
        target.description = description;
    }
    if accept_content && target.error.is_none() {
        target.error = error;
    }
    target.ctrl_b_background_hint |= ctrl_b_background_hint;
    if accept_content
        && target
            .output_summary
            .as_ref()
            .map(|summary| summary.trim().is_empty())
            .unwrap_or(true)
    {
        if let Some(summary) = output_summary.filter(|summary| !summary.trim().is_empty()) {
            target.output_summary = Some(summary);
        }
    }
    for child in children {
        if let Some(existing) = target
            .children
            .iter_mut()
            .find(|existing| existing.tool_use_id == child.tool_use_id)
        {
            if matches!(existing.status, ChildStatus::Running)
                && !matches!(child.status, ChildStatus::Running)
            {
                existing.status = child.status;
                existing.duration_ms = child.duration_ms;
            } else if existing.duration_ms.is_none() {
                existing.duration_ms = child.duration_ms;
            }
        } else {
            target.children.push(child);
        }
    }
    if accept_lifecycle && !target.status.is_terminal() && status.is_terminal() {
        target.status = status;
        target.completed_at = completed_at;
        target.duration_ms = duration_ms;
    } else if accept_lifecycle && target.completed_at.is_none() {
        target.completed_at = completed_at;
        if target.duration_ms.is_none() {
            target.duration_ms = duration_ms;
        }
    }
}

fn agent_fanout_membership(
    slot: astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity,
    group_title: Option<&str>,
    slot_label: &str,
) -> crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership {
    let group_id = slot.group_id;
    let group_title = group_title
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .unwrap_or(&group_id)
        .to_string();
    crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership {
        group_title,
        group_id,
        target_count: slot.target_count,
        slot_index: slot.slot_index,
        slot_label: slot_label.to_string(),
    }
}

fn agent_run_state_from_fanout_receipt(status: &str) -> Option<AgentRunStatus> {
    match status {
        "launched" | "running" => Some(AgentRunStatus::Running),
        "waiting" => Some(AgentRunStatus::Waiting),
        "completed" => Some(AgentRunStatus::Completed),
        "interrupted" => Some(AgentRunStatus::Interrupted),
        "cancelled" => Some(AgentRunStatus::Cancelled),
        "failed" => Some(AgentRunStatus::Failed),
        _ => None,
    }
}

fn transcript_target_from_wire(
    value: Option<&str>,
) -> Option<crate::tui::agent_run_projection::AgentTranscriptTarget> {
    match value {
        Some("local_journal") => {
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal)
        }
        Some("durable_server") => {
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer)
        }
        _ => None,
    }
}

/// UI-side evidence check for a completed fanout control action. A transport
/// error may have non-empty display text, but only a typed receipt identifies
/// the group that the user can inspect or control.
fn fanout_completion_has_receipt(output_summary: Option<&str>, output: Option<&str>) -> bool {
    [output, output_summary].into_iter().flatten().any(|text| {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .is_some_and(|receipt| {
                receipt
                    .get("group_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|group_id| !group_id.trim().is_empty())
                    && receipt
                        .get("status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|status| !status.trim().is_empty())
            })
    })
}

/// Project a typed fanout admission failure into an actionable user-facing
/// summary. The raw tool payload remains protocol evidence upstream; dumping
/// it into the transcript makes a simple rejected launch look like a broken
/// JSON document and obscures the only user-relevant fact: no child ran.
fn fanout_rejection_summary(output_summary: Option<&str>, output: Option<&str>) -> Option<String> {
    [output, output_summary].into_iter().flatten().find_map(|text| {
        let payload = serde_json::from_str::<serde_json::Value>(text).ok()?;
        (payload.get("status").and_then(serde_json::Value::as_str) == Some("failed"))
            .then_some(())?;
        (payload.get("error_kind").and_then(serde_json::Value::as_str)
            == Some("tool_invalid_args"))
            .then_some(())?;
        let next_step = payload
            .get("advisory")
            .and_then(|advisory| advisory.get("next_step"))
            .and_then(serde_json::Value::as_str)
            .filter(|step| !step.trim().is_empty())
            .unwrap_or("Submit one complete request matching the advertised agent_fanout schema.");
        Some(format!(
            "Fanout did not start · its arguments were invalid, so no agents were launched. {next_step}"
        ))
    })
}

fn local_agent_run_status(
    status: &astra_turn_core::orchestration_types::AgentStatus,
) -> AgentRunStatus {
    use astra_turn_core::orchestration_types::AgentStatus;
    match status {
        AgentStatus::Initializing => AgentRunStatus::Starting,
        AgentStatus::Running { .. } => AgentRunStatus::Running,
        AgentStatus::Idle | AgentStatus::Waiting { .. } => AgentRunStatus::Waiting,
        AgentStatus::Completed { .. } => AgentRunStatus::Completed,
        AgentStatus::Interrupted { .. } => AgentRunStatus::Interrupted,
        AgentStatus::Failed { .. } => AgentRunStatus::Failed,
        AgentStatus::Cancelled { .. } => AgentRunStatus::Cancelled,
    }
}

fn local_agent_runtime_metadata(
    agents: &[astra_turn_core::orchestration_types::SpawnedAgentInfo],
) -> std::collections::HashMap<String, (u32, usize)> {
    let by_run_id = agents
        .iter()
        .map(|agent| (agent.run_id.as_str(), agent))
        .collect::<std::collections::HashMap<_, _>>();
    let child_counts = agents.iter().fold(
        std::collections::HashMap::<&str, usize>::new(),
        |mut counts, agent| {
            *counts.entry(agent.parent_run_id.as_str()).or_default() += 1;
            counts
        },
    );

    agents
        .iter()
        .map(|agent| {
            let mut depth = 1_u32;
            let mut parent_run_id = agent.parent_run_id.as_str();
            let mut seen = std::collections::HashSet::from([agent.run_id.as_str()]);
            while depth < 64 {
                let Some(parent) = by_run_id.get(parent_run_id) else {
                    break;
                };
                if !seen.insert(parent.run_id.as_str()) {
                    break;
                }
                depth += 1;
                parent_run_id = parent.parent_run_id.as_str();
            }
            (
                agent.agent_id.clone(),
                (
                    depth,
                    child_counts
                        .get(agent.run_id.as_str())
                        .copied()
                        .unwrap_or(0),
                ),
            )
        })
        .collect()
}

fn append_agent_lineage_subtree(
    index: usize,
    depth: u32,
    rows: &[crate::tui::bottom_pane::in_flight_agents_view::AgentRow],
    children: &std::collections::HashMap<usize, Vec<usize>>,
    visited: &mut std::collections::HashSet<usize>,
    ordered: &mut Vec<crate::tui::bottom_pane::in_flight_agents_view::AgentRow>,
) {
    let mut pending = vec![(index, depth)];
    while let Some((index, depth)) = pending.pop() {
        if !visited.insert(index) {
            continue;
        }
        let mut row = rows[index].clone();
        row.depth = depth.max(1);
        ordered.push(row);
        if let Some(child_indices) = children.get(&index) {
            for child_index in child_indices.iter().rev() {
                pending.push((*child_index, depth.saturating_add(1)));
            }
        }
    }
}

/// Converts canonical run lineage into a visible forest. A child whose parent
/// was omitted by terminal filtering or a bounded server snapshot becomes a
/// visible root instead of being rendered with a misleading orphan indent.
fn order_agent_monitor_rows_by_lineage(
    rows: Vec<crate::tui::bottom_pane::in_flight_agents_view::AgentRow>,
) -> Vec<crate::tui::bottom_pane::in_flight_agents_view::AgentRow> {
    let run_indices = rows
        .iter()
        .enumerate()
        .filter_map(|(index, row)| row.run_id.as_deref().map(|run_id| (run_id, index)))
        .fold(
            std::collections::HashMap::<&str, usize>::new(),
            |mut indices, (run_id, index)| {
                indices.entry(run_id).or_insert(index);
                indices
            },
        );
    let mut children = std::collections::HashMap::<usize, Vec<usize>>::new();
    let mut roots = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let parent_index = row
            .parent_run_id
            .as_deref()
            .and_then(|parent_run_id| run_indices.get(parent_run_id).copied())
            .filter(|parent_index| *parent_index != index);
        if let Some(parent_index) = parent_index {
            children.entry(parent_index).or_default().push(index);
        } else {
            roots.push(index);
        }
    }

    let mut visited = std::collections::HashSet::with_capacity(rows.len());
    let mut ordered = Vec::with_capacity(rows.len());
    for root in roots {
        append_agent_lineage_subtree(root, 1, &rows, &children, &mut visited, &mut ordered);
    }
    // Malformed cycles have no root. Preserve every row as a visible root and
    // let the visited set break the cycle rather than dropping monitor data.
    for index in 0..rows.len() {
        append_agent_lineage_subtree(index, 1, &rows, &children, &mut visited, &mut ordered);
    }
    ordered
}

fn local_agent_available_actions(
    status: AgentRunStatus,
) -> Vec<astra_thin_client::SessionRunAction> {
    if status.is_active() && status != AgentRunStatus::Cancelling {
        vec![astra_thin_client::SessionRunAction::Cancel]
    } else {
        Vec::new()
    }
}

fn server_agent_run_status(status: astra_thin_client::SessionRunLifecycleStatus) -> AgentRunStatus {
    use astra_thin_client::SessionRunLifecycleStatus;
    match status {
        SessionRunLifecycleStatus::Running => AgentRunStatus::Running,
        SessionRunLifecycleStatus::Waiting => AgentRunStatus::Waiting,
        SessionRunLifecycleStatus::Paused => AgentRunStatus::Paused,
        SessionRunLifecycleStatus::Completed => AgentRunStatus::Completed,
        SessionRunLifecycleStatus::Delegated => AgentRunStatus::Delegated,
        SessionRunLifecycleStatus::Interrupted => AgentRunStatus::Interrupted,
        SessionRunLifecycleStatus::Failed => AgentRunStatus::Failed,
        SessionRunLifecycleStatus::Cancelled => AgentRunStatus::Cancelled,
    }
}

fn parse_run_timestamp(value: &str) -> Option<std::time::SystemTime> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&chrono::Utc).into())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f")
                .map(|value| value.and_utc().into())
        })
        .ok()
}

fn apply_server_agent_detail(
    projection: &mut AgentRunProjection,
    node: &astra_thin_client::SessionRunNode,
    status: AgentRunStatus,
) {
    projection.detail.error = if status == AgentRunStatus::Failed {
        node.error_message
            .clone()
            .or_else(|| node.error_code.clone())
    } else {
        None
    };
    if matches!(status, AgentRunStatus::Waiting | AgentRunStatus::Paused) {
        let attention = node.waiting_for.as_deref().map(|reason| {
            let reason = reason.replace('_', " ");
            match status {
                AgentRunStatus::Waiting => format!("Waiting for {reason}"),
                AgentRunStatus::Paused => format!("Paused · {reason}"),
                _ => unreachable!(),
            }
        });
        projection.detail.output_summary = attention.clone();
        projection.set_attention_summary(attention);
    }
}

fn restored_agent_run_status(status: &str) -> Option<AgentRunStatus> {
    match status {
        "pending" => Some(AgentRunStatus::Starting),
        "running" => Some(AgentRunStatus::Running),
        "waiting_for_input" => Some(AgentRunStatus::Waiting),
        "completed" => Some(AgentRunStatus::Completed),
        "interrupted" => Some(AgentRunStatus::Interrupted),
        "failed" => Some(AgentRunStatus::Failed),
        "cancelled" => Some(AgentRunStatus::Cancelled),
        "killed" => Some(AgentRunStatus::Cancelled),
        _ => None,
    }
}

fn restored_agent_depth(
    agent: &astra_services::session_workspace::BackgroundLocalAgentTaskProjection,
    restored: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
) -> u32 {
    let parents = restored
        .iter()
        .map(|candidate| (candidate.run_id.as_str(), candidate.parent_run_id.as_str()))
        .collect::<std::collections::HashMap<_, _>>();
    let mut depth = 1_u32;
    let mut parent = agent.parent_run_id.as_str();
    let mut visited = std::collections::HashSet::new();
    while !parent.is_empty() && parent != astra_runtime::orchestration::ROOT_RUN_ID {
        if !visited.insert(parent) {
            break;
        }
        let Some(next) = parents.get(parent).copied() else {
            break;
        };
        depth = depth.saturating_add(1);
        parent = next;
    }
    depth
}

fn align_projection_start_time(
    projection: &mut AgentRunProjection,
    started_at: std::time::SystemTime,
) {
    let Ok(elapsed) = started_at.elapsed() else {
        return;
    };
    if let Some(inferred) = std::time::Instant::now().checked_sub(elapsed)
        && inferred < projection.detail.started_at
    {
        projection.detail.started_at = inferred;
    }
}

fn align_projection_start_epoch_ms(projection: &mut AgentRunProjection, started_at_ms: u64) {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(started_at_ms);
    let elapsed = std::time::Duration::from_millis(now_ms.saturating_sub(started_at_ms));
    if let Some(inferred) = std::time::Instant::now().checked_sub(elapsed)
        && inferred < projection.detail.started_at
    {
        projection.detail.started_at = inferred;
    }
}

fn align_projection_end_time(
    projection: &mut AgentRunProjection,
    started_at: std::time::SystemTime,
    ended_at: Option<std::time::SystemTime>,
) {
    let Some(ended_at) = ended_at else {
        return;
    };
    if let Ok(duration) = ended_at.duration_since(started_at) {
        projection.detail.duration_ms = Some(duration.as_millis() as u64);
    }
    let elapsed_since_end = ended_at.elapsed().unwrap_or_default();
    let completed_at = std::time::Instant::now()
        .checked_sub(elapsed_since_end)
        .unwrap_or_else(std::time::Instant::now);
    projection.detail.completed_at = Some(completed_at);
    projection.terminal_at = Some(completed_at);
}

fn apply_local_agent_status(
    projection: &mut AgentRunProjection,
    status: &astra_turn_core::orchestration_types::AgentStatus,
) {
    use astra_turn_core::orchestration_types::AgentStatus;

    let elapsed_ms = projection.detail.started_at.elapsed().as_millis() as u64;
    match status {
        AgentStatus::Initializing => {}
        AgentStatus::Running { activity } => {
            if projection.detail.output_summary.is_none() && !activity.trim().is_empty() {
                projection.detail.output_summary = Some(activity.clone());
            }
        }
        AgentStatus::Idle => {
            if projection.detail.output_summary.is_none() {
                projection.detail.output_summary = Some("Agent is waiting for input.".into());
            }
            projection.set_attention_summary(Some("Waiting for input".into()));
        }
        AgentStatus::Waiting { reason } => {
            if projection.detail.output_summary.is_none() && !reason.trim().is_empty() {
                projection.detail.output_summary = Some(reason.clone());
            }
            projection.set_attention_summary((!reason.trim().is_empty()).then(|| reason.clone()));
        }
        AgentStatus::Completed { result, .. } => {
            projection
                .detail
                .completed_at
                .get_or_insert_with(std::time::Instant::now);
            projection.detail.duration_ms = Some(elapsed_ms);
            if !result.trim().is_empty() {
                projection.detail.output_summary = Some(result.clone());
            }
            projection.detail.error = None;
        }
        AgentStatus::Interrupted {
            partial_result,
            finish_reason,
        } => {
            projection
                .detail
                .completed_at
                .get_or_insert_with(std::time::Instant::now);
            projection.detail.duration_ms = Some(elapsed_ms);
            if !partial_result.trim().is_empty() {
                projection.detail.output_summary = Some(partial_result.clone());
            }
            let interruption_kind =
                astra_turn_core::interruption::InterruptionKind::from_label(finish_reason);
            projection.detail.error = Some(interruption_kind.map_or_else(
                || "The agent stopped before completing its result.".to_string(),
                |kind| kind.user_description().to_string(),
            ));
            projection.set_attention_summary(Some(interruption_kind.map_or_else(
                || "Needs continuation".to_string(),
                |kind| kind.user_status().to_string(),
            )));
        }
        AgentStatus::Failed { error, .. } => {
            projection
                .detail
                .completed_at
                .get_or_insert_with(std::time::Instant::now);
            projection.detail.duration_ms = Some(elapsed_ms);
            projection.detail.error = Some(error.clone());
        }
        AgentStatus::Cancelled { reason, .. } => {
            projection
                .detail
                .completed_at
                .get_or_insert_with(std::time::Instant::now);
            projection.detail.duration_ms = Some(elapsed_ms);
            if !reason.trim().is_empty() {
                projection.detail.output_summary = Some(reason.clone());
            }
            projection.detail.error = None;
        }
    }
}

enum AgentLiveMirror {
    Started {
        tool_use_id: String,
        name: String,
        description: String,
    },
    Completed {
        tool_use_id: String,
        status: String,
        duration_ms: u64,
    },
}

/// `live_tasks` is the multi-slot register for **parallel TaskCells**
/// (sub-agents spawned via the agent spawn action in a single turn). Each
/// keyed by its `tool_use_id`. Children events route by
/// `parent_tool_use_id` and mutate the matching live cell directly,
/// so spawning agent B no longer commits agent A to scrollback —
/// both stay live and continue to receive child events. On
/// terminal completion (`ToolCompleted` for the parent), the cell
/// finalises and moves to `history` with no disruption to its
/// siblings.
///
/// **Insertion order matters**: `LiveTaskOrder` records the order
/// agents were spawned so the renderer can show them deterministically
/// (oldest-first) regardless of HashMap iteration order.
enum DeferredStreamEvent {
    AnswerDelta(String),
    ReasoningDelta(String),
    ReasoningDone,
}

pub(crate) struct ChatWidget {
    session_id: String,
    history: Vec<Arc<dyn HistoryCell>>,
    /// UI projection identities parallel to `history`. IDs are allocated when
    /// a cell is created, not when it is committed, so a live cell keeps the
    /// same identity across mid-turn user insertions and live -> settled.
    history_cell_ids: Vec<u64>,
    active_cell: Option<Box<dyn HistoryCell>>,
    active_cell_id: Option<u64>,
    /// Identity of the non-Task ToolCell in `active_cell`. Tool completion
    /// must match this id; a late completion for some other tool must never
    /// finalize the currently visible command by name or position alone.
    active_tool_use_id: Option<String>,
    /// Providers may emit answer/reasoning tokens before the preceding tool's
    /// terminal event. Keep those events ordered until the tool receipt lands
    /// so the transcript does not manufacture a failed tool and later append
    /// a duplicate successful one.
    deferred_stream_events: Vec<DeferredStreamEvent>,
    next_cell_id: u64,
    /// Live parallel TaskCells, keyed by their `tool_use_id`.
    /// Mutating directly (no `Arc`) so child events can attach.
    live_tasks: std::collections::HashMap<String, Box<TaskCell>>,
    /// Spawn-order of `live_tasks` keys, for deterministic rendering.
    /// Pruned when a key transitions to terminal status.
    live_task_order: Vec<String>,
    /// Logical background agents keyed by their returned `agent_id`.
    ///
    /// the agent spawn/get_result actions are control-plane tool calls. They
    /// should not be the thing Ctrl+G drills into; users expect the actual
    /// child agent. This registry is populated from those tool outputs and is
    /// the canonical source for the multi-agent strip/drilldown.
    agent_runs: AgentRunRegistry,
    /// Immutable run identities already known when an `agent_fanout` starts.
    /// A missing receipt can later be annotated only from newly observed
    /// canonical run identities, never from display text or a mutable active
    /// count. The evidence does not prove the tool receipt succeeded, so it
    /// produces an explicit uncertain outcome rather than a false success.
    fanout_launch_baselines: std::collections::HashMap<String, HashSet<String>>,
    /// Index into `history` marking cells that have already been
    /// flushed to the terminal scrollback. `drain_new_committed`
    /// returns everything past this index and advances it.
    committed_watermark: usize,
    /// `tool_use_id`s of TaskCells spawned in the current turn that
    /// have not yet reached a terminal state. Ctrl+C on the parent
    /// turn cascades cancel to every id in this set; an individual
    /// TaskCell's Esc handler removes just its own id. Cleared at
    /// turn boundaries.
    in_flight_task_ids: Vec<String>,
    /// Control tool ids that have received a local cancel request but have
    /// not yet delivered their terminal event. Logical Agent cancellation is
    /// tracked in `AgentRunProjection::state`.
    cancelling_task_ids: std::collections::HashSet<String>,
    /// Current-turn UI capability for foreground bash promotion.
    /// Enabled by the interactive event loop only after it installs a
    /// detach handle that Ctrl+B can signal.
    bash_background_hint_enabled: bool,
}

impl ChatWidget {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            history: Vec::new(),
            history_cell_ids: Vec::new(),
            active_cell: None,
            active_cell_id: None,
            active_tool_use_id: None,
            deferred_stream_events: Vec::new(),
            next_cell_id: 1,
            live_tasks: std::collections::HashMap::new(),
            live_task_order: Vec::new(),
            agent_runs: AgentRunRegistry::default(),
            fanout_launch_baselines: std::collections::HashMap::new(),
            committed_watermark: 0,
            in_flight_task_ids: Vec::new(),
            cancelling_task_ids: std::collections::HashSet::new(),
            bash_background_hint_enabled: false,
        }
    }

    pub fn set_bash_background_hint_enabled(&mut self, enabled: bool) {
        self.bash_background_hint_enabled = enabled;
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tool) = cell.as_any_mut().downcast_mut::<ToolCell>()
            && tool.name == "bash"
        {
            tool.set_ctrl_b_background_hint(enabled);
        }
    }

    /// IDs of currently-live parallel TaskCells in spawn order.
    /// Used by the renderer (multi-agent view) and by selection UIs.
    pub fn live_task_ids(&self) -> Vec<String> {
        self.live_task_order.clone()
    }

    /// Borrow a live TaskCell by id. `None` once the cell has
    /// completed and moved to history.
    pub fn live_task_cell(&self, id: &str) -> Option<&TaskCell> {
        self.live_tasks.get(id).map(|b| b.as_ref())
    }

    /// Logical agent rows currently known to the session. Unlike
    /// `live_task_ids`, these are child agent IDs returned by the spawn action,
    /// not transient control-plane tool call IDs such as `get_result`.
    pub fn agent_run_ids(&self) -> Vec<String> {
        self.agent_runs.ids()
    }

    pub(crate) fn agent_status_strip_ids(&self) -> Vec<String> {
        self.agent_runs.status_strip_ids()
    }

    pub fn agent_run_cell(&self, id: &str) -> Option<&TaskCell> {
        self.agent_runs
            .get(id)
            .map(|projection| projection.detail.as_ref())
    }

    pub(crate) fn agent_run_state(&self, id: &str) -> Option<AgentRunState> {
        self.agent_runs.get(id).map(|projection| projection.state)
    }

    pub(crate) fn agent_run_activity_counts(
        &self,
        id: &str,
    ) -> Option<crate::tui::agent_run_projection::AgentActivityCounts> {
        self.agent_runs.get(id).map(|projection| {
            projection.activity_counts(self.agent_runs.durable_snapshot_truncated)
        })
    }

    pub(crate) fn agent_run_terminal_at(&self, id: &str) -> Option<std::time::Instant> {
        self.agent_runs
            .get(id)
            .and_then(|projection| projection.terminal_at)
    }

    pub(crate) fn reconcile_local_agent_snapshot(
        &mut self,
        snapshot: &crate::tui::local_agent_snapshot::LocalAgentSnapshot,
        restored: &[astra_services::session_workspace::BackgroundLocalAgentTaskProjection],
    ) -> bool {
        let before = self.agent_runs.signature();
        let fanout_titles = snapshot.fanout_titles();
        let local_runtime_metadata = local_agent_runtime_metadata(&snapshot.agents);
        let mut present = std::collections::HashSet::new();

        if snapshot.available {
            for agent in &snapshot.agents {
                if let Some(tool_use_id) = agent.spawn_tool_call_id.as_deref()
                    && let Some(provisional_key) = self
                        .agent_runs
                        .key_for_tool_use(tool_use_id)
                        .map(str::to_string)
                    && provisional_key != agent.agent_id
                {
                    self.agent_runs
                        .rename(&provisional_key, agent.agent_id.clone());
                }
                present.insert(agent.agent_id.clone());
                let status = local_agent_run_status(&agent.status);
                let state = AgentRunState::confirmed_local(status);
                let label = if agent.description.trim().is_empty() {
                    agent_display_name(&agent.agent_id, Some(&agent.agent_type))
                } else {
                    agent.description.clone()
                };
                let accepted = self.agent_runs.ensure(agent.agent_id.clone(), label, state);
                if let Some(projection) = self.agent_runs.get_mut(&agent.agent_id) {
                    let (depth, child_agents) = local_runtime_metadata
                        .get(agent.agent_id.as_str())
                        .copied()
                        .unwrap_or((1, 0));
                    projection.set_runtime_metadata(
                        AgentProjectionSource::LocalRuntime,
                        agent.run_id.clone(),
                        (!agent.parent_run_id.trim().is_empty())
                            .then(|| agent.parent_run_id.clone()),
                        depth,
                        child_agents,
                    );
                    projection.set_runtime_facts(
                        AgentProjectionSource::LocalRuntime,
                        astra_thin_client::SessionRunRuntimeFacts {
                            runtime_profile: Some("cli_local".into()),
                            agent_binding_name: Some(agent.agent_type.clone()),
                            background: Some(agent.run_in_background),
                            permission: Some(astra_thin_client::SessionRunPermissionFacts {
                                has_issues: agent.has_permission_issues,
                                requests: agent.metrics.permission_requests,
                                approved: agent.metrics.permission_requests_approved,
                                tools_blocked: agent.metrics.tools_blocked,
                            }),
                            ..Default::default()
                        },
                    );
                    projection.set_controls(
                        AgentProjectionSource::LocalRuntime,
                        crate::tui::agent_run_projection::AgentControlTarget::LocalAgent {
                            agent_id: agent.agent_id.clone(),
                        },
                        local_agent_available_actions(status),
                    );
                    projection.set_transcript_target(
                        AgentProjectionSource::LocalRuntime,
                        crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
                    );
                    if accepted {
                        projection.reported_tool_calls = agent.metrics.tool_calls as usize;
                        align_projection_start_time(projection, agent.started_at);
                        apply_local_agent_status(projection, &agent.status);
                        align_projection_end_time(projection, agent.started_at, agent.ended_at);
                    }
                }
                if accepted {
                    let fanout = agent.fanout_slot.clone().map(|slot| {
                        let title = fanout_titles.get(&slot.group_id).map(String::as_str);
                        agent_fanout_membership(slot, title, &agent.description)
                    });
                    self.agent_runs
                        .reconcile_fanout_membership(&agent.agent_id, fanout);
                }
            }

            for (id, projection) in &mut self.agent_runs.runs {
                if projection.state.source == AgentProjectionSource::LocalRuntime
                    && !present.contains(id)
                {
                    projection.mark_stale_if_active();
                }
            }
        }

        for restored_agent in restored {
            if self.agent_runs.contains_key(&restored_agent.id) {
                continue;
            }
            let Some(status) = restored_agent_run_status(&restored_agent.status) else {
                continue;
            };
            self.agent_runs.ensure(
                restored_agent.id.clone(),
                restored_agent.title.clone(),
                AgentRunState::stale_workspace(status),
            );
            if let Some(projection) = self.agent_runs.get_mut(&restored_agent.id) {
                projection.set_runtime_metadata(
                    AgentProjectionSource::WorkspaceSnapshot,
                    restored_agent.run_id.clone(),
                    Some(restored_agent.parent_run_id.clone()),
                    restored_agent_depth(restored_agent, restored),
                    0,
                );
                projection.set_transcript_target(
                    AgentProjectionSource::WorkspaceSnapshot,
                    crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
                );
                align_projection_start_epoch_ms(projection, restored_agent.started_at_ms);
                projection.detail.output_summary = restored_agent.output_tail.clone();
                if matches!(status, AgentRunStatus::Failed | AgentRunStatus::Interrupted) {
                    projection.detail.error = restored_agent.terminal_reason.clone();
                }
            }
            self.agent_runs.set_fanout_membership(
                &restored_agent.id,
                restored_agent.fanout.as_ref().map(|fanout| {
                    crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership {
                        group_id: fanout.group_id.clone(),
                        group_title: fanout.group_title.clone(),
                        target_count: fanout.target_count,
                        slot_index: fanout.slot_index,
                        slot_label: fanout.slot_label.clone(),
                    }
                }),
            );
        }

        before != self.agent_runs.signature()
    }

    /// Recover terminal local-agent rows from the canonical session journal.
    ///
    /// The dynamic spawner is an execution cache, not the ownership boundary
    /// for a completed conversation. Once a CLI restarts (or an old agent
    /// leaves the bounded in-memory archive), this path keeps `/agent` able to
    /// open the exact local transcript by its immutable run id.
    pub(crate) fn reconcile_local_agent_journal_runs(
        &mut self,
        runs: &[crate::tui::local_agent_journal::LocalJournalAgentRun],
    ) -> bool {
        let before = self.agent_runs.signature();
        for run in runs {
            let Some(status) = restored_agent_run_status(&run.status) else {
                continue;
            };
            let run_key = self
                .agent_runs
                .key_for_run_id(&run.run_id)
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    self.agent_runs
                        .key_for_live_event(&run.agent_id, &run.run_id)
                });
            let label = if run.description.trim().is_empty() {
                agent_display_name(&run.agent_id, None)
            } else {
                run.description.clone()
            };
            let accepted = self.agent_runs.ensure(
                run_key.clone(),
                label,
                AgentRunState::confirmed_local_journal(status),
            );
            let Some(projection) = self.agent_runs.get_mut(&run_key) else {
                continue;
            };
            projection.set_runtime_metadata(
                AgentProjectionSource::LocalJournal,
                run.run_id.clone(),
                run.parent_run_id.clone(),
                1,
                0,
            );
            projection.set_transcript_target(
                AgentProjectionSource::LocalJournal,
                crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal,
            );
            if accepted {
                projection.reported_tool_calls = run.tool_calls;
                projection.detail.duration_ms = Some(run.duration_ms);
                projection.detail.completed_at = Some(std::time::Instant::now());
            }
        }
        self.agent_runs
            .prune_terminal_history(crate::tui::local_agent_journal::RECENT_TERMINAL_RUN_LIMIT);
        before != self.agent_runs.signature()
    }

    pub(crate) fn reconcile_server_agent_projection(
        &mut self,
        server: &crate::tui::server_agent_observer::ServerAgentProjection,
    ) -> bool {
        use crate::tui::server_agent_observer::ServerAgentTruthState;

        let before = self.agent_runs.signature();
        self.agent_runs.server_truth_state = server.truth_state;
        match server.truth_state {
            ServerAgentTruthState::Stale => {
                for projection in self.agent_runs.runs.values_mut() {
                    if projection.state.source == AgentProjectionSource::DurableServer {
                        projection.mark_stale_if_active();
                    }
                }
            }
            ServerAgentTruthState::Confirmed => {
                if let Some(snapshot) = server.snapshot.as_ref() {
                    self.agent_runs.durable_snapshot_truncated = snapshot.truncated;
                    let child_counts = snapshot.runs.iter().fold(
                        std::collections::HashMap::<&str, usize>::new(),
                        |mut counts, node| {
                            if node.is_agent_run()
                                && let Some(parent_run_id) = node.parent_run_id.as_deref()
                            {
                                *counts.entry(parent_run_id).or_default() += 1;
                            }
                            counts
                        },
                    );
                    let mut present = std::collections::HashSet::new();
                    for node in snapshot.runs.iter().filter(|node| node.is_agent_run()) {
                        present.insert(node.run_id.as_str());
                        if self
                            .agent_runs
                            .get(&node.run_id)
                            .and_then(|projection| projection.durable_event_high_watermark)
                            .is_some_and(|watermark| watermark > node.run_event_high_watermark)
                        {
                            continue;
                        }

                        let status = server_agent_run_status(node.status);
                        let state = AgentRunState::confirmed_server(status);
                        let label = node
                            .agent_name
                            .as_deref()
                            .or(node.agent_id.as_deref())
                            .filter(|label| !label.trim().is_empty())
                            .map(str::to_string)
                            .unwrap_or_else(|| agent_display_name(&node.run_id, None));
                        let accepted = self.agent_runs.ensure(node.run_id.clone(), label, state);
                        let Some(projection) = self.agent_runs.get_mut(&node.run_id) else {
                            continue;
                        };
                        projection.set_controls(
                            AgentProjectionSource::DurableServer,
                            crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                                run_id: node.run_id.clone(),
                            },
                            node.available_actions.clone(),
                        );
                        projection.set_transcript_target(
                            AgentProjectionSource::DurableServer,
                            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer,
                        );
                        projection.set_runtime_facts(
                            AgentProjectionSource::DurableServer,
                            node.runtime.clone(),
                        );
                        if !accepted
                            && projection.durable_event_high_watermark
                                == Some(node.run_event_high_watermark)
                        {
                            continue;
                        }
                        projection.durable_event_high_watermark =
                            Some(node.run_event_high_watermark);
                        projection.set_runtime_metadata(
                            AgentProjectionSource::DurableServer,
                            node.run_id.clone(),
                            node.parent_run_id.clone(),
                            node.depth,
                            child_counts.get(node.run_id.as_str()).copied().unwrap_or(0),
                        );
                        projection.reported_tool_calls = node.total_tool_calls as usize;
                        if let Some(started_at) = parse_run_timestamp(&node.created_at) {
                            align_projection_start_time(projection, started_at);
                            if status.is_terminal() {
                                align_projection_end_time(
                                    projection,
                                    started_at,
                                    parse_run_timestamp(&node.updated_at),
                                );
                            }
                        }
                        apply_server_agent_detail(projection, node, status);
                    }

                    if !snapshot.truncated {
                        for (run_id, projection) in &mut self.agent_runs.runs {
                            if projection.state.source == AgentProjectionSource::DurableServer
                                && !present.contains(run_id.as_str())
                            {
                                projection.mark_stale_if_active();
                            }
                        }
                    }
                }
            }
            ServerAgentTruthState::Unbound
            | ServerAgentTruthState::Loading
            | ServerAgentTruthState::Unavailable => {}
        }

        self.agent_runs
            .prune_terminal_history(crate::tui::local_agent_journal::RECENT_TERMINAL_RUN_LIMIT);
        before != self.agent_runs.signature()
    }

    pub(crate) fn reset_agent_scope(&mut self) {
        self.agent_runs = AgentRunRegistry::default();
        self.fanout_launch_baselines.clear();
    }

    #[cfg(test)]
    pub(crate) fn set_agent_completed_at_for_test(
        &mut self,
        id: &str,
        completed_at: std::time::Instant,
    ) {
        if let Some(projection) = self.agent_runs.get_mut(id) {
            projection.detail.completed_at = Some(completed_at);
            projection.terminal_at = Some(completed_at);
        }
    }

    /// Look up a TaskCell by id in either the live register or in
    /// history. The agent transcript uses this only as a transient live
    /// fallback for activity that predates opening the transcript; canonical
    /// conversation is loaded separately. Live cells take priority because
    /// they have fresher elapsed/child counts; if a stale duplicate id ever
    /// lived in both places, the live one wins.
    pub fn task_cell_anywhere(&self, id: &str) -> Option<&TaskCell> {
        if let Some(tc) = self.agent_runs.get(id) {
            return Some(tc.detail.as_ref());
        }
        if let Some(tc) = self.live_tasks.get(id).map(|b| b.as_ref()) {
            return Some(tc);
        }
        self.history
            .iter()
            .filter_map(|cell| cell.as_any_ref().downcast_ref::<TaskCell>())
            .find(|tc| tc.tool_use_id == id)
    }

    /// Build rows for the `InFlightAgentsView` (Ctrl+G drill-in).
    /// Returns: live agents first (in spawn order), then up to
    /// `max_recent_completed` completed TaskCells from history
    /// (newest first within the completed group).
    ///
    /// Empty list ⇒ caller should show a "no agents" toast instead
    /// of opening the empty view. Recent completions are included so
    /// the user can drill into a finished sub-agent's output even
    /// after the live strip has dismissed (session 2a98814b: by the
    /// time the user pressed Ctrl+G, all 4 agents had completed and
    /// the strip was gone — pre-fix, Ctrl+G silently no-op'd).
    pub fn agent_monitor_snapshot(
        &self,
        max_recent_completed: usize,
    ) -> crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot {
        use crate::tui::bottom_pane::in_flight_agents_view::{AgentMonitorSnapshot, AgentRow};

        let registry_rows: Vec<AgentRow> = self
            .agent_runs
            .order
            .iter()
            .filter_map(|id| {
                self.agent_runs.get(id).map(|projection| AgentRow {
                    agent_id: id.clone(),
                    name: projection.detail.description.clone(),
                    spawn_tool_call_id: self.agent_runs.spawn_tool_use_for_key(id),
                    activity: projection
                        .activity_counts(self.agent_runs.durable_snapshot_truncated),
                    run_id: projection.run_id.clone(),
                    parent_run_id: projection.parent_run_id.clone(),
                    depth: projection.depth,
                    provenance: projection
                        .metadata_source
                        .unwrap_or(projection.state.source),
                    elapsed_ms: projection.detail.duration_ms.unwrap_or_else(|| {
                        projection.detail.started_at.elapsed().as_millis() as u64
                    }),
                    state: projection.state,
                    attention_summary: projection.attention_summary.clone(),
                    fanout: self.agent_runs.fanout_membership(id).cloned(),
                    control_target: projection.control_target.clone(),
                    transcript_target: projection.transcript_target,
                    available_actions: projection.available_actions.clone(),
                    runtime: projection.runtime_facts.clone(),
                })
            })
            .collect();

        let is_uncertain = |row: &AgentRow| {
            matches!(
                row.state.confidence,
                AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
            )
        };
        let mut rows: Vec<AgentRow> = registry_rows
            .iter()
            .filter(|row| row.state.status.is_active() || is_uncertain(row))
            .cloned()
            .collect();
        // Preserve every confirmed terminal ancestor needed to explain the
        // visible forest. Display names are neither unique nor lineage: only
        // immutable run identities may connect a child to its parent.
        let rows_by_run_id = registry_rows
            .iter()
            .filter_map(|row| row.run_id.as_ref().map(|run_id| (run_id.as_str(), row)))
            .collect::<std::collections::HashMap<_, _>>();
        let mut retained_ids = rows
            .iter()
            .map(|row| row.agent_id.clone())
            .collect::<std::collections::HashSet<_>>();
        let mut visited_run_ids = std::collections::HashSet::new();
        let mut ancestors = rows
            .iter()
            .filter_map(|row| row.parent_run_id.clone())
            .collect::<Vec<_>>();
        while let Some(parent_run_id) = ancestors.pop() {
            if !visited_run_ids.insert(parent_run_id.clone()) {
                continue;
            }
            let Some(parent) = rows_by_run_id.get(parent_run_id.as_str()) else {
                continue;
            };
            if parent.state.status.is_terminal()
                && !is_uncertain(parent)
                && retained_ids.insert(parent.agent_id.clone())
            {
                rows.push((*parent).clone());
            }
            if let Some(grandparent_run_id) = parent.parent_run_id.clone() {
                ancestors.push(grandparent_run_id);
            }
        }
        if max_recent_completed > 0 {
            rows.extend(
                registry_rows
                    .into_iter()
                    .rev()
                    .filter(|row| {
                        row.state.status.is_terminal()
                            && !is_uncertain(row)
                            && !retained_ids.contains(&row.agent_id)
                    })
                    .take(max_recent_completed),
            );
        }
        AgentMonitorSnapshot {
            rows: order_agent_monitor_rows_by_lineage(rows),
            show_root_conversation: false,
            server_truth_state: self.agent_runs.server_truth_state,
            durable_snapshot_truncated: self.agent_runs.durable_snapshot_truncated,
        }
    }

    pub(crate) fn agent_live_transcript_replay(
        &self,
        agent_id: &str,
        run_id: &str,
    ) -> (Vec<astra_turn_core::agent_live_event::AgentLiveEvent>, u64) {
        let Some(key) = self.agent_runs.key_for_run_id(run_id).or_else(|| {
            self.agent_runs.order.iter().find_map(|key| {
                self.agent_runs
                    .get(key)
                    .filter(|projection| {
                        key.as_str() == agent_id && projection.run_id.as_deref() == Some(run_id)
                    })
                    .map(|_| key.as_str())
            })
        }) else {
            return (Vec::new(), 0);
        };
        let Some(projection) = self.agent_runs.get(key) else {
            return (Vec::new(), 0);
        };
        (
            projection.live_transcript_events.iter().cloned().collect(),
            projection.live_transcript_dropped,
        )
    }

    /// Full Agent Workbench projection for every row currently loaded from
    /// the local runtime, workspace recovery, and the bounded durable server
    /// snapshot. Active rows and a bounded recent terminal working set are
    /// retained; older durable history remains available from transcript
    /// paging instead of accumulating in the render model.
    pub(crate) fn agent_workbench_snapshot(
        &self,
    ) -> crate::tui::bottom_pane::in_flight_agents_view::AgentMonitorSnapshot {
        let mut snapshot =
            self.agent_monitor_snapshot(crate::tui::local_agent_journal::RECENT_TERMINAL_RUN_LIMIT);
        // The root transcript belongs in an actual run tree, not as a fake
        // agent row. With no observed children and a confirmed/unbound server
        // lane, Ctrl+G should acknowledge the empty state without stealing
        // focus into a one-row workbench. Degraded durable observation still
        // opens the monitor so its freshness/action state remains visible.
        snapshot.show_root_conversation = !snapshot.rows.is_empty()
            || matches!(
                snapshot.server_truth_state,
                crate::tui::server_agent_observer::ServerAgentTruthState::Loading
                    | crate::tui::server_agent_observer::ServerAgentTruthState::Stale
                    | crate::tui::server_agent_observer::ServerAgentTruthState::Unavailable
            );
        snapshot
    }

    /// IDs of TaskCells still running in the current turn. The
    /// outer event loop calls this when the user hits Ctrl+C so
    /// every live sub-agent gets a cancel RPC. Cleared on turn
    /// boundaries; an individual TaskCell completion also prunes
    /// its entry (so you can Ctrl+C mid-turn with only some
    /// children cancelled).
    pub fn in_flight_task_ids(&self) -> &[String] {
        &self.in_flight_task_ids
    }

    pub fn mark_control_tasks_cancelling(&mut self, ids: &[String]) {
        for id in ids {
            self.cancelling_task_ids.insert(id.clone());
            if let Some(tc) = self.live_tasks.get_mut(id) {
                append_agent_live_output(tc, "\nCancelling…\n");
            }
            let Some(agent_key) = self.agent_runs.key_for_tool_use(id).map(str::to_owned) else {
                continue;
            };
            if let Some(projection) = self.agent_runs.get_mut(&agent_key) {
                if projection.set_state(AgentRunState::local_intent(AgentRunStatus::Cancelling)) {
                    append_agent_live_output(&mut projection.detail, "\nCancelling…\n");
                }
            }
        }
        // Once we've fanned out cancels for these ids, drop them from
        // the in-flight set so a follow-up Ctrl+C is a no-op (count=0,
        // no banner). Pre-fix the same ids stayed visible to the next
        // press; the task service rejected re-cancels and the user
        // saw "Stopped 1 local agent." printed once per press
        // until they all settled. Cancelling badges (the cancelling
        // map above) keep the strip showing "Cancelling…" until the
        // worker's terminal event prunes the rest.
        let to_drop: std::collections::HashSet<&str> = ids.iter().map(String::as_str).collect();
        self.in_flight_task_ids
            .retain(|id| !to_drop.contains(id.as_str()));
    }

    pub fn mark_agent_control_pending(
        &mut self,
        agent_id: &str,
        action: astra_thin_client::SessionRunAction,
    ) -> bool {
        self.agent_runs
            .get_mut(agent_id)
            .is_some_and(|projection| projection.begin_control(action))
    }

    pub fn reject_agent_control(&mut self, agent_id: &str) -> bool {
        self.agent_runs
            .get_mut(agent_id)
            .is_some_and(AgentRunProjection::reject_control)
    }

    /// Commit a single-line banner into scrollback confirming how
    /// many local agents were stopped by the latest Ctrl+C.
    /// No-op when `count == 0` so a normal Ctrl+C (no live tasks)
    /// doesn't clutter the transcript. Called by the event loop
    /// right after `cancel_fanout::fanout`.
    pub fn commit_cancel_banner(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let noun = if count == 1 {
            "local agent"
        } else {
            "local agents"
        };
        let msg = format!("Stopped {count} {noun}.");
        self.commit_cell(Box::new(SystemCell::warning(msg)));
    }

    /// Commit a resume-time summary banner telling the user what
    /// background tasks finished while they were gone. Called once
    /// after replay_session_into_widget finishes, with the message
    /// pre-rendered by `resume_summary::ResumeSummary::render`.
    /// Info-styled because it's neutral history, not an alert.
    pub fn commit_resume_summary(&mut self, message: String) {
        if message.is_empty() {
            return;
        }
        self.commit_cell(Box::new(SystemCell::info(message)));
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn history(&self) -> &[Arc<dyn HistoryCell>] {
        &self.history
    }

    pub(crate) fn history_cell_id(&self, index: usize) -> u64 {
        debug_assert_eq!(self.history.len(), self.history_cell_ids.len());
        self.history_cell_ids[index]
    }

    pub fn active_cell(&self) -> Option<&dyn HistoryCell> {
        self.active_cell.as_deref()
    }

    pub(crate) fn active_cell_id(&self) -> Option<u64> {
        debug_assert_eq!(self.active_cell.is_some(), self.active_cell_id.is_some());
        self.active_cell_id
    }

    /// Drain cells added since the last call. The outer loop uses
    /// this to know which cells to flush into the terminal
    /// scrollback since the previous frame. Invariant: the
    /// returned cells are in the same order they were committed.
    ///
    /// Keeping a "consumed" watermark rather than a queue avoids
    /// copying; callers consume by iterating the returned slice.
    pub fn drain_new_committed(&mut self) -> Vec<Arc<dyn HistoryCell>> {
        let out = self.history[self.committed_watermark..].to_vec();
        self.committed_watermark = self.history.len();
        out
    }

    /// Reset the commit watermark to the current history length.
    /// Used on resume so replayed cells don't get reflushed.
    pub fn mark_all_flushed(&mut self) {
        self.committed_watermark = self.history.len();
    }

    /// Find the last committed `UserCell` and return its text, along
    /// with the number of history entries from that cell onward
    /// (caller may want to also truncate its own model-visible
    /// history, e.g. `state.history`, by the matching turn count).
    ///
    /// Does NOT mutate the widget — this is a pure query. The display
    /// scrollback is already painted to the terminal and cannot be
    /// unwritten; the caller decides what state (if any) to
    /// invalidate. `Ctrl+R` uses this to seed the composer with the
    /// prior user prompt for re-editing.
    pub fn last_user_text(&self) -> Option<String> {
        use super::history_cell::user::UserCell;
        self.history
            .iter()
            .rev()
            .find_map(|c| c.as_any_ref().downcast_ref::<UserCell>())
            .map(|cell| cell.text().to_string())
    }

    /// Swap the backing session id.
    ///
    /// Canonical transcript ownership is the session journal. The widget only
    /// uses this identity for live projection and never writes a competing
    /// per-cell transcript file.
    pub fn set_session_id(&mut self, sid: impl Into<String>) {
        self.session_id = sid.into();
    }

    /// Replay a previously-persisted turn stream into `history`.
    /// Used by the Phase 4 resume path. Cells land already
    /// finalised — no live state, no further mutation.
    ///
    pub fn replay(&mut self, events: Vec<TurnEvent>) {
        for ev in events {
            if let Some(cell) = cell_from_persist(ev) {
                let id = self.allocate_cell_id();
                self.history.push(cell.into());
                self.history_cell_ids.push(id);
            }
        }
    }

    /// Commit a free-standing `SystemCell` — slash-command responses,
    /// info banners, inline errors, etc. Goes into local scrollback only;
    /// control-plane UI rows never become prompt-facing transcript history.
    ///
    pub fn commit_system(&mut self, cell: SystemCell) {
        self.commit_active_and_replay_deferred(); // finalise anything live first
        self.commit_cell(Box::new(cell));
    }

    /// Append a runtime-owned lifecycle projection without claiming that the
    /// currently executing tool has ended. Use this only for concurrent work
    /// receipts/handoffs; ordinary conversational system messages retain the
    /// serial `commit_system` boundary above.
    pub(crate) fn commit_concurrent_system(&mut self, cell: SystemCell) {
        self.commit_cell(Box::new(cell));
    }

    /// Show a local warning without mixing a UI-health issue into canonical
    /// conversation history.
    pub(crate) fn commit_ephemeral_warning(&mut self, message: impl Into<String>) {
        let id = self.allocate_cell_id();
        self.history
            .push(box_into_arc(Box::new(SystemCell::ephemeral_warning(
                message,
            ))));
        self.history_cell_ids.push(id);
    }

    /// Commit a user message directly into history without opening a new turn
    /// or draining the current live tool/assistant state.
    ///
    /// Used for user intents that become active mid-turn: the transcript
    /// should show the newest user message as a first-class user row, but the
    /// current streaming turn should remain live until the runtime yields.
    pub fn commit_applied_user_intent(
        &mut self,
        _intent_id: impl Into<String>,
        _delivery: astra_turn_types::UserIntentDelivery,
        _status: astra_turn_types::UserIntentStatus,
        text: impl Into<String>,
    ) {
        self.commit_cell(Box::new(UserCell::new(text.into())));
    }

    /// Single choke-point for routing events into state mutation.
    /// Any `AppEvent` emitted by the outer loop MUST go through
    /// here — nothing else in the TUI reaches into `history` or
    /// `active_cell`.
    pub fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::User(ue) => self.handle_user(ue),
            AppEvent::Wire(we) => self.handle_wire(*we),
        }
    }

    fn handle_user(&mut self, ev: UserEvent) {
        match ev {
            UserEvent::Submit(text) => self.on_user_submit(text),
        }
    }

    fn handle_wire(&mut self, ev: WireEvent) {
        match ev {
            WireEvent::AnswerDelta(d) => self.on_answer_delta(&d),
            WireEvent::ReasoningDelta(d) => self.on_reasoning_delta(&d),
            WireEvent::ReasoningDone => self.on_reasoning_done(),
            WireEvent::ToolStarted {
                name,
                description,
                tool_use_id,
                parent_tool_use_id,
            } => self.on_tool_started(name, description, tool_use_id, parent_tool_use_id),
            WireEvent::AgentControlStarted {
                action,
                label,
                tool_use_id,
                agent_id,
                fanout_slot,
                fanout_title,
            } => self.on_agent_control_started(
                action,
                label,
                tool_use_id,
                agent_id,
                fanout_slot,
                fanout_title,
            ),
            WireEvent::ToolCompleted {
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
                parent_tool_use_id,
            } => self.on_tool_completed(
                name,
                description,
                status,
                duration_ms,
                output_summary,
                output,
                tool_use_id,
                parent_tool_use_id,
            ),
            WireEvent::AgentControlCompleted {
                action,
                label,
                status,
                duration_ms,
                output,
                tool_use_id,
                agent_id,
            } => self.on_agent_control_completed(
                action,
                label,
                status,
                duration_ms,
                output.as_deref(),
                tool_use_id,
                agent_id,
            ),
            WireEvent::ToolOutput { name, lines, bytes } => {
                self.on_tool_output(&name, lines, bytes)
            }
            WireEvent::AgentLive(event) => self.on_agent_live_event(event),
            WireEvent::AgentLiveBatch(events) => {
                for event in events {
                    self.on_agent_live_event(event);
                }
            }
            WireEvent::AgentLiveGap(gap) => self.on_agent_live_gap(gap),
            WireEvent::AgentCommunication(event) => {
                self.agent_runs.record_communication(&event);
            }
            WireEvent::TurnComplete(stats) => self.on_turn_complete(*stats),
            WireEvent::TurnError(msg) => self.on_turn_error(msg),
            WireEvent::SystemWarning(msg) => self.on_system_warning(msg),
            WireEvent::SystemInfo(msg) => self.on_system_info(msg),
            WireEvent::ExplainReport(items) => self.on_explain_report(items),
            WireEvent::VerdictReport(items) => self.on_verdict_report(items),
            WireEvent::Compaction(event) => {
                self.commit_system(SystemCell::info(event.summary));
            }
        }
    }

    // ── Event handlers ───────────────────────────────────────────

    fn on_user_submit(&mut self, text: String) {
        // A new user turn implicitly finalises any live cell — the
        // previous turn is over whether it committed itself cleanly
        // or not. This includes BOTH the single active_cell slot
        // (assistant streaming, reasoning, single tools) AND every
        // parallel TaskCell still living in `live_tasks` from an
        // abnormally-ended prior turn (server stream drop with no
        // TurnComplete). Without draining live_tasks here, those
        // orphans would persist into the new turn and be misattributed to
        // that turn's transcript snapshot.
        self.commit_active_and_replay_deferred();
        self.drain_all_live_tasks();
        self.end_turn_agent_observation();
        let cell = UserCell::new(text);
        self.commit_cell(Box::new(cell));
    }

    /// Finalize and commit every still-live parallel TaskCell.
    /// Each is `finalize()`d (Running → Unconfirmed) and moved into
    /// history in spawn order. Used by both
    /// `on_user_submit` (orphan cleanup across abnormal turn ends)
    /// and `on_turn_complete` (normal end-of-turn drain).
    fn drain_all_live_tasks(&mut self) {
        let stuck_ids: Vec<String> = std::mem::take(&mut self.live_task_order);
        for id in stuck_ids {
            if let Some(mut tc) = self.live_tasks.remove(&id) {
                tc.finalize();
                self.commit_cell(tc);
            }
        }
        debug_assert!(self.live_tasks.is_empty());
        self.in_flight_task_ids.clear();
        self.cancelling_task_ids.clear();
    }

    fn end_turn_agent_observation(&mut self) {
        // A parent turn boundary says nothing about a child Agent's terminal
        // outcome. Keep the run projection, degrade only active live-stream
        // observations. Terminal history remains available to the Workbench;
        // the durable server snapshot is the only bounded authority.
        self.agent_runs.mark_active_unconfirmed();
    }

    fn on_answer_delta(&mut self, delta: &str) {
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ) {
            self.deferred_stream_events
                .push(DeferredStreamEvent::AnswerDelta(delta.to_string()));
            return;
        }
        // Tokens can begin flowing while another stream-owned cell is still
        // live. The single active-cell slot is type-stable: append to an
        // AssistantCell, or commit the old lane before opening the answer.
        if self
            .active_cell
            .as_deref()
            .map(cell_kind)
            .is_some_and(|kind| kind != CellKind::Assistant)
        {
            self.commit_active();
        }

        // Create the AssistantCell on first delta if needed.
        if !matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Assistant)
        ) {
            self.install_active_cell(Box::new(AssistantCell::new_streaming()));
        }

        if let Some(cell) = self.active_cell.as_mut()
            && let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantCell>()
        {
            ac.push_delta(delta);
        }
    }

    fn on_reasoning_delta(&mut self, delta: &str) {
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ) {
            self.deferred_stream_events
                .push(DeferredStreamEvent::ReasoningDelta(delta.to_string()));
            return;
        }
        // Reasoning can resume or arrive after visible answer/tool output from
        // providers that interleave lanes. Keep the active-cell slot type-stable
        // instead of asserting on a legitimate ordering variant.
        if self
            .active_cell
            .as_deref()
            .map(cell_kind)
            .is_some_and(|kind| kind != CellKind::Reasoning)
        {
            self.commit_active();
        }

        if !matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Reasoning)
        ) {
            self.install_active_cell(Box::new(ReasoningCell::new_streaming()));
        }

        if let Some(cell) = self.active_cell.as_mut()
            && let Some(rc) = cell.as_any_mut().downcast_mut::<ReasoningCell>()
        {
            rc.push_delta(delta);
        }
    }

    fn on_reasoning_done(&mut self) {
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ) {
            self.deferred_stream_events
                .push(DeferredStreamEvent::ReasoningDone);
            return;
        }
        // Only flips the reasoning cell's live flag; the cell
        // stays in `active_cell` because the model might still
        // emit the answer, and keeping it there avoids an extra
        // commit+rebuild round-trip if it does.
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(rc) = cell.as_any_mut().downcast_mut::<ReasoningCell>()
        {
            rc.finalize();
            // Reasoning is done — commit it so the answer can land
            // as its own cell. Keeps the scrollback readable as
            // discrete turns rather than one blob.
            self.commit_active();
        }
    }

    fn on_tool_started(
        &mut self,
        name: String,
        description: String,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    ) {
        if name == "agent_fanout" {
            self.fanout_launch_baselines.insert(
                tool_use_id.clone(),
                self.agent_runs.runs.keys().cloned().collect(),
            );
        }
        // Agent lifecycle is populated only by the structured
        // AgentControl* events. Generic tool descriptions remain display
        // text and are never parsed as a control protocol.
        let agent_spawn_backgroundable = name == "agent"
            && self.agent_runs.action_for_tool_use(&tool_use_id) == Some(AgentControlAction::Spawn);

        // Child event → attach to the matching live parent. Tries
        // both the multi-slot live_tasks register (for parallel
        // agents) and the active_cell slot (legacy single-task path).
        // Top-level scrollback isn't disturbed: the parent stays
        // live and the child renders inside its frame.
        if let Some(parent_id) = parent_tool_use_id.as_deref() {
            if self.route_child_started(parent_id, &tool_use_id, &name, &description) {
                return;
            }
            // Parent not found — fall through to rendering as a
            // top-level tool cell. Safer than dropping: the user
            // still sees the activity in scrollback.
        }

        if is_task_like_tool(&name) {
            // PARALLEL-SAFE: each task tool gets its OWN live slot
            // keyed by tool_use_id. Spawning agent B no longer
            // commits agent A — both stay live concurrently. The
            // renderer handles multi-display via `live_task_ids()`.
            if !self.in_flight_task_ids.iter().any(|s| s == &tool_use_id) {
                self.in_flight_task_ids.push(tool_use_id.clone());
            }
            // Idempotent on duplicate ToolStarted (replay / retry):
            // first event wins on identity; subsequent events for the
            // same id update the description. Children/status are
            // preserved (a duplicate ToolStarted never resets them).
            if let Some(existing) = self.live_tasks.get_mut(&tool_use_id) {
                existing.description = description;
                existing.set_ctrl_b_background_hint(agent_spawn_backgroundable);
            } else {
                let mut task = TaskCell::new_running(tool_use_id.clone(), description);
                task.set_ctrl_b_background_hint(agent_spawn_backgroundable);
                self.live_task_order.push(tool_use_id.clone());
                self.live_tasks.insert(tool_use_id.clone(), Box::new(task));
            }
        } else {
            // Non-Task tools (read_file, bash, …) stay in the single
            // active_cell slot. Within a turn the model emits at
            // most one of these at a time, so a single slot is
            // sufficient and the prior cell is correctly committed.
            self.commit_active_and_replay_deferred();
            let mut cell = ToolCell::new_running(name, description);
            if cell.name == "bash" && self.bash_background_hint_enabled {
                cell.set_ctrl_b_background_hint(true);
            }
            self.install_active_cell(Box::new(cell));
            self.active_tool_use_id = Some(tool_use_id);
        }
    }

    /// Route a `ToolOutput` progress tick to the currently active
    /// ToolCell.
    fn on_tool_output(&mut self, name: &str, lines: u64, bytes: u64) {
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<ToolCell>()
            && tc.name == name
        {
            tc.set_progress(lines, bytes);
        }
    }

    fn on_agent_control_started(
        &mut self,
        action: String,
        label: String,
        tool_use_id: String,
        agent_id: Option<String>,
        fanout_slot: Option<astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity>,
        fanout_title: Option<String>,
    ) {
        let Some(action) = AgentControlAction::from_wire(&action) else {
            return;
        };
        let fanout_membership =
            fanout_slot.map(|slot| agent_fanout_membership(slot, fanout_title.as_deref(), &label));
        let provisional = provisional_agent_key(&tool_use_id);
        let existing_key = self
            .agent_runs
            .key_for_tool_use(&tool_use_id)
            .map(str::to_string);
        let key = agent_id
            .or_else(|| existing_key.clone())
            .unwrap_or_else(|| provisional.clone());
        if let Some(existing) = self
            .agent_runs
            .key_for_tool_use(&tool_use_id)
            .map(str::to_string)
            && existing != key
        {
            self.agent_runs.rename(&existing, key.clone());
        }
        if self.agent_runs.contains_key(&key) {
            if let Some(projection) = self.agent_runs.get_mut(&key) {
                projection.detail.description = if action == AgentControlAction::GetResult {
                    agent_display_name(&key, Some(&label))
                } else {
                    label
                };
            }
            self.agent_runs
                .bind_tool_use(&tool_use_id, key.clone(), action);
        } else {
            let state = match action {
                AgentControlAction::Spawn => AgentRunState::observed(AgentRunStatus::Starting),
                AgentControlAction::GetResult => {
                    AgentRunState::unconfirmed(AgentRunStatus::Running)
                }
            };
            let label = if action == AgentControlAction::GetResult {
                agent_display_name(&key, Some(&label))
            } else {
                label
            };
            self.agent_runs
                .ensure_for_tool_use(key.clone(), label, state, &tool_use_id, action);
        }
        self.agent_runs
            .set_fanout_membership(&key, fanout_membership);
    }

    #[allow(clippy::too_many_arguments)]
    fn on_agent_control_completed(
        &mut self,
        action: String,
        label: String,
        status: String,
        duration_ms: u64,
        output: Option<&str>,
        tool_use_id: String,
        event_agent_id: Option<String>,
    ) {
        let Some(control_action) = AgentControlAction::from_wire(&action) else {
            return;
        };
        let parsed = output.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let canonical_run_id = parsed
            .as_ref()
            .and_then(|value| value.get("run_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|run_id| !run_id.trim().is_empty())
            .map(ToString::to_string);
        let transcript_target = parsed
            .as_ref()
            .and_then(|value| value.get("transcript_location"))
            .and_then(serde_json::Value::as_str)
            .and_then(|value| transcript_target_from_wire(Some(value)));
        let surface = AgentControlSurface::from_wire(&action, &status, parsed.as_ref());
        let agent_id = event_agent_id.or_else(|| surface.agent_id().map(str::to_string));
        let provisional = provisional_agent_key(&tool_use_id);
        let key = agent_id
            .clone()
            .or_else(|| {
                self.agent_runs
                    .key_for_tool_use(&tool_use_id)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| provisional.clone());
        if control_action == AgentControlAction::Spawn
            && agent_id.is_none()
            && matches!(
                surface.outcome(),
                AgentControlOutcome::Failed(
                    agent_control_surface::AgentControlFailureKind::ToolFailed
                )
            )
        {
            self.agent_runs.remove(&provisional);
            return;
        }
        if key != provisional && self.agent_runs.contains_key(&provisional) {
            self.agent_runs.rename(&provisional, key.clone());
        }
        if !self.agent_runs.contains_key(&key) {
            // A failed spawn control call without an assigned Agent identity
            // never created an Agent run. Its generic ToolCell carries the
            // failure; the Agent monitor must not synthesize a failed child.
            self.agent_runs.ensure(
                key.clone(),
                label.clone(),
                AgentRunState::unconfirmed(AgentRunStatus::Running),
            );
        }
        self.agent_runs
            .bind_tool_use(&tool_use_id, key.clone(), control_action);

        let Some(projection) = self.agent_runs.get_mut(&key) else {
            return;
        };
        if AgentRunState::observed(AgentRunStatus::Running).source_rank()
            >= projection.state.source_rank()
        {
            projection.detail.description = agent_id
                .as_deref()
                .map(|id| agent_display_name(id, surface.display_name_hint()))
                .unwrap_or(label);
        }
        if let Some(run_id) = canonical_run_id {
            projection.set_runtime_metadata(
                AgentProjectionSource::LiveStream,
                run_id,
                projection.parent_run_id.clone(),
                projection.depth,
                projection.reported_child_agents,
            );
        }
        if let Some(transcript_target) = transcript_target {
            projection.set_transcript_target(AgentProjectionSource::LiveStream, transcript_target);
        }

        match surface.outcome() {
            AgentControlOutcome::Completed => {
                if projection.set_state(AgentRunState::observed(AgentRunStatus::Completed)) {
                    complete_agent_cell(
                        &mut projection.detail,
                        duration_ms,
                        parsed.as_ref(),
                        output,
                    );
                }
            }
            AgentControlOutcome::Failed(kind) => {
                use agent_control_surface::AgentControlFailureKind;
                let message = surface.failure_message();
                match kind {
                    AgentControlFailureKind::AgentFailed => {
                        if projection.set_state(AgentRunState::observed(AgentRunStatus::Failed)) {
                            fail_agent_cell(
                                &mut projection.detail,
                                duration_ms,
                                parsed.as_ref(),
                                message.as_deref().unwrap_or("agent failed"),
                            );
                        }
                    }
                    AgentControlFailureKind::Interrupted => {
                        if projection
                            .set_state(AgentRunState::observed(AgentRunStatus::Interrupted))
                        {
                            let message =
                                message.unwrap_or_else(|| "agent interrupted".to_string());
                            let summary = crate::tui::agent_control_status::agent_control_result_output_summary(
                                parsed.as_ref(),
                                output,
                            )
                            .or_else(|| Some(message.clone()));
                            projection.detail.complete(
                                "interrupted",
                                duration_ms,
                                summary,
                                Some(message),
                            );
                        }
                    }
                    AgentControlFailureKind::TimedOut | AgentControlFailureKind::ToolFailed => {
                        projection.mark_unconfirmed_if_active();
                        if let Some(message) = message {
                            append_agent_live_output(
                                &mut projection.detail,
                                &format!("\n{message}\n"),
                            );
                        }
                    }
                }
            }
            AgentControlOutcome::Cancelled => {
                let fallback = surface
                    .cancelled_reason()
                    .unwrap_or("agent cancelled")
                    .to_string();
                if projection.set_state(AgentRunState::observed(AgentRunStatus::Cancelled)) {
                    projection.detail.complete(
                        "cancelled",
                        duration_ms,
                        Some(fallback.clone()),
                        Some(fallback),
                    );
                }
            }
            AgentControlOutcome::Running => {
                if projection.set_state(AgentRunState::observed(AgentRunStatus::Running))
                    && let Some(preview) = surface.running_preview()
                {
                    if projection
                        .detail
                        .output_summary
                        .as_deref()
                        .is_some_and(|s| !s.is_empty())
                    {
                        append_agent_live_output(&mut projection.detail, &format!("\n{preview}\n"));
                    } else {
                        projection.detail.output_summary = Some(preview);
                    }
                }
            }
            AgentControlOutcome::NoChange => {}
        }
    }

    /// Project a structured fanout launch receipt into the same run registry
    /// used by direct agent spawns. A group receipt carries the canonical
    /// `run_id` for every child; reducing it to a fanout summary would make
    /// live agents visible but their conversations unreachable.
    ///
    /// This consumes only typed receipt fields. A malformed or partial receipt
    /// creates no guessed run, and later live/server evidence can still fill
    /// any missing child independently.
    fn on_agent_fanout_launch_receipt(&mut self, output: &str) {
        let Ok(receipt) = serde_json::from_str::<serde_json::Value>(output) else {
            return;
        };
        let Some(group_id) = receipt
            .get("group_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        else {
            return;
        };
        let Some(target_count) = receipt
            .get("target_count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
        else {
            return;
        };
        // A launch reply carries `agents`; a recovered result carries the
        // registry's `fanout.slots`. Read both when available: the registry
        // is authoritative for a slot identity if a partial transport reply
        // omitted a child field.
        let agents = receipt
            .get("agents")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .chain(
                receipt
                    .get("fanout")
                    .and_then(|fanout| fanout.get("slots"))
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .collect::<Vec<_>>();
        if agents.is_empty() {
            return;
        }
        let group_title = receipt
            .get("title")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(group_id)
            .to_string();
        let parent_run_id = receipt
            .get("parent_run_id")
            .or_else(|| receipt.get("fanout")?.get("parent_run_id"))
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(ToString::to_string);
        let receipt_transcript_target = receipt
            .get("transcript_location")
            .and_then(serde_json::Value::as_str)
            .and_then(|value| transcript_target_from_wire(Some(value)));

        for agent in agents {
            let Some(agent_id) = agent
                .get("agent_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string)
            else {
                continue;
            };
            let Some(run_id) = agent
                .get("run_id")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map(ToString::to_string)
            else {
                continue;
            };
            let Some(state) = agent
                .get("status")
                .and_then(serde_json::Value::as_str)
                .and_then(agent_run_state_from_fanout_receipt)
            else {
                continue;
            };
            let transcript_target = agent
                .get("transcript_location")
                .and_then(serde_json::Value::as_str)
                .and_then(|value| transcript_target_from_wire(Some(value)))
                .or(receipt_transcript_target);
            let slot_index = agent
                .get("slot_index")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| usize::try_from(value).ok());
            let Some(slot_index) = slot_index.filter(|index| *index < target_count) else {
                continue;
            };
            let requested_description = receipt
                .get("fanout")
                .and_then(|fanout| fanout.get("slots"))
                .and_then(serde_json::Value::as_array)
                .and_then(|slots| slots.get(slot_index))
                .and_then(|slot| slot.get("requested_description"))
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty());
            let slot_label = requested_description
                .or_else(|| {
                    agent
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.trim().is_empty())
                })
                .unwrap_or(agent_id.as_str());
            let membership =
                crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership {
                    group_id: group_id.to_string(),
                    group_title: group_title.clone(),
                    target_count,
                    slot_index,
                    slot_label: slot_label.to_string(),
                };
            self.agent_runs.ensure(
                agent_id.clone(),
                slot_label.to_string(),
                AgentRunState::observed(state),
            );
            self.agent_runs
                .set_fanout_membership(&agent_id, Some(membership));
            let Some(projection) = self.agent_runs.get_mut(&agent_id) else {
                continue;
            };
            projection.set_runtime_metadata(
                AgentProjectionSource::LiveStream,
                run_id,
                parent_run_id.clone(),
                1,
                0,
            );
            if let Some(transcript_target) = transcript_target {
                projection
                    .set_transcript_target(AgentProjectionSource::LiveStream, transcript_target);
            }
        }
    }

    fn on_agent_live_event(&mut self, event: astra_turn_core::agent_live_event::AgentLiveEvent) {
        use astra_turn_core::agent_live_event::AgentLiveEventKind;

        // A control call is visible before the child runtime has allocated its
        // canonical identities. The typed start correlation closes that gap at
        // the first live boundary, so Ctrl+G opens one real conversation
        // instead of a provisional row that can never receive child output.
        if let AgentLiveEventKind::Signal(
            astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                spawn_tool_call_id: Some(tool_call_id),
                ..
            },
        ) = &event.kind
        {
            if let Some(provisional_key) = self
                .agent_runs
                .key_for_tool_use(tool_call_id)
                .map(str::to_owned)
            {
                self.agent_runs
                    .rename(&provisional_key, event.agent_id.clone());
            } else {
                self.agent_runs.bind_tool_use(
                    tool_call_id,
                    event.agent_id.clone(),
                    AgentControlAction::Spawn,
                );
            }
        }

        let (routing_agent_id, routing_run_id) = if let AgentLiveEventKind::Signal(
            astra_turn_core::agent_live_event::AgentLiveSignal::AgentCommunication(communication),
        ) = &event.kind
        {
            (
                communication.observed_by.agent_id.as_str(),
                communication.observed_by.run_id.as_str(),
            )
        } else {
            (event.agent_id.as_str(), event.run_id.as_str())
        };
        let run_key = self
            .agent_runs
            .key_for_live_event(routing_agent_id, routing_run_id);
        let is_terminal_event = matches!(event.kind, AgentLiveEventKind::AgentTerminated { .. });
        if !is_terminal_event
            && self
                .agent_runs
                .get(&run_key)
                .is_some_and(|projection| projection.state.status.is_terminal())
        {
            return;
        }

        let label = self
            .agent_runs
            .get(&run_key)
            .map(|projection| projection.detail.description.trim())
            .filter(|description| {
                !description.is_empty()
                    && *description != routing_agent_id
                    && *description != run_key
            })
            .map(str::to_owned)
            .unwrap_or_else(|| agent_display_name(routing_agent_id, None));
        let observed_status = match &event.kind {
            AgentLiveEventKind::AgentTerminated { termination, .. } => {
                use astra_turn_core::agent_live_event::AgentLiveTermination;
                match termination {
                    AgentLiveTermination::Completed => AgentRunStatus::Completed,
                    AgentLiveTermination::Delegated => AgentRunStatus::Delegated,
                    AgentLiveTermination::Failed => AgentRunStatus::Failed,
                    AgentLiveTermination::Interrupted => AgentRunStatus::Interrupted,
                    AgentLiveTermination::Cancelled => AgentRunStatus::Cancelled,
                }
            }
            // Attention is a lifecycle fact, not just another line of live
            // output. Until the agent receives the requested input or
            // approval, the run is waiting and the workbench must make that
            // distinct from model/tool activity.
            AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::AskUserPrompted { .. }
                | astra_turn_core::agent_live_event::AgentLiveSignal::ApprovalRequired { .. }
                | astra_turn_core::agent_live_event::AgentLiveSignal::ExecutionWaiting { .. },
            ) => AgentRunStatus::Waiting,
            AgentLiveEventKind::OutputDelta(_)
            | AgentLiveEventKind::ThinkingDelta(_)
            | AgentLiveEventKind::Status(_)
            | AgentLiveEventKind::Signal(_)
            | AgentLiveEventKind::ToolStarted { .. }
            | AgentLiveEventKind::ToolCompleted { .. } => AgentRunStatus::Running,
        };
        let state_accepted = self.agent_runs.ensure(
            run_key.clone(),
            label.clone(),
            AgentRunState::observed(observed_status),
        );
        if is_terminal_event && !state_accepted {
            return;
        }
        let mut parent_task_mirror = None;
        {
            let Some(projection) = self.agent_runs.get_mut(&run_key) else {
                return;
            };
            projection.record_live_transcript_event(&event);
            let (event_parent_run_id, event_depth) = match &event.kind {
                AgentLiveEventKind::Signal(
                    astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                        parent_run_id,
                        depth,
                        ..
                    },
                ) => (parent_run_id.clone(), *depth),
                _ => (projection.parent_run_id.clone(), projection.depth),
            };
            // Every typed live event is scoped to an immutable execution run.
            // `RunStarted` is useful lineage enrichment, but it is not a
            // prerequisite for transcript identity: token/tool events can
            // legitimately arrive first or be the only events delivered.
            if projection.run_id.is_none() {
                projection.set_runtime_metadata(
                    AgentProjectionSource::LiveStream,
                    routing_run_id.to_string(),
                    event_parent_run_id,
                    event_depth,
                    projection.reported_child_agents,
                );
                // A live envelope proves an execution identity, not where
                // durable history lives or who owns a control lease. Local,
                // Edge and server streams share this type, so guessing a
                // local journal or Cancel capability here would make a
                // remote run look controllable and addressable through the
                // wrong boundary. Those facts arrive through the typed start
                // event, launch receipt, local-runtime snapshot, journal
                // recovery, or durable server projection.
            }
            if let AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::AgentCommunication(
                    communication,
                ),
            ) = &event.kind
            {
                match communication.direction {
                    astra_turn_types::AgentCommunicationDirection::Sent => {
                        projection.messages_sent = projection.messages_sent.saturating_add(1);
                    }
                    astra_turn_types::AgentCommunicationDirection::Received => {
                        projection.messages_received =
                            projection.messages_received.saturating_add(1);
                    }
                }
            }
            if let AgentLiveEventKind::Signal(
                astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted {
                    parent_run_id,
                    depth,
                    spawn_tool_call_id: _,
                    transcript_location,
                },
            ) = &event.kind
            {
                projection.set_runtime_metadata(
                    AgentProjectionSource::LiveStream,
                    event.run_id.clone(),
                    parent_run_id.clone(),
                    *depth,
                    0,
                );
                projection.set_transcript_target(
                    AgentProjectionSource::LiveStream,
                    match transcript_location {
                        astra_turn_types::AgentTranscriptLocation::LocalJournal => {
                            crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal
                        }
                        astra_turn_types::AgentTranscriptLocation::DurableServer => {
                            crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer
                        }
                    },
                );
            }
            if let AgentLiveEventKind::Signal(signal) = &event.kind
                && matches!(
                    signal,
                    astra_turn_core::agent_live_event::AgentLiveSignal::AskUserPrompted { .. }
                        | astra_turn_core::agent_live_event::AgentLiveSignal::ApprovalRequired { .. }
                        | astra_turn_core::agent_live_event::AgentLiveSignal::ExecutionWaiting { .. }
                )
            {
                projection.set_attention_summary(Some(agent_live_signal_summary(signal)));
            }
            let cell = &mut projection.detail;
            if cell.description == event.agent_id {
                cell.description = label;
            }
            match event.kind {
                AgentLiveEventKind::OutputDelta(text) | AgentLiveEventKind::ThinkingDelta(text) => {
                    append_agent_live_output(cell, &text);
                }
                AgentLiveEventKind::Status(text) => {
                    append_agent_live_output(cell, &format!("\n{text}\n"));
                }
                AgentLiveEventKind::Signal(signal) => {
                    if !matches!(
                        signal,
                        astra_turn_core::agent_live_event::AgentLiveSignal::RunStarted { .. }
                    ) {
                        append_agent_live_output(
                            cell,
                            &format!("\n{}\n", agent_live_signal_summary(&signal)),
                        );
                    }
                }
                AgentLiveEventKind::ToolStarted {
                    name,
                    description,
                    tool_use_id,
                } => {
                    cell.push_child_started(tool_use_id.clone(), name.clone(), description.clone());
                    parent_task_mirror = Some(AgentLiveMirror::Started {
                        tool_use_id,
                        name,
                        description,
                    });
                }
                AgentLiveEventKind::ToolCompleted {
                    name: _,
                    description: _,
                    status,
                    duration_ms,
                    output_summary,
                    output,
                    tool_use_id,
                } => {
                    cell.push_child_completed(&tool_use_id, &status, duration_ms);
                    parent_task_mirror = Some(AgentLiveMirror::Completed {
                        tool_use_id,
                        status,
                        duration_ms,
                    });
                    if let Some(text) = output_summary.or(output).filter(|s| !s.trim().is_empty()) {
                        append_agent_live_output(cell, &format!("\n{text}\n"));
                    }
                }
                AgentLiveEventKind::AgentTerminated {
                    termination,
                    duration_ms,
                    reason,
                } => {
                    use astra_turn_core::agent_live_event::AgentLiveTermination;
                    let status_str = match termination {
                        AgentLiveTermination::Completed => "completed",
                        AgentLiveTermination::Delegated => "delegated",
                        AgentLiveTermination::Failed => "failed",
                        AgentLiveTermination::Interrupted => "interrupted",
                        AgentLiveTermination::Cancelled => "cancelled",
                    };
                    let summary = reason.clone().or_else(|| cell.output_summary.clone());
                    let elapsed = cell.started_at.elapsed().as_millis() as u64;
                    cell.complete(status_str, elapsed.max(duration_ms), summary, reason);
                }
            }
        }
        match parent_task_mirror {
            Some(AgentLiveMirror::Started {
                tool_use_id,
                name,
                description,
            }) => self.mirror_live_child_started_to_parent_tasks(
                &run_key,
                &tool_use_id,
                &name,
                &description,
            ),
            Some(AgentLiveMirror::Completed {
                tool_use_id,
                status,
                duration_ms,
            }) => self.mirror_live_child_completed_to_parent_tasks(
                &run_key,
                &tool_use_id,
                &status,
                duration_ms,
            ),
            None => {}
        }
        if is_terminal_event {
            self.agent_runs
                .prune_terminal_history(crate::tui::local_agent_journal::RECENT_TERMINAL_RUN_LIMIT);
        }
    }

    fn on_agent_live_gap(&mut self, gap: astra_turn_core::agent_live_event::AgentLiveGap) {
        let run_key = self
            .agent_runs
            .key_for_live_event(&gap.agent_id, &gap.run_id);
        let label = agent_display_name(&gap.agent_id, None);
        self.agent_runs.ensure(
            run_key.clone(),
            label.clone(),
            AgentRunState::observed(AgentRunStatus::Running),
        );
        let Some(projection) = self.agent_runs.get_mut(&run_key) else {
            return;
        };
        if projection.run_id.is_none() {
            projection.set_runtime_metadata(
                AgentProjectionSource::LiveStream,
                gap.run_id,
                projection.parent_run_id.clone(),
                projection.depth,
                projection.reported_child_agents,
            );
        }
        if projection.detail.description == gap.agent_id {
            projection.detail.description = label;
        }
        projection.set_attention_summary(Some(format!(
            "Live activity incomplete · {} update{} skipped · syncing durable state",
            gap.dropped_event_count,
            if gap.dropped_event_count == 1 {
                ""
            } else {
                "s"
            }
        )));
        projection.detail.error = None;
    }

    fn mirror_live_child_started_to_parent_tasks(
        &mut self,
        agent_id: &str,
        tool_use_id: &str,
        name: &str,
        description: &str,
    ) {
        for parent_tool_use_id in self.agent_runs.tool_uses_for_key(agent_id) {
            if let Some(task) = self.live_tasks.get_mut(&parent_tool_use_id) {
                task.push_child_started(tool_use_id, name, description);
            }
        }
    }

    fn mirror_live_child_completed_to_parent_tasks(
        &mut self,
        agent_id: &str,
        tool_use_id: &str,
        status: &str,
        duration_ms: u64,
    ) {
        for parent_tool_use_id in self.agent_runs.tool_uses_for_key(agent_id) {
            if let Some(task) = self.live_tasks.get_mut(&parent_tool_use_id) {
                task.push_child_completed(tool_use_id, status, duration_ms);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn on_tool_completed(
        &mut self,
        name: String,
        description: String,
        status: String,
        duration_ms: u64,
        output_summary: Option<String>,
        output: Option<String>,
        tool_use_id: String,
        parent_tool_use_id: Option<String>,
    ) {
        if name == "agent_fanout"
            && let Some(output) = output.as_deref()
        {
            self.on_agent_fanout_launch_receipt(output);
        }
        let fanout_baseline = (name == "agent_fanout").then(|| {
            self.fanout_launch_baselines
                .remove(&tool_use_id)
                .unwrap_or_default()
        });
        let receipt_missing = name == "agent_fanout"
            && !fanout_completion_has_receipt(output_summary.as_deref(), output.as_deref());
        let observed_new_runs = fanout_baseline
            .filter(|_| status == "failed" && receipt_missing)
            .map(|baseline| {
                self.agent_runs
                    .runs
                    .keys()
                    .filter(|run_key| !baseline.contains(*run_key))
                    .count()
            })
            .filter(|count| *count > 0);
        let (status, output_summary, output) = match observed_new_runs {
            Some(count) => (
                "uncertain".to_string(),
                Some(format!(
                    "Observed {count} new agent run{} while the launch receipt was unavailable · Shift+↓ opens background work.",
                    if count == 1 { "" } else { "s" }
                )),
                None,
            ),
            None if name == "agent_fanout" => {
                if let Some(summary) =
                    fanout_rejection_summary(output_summary.as_deref(), output.as_deref())
                {
                    (status, Some(summary), None)
                } else if status == "failed" && receipt_missing {
                    // A transport-side completion without a typed receipt is
                    // not lifecycle authority.  The local/server task
                    // registry may already own accepted children (and can
                    // arrive one observer tick later), so painting a red
                    // terminal failure here creates a contradictory UI. A
                    // typed admission rejection above remains a real failure.
                    (
                        "uncertain".to_string(),
                        Some(
                            "Launch confirmation is delayed · Astra is checking the task registry · Shift+↓ inspect."
                                .to_string(),
                        ),
                        None,
                    )
                } else {
                    (status, output_summary, output)
                }
            }
            None => (status, output_summary, output),
        };
        // Child completion → update the child row inside its
        // parent Task. If the parent is already terminal/gone we
        // fall back to top-level to stay visible rather than drop.
        if let Some(parent_id) = parent_tool_use_id.as_deref()
            && self.route_child_completed(parent_id, &tool_use_id, &status, duration_ms)
        {
            return;
        }

        // Task parent completion → always prune the in-flight set
        // (committed-then-completed tasks still need cleanup). Then
        // try the multi-slot live_tasks register first (parallel
        // agent path), then the legacy single-active-cell path.
        if is_task_like_tool(&name) {
            self.in_flight_task_ids.retain(|s| s != &tool_use_id);
            self.cancelling_task_ids.remove(&tool_use_id);
            // Multi-slot path: finalize and commit just THIS agent,
            // leaving other parallel agents live and undisturbed.
            if let Some(mut tc) = self.live_tasks.remove(&tool_use_id) {
                self.live_task_order.retain(|s| s != &tool_use_id);
                tc.complete(&status, duration_ms, output_summary, None);
                self.commit_cell(tc);
                return;
            }
            // Legacy active_cell path (single-task scenarios, replay).
            if let Some(cell) = self.active_cell.as_mut()
                && let Some(tc) = cell.as_any_mut().downcast_mut::<TaskCell>()
                && tc.tool_use_id == tool_use_id
            {
                tc.complete(&status, duration_ms, output_summary, None);
                self.commit_active();
                return;
            }
            // Late ToolCompleted: parent already drained by
            // `on_turn_complete` / `on_user_submit` orphan cleanup.
            // Do NOT fall through and synthesize a ToolCell — that
            // would render the task TWICE in scrollback (once as
            // its finalized TaskCell from drain_all_live_tasks,
            // again as a fresh ToolCell here). Silent no-op is the
            // right behavior; the user already saw the parent
            // completed via the prior drain.
            return;
        }

        // Update the in-flight tool cell if one exists; otherwise
        // synthesize a new completed cell (happens when the model
        // emits a ToolCompleted without a paired ToolStarted —
        // e.g. replayed from journal mid-turn).
        if self.active_tool_use_id.as_deref() == Some(tool_use_id.as_str())
            && let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<ToolCell>()
        {
            tc.complete(&status, duration_ms, description, output_summary, output);
            self.commit_active();
            self.replay_deferred_stream_events();
            return;
        }

        let mut synth = ToolCell::new_running(name, description);
        synth.complete(&status, duration_ms, String::new(), output_summary, output);
        self.commit_cell(Box::new(synth));
    }

    /// Route a child `ToolStarted` into a still-live TaskCell. Returns
    /// `true` when the parent was found and the child was appended,
    /// `false` to let the caller fall back to top-level rendering.
    fn route_child_started(
        &mut self,
        parent_id: &str,
        child_tool_use_id: &str,
        name: &str,
        description: &str,
    ) -> bool {
        // Multi-slot live_tasks first — the canonical home for
        // parallel agent parents. O(1) lookup by tool_use_id.
        if let Some(tc) = self.live_tasks.get_mut(parent_id) {
            tc.push_child_started(child_tool_use_id, name, description);
            return true;
        }
        // Legacy active_cell slot — covers single-task paths from
        // before the multi-slot rework, still used by tests/replay.
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<TaskCell>()
            && tc.tool_use_id == parent_id
        {
            tc.push_child_started(child_tool_use_id, name, description);
            return true;
        }
        // Committed history — Arc<dyn HistoryCell> isn't mutable
        // in-place. Children arriving after the parent finalised
        // are rare (out-of-order replay) and render top-level.
        false
    }

    fn route_child_completed(
        &mut self,
        parent_id: &str,
        child_tool_use_id: &str,
        status: &str,
        duration_ms: u64,
    ) -> bool {
        // Multi-slot first.
        if let Some(tc) = self.live_tasks.get_mut(parent_id) {
            tc.push_child_completed(child_tool_use_id, status, duration_ms);
            return true;
        }
        // Legacy active_cell path.
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<TaskCell>()
            && tc.tool_use_id == parent_id
        {
            tc.push_child_completed(child_tool_use_id, status, duration_ms);
            return true;
        }
        false
    }

    fn on_turn_complete(&mut self, stats: TurnStats) {
        // Any live cell at turn-complete time gets committed
        // unconditionally (the model ended the turn, so any
        // dangling stream is done). Same for parallel TaskCells:
        // finalize and commit. `drain_all_live_tasks` also clears
        // in_flight_task_ids — single source of truth for that
        // sequence (shared with on_user_submit).
        self.commit_active_and_replay_deferred();
        self.drain_all_live_tasks();
        self.end_turn_agent_observation();

        let summary = TurnSummaryCell {
            elapsed_ms: stats.elapsed_ms,
            ttft_ms: stats.ttft_ms,
            tokens_in: stats.tokens_in,
            tokens_out: stats.tokens_out,
            cache_read_tokens: stats.cache_read_tokens,
            tools: stats.tools,
            cumulative_tokens: stats.cumulative_tokens,
            cumulative_cost_usd: stats.cumulative_cost_usd,
            ts: None,
        };
        self.commit_cell(Box::new(summary));
    }

    /// Freeze the stream-owned cell when the transport closes, without
    /// claiming that the complete turn (durability, summary, background
    /// settlement) has finished. This keeps a completed answer from retaining
    /// live typing affordances while the turn owner finishes its own work.
    /// A running tool outlives the model SSE segment that requested it; only
    /// its typed completion/cancellation or the terminal turn boundary owns
    /// that lifecycle transition.
    pub(crate) fn finish_stream_projection(&mut self) {
        if self.has_live_tool_projection() {
            return;
        }
        self.commit_active_and_replay_deferred();
    }

    pub(crate) fn has_live_tool_projection(&self) -> bool {
        self.active_cell.as_ref().is_some_and(|cell| {
            matches!(cell_kind(cell.as_ref()), CellKind::Tool) && cell.is_live()
        }) || !self.live_tasks.is_empty()
    }

    fn on_turn_error(&mut self, msg: String) {
        self.commit_active_and_replay_deferred();
        self.drain_all_live_tasks();
        self.end_turn_agent_observation();
        self.commit_cell(Box::new(SystemCell::error(msg)));
    }

    fn on_system_warning(&mut self, msg: String) {
        self.commit_cell(Box::new(SystemCell::warning(msg)));
    }

    fn on_system_info(&mut self, msg: String) {
        self.commit_cell(Box::new(SystemCell::info(msg)));
    }

    fn on_explain_report(&mut self, items: Vec<serde_json::Value>) {
        if items.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        for item in &items {
            let mut line = String::new();
            if let Some(ms) = item.get("total_ms").and_then(|v| v.as_i64()) {
                line.push_str(&format!("⏱ {:.1}s", ms as f64 / 1000.0));
            }
            if let Some(selected) = item.get("visible_tools").and_then(|v| v.as_u64()) {
                if let Some(available) = item.get("tools_available").and_then(|v| v.as_u64()) {
                    if !line.is_empty() {
                        line.push_str(" | ");
                    }
                    line.push_str(&format!("🛠 {}/{} tools", selected, available));
                }
            }
            if let Some(steps) = item.get("steps").and_then(|v| v.as_array()) {
                if !line.is_empty() {
                    line.push_str(" | ");
                }
                line.push_str(&format!("📋 {} steps", steps.len()));
            }
            if !line.is_empty() {
                parts.push(line);
                continue;
            }
            if let Some(content) = item.get("content").and_then(|v| v.as_str()) {
                let content = content.trim();
                if !content.is_empty() {
                    parts.push(content.to_string());
                }
            }
        }
        if !parts.is_empty() {
            let text = format!("Context Explain\n{}", parts.join("\n"));
            self.commit_cell(Box::new(SystemCell::info(text)));
        }
    }

    fn on_verdict_report(&mut self, items: Vec<VerdictEvent>) {
        if items.is_empty() {
            return;
        }
        let mut parts = Vec::new();
        for item in &items {
            let severity: &str = &item.severity;
            let icon = match severity {
                "error" => "❌",
                "warn" => "⚠️",
                _ => "ℹ️",
            };
            let mut desc = format!("{} severity={}", icon, severity);
            if item.advisory_threshold_reached {
                desc.push_str(" strong_advisory");
            }
            if item.total_errors > 0 {
                desc.push_str(&format!(" errors={}", item.total_errors));
            }
            if item.nudge_count > 0 {
                desc.push_str(&format!(" nudges={}", item.nudge_count));
            }
            if !item.avoid_tools.is_empty() {
                desc.push_str(&format!(" avoid=[{}]", item.avoid_tools.join(", ")));
            }
            parts.push(desc);
        }
        let text = format!("Verdict report\n{}", parts.join("\n"));
        self.commit_cell(Box::new(SystemCell::info(text)));
    }

    // ── Invariant-preserving mutators ────────────────────────────

    fn allocate_cell_id(&mut self) -> u64 {
        let id = self.next_cell_id;
        self.next_cell_id = self
            .next_cell_id
            .checked_add(1)
            .expect("transcript cell identity space exhausted");
        id
    }

    fn install_active_cell(&mut self, cell: Box<dyn HistoryCell>) {
        debug_assert!(self.active_cell.is_none());
        debug_assert!(self.active_cell_id.is_none());
        debug_assert!(self.active_tool_use_id.is_none());
        self.active_cell_id = Some(self.allocate_cell_id());
        self.active_cell = Some(cell);
    }

    fn replay_deferred_stream_events(&mut self) {
        let deferred = std::mem::take(&mut self.deferred_stream_events);
        for event in deferred {
            match event {
                DeferredStreamEvent::AnswerDelta(delta) => self.on_answer_delta(&delta),
                DeferredStreamEvent::ReasoningDelta(delta) => self.on_reasoning_delta(&delta),
                DeferredStreamEvent::ReasoningDone => self.on_reasoning_done(),
            }
        }
    }

    /// Close the current lane at a transcript boundary, then materialize any
    /// answer/reasoning events held behind a running tool and close that lane
    /// too. The two commits preserve wire order: tool first, deferred text
    /// second, boundary cell last.
    fn commit_active_and_replay_deferred(&mut self) {
        self.commit_active();
        self.replay_deferred_stream_events();
        self.commit_active();
    }

    /// Take the currently-live cell, finalise it, append to
    /// history, and persist. No-op when `active_cell` is None.
    fn commit_active(&mut self) {
        let Some(mut cell) = self.active_cell.take() else {
            debug_assert!(self.active_cell_id.is_none());
            debug_assert!(self.active_tool_use_id.is_none());
            return;
        };
        self.active_tool_use_id = None;
        let id = self
            .active_cell_id
            .take()
            .unwrap_or_else(|| self.allocate_cell_id());
        cell.finalize();
        // Box → Arc: the scrollback index shares cells with
        // long-lived render paths (e.g. Ctrl+O overlay) without
        // forcing everyone onto `&dyn`.
        self.history.push(box_into_arc(cell));
        self.history_cell_ids.push(id);
    }

    /// Append an already-finalised cell. Used for UserCell /
    /// synthesised ToolCell / TurnSummary etc. — things built
    /// whole rather than streamed.
    fn commit_cell(&mut self, cell: Box<dyn HistoryCell>) {
        let id = self.allocate_cell_id();
        self.history.push(box_into_arc(cell));
        self.history_cell_ids.push(id);
    }
}

// ── Helpers ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CellKind {
    User,
    Assistant,
    Reasoning,
    Tool,
    System,
    TurnSummary,
    Other,
}

/// Names of tools whose scrollback rendering is a [`TaskCell`]
/// container — i.e. tools that spawn a sub-agent whose own tool
/// calls stream back with `parent_tool_use_id` set. Extend this
/// allowlist when a new delegate-style tool lands.
///
/// The `task_board` tool (session_todos) is intentionally NOT included:
/// it is a flat TODO tracker that never emits children, so a tree
/// header would be noise.
fn is_task_like_tool(name: &str) -> bool {
    matches!(name, "agent")
}

fn agent_display_name(agent_id: &str, fallback: Option<&str>) -> String {
    agent_id
        .split_once('@')
        .map(|(name, _)| name)
        .filter(|name| !name.is_empty())
        .or(fallback)
        .unwrap_or(agent_id)
        .to_string()
}

fn provisional_agent_key(tool_use_id: &str) -> String {
    format!("pending:{tool_use_id}")
}

fn complete_agent_cell(
    cell: &mut TaskCell,
    duration_ms: u64,
    parsed: Option<&serde_json::Value>,
    raw_output: Option<&str>,
) {
    let result =
        crate::tui::agent_control_status::agent_control_result_output_summary(parsed, raw_output);
    let elapsed = cell.started_at.elapsed().as_millis() as u64;
    cell.complete("completed", elapsed.max(duration_ms), result, None);
}

pub(crate) fn agent_live_signal_summary(
    signal: &astra_turn_core::agent_live_event::AgentLiveSignal,
) -> String {
    use astra_turn_core::agent_live_event::AgentLiveSignal;
    match signal {
        AgentLiveSignal::RunStarted { .. } => "Run started".into(),
        AgentLiveSignal::WaitingForModel => "Waiting for model".into(),
        AgentLiveSignal::ExecutionWaiting { reason } => format!("Waiting · {reason}"),
        AgentLiveSignal::ModelResponding => "Model responding".into(),
        AgentLiveSignal::OutputSettled => "Reply ready".into(),
        AgentLiveSignal::TranscriptCommitted {
            transcript_location,
            ..
        } => format!("Transcript committed · {transcript_location:?}"),
        AgentLiveSignal::AskUserPrompted { prompt, .. } => {
            let count = prompt
                .get("prompt")
                .and_then(|value| value.get("question_count"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            format!("Input requested · {count} question(s)")
        }
        AgentLiveSignal::AskUserResolved { resolution, .. } => {
            let outcome = resolution
                .get("audit")
                .and_then(|value| value.get("response"))
                .and_then(|value| value.get("outcome"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("resolved");
            format!("Input request · {outcome}")
        }
        AgentLiveSignal::UserIntentApplied {
            status, content, ..
        } => format!("Guidance {status:?} · {content}"),
        AgentLiveSignal::AgentCommunication(event) => {
            let peer = match event.direction {
                astra_turn_types::AgentCommunicationDirection::Sent => match &event.to {
                    astra_turn_types::AgentCommunicationTarget::Direct { address } => {
                        address.agent_id.as_str()
                    }
                    astra_turn_types::AgentCommunicationTarget::Broadcast { delegation_id } => {
                        delegation_id.as_str()
                    }
                    astra_turn_types::AgentCommunicationTarget::Parent => "parent",
                },
                astra_turn_types::AgentCommunicationDirection::Received => {
                    event.from.agent_id.as_str()
                }
            };
            let direction = match event.direction {
                astra_turn_types::AgentCommunicationDirection::Sent => "sent to",
                astra_turn_types::AgentCommunicationDirection::Received => "received from",
            };
            format!("Message {direction} {peer} · {}", event.payload_kind)
        }
        AgentLiveSignal::PermissionAutoApproved { tool, reason } => {
            astra_turn_core::permission::notice::format_auto_approved_permission(tool, reason)
                .trim()
                .to_string()
        }
        AgentLiveSignal::ApprovalRequired {
            tool,
            display_label,
            detail,
            ..
        } => format!(
            "Approval required · {}",
            display_label
                .as_deref()
                .or(detail.as_deref())
                .unwrap_or(tool)
        ),
        AgentLiveSignal::AgentControlStarted { label, .. } => {
            format!("Agent control started · {label}")
        }
        AgentLiveSignal::AgentControlCompleted { label, status, .. } => {
            format!("Agent control {status} · {label}")
        }
        AgentLiveSignal::ToolProgress { name, lines, bytes } => {
            format!("{name} · {lines} lines · {bytes} bytes")
        }
    }
}

const MAX_AGENT_LIVE_OUTPUT_CHARS: usize = 100_000;

fn append_agent_live_output(cell: &mut TaskCell, text: &str) {
    if text.is_empty() {
        return;
    }
    let mut next = cell.output_summary.take().unwrap_or_default();
    next.push_str(text);
    if next.len() > MAX_AGENT_LIVE_OUTPUT_CHARS {
        let keep_from = next.len().saturating_sub(MAX_AGENT_LIVE_OUTPUT_CHARS);
        let safe_keep_from = next
            .char_indices()
            .map(|(idx, _)| idx)
            .find(|idx| *idx >= keep_from)
            .unwrap_or(keep_from);
        next = format!(
            "[older live output truncated]\n{}",
            next.split_off(safe_keep_from)
        );
    }
    cell.output_summary = Some(next);
}

fn agent_live_event_payload_bytes(
    event: &astra_turn_core::agent_live_event::AgentLiveEvent,
) -> usize {
    use astra_turn_core::agent_live_event::AgentLiveEventKind;
    match &event.kind {
        AgentLiveEventKind::OutputDelta(text)
        | AgentLiveEventKind::ThinkingDelta(text)
        | AgentLiveEventKind::Status(text) => text.len(),
        AgentLiveEventKind::ToolStarted {
            name,
            description,
            tool_use_id,
        } => name.len() + description.len() + tool_use_id.len(),
        AgentLiveEventKind::ToolCompleted {
            name,
            description,
            output_summary,
            output,
            tool_use_id,
            ..
        } => {
            name.len()
                + description.len()
                + output_summary.as_deref().map_or(0, str::len)
                + output.as_deref().map_or(0, str::len)
                + tool_use_id.len()
        }
        AgentLiveEventKind::Signal(_) | AgentLiveEventKind::AgentTerminated { .. } => 256,
    }
}

fn fail_agent_cell(
    cell: &mut TaskCell,
    duration_ms: u64,
    parsed: Option<&serde_json::Value>,
    fallback: &str,
) {
    let output_summary =
        crate::tui::agent_control_status::agent_control_result_output_summary(parsed, None);
    let error = crate::tui::agent_control_status::agent_control_error_message(parsed, fallback);
    let elapsed = cell.started_at.elapsed().as_millis() as u64;
    cell.complete(
        "failed",
        elapsed.max(duration_ms),
        output_summary.or_else(|| Some(error.clone())),
        Some(error),
    );
}

fn cell_kind(c: &dyn HistoryCell) -> CellKind {
    let a = c.as_any_ref();
    if a.is::<UserCell>() {
        CellKind::User
    } else if a.is::<AssistantCell>() {
        CellKind::Assistant
    } else if a.is::<ReasoningCell>() {
        CellKind::Reasoning
    } else if a.is::<ToolCell>() {
        CellKind::Tool
    } else if a.is::<SystemCell>() {
        CellKind::System
    } else if a.is::<TurnSummaryCell>() {
        CellKind::TurnSummary
    } else {
        CellKind::Other
    }
}

/// Dispatch a persisted `TurnEvent` to the matching cell builder.
/// Unknown events land as `None` — caller drops them, preserving
/// the "skip, don't crash" contract.
fn cell_from_persist(ev: TurnEvent) -> Option<Box<dyn HistoryCell>> {
    match ev {
        TurnEvent::User { .. } => {
            UserCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::Assistant { .. } => {
            AssistantCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::Thinking { .. } => {
            ReasoningCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::Tool { .. } => {
            ToolCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::System { .. } => {
            SystemCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
        TurnEvent::TurnSummary { .. } => {
            TurnSummaryCell::from_persist(ev).map(|c| Box::new(c) as Box<dyn HistoryCell>)
        }
    }
}

/// `Box<dyn HistoryCell>` → `Arc<dyn HistoryCell>` without
/// re-boxing the payload. Safe because `Arc::from` consumes the
/// box.
fn box_into_arc(b: Box<dyn HistoryCell>) -> Arc<dyn HistoryCell> {
    Arc::from(b)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::agent_run_projection::{
        AgentProjectionConfidence, AgentProjectionSource, AgentRunStatus,
    };
    use crate::tui::history_cell::tool::ToolStatus;

    fn fresh() -> ChatWidget {
        // A local widget has no durable transcript until the runtime commits
        // canonical journal items. This keeps reducer tests filesystem-free.
        ChatWidget::new("")
    }

    fn local_agent_info(
        agent_id: &str,
        status: astra_turn_core::orchestration_types::AgentStatus,
    ) -> astra_turn_core::orchestration_types::SpawnedAgentInfo {
        let ended_at = status.is_terminal().then(std::time::SystemTime::now);
        astra_turn_core::orchestration_types::SpawnedAgentInfo {
            agent_id: agent_id.to_string(),
            run_id: format!("run-{agent_id}"),
            parent_run_id: "root-run".to_string(),
            agent_type: "reviewer".to_string(),
            description: "Review the runtime contract".to_string(),
            status,
            started_at: std::time::SystemTime::now() - std::time::Duration::from_millis(250),
            ended_at,
            metrics: astra_turn_core::orchestration_types::SpawnedAgentMetrics::default(),
            has_permission_issues: false,
            run_in_background: false,
            spawn_tool_call_id: None,
            fanout_slot: None,
        }
    }

    fn local_agent_snapshot(
        agents: Vec<astra_turn_core::orchestration_types::SpawnedAgentInfo>,
    ) -> crate::tui::local_agent_snapshot::LocalAgentSnapshot {
        crate::tui::local_agent_snapshot::LocalAgentSnapshot {
            available: true,
            agents,
            fanout_groups: Vec::new(),
        }
    }

    fn server_run_node(
        run_id: &str,
        status: astra_thin_client::SessionRunLifecycleStatus,
        event_high_watermark: i64,
    ) -> astra_thin_client::SessionRunNode {
        astra_thin_client::SessionRunNode {
            run_id: run_id.into(),
            parent_run_id: Some("root-run".into()),
            root_run_id: Some("root-run".into()),
            depth: 1,
            agent_id: Some("reviewer".into()),
            agent_name: Some("Durable reviewer".into()),
            status,
            waiting_for: None,
            error_code: None,
            error_message: None,
            run_event_high_watermark: event_high_watermark,
            total_tool_calls: 2,
            runtime: astra_thin_client::SessionRunRuntimeFacts {
                runtime_profile: Some("agent_binding_registry".into()),
                model_name: Some("gpt-5".into()),
                agent_binding_id: Some("reviewer-v2".into()),
                ..Default::default()
            },
            available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
            created_at: "2026-07-11T00:00:00Z".into(),
            updated_at: "2026-07-11T00:00:01Z".into(),
        }
    }

    fn server_agent_projection(
        truth_state: crate::tui::server_agent_observer::ServerAgentTruthState,
        runs: Vec<astra_thin_client::SessionRunNode>,
        truncated: bool,
    ) -> crate::tui::server_agent_observer::ServerAgentProjection {
        crate::tui::server_agent_observer::ServerAgentProjection {
            sequence: 1,
            truth_state,
            snapshot: Some(astra_thin_client::SessionRunTreeSnapshot {
                schema_version: astra_thin_client::SESSION_RUN_TREE_SCHEMA_VERSION,
                session_id: "session-1".into(),
                snapshot_revision: "revision-1".into(),
                observed_at: "2026-07-11T00:00:02Z".into(),
                node_limit: 200,
                truncated,
                runs,
            }),
        }
    }

    fn restored_local_agent(
        id: &str,
        status: &str,
    ) -> astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
        astra_services::session_workspace::BackgroundLocalAgentTaskProjection {
            id: id.to_string(),
            run_id: format!("run-{id}"),
            parent_run_id: "root".to_string(),
            status: status.to_string(),
            title: "Restored review".to_string(),
            started_at_ms: 1,
            ended_at_ms: None,
            output_tail: Some("last persisted output".to_string()),
            terminal_reason: None,
            fanout: None,
        }
    }

    fn tool_started(name: &str, description: &str) -> WireEvent {
        WireEvent::ToolStarted {
            name: name.into(),
            description: description.into(),
            tool_use_id: "tu_test".into(),
            parent_tool_use_id: None,
        }
    }

    fn tool_completed(
        name: &str,
        description: &str,
        status: &str,
        duration_ms: u64,
        summary: Option<&str>,
    ) -> WireEvent {
        WireEvent::ToolCompleted {
            name: name.into(),
            description: description.into(),
            status: status.into(),
            duration_ms,
            output_summary: summary.map(str::to_string),
            output: None,
            tool_use_id: "tu_test".into(),
            parent_tool_use_id: None,
        }
    }

    fn agent_control_started(
        action: &str,
        label: &str,
        tool_use_id: &str,
        agent_id: Option<&str>,
    ) -> WireEvent {
        WireEvent::AgentControlStarted {
            action: action.into(),
            label: label.into(),
            tool_use_id: tool_use_id.into(),
            agent_id: agent_id.map(str::to_string),
            fanout_slot: None,
            fanout_title: None,
        }
    }

    fn agent_control_completed(
        action: &str,
        label: &str,
        status: &str,
        duration_ms: u64,
        output: Option<&str>,
        tool_use_id: &str,
        agent_id: Option<&str>,
    ) -> WireEvent {
        WireEvent::AgentControlCompleted {
            action: action.into(),
            label: label.into(),
            status: status.into(),
            duration_ms,
            output: output.map(str::to_string),
            tool_use_id: tool_use_id.into(),
            agent_id: agent_id.map(str::to_string),
        }
    }

    // ── UserSubmit ───────────────────────────────────────────────

    #[test]
    fn user_submit_appends_usercell_and_clears_active() {
        let mut w = fresh();
        // Simulate a prior live assistant cell that never got
        // finalised (e.g. turn got interrupted).
        w.active_cell = Some(Box::new(AssistantCell::new_streaming()));
        w.handle_event(AppEvent::User(UserEvent::Submit("hello".into())));
        assert!(
            w.active_cell.is_none(),
            "UserSubmit must finalise any dangling live cell"
        );
        assert_eq!(w.history.len(), 2, "committed assistant + user");
        // Final entry is the user cell with the exact text.
        let last = w.history.last().unwrap();
        let persisted = last.to_persist().expect("user cell persists");
        assert!(matches!(&persisted, TurnEvent::User { text, .. } if text == "hello"));
    }

    #[test]
    fn commit_applied_user_intent_keeps_live_active_cell() {
        let mut w = fresh();
        w.active_cell = Some(Box::new(AssistantCell::new_streaming()));

        w.commit_applied_user_intent(
            "intent-1",
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::Applied,
            "stop after this tool",
        );

        assert_eq!(w.history.len(), 1, "user intent is committed as history");
        let persisted = w.history[0].to_persist().expect("user cell persists");
        assert!(matches!(
            &persisted,
            TurnEvent::User { text, .. } if text == "stop after this tool"
        ));
        assert_eq!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Assistant),
            "user intent must not finalize the live assistant/tool cell"
        );
    }

    #[test]
    fn active_cell_identity_survives_mid_turn_history_insertion_and_commit() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDelta(
            "visible reasoning".into(),
        )));
        let live_id = w.active_cell_id().expect("live reasoning identity");

        w.commit_applied_user_intent(
            "intent-2",
            astra_turn_types::UserIntentDelivery::GuideCurrentRun,
            astra_turn_types::UserIntentStatus::Applied,
            "new guidance",
        );

        assert_eq!(w.active_cell_id(), Some(live_id));
        assert_ne!(w.history_cell_id(0), live_id);

        w.handle_event(AppEvent::wire(WireEvent::ReasoningDone));

        assert!(w.active_cell().is_none());
        assert_eq!(w.history_cell_id(1), live_id);
        assert_eq!(w.history.len(), w.history_cell_ids.len());
    }

    // ── AnswerDelta ──────────────────────────────────────────────

    #[test]
    fn answer_delta_creates_assistant_then_accumulates() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("Hello ".into())));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("world".into())));
        let cell = w
            .active_cell
            .as_ref()
            .expect("active_cell should be live")
            .as_any_ref()
            .downcast_ref::<AssistantCell>()
            .expect("should be AssistantCell");
        assert_eq!(cell.source(), "Hello world");
        assert!(cell.is_live());
    }

    #[test]
    fn stream_close_freezes_reply_without_emitting_turn_summary() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta(
            "Complete reply".into(),
        )));

        w.finish_stream_projection();

        assert!(w.active_cell().is_none(), "the live projection must close");
        assert_eq!(w.history.len(), 1, "transport close must not add a summary");
        let assistant = w.history[0]
            .as_any_ref()
            .downcast_ref::<AssistantCell>()
            .expect("the reply is committed as an assistant cell");
        assert!(
            !assistant.is_live(),
            "a closed stream cannot retain typing state"
        );
    }

    #[test]
    fn stream_close_does_not_finalize_a_runtime_owned_tool() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent_fanout".into(),
            description: "three reviews".into(),
            tool_use_id: "fanout-call-live".into(),
            parent_tool_use_id: None,
        }));

        w.finish_stream_projection();

        assert!(w.has_live_tool_projection());
        assert!(w.history.is_empty(), "transport close is not tool failure");
        w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent_fanout".into(),
            description: "three reviews".into(),
            status: "completed".into(),
            duration_ms: 6_000,
            output_summary: None,
            output: Some(
                serde_json::json!({
                    "status": "completed",
                    "group_id": "review-group",
                    "target_count": 3,
                })
                .to_string(),
            ),
            tool_use_id: "fanout-call-live".into(),
            parent_tool_use_id: None,
        }));
        assert!(!w.has_live_tool_projection());
        assert_eq!(w.history.len(), 1, "one tool call has one visible cell");
        assert!(matches!(
            w.history[0].to_persist(),
            Some(TurnEvent::Tool {
                status: crate::tui::turn_event::ToolStatus::Success,
                ..
            })
        ));
    }

    #[test]
    fn concurrent_runtime_receipt_does_not_finalize_a_live_tool() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent_fanout".into(),
            description: "three reviews".into(),
            tool_use_id: "fanout-call-receipt".into(),
            parent_tool_use_id: None,
        }));

        w.commit_concurrent_system(SystemCell::runtime_work(
            "Three reviews · 3 agents started · parent waits",
        ));

        assert!(w.has_live_tool_projection());
        assert_eq!(w.history.len(), 1, "only the receipt is committed");
        assert!(matches!(
            w.history[0].to_persist(),
            Some(TurnEvent::System { .. })
        ));
        w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent_fanout".into(),
            description: "three reviews".into(),
            status: "completed".into(),
            duration_ms: 6_000,
            output_summary: None,
            output: Some(
                serde_json::json!({
                    "status": "completed",
                    "group_id": "review-group",
                    "target_count": 3,
                })
                .to_string(),
            ),
            tool_use_id: "fanout-call-receipt".into(),
            parent_tool_use_id: None,
        }));
        assert_eq!(w.history.len(), 2);
        assert!(matches!(
            w.history[1].to_persist(),
            Some(TurnEvent::Tool {
                status: crate::tui::turn_event::ToolStatus::Success,
                ..
            })
        ));
    }

    #[test]
    fn answer_delta_finalises_live_reasoning_cell() {
        let mut w = fresh();
        // Begin a reasoning cell then jump straight to answer —
        // models that don't emit ReasoningDone rely on this
        // transition.
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDelta("thinking".into())));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("answer".into())));
        // Reasoning must be committed before the assistant cell
        // takes over.
        assert_eq!(w.history.len(), 1);
        let reasoning = &w.history[0];
        let ev = reasoning.to_persist().unwrap();
        assert!(matches!(ev, TurnEvent::Thinking { .. }));
        // Active cell is now the Assistant one.
        assert!(
            matches!(
                w.active_cell.as_deref().map(cell_kind),
                Some(CellKind::Assistant)
            ),
            "answer should supplant reasoning"
        );
    }

    #[test]
    fn reasoning_delta_finalises_live_answer_cell() {
        let mut w = fresh();

        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("answer".into())));
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDelta(
            "late reasoning".into(),
        )));

        assert_eq!(w.history.len(), 1);
        assert!(
            matches!(
                w.history[0].as_any_ref().downcast_ref::<AssistantCell>(),
                Some(assistant) if !assistant.is_live()
            ),
            "the answer lane must be committed before reasoning reopens"
        );
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Reasoning)
        ));
    }

    #[test]
    fn answer_delta_waits_for_live_tool_completion_without_false_failure() {
        let mut w = fresh();

        w.handle_event(AppEvent::wire(tool_started("read_file", "Cargo.toml")));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("answer".into())));

        assert!(w.history.is_empty(), "running tool must remain live");
        assert!(matches!(
            w.active_cell
                .as_deref()
                .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>()),
            Some(tool) if tool.status == ToolStatus::Running
        ));
        w.handle_event(AppEvent::wire(tool_completed(
            "read_file",
            "",
            "completed",
            8,
            Some("read 12 lines"),
        )));

        assert_eq!(w.history.len(), 1);
        assert!(matches!(
            w.history[0].as_any_ref().downcast_ref::<ToolCell>(),
            Some(tool) if tool.status == ToolStatus::Success
        ));
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Assistant)
        ));
        let answer = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<AssistantCell>())
            .unwrap();
        assert_eq!(answer.source(), "answer");
    }

    #[test]
    fn interleaved_reasoning_events_replay_in_order_after_tool_completion() {
        let mut w = fresh();

        w.handle_event(AppEvent::wire(tool_started("bash", "cargo metadata")));
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDelta(
            "checking metadata".into(),
        )));
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDone));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta(
            "metadata is valid".into(),
        )));
        w.handle_event(AppEvent::wire(tool_completed(
            "bash",
            "",
            "completed",
            12,
            Some("ok"),
        )));

        assert_eq!(w.history.len(), 2, "tool then reasoning should be settled");
        assert!(matches!(
            w.history[0].as_any_ref().downcast_ref::<ToolCell>(),
            Some(tool) if tool.status == ToolStatus::Success
        ));
        assert!(
            w.history[1]
                .as_any_ref()
                .downcast_ref::<ReasoningCell>()
                .is_some()
        );
        let answer = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<AssistantCell>())
            .unwrap();
        assert_eq!(answer.source(), "metadata is valid");
    }

    // ── Reasoning lifecycle ──────────────────────────────────────

    #[test]
    fn reasoning_done_commits_reasoning_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDelta("step 1".into())));
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDone));
        assert_eq!(w.history.len(), 1, "reasoning cell committed");
        assert!(w.active_cell.is_none(), "active cleared after done");
    }

    #[test]
    fn reasoning_done_without_reasoning_is_noop() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ReasoningDone));
        assert_eq!(w.history.len(), 0);
        assert!(w.active_cell.is_none());
    }

    // ── Tool lifecycle ───────────────────────────────────────────

    #[test]
    fn tool_started_then_completed_commits_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(tool_started("bash", "ls /tmp")));
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ));
        w.handle_event(AppEvent::wire(tool_completed(
            "bash",
            "",
            "completed",
            42,
            Some("3 entries"),
        )));
        assert_eq!(w.history.len(), 1);
        assert!(w.active_cell.is_none());

        let cell = w.history[0]
            .as_any_ref()
            .downcast_ref::<ToolCell>()
            .unwrap();
        assert_eq!(cell.status, ToolStatus::Success);
        assert_eq!(cell.duration_ms, Some(42));
    }

    #[test]
    fn bash_tool_started_carries_ctrl_b_hint_only_when_enabled() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(tool_started("bash", "sleep 60")));
        let cell = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>())
            .unwrap();
        assert!(!cell.ctrl_b_background_hint);

        let mut w = fresh();
        w.set_bash_background_hint_enabled(true);
        w.handle_event(AppEvent::wire(tool_started("bash", "sleep 60")));
        let cell = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>())
            .unwrap();
        assert!(cell.ctrl_b_background_hint);
    }

    #[test]
    fn bash_ctrl_b_hint_updates_existing_active_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(tool_started("bash", "sleep 60")));

        w.set_bash_background_hint_enabled(true);
        let cell = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>())
            .unwrap();
        assert!(cell.ctrl_b_background_hint);

        w.set_bash_background_hint_enabled(false);
        let cell = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>())
            .unwrap();
        assert!(!cell.ctrl_b_background_hint);
    }

    #[test]
    fn non_bash_tool_started_ignores_ctrl_b_hint_capability() {
        let mut w = fresh();
        w.set_bash_background_hint_enabled(true);
        w.handle_event(AppEvent::wire(tool_started("read_file", "src/main.rs")));
        let cell = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>())
            .unwrap();
        assert!(!cell.ctrl_b_background_hint);
    }

    #[test]
    fn agent_spawn_task_started_carries_ctrl_b_hint_but_get_result_does_not() {
        let mut spawn = fresh();
        spawn.handle_event(AppEvent::wire(agent_control_started(
            "spawn", "reviewer", "tu_test", None,
        )));
        spawn.handle_event(AppEvent::wire(tool_started("agent", "reviewer")));
        let spawn_cell = spawn
            .live_task_cell("tu_test")
            .expect("agent spawn should render as a live TaskCell");
        assert!(spawn_cell.ctrl_b_background_hint);

        let mut get_result = fresh();
        get_result.handle_event(AppEvent::wire(agent_control_started(
            "get_result",
            "reviewer",
            "tu_test",
            Some("reviewer@abc"),
        )));
        get_result.handle_event(AppEvent::wire(tool_started("agent", "reviewer")));
        let get_result_cell = get_result
            .live_task_cell("tu_test")
            .expect("agent get_result should render as a live TaskCell");
        assert!(!get_result_cell.ctrl_b_background_hint);
    }

    #[test]
    fn unpaired_tool_completed_synthesises_cell() {
        // A bare ToolCompleted (no preceding ToolStarted) still
        // yields a committed cell. Defensive: journals can
        // sometimes replay events out of order.
        let mut w = fresh();
        w.handle_event(AppEvent::wire(tool_completed(
            "bash",
            "echo hi",
            "completed",
            10,
            None,
        )));
        assert_eq!(w.history.len(), 1);
    }

    // ── Task tool + child routing (P1 of TUI/task UX) ────────────

    fn task_started(tool_use_id: &str, description: &str) -> WireEvent {
        WireEvent::ToolStarted {
            name: "agent".into(),
            description: description.into(),
            tool_use_id: tool_use_id.into(),
            parent_tool_use_id: None,
        }
    }

    fn task_completed(tool_use_id: &str, status: &str, duration_ms: u64) -> WireEvent {
        WireEvent::ToolCompleted {
            name: "agent".into(),
            description: String::new(),
            status: status.into(),
            duration_ms,
            output_summary: None,
            output: None,
            tool_use_id: tool_use_id.into(),
            parent_tool_use_id: None,
        }
    }

    fn child_started(parent: &str, id: &str, name: &str, description: &str) -> WireEvent {
        WireEvent::ToolStarted {
            name: name.into(),
            description: description.into(),
            tool_use_id: id.into(),
            parent_tool_use_id: Some(parent.into()),
        }
    }

    fn child_completed(parent: &str, id: &str, status: &str, duration_ms: u64) -> WireEvent {
        WireEvent::ToolCompleted {
            name: "bash".into(),
            description: String::new(),
            status: status.into(),
            duration_ms,
            output_summary: None,
            output: None,
            tool_use_id: id.into(),
            parent_tool_use_id: Some(parent.into()),
        }
    }

    use crate::tui::history_cell::task::{ChildStatus, TaskCell, TaskStatus};

    #[test]
    fn task_started_creates_taskcell_in_live_slot() {
        // Post multi-slot rework: a Task tool no longer occupies
        // `active_cell` (reserved for non-Task live cells). It goes
        // into `live_tasks` keyed by `tool_use_id`. Verifies the
        // multi-slot register accepts new agents.
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_parent", "audit cache")));
        let tc = w
            .live_task_cell("tu_parent")
            .expect("task tool must materialise into the live_tasks register");
        assert_eq!(tc.tool_use_id, "tu_parent");
        assert_eq!(tc.description, "audit cache");
        assert_eq!(tc.status, TaskStatus::Running);
    }

    #[test]
    fn child_tool_started_routes_under_parent_taskcell() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_parent", "run things")));
        w.handle_event(AppEvent::wire(child_started(
            "tu_parent",
            "tu_child_1",
            "bash",
            "ls",
        )));
        // The child must land inside the live TaskCell, NOT as a
        // top-level ToolCell that would reorder scrollback.
        assert_eq!(
            w.history.len(),
            0,
            "child event should not commit an extra top-level cell"
        );
        let tc = w
            .live_task_cell("tu_parent")
            .expect("parent should still be live");
        assert_eq!(tc.children.len(), 1);
        assert_eq!(tc.children[0].tool_use_id, "tu_child_1");
        assert_eq!(tc.children[0].name, "bash");
    }

    #[test]
    fn child_tool_completed_flips_child_status_inside_taskcell() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_parent", "run")));
        w.handle_event(AppEvent::wire(child_started(
            "tu_parent",
            "tu_child",
            "bash",
            "ls",
        )));
        w.handle_event(AppEvent::wire(child_completed(
            "tu_parent",
            "tu_child",
            "completed",
            50,
        )));
        let tc = w
            .live_task_cell("tu_parent")
            .expect("parent should still be live");
        assert_eq!(tc.children[0].status, ChildStatus::Success);
        assert_eq!(tc.children[0].duration_ms, Some(50));
    }

    #[test]
    fn child_event_with_unknown_parent_falls_back_to_top_level() {
        // The parent lookup missed (parent cell isn't live) — the
        // child must still appear somewhere so the user sees
        // activity instead of silent drop.
        let mut w = fresh();
        w.handle_event(AppEvent::wire(child_started(
            "tu_missing_parent",
            "tu_orphan",
            "bash",
            "ls",
        )));
        assert!(
            matches!(
                w.active_cell.as_deref().map(cell_kind),
                Some(CellKind::Tool)
            ),
            "orphan child must render as a top-level ToolCell fallback"
        );
    }

    #[test]
    fn task_completed_transitions_taskcell_and_commits() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_parent", "run")));
        w.handle_event(AppEvent::wire(child_started(
            "tu_parent",
            "tu_child",
            "bash",
            "ls",
        )));
        w.handle_event(AppEvent::wire(child_completed(
            "tu_parent",
            "tu_child",
            "completed",
            10,
        )));
        w.handle_event(AppEvent::wire(task_completed(
            "tu_parent",
            "completed",
            100,
        )));
        assert!(w.active_cell.is_none(), "task completion commits the cell");
        assert_eq!(w.history.len(), 1);
        let tc = w.history[0]
            .as_any_ref()
            .downcast_ref::<TaskCell>()
            .expect("committed cell must be the TaskCell");
        assert_eq!(tc.status, TaskStatus::Completed);
        assert_eq!(tc.children.len(), 1);
        assert_eq!(tc.children[0].status, ChildStatus::Success);
    }

    // ── In-flight task tracking for cancel propagation ───────────

    #[test]
    fn agent_tool_started_registers_in_flight_task_id() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::wire(task_started("tu_2", "second")));
        // Two starts, neither completed — both ids must be
        // addressable so a Ctrl+C cascade can cancel each.
        assert_eq!(
            w.in_flight_task_ids(),
            &["tu_1".to_string(), "tu_2".to_string()],
            "both in-flight task ids must be tracked in insertion order"
        );
    }

    /// REGRESSION (uncommitted-changes review of commits 7db185056 +
    /// 0d65ad005): `name == "agent"` `ToolStarted` was being
    /// diverted early into `on_agent_control_started` only — that
    /// populates the `agent_runs` registry but skips
    /// `in_flight_task_ids` and `live_tasks`. Result: `event_loop`'s
    /// Ctrl+C cancel-fanout (which reads `in_flight_task_ids()` to
    /// issue cancel RPCs against the durable task store) silently
    /// found an empty list and didn't cancel any running sub-agent.
    /// The user pressed Ctrl+C and watched the spawned children
    /// keep running.
    ///
    /// This test pins the contract end-to-end: `ToolStarted` for
    /// `name == "agent"` MUST register its `tool_use_id` in
    /// `in_flight_task_ids` so cancel-fanout can find it.
    #[test]
    fn agent_spawn_tool_started_must_be_visible_to_cancel_fanout() {
        let mut w = fresh();
        // Three parallel spawns, like a typical multi-angle review
        // turn. None has completed yet.
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "reviewer-A",
            "spawn-a",
            Some("reviewer-A@a"),
        )));
        w.handle_event(AppEvent::wire(task_started("spawn-a", "reviewer-A")));
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "reviewer-B",
            "spawn-b",
            Some("reviewer-B@b"),
        )));
        w.handle_event(AppEvent::wire(task_started("spawn-b", "reviewer-B")));
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "reviewer-C",
            "spawn-c",
            Some("reviewer-C@c"),
        )));
        w.handle_event(AppEvent::wire(task_started("spawn-c", "reviewer-C")));

        let in_flight = w.in_flight_task_ids().to_vec();
        assert_eq!(
            in_flight.len(),
            3,
            "all 3 in-flight `agent` tool calls must be registered for \
             cancel-fanout (Ctrl+C reads this list); got {in_flight:?}"
        );
        for expected in ["spawn-a", "spawn-b", "spawn-c"] {
            assert!(
                in_flight.iter().any(|s| s == expected),
                "missing {expected}: {in_flight:?}"
            );
        }

        // The agent_runs registry (logical row UI) should ALSO have
        // entries — they are independent populations of data that
        // serve different concerns (cancel vs display). Both must be
        // populated by the same ToolStarted event.
        assert!(
            !w.agent_run_ids().is_empty(),
            "agent_runs registry must mirror live agents so the multi_agent \
             strip and Ctrl+G drilldown surface the same activity"
        );
    }

    #[test]
    fn agent_tool_completed_removes_task_id_from_in_flight() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::wire(task_started("tu_2", "second")));
        // Completing tu_1 leaves only tu_2 pending.
        w.handle_event(AppEvent::wire(task_completed("tu_1", "completed", 50)));
        assert_eq!(w.in_flight_task_ids(), &["tu_2".to_string()]);
    }

    #[test]
    fn turn_complete_clears_in_flight_task_set() {
        // Defensive: even if a task completion event is lost,
        // turn_complete MUST reset the set so the next turn's
        // Ctrl+C doesn't target a stale id from the previous
        // conversation.
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));
        assert!(
            w.in_flight_task_ids().is_empty(),
            "turn boundary must reset cancel bookkeeping: {:?}",
            w.in_flight_task_ids()
        );
    }

    #[test]
    fn turn_complete_preserves_explicit_terminal_agent_projection() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"cancelled","agent_id":"reviewer-A@abc"}"#.into()),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));
        assert_eq!(w.agent_run_ids(), vec!["reviewer-A@abc".to_string()]);
        assert_eq!(
            w.agent_run_state("reviewer-A@abc").unwrap().status,
            AgentRunStatus::Cancelled
        );

        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));

        assert_eq!(w.agent_run_ids(), vec!["reviewer-A@abc".to_string()]);
        assert_eq!(
            w.agent_run_state("reviewer-A@abc").unwrap(),
            AgentRunState::observed(AgentRunStatus::Cancelled),
            "a parent turn boundary must not erase or reinterpret an explicit child terminal event"
        );
    }

    #[test]
    fn later_completed_result_reconciles_cancelled_projection() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"cancelled","agent_id":"reviewer-A@abc"}"#.into()),
            tool_use_id: "spawn-tu-legacy".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));
        assert_eq!(
            w.agent_run_state("reviewer-A@abc").unwrap().status,
            AgentRunStatus::Cancelled
        );

        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "get_result".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 42,
            output: Some(r#"{"agent_id":"reviewer-A@abc","result":"done"}"#.into()),
            tool_use_id: "result-tu-legacy".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));

        assert_eq!(
            w.agent_run_state("reviewer-A@abc").unwrap().status,
            AgentRunStatus::Completed
        );
        let detail = w.task_cell_anywhere("reviewer-A@abc").unwrap();
        assert!(matches!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Completed
        ));
    }

    #[test]
    fn explain_report_with_content_fallback_commits_system_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ExplainReport(vec![
            serde_json::json!(
                {
                    "type": "explain",
                    "content": "why this happened"
                }
            ),
        ])));

        let sys = w
            .history
            .last()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<SystemCell>())
            .expect("explain report should append a system cell");
        assert!(
            sys.message().contains("why this happened"),
            "content fallback should render the explain text: {}",
            sys.message()
        );
    }

    #[test]
    fn duplicate_agent_tool_started_does_not_double_register() {
        // Replay path: a ToolStarted fired twice for the same id
        // must not inflate the cancel list (otherwise Ctrl+C
        // would cancel twice and risk cancelling the retry).
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::wire(task_started("tu_1", "first")));
        assert_eq!(w.in_flight_task_ids(), &["tu_1".to_string()]);
    }

    /// CRITICAL: a single Ctrl+C must take ALL live ids out of the
    /// in-flight set so a follow-up press doesn't re-target the same
    /// tasks. Pre-fix users saw "Stopped 1 local agent." print
    /// six times for one Ctrl+C burst — every press kept finding the
    /// same ids and the durable task service rejected the
    /// already-cancelled ones, so only one new acked-success per
    /// press counted.
    #[test]
    fn mark_control_tasks_cancelling_drains_in_flight() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::wire(task_started("tu_2", "second")));
        w.handle_event(AppEvent::wire(task_started("tu_3", "third")));
        assert_eq!(w.in_flight_task_ids().len(), 3);

        let ids = w.in_flight_task_ids().to_vec();
        w.mark_control_tasks_cancelling(&ids);

        assert!(
            w.in_flight_task_ids().is_empty(),
            "after marking cancelling, those ids must NOT appear in the \
             in-flight set or a follow-up Ctrl+C would re-cancel them; got {:?}",
            w.in_flight_task_ids()
        );
        // Cancelling state itself is preserved so the strip can show
        // a "Cancelling…" badge until the worker acks.
        assert!(w.cancelling_task_ids.contains("tu_1"));
        assert!(w.cancelling_task_ids.contains("tu_2"));
        assert!(w.cancelling_task_ids.contains("tu_3"));
    }

    // ── Cancel banner after Ctrl+C fan-out ───────────────────────

    #[test]
    fn cancel_banner_commits_system_cell_when_count_positive() {
        let mut w = fresh();
        w.commit_cancel_banner(2);
        assert_eq!(w.history.len(), 1);
        let sys = w.history[0]
            .as_any_ref()
            .downcast_ref::<SystemCell>()
            .expect("cancel banner must be a SystemCell");
        assert!(
            sys.message().contains("Stopped 2 local agents"),
            "banner must name the plural count: {}",
            sys.message()
        );
    }

    #[test]
    fn cancel_banner_uses_singular_copy_when_count_is_one() {
        let mut w = fresh();
        w.commit_cancel_banner(1);
        let sys = w.history[0]
            .as_any_ref()
            .downcast_ref::<SystemCell>()
            .unwrap();
        assert!(
            sys.message().contains("Stopped 1 local agent."),
            "singular copy required: {}",
            sys.message()
        );
    }

    #[test]
    fn resume_summary_commits_info_cell_with_message() {
        let mut w = fresh();
        w.commit_resume_summary(
            "While you were away: 3 background shells finished (2 ok, 1 failed).".into(),
        );
        assert_eq!(w.history.len(), 1);
        let sys = w.history[0]
            .as_any_ref()
            .downcast_ref::<SystemCell>()
            .expect("resume summary must be a SystemCell");
        assert!(sys.message().contains("While you were away"));
        assert!(sys.message().contains("3 background shells"));
    }

    #[test]
    fn resume_summary_noop_on_empty_message() {
        // Empty string = nothing finished since last_seen_at. Don't
        // push an empty SystemCell that would render as a bare blank
        // line at the top of scrollback.
        let mut w = fresh();
        w.commit_resume_summary(String::new());
        assert!(w.history.is_empty());
    }

    #[test]
    fn cancel_banner_noop_when_count_is_zero() {
        // A bare Ctrl+C with no live sub-agents must NOT add
        // scrollback noise — the interrupt is already visible via
        // rustyline-style feedback in the footer.
        let mut w = fresh();
        w.commit_cancel_banner(0);
        assert!(
            w.history.is_empty(),
            "no banner for zero-cancel Ctrl+C: {:?}",
            w.history.len()
        );
    }

    // ── Turn lifecycle ───────────────────────────────────────────

    #[test]
    fn turn_complete_emits_summary() {
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("hi".into())));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("answer".into())));
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::new(
            TurnStats {
                elapsed_ms: Some(1_500),
                tokens_in: Some(50),
                tokens_out: Some(10),
                tools: 0,
                ..Default::default()
            },
        ))));
        assert!(w.active_cell.is_none());
        // Expect: user cell + assistant cell + summary cell.
        assert_eq!(w.history.len(), 3);
        assert!(
            w.history
                .last()
                .unwrap()
                .as_any_ref()
                .downcast_ref::<TurnSummaryCell>()
                .is_some()
        );
    }

    #[test]
    fn turn_error_commits_system_error_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("hi".into())));
        w.handle_event(AppEvent::wire(WireEvent::TurnError(
            "<error>rate limited</error>".into(),
        )));
        assert_eq!(w.history.len(), 2);
        let err = w
            .history
            .last()
            .unwrap()
            .as_any_ref()
            .downcast_ref::<SystemCell>()
            .expect("last cell should be SystemCell");
        // Humanisation strips the tag.
        assert_eq!(err.message(), "rate limited");
    }

    // ── Invariant: at most one live cell ─────────────────────────

    #[test]
    fn tool_started_mid_stream_commits_assistant_first() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("first half ".into())));
        w.handle_event(AppEvent::wire(tool_started("bash", "ls")));
        // Assistant should have been committed before the tool
        // took the active slot. Two cells in history: partial
        // assistant + (nothing else yet).
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ));
        assert_eq!(w.history.len(), 1);
        let ev = w.history[0].to_persist().unwrap();
        assert!(matches!(ev, TurnEvent::Assistant { .. }));
    }

    // ── Watermark / flush tracking ──────────────────────────────

    #[test]
    fn drain_new_committed_returns_only_unflushed_cells() {
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("a".into())));
        w.handle_event(AppEvent::User(UserEvent::Submit("b".into())));
        // First drain returns both new cells.
        let first = w.drain_new_committed();
        assert_eq!(first.len(), 2, "first drain covers all so far");

        // Second drain returns nothing new.
        let second = w.drain_new_committed();
        assert!(second.is_empty(), "no new cells since first drain");

        // After another commit, only the delta.
        w.handle_event(AppEvent::User(UserEvent::Submit("c".into())));
        let third = w.drain_new_committed();
        assert_eq!(third.len(), 1);
    }

    #[test]
    fn mark_all_flushed_suppresses_existing_cells() {
        // Used by resume: after loading history we don't want to
        // reflush it into the terminal, the caller paints it once
        // and advances the watermark.
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("existing".into())));
        w.mark_all_flushed();
        let out = w.drain_new_committed();
        assert!(out.is_empty(), "marked-flushed cells must not redraw");

        // New cells after the mark still surface.
        w.handle_event(AppEvent::User(UserEvent::Submit("new".into())));
        let out = w.drain_new_committed();
        assert_eq!(out.len(), 1);
    }

    #[serial_test::serial]
    #[test]
    fn set_session_id_swaps_without_losing_history() {
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("before".into())));
        assert_eq!(w.history().len(), 1);
        w.set_session_id("new-sid");
        assert_eq!(w.session_id(), "new-sid");
        assert_eq!(w.history().len(), 1, "history survives sid swap");
    }

    #[test]
    fn ephemeral_warning_is_visible_without_becoming_canonical_history() {
        let mut widget = ChatWidget::new("");
        widget.handle_event(AppEvent::User(UserEvent::Submit("pending identity".into())));

        widget.commit_ephemeral_warning("history write failed");

        assert_eq!(widget.history().len(), 2);
        assert!(matches!(
            widget.history()[0].to_persist(),
            Some(TurnEvent::User { .. })
        ));
        assert!(widget.history()[1].to_persist().is_none());
        assert_eq!(widget.history.len(), widget.history_cell_ids.len());
    }

    // ── Last user text lookup (Ctrl+R edit-last) ────────────────

    #[test]
    fn last_user_text_walks_back_past_trailing_cells() {
        // History ends with non-User cells (assistant + summary);
        // lookup must still surface the most recent user message.
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("first".into())));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("reply 1".into())));
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));
        w.handle_event(AppEvent::User(UserEvent::Submit("second".into())));
        w.handle_event(AppEvent::wire(WireEvent::AnswerDelta("reply 2".into())));
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));

        assert_eq!(w.last_user_text().as_deref(), Some("second"));
    }

    #[test]
    fn last_user_text_none_on_empty_history() {
        let w = fresh();
        assert!(w.last_user_text().is_none());
    }

    // ── Replay ──────────────────────────────────────────────────

    #[test]
    fn replay_reconstructs_history_in_order() {
        let mut w = fresh();
        let events = vec![
            TurnEvent::User {
                ts: None,
                text: "hi".into(),
            },
            TurnEvent::Assistant {
                ts: None,
                markdown: "hello".into(),
            },
            TurnEvent::TurnSummary {
                ts: None,
                elapsed_ms: Some(100),
                ttft_ms: None,
                tokens_in: Some(10),
                tokens_out: Some(5),
                cache_read_tokens: None,
                tools: 0,
                cumulative_tokens: Some(15),
                cumulative_cost_usd: None,
            },
        ];
        w.replay(events);
        assert_eq!(w.history.len(), 3);
        assert!(
            w.history[0].as_any_ref().is::<UserCell>(),
            "first should be User"
        );
        assert!(
            w.history[1].as_any_ref().is::<AssistantCell>(),
            "second should be Assistant"
        );
        assert!(
            w.history[2].as_any_ref().is::<TurnSummaryCell>(),
            "third should be TurnSummary"
        );
    }

    // ── Multi-agent parallel TaskCells ──────────────────────────────
    //
    // RED tests for the multi-agent parallel UI rework. Today the
    // ChatWidget has a single `active_cell` slot, so spawning a
    // second parallel agent commits the first to scrollback and
    // child events for the first parent then route to history (via
    // `route_child_started`'s false-return), losing the visual link
    // between parent and child for all agents except the most-
    // recently-started one. These tests pin the desired post-rework
    // contract.

    /// Two parallel agent spawn calls (reference-agent pattern: single
    /// assistant turn, multiple Agent tool uses) must produce TWO
    /// live TaskCells, not one + one-committed-to-scrollback.
    #[test]
    fn two_parallel_task_tools_keep_both_live() {
        let mut w = fresh();
        // Spawn agent A
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "review module X".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        // Spawn agent B (parallel, before A completes)
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "review module Y".into(),
            tool_use_id: "agent-B".into(),
            parent_tool_use_id: None,
        }));

        // Both must be live. New API:
        let live_ids = w.live_task_ids();
        assert!(
            live_ids.contains(&"agent-A".to_string()),
            "agent-A must be live, got {live_ids:?}"
        );
        assert!(
            live_ids.contains(&"agent-B".to_string()),
            "agent-B must be live, got {live_ids:?}"
        );
        assert_eq!(live_ids.len(), 2, "exactly 2 live agents: {live_ids:?}");

        // Neither should have been committed to scrollback yet.
        for cell in w.history() {
            assert!(
                cell.as_any_ref().downcast_ref::<TaskCell>().is_none(),
                "no TaskCell should be in history while parents still live"
            );
        }
    }

    /// Children of agent A arriving while agent B is the most-recently-
    /// spawned must still attach to A — not render top-level or attach
    /// to B.
    #[test]
    fn child_events_route_to_correct_parent_when_multiple_live() {
        let mut w = fresh();
        // Spawn A and B
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "A".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "B".into(),
            tool_use_id: "agent-B".into(),
            parent_tool_use_id: None,
        }));
        // Child belongs to A
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "read_file".into(),
            description: "src/foo.rs".into(),
            tool_use_id: "child-A1".into(),
            parent_tool_use_id: Some("agent-A".into()),
        }));

        // The child must show up under A, NOT under B, NOT top-level.
        let cell_a = w.live_task_cell("agent-A").expect("A still live");
        assert_eq!(
            cell_a.children.len(),
            1,
            "A must have 1 child, got {}",
            cell_a.children.len()
        );
        assert_eq!(cell_a.children[0].tool_use_id, "child-A1");

        let cell_b = w.live_task_cell("agent-B").expect("B still live");
        assert_eq!(cell_b.children.len(), 0, "B must have 0 children");
    }

    /// When agent A completes while B is still running, only A is
    /// committed to scrollback. B continues live with no disruption.
    #[test]
    fn completing_one_parallel_agent_leaves_others_live() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "A".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "B".into(),
            tool_use_id: "agent-B".into(),
            parent_tool_use_id: None,
        }));
        // Complete A
        w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "A".into(),
            status: "completed".into(),
            duration_ms: 100,
            output_summary: Some("done".into()),
            output: None,
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));

        // A is gone from live, in history. B still live.
        let live = w.live_task_ids();
        assert_eq!(live, vec!["agent-B".to_string()], "only B live: {live:?}");
        let task_cells_in_history: Vec<&TaskCell> = w
            .history()
            .iter()
            .filter_map(|c| c.as_any_ref().downcast_ref::<TaskCell>())
            .collect();
        assert_eq!(
            task_cells_in_history.len(),
            1,
            "exactly A should be in history"
        );
    }

    /// Once all parallel agents complete, history holds them all in
    /// completion order. No ghost live entries.
    #[test]
    fn all_parallel_agents_complete_into_history() {
        let mut w = fresh();
        for id in ["agent-A", "agent-B", "agent-C"] {
            w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
                name: "agent".into(),
                description: id.into(),
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }
        for id in ["agent-B", "agent-A", "agent-C"] {
            // out-of-order completions
            w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
                name: "agent".into(),
                description: id.into(),
                status: "completed".into(),
                duration_ms: 50,
                output_summary: None,
                output: None,
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }
        assert_eq!(w.live_task_ids().len(), 0, "no live agents left");
        let task_cells: Vec<&TaskCell> = w
            .history()
            .iter()
            .filter_map(|c| c.as_any_ref().downcast_ref::<TaskCell>())
            .collect();
        assert_eq!(task_cells.len(), 3, "all 3 in history");
    }

    /// Cancel propagation (Ctrl+C) sees every parallel agent in
    /// `in_flight_task_ids`, not just the most recent.
    #[test]
    fn in_flight_task_ids_includes_all_parallel_agents() {
        let mut w = fresh();
        for id in ["agent-A", "agent-B", "agent-C"] {
            w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
                name: "agent".into(),
                description: id.into(),
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }
        let ids = w.in_flight_task_ids();
        assert_eq!(ids.len(), 3, "all 3 in flight: {ids:?}");
        for id in ["agent-A", "agent-B", "agent-C"] {
            assert!(ids.iter().any(|s| s == id), "{id} missing: {ids:?}");
        }
    }

    /// CRITICAL regression: a new user submit must drain any live
    /// parallel agents from the previous turn. Otherwise an
    /// abnormally-ended turn (server stream drop with no
    /// TurnComplete) leaks live_tasks across turns and
    /// `on_turn_complete` of the NEXT turn snapshots them as though
    /// they belonged to that turn.
    #[test]
    fn user_submit_drains_live_tasks_from_prior_turn() {
        let mut w = fresh();
        // Spawn 2 parallel agents in the implicit prior turn.
        for id in ["agent-A", "agent-B"] {
            w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
                name: "agent".into(),
                description: id.into(),
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }
        // Server drops without TurnComplete. User types a new message.
        w.handle_event(AppEvent::User(UserEvent::Submit("next request".into())));
        // Live register must be empty; the orphan agents are now in
        // history (finalized as Failed via TaskCell::finalize()).
        assert_eq!(
            w.live_task_ids().len(),
            0,
            "user submit must drain live_tasks: {:?}",
            w.live_task_ids()
        );
        assert_eq!(
            w.in_flight_task_ids().len(),
            0,
            "user submit must clear in_flight_task_ids: {:?}",
            w.in_flight_task_ids()
        );
        // Both orphan agents should appear in history as finalized
        // TaskCells (Failed status from finalize()).
        let task_cells_in_history: Vec<&TaskCell> = w
            .history()
            .iter()
            .filter_map(|c| c.as_any_ref().downcast_ref::<TaskCell>())
            .collect();
        assert_eq!(
            task_cells_in_history.len(),
            2,
            "orphan agents must end up in history, got {}",
            task_cells_in_history.len()
        );
    }

    #[test]
    fn user_submit_preserves_terminal_agent_runs_with_bounded_registry() {
        let mut w = fresh();
        // Turn 1: spawn two agents, both finish.
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"completed","agent_id":"reviewer-A@abc"}"#.into()),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-B".into(),
            tool_use_id: "spawn-tu-2".into(),
            agent_id: Some("reviewer-B@def".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-B".into(),
            status: "failed".into(),
            duration_ms: 5,
            output: Some(r#"{"status":"failed","agent_id":"reviewer-B@def"}"#.into()),
            tool_use_id: "spawn-tu-2".into(),
            agent_id: Some("reviewer-B@def".into()),
        }));
        assert_eq!(
            w.agent_run_ids().len(),
            2,
            "both completed agents present at end of prior turn"
        );

        // Starting the next parent turn does not change child outcomes.
        w.handle_event(AppEvent::User(UserEvent::Submit("next".into())));
        assert_eq!(
            w.agent_run_ids().len(),
            2,
            "terminal child projections remain inspectable across parent turns: {:?}",
            w.agent_run_ids()
        );
    }

    #[test]
    fn user_submit_degrades_live_agent_to_unconfirmed_without_erasing_it() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "stuck-agent".into(),
            tool_use_id: "spawn-tu-x".into(),
            agent_id: Some("stuck@id".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        // No AgentControlCompleted — the stream dropped.
        assert_eq!(w.agent_run_ids().len(), 1, "agent is live");

        w.handle_event(AppEvent::User(UserEvent::Submit("next".into())));
        assert_eq!(w.agent_run_ids(), vec!["stuck@id".to_string()]);
        let state = w.agent_run_state("stuck@id").unwrap();
        assert_eq!(state.status, AgentRunStatus::Starting);
        assert_eq!(
            state.confidence,
            AgentProjectionConfidence::Unconfirmed,
            "a missing terminal event is uncertainty, not completion or failure"
        );
    }

    /// HIGH regression: a duplicate `ToolStarted` for an existing
    /// live task should update the description (in case the model
    /// emits the started event with extra detail on retry/replay),
    /// not silently drop it. This pins the documented "first wins,
    /// additional events update description" policy.
    #[test]
    fn duplicate_tool_started_updates_description() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "initial".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        // Same id, different description (e.g. server retry with
        // a more detailed task description).
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "refined description".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        // Still exactly one live entry — not duplicated into history.
        assert_eq!(w.live_task_ids().len(), 1);
        let tc = w.live_task_cell("agent-A").unwrap();
        // The description was updated, not discarded.
        assert_eq!(
            tc.description, "refined description",
            "duplicate ToolStarted should update description"
        );
    }

    /// HIGH regression: a parent `ToolCompleted` arriving AFTER
    /// `on_turn_complete` already drained the live_tasks must NOT
    /// synthesize a duplicate ToolCell or otherwise misrender. The
    /// in_flight_task_ids was cleared, the live cell was finalized
    /// and committed; the late ToolCompleted is a no-op.
    #[test]
    fn late_tool_completed_after_turn_complete_is_noop() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "agent-A".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        let history_len_before_turn_complete = w.history().len();
        // Turn ends — agent-A finalized as Failed and committed.
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::new(
            TurnStats {
                elapsed_ms: None,
                ttft_ms: None,
                tokens_in: None,
                tokens_out: None,
                cache_read_tokens: None,
                tools: 0,
                cumulative_tokens: None,
                cumulative_cost_usd: None,
            },
        ))));
        let history_len_after_turn_complete = w.history().len();
        assert!(
            history_len_after_turn_complete > history_len_before_turn_complete,
            "agent-A and turn summary should land in history"
        );

        // Late ToolCompleted for agent-A — must be a no-op, not
        // synthesize a new cell.
        w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "agent-A".into(),
            status: "completed".into(),
            duration_ms: 100,
            output_summary: Some("done".into()),
            output: None,
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));

        let history_len_after_late_event = w.history().len();
        assert_eq!(
            history_len_after_late_event, history_len_after_turn_complete,
            "late ToolCompleted must NOT add a new cell to history"
        );
    }

    #[test]
    fn late_agent_control_completed_reconciles_retained_projection() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-late".into(),
            agent_id: Some("reviewer-A@late".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));
        assert_eq!(
            w.agent_run_state("reviewer-A@late").unwrap().confidence,
            AgentProjectionConfidence::Unconfirmed
        );

        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"cancelled","agent_id":"reviewer-A@late"}"#.into()),
            tool_use_id: "spawn-tu-late".into(),
            agent_id: Some("reviewer-A@late".into()),
        }));

        assert_eq!(w.agent_run_ids(), vec!["reviewer-A@late".to_string()]);
        assert_eq!(
            w.agent_run_state("reviewer-A@late").unwrap().status,
            AgentRunStatus::Cancelled
        );
    }

    #[test]
    fn agent_live_gap_marks_projection_incomplete_without_fabricating_output() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLiveGap(
            astra_turn_core::agent_live_event::AgentLiveGap {
                run_id: "run-reviewer".into(),
                agent_id: "reviewer".into(),
                dropped_event_count: 2,
            },
        )));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Running);
        assert_eq!(rows[0].run_id.as_deref(), Some("run-reviewer"));
        assert_eq!(
            rows[0].attention_summary.as_deref(),
            Some("Live activity incomplete · 2 updates skipped · syncing durable state")
        );
        assert!(
            w.history().is_empty(),
            "a transport gap must not become a synthetic conversation message"
        );
    }

    #[test]
    fn late_agent_terminated_reconciles_unconfirmed_projection() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@late-term".into(),
            kind: AgentLiveEventKind::OutputDelta("running".into()),
        })));
        w.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));
        assert_eq!(
            w.agent_run_state("reviewer@late-term").unwrap().confidence,
            AgentProjectionConfidence::Unconfirmed
        );

        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@late-term".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Cancelled,
                duration_ms: 20,
                reason: Some("late termination".into()),
            },
        })));

        assert_eq!(
            w.agent_run_state("reviewer@late-term").unwrap().status,
            AgentRunStatus::Cancelled
        );
    }

    /// REGRESSION (session 2a98814b): pressing Ctrl+G after sub-agents
    /// have completed used to silently no-op. The user lost any way to
    /// drill into the agents' output. Now `agent_monitor_snapshot`
    /// returns the most-recent completed Task cells too.
    #[test]
    fn agents_drilldown_includes_recent_completed_after_strip_dismissed() {
        let mut w = fresh();
        // Spawn 3 parallel agents, complete all of them.
        for id in ["agent-A", "agent-B", "agent-C"] {
            let tool_use_id = format!("spawn-{id}");
            w.handle_event(AppEvent::wire(agent_control_started(
                "spawn",
                id,
                &tool_use_id,
                Some(id),
            )));
            let output = serde_json::json!({
                "status": "completed",
                "agent_id": id,
                "result": format!("done-{id}")
            })
            .to_string();
            w.handle_event(AppEvent::wire(agent_control_completed(
                "spawn",
                id,
                "completed",
                12_000,
                Some(&output),
                &tool_use_id,
                Some(id),
            )));
        }

        // Live strip is empty (all completed) — the pre-fix Ctrl+G path.
        assert_eq!(
            w.live_task_ids().len(),
            0,
            "all agents completed, live strip should be empty"
        );

        // New behavior: drilldown rows include the recent completions.
        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(
            rows.len(),
            3,
            "Ctrl+G must surface the 3 completed agents so the user \
             can still drill into their output"
        );
        // All 3 should be flagged as not-live so the view renders the
        // ✓ icon instead of the spinner.
        assert!(
            rows.iter()
                .all(|r| r.state.status == AgentRunStatus::Completed),
            "completed-only rows must report completed status"
        );
        let ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
        for id in ["agent-A", "agent-B", "agent-C"] {
            assert!(
                ids.contains(&id),
                "missing completed agent {id}; rows={ids:?}"
            );
        }
    }

    /// `max_recent_completed=0` ⇒ live-only behaviour (pre-fix
    /// shape). Used by callers that explicitly want the strip-mirror.
    #[test]
    fn agents_drilldown_with_zero_recent_returns_only_live() {
        let mut w = fresh();
        // Spawn one agent and complete it.
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "live-only check",
            "spawn-X",
            Some("agent-X"),
        )));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "spawn",
            "live-only check",
            "completed",
            1_000,
            Some(r#"{"status":"completed","agent_id":"agent-X","result":"done"}"#),
            "spawn-X",
            Some("agent-X"),
        )));

        let rows = w.agent_monitor_snapshot(0);
        assert!(
            rows.is_empty(),
            "max_recent_completed=0 must NOT surface completed rows"
        );
    }

    #[test]
    fn agent_workbench_does_not_inherit_compact_strip_terminal_limit() {
        let mut widget = fresh();
        for index in 0..8 {
            let agent_id = format!("reviewer-{index}");
            let tool_use_id = format!("spawn-{index}");
            widget.handle_event(AppEvent::wire(agent_control_started(
                "spawn",
                &agent_id,
                &tool_use_id,
                Some(&agent_id),
            )));
            let output = serde_json::json!({
                "status": "completed",
                "agent_id": agent_id,
                "result": format!("finding-{index}")
            })
            .to_string();
            widget.handle_event(AppEvent::wire(agent_control_completed(
                "spawn",
                &format!("reviewer-{index}"),
                "completed",
                1_000,
                Some(&output),
                &tool_use_id,
                Some(&format!("reviewer-{index}")),
            )));
        }

        assert_eq!(widget.agent_monitor_snapshot(5).len(), 5);
        assert_eq!(widget.agent_workbench_snapshot().len(), 8);
    }

    /// `task_cell_anywhere` finds completed agents in history so
    /// Ctrl+G drill-in still works after the live strip is gone.
    #[test]
    fn task_cell_anywhere_finds_completed_in_history() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "find me".into(),
            tool_use_id: "completed-id".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "find me".into(),
            status: "completed".into(),
            duration_ms: 250,
            output_summary: Some("done".into()),
            output: None,
            tool_use_id: "completed-id".into(),
            parent_tool_use_id: None,
        }));
        // No longer in live register.
        assert!(w.live_task_cell("completed-id").is_none());
        // But still findable in history.
        let found = w
            .task_cell_anywhere("completed-id")
            .expect("completed cell must be findable in history");
        assert_eq!(found.tool_use_id, "completed-id");
        assert_eq!(found.description, "find me");
        // Unknown id returns None.
        assert!(w.task_cell_anywhere("nope").is_none());
    }

    /// Live agents come BEFORE completed ones — the user's mental
    /// model is "what's running now first, history second."
    #[test]
    fn agents_drilldown_orders_live_before_completed() {
        let mut w = fresh();
        // Complete one, then start a new live one.
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "old completed",
            "spawn-old",
            Some("old"),
        )));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "spawn",
            "old completed",
            "completed",
            500,
            Some(r#"{"status":"completed","agent_id":"old","result":"done"}"#),
            "spawn-old",
            Some("old"),
        )));
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "new live",
            "spawn-new",
            Some("new"),
        )));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "spawn",
            "new live",
            "completed",
            0,
            Some(r#"{"status":"launched","agent_id":"new"}"#),
            "spawn-new",
            Some("new"),
        )));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent_id, "new", "live must come first");
        assert_eq!(rows[0].state.status, AgentRunStatus::Running);
        assert_eq!(rows[1].agent_id, "old", "completed must come second");
        assert_eq!(rows[1].state.status, AgentRunStatus::Completed);
    }

    #[test]
    fn agent_spawn_control_tool_creates_logical_agent_row() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "arch-reviewer",
            "spawn-arch",
            None,
        )));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "spawn",
            "arch-reviewer",
            "completed",
            0,
            Some(
                r#"{"status":"launched","agent_id":"arch-reviewer@abc12345","description":"Architecture review"}"#,
            ),
            "spawn-arch",
            Some("arch-reviewer@abc12345"),
        )));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(
            rows.len(),
            1,
            "spawn control cell must not appear as its own agent row"
        );
        assert_eq!(rows[0].agent_id, "arch-reviewer@abc12345");
        assert_eq!(rows[0].name, "arch-reviewer");
        assert_eq!(rows[0].state.status, AgentRunStatus::Running);

        let detail = w
            .task_cell_anywhere("arch-reviewer@abc12345")
            .expect("logical agent row should be drillable");
        assert_eq!(detail.description, "arch-reviewer");
    }

    #[test]
    fn get_result_updates_logical_agent_detail() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_completed(
            "spawn",
            "ux-reviewer",
            "completed",
            0,
            Some(
                r#"{"status":"launched","agent_id":"ux-reviewer@def67890","description":"UX review"}"#,
            ),
            "spawn-ux",
            Some("ux-reviewer@def67890"),
        )));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "ux-reviewer",
            "completed",
            123,
            Some(
                r#"{"status":"completed","agent_id":"ux-reviewer@def67890","finish_reason":"normal","result":"finding one\nfinding two"}"#,
            ),
            "result-ux",
            Some("ux-reviewer@def67890"),
        )));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Completed);
        let detail = w.task_cell_anywhere("ux-reviewer@def67890").unwrap();
        assert_eq!(
            detail.output_summary.as_deref(),
            Some("finding one\nfinding two")
        );
    }

    #[test]
    fn get_result_without_status_still_completes_agent() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            77,
            Some(r#"{"agent_id":"reviewer@abc12345","finish_reason":"normal","result":"done"}"#),
            "result-reviewer",
            Some("reviewer@abc12345"),
        )));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "reviewer@abc12345");
        assert!(
            rows[0].state.status == AgentRunStatus::Completed,
            "get_result output with a result is terminal even when legacy JSON lacks status=completed"
        );
        assert_eq!(
            w.task_cell_anywhere("reviewer@abc12345")
                .unwrap()
                .output_summary
                .as_deref(),
            Some("done")
        );
    }

    #[test]
    fn still_running_agent_detail_shows_wait_status() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            120_000,
            Some(
                r#"{"status":"still_running","agent_id":"reviewer@abc12345","current_status":"running","waited_secs":120,"hint":"call again"}"#,
            ),
            "result-reviewer",
            Some("reviewer@abc12345"),
        )));

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert!(matches!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Running
        ));
        assert_eq!(
            detail.output_summary.as_deref(),
            Some("Agent is running after 120s. call again")
        );
    }

    #[test]
    fn still_running_agent_result_appends_without_clobbering_live_output() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("live token".into()),
        })));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            120_000,
            Some(
                r#"{"status":"still_running","agent_id":"reviewer@abc12345","current_status":"running","waited_secs":120,"hint":"call again"}"#,
            ),
            "result-reviewer",
            Some("reviewer@abc12345"),
        )));

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        let output = detail.output_summary.as_deref().unwrap_or("");
        assert!(output.contains("live token"));
        assert!(output.contains("Agent is running after 120s. call again"));
    }

    #[test]
    fn live_run_start_binds_transcript_location_without_inventing_control_ownership() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-child-1".into(),
            agent_id: "reviewer@run-child-1".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                parent_run_id: Some("run-root".into()),
                depth: 2,
                spawn_tool_call_id: None,
                transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
            }),
        })));

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.agent_id, "reviewer@run-child-1");
        assert_eq!(row.run_id.as_deref(), Some("run-child-1"));
        assert_eq!(row.parent_run_id.as_deref(), Some("run-root"));
        assert_eq!(
            row.depth, 1,
            "a parent outside the visible agent set is rendered as a visible forest root"
        );
        assert!(row.available_actions.is_empty());
        assert!(row.control_target.is_none());
        assert_eq!(
            row.transcript_target,
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer)
        );
        assert!(
            widget
                .agent_run_cell("reviewer@run-child-1")
                .unwrap()
                .output_summary
                .is_none()
        );
    }

    #[test]
    fn live_agent_attention_is_waiting_not_generic_running_output() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-attention".into(),
            agent_id: "reviewer@attention".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::ApprovalRequired {
                request_id: "approval-1".into(),
                tool: "bash".into(),
                approval_kind: "explicit".into(),
                path: None,
                detail: Some("git status".into()),
                display_label: None,
            }),
        })));

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Waiting);
        assert_eq!(
            rows[0].attention_summary.as_deref(),
            Some("Approval required · git status")
        );
        assert!(matches!(
            widget
                .agent_run_cell("reviewer@attention")
                .map(|cell| cell.status),
            Some(crate::tui::history_cell::task::TaskStatus::Waiting)
        ));
        assert!(
            widget
                .agent_run_cell("reviewer@attention")
                .and_then(|cell| cell.output_summary.as_deref())
                .is_some_and(|summary| summary.contains("Approval required"))
        );

        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-attention".into(),
            agent_id: "reviewer@attention".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "git status".into(),
                tool_use_id: "tool-after-approval".into(),
            },
        })));
        assert_eq!(
            widget.agent_monitor_snapshot(0).rows[0].state.status,
            AgentRunStatus::Running,
            "a post-approval tool event resumes the run"
        );
        assert!(
            widget.agent_monitor_snapshot(0).rows[0]
                .attention_summary
                .is_none(),
            "a resumed run must not retain an old attention reason"
        );
    }

    #[test]
    fn waiting_for_model_is_activity_without_user_attention() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-model".into(),
            agent_id: "reviewer@model".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::WaitingForModel),
        })));

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Running);
        assert!(rows[0].attention_summary.is_none());
        assert!(matches!(
            widget
                .agent_run_cell("reviewer@model")
                .map(|cell| cell.status),
            Some(crate::tui::history_cell::task::TaskStatus::Running)
        ));
    }

    #[test]
    fn execution_waiting_is_recoverable_attention_not_termination() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-waiting".into(),
            agent_id: "reviewer@waiting".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::ExecutionWaiting {
                reason: "executor_offline".into(),
            }),
        })));

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Waiting);
        assert_eq!(
            rows[0].attention_summary.as_deref(),
            Some("Waiting · executor_offline")
        );
        assert!(!rows[0].state.status.is_terminal());
    }

    #[test]
    fn first_live_tool_event_binds_run_identity_without_run_started_signal() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-fanout-slot-1".into(),
            agent_id: "reviewer@slot-1".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "read".into(),
                description: "inspect the scheduler".into(),
                tool_use_id: "tool-read-1".into(),
            },
        })));

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.run_id.as_deref(), Some("run-fanout-slot-1"));
        assert_eq!(row.depth, 1);
        assert_eq!(row.activity.tool_calls, 1);
        assert!(row.control_target.is_none());
        assert!(row.transcript_target.is_none());
        assert!(row.available_actions.is_empty());
    }

    #[test]
    fn fanout_with_new_canonical_run_and_no_typed_receipt_is_uncertain() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent_fanout".into(),
            description: "start parallel review".into(),
            tool_use_id: "fanout-call-1".into(),
            parent_tool_use_id: None,
        }));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-fanout-slot-1".into(),
            agent_id: "reviewer@slot-1".into(),
            kind: AgentLiveEventKind::OutputDelta("reviewing".into()),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent_fanout".into(),
            description: "start parallel review".into(),
            status: "failed".into(),
            duration_ms: 3,
            output_summary: Some("control tool report failed".into()),
            output: Some("transport completed without a structured result".into()),
            tool_use_id: "fanout-call-1".into(),
            parent_tool_use_id: None,
        }));

        let persisted = widget
            .history()
            .last()
            .and_then(|cell| cell.to_persist())
            .expect("fanout cell is committed");
        assert!(matches!(
            persisted,
            TurnEvent::Tool {
                status: crate::tui::turn_event::ToolStatus::Uncertain,
                ..
            }
        ));
        let rows = widget.agent_monitor_snapshot(0);
        assert!(rows.iter().any(|row| {
            row.run_id.as_deref() == Some("run-fanout-slot-1")
                && row.control_target.is_none()
                && row.transcript_target.is_none()
                && row.available_actions.is_empty()
        }));
    }

    #[test]
    fn fanout_launch_receipt_makes_each_child_transcript_addressable() {
        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent_fanout".into(),
            description: "start parallel review".into(),
            tool_use_id: "fanout-call-addressable".into(),
            parent_tool_use_id: None,
        }));
        widget.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent_fanout".into(),
            description: "start parallel review".into(),
            status: "completed".into(),
            duration_ms: 3,
            output_summary: None,
            output: Some(
                serde_json::json!({
                    "status": "started",
                    "group_id": "review-42",
                    "title": "multi-angle review",
                    "target_count": 2,
                    "transcript_location": "durable_server",
                    "fanout": {
                        "parent_run_id": "root-run",
                        "slots": [
                            {"requested_description": "Correctness boundary review"},
                            {"requested_description": "Performance boundary review"}
                        ]
                    },
                    "agents": [
                        {
                            "slot_index": 0,
                            "id": "correctness",
                            "agent_id": "reviewer@one",
                            "run_id": "run-review-one",
                            "status": "launched",
                            "transcript_location": "durable_server"
                        },
                        {
                            "slot_index": 1,
                            "id": "performance",
                            "agent_id": "reviewer@two",
                            "run_id": "run-review-two",
                            "status": "launched",
                            "transcript_location": "durable_server"
                        }
                    ]
                })
                .to_string(),
            ),
            tool_use_id: "fanout-call-addressable".into(),
            parent_tool_use_id: None,
        }));

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| {
            row.run_id
                .as_deref()
                .is_some_and(|run_id| matches!(run_id, "run-review-one" | "run-review-two"))
                && row.parent_run_id.as_deref() == Some("root-run")
                && row.fanout.as_ref().is_some_and(|fanout| {
                    fanout.group_id == "review-42" && fanout.target_count == 2
                })
                && row.transcript_target
                    == Some(crate::tui::agent_run_projection::AgentTranscriptTarget::DurableServer)
                && row.control_target.is_none()
                && row.available_actions.is_empty()
        }));
        assert_eq!(
            widget
                .agent_run_cell("reviewer@one")
                .map(|cell| cell.description.as_str()),
            Some("Correctness boundary review")
        );
    }

    #[test]
    fn large_terminal_fanout_receipt_settles_every_child_before_turn_end() {
        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent_fanout".into(),
            description: "review from three angles".into(),
            tool_use_id: "fanout-terminal-large".into(),
            parent_tool_use_id: None,
        }));
        let slots = (0..3)
            .map(|slot| {
                serde_json::json!({
                    "slot_index": slot,
                    "id": format!("review-{slot}"),
                    "requested_description": format!("Review boundary {slot}"),
                    "agent_id": format!("reviewer@{slot}"),
                    "run_id": format!("run-review-{slot}"),
                    "status": "completed",
                    "transcript_location": "durable_server"
                })
            })
            .collect::<Vec<_>>();
        let launched_slots = slots
            .iter()
            .cloned()
            .map(|mut slot| {
                slot["status"] = serde_json::json!("launched");
                slot
            })
            .collect::<Vec<_>>();
        widget.on_agent_fanout_launch_receipt(
            &serde_json::json!({
                "status": "started",
                "group_id": "review-large-terminal",
                "title": "three-angle review",
                "target_count": 3,
                "transcript_location": "durable_server",
                "fanout": {
                    "parent_run_id": "root-run",
                    "slots": launched_slots
                }
            })
            .to_string(),
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        let raw = serde_json::json!({
            "status": "completed",
            "group_id": "review-large-terminal",
            "title": "three-angle review",
            "target_count": 3,
            "transcript_location": "durable_server",
            "fanout": {
                "parent_run_id": "root-run",
                "slots": slots
            },
            "results": "x".repeat(6_000),
            "work_unit_observation": {
                "id": "review-large-terminal",
                "kind": "agent_fanout",
                "status": "completed",
                "revision": 3,
                "mode": "transition",
                "wake_policy": "none"
            }
        })
        .to_string();
        assert!(
            raw.len() > 5_000,
            "regression requires the old truncation boundary"
        );
        let event_output =
            crate::cli::stream::stream_render::tool_output_event_text("agent_fanout", &raw);
        serde_json::from_str::<serde_json::Value>(&event_output)
            .expect("the CLI/UI boundary must preserve valid terminal JSON");

        widget.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent_fanout".into(),
            description: "review from three angles".into(),
            status: "completed".into(),
            duration_ms: 34_000,
            output_summary: None,
            output: Some(event_output),
            tool_use_id: "fanout-terminal-large".into(),
            parent_tool_use_id: None,
        }));
        widget.handle_event(AppEvent::wire(WireEvent::TurnComplete(Box::default())));

        let rows = widget.agent_monitor_snapshot(3);
        assert_eq!(rows.len(), 3);
        assert!(
            rows.iter().all(|row| {
                row.state.status == AgentRunStatus::Completed
                    && row.state.confidence == AgentProjectionConfidence::Observed
                    && row.elapsed_ms > 0
            }),
            "terminal children must not degrade to new 0ms unconfirmed placeholders"
        );
    }

    #[test]
    fn compact_agent_strip_scopes_history_to_the_current_fanout_group() {
        use crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership;

        let mut registry = AgentRunRegistry::default();
        for (group, state) in [
            ("old", AgentRunStatus::Completed),
            ("current", AgentRunStatus::Running),
        ] {
            for slot in 0..3 {
                let id = format!("{group}-{slot}");
                registry.ensure(
                    id.clone(),
                    format!("{group} slot {slot}"),
                    AgentRunState::observed(state),
                );
                registry.set_fanout_membership(
                    &id,
                    Some(AgentFanoutMembership {
                        group_id: group.into(),
                        group_title: group.into(),
                        target_count: 3,
                        slot_index: slot,
                        slot_label: format!("slot-{slot}"),
                    }),
                );
            }
        }

        assert_eq!(
            registry.status_strip_ids(),
            vec!["current-0", "current-1", "current-2"]
        );
        assert_eq!(registry.ids().len(), 6, "workbench retains session history");
    }

    #[test]
    fn recovered_fanout_registry_slots_restore_each_child_transcript_identity() {
        let mut widget = fresh();
        widget.on_agent_fanout_launch_receipt(
            &serde_json::json!({
                "status": "incomplete",
                "group_id": "review-recovered",
                "title": "recovered review",
                "target_count": 1,
                "transcript_location": "local_journal",
                "fanout": {
                    "parent_run_id": "root-run",
                    "slots": [{
                        "slot_index": 0,
                        "id": "correctness",
                        "agent_id": "reviewer@one",
                        "run_id": "run-review-one",
                        "status": "running"
                    }]
                }
            })
            .to_string(),
        );

        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.agent_id, "reviewer@one");
        assert_eq!(row.run_id.as_deref(), Some("run-review-one"));
        assert_eq!(row.parent_run_id.as_deref(), Some("root-run"));
        assert_eq!(
            row.transcript_target,
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal)
        );
        assert!(row.fanout.as_ref().is_some_and(|fanout| {
            fanout.group_id == "review-recovered" && fanout.slot_label == "correctness"
        }));
    }

    #[test]
    fn malformed_fanout_payload_becomes_an_actionable_rejection_without_raw_json() {
        let payload = serde_json::json!({
            "status": "failed",
            "error_kind": "tool_invalid_args",
            "error": "Tool arguments were not valid JSON",
            "advisory": {
                "kind": "malformed_tool_arguments",
                "next_step": "Create one new complete JSON tool call that matches the advertised schema."
            }
        })
        .to_string();

        let summary = fanout_rejection_summary(None, Some(&payload))
            .expect("typed fanout admission failure should have a user surface");
        assert_eq!(
            summary,
            "Fanout did not start · its arguments were invalid, so no agents were launched. Create one new complete JSON tool call that matches the advertised schema."
        );
    }

    #[test]
    fn fanout_receipt_requires_group_identity_and_status() {
        assert!(!fanout_completion_has_receipt(
            Some("transport completed without a structured result"),
            None,
        ));
        assert!(!fanout_completion_has_receipt(
            None,
            Some(r#"{"status":"started"}"#),
        ));
        assert!(fanout_completion_has_receipt(
            None,
            Some(r#"{"status":"started","group_id":"review-42"}"#),
        ));
    }

    #[test]
    fn fanout_without_receipt_is_uncertain_until_the_registry_confirms_it() {
        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent_fanout".into(),
            description: "start parallel review".into(),
            tool_use_id: "fanout-call-2".into(),
            parent_tool_use_id: None,
        }));
        widget.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent_fanout".into(),
            description: "start parallel review".into(),
            status: "failed".into(),
            duration_ms: 3,
            output_summary: None,
            output: None,
            tool_use_id: "fanout-call-2".into(),
            parent_tool_use_id: None,
        }));

        let persisted = widget
            .history()
            .last()
            .and_then(|cell| cell.to_persist())
            .expect("fanout cell is committed");
        assert!(matches!(
            persisted,
            TurnEvent::Tool {
                status: crate::tui::turn_event::ToolStatus::Uncertain,
                output_summary: Some(ref summary),
                ..
            } if summary.contains("checking the task registry")
        ));
    }

    #[test]
    fn concurrent_runs_of_one_profile_keep_independent_live_projections() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal, AgentLiveTermination,
        };

        let mut widget = fresh();
        for (run_id, parent_run_id) in [("run-one", "root-one"), ("run-two", "root-two")] {
            widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
                run_id: run_id.into(),
                // Server-side profiles are intentionally reusable. The run id,
                // not this label, separates their conversations.
                agent_id: "reviewer".into(),
                kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                    parent_run_id: Some(parent_run_id.into()),
                    depth: 1,
                    spawn_tool_call_id: None,
                    transcript_location: astra_turn_types::AgentTranscriptLocation::DurableServer,
                }),
            })));
        }
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-one".into(),
            agent_id: "reviewer".into(),
            kind: AgentLiveEventKind::OutputDelta("first run finding".into()),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-two".into(),
            agent_id: "reviewer".into(),
            kind: AgentLiveEventKind::OutputDelta("second run finding".into()),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-one".into(),
            agent_id: "reviewer".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Completed,
                duration_ms: 1,
                reason: None,
            },
        })));

        // Keep the just-completed first run alongside the still-live second
        // run. `0` intentionally means active/uncertain rows only.
        let rows = widget.agent_monitor_snapshot(1);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().any(|row| {
            row.run_id.as_deref() == Some("run-one")
                && row.state.status == AgentRunStatus::Completed
        }));
        assert!(rows.iter().any(|row| {
            row.run_id.as_deref() == Some("run-two") && row.state.status == AgentRunStatus::Running
        }));
        assert!(
            widget
                .agent_run_cell("reviewer")
                .unwrap()
                .output_summary
                .as_deref()
                .unwrap_or_default()
                .contains("first run finding")
        );
        let second = widget.agent_run_cell("run-two").unwrap();
        assert!(
            second
                .output_summary
                .as_deref()
                .unwrap_or_default()
                .contains("second run finding")
        );
        assert!(
            !second
                .output_summary
                .as_deref()
                .unwrap_or_default()
                .contains("first run finding")
        );
    }

    #[test]
    fn result_timeout_degrades_confidence_without_poisoning_agent_lifecycle() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("working".into()),
        })));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            120_000,
            Some(r#"{"status":"timeout","agent_id":"reviewer@abc12345","waited_secs":120}"#),
            "result-reviewer",
            Some("reviewer@abc12345"),
        )));

        let state = w.agent_run_state("reviewer@abc12345").unwrap();
        assert_eq!(state.status, AgentRunStatus::Running);
        assert_eq!(state.confidence, AgentProjectionConfidence::Unconfirmed);
        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert_eq!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Unconfirmed
        );
        assert!(
            detail
                .output_summary
                .as_deref()
                .is_some_and(|output| output.contains("timed out"))
        );
    }

    #[test]
    fn interrupted_get_result_preserves_interrupted_lifecycle() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            77,
            Some(
                r#"{"status":"interrupted","agent_id":"reviewer@abc12345","finish_reason":"budget_exhausted"}"#,
            ),
            "result-reviewer",
            Some("reviewer@abc12345"),
        )));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "reviewer@abc12345");
        assert_eq!(rows[0].state.status, AgentRunStatus::Interrupted);

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert!(matches!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Interrupted
        ));
        assert_eq!(
            detail.error.as_deref(),
            Some("Needs continuation: The run reached its turn budget.")
        );
    }

    #[test]
    fn interrupted_get_result_preserves_partial_result_text() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            77,
            Some(
                r#"{"status":"interrupted","agent_id":"reviewer@abc12345","result":"partial draft","finish_reason":"budget_exhausted"}"#,
            ),
            "result-reviewer",
            Some("reviewer@abc12345"),
        )));

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert!(matches!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Interrupted
        ));
        assert_eq!(detail.output_summary.as_deref(), Some("partial draft"));
        assert_eq!(
            detail.error.as_deref(),
            Some("Needs continuation: The run reached its turn budget.")
        );
    }

    #[test]
    fn agent_live_events_append_output_and_child_tools() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("hello ".into()),
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("world".into()),
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "Run checks".into(),
                tool_use_id: "child-tool".into(),
            },
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::ToolCompleted {
                name: "bash".into(),
                description: "Run checks".into(),
                status: "completed".into(),
                duration_ms: 42,
                output_summary: Some("ok".into()),
                output: None,
                tool_use_id: "child-tool".into(),
            },
        })));

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert_eq!(detail.output_summary.as_deref(), Some("hello world\nok\n"));
        assert_eq!(detail.children.len(), 1);
        assert!(matches!(
            detail.children[0].status,
            crate::tui::history_cell::task::ChildStatus::Success
        ));
    }

    #[test]
    fn failed_spawn_without_agent_id_stays_in_control_tool_history_only() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "broken-reviewer",
            "spawn-broken",
            None,
        )));
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "broken-reviewer".into(),
            tool_use_id: "spawn-broken".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::wire(agent_control_completed(
            "spawn",
            "broken-reviewer",
            "failed",
            10,
            Some(r#"{"error":"spawn failed"}"#),
            "spawn-broken",
            None,
        )));
        w.handle_event(AppEvent::wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "broken-reviewer".into(),
            status: "failed".into(),
            duration_ms: 10,
            output_summary: None,
            output: Some(r#"{"error":"spawn failed"}"#.into()),
            tool_use_id: "spawn-broken".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agent_monitor_snapshot(5);
        assert!(
            rows.is_empty(),
            "a failed control call with no Agent identity must not synthesize an Agent run"
        );
        let control = w
            .task_cell_anywhere("spawn-broken")
            .expect("the failed control call remains visible in transcript history");
        assert_eq!(
            control.status,
            crate::tui::history_cell::task::TaskStatus::Failed
        );
    }

    #[test]
    fn generic_tool_description_never_drives_agent_identity_or_lifecycle() {
        let mut generic_only = fresh();
        generic_only.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: looks-like-a-protocol".into(),
            tool_use_id: "generic-only".into(),
            parent_tool_use_id: None,
        }));
        assert!(generic_only.agent_run_ids().is_empty());

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: reviewer-A (code-review)".into(),
            tool_use_id: "spawn-tu-1".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agent_monitor_snapshot(5);
        let ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(ids.len(), 1, "spawn lifecycle must produce one logical row");
        assert!(
            ids.contains(&"reviewer-A@abc12345"),
            "only the structured agent_id may surface as the row id; got {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.starts_with("pending:")),
            "no row should fall back to the provisional `pending:<tool_use_id>` key \
             once the structured event has provided a real agent_id; got {ids:?}"
        );
    }

    #[test]
    fn agent_live_child_events_also_render_inside_parent_task_cell() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: reviewer-A (code-review)".into(),
            tool_use_id: "spawn-tu-1".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer-A@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "cargo test".into(),
                tool_use_id: "child-tu-1".into(),
            },
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer-A@abc12345".into(),
            kind: AgentLiveEventKind::ToolCompleted {
                name: "bash".into(),
                description: "cargo test".into(),
                status: "completed".into(),
                duration_ms: 25,
                output_summary: None,
                output: None,
                tool_use_id: "child-tu-1".into(),
            },
        })));

        let parent_task = w
            .task_cell_anywhere("spawn-tu-1")
            .expect("live parent task");
        assert_eq!(parent_task.children.len(), 1);
        assert!(matches!(
            parent_task.children[0].status,
            crate::tui::history_cell::task::ChildStatus::Success
        ));
    }

    #[test]
    fn agent_completion_merges_provisional_row_into_existing_live_row() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: reviewer-A (code-review)".into(),
            tool_use_id: "spawn-tu-1".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer-A@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "cargo test".into(),
                tool_use_id: "child-tu-1".into(),
            },
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 30,
            output: Some(
                r#"{"status":"completed","agent_id":"reviewer-A@abc12345","result":"done"}"#.into(),
            ),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
        }));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "reviewer-A@abc12345");
        let detail = w
            .task_cell_anywhere("reviewer-A@abc12345")
            .expect("merged live row");
        assert_eq!(
            detail.children.len(),
            1,
            "existing live child state must survive rename"
        );
        assert!(
            !detail.tool_use_id.starts_with("pending:"),
            "the merged row must keep the canonical id"
        );
    }

    #[test]
    fn live_spawn_identity_merges_provisional_control_before_child_output() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "Mock child review",
            "call-spawn-child",
            None,
        )));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-child".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                parent_run_id: Some("run-root".into()),
                depth: 1,
                spawn_tool_call_id: Some("call-spawn-child".into()),
                transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
            }),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-child".into(),
            kind: AgentLiveEventKind::OutputDelta("child evidence".into()),
        })));

        let rows = widget.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "agent-child");
        assert_eq!(rows[0].name, "Mock child review");
        let (events, dropped) = widget.agent_live_transcript_replay("agent-child", "run-child");
        assert_eq!(dropped, 0);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            AgentLiveEventKind::OutputDelta(text) if text == "child evidence"
        )));
    }

    #[test]
    fn live_spawn_identity_before_control_start_binds_late_control_to_canonical_row() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveSignal,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-child".into(),
            kind: AgentLiveEventKind::Signal(AgentLiveSignal::RunStarted {
                parent_run_id: Some("run-root".into()),
                depth: 1,
                spawn_tool_call_id: Some("call-spawn-child".into()),
                transcript_location: astra_turn_types::AgentTranscriptLocation::LocalJournal,
            }),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "run-child".into(),
            agent_id: "agent-child".into(),
            kind: AgentLiveEventKind::OutputDelta("early child evidence".into()),
        })));
        widget.handle_event(AppEvent::wire(agent_control_started(
            "spawn",
            "Mock child review",
            "call-spawn-child",
            None,
        )));

        let rows = widget.agent_monitor_snapshot(5);
        assert_eq!(
            rows.len(),
            1,
            "late control start must not add a pending row"
        );
        assert_eq!(rows[0].agent_id, "agent-child");
        assert_eq!(rows[0].name, "Mock child review");
        assert_eq!(rows[0].run_id.as_deref(), Some("run-child"));
        assert_eq!(
            widget.agent_runs.key_for_tool_use("call-spawn-child"),
            Some("agent-child")
        );
        let (events, dropped) = widget.agent_live_transcript_replay("agent-child", "run-child");
        assert_eq!(dropped, 0);
        assert!(events.iter().any(|event| matches!(
            &event.kind,
            AgentLiveEventKind::OutputDelta(text) if text == "early child evidence"
        )));
    }

    #[test]
    fn first_tool_use_binding_wins_until_explicit_rename() {
        let mut registry = AgentRunRegistry::default();
        registry.ensure_for_tool_use(
            "pending:spawn-tu-1".into(),
            "reviewer-A".into(),
            AgentRunState::observed(AgentRunStatus::Starting),
            "spawn-tu-1",
            AgentControlAction::Spawn,
        );
        registry.ensure_for_tool_use(
            "late-other-key".into(),
            "reviewer-B".into(),
            AgentRunState::observed(AgentRunStatus::Starting),
            "spawn-tu-1",
            AgentControlAction::Spawn,
        );

        assert_eq!(
            registry.key_for_tool_use("spawn-tu-1"),
            Some("pending:spawn-tu-1"),
            "late duplicate binds must not clobber the first tool_use -> row mapping"
        );

        registry.rename("pending:spawn-tu-1", "reviewer-A@abc12345".into());
        assert_eq!(
            registry.key_for_tool_use("spawn-tu-1"),
            Some("reviewer-A@abc12345"),
            "explicit rename is the only path that should re-point the mapping"
        );
    }

    #[test]
    fn ctrlc_renders_cancelling_state_before_agent_terminated_arrives() {
        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
            fanout_slot: None,
            fanout_title: None,
        }));

        w.mark_control_tasks_cancelling(&["spawn-tu-1".to_string()]);

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Cancelling);
        assert!(
            w.task_cell_anywhere("reviewer-A@abc12345")
                .and_then(|tc| tc.output_summary.as_deref())
                .is_some_and(|output| output.contains("Cancelling…")),
            "detail output should show an immediate cancelling marker"
        );
        w.agent_runs
            .get_mut("reviewer-A@abc12345")
            .expect("logical row")
            .detail
            .output_summary = Some("x".repeat(100_000));
        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(
            rows[0].state.status,
            AgentRunStatus::Cancelling,
            "cancelling status must be structural, not parsed from truncated output text"
        );
    }

    #[test]
    fn agent_control_started_projects_fanout_membership_to_drilldown_rows_after_rename() {
        use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "auth reviewer".into(),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: None,
            fanout_slot: Some(AgentFanoutSlotIdentity::new("review-1", 3, 0, None).unwrap()),
            fanout_title: Some("review fanout".into()),
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "auth reviewer".into(),
            status: "completed".into(),
            duration_ms: 25,
            output: Some(r#"{"status":"completed","agent_id":"auth@abc12345"}"#.into()),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: Some("auth@abc12345".into()),
        }));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "auth@abc12345");
        let fanout = rows[0].fanout.as_ref().expect("fanout membership");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.target_count, 3);
        assert_eq!(fanout.slot_index, 0);
        assert_eq!(fanout.slot_label, "auth reviewer");
    }

    #[test]
    fn fanout_membership_survives_merge_into_live_agent_row() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "storage reviewer".into(),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: None,
            fanout_slot: Some(
                AgentFanoutSlotIdentity::new("review-1", 3, 1, Some("storage".into())).unwrap(),
            ),
            fanout_title: Some("review fanout".into()),
        }));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "storage@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "cargo test".into(),
                tool_use_id: "child-tu-1".into(),
            },
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "storage reviewer".into(),
            status: "completed".into(),
            duration_ms: 25,
            output: Some(r#"{"status":"completed","agent_id":"storage@abc12345"}"#.into()),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: Some("storage@abc12345".into()),
        }));

        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "storage@abc12345");
        let fanout = rows[0].fanout.as_ref().expect("fanout membership");
        assert_eq!(fanout.group_id, "review-1");
        assert_eq!(fanout.group_title, "review fanout");
        assert_eq!(fanout.slot_index, 1);
        let detail = w
            .task_cell_anywhere("storage@abc12345")
            .expect("merged live row");
        assert_eq!(detail.children.len(), 1);
    }

    /// REGRESSION (reviewer L2-5): a sub-agent that crashes / times
    /// out / is cancelled MUST flip its multi_agent strip row out of
    /// the `live` state. Pre-fix the live bridge had no terminal
    /// event variant, so the row stayed visually `live` forever.
    #[test]
    fn agent_terminated_failed_flips_row_to_failed() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut w = fresh();
        // Establish a live row first.
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@def01234".into(),
            kind: AgentLiveEventKind::OutputDelta("starting".into()),
        })));
        let row = w
            .agent_run_cell("reviewer@def01234")
            .expect("live event must establish row");
        assert!(matches!(
            row.status,
            crate::tui::history_cell::task::TaskStatus::Running
        ));

        // Sub-agent crashes.
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@def01234".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Failed,
                duration_ms: 500,
                reason: Some("subprocess panicked".into()),
            },
        })));

        let row = w
            .agent_run_cell("reviewer@def01234")
            .expect("row must persist after termination");
        assert!(
            matches!(
                row.status,
                crate::tui::history_cell::task::TaskStatus::Failed
            ),
            "Failed termination must flip status to Failed; got {:?}",
            row.status
        );
        assert_eq!(
            row.error.as_deref(),
            Some("subprocess panicked"),
            "reason must be preserved as error so the user can see what crashed"
        );

        let rows = w.agent_monitor_snapshot(5);
        let target = rows
            .iter()
            .find(|r| r.agent_id == "reviewer@def01234")
            .unwrap();
        assert_eq!(
            target.state.status,
            AgentRunStatus::Failed,
            "drilldown row must report failed status"
        );
    }

    #[test]
    fn agent_terminated_interrupted_preserves_resumable_status() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@paused".into(),
            kind: AgentLiveEventKind::OutputDelta("partial findings".into()),
        })));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@paused".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Interrupted,
                duration_ms: 500,
                reason: Some("paused".into()),
            },
        })));

        let row = widget
            .agent_run_cell("reviewer@paused")
            .expect("interrupted row remains inspectable");
        assert_eq!(
            row.status,
            crate::tui::history_cell::task::TaskStatus::Interrupted
        );
        assert_eq!(
            widget.agent_monitor_snapshot(5)[0].state.status,
            AgentRunStatus::Interrupted
        );
    }

    #[test]
    fn durable_server_interrupted_status_stays_interrupted_in_agent_monitor() {
        assert_eq!(
            server_agent_run_status(astra_thin_client::SessionRunLifecycleStatus::Interrupted),
            AgentRunStatus::Interrupted
        );
    }

    /// Cancelled sub-agents also flip out of the live state and keep a
    /// distinct row status even though TaskCell itself has no Cancelled
    /// variant.
    #[test]
    fn agent_terminated_cancelled_flips_row_terminal() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@cancel01".into(),
            kind: AgentLiveEventKind::OutputDelta("running".into()),
        })));
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@cancel01".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Cancelled,
                duration_ms: 200,
                reason: Some("user cancellation".into()),
            },
        })));

        let row = w.agent_run_cell("reviewer@cancel01").unwrap();
        assert_eq!(
            row.status,
            crate::tui::history_cell::task::TaskStatus::Cancelled,
            "list and detail must agree that user cancellation is not failure"
        );
        let rows = w.agent_monitor_snapshot(5);
        assert_eq!(rows[0].state.status, AgentRunStatus::Cancelled);
    }

    /// Completed termination is the happy path: just confirm the
    /// row is no longer Running and that the `reason` populates the
    /// summary cleanly.
    #[test]
    fn agent_terminated_completed_marks_row_done() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@done7777".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Completed,
                duration_ms: 1_000,
                reason: Some("normal".into()),
            },
        })));

        let row = w.agent_run_cell("reviewer@done7777").unwrap();
        assert!(matches!(
            row.status,
            crate::tui::history_cell::task::TaskStatus::Completed
        ));
    }

    #[test]
    fn late_live_delta_after_termination_does_not_reopen_agent_row() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut w = fresh();
        w.handle_event(AppEvent::wire(WireEvent::AgentLiveBatch(vec![
            AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "reviewer@done7777".into(),
                kind: AgentLiveEventKind::AgentTerminated {
                    termination: AgentLiveTermination::Completed,
                    duration_ms: 1_000,
                    reason: Some("normal".into()),
                },
            },
            AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "reviewer@done7777".into(),
                kind: AgentLiveEventKind::OutputDelta("late token".into()),
            },
        ])));

        let row = w.agent_run_cell("reviewer@done7777").unwrap();
        assert!(matches!(
            row.status,
            crate::tui::history_cell::task::TaskStatus::Completed
        ));
        assert_eq!(row.output_summary.as_deref(), Some("normal"));
    }

    #[test]
    fn server_lane_truth_updates_even_without_agent_rows() {
        use crate::tui::server_agent_observer::{ServerAgentProjection, ServerAgentTruthState};

        let mut widget = fresh();
        let loading = ServerAgentProjection {
            sequence: 0,
            truth_state: ServerAgentTruthState::Loading,
            snapshot: None,
        };
        assert!(widget.reconcile_server_agent_projection(&loading));
        let snapshot = widget.agent_monitor_snapshot(5);
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.server_truth_state, ServerAgentTruthState::Loading);
        assert!(snapshot.should_open());
        assert!(!widget.reconcile_server_agent_projection(&loading));

        let unavailable = ServerAgentProjection {
            sequence: 1,
            truth_state: ServerAgentTruthState::Unavailable,
            snapshot: None,
        };
        assert!(widget.reconcile_server_agent_projection(&unavailable));
        assert_eq!(
            widget.agent_monitor_snapshot(5).server_truth_state,
            ServerAgentTruthState::Unavailable
        );
    }

    #[test]
    fn server_only_snapshot_projects_child_run_with_durable_controls() {
        use astra_thin_client::{
            SessionRunAction, SessionRunLifecycleStatus, SessionRunTreeSnapshot,
        };

        let mut widget = fresh();
        let mut root = server_run_node("root-run", SessionRunLifecycleStatus::Running, 1);
        root.parent_run_id = None;
        root.depth = 0;
        root.agent_id = None;
        root.agent_name = None;
        let mut child = server_run_node("child-run", SessionRunLifecycleStatus::Paused, 4);
        child.waiting_for = Some("user_resume".into());
        child.available_actions = vec![SessionRunAction::Resume, SessionRunAction::Cancel];
        let projection = crate::tui::server_agent_observer::ServerAgentProjection {
            sequence: 1,
            truth_state: crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            snapshot: Some(SessionRunTreeSnapshot {
                schema_version: astra_thin_client::SESSION_RUN_TREE_SCHEMA_VERSION,
                session_id: "session-1".into(),
                snapshot_revision: "revision-paused".into(),
                observed_at: "2026-07-11T00:00:02Z".into(),
                node_limit: 200,
                truncated: false,
                runs: vec![child, root],
            }),
        };

        assert!(widget.reconcile_server_agent_projection(&projection));
        let rows = widget.agent_monitor_snapshot(5);
        assert_eq!(
            rows.len(),
            1,
            "root conversation run is not a sub-agent row"
        );
        assert_eq!(rows[0].agent_id, "child-run");
        assert_eq!(rows[0].state.status, AgentRunStatus::Paused);
        assert_eq!(rows[0].state.source, AgentProjectionSource::DurableServer);
        assert_eq!(
            rows[0].control_target,
            Some(
                crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                    run_id: "child-run".into(),
                }
            )
        );
        assert_eq!(
            rows[0].available_actions,
            vec![SessionRunAction::Resume, SessionRunAction::Cancel]
        );
        assert_eq!(
            rows[0].runtime.runtime_profile.as_deref(),
            Some("agent_binding_registry")
        );
        assert_eq!(rows[0].runtime.model_name.as_deref(), Some("gpt-5"));
        assert_eq!(
            rows[0].runtime.agent_binding_id.as_deref(),
            Some("reviewer-v2")
        );
        assert!(rows[0].runtime.permission.is_none());
    }

    #[test]
    fn durable_agent_membership_does_not_infer_from_parentage() {
        use astra_thin_client::SessionRunLifecycleStatus;

        let mut ordinary_child =
            server_run_node("ordinary-child-run", SessionRunLifecycleStatus::Running, 1);
        ordinary_child.agent_id = None;
        ordinary_child.agent_name = Some("not sufficient identity".into());
        let mut root_agent = server_run_node(
            "team-orchestrator-run",
            SessionRunLifecycleStatus::Running,
            1,
        );
        root_agent.parent_run_id = None;
        root_agent.root_run_id = Some(root_agent.run_id.clone());
        root_agent.depth = 0;
        root_agent.agent_id = Some("team-orchestrator".into());

        let mut widget = fresh();
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![ordinary_child, root_agent],
            false,
        ));

        let snapshot = widget.agent_monitor_snapshot(0);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].agent_id, "team-orchestrator-run");
        assert_eq!(snapshot[0].depth, 1);
    }

    #[test]
    fn durable_tree_preserves_lineage_and_separates_tools_from_child_agents() {
        use astra_thin_client::SessionRunLifecycleStatus;

        let mut widget = fresh();
        let mut parent = server_run_node("parent-run", SessionRunLifecycleStatus::Running, 2);
        parent.total_tool_calls = 3;
        let mut child = server_run_node("nested-run", SessionRunLifecycleStatus::Running, 1);
        child.parent_run_id = Some("parent-run".into());
        child.root_run_id = Some("root-run".into());
        child.depth = 2;
        child.total_tool_calls = 1;

        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![child, parent],
            false,
        ));
        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(
            rows.iter()
                .map(|row| row.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["parent-run", "nested-run"],
            "visible hierarchy is parent-first even when snapshots arrive in another order"
        );
        let parent = rows
            .iter()
            .find(|row| row.agent_id == "parent-run")
            .unwrap();
        assert_eq!(parent.activity.tool_calls, 3);
        assert_eq!(parent.activity.child_agents, 1);
        assert!(!parent.activity.child_agents_partial);
        assert_eq!(parent.depth, 1);
        assert_eq!(parent.provenance, AgentProjectionSource::DurableServer);
        let child = rows
            .iter()
            .find(|row| row.agent_id == "nested-run")
            .unwrap();
        assert_eq!(child.activity.tool_calls, 1);
        assert_eq!(child.activity.child_agents, 0);
        assert_eq!(child.parent_run_id.as_deref(), Some("parent-run"));
        assert_eq!(child.depth, 2);
    }

    #[test]
    fn visible_forest_preserves_terminal_parent_for_lineage() {
        use astra_thin_client::SessionRunLifecycleStatus;

        let mut parent = server_run_node("parent-run", SessionRunLifecycleStatus::Completed, 2);
        parent.agent_id = Some("parent-agent".into());
        parent.agent_name = Some("Planning parent".into());
        parent.available_actions.clear();
        let mut child = server_run_node("nested-run", SessionRunLifecycleStatus::Running, 1);
        child.agent_id = Some("child-agent".into());
        child.agent_name = Some("Implementation child".into());
        child.parent_run_id = Some("parent-run".into());
        child.depth = 2;
        let mut same_name_decoy = server_run_node(
            "unrelated-terminal-run",
            SessionRunLifecycleStatus::Completed,
            3,
        );
        same_name_decoy.agent_id = Some("unrelated-agent".into());
        same_name_decoy.agent_name = Some("Implementation child".into());
        same_name_decoy.parent_run_id = Some("another-root".into());
        same_name_decoy.available_actions.clear();
        let mut widget = fresh();
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![same_name_decoy, parent, child],
            false,
        ));

        let visible = widget.agent_monitor_snapshot(0);
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].agent_id, "parent-run");
        assert_eq!(visible[0].depth, 1);
        assert_eq!(visible[1].agent_id, "nested-run");
        assert_eq!(visible[1].parent_run_id.as_deref(), Some("parent-run"));
        assert!(
            visible
                .iter()
                .all(|row| row.agent_id != "unrelated-terminal-run"),
            "an unrelated terminal row with the child's display name is not lineage"
        );
        assert_eq!(
            visible[1].depth, 2,
            "a retained child keeps its visible ancestor instead of becoming an unexplained root"
        );
    }

    #[test]
    fn older_server_snapshot_cannot_regress_a_terminal_run() {
        use astra_thin_client::SessionRunLifecycleStatus;

        let mut widget = fresh();
        let mut completed = server_run_node("child-run", SessionRunLifecycleStatus::Completed, 9);
        completed.available_actions.clear();
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![completed],
            false,
        ));
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![server_run_node(
                "child-run",
                SessionRunLifecycleStatus::Running,
                8,
            )],
            false,
        ));

        let state = widget.agent_run_state("child-run").unwrap();
        assert_eq!(state.status, AgentRunStatus::Completed);
        assert_eq!(state.source, AgentProjectionSource::DurableServer);
    }

    #[test]
    fn server_failure_marks_durable_rows_stale_but_keeps_local_capacity_confirmed() {
        use astra_thin_client::SessionRunLifecycleStatus;
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "local-child",
                AgentStatus::Running {
                    activity: "reviewing".into(),
                },
            )]),
            &[],
        );
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![server_run_node(
                "server-child",
                SessionRunLifecycleStatus::Running,
                3,
            )],
            false,
        ));
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Stale,
            vec![server_run_node(
                "server-child",
                SessionRunLifecycleStatus::Running,
                3,
            )],
            false,
        ));

        assert_eq!(
            widget.agent_run_state("server-child").unwrap().confidence,
            AgentProjectionConfidence::Stale
        );
        assert_eq!(
            widget.agent_run_state("local-child").unwrap().confidence,
            AgentProjectionConfidence::Confirmed
        );
        assert_eq!(widget.agent_monitor_snapshot(5).len(), 2);
    }

    #[test]
    fn complete_snapshot_omission_degrades_active_run_but_truncation_does_not() {
        use astra_thin_client::SessionRunLifecycleStatus;

        let confirmed = server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![server_run_node(
                "server-child",
                SessionRunLifecycleStatus::Running,
                3,
            )],
            false,
        );
        let mut complete_widget = fresh();
        complete_widget.reconcile_server_agent_projection(&confirmed);
        complete_widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            Vec::new(),
            false,
        ));
        assert_eq!(
            complete_widget
                .agent_run_state("server-child")
                .unwrap()
                .confidence,
            AgentProjectionConfidence::Stale
        );

        let mut truncated_widget = fresh();
        truncated_widget.reconcile_server_agent_projection(&confirmed);
        truncated_widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            Vec::new(),
            true,
        ));
        assert_eq!(
            truncated_widget
                .agent_run_state("server-child")
                .unwrap()
                .confidence,
            AgentProjectionConfidence::Confirmed
        );
        let snapshot = truncated_widget.agent_monitor_snapshot(5);
        assert!(snapshot.durable_snapshot_truncated);
        assert!(snapshot[0].activity.child_agents_partial);
    }

    #[test]
    fn rejected_cancel_restores_last_lifecycle_as_stale_instead_of_sticking() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "local-child",
                AgentStatus::Running {
                    activity: "reviewing".into(),
                },
            )]),
            &[],
        );
        assert!(widget.mark_agent_control_pending(
            "local-child",
            astra_thin_client::SessionRunAction::Cancel,
        ));
        assert_eq!(
            widget.agent_run_state("local-child").unwrap().status,
            AgentRunStatus::Cancelling
        );

        assert!(widget.reject_agent_control("local-child"));
        let restored = widget.agent_run_state("local-child").unwrap();
        assert_eq!(restored.status, AgentRunStatus::Running);
        assert_eq!(restored.confidence, AgentProjectionConfidence::Stale);
    }

    #[test]
    fn pause_and_resume_have_typed_pending_states_and_authoritative_reconciliation() {
        use astra_thin_client::{SessionRunAction, SessionRunLifecycleStatus};

        let mut widget = fresh();
        let mut running = server_run_node("server-child", SessionRunLifecycleStatus::Running, 3);
        running.available_actions = vec![SessionRunAction::Pause, SessionRunAction::Cancel];
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![running],
            false,
        ));

        assert!(widget.mark_agent_control_pending("server-child", SessionRunAction::Pause));
        assert_eq!(
            widget.agent_run_state("server-child").unwrap().status,
            AgentRunStatus::Pausing
        );
        assert!(widget.reject_agent_control("server-child"));
        assert_eq!(
            widget.agent_run_state("server-child").unwrap(),
            AgentRunState {
                status: AgentRunStatus::Running,
                confidence: AgentProjectionConfidence::Stale,
                source: AgentProjectionSource::DurableServer,
            }
        );

        let mut paused = server_run_node("server-child", SessionRunLifecycleStatus::Paused, 4);
        paused.available_actions = vec![SessionRunAction::Resume, SessionRunAction::Cancel];
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![paused],
            false,
        ));
        assert!(widget.mark_agent_control_pending("server-child", SessionRunAction::Resume));
        assert_eq!(
            widget.agent_run_state("server-child").unwrap().status,
            AgentRunStatus::Resuming
        );

        let mut resumed = server_run_node("server-child", SessionRunLifecycleStatus::Running, 5);
        resumed.available_actions = vec![SessionRunAction::Pause, SessionRunAction::Cancel];
        widget.reconcile_server_agent_projection(&server_agent_projection(
            crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
            vec![resumed],
            false,
        ));
        let state = widget.agent_run_state("server-child").unwrap();
        assert_eq!(state.status, AgentRunStatus::Running);
        assert_eq!(state.confidence, AgentProjectionConfidence::Confirmed);
        assert!(!widget.reject_agent_control("server-child"));
    }

    #[test]
    fn runtime_profile_does_not_masquerade_as_execution_provenance() {
        use astra_thin_client::{SessionRunAction, SessionRunLifecycleStatus};

        for runtime_profile in ["request_scoped_runtime_mcp", "agent_binding_registry"] {
            let mut widget = fresh();
            let mut node = server_run_node(
                &format!("{runtime_profile}-child"),
                SessionRunLifecycleStatus::Running,
                1,
            );
            node.runtime.runtime_profile = Some(runtime_profile.into());
            node.available_actions = vec![SessionRunAction::Pause, SessionRunAction::Cancel];
            widget.reconcile_server_agent_projection(&server_agent_projection(
                crate::tui::server_agent_observer::ServerAgentTruthState::Confirmed,
                vec![node],
                false,
            ));

            let row = widget.agent_monitor_snapshot(1).into_iter().next().unwrap();
            assert_eq!(row.state.source, AgentProjectionSource::DurableServer);
            assert_eq!(row.provenance, AgentProjectionSource::DurableServer);
            assert_eq!(
                row.available_actions,
                vec![SessionRunAction::Pause, SessionRunAction::Cancel]
            );
            assert_eq!(
                row.control_target,
                Some(
                    crate::tui::agent_run_projection::AgentControlTarget::DurableRun {
                        run_id: format!("{runtime_profile}-child"),
                    }
                )
            );
        }
    }

    #[test]
    fn local_runtime_derives_nested_lineage_without_counting_children_as_tools() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut parent = local_agent_info(
            "parent-agent",
            AgentStatus::Running {
                activity: "delegating".into(),
            },
        );
        parent.metrics.tool_calls = 4;
        let mut child = local_agent_info(
            "child-agent",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
        );
        child.parent_run_id = parent.run_id.clone();
        child.metrics.tool_calls = 1;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(&local_agent_snapshot(vec![child, parent]), &[]);
        let rows = widget.agent_monitor_snapshot(0);
        assert_eq!(
            rows.iter()
                .map(|row| row.agent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["parent-agent", "child-agent"]
        );
        let parent = rows
            .iter()
            .find(|row| row.agent_id == "parent-agent")
            .unwrap();
        assert_eq!(parent.activity.tool_calls, 4);
        assert_eq!(parent.activity.child_agents, 1);
        assert!(!parent.activity.child_agents_partial);
        assert_eq!(parent.depth, 1);
        assert_eq!(parent.provenance, AgentProjectionSource::LocalRuntime);
        let child = rows
            .iter()
            .find(|row| row.agent_id == "child-agent")
            .unwrap();
        assert_eq!(child.activity.tool_calls, 1);
        assert_eq!(child.activity.child_agents, 0);
        assert_eq!(child.parent_run_id, parent.run_id);
        assert_eq!(child.depth, 2);
    }

    #[test]
    fn local_control_intent_keeps_runtime_provenance() {
        use astra_thin_client::SessionRunAction;
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "local-agent",
                AgentStatus::Running {
                    activity: "working".into(),
                },
            )]),
            &[],
        );
        assert!(widget.mark_agent_control_pending("local-agent", SessionRunAction::Cancel));

        let snapshot = widget.agent_monitor_snapshot(0);
        assert_eq!(snapshot[0].state.source, AgentProjectionSource::LocalIntent);
        assert_eq!(snapshot[0].provenance, AgentProjectionSource::LocalRuntime);
    }

    #[test]
    fn local_runtime_snapshot_confirms_live_agent_and_authoritative_result() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@local".into(),
            kind: AgentLiveEventKind::OutputDelta("streaming finding".into()),
        })));
        assert_eq!(
            widget
                .agent_run_state("reviewer@local")
                .expect("live projection")
                .confidence,
            AgentProjectionConfidence::Observed
        );

        let mut agent = local_agent_info(
            "reviewer@local",
            AgentStatus::Completed {
                result: "authoritative result".into(),
                finish_reason: Some("normal".into()),
            },
        );
        agent.metrics.tool_calls = 4;
        agent.metrics.permission_requests = 3;
        agent.metrics.permission_requests_approved = 2;
        agent.metrics.tools_blocked = 1;
        agent.has_permission_issues = true;
        agent.run_in_background = true;
        assert!(widget.reconcile_local_agent_snapshot(&local_agent_snapshot(vec![agent]), &[]));

        let state = widget
            .agent_run_state("reviewer@local")
            .expect("reconciled projection");
        assert_eq!(state.status, AgentRunStatus::Completed);
        assert_eq!(state.confidence, AgentProjectionConfidence::Confirmed);
        assert_eq!(state.source, AgentProjectionSource::LocalRuntime);
        let detail = widget
            .agent_run_cell("reviewer@local")
            .expect("reconciled detail");
        assert_eq!(
            detail.output_summary.as_deref(),
            Some("authoritative result")
        );
        let monitor = widget.agent_monitor_snapshot(5);
        let row = &monitor[0];
        assert_eq!(
            row.activity.tool_calls, 4,
            "runtime metrics remain visible even when child events were missed"
        );
        assert_eq!(row.runtime.runtime_profile.as_deref(), Some("cli_local"));
        assert_eq!(row.runtime.agent_binding_name.as_deref(), Some("reviewer"));
        assert_eq!(row.runtime.background, Some(true));
        assert_eq!(
            row.runtime.permission,
            Some(astra_thin_client::SessionRunPermissionFacts {
                has_issues: true,
                requests: 3,
                approved: 2,
                tools_blocked: 1,
            })
        );
    }

    #[test]
    fn parent_turn_boundary_does_not_downgrade_runtime_confirmation() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        let snapshot = local_agent_snapshot(vec![local_agent_info(
            "reviewer@running",
            AgentStatus::Running {
                activity: "reviewing".into(),
            },
        )]);
        widget.reconcile_local_agent_snapshot(&snapshot, &[]);

        widget.handle_event(AppEvent::User(UserEvent::Submit("continue".into())));

        let state = widget.agent_run_state("reviewer@running").unwrap();
        assert_eq!(state.status, AgentRunStatus::Running);
        assert_eq!(state.confidence, AgentProjectionConfidence::Confirmed);
        assert_eq!(state.source, AgentProjectionSource::LocalRuntime);
    }

    #[test]
    fn missing_agent_in_available_runtime_snapshot_becomes_stale_not_failed() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "reviewer@missing",
                AgentStatus::Running {
                    activity: "reviewing".into(),
                },
            )]),
            &[],
        );

        assert!(widget.reconcile_local_agent_snapshot(&local_agent_snapshot(Vec::new()), &[]));

        let state = widget.agent_run_state("reviewer@missing").unwrap();
        assert_eq!(state.status, AgentRunStatus::Running);
        assert_eq!(state.confidence, AgentProjectionConfidence::Stale);
        assert!(!state.is_actionable_active());
        assert_eq!(
            widget.agent_run_cell("reviewer@missing").unwrap().status,
            crate::tui::history_cell::task::TaskStatus::Unconfirmed
        );
    }

    #[test]
    fn agent_live_transcript_replay_preserves_events_observed_before_open() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut widget = fresh();
        for kind in [
            AgentLiveEventKind::ThinkingDelta("inspect ".into()),
            AgentLiveEventKind::ThinkingDelta("ownership".into()),
            AgentLiveEventKind::OutputDelta("finding ".into()),
            AgentLiveEventKind::OutputDelta("one".into()),
            AgentLiveEventKind::ToolStarted {
                name: "read_file".into(),
                description: "src/lib.rs".into(),
                tool_use_id: "call-1".into(),
            },
            AgentLiveEventKind::AgentTerminated {
                termination: astra_turn_core::agent_live_event::AgentLiveTermination::Completed,
                duration_ms: 42,
                reason: Some("review complete".into()),
            },
        ] {
            widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
                run_id: "run-review".into(),
                agent_id: "reviewer".into(),
                kind,
            })));
        }

        let (events, dropped) = widget.agent_live_transcript_replay("reviewer", "run-review");
        assert_eq!(dropped, 0);
        assert_eq!(events.len(), 4, "adjacent deltas should be coalesced");
        assert!(matches!(
            &events[0].kind,
            AgentLiveEventKind::ThinkingDelta(text) if text == "inspect ownership"
        ));
        assert!(matches!(
            &events[1].kind,
            AgentLiveEventKind::OutputDelta(text) if text == "finding one"
        ));
        assert!(matches!(
            &events[2].kind,
            AgentLiveEventKind::ToolStarted { tool_use_id, .. } if tool_use_id == "call-1"
        ));
        assert!(matches!(
            &events[3].kind,
            AgentLiveEventKind::AgentTerminated { reason: Some(reason), .. }
                if reason == "review complete"
        ));
    }

    #[test]
    fn workspace_projection_is_stale_evidence_and_unknown_status_is_not_invented() {
        let mut widget = fresh();
        let restored = vec![
            restored_local_agent("reviewer@restored", "running"),
            restored_local_agent("reviewer@unknown", "future_status"),
        ];

        assert!(widget.reconcile_local_agent_snapshot(
            &crate::tui::local_agent_snapshot::LocalAgentSnapshot::default(),
            &restored,
        ));

        let state = widget.agent_run_state("reviewer@restored").unwrap();
        assert_eq!(state.status, AgentRunStatus::Running);
        assert_eq!(state.confidence, AgentProjectionConfidence::Stale);
        assert_eq!(state.source, AgentProjectionSource::WorkspaceSnapshot);
        assert!(!state.is_actionable_active());
        assert!(widget.agent_run_state("reviewer@unknown").is_none());

        let row = widget
            .agent_monitor_snapshot(10)
            .rows
            .into_iter()
            .find(|row| row.agent_id == "reviewer@restored")
            .expect("restored agent row");
        assert_eq!(row.run_id.as_deref(), Some("run-reviewer@restored"));
        assert_eq!(row.parent_run_id.as_deref(), Some("root"));
        assert_eq!(row.depth, 1);
        assert_eq!(
            row.transcript_target,
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal)
        );
        assert!(row.available_actions.is_empty());
    }

    #[test]
    fn restored_workspace_rebuilds_nested_run_lineage_for_transcript_navigation() {
        let mut widget = fresh();
        let parent = restored_local_agent("reviewer@parent", "completed");
        let mut child = restored_local_agent("reviewer@child", "running");
        child.parent_run_id = parent.run_id.clone();

        widget.reconcile_local_agent_snapshot(
            &crate::tui::local_agent_snapshot::LocalAgentSnapshot::default(),
            &[parent.clone(), child],
        );

        let rows = widget.agent_monitor_snapshot(10).rows;
        let parent_row = rows
            .iter()
            .find(|row| row.run_id.as_deref() == Some(parent.run_id.as_str()))
            .expect("parent row");
        let child_row = rows
            .iter()
            .find(|row| row.agent_id == "reviewer@child")
            .expect("child row");
        assert_eq!(parent_row.depth, 1);
        assert_eq!(
            child_row.parent_run_id.as_deref(),
            Some(parent.run_id.as_str())
        );
        assert_eq!(child_row.depth, 2);
        assert_eq!(
            child_row.transcript_target,
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal)
        );
    }

    #[test]
    fn state_authority_prevents_regression_but_allows_owner_repair() {
        let mut projection = AgentRunProjection::new(
            "agent".into(),
            "agent".into(),
            AgentRunState::confirmed_local(AgentRunStatus::Completed),
        );
        projection.set_state(AgentRunState::observed(AgentRunStatus::Failed));
        assert_eq!(
            projection.state,
            AgentRunState::confirmed_local(AgentRunStatus::Completed),
            "late stream terminal state must not replace the runtime terminal state"
        );
        projection.set_state(AgentRunState::confirmed_server(AgentRunStatus::Running));
        assert_eq!(
            projection.state,
            AgentRunState::confirmed_local(AgentRunStatus::Completed),
            "a stale higher-ranked server poll must not reopen a confirmed terminal run"
        );

        let mut repaired = AgentRunProjection::new(
            "agent".into(),
            "agent".into(),
            AgentRunState::observed(AgentRunStatus::Completed),
        );
        repaired.set_state(AgentRunState::confirmed_local(AgentRunStatus::Running));
        assert_eq!(
            repaired.state,
            AgentRunState::confirmed_local(AgentRunStatus::Running),
            "the owning runtime may reopen an incorrect live-stream terminal projection"
        );

        repaired.set_state(AgentRunState::observed(AgentRunStatus::Running));
        assert_eq!(
            repaired.state.confidence,
            AgentProjectionConfidence::Confirmed,
            "same-status live events must not erase owner confirmation"
        );
    }

    #[test]
    fn sparse_runtime_facts_merge_per_field_without_erasing_local_evidence() {
        let mut projection = AgentRunProjection::new(
            "agent".into(),
            "agent".into(),
            AgentRunState::confirmed_local(AgentRunStatus::Running),
        );
        projection.set_runtime_facts(
            AgentProjectionSource::LocalRuntime,
            astra_thin_client::SessionRunRuntimeFacts {
                background: Some(true),
                permission: Some(astra_thin_client::SessionRunPermissionFacts {
                    has_issues: true,
                    requests: 2,
                    approved: 1,
                    tools_blocked: 1,
                }),
                ..Default::default()
            },
        );
        projection.set_runtime_facts(
            AgentProjectionSource::DurableServer,
            astra_thin_client::SessionRunRuntimeFacts {
                offering_id: Some("offer-gpt-5".into()),
                model_name: Some("gpt-5".into()),
                ..Default::default()
            },
        );

        assert_eq!(projection.runtime_facts.background, Some(true));
        assert!(projection.runtime_facts.permission.is_some());
        assert_eq!(
            projection.runtime_facts.offering_id.as_deref(),
            Some("offer-gpt-5")
        );
        assert_eq!(
            projection.runtime_facts.model_name.as_deref(),
            Some("gpt-5")
        );
    }

    #[test]
    fn stale_poll_does_not_clear_pending_control_overlay() {
        let mut projection = AgentRunProjection::new(
            "agent".into(),
            "agent".into(),
            AgentRunState::confirmed_server(AgentRunStatus::Running),
        );
        projection.available_actions = vec![astra_thin_client::SessionRunAction::Pause];
        assert!(projection.begin_control(astra_thin_client::SessionRunAction::Pause));
        assert_eq!(projection.state.status, AgentRunStatus::Pausing);

        assert!(!projection.set_state(AgentRunState::confirmed_server(AgentRunStatus::Running)));
        assert_eq!(projection.state.status, AgentRunStatus::Pausing);
        assert!(projection.control_requested_from.is_some());

        assert!(projection.set_state(AgentRunState::confirmed_server(AgentRunStatus::Paused)));
        assert_eq!(projection.state.status, AgentRunStatus::Paused);
        assert!(projection.control_requested_from.is_none());
    }

    #[test]
    fn rejected_late_terminal_event_cannot_corrupt_authoritative_detail() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "reviewer@authoritative",
                AgentStatus::Completed {
                    result: "verified result".into(),
                    finish_reason: Some("normal".into()),
                },
            )]),
            &[],
        );

        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(AgentLiveEvent {
            run_id: "test-run".into(),
            agent_id: "reviewer@authoritative".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Failed,
                duration_ms: 999,
                reason: Some("late stream failure".into()),
            },
        })));

        assert_eq!(
            widget.agent_run_state("reviewer@authoritative").unwrap(),
            AgentRunState::confirmed_local(AgentRunStatus::Completed)
        );
        let detail = widget.agent_run_cell("reviewer@authoritative").unwrap();
        assert_eq!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Completed
        );
        assert_eq!(detail.output_summary.as_deref(), Some("verified result"));
        assert!(detail.error.is_none());
    }

    #[test]
    fn rejected_control_result_cannot_corrupt_authoritative_detail() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "reviewer@control",
                AgentStatus::Completed {
                    result: "verified result".into(),
                    finish_reason: Some("normal".into()),
                },
            )]),
            &[],
        );
        widget.handle_event(AppEvent::wire(agent_control_completed(
            "get_result",
            "reviewer",
            "completed",
            999,
            Some(
                r#"{"status":"failed","agent_id":"reviewer@control","error":"late control failure"}"#,
            ),
            "late-result-tool",
            Some("reviewer@control"),
        )));

        assert_eq!(
            widget.agent_run_state("reviewer@control").unwrap(),
            AgentRunState::confirmed_local(AgentRunStatus::Completed)
        );
        let detail = widget.agent_run_cell("reviewer@control").unwrap();
        assert_eq!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Completed
        );
        assert_eq!(detail.output_summary.as_deref(), Some("verified result"));
        assert!(detail.error.is_none());
    }

    #[test]
    fn reconciliation_detects_equal_length_result_changes() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        let first = local_agent_snapshot(vec![local_agent_info(
            "reviewer@repair",
            AgentStatus::Completed {
                result: "done".into(),
                finish_reason: Some("normal".into()),
            },
        )]);
        assert!(widget.reconcile_local_agent_snapshot(&first, &[]));

        let repaired = local_agent_snapshot(vec![local_agent_info(
            "reviewer@repair",
            AgentStatus::Completed {
                result: "over".into(),
                finish_reason: Some("normal".into()),
            },
        )]);
        assert!(widget.reconcile_local_agent_snapshot(&repaired, &[]));
        assert_eq!(
            widget
                .agent_run_cell("reviewer@repair")
                .unwrap()
                .output_summary
                .as_deref(),
            Some("over")
        );
    }

    #[test]
    fn local_waiting_state_remains_live_but_is_not_rendered_as_working() {
        use astra_turn_core::orchestration_types::AgentStatus;

        let mut widget = fresh();
        widget.reconcile_local_agent_snapshot(
            &local_agent_snapshot(vec![local_agent_info(
                "reviewer@waiting",
                AgentStatus::Waiting {
                    reason: "needs input".into(),
                },
            )]),
            &[],
        );

        let state = widget.agent_run_state("reviewer@waiting").unwrap();
        assert_eq!(state.status, AgentRunStatus::Waiting);
        assert!(state.is_actionable_active());
        assert_eq!(
            widget.agent_run_cell("reviewer@waiting").unwrap().status,
            crate::tui::history_cell::task::TaskStatus::Waiting
        );
    }

    #[test]
    fn typed_communication_updates_the_matching_agent_activity() {
        let mut widget = fresh();
        widget.agent_runs.ensure(
            "reviewer".into(),
            "Review patch".into(),
            AgentRunState::observed(AgentRunStatus::Running),
        );
        widget
            .agent_runs
            .get_mut("reviewer")
            .unwrap()
            .set_runtime_metadata(
                AgentProjectionSource::LiveStream,
                "run-review".into(),
                Some("run-root".into()),
                1,
                0,
            );

        let base = astra_turn_types::AgentCommunicationEvent {
            schema_version: astra_turn_types::AGENT_COMMUNICATION_SCHEMA_VERSION.into(),
            observed_by: astra_turn_types::AgentCommunicationParty {
                run_id: "run-review".into(),
                agent_id: "reviewer".into(),
            },
            direction: astra_turn_types::AgentCommunicationDirection::Sent,
            message_id: "msg-1".into(),
            from: astra_turn_types::AgentCommunicationParty {
                run_id: "run-review".into(),
                agent_id: "reviewer".into(),
            },
            to: astra_turn_types::AgentCommunicationTarget::Parent,
            payload_kind: "progress".into(),
            summary: Some("review started".into()),
            response_accepted: None,
            related_message_id: None,
            timestamp_ms: 42,
            correlation_id: None,
            requires_ack: false,
        };
        widget.handle_event(AppEvent::wire(WireEvent::AgentCommunication(base.clone())));
        widget.handle_event(AppEvent::wire(WireEvent::AgentCommunication(
            astra_turn_types::AgentCommunicationEvent {
                direction: astra_turn_types::AgentCommunicationDirection::Received,
                message_id: "msg-2".into(),
                ..base.clone()
            },
        )));
        widget.handle_event(AppEvent::wire(WireEvent::AgentLive(
            astra_turn_core::agent_live_event::AgentLiveEvent {
                run_id: "test-run".into(),
                agent_id: "reviewer".into(),
                kind: astra_turn_core::agent_live_event::AgentLiveEventKind::Signal(
                    astra_turn_core::agent_live_event::AgentLiveSignal::AgentCommunication(
                        astra_turn_types::AgentCommunicationEvent {
                            message_id: "msg-3".into(),
                            ..base
                        },
                    ),
                ),
            },
        )));

        let rows = widget.agent_workbench_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].activity.messages_sent, 2);
        assert_eq!(rows[0].activity.messages_received, 1);
    }

    #[test]
    fn durable_local_journal_restores_completed_agent_as_inspectable_transcript() {
        let mut widget = fresh();
        let restored = crate::tui::local_agent_journal::LocalJournalAgentRun {
            agent_id: "reviewer@archive".into(),
            run_id: "run-archived-review".into(),
            parent_run_id: Some("root-run".into()),
            description: "Review the storage patch".into(),
            status: "completed".into(),
            duration_ms: 4_200,
            tool_calls: 7,
        };

        assert!(widget.reconcile_local_agent_journal_runs(&[restored]));

        let rows = widget.agent_workbench_snapshot();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state.status, AgentRunStatus::Completed);
        assert_eq!(rows[0].provenance, AgentProjectionSource::LocalJournal);
        assert_eq!(rows[0].run_id.as_deref(), Some("run-archived-review"));
        assert_eq!(
            rows[0].transcript_target,
            Some(crate::tui::agent_run_projection::AgentTranscriptTarget::LocalJournal)
        );
        assert_eq!(rows[0].activity.tool_calls, 7);
        assert_eq!(rows[0].elapsed_ms, 4_200);
    }

    #[test]
    fn terminal_journal_cancelled_status_stays_cancelled() {
        assert_eq!(
            restored_agent_run_status("cancelled"),
            Some(AgentRunStatus::Cancelled)
        );
    }

    #[test]
    fn terminal_working_set_preserves_active_runs_and_recent_lineage() {
        let mut registry = AgentRunRegistry::default();
        for (key, status, parent) in [
            ("parent", AgentRunStatus::Completed, None),
            ("evict", AgentRunStatus::Completed, None),
            ("child", AgentRunStatus::Completed, Some("run-parent")),
            ("latest", AgentRunStatus::Completed, None),
            ("active", AgentRunStatus::Running, None),
        ] {
            registry.ensure(
                key.to_string(),
                key.to_string(),
                AgentRunState::observed(status),
            );
            registry.get_mut(key).unwrap().set_runtime_metadata(
                AgentProjectionSource::LiveStream,
                format!("run-{key}"),
                parent.map(str::to_string),
                u32::from(parent.is_some()),
                0,
            );
        }

        registry.prune_terminal_history(2);

        assert!(registry.contains_key("active"));
        assert!(registry.contains_key("child"));
        assert!(registry.contains_key("latest"));
        assert!(registry.contains_key("parent"));
        assert!(!registry.contains_key("evict"));
    }
}
