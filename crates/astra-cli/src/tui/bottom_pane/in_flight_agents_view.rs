//! Interactive agent-run navigator.
//!
//! When multiple sub-agents are running in parallel (the model spawned
//! N agent spawn actions in one turn), the user presses `Ctrl+G` to
//! open this view: a vertical list of every run with its description, lineage,
//! typed activity counts, and elapsed time. ↑↓ navigates, Enter opens the
//! selected run's complete conversation in the shared `TranscriptView`,
//! Esc/← closes.
//!
//! Rows are a snapshot supplied by `ChatWidget`, and the outer event
//! loop refreshes the snapshot while the monitor is open whenever an
//! agent lifecycle event can affect row state. The view itself stays
//! ownership-only: it never holds a reference back into `ChatWidget`.
//!
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::view::{
    BottomPaneView, BottomPaneViewAction, CancellationEvent, ViewActionDisposition,
    ViewActionRequest,
};
use crate::tui::agent_run_projection::{
    AgentActivityCounts, AgentControlTarget, AgentProjectionConfidence, AgentProjectionSource,
    AgentRunState, AgentRunStatus, AgentTranscriptTarget,
};
use crate::tui::server_agent_observer::ServerAgentTruthState;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentFanoutMembership {
    pub group_id: String,
    pub group_title: String,
    pub target_count: usize,
    pub slot_index: usize,
    pub slot_label: String,
}

#[derive(Clone)]
pub(crate) struct AgentRow {
    pub agent_id: String,
    pub name: String,
    pub spawn_tool_call_id: Option<String>,
    pub activity: AgentActivityCounts,
    pub run_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub depth: u32,
    pub provenance: AgentProjectionSource,
    pub elapsed_ms: u64,
    pub state: AgentRunState,
    /// Latest typed reason for a waiting/paused run. Absent means the
    /// lifecycle is known but no reason was supplied; the UI must not invent
    /// one from arbitrary output text.
    pub attention_summary: Option<String>,
    pub fanout: Option<AgentFanoutMembership>,
    pub control_target: Option<AgentControlTarget>,
    pub transcript_target: Option<AgentTranscriptTarget>,
    pub available_actions: Vec<astra_thin_client::SessionRunAction>,
    pub runtime: astra_thin_client::SessionRunRuntimeFacts,
}

#[derive(Clone, Default)]
pub(crate) struct AgentMonitorSnapshot {
    pub rows: Vec<AgentRow>,
    /// The full workbench navigator includes the main conversation as the
    /// root of the run tree. Compact activity strips intentionally omit it.
    pub show_root_conversation: bool,
    /// Health of the durable-server observation lane. This is independent of
    /// local rows: an empty failed read must not masquerade as "no agents".
    pub server_truth_state: crate::tui::server_agent_observer::ServerAgentTruthState,
    /// The durable server returned its configured node limit. Included rows
    /// remain authoritative, but absence and server-derived child counts are
    /// not complete facts.
    pub durable_snapshot_truncated: bool,
}

impl AgentMonitorSnapshot {
    pub(crate) fn complete(rows: Vec<AgentRow>) -> Self {
        Self {
            rows,
            show_root_conversation: false,
            server_truth_state: crate::tui::server_agent_observer::ServerAgentTruthState::Unbound,
            durable_snapshot_truncated: false,
        }
    }

    pub(crate) fn should_open(&self) -> bool {
        self.show_root_conversation
            || !self.rows.is_empty()
            || matches!(
                self.server_truth_state,
                crate::tui::server_agent_observer::ServerAgentTruthState::Loading
                    | crate::tui::server_agent_observer::ServerAgentTruthState::Stale
                    | crate::tui::server_agent_observer::ServerAgentTruthState::Unavailable
            )
    }
}

impl From<Vec<AgentRow>> for AgentMonitorSnapshot {
    fn from(rows: Vec<AgentRow>) -> Self {
        Self::complete(rows)
    }
}

impl std::ops::Deref for AgentMonitorSnapshot {
    type Target = [AgentRow];

    fn deref(&self) -> &Self::Target {
        &self.rows
    }
}

impl IntoIterator for AgentMonitorSnapshot {
    type Item = AgentRow;
    type IntoIter = std::vec::IntoIter<AgentRow>;

    fn into_iter(self) -> Self::IntoIter {
        self.rows.into_iter()
    }
}

impl AgentRow {
    /// Attention navigation follows the typed run lifecycle. Its
    /// human-readable summary is rendered only after this classification;
    /// control flow never infers an attention state from presentation text.
    fn has_attention(&self) -> bool {
        self.state.status == AgentRunStatus::Waiting
    }

    fn target_for_action(
        &self,
        action: astra_thin_client::SessionRunAction,
    ) -> Option<&AgentControlTarget> {
        if !self.state.is_actionable_active()
            || matches!(
                self.state.status,
                AgentRunStatus::Pausing | AgentRunStatus::Resuming | AgentRunStatus::Cancelling
            )
            || !self.available_actions.contains(&action)
        {
            return None;
        }
        self.control_target.as_ref()
    }

    fn guide_target(&self) -> Option<(&AgentControlTarget, &str)> {
        if !self.state.is_actionable_active()
            || !matches!(
                self.state.status,
                AgentRunStatus::Running | AgentRunStatus::Waiting
            )
        {
            return None;
        }
        let target = self.control_target.as_ref()?;
        let run_id = match target {
            AgentControlTarget::DurableRun { run_id } => run_id.as_str(),
            AgentControlTarget::LocalAgent { .. } => self.run_id.as_deref()?,
            AgentControlTarget::LocalDelegatedRun { .. } => return None,
        };
        Some((target, run_id))
    }
}

pub(crate) struct InFlightAgentsView {
    rows: Vec<AgentRow>,
    show_root_conversation: bool,
    server_truth_state: crate::tui::server_agent_observer::ServerAgentTruthState,
    durable_snapshot_truncated: bool,
    live_count: usize,
    failed_count: usize,
    uncertain_count: usize,
    /// `ROOT_SELECTION` means the main conversation. Every other value is a
    /// stable index into `rows` for the duration of a snapshot.
    selected: usize,
    completed: bool,
    pending_action: Option<ViewActionRequest>,
}

impl InFlightAgentsView {
    const ROOT_SELECTION: usize = usize::MAX;

    pub fn new(snapshot: impl Into<AgentMonitorSnapshot>) -> Self {
        let snapshot = snapshot.into();
        let AgentMonitorSnapshot {
            rows,
            show_root_conversation,
            server_truth_state,
            durable_snapshot_truncated,
        } = snapshot;
        let (live_count, failed_count, uncertain_count) = count_rows(&rows);
        Self {
            rows,
            show_root_conversation,
            server_truth_state,
            durable_snapshot_truncated,
            live_count,
            failed_count,
            uncertain_count,
            selected: if show_root_conversation {
                Self::ROOT_SELECTION
            } else {
                0
            },
            completed: false,
            pending_action: None,
        }
    }

    fn replace_snapshot(&mut self, snapshot: AgentMonitorSnapshot) {
        let AgentMonitorSnapshot {
            rows,
            show_root_conversation,
            server_truth_state,
            durable_snapshot_truncated,
        } = snapshot;
        let preserve_root_selection = self.selected == Self::ROOT_SELECTION;
        let selected_id = self.rows.get(self.selected).map(|row| row.agent_id.clone());
        let (live_count, failed_count, uncertain_count) = count_rows(&rows);
        self.selected = if preserve_root_selection && show_root_conversation {
            Self::ROOT_SELECTION
        } else {
            selected_id
                .and_then(|id| rows.iter().position(|row| row.agent_id == id))
                .unwrap_or(if show_root_conversation {
                    Self::ROOT_SELECTION
                } else {
                    0
                })
                .min(rows.len().saturating_sub(1))
        };
        self.rows = rows;
        self.show_root_conversation = show_root_conversation;
        self.server_truth_state = server_truth_state;
        self.durable_snapshot_truncated = durable_snapshot_truncated;
        self.live_count = live_count;
        self.failed_count = failed_count;
        self.uncertain_count = uncertain_count;
    }

    fn move_up(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if self.selected == Self::ROOT_SELECTION {
            self.rows.len() - 1
        } else if self.selected == 0 && self.show_root_conversation {
            Self::ROOT_SELECTION
        } else if self.selected == 0 {
            self.rows.len() - 1
        } else {
            self.selected - 1
        };
    }

    fn move_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.selected = if self.selected == Self::ROOT_SELECTION {
            0
        } else if self.selected + 1 >= self.rows.len() && self.show_root_conversation {
            Self::ROOT_SELECTION
        } else {
            (self.selected + 1) % self.rows.len()
        };
    }

    fn move_page_up(&mut self) {
        if self.selected == Self::ROOT_SELECTION {
            return;
        }
        self.selected = self.selected.saturating_sub(PAGE_STEP);
    }

    fn move_page_down(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        if self.selected == Self::ROOT_SELECTION {
            self.selected = PAGE_STEP.min(self.rows.len().saturating_sub(1));
            return;
        }
        self.selected = self
            .selected
            .saturating_add(PAGE_STEP)
            .min(self.rows.len().saturating_sub(1));
    }

    fn select_number(&mut self, n: u8) {
        let idx = usize::from(n.saturating_sub(1));
        if idx < self.rows.len() {
            self.selected = idx;
        }
    }

    fn accept(&mut self) {
        if self.selected == Self::ROOT_SELECTION {
            self.pending_action = Some(ViewActionRequest {
                action: BottomPaneViewAction::OpenRootTranscript,
                // Keep the navigator below the conversation. Left/Esc
                // returns to the exact tree selection rather than treating
                // this transcript as an expanded task detail.
                disposition: ViewActionDisposition::KeepOpen,
            });
            return;
        }
        if let Some(row) = self.rows.get(self.selected) {
            self.pending_action = Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent {
                    agent_id: row.agent_id.clone(),
                    agent_name: row.name.clone(),
                    run_id: transcript_run_id(row).map(ToString::to_string),
                    transcript_target: row.transcript_target,
                },
                disposition: ViewActionDisposition::KeepOpen,
            });
        }
    }

    /// User pressed `x` (or Delete) on the selected row.
    ///
    /// Queues a typed cancel action while keeping the view open. That way
    /// the user sees
    /// the row transition Live → Cancelling → Cancelled in real time
    /// and can stop additional rows without re-opening Ctrl+G.
    ///
    /// Only fires when the row is actually stoppable (Live or already
    /// Cancelling). Terminal rows (Completed / Failed / Cancelled) do
    /// nothing — there's nothing to stop, and an inert keypress should
    /// not silently dismiss the view.
    fn request_control(&mut self, action: astra_thin_client::SessionRunAction) {
        if let Some(row) = self.rows.get(self.selected)
            && let Some(target) = row.target_for_action(action)
        {
            self.pending_action = Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent {
                    agent_id: row.agent_id.clone(),
                    target: target.clone(),
                    action,
                },
                disposition: ViewActionDisposition::KeepOpen,
            });
        }
    }

    fn request_pause_or_resume(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let action = if row
            .available_actions
            .contains(&astra_thin_client::SessionRunAction::Pause)
        {
            astra_thin_client::SessionRunAction::Pause
        } else if row
            .available_actions
            .contains(&astra_thin_client::SessionRunAction::Resume)
        {
            astra_thin_client::SessionRunAction::Resume
        } else if row
            .available_actions
            .contains(&astra_thin_client::SessionRunAction::ContinueSession)
        {
            astra_thin_client::SessionRunAction::ContinueSession
        } else {
            return;
        };
        self.request_control(action);
    }

    fn request_guide(&mut self) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let Some((target, run_id)) = row.guide_target() else {
            return;
        };
        self.pending_action = Some(ViewActionRequest {
            action: BottomPaneViewAction::BeginAgentGuide {
                agent_id: row.agent_id.clone(),
                agent_name: row.name.clone(),
                run_id: run_id.to_string(),
                target: target.clone(),
            },
            disposition: ViewActionDisposition::KeepOpen,
        });
    }

    /// Move focus among agents that currently report a typed attention fact.
    /// This is navigation only: it never fabricates an approval/input action
    /// from a status label, and it leaves the user's normal run-tree view
    /// intact when no such agent exists.
    fn focus_next_attention(&mut self, reverse: bool) {
        let attention_rows: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter_map(|(index, row)| row.has_attention().then_some(index))
            .collect();
        let Some(first) = attention_rows.first().copied() else {
            return;
        };
        let current = attention_rows
            .iter()
            .position(|index| *index == self.selected);
        self.selected = match (current, reverse) {
            (Some(index), false) => attention_rows[(index + 1) % attention_rows.len()],
            (Some(0), true) => *attention_rows.last().expect("non-empty attention rows"),
            (Some(index), true) => attention_rows[index - 1],
            (None, _) => first,
        };
    }
}

fn count_rows(rows: &[AgentRow]) -> (usize, usize, usize) {
    let live_count = rows
        .iter()
        .filter(|row| row.state.is_actionable_active())
        .count();
    let failed_count = rows
        .iter()
        .filter(|row| {
            row.state.status.is_failure()
                && !matches!(
                    row.state.confidence,
                    AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
                )
        })
        .count();
    let uncertain_count = rows
        .iter()
        .filter(|row| {
            matches!(
                row.state.confidence,
                AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
            )
        })
        .count();
    (live_count, failed_count, uncertain_count)
}

#[derive(Clone)]
struct FanoutHeader {
    title: String,
    target_count: usize,
    running: usize,
    waiting: usize,
    paused: usize,
    done: usize,
    failed: usize,
    stopped: usize,
    uncertain: usize,
}

enum AgentListEntry<'a> {
    RootConversation,
    FanoutHeader(FanoutHeader),
    Row {
        row_idx: usize,
        row: &'a AgentRow,
        grouped: bool,
    },
}

impl AgentListEntry<'_> {
    fn row_index(&self) -> Option<usize> {
        match self {
            AgentListEntry::RootConversation | AgentListEntry::FanoutHeader(_) => None,
            AgentListEntry::Row { row_idx, .. } => Some(*row_idx),
        }
    }
}

fn agent_list_entries(rows: &[AgentRow], show_root_conversation: bool) -> Vec<AgentListEntry<'_>> {
    let mut entries = Vec::with_capacity(rows.len() + usize::from(show_root_conversation));
    if show_root_conversation {
        entries.push(AgentListEntry::RootConversation);
    }
    let mut rendered = vec![false; rows.len()];

    for idx in 0..rows.len() {
        if rendered[idx] {
            continue;
        }

        let Some(fanout) = rows[idx].fanout.as_ref() else {
            rendered[idx] = true;
            entries.push(AgentListEntry::Row {
                row_idx: idx,
                row: &rows[idx],
                grouped: false,
            });
            continue;
        };

        let member_indices = rows
            .iter()
            .enumerate()
            .filter_map(|(member_idx, row)| {
                row.fanout
                    .as_ref()
                    .is_some_and(|member| member.group_id == fanout.group_id)
                    .then_some(member_idx)
            })
            .collect::<Vec<_>>();
        entries.push(AgentListEntry::FanoutHeader(fanout_header(
            fanout,
            &member_indices,
            rows,
        )));
        for member_idx in member_indices {
            rendered[member_idx] = true;
            entries.push(AgentListEntry::Row {
                row_idx: member_idx,
                row: &rows[member_idx],
                grouped: true,
            });
        }
    }

    entries
}

fn fanout_header(
    fanout: &AgentFanoutMembership,
    member_indices: &[usize],
    rows: &[AgentRow],
) -> FanoutHeader {
    let mut header = FanoutHeader {
        title: if fanout.group_title.trim().is_empty() {
            fanout.group_id.clone()
        } else {
            fanout.group_title.clone()
        },
        target_count: fanout.target_count,
        running: 0,
        waiting: 0,
        paused: 0,
        done: 0,
        failed: 0,
        stopped: 0,
        uncertain: 0,
    };

    for row in member_indices.iter().filter_map(|idx| rows.get(*idx)) {
        if matches!(
            row.state.confidence,
            AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
        ) {
            header.uncertain += 1;
            continue;
        }
        match row.state.status {
            AgentRunStatus::Starting
            | AgentRunStatus::Running
            | AgentRunStatus::Pausing
            | AgentRunStatus::Resuming
            | AgentRunStatus::Cancelling => {
                header.running += 1;
            }
            AgentRunStatus::Waiting => header.waiting += 1,
            AgentRunStatus::Paused => header.paused += 1,
            AgentRunStatus::Completed | AgentRunStatus::Delegated => header.done += 1,
            AgentRunStatus::Failed => header.failed += 1,
            AgentRunStatus::Interrupted | AgentRunStatus::Cancelled => header.stopped += 1,
        }
    }

    header
}

fn format_elapsed(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs}s")
    }
}

const PAGE_STEP: usize = 8;

impl BottomPaneView for InFlightAgentsView {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        if area.height == 0 || area.width == 0 {
            return;
        }
        let theme = crate::tui::theme::current();
        let dim = Style::default().fg(theme.dim);
        let title_style = Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD);

        // Header
        let live = self.live_count;
        let failed = self.failed_count;
        let uncertain = self.uncertain_count;
        let attention = self.rows.iter().filter(|row| row.has_attention()).count();
        let waiting = self
            .rows
            .iter()
            .filter(|row| {
                row.state.status == AgentRunStatus::Waiting
                    && !matches!(
                        row.state.confidence,
                        AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
                    )
            })
            .count();
        let paused = self
            .rows
            .iter()
            .filter(|row| {
                row.state.status == AgentRunStatus::Paused
                    && !matches!(
                        row.state.confidence,
                        AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
                    )
            })
            .count();
        let working = live.saturating_sub(waiting).saturating_sub(paused);
        let completed = self
            .rows
            .iter()
            .filter(|row| {
                row.state.status == AgentRunStatus::Completed
                    && !matches!(
                        row.state.confidence,
                        AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
                    )
            })
            .count();
        let stopped = self
            .rows
            .iter()
            .filter(|row| {
                matches!(
                    row.state.status,
                    AgentRunStatus::Interrupted | AgentRunStatus::Cancelled
                ) && !matches!(
                    row.state.confidence,
                    AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
                )
            })
            .count();
        let mut counts = Vec::new();
        if working > 0 {
            counts.push(format!("{working} working"));
        }
        if waiting > 0 {
            counts.push(format!("{waiting} waiting"));
        }
        if paused > 0 {
            counts.push(format!("{paused} paused"));
        }
        if completed > 0 {
            counts.push(format!("{completed} done"));
        }
        if failed > 0 {
            counts.push(format!("{failed} failed"));
        }
        if stopped > 0 {
            counts.push(format!("{stopped} stopped"));
        }
        if uncertain > 0 {
            counts.push(format!("{uncertain} unconfirmed"));
        }
        if attention > 0 {
            counts.push(format!("{attention} attention"));
        }
        if self.durable_snapshot_truncated {
            counts.push("partial server list".to_string());
        }
        match self.server_truth_state {
            ServerAgentTruthState::Loading => counts.push("server loading".to_string()),
            ServerAgentTruthState::Stale => counts.push("server stale".to_string()),
            ServerAgentTruthState::Unavailable => counts.push("server unavailable".to_string()),
            ServerAgentTruthState::Unbound | ServerAgentTruthState::Confirmed => {}
        }
        let header_label = if self.show_root_conversation {
            "Conversations"
        } else {
            "Agent runs"
        };
        let header_text = if counts.is_empty() {
            format!("  {header_label}")
        } else {
            format!("  {header_label} · {}", counts.join(" · "))
        };
        let header = Line::from(Span::styled(header_text, title_style));
        buf.set_line(area.x, area.y, &header, area.width);

        if self.rows.is_empty() && !self.show_root_conversation {
            let (message, style) = match self.server_truth_state {
                ServerAgentTruthState::Loading => (
                    "  Loading durable agent state…",
                    Style::default().fg(theme.accent),
                ),
                ServerAgentTruthState::Unavailable => (
                    "  Durable agent state unavailable · R refresh",
                    Style::default().fg(theme.warn),
                ),
                ServerAgentTruthState::Stale => (
                    "  No current rows · durable agent snapshot is stale",
                    Style::default().fg(theme.warn),
                ),
                ServerAgentTruthState::Confirmed => {
                    ("  No agent runs in this session · server confirmed", dim)
                }
                ServerAgentTruthState::Unbound => ("  No agent runs in this session.", dim),
            };
            let empty = Line::from(Span::styled(message, style));
            if area.height >= 2 {
                buf.set_line(area.x, area.y + 1, &empty, area.width);
            }
            return;
        }

        let body_y = area.y + 1;
        let detail_h = usize::from(area.height >= 5) * 2;
        let body_h = area
            .height
            .saturating_sub(1)
            .saturating_sub(detail_h as u16) as usize;
        let entries = agent_list_entries(&self.rows, self.show_root_conversation);
        let selected_entry = entries
            .iter()
            .position(|entry| {
                matches!(entry, AgentListEntry::RootConversation)
                    .then_some(Self::ROOT_SELECTION)
                    .or_else(|| entry.row_index())
                    == Some(self.selected)
            })
            .unwrap_or(0);
        let window_start = selected_entry.saturating_add(1).saturating_sub(body_h);
        for (i, entry) in entries.iter().skip(window_start).take(body_h).enumerate() {
            let line = match entry {
                AgentListEntry::RootConversation => {
                    let selected = self.selected == Self::ROOT_SELECTION;
                    let marker = if selected { "› " } else { "  " };
                    Line::from(vec![
                        Span::styled(
                            marker.to_string(),
                            if selected {
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                dim
                            },
                        ),
                        Span::styled(
                            "Main conversation".to_string(),
                            if selected {
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(theme.accent)
                            },
                        ),
                        Span::styled(" · root · transcript".to_string(), dim),
                    ])
                }
                AgentListEntry::FanoutHeader(header) => fanout_header_line(header, dim),
                AgentListEntry::Row {
                    row_idx,
                    row,
                    grouped,
                } => {
                    let selected = *row_idx == self.selected;
                    let marker = if selected { "› " } else { "  " };
                    let status_color = state_color(row.state);
                    let label = if *grouped {
                        fanout_slot_row_label(*row_idx, row)
                    } else {
                        format!("{}. {}", row_idx + 1, row.name)
                    };
                    let label = format!("{}{label}", lineage_prefix(row.depth));
                    let content_width = usize::from(area.width).saturating_sub(2);
                    let meta_budget = content_width.saturating_sub(11);
                    let meta = row_meta_for_width(row, meta_budget);
                    let separator_width = if meta.is_empty() { 0 } else { 3 };
                    let label_budget = content_width
                        .saturating_sub(UnicodeWidthStr::width(meta.as_str()))
                        .saturating_sub(separator_width);
                    let label = truncate_to_width(&label, label_budget);
                    let mut spans = vec![
                        Span::styled(
                            marker.to_string(),
                            if selected {
                                Style::default()
                                    .fg(theme.accent)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                dim
                            },
                        ),
                        Span::styled(
                            label,
                            if selected {
                                Style::default()
                                    .fg(status_color)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(status_color)
                            },
                        ),
                    ];
                    if !meta.is_empty() {
                        spans.push(Span::styled(format!(" · {meta}"), dim));
                    }
                    if *grouped {
                        spans.push(Span::styled(
                            format!(" · {}", truncate_label(&row.agent_id, 18)),
                            dim,
                        ));
                    }
                    Line::from(spans)
                }
            };
            buf.set_line(area.x, body_y + i as u16, &line, area.width);
        }
        if detail_h > 0 && self.selected == Self::ROOT_SELECTION {
            let detail_y = body_y + body_h as u16;
            for (offset, text) in root_conversation_detail().into_iter().enumerate() {
                let line = Line::from(Span::styled(
                    truncate_to_width(&text, usize::from(area.width)),
                    if offset == 0 {
                        Style::default().fg(theme.accent)
                    } else {
                        dim
                    },
                ));
                buf.set_line(area.x, detail_y + offset as u16, &line, area.width);
            }
        } else if detail_h > 0
            && let Some(row) = self.rows.get(self.selected)
        {
            let detail_y = body_y + body_h as u16;
            let detail = selected_runtime_detail(row);
            for (offset, text) in detail.into_iter().enumerate() {
                let line = Line::from(Span::styled(
                    truncate_to_width(&text, usize::from(area.width)),
                    if offset == 0 {
                        Style::default().fg(theme.accent)
                    } else {
                        dim
                    },
                ));
                buf.set_line(area.x, detail_y + offset as u16, &line, area.width);
            }
        }
    }

    fn desired_height(&self, _width: u16) -> u16 {
        let rows = agent_list_entries(&self.rows, self.show_root_conversation)
            .len()
            .max(1);
        (rows as u16).saturating_add(3).min(12)
    }

    fn handle_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::PageUp => self.move_page_up(),
            KeyCode::PageDown => self.move_page_down(),
            KeyCode::Home => {
                self.selected = if self.show_root_conversation {
                    Self::ROOT_SELECTION
                } else {
                    0
                }
            }
            KeyCode::End if !self.rows.is_empty() => self.selected = self.rows.len() - 1,
            KeyCode::Char(ch) if ('1'..='9').contains(&ch) => self.select_number(ch as u8 - b'0'),
            KeyCode::Enter | KeyCode::Right => self.accept(),
            // Kill the selected live agent. `x` is the conventional
            // "stop"/"close" gesture in dashboard-style TUIs; `Delete`
            // is the keyboard-discoverable equivalent.
            KeyCode::Char('x') | KeyCode::Char('X') | KeyCode::Delete => {
                self.request_control(astra_thin_client::SessionRunAction::Cancel);
            }
            KeyCode::Char('p') | KeyCode::Char('P') => self.request_pause_or_resume(),
            KeyCode::Char('g') | KeyCode::Char('G') => self.request_guide(),
            KeyCode::Char('a') => self.focus_next_attention(false),
            KeyCode::Char('A') => self.focus_next_attention(true),
            KeyCode::Char('r') | KeyCode::Char('R')
                if self.server_truth_state != ServerAgentTruthState::Unbound =>
            {
                self.pending_action = Some(ViewActionRequest {
                    action: BottomPaneViewAction::RefreshAgentMonitor,
                    disposition: ViewActionDisposition::KeepOpen,
                });
            }
            KeyCode::Esc | KeyCode::Left | KeyCode::Char('q') => {
                self.completed = true;
            }
            _ => {}
        }
    }

    fn cursor_pos(&self, _area: Rect) -> Option<(u16, u16)> {
        None
    }

    fn is_complete(&self) -> bool {
        self.completed
    }

    fn take_action_request(&mut self) -> Option<ViewActionRequest> {
        self.pending_action.take()
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        self.completed = true;
        CancellationEvent::Consumed
    }

    fn refresh_agent_monitor(&mut self, snapshot: AgentMonitorSnapshot) -> bool {
        self.replace_snapshot(snapshot);
        true
    }

    fn accepts_agent_rows(&self) -> bool {
        true
    }

    fn hint_keys(&self) -> Option<String> {
        if self.rows.is_empty() && !self.show_root_conversation {
            let hint = if self.server_truth_state != ServerAgentTruthState::Unbound {
                "R refresh · ←/Esc close"
            } else {
                "←/Esc close"
            };
            return Some(hint.into());
        }
        let mut hints = vec!["↑↓ move", "Enter/→ transcript"];
        let selected = self.rows.get(self.selected);
        if selected.is_some_and(|row| {
            row.target_for_action(astra_thin_client::SessionRunAction::Pause)
                .is_some()
                || row
                    .target_for_action(astra_thin_client::SessionRunAction::Resume)
                    .is_some()
                || row
                    .target_for_action(astra_thin_client::SessionRunAction::ContinueSession)
                    .is_some()
        }) {
            hints.push("P pause/resume/continue");
        }
        if selected.is_some_and(|row| {
            row.target_for_action(astra_thin_client::SessionRunAction::Cancel)
                .is_some()
        }) {
            hints.push("X stop");
        }
        if selected.is_some_and(|row| row.guide_target().is_some()) {
            hints.push("G guide");
        }
        if self.rows.iter().any(AgentRow::has_attention) {
            hints.push("A attention");
        }
        if self.server_truth_state != ServerAgentTruthState::Unbound {
            hints.push("R refresh");
        }
        hints.push("←/Esc close");
        Some(hints.join(" · "))
    }
}

use crate::cli::effects::truncate_label;

fn lineage_prefix(depth: u32) -> String {
    if depth <= 1 {
        return String::new();
    }
    let indentation = "  ".repeat(depth.saturating_sub(2).min(3) as usize);
    format!("{indentation}{} ", crate::tui::glyphs::current().lineage)
}

fn activity_labels(activity: AgentActivityCounts) -> Vec<String> {
    let mut labels = Vec::new();
    if activity.tool_calls > 0 {
        labels.push(if activity.tool_calls == 1 {
            "1 tool".into()
        } else {
            format!("{} tools", activity.tool_calls)
        });
    }
    if activity.child_agents > 0 {
        labels.push(if activity.child_agents_partial {
            if activity.child_agents == 1 {
                "≥1 child".into()
            } else {
                format!("≥{} children", activity.child_agents)
            }
        } else if activity.child_agents == 1 {
            "1 child".into()
        } else {
            format!("{} children", activity.child_agents)
        });
    }
    if activity.messages_sent > 0 {
        labels.push(format!("{} sent", activity.messages_sent));
    }
    if activity.messages_received > 0 {
        labels.push(format!("{} received", activity.messages_received));
    }
    labels
}

fn selected_runtime_detail(row: &AgentRow) -> [String; 2] {
    let mut overview = Vec::with_capacity(6);
    match row.run_id.as_deref().filter(|id| !id.is_empty()) {
        Some(run_id) => overview.push(format!("run {}", truncate_label(run_id, 18))),
        None => overview.push("run identity unavailable".into()),
    }
    let parent = row
        .parent_run_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| truncate_label(id, 18))
        .unwrap_or_else(|| "root".into());
    overview.push(format!("parent {parent}"));
    if row.depth > 1 {
        overview.push(format!("depth {}", row.depth));
    }
    if let Some(runtime) = row
        .runtime
        .runtime_profile
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        overview.push(runtime.replace('_', " "));
    }
    if let Some(model) = row
        .runtime
        .model_name
        .as_deref()
        .filter(|value| !value.is_empty())
    {
        overview.push(model.to_string());
    }
    if let Some(background) = row.runtime.background {
        overview.push(if background {
            "background".into()
        } else {
            "foreground".into()
        });
    }
    let glyphs = crate::tui::glyphs::current();
    let line_one = format!("  {} {}", glyphs.detail_branch, overview.join(" · "));

    let mut details = Vec::with_capacity(4);
    if row.run_id.as_deref().is_none_or(str::is_empty) {
        details.push("live transcript opens now · run identity and controls are syncing".into());
    }
    if let Some(facts) = row.runtime.permission.as_ref() {
        let health = if facts.has_issues {
            "permission issues"
        } else {
            "permissions clear"
        };
        details.push(format!(
            "{health} · {}/{} approved · {} blocked",
            facts.approved, facts.requests, facts.tools_blocked
        ));
    }
    if let Some(summary) = row
        .attention_summary
        .as_deref()
        .filter(|summary| !summary.trim().is_empty())
    {
        details.push(format!("attention {summary}"));
    }
    if let Some(binding) = row
        .runtime
        .agent_binding_name
        .as_deref()
        .or(row.runtime.agent_binding_id.as_deref())
        .filter(|value| !value.is_empty())
    {
        details.push(format!("binding {binding}"));
    }
    let mut controls = Vec::with_capacity(3);
    if row
        .target_for_action(astra_thin_client::SessionRunAction::Pause)
        .is_some()
    {
        controls.push("pause available (P)");
    }
    if row
        .target_for_action(astra_thin_client::SessionRunAction::Resume)
        .is_some()
    {
        controls.push("resume available (P)");
    }
    if row
        .target_for_action(astra_thin_client::SessionRunAction::Cancel)
        .is_some()
    {
        controls.push("stop available (X)");
    }
    if row.guide_target().is_some() {
        controls.push("guide available (G)");
    }
    if !controls.is_empty() {
        details.push(controls.join(" · "));
    }
    if let Some(target) = row.transcript_target {
        let location = match target {
            AgentTranscriptTarget::LocalJournal => "local journal",
            AgentTranscriptTarget::DurableServer => "durable server",
        };
        details.push(format!("transcript {location}"));
    }
    if details.is_empty() {
        details.push(if row.run_id.as_deref().is_some_and(|id| !id.is_empty()) {
            "live transcript available · canonical location pending".into()
        } else {
            "live transcript opens now · run identity and controls are syncing".into()
        });
    }
    let line_two = format!("  {} {}", glyphs.detail_last, details.join(" · "));
    [line_one, line_two]
}

fn transcript_run_id(row: &AgentRow) -> Option<&str> {
    match row.control_target.as_ref() {
        Some(AgentControlTarget::DurableRun { run_id }) if !run_id.is_empty() => Some(run_id),
        _ => row.run_id.as_deref().filter(|run_id| !run_id.is_empty()),
    }
}

fn root_conversation_detail() -> [String; 2] {
    let glyphs = crate::tui::glyphs::current();
    [
        format!(
            "  {} root conversation · current session",
            glyphs.detail_branch
        ),
        format!(
            "  {} Enter opens the same transcript browser as every delegated run",
            glyphs.detail_last
        ),
    ]
}

fn provenance_label(source: AgentProjectionSource) -> &'static str {
    match source {
        AgentProjectionSource::LiveStream => "live event",
        AgentProjectionSource::LocalJournal => "local journal",
        AgentProjectionSource::LocalRuntime => "local runtime",
        AgentProjectionSource::DurableServer => "server record",
        AgentProjectionSource::WorkspaceSnapshot => "restored snapshot",
        AgentProjectionSource::LocalIntent => "pending intent",
    }
}

fn row_meta_parts(row: &AgentRow, include_activity: bool) -> Vec<String> {
    let mut parts = Vec::new();
    if let Some(status) = state_phrase(row.state) {
        parts.push(status.to_string());
    }
    parts.push(provenance_label(row.provenance).to_string());
    if include_activity {
        parts.extend(activity_labels(row.activity));
    }
    parts.push(format_elapsed(row.elapsed_ms));
    parts
}

fn row_meta_for_width(row: &AgentRow, max_width: usize) -> String {
    let full = row_meta_parts(row, true);
    let essential = row_meta_parts(row, false);
    let status_and_elapsed = state_phrase(row.state)
        .map(|status| vec![status.to_string(), format_elapsed(row.elapsed_ms)]);
    let provenance_and_elapsed = vec![
        provenance_label(row.provenance).to_string(),
        format_elapsed(row.elapsed_ms),
    ];
    let elapsed = vec![format_elapsed(row.elapsed_ms)];

    [
        Some(full),
        Some(essential),
        status_and_elapsed,
        Some(provenance_and_elapsed),
        Some(elapsed),
    ]
    .into_iter()
    .flatten()
    .map(|parts| parts.join(" · "))
    .find(|candidate| UnicodeWidthStr::width(candidate.as_str()) <= max_width)
    .unwrap_or_default()
}

fn truncate_to_width(text: &str, max_width: usize) -> String {
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    if max_width == 0 {
        return String::new();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let content_width = max_width - 1;
    let mut width = 0;
    let mut truncated = String::new();
    for ch in text.chars() {
        let char_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + char_width > content_width {
            break;
        }
        width += char_width;
        truncated.push(ch);
    }
    truncated.push('…');
    truncated
}

fn state_color(state: AgentRunState) -> Color {
    let theme = crate::tui::theme::current();
    if matches!(
        state.confidence,
        AgentProjectionConfidence::Stale | AgentProjectionConfidence::Unconfirmed
    ) {
        return theme.dim;
    }
    match state.status {
        AgentRunStatus::Starting
        | AgentRunStatus::Running
        | AgentRunStatus::Waiting
        | AgentRunStatus::Paused
        | AgentRunStatus::Pausing
        | AgentRunStatus::Resuming
        | AgentRunStatus::Cancelling
        | AgentRunStatus::Interrupted => theme.warn,
        AgentRunStatus::Completed | AgentRunStatus::Delegated => theme.success,
        AgentRunStatus::Failed => theme.error,
        AgentRunStatus::Cancelled => theme.dim,
    }
}

fn state_phrase(state: AgentRunState) -> Option<&'static str> {
    match state.confidence {
        AgentProjectionConfidence::Unconfirmed => return Some("status unconfirmed"),
        AgentProjectionConfidence::Stale => return Some("stale"),
        AgentProjectionConfidence::Observed | AgentProjectionConfidence::Confirmed => {}
    }
    match state.status {
        AgentRunStatus::Starting => Some("starting"),
        AgentRunStatus::Running => None,
        AgentRunStatus::Waiting => Some("waiting"),
        AgentRunStatus::Paused => Some("paused"),
        AgentRunStatus::Pausing => Some("pausing"),
        AgentRunStatus::Resuming => Some("resuming"),
        AgentRunStatus::Cancelling => Some("stopping"),
        AgentRunStatus::Completed => Some("done"),
        AgentRunStatus::Delegated => Some("delegated"),
        AgentRunStatus::Interrupted => Some("interrupted"),
        AgentRunStatus::Failed => Some("failed"),
        AgentRunStatus::Cancelled => Some("stopped"),
    }
}

fn fanout_header_line(header: &FanoutHeader, dim: Style) -> Line<'static> {
    let mut parts = vec![format!("{} target", header.target_count)];
    if header.running > 0 {
        parts.push(format!("{} running", header.running));
    }
    if header.waiting > 0 {
        parts.push(format!("{} waiting", header.waiting));
    }
    if header.paused > 0 {
        parts.push(format!("{} paused", header.paused));
    }
    if header.done > 0 {
        parts.push(format!("{} done", header.done));
    }
    if header.failed > 0 {
        parts.push(format!("{} failed", header.failed));
    }
    if header.stopped > 0 {
        parts.push(format!("{} stopped", header.stopped));
    }
    if header.uncertain > 0 {
        parts.push(format!("{} unconfirmed", header.uncertain));
    }

    Line::from(vec![
        Span::styled("  ▣ ".to_string(), dim),
        Span::styled(
            truncate_label(&header.title, 30),
            Style::default()
                .fg(crate::tui::theme::current().accent)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", parts.join(" · ")), dim),
    ])
}

fn fanout_slot_row_label(row_idx: usize, row: &AgentRow) -> String {
    let Some(fanout) = row.fanout.as_ref() else {
        return format!("{}. {}", row_idx + 1, truncate_label(&row.name, 38));
    };
    let label = if fanout.slot_label.trim().is_empty() {
        row.name.as_str()
    } else {
        fanout.slot_label.as_str()
    };
    format!(
        "{}. slot {}: {}",
        row_idx + 1,
        fanout.slot_index + 1,
        truncate_label(label, 30)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::testing::render::{buffer_to_string, draw_widget};
    use crossterm::event::{KeyEventKind, KeyEventState, KeyModifiers};
    use ratatui::widgets::Widget;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn rows(n: usize) -> Vec<AgentRow> {
        (0..n)
            .map(|i| AgentRow {
                agent_id: format!("agent-{i}"),
                name: format!("task {i}"),
                spawn_tool_call_id: None,
                activity: AgentActivityCounts {
                    tool_calls: i,
                    child_agents: 0,
                    messages_sent: 0,
                    messages_received: 0,
                    child_agents_partial: false,
                },
                run_id: Some(format!("run-{i}")),
                parent_run_id: Some("root-run".into()),
                depth: 1,
                provenance: AgentProjectionSource::LiveStream,
                elapsed_ms: 1000 * (i as u64 + 1),
                state: AgentRunState::observed(AgentRunStatus::Running),
                attention_summary: None,
                fanout: None,
                control_target: Some(AgentControlTarget::LocalAgent {
                    agent_id: format!("agent-{i}"),
                }),
                transcript_target: Some(AgentTranscriptTarget::LocalJournal),
                available_actions: vec![astra_thin_client::SessionRunAction::Cancel],
                runtime: Default::default(),
            })
            .collect()
    }

    fn fanout(group_id: &str, target_count: usize, slot_index: usize) -> AgentFanoutMembership {
        AgentFanoutMembership {
            group_id: group_id.to_string(),
            group_title: "review fanout".to_string(),
            target_count,
            slot_index,
            slot_label: format!("slot task {slot_index}"),
        }
    }

    fn render(view: &InFlightAgentsView, width: u16, height: u16) -> String {
        struct ViewWidget<'a>(&'a InFlightAgentsView);
        impl Widget for ViewWidget<'_> {
            fn render(self, area: Rect, buf: &mut Buffer) {
                self.0.render(area, buf);
            }
        }
        buffer_to_string(&draw_widget(ViewWidget(view), width, height))
    }

    /// Empty agent list: must not panic, must not select anything.
    #[test]
    fn empty_list_is_inert() {
        let mut v = InFlightAgentsView::new(vec![]);
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Up));
        v.handle_key(key(KeyCode::Enter));
        // Enter on empty must not complete the view with a result.
        assert!(v.completion().is_none());
        // Esc completes without a result (just dismisses).
        v.handle_key(key(KeyCode::Esc));
        assert!(v.is_complete());
        assert!(v.completion().is_none());
    }

    #[test]
    fn workbench_root_is_a_transcript_entry_not_a_status_summary() {
        let snapshot = AgentMonitorSnapshot {
            show_root_conversation: true,
            ..AgentMonitorSnapshot::default()
        };
        assert!(snapshot.should_open());
        let mut view = InFlightAgentsView::new(snapshot);
        assert_eq!(view.selected, InFlightAgentsView::ROOT_SELECTION);
        let rendered = render(&view, 90, 5);
        assert!(rendered.contains("Conversations"), "{rendered}");
        assert!(rendered.contains("Main conversation"), "{rendered}");
        assert!(rendered.contains("same transcript browser"), "{rendered}");
        assert_eq!(
            view.hint_keys().as_deref(),
            Some("↑↓ move · Enter/→ transcript · ←/Esc close")
        );

        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::OpenRootTranscript,
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
    }

    #[test]
    fn selected_waiting_agent_shows_typed_attention_reason() {
        let mut rows = rows(1);
        rows[0].state = AgentRunState::observed(AgentRunStatus::Waiting);
        rows[0].attention_summary = Some("Approval required · git status".into());
        let rendered = render(&InFlightAgentsView::new(rows), 100, 5);

        assert!(rendered.contains("waiting"), "{rendered}");
        assert!(
            rendered.contains("attention Approval required · git status"),
            "{rendered}"
        );
    }

    #[test]
    fn attention_navigation_cycles_typed_attention_without_reordering_the_run_tree() {
        let mut rows = rows(3);
        rows[1].state = AgentRunState::observed(AgentRunStatus::Waiting);
        rows[2].state = AgentRunState::observed(AgentRunStatus::Waiting);
        rows[1].attention_summary = Some("Approval required".into());
        rows[2].attention_summary = Some("Waiting for user input".into());
        let mut view = InFlightAgentsView::new(rows);

        assert_eq!(view.selected, 0);
        assert!(render(&view, 100, 5).contains("2 attention"));
        assert!(
            view.hint_keys()
                .expect("agent monitor hints")
                .contains("A attention")
        );

        view.handle_key(key(KeyCode::Char('a')));
        assert_eq!(view.selected, 1);
        view.handle_key(key(KeyCode::Char('a')));
        assert_eq!(view.selected, 2);
        view.handle_key(key(KeyCode::Char('A')));
        assert_eq!(view.selected, 1);

        // The normal tree is unchanged: ordinary keyboard navigation remains
        // positional rather than turning attention into a separate local
        // source of truth.
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.selected, 2);
    }

    /// Down/Up arrow navigation wraps correctly in both directions.
    #[test]
    fn navigation_wraps() {
        let mut v = InFlightAgentsView::new(rows(3));
        assert_eq!(v.selected, 0);
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.selected, 1);
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Down)); // wraps to 0
        assert_eq!(v.selected, 0);
        v.handle_key(key(KeyCode::Up)); // wraps to 2
        assert_eq!(v.selected, 2);
    }

    /// Enter produces a typed inspect action while preserving the navigator
    /// as the transcript's parent surface.
    #[test]
    fn enter_emits_typed_inspect_action_that_keeps_navigation_open() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Enter));
        assert_eq!(
            v.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent {
                    agent_id: "agent-1".into(),
                    agent_name: "task 1".into(),
                    run_id: Some("run-1".into()),
                    transcript_target: Some(AgentTranscriptTarget::LocalJournal),
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
        assert!(!v.is_complete());
    }

    #[test]
    fn right_arrow_drills_into_the_selected_agent_transcript() {
        let mut v = InFlightAgentsView::new(rows(1));

        v.handle_key(key(KeyCode::Right));

        assert!(matches!(
            v.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent {
                    agent_id,
                    run_id: Some(run_id),
                    ..
                },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-0" && run_id == "run-0"
        ));
    }

    #[test]
    fn enter_opens_live_transcript_when_run_identity_precedes_location() {
        let mut pending = rows(1);
        pending[0].transcript_target = None;
        let mut view = InFlightAgentsView::new(pending);

        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent {
                    agent_id: "agent-0".into(),
                    agent_name: "task 0".into(),
                    run_id: Some("run-0".into()),
                    transcript_target: None,
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );

        view.replace_snapshot(AgentMonitorSnapshot::complete(rows(1)));
        assert!(view.take_action_request().is_none());
    }

    /// `x` on a Live row emits a typed cancel action
    /// — the view STAYS OPEN so the user keeps watching the row
    /// transition Live → Cancelling → Cancelled, and can stop more
    /// agents in the same Ctrl+G session without re-opening the view.
    ///
    /// Pre-fix: `x` set `completed=true`, dropping the user back into
    /// the chat with no visibility into whether the cancel landed.
    #[test]
    fn x_on_live_row_emits_typed_cancel_action_and_stays_open() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char('x')));

        let pending = v.take_action_request();
        assert_eq!(
            pending,
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent {
                    agent_id: "agent-1".into(),
                    target: AgentControlTarget::LocalAgent {
                        agent_id: "agent-1".into(),
                    },
                    action: astra_thin_client::SessionRunAction::Cancel,
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
        // Drained: a second poll returns None.
        assert!(v.take_action_request().is_none());
        // Critical: the view is NOT complete — the user keeps watching.
        assert!(!v.is_complete(), "x on live row must NOT close the monitor");
        assert!(
            v.completion().is_none(),
            "view must not produce a completion until the user explicitly dismisses"
        );
    }

    #[test]
    fn delete_key_also_emits_typed_cancel_action() {
        let mut v = InFlightAgentsView::new(rows(2));
        v.handle_key(key(KeyCode::Delete));
        let pending = v.take_action_request();
        assert_eq!(
            pending,
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent {
                    agent_id: "agent-0".into(),
                    target: AgentControlTarget::LocalAgent {
                        agent_id: "agent-0".into(),
                    },
                    action: astra_thin_client::SessionRunAction::Cancel,
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
        assert!(!v.is_complete());
    }

    #[test]
    fn durable_row_emits_its_server_run_target() {
        let mut durable_rows = rows(1);
        durable_rows[0].state = AgentRunState::confirmed_server(AgentRunStatus::Paused);
        durable_rows[0].control_target = Some(AgentControlTarget::DurableRun {
            run_id: "durable-run-1".into(),
        });
        durable_rows[0].transcript_target = Some(AgentTranscriptTarget::DurableServer);
        durable_rows[0].available_actions = vec![astra_thin_client::SessionRunAction::Cancel];
        let mut view = InFlightAgentsView::new(durable_rows);

        view.handle_key(key(KeyCode::Char('x')));
        assert_eq!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent {
                    agent_id: "agent-0".into(),
                    target: AgentControlTarget::DurableRun {
                        run_id: "durable-run-1".into(),
                    },
                    action: astra_thin_client::SessionRunAction::Cancel,
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
    }

    #[test]
    fn guide_is_a_typed_workbench_action_for_running_durable_agents() {
        let mut durable_rows = rows(1);
        durable_rows[0].name = "Reviewer".into();
        durable_rows[0].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        durable_rows[0].control_target = Some(AgentControlTarget::DurableRun {
            run_id: "durable-run-1".into(),
        });
        let mut view = InFlightAgentsView::new(durable_rows);

        assert!(view.hint_keys().unwrap().contains("G guide"));
        view.handle_key(key(KeyCode::Char('g')));
        assert_eq!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::BeginAgentGuide {
                    agent_id: "agent-0".into(),
                    agent_name: "Reviewer".into(),
                    run_id: "durable-run-1".into(),
                    target: AgentControlTarget::DurableRun {
                        run_id: "durable-run-1".into(),
                    },
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
        assert!(!view.is_complete());
    }

    #[test]
    fn enter_on_durable_agent_carries_exact_transcript_target() {
        let mut durable_rows = rows(1);
        durable_rows[0].name = "Reviewer".into();
        durable_rows[0].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        durable_rows[0].control_target = Some(AgentControlTarget::DurableRun {
            run_id: "durable-run-1".into(),
        });
        durable_rows[0].transcript_target = Some(AgentTranscriptTarget::DurableServer);
        let mut view = InFlightAgentsView::new(durable_rows);

        view.handle_key(key(KeyCode::Enter));
        assert_eq!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent {
                    agent_id: "agent-0".into(),
                    agent_name: "Reviewer".into(),
                    run_id: Some("durable-run-1".into()),
                    transcript_target: Some(AgentTranscriptTarget::DurableServer),
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
    }

    #[test]
    fn guide_uses_the_local_runtime_mailbox_target() {
        let mut local = InFlightAgentsView::new(rows(1));
        assert!(local.hint_keys().unwrap().contains("G guide"));
        local.handle_key(key(KeyCode::Char('g')));
        assert!(matches!(
            local.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::BeginAgentGuide {
                    target: AgentControlTarget::LocalAgent { agent_id },
                    ..
                },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-0"
        ));

        // Paused durable runs still cannot apply a new model-boundary intent.
        let mut paused_rows = rows(1);
        paused_rows[0].state = AgentRunState::confirmed_server(AgentRunStatus::Paused);
        paused_rows[0].control_target = Some(AgentControlTarget::DurableRun {
            run_id: "durable-run-1".into(),
        });
        let mut paused = InFlightAgentsView::new(paused_rows);
        assert!(!paused.hint_keys().unwrap().contains("guide"));
        paused.handle_key(key(KeyCode::Char('g')));
        assert!(paused.take_action_request().is_none());
    }

    #[test]
    fn pause_key_emits_pause_for_running_durable_run_and_resume_for_paused_run() {
        let durable_target = AgentControlTarget::DurableRun {
            run_id: "durable-run-1".into(),
        };
        let mut running_rows = rows(1);
        running_rows[0].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        running_rows[0].control_target = Some(durable_target.clone());
        running_rows[0].available_actions = vec![
            astra_thin_client::SessionRunAction::Pause,
            astra_thin_client::SessionRunAction::Cancel,
        ];
        let mut running = InFlightAgentsView::new(running_rows);
        running.handle_key(key(KeyCode::Char('p')));
        assert_eq!(
            running.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent {
                    agent_id: "agent-0".into(),
                    target: durable_target.clone(),
                    action: astra_thin_client::SessionRunAction::Pause,
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );

        let mut paused_rows = rows(1);
        paused_rows[0].state = AgentRunState::confirmed_server(AgentRunStatus::Paused);
        paused_rows[0].control_target = Some(durable_target.clone());
        paused_rows[0].available_actions = vec![
            astra_thin_client::SessionRunAction::Resume,
            astra_thin_client::SessionRunAction::Cancel,
        ];
        let mut paused = InFlightAgentsView::new(paused_rows);
        paused.handle_key(key(KeyCode::Char('P')));
        assert_eq!(
            paused.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent {
                    agent_id: "agent-0".into(),
                    target: durable_target,
                    action: astra_thin_client::SessionRunAction::Resume,
                },
                disposition: ViewActionDisposition::KeepOpen,
            })
        );
    }

    #[test]
    fn pause_hint_and_action_only_exist_when_backend_declares_them() {
        let mut local = InFlightAgentsView::new(rows(1));
        let local_hints = local.hint_keys().unwrap_or_default();
        assert!(!local_hints.contains("P pause/resume"));
        assert!(local_hints.contains("G guide"));
        local.handle_key(key(KeyCode::Char('p')));
        assert_eq!(local.take_action_request(), None);

        let mut durable_rows = rows(1);
        durable_rows[0].available_actions = vec![astra_thin_client::SessionRunAction::Pause];
        let durable = InFlightAgentsView::new(durable_rows);
        assert!(
            durable
                .hint_keys()
                .is_some_and(|hints| hints.contains("P pause/resume"))
        );
    }

    #[test]
    fn row_without_declared_cancel_action_is_not_actionable() {
        let mut unavailable_rows = rows(1);
        unavailable_rows[0].available_actions.clear();
        let mut view = InFlightAgentsView::new(unavailable_rows);

        view.handle_key(key(KeyCode::Char('x')));
        assert_eq!(view.take_action_request(), None);
    }

    #[test]
    fn x_on_terminal_row_is_inert_and_keeps_view_open() {
        // Pressing x on a row that already finished (Completed/Failed/
        // Cancelled) must NOT emit a stop AND must not close the view.
        let mut rows = rows(3);
        rows[0].state = AgentRunState::observed(AgentRunStatus::Completed);
        rows[1].state = AgentRunState::observed(AgentRunStatus::Failed);
        rows[2].state = AgentRunState::observed(AgentRunStatus::Cancelled);
        let mut v = InFlightAgentsView::new(rows);
        for _ in 0..3 {
            v.handle_key(key(KeyCode::Char('x')));
            v.handle_key(key(KeyCode::Down));
        }
        assert!(
            !v.is_complete(),
            "x on terminal rows must not complete view"
        );
        assert!(v.take_action_request().is_none());
    }

    #[test]
    fn x_on_cancelling_row_does_not_duplicate_in_flight_intent() {
        let mut rs = rows(2);
        rs[0].state = AgentRunState::local_intent(AgentRunStatus::Cancelling);
        let mut v = InFlightAgentsView::new(rs);
        v.handle_key(key(KeyCode::Char('x')));
        assert!(v.take_action_request().is_none());
        assert!(!v.is_complete());
    }

    #[test]
    fn unconfirmed_active_row_stays_visible_and_is_not_actionable() {
        let mut rs = rows(1);
        rs[0].state = AgentRunState::unconfirmed(AgentRunStatus::Running);
        let mut view = InFlightAgentsView::new(rs);

        view.handle_key(key(KeyCode::Char('x')));
        assert!(view.take_action_request().is_none());
        assert!(!view.is_complete());
        let output = render(&view, 90, 4);
        assert!(output.contains("1 unconfirmed"), "{output}");
        assert!(output.contains("status unconfirmed"), "{output}");
    }

    #[test]
    fn waiting_agent_is_counted_separately_from_working_agent() {
        let mut rs = rows(2);
        rs[1].state = AgentRunState::confirmed_local(AgentRunStatus::Waiting);
        let mut view = InFlightAgentsView::new(rs);

        let output = render(&view, 90, 5);
        assert!(output.contains("1 working"), "{output}");
        assert!(output.contains("1 waiting"), "{output}");
        assert!(output.contains("task 1 · waiting"), "{output}");

        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent { agent_id, .. },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1"
        ));
    }

    #[test]
    fn paused_agent_is_not_reported_as_waiting() {
        let mut rs = rows(2);
        rs[1].state = AgentRunState::confirmed_server(AgentRunStatus::Paused);

        let output = render(&InFlightAgentsView::new(rs), 90, 5);

        assert!(output.contains("1 working"), "{output}");
        assert!(output.contains("1 paused"), "{output}");
        assert!(!output.contains("1 waiting"), "{output}");
        assert!(output.contains("task 1 · paused"), "{output}");
    }

    #[test]
    fn x_can_be_invoked_repeatedly_in_the_same_session() {
        // Multiple stops in one Ctrl+G session.
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Char('x'))); // selected=0
        let p1 = v.take_action_request();
        assert!(matches!(
            p1,
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent { agent_id, .. },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-0"
        ));
        assert!(!v.is_complete());

        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char('x'))); // selected=1
        let p2 = v.take_action_request();
        assert!(matches!(
            p2,
            Some(ViewActionRequest {
                action: BottomPaneViewAction::ControlAgent { agent_id, .. },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1"
        ));
        assert!(!v.is_complete());
    }

    #[test]
    fn esc_closes_view_after_stop() {
        // After a stop, Esc still cleanly dismisses the view.
        let mut v = InFlightAgentsView::new(rows(2));
        v.handle_key(key(KeyCode::Char('x')));
        let _ = v.take_action_request();
        v.handle_key(key(KeyCode::Esc));
        assert!(v.is_complete());
        assert!(v.completion().is_none());
    }

    #[test]
    fn enter_after_stop_opens_selected_row() {
        // Enter after stop must still open detail view.
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Char('x')));
        let _ = v.take_action_request();
        v.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            v.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent { agent_id, .. },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1"
        ));
    }

    /// Ctrl+C dismisses the view without producing a selection.
    #[test]
    fn ctrl_c_dismisses() {
        let mut v = InFlightAgentsView::new(rows(2));
        let ev = v.on_ctrl_c();
        assert!(matches!(ev, CancellationEvent::Consumed));
        assert!(v.is_complete());
        // Dismissed: no result emitted.
        assert!(v.completion().is_none());
    }

    /// hjkl vim-style nav also works (alias for arrow keys).
    #[test]
    fn vim_keys_navigate() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Char('j')));
        assert_eq!(v.selected, 1);
        v.handle_key(key(KeyCode::Char('k')));
        assert_eq!(v.selected, 0);
    }

    #[test]
    fn paging_and_number_jump_navigate_long_agent_lists() {
        let mut v = InFlightAgentsView::new(rows(12));
        v.handle_key(key(KeyCode::PageDown));
        assert_eq!(v.selected, 8);
        v.handle_key(key(KeyCode::PageDown));
        assert_eq!(v.selected, 11);
        v.handle_key(key(KeyCode::PageUp));
        assert_eq!(v.selected, 3);
        v.handle_key(key(KeyCode::Char('7')));
        assert_eq!(v.selected, 6);
        v.handle_key(key(KeyCode::Char('9')));
        assert_eq!(v.selected, 8);
    }

    #[test]
    fn refresh_agent_monitor_recomputes_counts_and_preserves_selection() {
        let mut v = InFlightAgentsView::new(rows(3));
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.rows[v.selected].agent_id, "agent-1");

        let mut updated = rows(3);
        updated[0].state = AgentRunState::observed(AgentRunStatus::Completed);
        updated[1].state = AgentRunState::observed(AgentRunStatus::Failed);
        updated[2].state = AgentRunState::observed(AgentRunStatus::Cancelled);
        assert!(v.refresh_agent_monitor(updated.into()));

        assert_eq!(v.rows[v.selected].agent_id, "agent-1");
        assert_eq!(v.live_count, 0);
        assert_eq!(v.failed_count, 1);
    }

    #[test]
    fn render_groups_fanout_rows_under_header() {
        let mut rows = rows(3);
        rows[0].fanout = Some(fanout("review-1", 3, 0));
        rows[1].fanout = Some(fanout("review-1", 3, 1));
        rows[2].fanout = Some(fanout("review-1", 3, 2));
        rows[1].state = AgentRunState::observed(AgentRunStatus::Failed);
        rows[2].state = AgentRunState::observed(AgentRunStatus::Completed);

        let out = render(&InFlightAgentsView::new(rows), 100, 7);

        assert!(out.contains("review fanout"), "{out}");
        assert!(out.contains("3 target"), "{out}");
        assert!(out.contains("1 running"), "{out}");
        assert!(out.contains("1 done"), "{out}");
        assert!(out.contains("1 failed"), "{out}");
        assert!(out.contains("1. slot 1: slot task 0"), "{out}");
        assert!(out.contains("2. slot 2: slot task 1"), "{out}");
        assert!(out.contains("3. slot 3: slot task 2"), "{out}");
        assert!(out.contains("agent-0"), "{out}");
    }

    #[test]
    fn render_preserves_hierarchy_provenance_and_typed_activity() {
        let mut hierarchy = rows(2);
        hierarchy[0].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        hierarchy[0].provenance = AgentProjectionSource::DurableServer;
        hierarchy[0].activity = AgentActivityCounts {
            tool_calls: 3,
            child_agents: 1,
            messages_sent: 2,
            messages_received: 1,
            child_agents_partial: false,
        };
        hierarchy[0].runtime = astra_thin_client::SessionRunRuntimeFacts {
            runtime_profile: Some("agent_binding_registry".into()),
            model_name: Some("gpt-5".into()),
            agent_binding_name: Some("Reviewer".into()),
            ..Default::default()
        };
        let parent_run_id = hierarchy[0].run_id.clone();
        hierarchy[1].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        hierarchy[1].provenance = AgentProjectionSource::DurableServer;
        hierarchy[1].parent_run_id = parent_run_id;
        hierarchy[1].depth = 2;

        let out = render(&InFlightAgentsView::new(hierarchy), 100, 6);

        assert!(out.contains("3 tools"), "{out}");
        assert!(out.contains("1 child"), "{out}");
        assert!(out.contains("2 sent"), "{out}");
        assert!(out.contains("1 received"), "{out}");
        assert!(out.contains("server record"), "{out}");
        assert!(out.contains("agent binding registry"), "{out}");
        assert!(out.contains("gpt-5"), "{out}");
        assert!(out.contains("binding Reviewer"), "{out}");
        assert!(!out.contains("not reported"), "{out}");
        assert!(out.contains("↳ 2. task 1"), "{out}");
        assert!(!out.contains("steps"), "{out}");
    }

    #[test]
    fn selected_row_uses_availability_facts_not_missing_metadata_placeholders() {
        let out = render(&InFlightAgentsView::new(rows(1)), 100, 5);

        assert!(out.contains("run run-0"), "{out}");
        assert!(out.contains("stop available (X)"), "{out}");
        assert!(!out.contains("not reported"), "{out}");
        assert!(!out.contains("model not reported"), "{out}");
    }

    #[test]
    fn selected_runtime_detail_lists_only_the_selected_runs_actionable_controls() {
        let mut row = rows(1).remove(0);
        row.state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        row.control_target = Some(AgentControlTarget::DurableRun {
            run_id: "durable-run-1".into(),
        });
        row.available_actions = vec![
            astra_thin_client::SessionRunAction::Pause,
            astra_thin_client::SessionRunAction::Cancel,
        ];

        let detail = selected_runtime_detail(&row).join("\n");

        assert!(detail.contains("pause available (P)"), "{detail}");
        assert!(detail.contains("stop available (X)"), "{detail}");
        assert!(detail.contains("guide available (G)"), "{detail}");
        assert!(!detail.contains("resume available (P)"), "{detail}");
    }

    #[test]
    fn truncated_durable_snapshot_discloses_partial_collection_and_child_count() {
        let mut partial = rows(1);
        partial[0].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        partial[0].provenance = AgentProjectionSource::DurableServer;
        partial[0].activity = AgentActivityCounts {
            tool_calls: 2,
            child_agents: 1,
            messages_sent: 0,
            messages_received: 0,
            child_agents_partial: true,
        };
        let snapshot = AgentMonitorSnapshot {
            rows: partial,
            show_root_conversation: false,
            server_truth_state: ServerAgentTruthState::Confirmed,
            durable_snapshot_truncated: true,
        };

        let out = render(&InFlightAgentsView::new(snapshot), 100, 3);

        assert!(out.contains("partial server list"), "{out}");
        assert!(out.contains("≥1 child"), "{out}");
    }

    #[test]
    fn empty_server_lane_distinguishes_loading_unavailable_and_confirmed_empty() {
        let loading = AgentMonitorSnapshot {
            server_truth_state: ServerAgentTruthState::Loading,
            ..AgentMonitorSnapshot::default()
        };
        assert!(loading.should_open());
        let loading = render(&InFlightAgentsView::new(loading), 80, 4);
        assert!(loading.contains("server loading"), "{loading}");
        assert!(loading.contains("Loading durable agent state"), "{loading}");

        let unavailable = AgentMonitorSnapshot {
            server_truth_state: ServerAgentTruthState::Unavailable,
            ..AgentMonitorSnapshot::default()
        };
        assert!(unavailable.should_open());
        let unavailable = render(&InFlightAgentsView::new(unavailable), 80, 4);
        assert!(unavailable.contains("server unavailable"), "{unavailable}");
        assert!(
            unavailable.contains("unavailable · R refresh"),
            "{unavailable}"
        );

        let confirmed_empty = AgentMonitorSnapshot {
            server_truth_state: ServerAgentTruthState::Confirmed,
            ..AgentMonitorSnapshot::default()
        };
        assert!(!confirmed_empty.should_open());
    }

    #[test]
    fn durable_agent_monitor_exposes_typed_manual_refresh_when_observation_is_degraded() {
        let snapshot = AgentMonitorSnapshot {
            server_truth_state: ServerAgentTruthState::Unavailable,
            ..AgentMonitorSnapshot::default()
        };
        let mut view = InFlightAgentsView::new(snapshot);
        assert!(
            view.hint_keys()
                .expect("agent monitor hints")
                .contains("R refresh")
        );

        view.handle_key(key(KeyCode::Char('r')));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::RefreshAgentMonitor,
                disposition: ViewActionDisposition::KeepOpen,
            })
        ));
    }

    #[test]
    fn fanout_group_header_is_not_selectable() {
        let mut rows = rows(2);
        rows[0].fanout = Some(fanout("review-1", 2, 0));
        rows[1].fanout = Some(fanout("review-1", 2, 1));

        let mut v = InFlightAgentsView::new(rows);
        let out = render(&v, 100, 4);
        assert!(out.contains("review fanout"), "{out}");
        assert!(out.contains("› 1. slot 1"), "{out}");

        v.handle_key(key(KeyCode::Down));
        v.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            v.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent { agent_id, .. },
                disposition: ViewActionDisposition::KeepOpen,
            }) if agent_id == "agent-1"
        ));
    }

    #[test]
    fn enter_before_live_identity_opens_a_pending_conversation_and_later_binds() {
        let mut pending = rows(1);
        pending[0].run_id = None;
        pending[0].control_target = None;
        let mut view = InFlightAgentsView::new(pending);

        view.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent { run_id: None, .. },
                disposition: ViewActionDisposition::KeepOpen,
            })
        ));
        assert!(!view.is_complete());
        let pending_render = render(&view, 100, 5);
        assert!(
            pending_render.contains("run identity unavailable"),
            "{pending_render}"
        );
        assert!(
            pending_render.contains("live transcript opens now"),
            "{pending_render}"
        );

        assert!(view.refresh_agent_monitor(rows(1).into()));
        view.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            view.take_action_request(),
            Some(ViewActionRequest {
                action: BottomPaneViewAction::InspectAgent { run_id: Some(run_id), .. },
                disposition: ViewActionDisposition::KeepOpen,
            }) if run_id == "run-0"
        ));
    }

    #[test]
    fn refresh_agent_monitor_preserves_selection_for_grouped_rows() {
        let mut initial_rows = rows(3);
        initial_rows[0].fanout = Some(fanout("review-1", 3, 0));
        initial_rows[1].fanout = Some(fanout("review-1", 3, 1));
        initial_rows[2].fanout = Some(fanout("review-1", 3, 2));
        let mut v = InFlightAgentsView::new(initial_rows);
        v.handle_key(key(KeyCode::Down));
        assert_eq!(v.rows[v.selected].agent_id, "agent-1");

        let mut updated = rows(3);
        updated[0].fanout = Some(fanout("review-1", 3, 0));
        updated[1].fanout = Some(fanout("review-1", 3, 1));
        updated[2].fanout = Some(fanout("review-1", 3, 2));
        updated[1].state = AgentRunState::observed(AgentRunStatus::Completed);
        assert!(v.refresh_agent_monitor(updated.into()));

        assert_eq!(v.rows[v.selected].agent_id, "agent-1");
        let out = render(&v, 100, 5);
        assert!(out.contains("1 done"), "{out}");
        assert!(out.contains("› 2. slot 2"), "{out}");
    }

    /// Responsive truncation is display-width aware and multi-byte safe.
    #[test]
    fn truncate_handles_cjk() {
        let s = "日本語のとても長いタスク説明".repeat(3);
        let result = truncate_to_width(&s, 10);
        assert!(UnicodeWidthStr::width(result.as_str()) <= 10);
        assert!(result.ends_with('…'));
    }

    #[test]
    fn narrow_layout_keeps_identity_and_essential_runtime_facts() {
        let mut narrow = rows(1);
        narrow[0].name = "审查一个非常长的异步代理任务名称".into();
        narrow[0].state = AgentRunState::confirmed_server(AgentRunStatus::Running);
        narrow[0].provenance = AgentProjectionSource::DurableServer;
        narrow[0].activity = AgentActivityCounts {
            tool_calls: 128,
            child_agents: 12,
            messages_sent: 0,
            messages_received: 0,
            child_agents_partial: false,
        };

        let out = render(&InFlightAgentsView::new(narrow), 40, 3);

        assert!(out.contains("› 1."), "{out}");
        assert!(out.contains("server record"), "{out}");
        assert!(out.contains("1.0s"), "{out}");
        assert!(out.contains('…'), "{out}");
    }

    #[test]
    fn render_uses_calmer_agents_header_and_hint_copy() {
        let v = InFlightAgentsView::new(rows(2));
        let out = render(&v, 80, 4);
        assert!(out.contains("Agent runs · 2 working"), "{out}");
        assert!(!out.contains("SUBAGENTS"), "{out}");
        assert!(out.contains("1 tool"), "{out}");
        assert!(!out.contains("steps"), "{out}");
        assert_eq!(
            v.hint_keys().as_deref(),
            Some("↑↓ move · Enter/→ transcript · X stop · G guide · ←/Esc close")
        );
    }

    #[test]
    fn render_uses_meta_words_for_terminal_and_cancelling_rows() {
        let mut rows = rows(4);
        rows[0].state = AgentRunState::local_intent(AgentRunStatus::Cancelling);
        rows[1].state = AgentRunState::observed(AgentRunStatus::Completed);
        rows[2].state = AgentRunState::observed(AgentRunStatus::Failed);
        rows[3].state = AgentRunState::observed(AgentRunStatus::Cancelled);
        let out = render(&InFlightAgentsView::new(rows), 80, 6);
        assert!(out.contains("stopping"), "{out}");
        assert!(out.contains("done"), "{out}");
        assert!(out.contains("failed"), "{out}");
        assert!(out.contains("stopped"), "{out}");
    }
}
