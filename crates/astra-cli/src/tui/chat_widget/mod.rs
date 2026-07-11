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
//!                                         ──▶ append TurnEvent to disk
//!                                         ──▶ (outer draws on next frame)
//! ```
//!
//! `handle_event` is deliberately one big `match` (§3.2 of the
//! design doc). A reducer abstraction was tried and failed — the
//! async HTTP stream + direct terminal IO don't map cleanly to pure
//! `State, Action -> State`. One readable match beats a reducer that
//! leaks `Effect`s everywhere.
//!
//! All non-trait callers still live in `tui/mod.rs` in Phase 3 —
//! this module just provides the target API. Wire-up comes in
//! step 3d.

mod agent_control_surface;
mod bridge;
mod resume;
#[cfg(test)]
mod turn_driver;

use self::agent_control_surface::{AgentControlOutcome, AgentControlSurface, CancelledStateUpdate};
pub(crate) use bridge::{TurnContext, translate};
pub(crate) use resume::load as load_resume;

use std::sync::Arc;

use super::history_cell::{
    HistoryCell, assistant::AssistantCell, reasoning::ReasoningCell, system::SystemCell,
    task::TaskCell, tool::ToolCell, turn_summary::TurnSummaryCell, user::UserCell,
};
use super::transcript_jsonl;
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
    Wire(WireEvent),
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
#[derive(Default)]
struct AgentRunRegistry {
    runs: std::collections::HashMap<String, Box<TaskCell>>,
    order: Vec<String>,
    fanout_membership: std::collections::HashMap<
        String,
        crate::tui::bottom_pane::in_flight_agents_view::AgentFanoutMembership,
    >,
    /// Map of `tool_use_id → registry key` so a follow-up event for
    /// the same tool call (e.g. a generic `ToolStarted` after a
    /// structured `AgentControlStarted` already established the
    /// row) can reuse the existing entry instead of creating a
    /// duplicate row keyed by the provisional fallback.
    tool_use_to_key: std::collections::HashMap<String, String>,
}

impl AgentRunRegistry {
    fn ids(&self) -> Vec<String> {
        self.order.clone()
    }

    fn get(&self, id: &str) -> Option<&TaskCell> {
        self.runs.get(id).map(Box::as_ref)
    }

    fn get_mut(&mut self, id: &str) -> Option<&mut TaskCell> {
        self.runs.get_mut(id).map(Box::as_mut)
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

    fn contains_key(&self, id: &str) -> bool {
        self.runs.contains_key(id)
    }

    /// Fetch the registry key bound to `tool_use_id`, if any.
    fn key_for_tool_use(&self, tool_use_id: &str) -> Option<&str> {
        self.tool_use_to_key.get(tool_use_id).map(String::as_str)
    }

    fn tool_uses_for_key(&self, key: &str) -> Vec<String> {
        self.tool_use_to_key
            .iter()
            .filter(|(_, value)| value.as_str() == key)
            .map(|(tool_use_id, _)| tool_use_id.clone())
            .collect()
    }

    fn bound_tool_use_ids(&self) -> Vec<String> {
        self.tool_use_to_key.keys().cloned().collect()
    }

    /// Ensure a row exists for `id` and bind `tool_use_id` to it so
    /// follow-up events can find the same row.
    fn ensure_running_for_tool_use(
        &mut self,
        id: String,
        label: String,
        tool_use_id: Option<&str>,
    ) {
        self.ensure_running(id.clone(), label);
        if let Some(tu) = tool_use_id {
            self.tool_use_to_key.entry(tu.to_string()).or_insert(id);
        }
    }

    fn ensure_running(&mut self, id: String, label: String) {
        if let Some(cell) = self.runs.get_mut(&id) {
            cell.description = label;
            cell.status = crate::tui::history_cell::task::TaskStatus::Running;
            cell.duration_ms = None;
            return;
        }

        self.order.push(id.clone());
        self.runs
            .insert(id.clone(), Box::new(TaskCell::new_running(id, label)));
    }

    fn rename(&mut self, old: &str, new: String) {
        if old == new || !self.runs.contains_key(old) {
            return;
        }
        if let Some(cell) = self.runs.remove(old) {
            if let Some(existing) = self.runs.get_mut(&new) {
                merge_agent_task_cells(existing.as_mut(), *cell);
            } else {
                self.runs.insert(new.clone(), cell);
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
        for value in self.tool_use_to_key.values_mut() {
            if value == old {
                *value = new.clone();
            }
        }
    }
}

fn merge_agent_task_cells(target: &mut TaskCell, source: crate::tui::history_cell::task::TaskCell) {
    use crate::tui::history_cell::task::{ChildStatus, TaskStatus};

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
    if target.error.is_none() {
        target.error = error;
    }
    target.ctrl_b_background_hint |= ctrl_b_background_hint;
    if target
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
    if matches!(target.status, TaskStatus::Running) && !matches!(status, TaskStatus::Running) {
        target.status = status;
        target.completed_at = completed_at;
        target.duration_ms = duration_ms;
    } else if target.completed_at.is_none() {
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
pub(crate) struct ChatWidget {
    session_id: String,
    history: Vec<Arc<dyn HistoryCell>>,
    active_cell: Option<Box<dyn HistoryCell>>,
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
    /// Index into `history` marking cells that have already been
    /// flushed to the terminal scrollback. `drain_new_committed`
    /// returns everything past this index and advances it.
    committed_watermark: usize,
    /// Index into `history` marking cells that have already been
    /// persisted to the JSONL transcript. Starts at 0; advanced by
    /// `persist_from_watermark`. Kept separate from the display
    /// watermark because their lifecycles diverge — a cell is
    /// committed to scrollback as soon as it finalises, but may be
    /// held back from disk if the server hasn't yet assigned a
    /// session id (turn 1 edge case). When `set_session_id` is
    /// eventually called, we drain this watermark to persist every
    /// cell accumulated in the meantime.
    persist_watermark: usize,
    /// `tool_use_id`s of TaskCells spawned in the current turn that
    /// have not yet reached a terminal state. Ctrl+C on the parent
    /// turn cascades cancel to every id in this set; an individual
    /// TaskCell's Esc handler removes just its own id. Cleared at
    /// turn boundaries.
    in_flight_task_ids: Vec<String>,
    /// Agent/control ids that have received a local cancel request but
    /// have not yet delivered their terminal event.
    cancelling_task_ids: std::collections::HashSet<String>,
    /// Logical agent ids that ended via cancellation. TaskCell has only
    /// Completed/Failed, so keep this alongside the cell for row status.
    cancelled_task_ids: std::collections::HashSet<String>,
    /// Agent ids cleared at a turn boundary. Late terminal events for
    /// these prior-turn agents must not resurrect rows in the next turn.
    cleared_agent_run_ids: std::collections::HashSet<String>,
    /// Control tool ids cleared at a turn boundary. Used to detect
    /// late `AgentControlCompleted` arrivals after the row was dropped.
    cleared_agent_tool_use_ids: std::collections::HashSet<String>,
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
            active_cell: None,
            live_tasks: std::collections::HashMap::new(),
            live_task_order: Vec::new(),
            agent_runs: AgentRunRegistry::default(),
            committed_watermark: 0,
            persist_watermark: 0,
            in_flight_task_ids: Vec::new(),
            cancelling_task_ids: std::collections::HashSet::new(),
            cancelled_task_ids: std::collections::HashSet::new(),
            cleared_agent_run_ids: std::collections::HashSet::new(),
            cleared_agent_tool_use_ids: std::collections::HashSet::new(),
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

    pub fn agent_run_cell(&self, id: &str) -> Option<&TaskCell> {
        self.agent_runs.get(id)
    }

    #[cfg(test)]
    pub(crate) fn set_agent_completed_at_for_test(
        &mut self,
        id: &str,
        completed_at: std::time::Instant,
    ) {
        if let Some(tc) = self.agent_runs.get_mut(id) {
            tc.completed_at = Some(completed_at);
        }
    }

    /// Whether the user has issued a cancel for this agent that has
    /// already terminated. Distinct from Failed so the strip / drill
    /// view can render a different icon (■, dim) — a cancelled run is
    /// the user's intent, not an alarm.
    pub fn agent_is_cancelled(&self, id: &str) -> bool {
        self.cancelled_task_ids.contains(id)
    }

    /// Whether the user has issued a cancel for this agent that has
    /// NOT yet terminated. Distinct from `agent_is_cancelled` so the
    /// strip can show "Cancelling…" while the cancel is in flight.
    pub fn agent_is_cancelling(&self, id: &str) -> bool {
        self.cancelling_task_ids.contains(id)
    }

    fn agent_row_status(
        &self,
        id: &str,
        cell: &TaskCell,
    ) -> crate::tui::bottom_pane::in_flight_agents_view::AgentRowStatus {
        use crate::tui::bottom_pane::in_flight_agents_view::AgentRowStatus;
        if self.cancelled_task_ids.contains(id) {
            AgentRowStatus::Cancelled
        } else if matches!(
            cell.status,
            crate::tui::history_cell::task::TaskStatus::Failed
        ) {
            AgentRowStatus::Failed
        } else if matches!(
            cell.status,
            crate::tui::history_cell::task::TaskStatus::Completed
        ) {
            AgentRowStatus::Completed
        } else if matches!(
            cell.status,
            crate::tui::history_cell::task::TaskStatus::Interrupted
        ) {
            AgentRowStatus::Interrupted
        } else if self.cancelling_task_ids.contains(id) {
            AgentRowStatus::Cancelling
        } else {
            AgentRowStatus::Live
        }
    }

    /// Look up a TaskCell by id in either the live register or in
    /// history. Used by the Ctrl+G drilldown to open a TaskDetailView
    /// for agents that have already completed (the live register
    /// drains them on completion). Live cells take priority because
    /// they have fresher elapsed/child counts; if a stale duplicate
    /// id ever lived in both places, the live one wins.
    pub fn task_cell_anywhere(&self, id: &str) -> Option<&TaskCell> {
        if let Some(tc) = self.agent_runs.get(id) {
            return Some(tc);
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
    pub fn agents_drilldown_rows(
        &self,
        max_recent_completed: usize,
    ) -> Vec<crate::tui::bottom_pane::in_flight_agents_view::AgentRow> {
        use crate::tui::bottom_pane::in_flight_agents_view::{AgentRow, AgentRowStatus};

        let registry_rows: Vec<AgentRow> = self
            .agent_runs
            .order
            .iter()
            .filter_map(|id| {
                self.agent_runs.get(id).map(|tc| AgentRow {
                    agent_id: id.clone(),
                    name: tc.description.clone(),
                    child_count: tc.children.len(),
                    elapsed_ms: tc
                        .duration_ms
                        .unwrap_or_else(|| tc.started_at.elapsed().as_millis() as u64),
                    status: self.agent_row_status(id, tc),
                    fanout: self.agent_runs.fanout_membership(id).cloned(),
                })
            })
            .collect();

        if !registry_rows.is_empty() {
            // Live agents always appear; completed/failed are capped at
            // `max_recent_completed` so a small Ctrl+G list doesn't blow
            // up after a long-running session that's spawned dozens of
            // sub-agents. `max_recent_completed=0` means strip-mirror
            // mode (only-live).
            let mut rows: Vec<AgentRow> = registry_rows
                .iter()
                .filter(|row| {
                    row.status == AgentRowStatus::Live || row.status == AgentRowStatus::Cancelling
                })
                .cloned()
                .collect();
            rows.extend(
                registry_rows
                    .into_iter()
                    .filter(|row| {
                        row.status != AgentRowStatus::Live
                            && row.status != AgentRowStatus::Cancelling
                    })
                    .take(max_recent_completed),
            );
            return rows;
        }

        let is_control_plane_agent_task = |tc: &TaskCell| {
            tc.description.starts_with("Spawn agent:")
                || tc.description.starts_with("Get agent result:")
        };
        let is_terminal = |tc: &TaskCell| {
            !matches!(
                tc.status,
                crate::tui::history_cell::task::TaskStatus::Running
            )
        };

        // Legacy replay fallback: old transcripts only have generic
        // ToolStarted/ToolCompleted events, not AgentControl* events.
        let mut rows: Vec<AgentRow> = self
            .live_task_order
            .iter()
            .filter_map(|id| {
                self.live_tasks.get(id).and_then(|tc| {
                    if is_control_plane_agent_task(tc) {
                        None
                    } else {
                        Some(AgentRow {
                            agent_id: id.clone(),
                            name: tc.description.clone(),
                            child_count: tc.children.len(),
                            elapsed_ms: tc.started_at.elapsed().as_millis() as u64,
                            status: if self.cancelling_task_ids.contains(id) {
                                AgentRowStatus::Cancelling
                            } else {
                                AgentRowStatus::Live
                            },
                            fanout: None,
                        })
                    }
                })
            })
            .collect();

        if max_recent_completed > 0 {
            let completed: Vec<AgentRow> = self
                .history
                .iter()
                .rev()
                .filter_map(|cell| cell.as_any_ref().downcast_ref::<TaskCell>())
                .filter(|tc| is_terminal(tc) && !is_control_plane_agent_task(tc))
                .take(max_recent_completed)
                .map(|tc| AgentRow {
                    agent_id: tc.tool_use_id.clone(),
                    name: tc.description.clone(),
                    child_count: tc.children.len(),
                    elapsed_ms: tc
                        .duration_ms
                        .unwrap_or_else(|| tc.started_at.elapsed().as_millis() as u64),
                    status: task_status_to_agent_row_status(tc.status),
                    fanout: None,
                })
                .collect();
            rows.extend(completed);
        }

        rows
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

    pub fn mark_agent_controls_cancelling(&mut self, ids: &[String]) {
        for id in ids {
            self.cancelling_task_ids.insert(id.clone());
            if let Some(tc) = self.live_tasks.get_mut(id) {
                append_agent_live_output(tc, "\nCancelling…\n");
            }
            let Some(agent_key) = self.agent_runs.key_for_tool_use(id).map(str::to_owned) else {
                continue;
            };
            self.cancelling_task_ids.insert(agent_key.clone());
            if let Some(tc) = self.agent_runs.get_mut(&agent_key) {
                append_agent_live_output(tc, "\nCancelling…\n");
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

    pub fn active_cell(&self) -> Option<&dyn HistoryCell> {
        self.active_cell.as_deref()
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
    /// - If cells accumulated under an empty sid (turn-1 edge case:
    ///   server hadn't assigned one yet), they get flushed to the
    ///   new session's JSONL transcript on first assignment. This
    ///   is what lets resume replay show the user's very first
    ///   message instead of starting mid-conversation.
    /// - Cells already persisted under a non-empty sid stay in
    ///   their original transcript; only the new cells ride under
    ///   the new id.
    pub fn set_session_id(&mut self, sid: impl Into<String>) {
        self.session_id = sid.into();
        // Flush any cells that accumulated while sid was empty.
        self.persist_from_watermark();
    }

    /// Replay a previously-persisted turn stream into `history`.
    /// Used by the Phase 4 resume path. Cells land already
    /// finalised — no live state, no further mutation.
    ///
    /// Advances the persist watermark past the replayed cells so
    /// subsequent `commit_*` calls don't re-persist them to the
    /// JSONL (which would double every resumed line on every
    /// future write).
    pub fn replay(&mut self, events: Vec<TurnEvent>) {
        for ev in events {
            if let Some(cell) = cell_from_persist(ev) {
                self.history.push(cell.into());
            }
        }
        self.persist_watermark = self.history.len();
    }

    /// Commit a free-standing `SystemCell` — slash-command responses,
    /// info banners, inline errors, etc. Goes into `history` and the
    /// JSONL transcript the same way model-generated cells do, so
    /// resume replay surfaces them and the Ctrl+O overlay keeps them.
    ///
    /// Before this, slash-dispatch wrote system lines directly to the
    /// terminal via `queue_history_lines` — they showed in the live
    /// scrollback but never made it to disk, so a resumed session
    /// silently lost every `/model`, `/login`, `/permission` response
    /// as well as the `Session expired` / "token refreshed" banners.
    pub fn commit_system(&mut self, cell: SystemCell) {
        self.commit_active(); // finalise anything live first
        self.commit_cell(Box::new(cell));
    }

    /// Commit a user message directly into history without opening a new turn
    /// or draining the current live tool/assistant state.
    ///
    /// Used for deferred inputs that become active mid-turn: the transcript
    /// should show the newest user message as a first-class user row, but the
    /// current streaming turn should remain live until the runtime yields.
    pub fn commit_deferred_user(&mut self, text: impl Into<String>) {
        self.commit_cell(Box::new(UserCell::new(text.into())));
    }

    /// Single choke-point for routing events into state mutation.
    /// Any `AppEvent` emitted by the outer loop MUST go through
    /// here — nothing else in the TUI reaches into `history` or
    /// `active_cell`.
    pub fn handle_event(&mut self, ev: AppEvent) {
        match ev {
            AppEvent::User(ue) => self.handle_user(ue),
            AppEvent::Wire(we) => self.handle_wire(we),
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
        // orphans would persist into the new turn and `on_turn_complete`
        // of the new turn would force-fail them as if they belonged.
        self.commit_active();
        self.drain_all_live_tasks();
        // Drop agent_runs registry entries from the prior turn. The
        // strip's terminal rows (✓/✗) and stuck-Live rows from a
        // dropped server stream are turn-scoped UI; carrying them
        // forward grows the strip unboundedly ("12 parallel agents"
        // shown for a single new spawn). Drilldown for prior-turn
        // agents still works via the legacy history-fallback path in
        // `agents_drilldown_rows` (TaskCells are committed to history
        // before this point).
        self.clear_turn_scoped_agent_state();
        let cell = UserCell::new(text);
        self.commit_cell(Box::new(cell));
    }

    /// Finalize and commit every still-live parallel TaskCell.
    /// Each is `finalize()`d (Running → Failed with placeholder
    /// error) and moved into history in spawn order. Used by both
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

    fn forget_cleared_agent_markers(&mut self, agent_keys: &[&str], tool_use_id: Option<&str>) {
        for key in agent_keys {
            if !key.is_empty() {
                self.cleared_agent_run_ids.remove(*key);
            }
        }
        if let Some(tool_use_id) = tool_use_id {
            self.cleared_agent_tool_use_ids.remove(tool_use_id);
        }
    }

    fn clear_turn_scoped_agent_state(&mut self) {
        self.cleared_agent_run_ids.extend(self.agent_runs.ids());
        self.cleared_agent_tool_use_ids
            .extend(self.agent_runs.bound_tool_use_ids());
        self.agent_runs = AgentRunRegistry::default();
        self.cancelled_task_ids.clear();
    }

    fn on_answer_delta(&mut self, delta: &str) {
        // Tokens can begin flowing while a `ReasoningCell` is
        // still live (some providers end reasoning implicitly by
        // starting text); in that case we finalise the reasoning
        // cell first, then build a fresh AssistantCell.
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Reasoning)
        ) {
            self.commit_active();
        }

        // Create the AssistantCell on first delta if needed.
        if !matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Assistant)
        ) {
            self.active_cell = Some(Box::new(AssistantCell::new_streaming()));
        }

        if let Some(cell) = self.active_cell.as_mut()
            && let Some(ac) = cell.as_any_mut().downcast_mut::<AssistantCell>()
        {
            ac.push_delta(delta);
        }
    }

    fn on_reasoning_delta(&mut self, delta: &str) {
        // Reasoning arriving while a tool is live shouldn't
        // happen in practice, but just in case: commit the tool
        // cell first so the reasoning gets its own scrollback row
        // instead of overwriting.
        if matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ) {
            self.commit_active();
        }

        if !matches!(
            self.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Reasoning)
        ) {
            self.active_cell = Some(Box::new(ReasoningCell::new_streaming()));
        }

        if let Some(cell) = self.active_cell.as_mut()
            && let Some(rc) = cell.as_any_mut().downcast_mut::<ReasoningCell>()
        {
            rc.push_delta(delta);
        }
    }

    fn on_reasoning_done(&mut self) {
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
        // For `agent` tool calls we ALSO populate `agent_runs` (the
        // logical-row registry that powers the multi_agent strip and
        // the Ctrl+G drilldown). The companion
        // `WireEvent::AgentControlStarted` (emitted by stream_render
        // when it has access to structured args) is the canonical
        // path with the real agent_id; this fallback uses a
        // provisional id derived from the description so legacy
        // replay paths (which only get `ToolStarted`) still surface
        // a row. The standard task-like flow below populates
        // `live_tasks` + `in_flight_task_ids` so cancel-fanout,
        // child-event routing, and turn-drain invariants hold.
        if name == "agent" {
            self.on_agent_control_started(
                agent_action_from_description(&description)
                    .unwrap_or("agent")
                    .to_string(),
                agent_label_from_description(&description),
                tool_use_id.clone(),
                None,
                None,
                None,
            );
        }
        let agent_spawn_backgroundable =
            name == "agent" && agent_action_from_description(&description) == Some("spawn");

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
            self.commit_active();
            let mut cell = ToolCell::new_running(name, description);
            if cell.name == "bash" && self.bash_background_hint_enabled {
                cell.set_ctrl_b_background_hint(true);
            }
            self.active_cell = Some(Box::new(cell));
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
        let fanout_membership =
            fanout_slot.map(|slot| agent_fanout_membership(slot, fanout_title.as_deref(), &label));
        // If a prior event already bound this tool_use_id to a row
        // (e.g. structured AgentControlStarted established the real
        // agent_id, then a generic ToolStarted arrives without it),
        // reuse that key instead of creating a provisional duplicate.
        if agent_id.is_none()
            && let Some(existing) = self
                .agent_runs
                .key_for_tool_use(&tool_use_id)
                .map(str::to_string)
        {
            // Refresh the label / status on the existing entry but
            // don't create a new row.
            let existing_key = existing.clone();
            self.cancelled_task_ids.remove(&existing);
            self.agent_runs
                .ensure_running_for_tool_use(existing, label, Some(&tool_use_id));
            self.agent_runs
                .set_fanout_membership(&existing_key, fanout_membership);
            return;
        }

        let key = agent_id.unwrap_or_else(|| {
            if action == "agent" {
                tool_use_id.clone()
            } else {
                provisional_agent_key(&tool_use_id)
            }
        });
        let label = if action == "get_result" {
            key.strip_prefix("pending:")
                .map(|_| label.clone())
                .unwrap_or_else(|| agent_display_name(&key, Some(&label)))
        } else {
            label
        };
        self.cancelled_task_ids.remove(&key);
        // If a prior `ToolStarted("agent", tool_use_id)` already bound
        // `tool_use_id → <provisional key>` (either the bare
        // `tool_use_id` for action=="agent", or `pending:<tool_use_id>`
        // for action=="spawn"/"get_result"), and a structured
        // `AgentControlStarted` now arrives with the real `agent_id`,
        // explicitly rename. Without this, `tool_use_to_key` points at
        // the provisional key forever and `tool_uses_for_key(real_agent_id)`
        // returns empty — child tool events never reach the parent
        // task panel.
        //
        // Guard with the provisional-key shape so a stray duplicate
        // ToolStarted whose agent_id binding has already been promoted
        // to a real id can't clobber the canonical row.
        let provisional = provisional_agent_key(&tool_use_id);
        self.forget_cleared_agent_markers(&[&key, &provisional], Some(&tool_use_id));
        if let Some(existing) = self
            .agent_runs
            .key_for_tool_use(&tool_use_id)
            .map(str::to_string)
            && existing != key
            && (existing == tool_use_id || existing == provisional)
        {
            self.agent_runs.rename(&existing, key.clone());
        }
        self.agent_runs
            .ensure_running_for_tool_use(key.clone(), label, Some(&tool_use_id));
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
        let parsed = output.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
        let surface = AgentControlSurface::from_wire(&action, &status, parsed.as_ref());
        let agent_id = event_agent_id.or_else(|| surface.agent_id().map(str::to_string));
        let provisional = provisional_agent_key(&tool_use_id);
        let fallback_key = if action == "agent" {
            tool_use_id.clone()
        } else {
            provisional.clone()
        };
        let key = agent_id.clone().unwrap_or(fallback_key);
        let late_after_turn_boundary = !self.agent_runs.contains_key(&key)
            && !self.agent_runs.contains_key(&provisional)
            && self.agent_runs.key_for_tool_use(&tool_use_id).is_none()
            && (self.cleared_agent_run_ids.contains(&key)
                || self.cleared_agent_run_ids.contains(&provisional)
                || self.cleared_agent_tool_use_ids.contains(&tool_use_id));
        if late_after_turn_boundary {
            return;
        }
        self.forget_cleared_agent_markers(&[&key, &provisional], Some(&tool_use_id));
        if key != provisional && self.agent_runs.contains_key(&provisional) {
            if self.cancelled_task_ids.remove(&provisional) {
                self.cancelled_task_ids.insert(key.clone());
            }
            if self.cancelling_task_ids.remove(&provisional) {
                self.cancelling_task_ids.insert(key.clone());
            }
            self.agent_runs.rename(&provisional, key.clone());
        }
        if !self.agent_runs.contains_key(&key) {
            self.agent_runs.ensure_running(key.clone(), label.clone());
        }

        if surface.is_terminal() {
            self.cancelling_task_ids.remove(&key);
        }
        match surface.cancelled_state_update() {
            CancelledStateUpdate::Set => {
                self.cancelled_task_ids.insert(key.clone());
            }
            CancelledStateUpdate::Clear => {
                self.cancelled_task_ids.remove(&key);
            }
            CancelledStateUpdate::Preserve => {}
        }

        let Some(cell) = self.agent_runs.get_mut(&key) else {
            return;
        };
        cell.description = agent_id
            .as_deref()
            .map(|id| agent_display_name(id, surface.display_name_hint()))
            .unwrap_or(label);

        match surface.outcome() {
            AgentControlOutcome::Completed => {
                complete_agent_cell(cell, duration_ms, parsed.as_ref(), output);
            }
            AgentControlOutcome::Failed(_) => {
                let failure_message = surface.failure_message();
                fail_agent_cell(
                    cell,
                    duration_ms,
                    parsed.as_ref(),
                    failure_message.as_deref().unwrap_or("agent failed"),
                );
            }
            AgentControlOutcome::Cancelled => {
                let fallback = surface
                    .cancelled_reason()
                    .unwrap_or("agent cancelled")
                    .to_string();
                cell.complete(
                    "cancelled",
                    duration_ms,
                    Some(fallback.clone()),
                    Some(fallback),
                );
            }
            AgentControlOutcome::Running => {
                cell.status = crate::tui::history_cell::task::TaskStatus::Running;
                cell.duration_ms = None;
                if let Some(preview) = surface.running_preview() {
                    if cell
                        .output_summary
                        .as_deref()
                        .is_some_and(|s| !s.is_empty())
                    {
                        append_agent_live_output(cell, &format!("\n{preview}\n"));
                    } else {
                        cell.output_summary = Some(preview);
                    }
                }
            }
            AgentControlOutcome::NoChange => {}
        }
    }

    fn on_agent_live_event(&mut self, event: astra_turn_core::agent_live_event::AgentLiveEvent) {
        use astra_turn_core::agent_live_event::AgentLiveEventKind;

        if !self.agent_runs.contains_key(&event.agent_id)
            && self.cleared_agent_run_ids.contains(&event.agent_id)
        {
            return;
        }
        self.forget_cleared_agent_markers(&[event.agent_id.as_str()], None);

        let is_terminal_event = matches!(event.kind, AgentLiveEventKind::AgentTerminated { .. });
        if !is_terminal_event
            && self.agent_runs.get(&event.agent_id).is_some_and(|cell| {
                !matches!(
                    cell.status,
                    crate::tui::history_cell::task::TaskStatus::Running
                )
            })
        {
            return;
        }

        if let AgentLiveEventKind::AgentTerminated { termination, .. } = &event.kind {
            self.cancelling_task_ids.remove(&event.agent_id);
            if matches!(
                termination,
                astra_turn_core::agent_live_event::AgentLiveTermination::Cancelled
            ) {
                self.cancelled_task_ids.insert(event.agent_id.clone());
            } else {
                self.cancelled_task_ids.remove(&event.agent_id);
            }
        }
        let label = agent_display_name(&event.agent_id, None);
        self.agent_runs
            .ensure_running(event.agent_id.clone(), label.clone());
        let mut parent_task_mirror = None;
        {
            let Some(cell) = self.agent_runs.get_mut(&event.agent_id) else {
                return;
            };
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
                    let summary = reason.clone();
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
                &event.agent_id,
                &tool_use_id,
                &name,
                &description,
            ),
            Some(AgentLiveMirror::Completed {
                tool_use_id,
                status,
                duration_ms,
            }) => self.mirror_live_child_completed_to_parent_tasks(
                &event.agent_id,
                &tool_use_id,
                &status,
                duration_ms,
            ),
            None => {}
        }
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
        // Child completion → update the child row inside its
        // parent Task. If the parent is already terminal/gone we
        // fall back to top-level to stay visible rather than drop.
        if let Some(parent_id) = parent_tool_use_id.as_deref()
            && self.route_child_completed(parent_id, &tool_use_id, &status, duration_ms)
        {
            return;
        }

        if name == "agent" {
            self.on_agent_control_completed(
                agent_action_from_description(&description)
                    .unwrap_or("agent")
                    .to_string(),
                agent_label_from_description(&description),
                status.clone(),
                duration_ms,
                output.as_deref(),
                tool_use_id.clone(),
                None,
            );
        }

        // Task parent completion → always prune the in-flight set
        // (committed-then-completed tasks still need cleanup). Then
        // try the multi-slot live_tasks register first (parallel
        // agent path), then the legacy single-active-cell path.
        if is_task_like_tool(&name) {
            self.in_flight_task_ids.retain(|s| s != &tool_use_id);
            self.cancelling_task_ids.remove(&tool_use_id);
            if name == "agent"
                && let Some(agent_cell) = self.agent_runs.get_mut(&tool_use_id)
            {
                agent_cell.complete(&status, duration_ms, output_summary.clone(), output.clone());
            }
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
        if let Some(cell) = self.active_cell.as_mut()
            && let Some(tc) = cell.as_any_mut().downcast_mut::<ToolCell>()
        {
            tc.complete(&status, duration_ms, description, output_summary, output);
            self.commit_active();
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
        self.commit_active();
        self.drain_all_live_tasks();
        self.clear_turn_scoped_agent_state();

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

    fn on_turn_error(&mut self, msg: String) {
        self.commit_active();
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

    /// Take the currently-live cell, finalise it, append to
    /// history, and persist. No-op when `active_cell` is None.
    fn commit_active(&mut self) {
        let Some(mut cell) = self.active_cell.take() else {
            return;
        };
        cell.finalize();
        // Box → Arc: the scrollback index shares cells with
        // long-lived render paths (e.g. Ctrl+O overlay) without
        // forcing everyone onto `&dyn`.
        self.history.push(box_into_arc(cell));
        self.persist_from_watermark();
    }

    /// Append an already-finalised cell. Used for UserCell /
    /// synthesised ToolCell / TurnSummary etc. — things built
    /// whole rather than streamed.
    fn commit_cell(&mut self, cell: Box<dyn HistoryCell>) {
        self.history.push(box_into_arc(cell));
        self.persist_from_watermark();
    }

    /// Persist every cell between `persist_watermark` and
    /// `history.len()`. Best-effort: errors are logged by the
    /// underlying `transcript_jsonl` helper and the watermark is
    /// advanced regardless, because the TUI must keep running and
    /// retrying a flaky write every turn would re-attempt the same
    /// failure.
    ///
    /// When `session_id` is empty (turn-1 edge case: server hasn't
    /// assigned an id yet) the watermark is NOT advanced, so
    /// subsequent `set_session_id` can flush the accumulated cells.
    fn persist_from_watermark(&mut self) {
        if self.session_id.is_empty() {
            return;
        }
        while self.persist_watermark < self.history.len() {
            let cell = &self.history[self.persist_watermark];
            if let Some(ev) = cell.to_persist() {
                transcript_jsonl::append(&self.session_id, &ev);
            }
            self.persist_watermark += 1;
        }
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

/// LEGACY parser for `agent` tool descriptions. The structured
/// [`WireEvent::AgentControlStarted`] / `Completed` events carry
/// `action` directly and are emitted by the live SSE path — those
/// SHOULD always be used in production. This parser only kicks in
/// when the only event available is the generic [`WireEvent::ToolStarted`]
/// (e.g. journal replay of older sessions, where the structured
/// events didn't exist yet). Tied to the format produced by
/// `astra_turn_core::tool_preview::agent_preview`.
fn agent_action_from_description(description: &str) -> Option<&'static str> {
    if description.starts_with("Spawn agent:") {
        Some("spawn")
    } else if description.starts_with("Get agent result:") {
        Some("get_result")
    } else {
        None
    }
}

/// Compatibility label extractor for journal replay / older task rows.
fn agent_label_from_description(description: &str) -> String {
    description
        .strip_prefix("Spawn agent:")
        .or_else(|| description.strip_prefix("Get agent result:"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(description)
        .to_string()
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

const MAX_AGENT_LIVE_OUTPUT_CHARS: usize = 100_000;

fn task_status_to_agent_row_status(
    status: crate::tui::history_cell::task::TaskStatus,
) -> crate::tui::bottom_pane::in_flight_agents_view::AgentRowStatus {
    use crate::tui::bottom_pane::in_flight_agents_view::AgentRowStatus;
    match status {
        crate::tui::history_cell::task::TaskStatus::Running => AgentRowStatus::Live,
        crate::tui::history_cell::task::TaskStatus::Completed => AgentRowStatus::Completed,
        crate::tui::history_cell::task::TaskStatus::Interrupted => AgentRowStatus::Interrupted,
        crate::tui::history_cell::task::TaskStatus::Failed => AgentRowStatus::Failed,
    }
}

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
    use crate::tui::bottom_pane::in_flight_agents_view::AgentRowStatus;
    use crate::tui::history_cell::tool::ToolStatus;

    fn fresh() -> ChatWidget {
        // Empty sid → persistence becomes a no-op (see
        // `transcript_jsonl::append` / `persist` guard). Lets
        // tests run without touching $HOME.
        ChatWidget::new("")
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
    fn commit_deferred_user_keeps_live_active_cell() {
        let mut w = fresh();
        w.active_cell = Some(Box::new(AssistantCell::new_streaming()));

        w.commit_deferred_user("stop after this tool");

        assert_eq!(w.history.len(), 1, "deferred input is committed as history");
        let persisted = w.history[0].to_persist().expect("user cell persists");
        assert!(matches!(
            &persisted,
            TurnEvent::User { text, .. } if text == "stop after this tool"
        ));
        assert_eq!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Assistant),
            "deferred input must not finalize the live assistant/tool cell"
        );
    }

    // ── AnswerDelta ──────────────────────────────────────────────

    #[test]
    fn answer_delta_creates_assistant_then_accumulates() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("Hello ".into())));
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("world".into())));
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
    fn answer_delta_finalises_live_reasoning_cell() {
        let mut w = fresh();
        // Begin a reasoning cell then jump straight to answer —
        // models that don't emit ReasoningDone rely on this
        // transition.
        w.handle_event(AppEvent::Wire(WireEvent::ReasoningDelta("thinking".into())));
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("answer".into())));
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

    // ── Reasoning lifecycle ──────────────────────────────────────

    #[test]
    fn reasoning_done_commits_reasoning_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ReasoningDelta("step 1".into())));
        w.handle_event(AppEvent::Wire(WireEvent::ReasoningDone));
        assert_eq!(w.history.len(), 1, "reasoning cell committed");
        assert!(w.active_cell.is_none(), "active cleared after done");
    }

    #[test]
    fn reasoning_done_without_reasoning_is_noop() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ReasoningDone));
        assert_eq!(w.history.len(), 0);
        assert!(w.active_cell.is_none());
    }

    // ── Tool lifecycle ───────────────────────────────────────────

    #[test]
    fn tool_started_then_completed_commits_cell() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(tool_started("bash", "ls /tmp")));
        assert!(matches!(
            w.active_cell.as_deref().map(cell_kind),
            Some(CellKind::Tool)
        ));
        w.handle_event(AppEvent::Wire(tool_completed(
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
        w.handle_event(AppEvent::Wire(tool_started("bash", "sleep 60")));
        let cell = w
            .active_cell
            .as_deref()
            .and_then(|cell| cell.as_any_ref().downcast_ref::<ToolCell>())
            .unwrap();
        assert!(!cell.ctrl_b_background_hint);

        let mut w = fresh();
        w.set_bash_background_hint_enabled(true);
        w.handle_event(AppEvent::Wire(tool_started("bash", "sleep 60")));
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
        w.handle_event(AppEvent::Wire(tool_started("bash", "sleep 60")));

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
        w.handle_event(AppEvent::Wire(tool_started("read_file", "src/main.rs")));
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
        spawn.handle_event(AppEvent::Wire(tool_started(
            "agent",
            "Spawn agent: reviewer",
        )));
        let spawn_cell = spawn
            .live_task_cell("tu_test")
            .expect("agent spawn should render as a live TaskCell");
        assert!(spawn_cell.ctrl_b_background_hint);

        let mut get_result = fresh();
        get_result.handle_event(AppEvent::Wire(tool_started(
            "agent",
            "Get agent result: reviewer@abc",
        )));
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
        w.handle_event(AppEvent::Wire(tool_completed(
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
        w.handle_event(AppEvent::Wire(task_started("tu_parent", "audit cache")));
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
        w.handle_event(AppEvent::Wire(task_started("tu_parent", "run things")));
        w.handle_event(AppEvent::Wire(child_started(
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
        w.handle_event(AppEvent::Wire(task_started("tu_parent", "run")));
        w.handle_event(AppEvent::Wire(child_started(
            "tu_parent",
            "tu_child",
            "bash",
            "ls",
        )));
        w.handle_event(AppEvent::Wire(child_completed(
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
        w.handle_event(AppEvent::Wire(child_started(
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
        w.handle_event(AppEvent::Wire(task_started("tu_parent", "run")));
        w.handle_event(AppEvent::Wire(child_started(
            "tu_parent",
            "tu_child",
            "bash",
            "ls",
        )));
        w.handle_event(AppEvent::Wire(child_completed(
            "tu_parent",
            "tu_child",
            "completed",
            10,
        )));
        w.handle_event(AppEvent::Wire(task_completed(
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
        w.handle_event(AppEvent::Wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::Wire(task_started("tu_2", "second")));
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
        w.handle_event(AppEvent::Wire(task_started(
            "spawn-a",
            "Spawn agent: reviewer-A",
        )));
        w.handle_event(AppEvent::Wire(task_started(
            "spawn-b",
            "Spawn agent: reviewer-B",
        )));
        w.handle_event(AppEvent::Wire(task_started(
            "spawn-c",
            "Spawn agent: reviewer-C",
        )));

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
        w.handle_event(AppEvent::Wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::Wire(task_started("tu_2", "second")));
        // Completing tu_1 leaves only tu_2 pending.
        w.handle_event(AppEvent::Wire(task_completed("tu_1", "completed", 50)));
        assert_eq!(w.in_flight_task_ids(), &["tu_2".to_string()]);
    }

    #[test]
    fn turn_complete_clears_in_flight_task_set() {
        // Defensive: even if a task completion event is lost,
        // turn_complete MUST reset the set so the next turn's
        // Ctrl+C doesn't target a stale id from the previous
        // conversation.
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));
        assert!(
            w.in_flight_task_ids().is_empty(),
            "turn boundary must reset cancel bookkeeping: {:?}",
            w.in_flight_task_ids()
        );
    }

    #[test]
    fn turn_complete_clears_agent_runs_and_cancelled_ids() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"cancelled","agent_id":"reviewer-A@abc"}"#.into()),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));
        assert_eq!(w.agent_run_ids(), vec!["reviewer-A@abc".to_string()]);
        assert!(
            w.cancelled_task_ids.contains("reviewer-A@abc"),
            "terminal cancelled agent should be tracked before the turn boundary"
        );

        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));

        assert!(
            w.agent_run_ids().is_empty(),
            "turn complete must clear prior-turn agent strip rows: {:?}",
            w.agent_run_ids()
        );
        assert!(
            w.cancelled_task_ids.is_empty(),
            "turn complete must clear prior-turn cancelled ids: {:?}",
            w.cancelled_task_ids
        );
    }

    #[test]
    fn completed_agent_result_clears_cancelled_tracking() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"cancelled","agent_id":"reviewer-A@abc"}"#.into()),
            tool_use_id: "spawn-tu-legacy".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));
        assert!(w.cancelled_task_ids.contains("reviewer-A@abc"));

        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "get_result".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 42,
            output: Some(r#"{"agent_id":"reviewer-A@abc","result":"done"}"#.into()),
            tool_use_id: "result-tu-legacy".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));

        assert!(
            !w.cancelled_task_ids.contains("reviewer-A@abc"),
            "non-cancelled terminal result must clear stale cancelled tracking"
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
        w.handle_event(AppEvent::Wire(WireEvent::ExplainReport(vec![
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
        w.handle_event(AppEvent::Wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::Wire(task_started("tu_1", "first")));
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
    fn mark_agent_controls_cancelling_drains_in_flight() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(task_started("tu_1", "first")));
        w.handle_event(AppEvent::Wire(task_started("tu_2", "second")));
        w.handle_event(AppEvent::Wire(task_started("tu_3", "third")));
        assert_eq!(w.in_flight_task_ids().len(), 3);

        let ids = w.in_flight_task_ids().to_vec();
        w.mark_agent_controls_cancelling(&ids);

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
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("answer".into())));
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::new(
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
        w.handle_event(AppEvent::Wire(WireEvent::TurnError(
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
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("first half ".into())));
        w.handle_event(AppEvent::Wire(tool_started("bash", "ls")));
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

    // ── Persist watermark (turn-1 edge case) ────────────────────

    /// Run a test body with `$HOME` pointed at a fresh tempdir so
    /// real `~/.astra/transcripts/` is left alone.
    fn with_tmp_home<F: FnOnce()>(f: F) {
        let _home = crate::tests::HomeGuard::temp();
        f();
    }

    #[test]
    #[serial_test::serial]
    fn set_session_id_flushes_cells_committed_under_empty_sid() {
        // Turn 1 edge case: cells commit before the server returns
        // a session id. `set_session_id` must retroactively flush
        // them to the new session's JSONL, so resume replay can
        // surface the user's very first message.
        with_tmp_home(|| {
            let mut w = ChatWidget::new(""); // empty sid — server pending
            w.handle_event(AppEvent::User(UserEvent::Submit("hi".into())));
            w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("hello back".into())));
            w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));

            // Before sid is set, nothing should be on disk yet.
            assert!(super::super::transcript_jsonl::load("late-sid").is_empty());

            // Server finally assigns an id → we flush retroactively.
            w.set_session_id("late-sid");
            let events = super::super::transcript_jsonl::load("late-sid");
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::User { text, .. } if text == "hi")),
                "turn-1 user message must be persisted after sid arrives"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::Assistant { .. })),
                "turn-1 assistant reply must be persisted after sid arrives"
            );
            assert!(
                events
                    .iter()
                    .any(|e| matches!(e, TurnEvent::TurnSummary { .. })),
                "turn-1 summary must be persisted after sid arrives"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn post_sid_cells_dont_double_persist_previous_cells() {
        // Sanity: after the initial flush, committing another turn
        // must append only that turn's cells — we must NOT re-write
        // the earlier turn's cells. This is what the persist
        // watermark guards against.
        with_tmp_home(|| {
            let mut w = ChatWidget::new("");
            w.handle_event(AppEvent::User(UserEvent::Submit("first".into())));
            w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));
            w.set_session_id("s");

            let count_after_first = super::super::transcript_jsonl::load("s").len();

            w.handle_event(AppEvent::User(UserEvent::Submit("second".into())));
            w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));
            let count_after_second = super::super::transcript_jsonl::load("s").len();

            assert!(
                count_after_second > count_after_first,
                "second turn must add cells: {count_after_first} → {count_after_second}"
            );
            // Each commit cycle (UserSubmit + TurnComplete) adds 2
            // cells: the user + the summary. Duplicate persistence
            // would give us 4 new rows instead of 2.
            assert_eq!(
                count_after_second - count_after_first,
                2,
                "second turn should append exactly 2 cells, not double-write earlier ones"
            );
        });
    }

    #[test]
    #[serial_test::serial]
    fn replay_does_not_re_persist_resumed_cells() {
        // When resuming a session, cells land in `history` via
        // `replay()` — they already exist on disk. A subsequent
        // `commit_*` must not re-persist the replayed cells.
        with_tmp_home(|| {
            let sid = "s_replay";
            // Seed a one-turn session on disk.
            super::super::transcript_jsonl::append(
                sid,
                &TurnEvent::User {
                    ts: None,
                    text: "seed".into(),
                },
            );
            let before = super::super::transcript_jsonl::load(sid).len();
            assert_eq!(before, 1);

            let mut w = ChatWidget::new(sid);
            w.replay(super::super::transcript_jsonl::load(sid));
            // Commit a new cell — only this cell should land on disk.
            w.handle_event(AppEvent::User(UserEvent::Submit("new".into())));
            let after = super::super::transcript_jsonl::load(sid).len();
            assert_eq!(
                after,
                before + 1,
                "only the new cell should persist; {before} → {after}"
            );
        });
    }

    // ── Last user text lookup (Ctrl+R edit-last) ────────────────

    #[test]
    fn last_user_text_walks_back_past_trailing_cells() {
        // History ends with non-User cells (assistant + summary);
        // lookup must still surface the most recent user message.
        let mut w = fresh();
        w.handle_event(AppEvent::User(UserEvent::Submit("first".into())));
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("reply 1".into())));
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));
        w.handle_event(AppEvent::User(UserEvent::Submit("second".into())));
        w.handle_event(AppEvent::Wire(WireEvent::AnswerDelta("reply 2".into())));
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));

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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "review module X".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        // Spawn agent B (parallel, before A completes)
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "A".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "B".into(),
            tool_use_id: "agent-B".into(),
            parent_tool_use_id: None,
        }));
        // Child belongs to A
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "A".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "B".into(),
            tool_use_id: "agent-B".into(),
            parent_tool_use_id: None,
        }));
        // Complete A
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
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
            w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
                name: "agent".into(),
                description: id.into(),
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }
        for id in ["agent-B", "agent-A", "agent-C"] {
            // out-of-order completions
            w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
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
            w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
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
    /// `on_turn_complete` of the NEXT turn force-fails them as
    /// belonging to the new turn.
    #[test]
    fn user_submit_drains_live_tasks_from_prior_turn() {
        let mut w = fresh();
        // Spawn 2 parallel agents in the implicit prior turn.
        for id in ["agent-A", "agent-B"] {
            w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
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

    /// CRITICAL regression: `agent_runs` (the registry that powers
    /// the multi-agent strip + Ctrl+G drilldown) must drop terminal
    /// rows from the previous turn when a new user turn begins.
    /// Otherwise the strip carries stale "✓"/"✗" rows forward — the
    /// reported bug had turn 1's six completed agents still showing
    /// alongside turn 2's six, totalling "12 parallel agents".
    /// Live rows belonging to the prior turn are NOT preserved
    /// either: `drain_all_live_tasks` already finalises them and
    /// they get committed to history (where Ctrl+G can still find
    /// them via `task_cell_anywhere` history fallback).
    #[test]
    fn user_submit_clears_terminal_agent_runs_from_prior_turn() {
        let mut w = fresh();
        // Turn 1: spawn two agents, both finish.
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"completed","agent_id":"reviewer-A@abc"}"#.into()),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc".into()),
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-B".into(),
            tool_use_id: "spawn-tu-2".into(),
            agent_id: Some("reviewer-B@def".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
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

        // Turn 2 starts. The registry must drop both terminal entries.
        w.handle_event(AppEvent::User(UserEvent::Submit("next".into())));
        assert_eq!(
            w.agent_run_ids().len(),
            0,
            "terminal agent_runs from prior turn must be cleared on user submit, got: {:?}",
            w.agent_run_ids()
        );
    }

    /// Live agents from a prior turn (abnormal turn end, e.g. server
    /// stream drop with no TurnComplete) must also be removed from
    /// `agent_runs` on user submit. They get finalised and committed
    /// to history by `drain_all_live_tasks`, so the user can still
    /// drill into them via Ctrl+G's history-fallback path.
    #[test]
    fn user_submit_clears_live_agent_runs_from_prior_turn() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
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
        assert_eq!(
            w.agent_run_ids().len(),
            0,
            "live agent_runs from a dropped prior turn must also be cleared, got: {:?}",
            w.agent_run_ids()
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "initial".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        // Same id, different description (e.g. server retry with
        // a more detailed task description).
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "agent-A".into(),
            tool_use_id: "agent-A".into(),
            parent_tool_use_id: None,
        }));
        let history_len_before_turn_complete = w.history().len();
        // Turn ends — agent-A finalized as Failed and committed.
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::new(
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
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
    fn late_agent_control_completed_after_turn_complete_is_noop() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-late".into(),
            agent_id: Some("reviewer-A@late".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));
        assert!(
            w.agent_run_ids().is_empty(),
            "turn boundary should clear the prior-turn agent row first"
        );

        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            status: "completed".into(),
            duration_ms: 10,
            output: Some(r#"{"status":"cancelled","agent_id":"reviewer-A@late"}"#.into()),
            tool_use_id: "spawn-tu-late".into(),
            agent_id: Some("reviewer-A@late".into()),
        }));

        assert!(
            w.agent_run_ids().is_empty(),
            "late AgentControlCompleted must not resurrect a cleared prior-turn row: {:?}",
            w.agent_run_ids()
        );
        assert!(
            w.cancelled_task_ids.is_empty(),
            "late AgentControlCompleted must not restore cancelled state from the prior turn: {:?}",
            w.cancelled_task_ids
        );
    }

    #[test]
    fn late_agent_terminated_after_turn_complete_is_noop() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@late-term".into(),
            kind: AgentLiveEventKind::OutputDelta("running".into()),
        })));
        w.handle_event(AppEvent::Wire(WireEvent::TurnComplete(Box::default())));
        assert!(
            w.agent_run_ids().is_empty(),
            "turn boundary should clear the prior-turn live row first"
        );

        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@late-term".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Cancelled,
                duration_ms: 20,
                reason: Some("late termination".into()),
            },
        })));

        assert!(
            w.agent_run_ids().is_empty(),
            "late AgentTerminated must not resurrect a cleared prior-turn row: {:?}",
            w.agent_run_ids()
        );
        assert!(
            w.cancelled_task_ids.is_empty(),
            "late AgentTerminated must not restore cancelled state from the prior turn: {:?}",
            w.cancelled_task_ids
        );
    }

    /// REGRESSION (session 2a98814b): pressing Ctrl+G after sub-agents
    /// have completed used to silently no-op. The user lost any way to
    /// drill into the agents' output. Now `agents_drilldown_rows`
    /// returns the most-recent completed Task cells too.
    #[test]
    fn agents_drilldown_includes_recent_completed_after_strip_dismissed() {
        let mut w = fresh();
        // Spawn 3 parallel agents, complete all of them.
        for id in ["agent-A", "agent-B", "agent-C"] {
            w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
                name: "agent".into(),
                description: format!("review {id}"),
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }
        for id in ["agent-A", "agent-B", "agent-C"] {
            w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
                name: "agent".into(),
                description: format!("review {id}"),
                status: "completed".into(),
                duration_ms: 12_000,
                output_summary: Some(format!("done-{id}")),
                output: None,
                tool_use_id: id.into(),
                parent_tool_use_id: None,
            }));
        }

        // Live strip is empty (all completed) — the pre-fix Ctrl+G path.
        assert_eq!(
            w.live_task_ids().len(),
            0,
            "all agents completed, live strip should be empty"
        );

        // New behavior: drilldown rows include the recent completions.
        let rows = w.agents_drilldown_rows(5);
        assert_eq!(
            rows.len(),
            3,
            "Ctrl+G must surface the 3 completed agents so the user \
             can still drill into their output"
        );
        // All 3 should be flagged as not-live so the view renders the
        // ✓ icon instead of the spinner.
        assert!(
            rows.iter().all(|r| r.status == AgentRowStatus::Completed),
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "live-only check".into(),
            tool_use_id: "agent-X".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "live-only check".into(),
            status: "completed".into(),
            duration_ms: 1_000,
            output_summary: None,
            output: None,
            tool_use_id: "agent-X".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(0);
        assert!(
            rows.is_empty(),
            "max_recent_completed=0 must NOT surface completed rows"
        );
    }

    /// `task_cell_anywhere` finds completed agents in history so
    /// Ctrl+G drill-in still works after the live strip is gone.
    #[test]
    fn task_cell_anywhere_finds_completed_in_history() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "find me".into(),
            tool_use_id: "completed-id".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "old completed".into(),
            tool_use_id: "old".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "old completed".into(),
            status: "completed".into(),
            duration_ms: 500,
            output_summary: None,
            output: None,
            tool_use_id: "old".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "new live".into(),
            tool_use_id: "new".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent_id, "new", "live must come first");
        assert_eq!(rows[0].status, AgentRowStatus::Live);
        assert_eq!(rows[1].agent_id, "old", "completed must come second");
        assert_eq!(rows[1].status, AgentRowStatus::Completed);
    }

    #[test]
    fn agent_spawn_control_tool_creates_logical_agent_row() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: arch-reviewer (code-review)".into(),
            tool_use_id: "spawn-arch".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Spawn agent: arch-reviewer (code-review)".into(),
            status: "completed".into(),
            duration_ms: 0,
            output_summary: Some("json object".into()),
            output: Some(
                r#"{"status":"launched","agent_id":"arch-reviewer@abc12345","description":"Architecture review"}"#
                    .into(),
            ),
            tool_use_id: "spawn-arch".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(
            rows.len(),
            1,
            "spawn control cell must not appear as its own agent row"
        );
        assert_eq!(rows[0].agent_id, "arch-reviewer@abc12345");
        assert_eq!(rows[0].name, "arch-reviewer");
        assert_eq!(rows[0].status, AgentRowStatus::Live);

        let detail = w
            .task_cell_anywhere("arch-reviewer@abc12345")
            .expect("logical agent row should be drillable");
        assert_eq!(detail.description, "arch-reviewer");
    }

    #[test]
    fn get_result_updates_logical_agent_detail() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Spawn agent: ux-reviewer (code-review)".into(),
            status: "completed".into(),
            duration_ms: 0,
            output_summary: None,
            output: Some(
                r#"{"status":"launched","agent_id":"ux-reviewer@def67890","description":"UX review"}"#
                    .into(),
            ),
            tool_use_id: "spawn-ux".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: ux-reviewer@def67890".into(),
            status: "completed".into(),
            duration_ms: 123,
            output_summary: None,
            output: Some(
                r#"{"status":"completed","agent_id":"ux-reviewer@def67890","finish_reason":"normal","result":"finding one\nfinding two"}"#
                    .into(),
            ),
            tool_use_id: "result-ux".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AgentRowStatus::Completed);
        let detail = w.task_cell_anywhere("ux-reviewer@def67890").unwrap();
        assert_eq!(
            detail.output_summary.as_deref(),
            Some("finding one\nfinding two")
        );
    }

    #[test]
    fn get_result_without_status_still_completes_agent() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: reviewer@abc12345".into(),
            status: "completed".into(),
            duration_ms: 77,
            output_summary: None,
            output: Some(
                r#"{"agent_id":"reviewer@abc12345","finish_reason":"normal","result":"done"}"#
                    .into(),
            ),
            tool_use_id: "result-reviewer".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "reviewer@abc12345");
        assert!(
            rows[0].status == AgentRowStatus::Completed,
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: reviewer@abc12345".into(),
            status: "completed".into(),
            duration_ms: 120_000,
            output_summary: None,
            output: Some(
                r#"{"status":"still_running","agent_id":"reviewer@abc12345","current_status":"running","waited_secs":120,"hint":"call again"}"#
                    .into(),
            ),
            tool_use_id: "result-reviewer".into(),
            parent_tool_use_id: None,
        }));

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
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("live token".into()),
        })));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: reviewer@abc12345".into(),
            status: "completed".into(),
            duration_ms: 120_000,
            output_summary: None,
            output: Some(
                r#"{"status":"still_running","agent_id":"reviewer@abc12345","current_status":"running","waited_secs":120,"hint":"call again"}"#
                    .into(),
            ),
            tool_use_id: "result-reviewer".into(),
            parent_tool_use_id: None,
        }));

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        let output = detail.output_summary.as_deref().unwrap_or("");
        assert!(output.contains("live token"));
        assert!(output.contains("Agent is running after 120s. call again"));
    }

    #[test]
    fn interrupted_get_result_marks_agent_failed_instead_of_completed() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: reviewer@abc12345".into(),
            status: "completed".into(),
            duration_ms: 77,
            output_summary: None,
            output: Some(
                r#"{"status":"interrupted","agent_id":"reviewer@abc12345","finish_reason":"budget_exhausted"}"#
                    .into(),
            ),
            tool_use_id: "result-reviewer".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "reviewer@abc12345");
        assert_eq!(rows[0].status, AgentRowStatus::Failed);

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert!(matches!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Failed
        ));
        assert_eq!(
            detail.error.as_deref(),
            Some(crate::tui::agent_control_status::AGENT_RESULT_INTERRUPTED_ERROR)
        );
    }

    #[test]
    fn interrupted_get_result_preserves_partial_result_text() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Get agent result: reviewer@abc12345".into(),
            status: "completed".into(),
            duration_ms: 77,
            output_summary: None,
            output: Some(
                r#"{"status":"interrupted","agent_id":"reviewer@abc12345","result":"partial draft","finish_reason":"budget_exhausted"}"#
                    .into(),
            ),
            tool_use_id: "result-reviewer".into(),
            parent_tool_use_id: None,
        }));

        let detail = w.task_cell_anywhere("reviewer@abc12345").unwrap();
        assert!(matches!(
            detail.status,
            crate::tui::history_cell::task::TaskStatus::Failed
        ));
        assert_eq!(detail.output_summary.as_deref(), Some("partial draft"));
        assert_eq!(
            detail.error.as_deref(),
            Some(crate::tui::agent_control_status::AGENT_RESULT_INTERRUPTED_ERROR)
        );
    }

    #[test]
    fn agent_live_events_append_output_and_child_tools() {
        use astra_turn_core::agent_live_event::{AgentLiveEvent, AgentLiveEventKind};

        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("hello ".into()),
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::OutputDelta("world".into()),
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "Run checks".into(),
                tool_use_id: "child-tool".into(),
            },
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
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
    fn failed_spawn_without_agent_id_is_visible_without_control_plane_fallback() {
        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: broken-reviewer (code-review)".into(),
            tool_use_id: "spawn-broken".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::ToolCompleted {
            name: "agent".into(),
            description: "Spawn agent: broken-reviewer (code-review)".into(),
            status: "failed".into(),
            duration_ms: 10,
            output_summary: None,
            output: Some(r#"{"error":"spawn failed"}"#.into()),
            tool_use_id: "spawn-broken".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].agent_id, "pending:spawn-broken");
        assert_eq!(rows[0].status, AgentRowStatus::Failed);
        assert!(
            !rows[0].name.starts_with("Spawn agent:"),
            "failed control calls should stay in the logical agent registry, not fall back to control-plane labels"
        );
    }

    /// REGRESSION (reviewer L2-2): the structured
    /// `WireEvent::AgentControlStarted` path MUST take priority over
    /// the description-string-prefix legacy parser. When both events
    /// fire (live SSE), the structured event carries the canonical
    /// `agent_id` and `action`; the legacy parser is a fallback for
    /// journal-replay where only `ToolStarted` survives.
    ///
    /// Pin: emitting AgentControlStarted with a known agent_id pulls
    /// the row in by that id, and a follow-up `ToolStarted` for the
    /// same control call doesn't create a duplicate row.
    #[test]
    fn structured_agent_control_event_takes_priority_over_legacy_parsing() {
        let mut w = fresh();
        // Structured event arrives first with the real agent_id.
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        // Then the generic ToolStarted arrives (stream_render emits both).
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: reviewer-A (code-review)".into(),
            tool_use_id: "spawn-tu-1".into(),
            parent_tool_use_id: None,
        }));

        let rows = w.agents_drilldown_rows(5);
        // The agent_id from the structured event should win — NOT
        // the provisional id derived from tool_use_id by the legacy parser.
        let ids: Vec<&str> = rows.iter().map(|r| r.agent_id.as_str()).collect();
        assert_eq!(ids.len(), 1, "spawn lifecycle must produce one logical row");
        assert!(
            ids.contains(&"reviewer-A@abc12345"),
            "structured agent_id must surface as the row id; got {ids:?}"
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: reviewer-A (code-review)".into(),
            tool_use_id: "spawn-tu-1".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
            fanout_slot: None,
            fanout_title: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer-A@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "cargo test".into(),
                tool_use_id: "child-tu-1".into(),
            },
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
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
        w.handle_event(AppEvent::Wire(WireEvent::ToolStarted {
            name: "agent".into(),
            description: "Spawn agent: reviewer-A (code-review)".into(),
            tool_use_id: "spawn-tu-1".into(),
            parent_tool_use_id: None,
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer-A@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "cargo test".into(),
                tool_use_id: "child-tu-1".into(),
            },
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
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

        let rows = w.agents_drilldown_rows(5);
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
    fn first_tool_use_binding_wins_until_explicit_rename() {
        let mut registry = AgentRunRegistry::default();
        registry.ensure_running_for_tool_use(
            "pending:spawn-tu-1".into(),
            "reviewer-A".into(),
            Some("spawn-tu-1"),
        );
        registry.ensure_running_for_tool_use(
            "late-other-key".into(),
            "reviewer-B".into(),
            Some("spawn-tu-1"),
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "reviewer-A".into(),
            tool_use_id: "spawn-tu-1".into(),
            agent_id: Some("reviewer-A@abc12345".into()),
            fanout_slot: None,
            fanout_title: None,
        }));

        w.mark_agent_controls_cancelling(&["spawn-tu-1".to_string()]);

        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, AgentRowStatus::Cancelling);
        assert!(
            w.task_cell_anywhere("reviewer-A@abc12345")
                .and_then(|tc| tc.output_summary.as_deref())
                .is_some_and(|output| output.contains("Cancelling…")),
            "detail output should show an immediate cancelling marker"
        );
        w.agent_runs
            .get_mut("reviewer-A@abc12345")
            .expect("logical row")
            .output_summary = Some("x".repeat(100_000));
        let rows = w.agents_drilldown_rows(5);
        assert_eq!(
            rows[0].status,
            AgentRowStatus::Cancelling,
            "cancelling status must be structural, not parsed from truncated output text"
        );
    }

    #[test]
    fn agent_control_started_projects_fanout_membership_to_drilldown_rows_after_rename() {
        use astra_turn_core::orchestration_fanout_group::AgentFanoutSlotIdentity;

        let mut w = fresh();
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "auth reviewer".into(),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: None,
            fanout_slot: Some(AgentFanoutSlotIdentity::new("review-1", 3, 0, None).unwrap()),
            fanout_title: Some("review fanout".into()),
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "auth reviewer".into(),
            status: "completed".into(),
            duration_ms: 25,
            output: Some(r#"{"status":"completed","agent_id":"auth@abc12345"}"#.into()),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: Some("auth@abc12345".into()),
        }));

        let rows = w.agents_drilldown_rows(5);
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlStarted {
            action: "spawn".into(),
            label: "storage reviewer".into(),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: None,
            fanout_slot: Some(
                AgentFanoutSlotIdentity::new("review-1", 3, 1, Some("storage".into())).unwrap(),
            ),
            fanout_title: Some("review fanout".into()),
        }));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "storage@abc12345".into(),
            kind: AgentLiveEventKind::ToolStarted {
                name: "bash".into(),
                description: "cargo test".into(),
                tool_use_id: "child-tu-1".into(),
            },
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentControlCompleted {
            action: "spawn".into(),
            label: "storage reviewer".into(),
            status: "completed".into(),
            duration_ms: 25,
            output: Some(r#"{"status":"completed","agent_id":"storage@abc12345"}"#.into()),
            tool_use_id: "spawn-tu-fanout".into(),
            agent_id: Some("storage@abc12345".into()),
        }));

        let rows = w.agents_drilldown_rows(5);
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
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

        let rows = w.agents_drilldown_rows(5);
        let target = rows
            .iter()
            .find(|r| r.agent_id == "reviewer@def01234")
            .unwrap();
        assert_eq!(
            target.status,
            AgentRowStatus::Failed,
            "drilldown row must report failed status"
        );
    }

    #[test]
    fn agent_terminated_interrupted_preserves_resumable_status() {
        use astra_turn_core::agent_live_event::{
            AgentLiveEvent, AgentLiveEventKind, AgentLiveTermination,
        };

        let mut widget = fresh();
        widget.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@paused".into(),
            kind: AgentLiveEventKind::OutputDelta("partial findings".into()),
        })));
        widget.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
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
        assert!(!widget.agent_is_cancelled("reviewer@paused"));
        assert_eq!(
            widget.agents_drilldown_rows(5)[0].status,
            AgentRowStatus::Interrupted
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@cancel01".into(),
            kind: AgentLiveEventKind::OutputDelta("running".into()),
        })));
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
            agent_id: "reviewer@cancel01".into(),
            kind: AgentLiveEventKind::AgentTerminated {
                termination: AgentLiveTermination::Cancelled,
                duration_ms: 200,
                reason: Some("user cancellation".into()),
            },
        })));

        let row = w.agent_run_cell("reviewer@cancel01").unwrap();
        assert!(
            !matches!(
                row.status,
                crate::tui::history_cell::task::TaskStatus::Running
            ),
            "Cancelled termination must NOT leave the row in Running; got {:?}",
            row.status
        );
        let rows = w.agents_drilldown_rows(5);
        assert_eq!(rows[0].status, AgentRowStatus::Cancelled);
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentLive(AgentLiveEvent {
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
        w.handle_event(AppEvent::Wire(WireEvent::AgentLiveBatch(vec![
            AgentLiveEvent {
                agent_id: "reviewer@done7777".into(),
                kind: AgentLiveEventKind::AgentTerminated {
                    termination: AgentLiveTermination::Completed,
                    duration_ms: 1_000,
                    reason: Some("normal".into()),
                },
            },
            AgentLiveEvent {
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
}
