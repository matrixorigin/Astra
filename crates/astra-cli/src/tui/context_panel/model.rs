//! Pure data model for the `/context` panel.
//!
//! Sourced from the most recent [`ContextAssemblyTrace`] captured by
//! the observability session. The shape is a grid + category
//! legend + nested sections, driven by what astra's trace actually
//! carries: token-budget breakdown, tool / memory / skill /
//! system-prompt sub-rows, and an explicit "free space" category
//! derived from `max_tokens - total_used`.
//!
//! This module has no render logic — it just produces a structured
//! snapshot the view can walk top-down. Keeping the model pure
//! makes the behaviour easy to unit-test without touching a Ratatui
//! buffer.

use std::collections::HashSet;

use crate::cli::session::session_state::ContinuationAnchor;
use astra_turn_core::context_assembly_trace::{
    ContextAssemblyTrace, DecisionExplanation, DecisionType, MemoryInjection, MemoryRejection,
    MemorySelection, RejectionReason, SkillInjection, VisibleTool,
};

use ratatui::style::Color;

/// The full breakdown the panel renders.  Aggregates the top-level
/// token budget, every category, and the nested sub-items (tools,
/// memories, skills, system-prompt sections) so the view can render
/// them as collapsible lists.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextBreakdown {
    pub total_used: u32,
    pub limit: u32,
    pub pressure: f64,
    pub categories: Vec<Category>,
    pub free_space_tokens: u32,
    /// Whether the last turn triggered context compression.
    pub compression_triggered: bool,
    pub tools: Vec<ToolItem>,
    pub memories: Vec<MemoryItem>,
    pub skills: Vec<SkillItem>,
    pub system_sections: Vec<SectionItem>,
    pub history: HistorySummary,
    pub memory_focus: MemoryFocus,
    pub prompt_signals: Vec<SignalItem>,
    pub session_summary: Option<SessionSummary>,
    pub decisions: Vec<DecisionItem>,
    pub compaction: CompactionSummary,
}

/// Compaction stats sourced from two places:
///   • `ContextAssemblyTrace.history` — last turn's compression
///     method + pre/post tokens + information_lost.
///   • `ObservabilitySession.compressed_turns` — every turn this
///     session that fired compaction, for the big-picture story.
/// Collapsed view shows aggregate counts; expansion walks per-event
/// detail; drill shows full `information_lost` + summary text.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct CompactionSummary {
    /// `true` when the most recent turn triggered compaction.
    pub triggered_this_turn: bool,
    /// All turns in the session that fired compaction.  Includes
    /// older events beyond just this turn.
    pub compressed_turns: Vec<u32>,
    /// Exact per-turn compaction detail, only when a producer recorded a
    /// real turn identity.
    pub events: Vec<CompactionEventItem>,
    /// Pipeline-level work that freed context without a meaningful individual
    /// turn identity (for example duplicate-read elimination).
    pub stages: Vec<CompactionStageItem>,
    /// Aggregate token shape (last turn only).
    pub tokens_before: u32,
    pub tokens_after: u32,
}

impl CompactionSummary {
    pub fn is_empty(&self) -> bool {
        !self.triggered_this_turn
            && self.compressed_turns.is_empty()
            && self.events.is_empty()
            && self.stages.is_empty()
    }

    pub fn tokens_saved(&self) -> u32 {
        self.tokens_before.saturating_sub(self.tokens_after)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompactionEventItem {
    pub turn_index: u32,
    pub role: String,
    pub method: String,
    pub original_tokens: u32,
    pub compressed_tokens: u32,
    /// Human-readable bullets describing what was lost.
    pub information_lost: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct CompactionStageItem {
    pub stage: String,
    pub method: String,
    pub tokens_freed: u32,
}

/// Extra detail for the Memory section.  Populated directly from
/// `MemoryRetrievalTrace` — query, candidates considered, rejection
/// list with reasons, retrieval latency. Direct prompt injections are
/// tracked separately from retrieval-selected memories so `/context`
/// can distinguish "retrieved this turn" from "injected anyway."
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MemoryFocus {
    pub query: String,
    pub candidates_considered: u32,
    pub retrieval_latency_ms: u64,
    pub rejected: Vec<MemoryRejectionItem>,
    pub repository_injected: Vec<InjectedMemoryItem>,
    pub session_injected: Option<InjectedMemoryItem>,
}

impl MemoryFocus {
    pub fn has_retrieval_activity(&self) -> bool {
        !self.query.is_empty()
            || self.candidates_considered > 0
            || self.retrieval_latency_ms > 0
            || !self.rejected.is_empty()
    }

    pub fn has_prompt_injections(&self) -> bool {
        !self.repository_injected.is_empty() || self.session_injected.is_some()
    }

    pub fn is_empty(&self) -> bool {
        !self.has_retrieval_activity() && !self.has_prompt_injections()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MemoryRejectionItem {
    pub memory_id: String,
    pub relevance: f64,
    /// Human-readable rendering of the rejection reason.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct InjectedMemoryItem {
    pub source_label: String,
    pub memory_id: String,
    pub memory_type: String,
    pub tokens: u32,
    pub relevance: f64,
    pub preview: String,
}

impl InjectedMemoryItem {
    pub(crate) const LABEL_REPOSITORY: &'static str = "Repository memory";
    pub(crate) const LABEL_SESSION: &'static str = "Session memory";
}

/// A single bit from `PromptContextSignals` or `PromptGuidanceSignals`
/// that was active this turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SignalItem {
    pub name: &'static str,
    pub description: &'static str,
    /// Distinguishes context signals (dynamic prompt sections) from
    /// guidance signals (late-round nudges) so the rendered
    /// section can sub-group them.
    pub kind: SignalKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SignalKind {
    Context,
    Guidance,
}

/// Session + budget summary built from [`ContextSnapshot::session`]
/// (which in turn wraps `SessionState` fields).  Rendered as a
/// dedicated section so users can see cost, token totals, and
/// sticky state that drives follow-up turns.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SessionSummary {
    pub session_id: String,
    pub turn: u32,
    pub model: Option<String>,
    pub total_cost: f64,
    pub max_budget: f64,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// One request's actual context-window occupancy. This is deliberately
    /// separate from cumulative session/billing totals above.
    pub request_context: Option<RequestContextEvidence>,
    pub continuation_anchor: Option<ContinuationAnchor>,
    pub queued_message: Option<String>,
    pub diagnostics_context: Option<String>,
    /// Session-wide `read_file` evidence from the canonical journal. This is
    /// intentionally separate from the latest prompt trace: a prompt can be
    /// small while the session has accumulated substantial tool activity.
    pub read_activity: ReadActivity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RequestContextEvidence {
    pub usage: astra_turn_types::ContextWindowUsage,
    pub scope: RequestContextScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RequestContextScope {
    CurrentRequest,
    PreviousRequestWhileAssembling,
    LastCompletedRequest,
}

impl RequestContextScope {
    pub fn label(self) -> &'static str {
        match self {
            Self::CurrentRequest => "current request",
            Self::PreviousRequestWhileAssembling => "previous request · assembling next",
            Self::LastCompletedRequest => "last completed request",
        }
    }
}

/// Availability and provenance of the session-level file-read evidence.
///
/// A prompt trace only describes one assembled prompt. Journal evidence is
/// useful for a longer lived question ("are we repeatedly reading the same
/// files?"), but it is not guaranteed to exist on every topology. In
/// particular a Server-only client must not be shown invented local facts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum ReadActivity {
    /// The workbench has scheduled the local durable read, but it has not
    /// completed yet. This state is only presentation progress; it is never
    /// interpreted as an absence of activity.
    #[default]
    Loading,
    /// Evidence was computed from append-ordered local journal events.
    Available(SessionReadActivity),
    /// No local durable source was available. The reason is deliberately
    /// surfaced rather than silently rendering zero reads.
    Unavailable(String),
}

/// Auditable summary of `read_file` activity across a durable session.
///
/// `exact_repeat_requests` only counts calls whose full structured arguments
/// were recorded. `repeats_after_recorded_compaction` is a temporal
/// correlation, not a causal conclusion: compaction may be one reason for a
/// reread, but source changes and a different task are also possible.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SessionReadActivity {
    pub requested: u32,
    pub executed: u32,
    pub reused_or_suppressed: u32,
    pub other_not_executed: u32,
    pub distinct_files: u32,
    pub requests_with_exact_identity: u32,
    pub exact_repeat_requests: u32,
    pub repeats_after_recorded_compaction: u32,
}

impl SessionReadActivity {
    pub fn has_activity(&self) -> bool {
        self.requested > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ReadRequestIdentity {
    path: String,
    start_line: Option<u64>,
    end_line: Option<u64>,
    outline: bool,
}

impl ReadRequestIdentity {
    fn from_record(record: &astra_services::session_journal::ToolCallRecord) -> Option<Self> {
        let args = record.args_full.as_deref()?;
        let args: serde_json::Value = serde_json::from_str(args).ok()?;
        let path = args.get("path")?.as_str()?.trim();
        if path.is_empty() {
            return None;
        }
        Some(Self {
            path: path.to_string(),
            start_line: args.get("start_line").and_then(serde_json::Value::as_u64),
            end_line: args.get("end_line").and_then(serde_json::Value::as_u64),
            outline: args
                .get("outline")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// Summarize canonical, append-ordered journal evidence without inspecting
/// rendered tool text. The result distinguishes executions from reused or
/// suppressed calls, so a high request count is never misreported as disk I/O.
pub(crate) fn summarize_session_read_activity(
    events: &[astra_services::session_journal::JournalEvent],
) -> SessionReadActivity {
    use astra_services::session_journal::ToolCallDisposition;

    let mut summary = SessionReadActivity::default();
    let mut paths = HashSet::new();
    let mut seen_requests = HashSet::new();
    let mut compaction_recorded = false;

    for event in events {
        if event
            .context_assembly_trace
            .as_ref()
            .and_then(|trace| trace.get("token_budget"))
            .and_then(|budget| budget.get("compression_triggered"))
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            compaction_recorded = true;
        }

        let Some(records) = event.tool_calls.as_ref() else {
            continue;
        };
        for record in records.iter().filter(|record| record.name == "read_file") {
            summary.requested = summary.requested.saturating_add(1);
            match record.effective_disposition() {
                ToolCallDisposition::Executed => {
                    summary.executed = summary.executed.saturating_add(1)
                }
                ToolCallDisposition::Reused | ToolCallDisposition::Suppressed => {
                    summary.reused_or_suppressed = summary.reused_or_suppressed.saturating_add(1)
                }
                ToolCallDisposition::Rejected | ToolCallDisposition::Deferred => {
                    summary.other_not_executed = summary.other_not_executed.saturating_add(1)
                }
            }

            if let Some(path) = record
                .file_path
                .as_deref()
                .filter(|path| !path.trim().is_empty())
            {
                paths.insert(path.to_string());
            }
            let Some(identity) = ReadRequestIdentity::from_record(record) else {
                continue;
            };
            paths.insert(identity.path.clone());
            summary.requests_with_exact_identity =
                summary.requests_with_exact_identity.saturating_add(1);
            if !seen_requests.insert(identity) {
                summary.exact_repeat_requests = summary.exact_repeat_requests.saturating_add(1);
                if compaction_recorded {
                    summary.repeats_after_recorded_compaction =
                        summary.repeats_after_recorded_compaction.saturating_add(1);
                }
            }
        }
    }

    summary.distinct_files = paths.len().try_into().unwrap_or(u32::MAX);
    summary
}

/// One decision lifted from `ContextAssemblyTrace::explanations`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DecisionItem {
    pub label: String,
    pub reasoning: String,
    pub confidence: f64,
    pub alternatives: Vec<AlternativeItem>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AlternativeItem {
    pub description: String,
    pub score: f64,
    pub why_not_chosen: String,
}

/// Aggregate summary of the history slice the context carried into
/// the most recent turn.  The view renders this as a labelled
/// section so the user can see how aggressively the compactor is
/// trimming their backlog. `turns` carries per-turn details that
/// the expanded view surfaces.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct HistorySummary {
    pub total_turns: u32,
    pub retained: u32,
    pub compressed: u32,
    pub dropped: u32,
    pub tokens_before: u32,
    pub tokens_after: u32,
    pub compression_ratio: f64,
    pub turns: Vec<TurnDetail>,
    pub dropped_indices: Vec<u32>,
    /// Whether item text came from the exact prompt-history trace or is a
    /// local visible-conversation fallback. The UI must never present the
    /// latter as proof of what entered the model prompt.
    pub evidence_source: HistoryEvidenceSource,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum HistoryEvidenceSource {
    #[default]
    PromptTrace,
    LocalVisibleConversation,
}

impl HistorySummary {
    pub fn is_empty(&self) -> bool {
        self.total_turns == 0
            && self.retained == 0
            && self.compressed == 0
            && self.dropped == 0
            && self.tokens_before == 0
    }
}

/// Categorical share of the context window. Category ordering drives
/// both the left-side grid filling and the right-side legend order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CategoryKind {
    System,
    Tools,
    Memory,
    History,
    UserMessage,
}

impl CategoryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::System => "System prompt",
            Self::Tools => "Tools",
            Self::Memory => "Memory",
            Self::History => "History",
            Self::UserMessage => "Current turn",
        }
    }

    pub fn color(self) -> Color {
        let theme = crate::tui::theme::current();
        match self {
            Self::System => theme.accent,
            Self::Tools => theme.command,
            Self::Memory => theme.quote,
            Self::History => theme.warn,
            Self::UserMessage => theme.success,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Category {
    pub kind: CategoryKind,
    pub tokens: u32,
    /// `tokens / limit` as a percentage; 0.0 when limit is 0.
    pub pct_of_limit: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ToolItem {
    pub name: String,
    pub tokens: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MemoryItem {
    pub preview: String,
    pub tokens: u32,
    pub relevance: f64,
    pub memory_type: String,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SkillItem {
    pub name: String,
    pub tokens: u32,
    /// Optional one-line description (populated only when the
    /// model data source carries it, e.g. injected skill metadata).
    pub description: Option<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TurnDetail {
    pub index: u32,
    pub role: String,
    pub tokens: u32,
    pub has_tool_calls: bool,
    /// `None` when this turn was retained; `Some(method)` when
    /// compressed with that method.
    pub compressed_from: Option<(u32, String)>,
    /// Short content preview taken from the turn body (first
    /// non-blank line). Empty when the caller didn't attach
    /// transcript text.
    pub preview: String,
    /// Full turn body — used when the user drills into this turn.
    /// Empty when the caller didn't attach transcript text.
    pub body: String,
}

/// A labelled sub-section of the system prompt (e.g. "Environment",
/// "Guidance signals").  We keep the structure flat — the trace
/// doesn't currently surface named sections, so most deployments
/// will see just the aggregated system total.  `preview` is
/// synthesized locally at /context-build time (e.g. cwd + git
/// branch for Environment) — populated when the caller passes
/// [`ContextSnapshot`] with env details.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SectionItem {
    pub name: String,
    pub tokens: u32,
    pub preview: Option<String>,
}

/// Which nested section the user currently has focused. Drives
/// heading highlight + `Enter to expand` hint visibility. Cycles
/// via Tab / Shift+Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    Session,
    SystemPrompt,
    PromptSignals,
    Tools,
    Skills,
    Memory,
    History,
    Compaction,
    Decisions,
}

impl Section {
    pub fn all() -> &'static [Section] {
        &[
            Section::Session,
            Section::SystemPrompt,
            Section::PromptSignals,
            Section::Tools,
            Section::Skills,
            Section::Memory,
            Section::History,
            Section::Compaction,
            Section::Decisions,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::Session => "Session · budget & state",
            Section::SystemPrompt => "System prompt",
            Section::PromptSignals => "Prompt signals",
            Section::Tools => "Tools · /tool",
            Section::Skills => "Skills · /skills",
            Section::Memory => "Memory · /memory",
            Section::History => "History · conversation turns",
            Section::Compaction => "Compaction · context trimming",
            Section::Decisions => "Decisions · why did it pick this",
        }
    }

    /// Next section in cycle order. Wraps.
    pub fn next(self) -> Section {
        let all = Self::all();
        let idx = all.iter().position(|s| *s == self).unwrap_or(0);
        all[(idx + 1) % all.len()]
    }

    /// Previous section in cycle order. Wraps.
    pub fn prev(self) -> Section {
        let all = Self::all();
        let idx = all.iter().position(|s| *s == self).unwrap_or(0);
        all[(idx + all.len() - 1) % all.len()]
    }
}

/// Does a given [`Section`] have any data in this breakdown? Used
/// by focus-cycling to skip sections with nothing to show.
impl ContextBreakdown {
    pub fn section_non_empty(&self, s: Section) -> bool {
        match s {
            Section::Session => self.session_summary.is_some(),
            Section::SystemPrompt => !self.system_sections.is_empty(),
            Section::PromptSignals => !self.prompt_signals.is_empty(),
            Section::Tools => !self.tools.is_empty(),
            Section::Skills => !self.skills.is_empty(),
            Section::Memory => !self.memories.is_empty() || !self.memory_focus.is_empty(),
            Section::History => !self.history.is_empty(),
            Section::Compaction => !self.compaction.is_empty(),
            Section::Decisions => !self.decisions.is_empty(),
        }
    }

    /// First section that has content, or None if nothing to drill
    /// into.
    pub fn first_focusable_section(&self) -> Option<Section> {
        Section::all()
            .iter()
            .copied()
            .find(|s| self.section_non_empty(*s))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PressureBand {
    Low,      // <60%
    Warning,  // 60-85%
    Critical, // >=85%
}

impl PressureBand {
    pub fn color(self) -> Color {
        let theme = crate::tui::theme::current();
        match self {
            Self::Low => theme.success,
            Self::Warning => theme.warn,
            Self::Critical => theme.error,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Warning => "warn",
            Self::Critical => "high",
        }
    }
}

/// Auxiliary data the caller supplies alongside a trace. The
/// trace carries its own exact prompt-history previews; this snapshot carries
/// local evidence that the trace cannot own (visible conversation when no
/// trace exists, cwd + git branch for Environment, etc). All fields are
/// optional so callers can opt in incrementally.
#[derive(Debug, Default, Clone)]
pub(crate) struct ContextSnapshot<'a> {
    /// Local conversation visible in the current TUI session. This is a
    /// fallback when an exact prompt-composition trace is unavailable; its
    /// ordinal intentionally is not treated as a trace turn identity.
    pub visible_conversation: Vec<VisibleConversationItem>,
    /// Current model identifier (for the System-prompt Persona row).
    pub model: Option<&'a str>,
    /// cwd rendered as a display string, e.g. `~/github/astra`.
    pub cwd: Option<String>,
    /// Current git branch, e.g. `improve_tui3`.
    pub git_branch: Option<String>,
    /// Path to the user-rules file backing the User-preferences
    /// section (e.g. `~/.astra/rules/…`).
    pub user_rules_path: Option<String>,
    /// Session + budget state the trace doesn't carry. Populated
    /// by the `/context` dispatch from `SessionState`.
    pub session: Option<SessionSummary>,
    /// User-activated system skills (from `/skill` or auto-detect).
    /// These feed the prompt via `edge_profile.active_skills` but
    /// the trace may not capture them in `skills_injected` when
    /// the system-prompt breakdown wasn't recorded this turn.
    /// Surfaced as a Skills-section fallback so users always see
    /// what skills are loaded.  Read-only display — no prompt
    /// cache impact.
    pub active_skills: Vec<ActiveSkill>,
    /// Skill names actually chosen in the last completed turn.
    /// Used as a more accurate fallback than `active_skills` when
    /// the trace omitted per-skill injection details.
    pub selected_skills: Vec<String>,
    /// Every turn in this session that fired compaction.  Sourced
    /// from `ObservabilitySession.compressed_turns` — the current
    /// trace only knows about the LAST turn's compaction events,
    /// so this list is what makes the Compaction section show a
    /// session-level timeline.
    pub compressed_turns: Vec<u32>,
}

/// One local, user-visible conversation item. Stored independently from the
/// prompt trace so callers cannot accidentally pair two unrelated sequences
/// by ordinal position.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct VisibleConversationItem {
    pub role: String,
    pub preview: String,
    pub body: String,
}

/// One loaded system skill surfaced by the snapshot.  Decoupled
/// from `astra-prompts::SystemSkill` so the context-panel module
/// doesn't need to import that crate.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ActiveSkill {
    pub name: String,
    pub description: String,
}

impl ContextBreakdown {
    /// Empty breakdown used when no trace is available.
    pub fn empty() -> Self {
        Self {
            total_used: 0,
            limit: 0,
            pressure: 0.0,
            categories: Vec::new(),
            free_space_tokens: 0,
            compression_triggered: false,
            tools: Vec::new(),
            memories: Vec::new(),
            skills: Vec::new(),
            system_sections: Vec::new(),
            history: HistorySummary::default(),
            memory_focus: MemoryFocus::default(),
            prompt_signals: Vec::new(),
            session_summary: None,
            decisions: Vec::new(),
            compaction: CompactionSummary::default(),
        }
    }

    /// Build an honest partial view when the exact prompt-composition trace
    /// has not arrived yet. `/context` must still show session, visible
    /// conversation, loaded skills and compaction facts that the client
    /// already owns; it must not fabricate per-category token counts.
    pub fn from_snapshot_without_trace(snap: &ContextSnapshot<'_>) -> Self {
        let turns = snap
            .visible_conversation
            .iter()
            .enumerate()
            .map(|(index, item)| TurnDetail {
                index: index as u32,
                role: item.role.clone(),
                tokens: 0,
                has_tool_calls: false,
                compressed_from: None,
                preview: item.preview.clone(),
                body: item.body.clone(),
            })
            .collect::<Vec<_>>();
        let retained = turns.len() as u32;
        let skills = if !snap.selected_skills.is_empty() {
            snap.selected_skills
                .iter()
                .map(|name| SkillItem {
                    name: name.clone(),
                    tokens: 0,
                    description: None,
                    source: Some("selected".to_string()),
                })
                .collect()
        } else {
            snap.active_skills
                .iter()
                .map(|skill| SkillItem {
                    name: skill.name.clone(),
                    tokens: 0,
                    description: (!skill.description.is_empty()).then(|| skill.description.clone()),
                    source: Some("loaded".to_string()),
                })
                .collect()
        };

        Self {
            total_used: 0,
            limit: 0,
            pressure: 0.0,
            categories: Vec::new(),
            free_space_tokens: 0,
            compression_triggered: false,
            tools: Vec::new(),
            memories: Vec::new(),
            skills,
            system_sections: Vec::new(),
            history: HistorySummary {
                total_turns: retained,
                retained,
                compressed: 0,
                dropped: 0,
                tokens_before: 0,
                tokens_after: 0,
                compression_ratio: 0.0,
                turns,
                dropped_indices: Vec::new(),
                evidence_source: HistoryEvidenceSource::LocalVisibleConversation,
            },
            memory_focus: MemoryFocus::default(),
            prompt_signals: Vec::new(),
            session_summary: snap.session.clone(),
            decisions: Vec::new(),
            compaction: CompactionSummary {
                compressed_turns: snap.compressed_turns.clone(),
                ..Default::default()
            },
        }
    }

    /// Whether this panel has any state worth rendering. Exact token layout
    /// and locally-known session evidence are distinct, both useful states.
    pub fn has_observable_data(&self) -> bool {
        self.limit > 0 || !self.categories.is_empty() || self.first_focusable_section().is_some()
    }

    /// Build from the most recent full [`ContextAssemblyTrace`].
    /// Equivalent to [`from_trace_with`] with an empty snapshot —
    /// no content previews, just the counts the trace carries.
    pub fn from_trace(trace: &ContextAssemblyTrace) -> Self {
        Self::from_trace_with(trace, &ContextSnapshot::default())
    }

    /// Build from a trace plus auxiliary local evidence. The trace's history
    /// preview remains authoritative for prompt contents; the snapshot adds
    /// environment, skill and session facts without attempting an ordinal
    /// join to local transcript cells.
    pub fn from_trace_with(trace: &ContextAssemblyTrace, snap: &ContextSnapshot<'_>) -> Self {
        let budget = &trace.token_budget;
        let limit = budget.max_tokens;
        let pct = |tokens: u32| -> f64 {
            if limit == 0 {
                0.0
            } else {
                tokens as f64 / limit as f64 * 100.0
            }
        };
        let raw = [
            (CategoryKind::System, budget.system_prompt_tokens),
            (CategoryKind::Tools, budget.tool_schema_tokens),
            (CategoryKind::Memory, budget.memory_tokens),
            (CategoryKind::History, budget.history_tokens),
            (CategoryKind::UserMessage, budget.user_message_tokens),
        ];
        let categories: Vec<Category> = raw
            .into_iter()
            .filter(|(_, t)| *t > 0)
            .map(|(kind, tokens)| Category {
                kind,
                tokens,
                pct_of_limit: pct(tokens),
            })
            .collect();

        let free_space_tokens = limit.saturating_sub(budget.total_used);

        // Tools: keep the original visible-surface order. Zero-token entries
        // are filtered so a partial trace doesn't fill the panel with noise.
        let tools: Vec<ToolItem> = trace
            .tools
            .visible_tools
            .iter()
            .filter(|t: &&VisibleTool| t.tokens > 0)
            .map(|t| ToolItem {
                name: t.tool_name.clone(),
                tokens: t.tokens,
            })
            .collect();

        // Memories: sort by tokens desc so the biggest contributors
        // appear at the top — users scan for what's eating the
        // budget first.  Content preview is truncated to ~80 chars
        // by the trace builder; we just pass it through.
        let mut memories: Vec<MemoryItem> = trace
            .memory
            .memories_selected
            .iter()
            .filter(|m: &&MemorySelection| m.tokens > 0)
            .map(|m| MemoryItem {
                preview: m.content_preview.clone(),
                tokens: m.tokens,
                relevance: m.relevance_score,
                memory_type: m.memory_type.clone(),
                source: format!("{:?}", m.source),
            })
            .collect();
        memories.sort_by_key(|m| std::cmp::Reverse(m.tokens));

        // Skills: prefer the rich `skills_injected` list — it has
        // per-skill token counts. When the runtime only records names
        // without per-skill cost, fall back to those names with
        // tokens=0 so the section still renders something useful.
        let mut skills: Vec<SkillItem> = trace
            .system_prompt
            .skills_injected
            .iter()
            .filter(|s: &&SkillInjection| s.tokens > 0)
            .map(|s| SkillItem {
                name: s.skill_name.clone(),
                tokens: s.tokens,
                description: None,
                source: None,
            })
            .collect();
        skills.sort_by_key(|s| std::cmp::Reverse(s.tokens));
        // Fallback #2: last completed turn recorded which skills were
        // actually chosen, even if per-skill prompt injection details
        // were omitted from the trace.
        if skills.is_empty() && !snap.selected_skills.is_empty() {
            skills = snap
                .selected_skills
                .iter()
                .map(|name| SkillItem {
                    name: name.clone(),
                    tokens: 0,
                    description: None,
                    source: Some("selected".to_string()),
                })
                .collect();
        }
        // Last-resort fallback: the trace is silent but the CLI
        // state knows which system skills are currently loaded
        // via `/skill` (or auto-detect).  Surface them so users
        // see *some* signal about what skills shape their turn.
        // Tokens=0 because the per-skill cost lives inside the
        // system-prompt total, not in a dedicated line item.
        if skills.is_empty() && !snap.active_skills.is_empty() {
            skills = snap
                .active_skills
                .iter()
                .map(|s| SkillItem {
                    name: s.name.clone(),
                    tokens: 0,
                    description: if s.description.is_empty() {
                        None
                    } else {
                        Some(s.description.clone())
                    },
                    source: Some("loaded".to_string()),
                })
                .collect();
        }

        // System-prompt sub-rows: the trace doesn't currently split
        // the system prompt into named sections, so we synthesize a
        // coarse split from the known scalar fields.  Zero-token
        // rows are dropped.  Per-row previews come from the caller's
        // snapshot — cwd + git branch for Environment, model name
        // for Persona, user-rules path for User preferences.
        let system_sections = build_system_sections(trace, snap);

        // History summary: counts + pre/post-compression token shape.
        // Rendered as a dedicated section so users can see how much
        // of their backlog survived the compactor this turn.
        let h = &trace.history;
        let mut turns: Vec<TurnDetail> =
            Vec::with_capacity(h.turns_retained.len() + h.turns_compressed.len());
        for r in &h.turns_retained {
            turns.push(TurnDetail {
                index: r.turn_index,
                role: r.role.clone(),
                tokens: r.tokens,
                has_tool_calls: r.has_tool_calls,
                compressed_from: None,
                preview: r.content_preview.clone(),
                body: String::new(),
            });
        }
        for c in &h.turns_compressed {
            turns.push(TurnDetail {
                index: c.turn_index,
                role: c.role.clone(),
                tokens: c.compressed_tokens,
                has_tool_calls: false,
                compressed_from: Some((c.original_tokens, format!("{:?}", c.compression_method))),
                preview: String::new(),
                body: String::new(),
            });
        }
        // Sort ascending by turn index so the expanded view reads
        // chronologically — matches how scrollback is ordered.
        turns.sort_by_key(|t| t.index);

        let history = HistorySummary {
            total_turns: h.total_turns_available,
            retained: h.turns_retained.len() as u32,
            compressed: h.turns_compressed.len() as u32,
            dropped: h.turns_dropped.len() as u32,
            tokens_before: h.tokens_before,
            tokens_after: h.tokens_after,
            compression_ratio: h.compression_ratio,
            turns,
            dropped_indices: h.turns_dropped.clone(),
            evidence_source: HistoryEvidenceSource::PromptTrace,
        };

        let memory_focus = build_memory_focus(trace);
        let prompt_signals = build_prompt_signals(trace);
        let decisions = build_decisions(trace);
        let session_summary = snap.session.clone();
        let compaction = build_compaction_summary(trace, snap);

        Self {
            total_used: budget.total_used,
            limit,
            pressure: budget.budget_pressure,
            categories,
            free_space_tokens,
            compression_triggered: budget.compression_triggered,
            tools,
            memories,
            skills,
            system_sections,
            history,
            memory_focus,
            prompt_signals,
            session_summary,
            decisions,
            compaction,
        }
    }

    pub fn band(&self) -> PressureBand {
        let p = self.usage_percent();
        if p >= 85.0 {
            PressureBand::Critical
        } else if p >= 60.0 {
            PressureBand::Warning
        } else {
            PressureBand::Low
        }
    }

    pub fn usage_percent(&self) -> f64 {
        if self.limit == 0 {
            0.0
        } else {
            self.total_used as f64 / self.limit as f64 * 100.0
        }
    }

    pub(crate) fn set_read_activity(&mut self, read_activity: ReadActivity) {
        if let Some(session) = self.session_summary.as_mut() {
            session.read_activity = read_activity;
        }
    }
}

fn build_compaction_summary(
    trace: &ContextAssemblyTrace,
    snap: &ContextSnapshot<'_>,
) -> CompactionSummary {
    let h = &trace.history;
    let events: Vec<CompactionEventItem> = h
        .turns_compressed
        .iter()
        .map(|c| CompactionEventItem {
            turn_index: c.turn_index,
            role: c.role.clone(),
            method: format!("{:?}", c.compression_method),
            original_tokens: c.original_tokens,
            compressed_tokens: c.compressed_tokens,
            information_lost: c.information_lost.clone(),
        })
        .collect();
    let stages = h
        .compression_stages
        .iter()
        .map(|stage| CompactionStageItem {
            stage: stage.stage.clone(),
            method: format!("{:?}", stage.method),
            tokens_freed: stage.tokens_freed,
        })
        .collect();
    CompactionSummary {
        triggered_this_turn: trace.token_budget.compression_triggered,
        compressed_turns: snap.compressed_turns.clone(),
        events,
        stages,
        tokens_before: h.tokens_before,
        tokens_after: h.tokens_after,
    }
}

fn build_memory_focus(trace: &ContextAssemblyTrace) -> MemoryFocus {
    let m = &trace.memory;
    let rejected: Vec<MemoryRejectionItem> = m
        .memories_rejected
        .iter()
        .map(|r: &MemoryRejection| MemoryRejectionItem {
            memory_id: r.memory_id.clone(),
            relevance: r.relevance_score,
            reason: render_rejection_reason(&r.rejection_reason),
        })
        .collect();
    let repository_injected: Vec<InjectedMemoryItem> = trace
        .system_prompt
        .repository_memories
        .iter()
        .filter(|mi: &&MemoryInjection| mi.tokens > 0)
        .map(|mi| InjectedMemoryItem {
            source_label: InjectedMemoryItem::LABEL_REPOSITORY.to_string(),
            memory_id: mi.memory_id.clone(),
            memory_type: mi.memory_type.clone(),
            tokens: mi.tokens,
            relevance: mi.relevance_score,
            preview: mi.content_preview.clone(),
        })
        .collect();
    let session_injected = trace
        .system_prompt
        .session_memory_injected
        .as_ref()
        .filter(|mi| mi.tokens > 0)
        .map(|mi| InjectedMemoryItem {
            source_label: InjectedMemoryItem::LABEL_SESSION.to_string(),
            memory_id: mi.memory_id.clone(),
            memory_type: mi.memory_type.clone(),
            tokens: mi.tokens,
            relevance: mi.relevance_score,
            preview: mi.content_preview.clone(),
        });
    MemoryFocus {
        query: m.query.clone(),
        candidates_considered: m.candidates_considered,
        retrieval_latency_ms: m.retrieval_latency_ms,
        rejected,
        repository_injected,
        session_injected,
    }
}

fn render_rejection_reason(r: &RejectionReason) -> String {
    match r {
        RejectionReason::BelowThreshold { threshold, score } => {
            format!("below threshold (score {score:.2} < {threshold:.2})")
        }
        RejectionReason::TokenBudgetExceeded {
            available,
            required,
        } => format!("token budget exceeded ({required} needed, {available} free)"),
        RejectionReason::Duplicate { of_memory_id } => {
            format!("duplicate of {of_memory_id}")
        }
        RejectionReason::Stale { age_days } => format!("stale ({age_days}d old)"),
    }
}

fn build_prompt_signals(trace: &ContextAssemblyTrace) -> Vec<SignalItem> {
    let cs = &trace.system_prompt.context_signals;
    let gs = &trace.system_prompt.guidance_signals;
    let ctx = |on: bool, name: &'static str, desc: &'static str| -> Option<SignalItem> {
        on.then_some(SignalItem {
            name,
            description: desc,
            kind: SignalKind::Context,
        })
    };
    let guide = |on: bool, name: &'static str, desc: &'static str| -> Option<SignalItem> {
        on.then_some(SignalItem {
            name,
            description: desc,
            kind: SignalKind::Guidance,
        })
    };
    [
        ctx(
            cs.active_output_skills,
            "active_output_skills",
            "The session has output-shaping skills that mutate the reply format",
        ),
        ctx(
            cs.memory_signal_detected,
            "memory_signal_detected",
            "Retrieval found a memory worth surfacing this turn",
        ),
        ctx(
            cs.system_prompt_override,
            "system_prompt_override",
            "User/config overrode the default system prompt",
        ),
        ctx(
            cs.effort_hint,
            "effort_hint",
            "Turn carries a target effort level (light / thorough / …)",
        ),
        ctx(
            cs.agent_type_hint,
            "agent_type_hint",
            "Prompt declares a specific agent sub-type for this turn",
        ),
        ctx(
            cs.self_awareness,
            "self_awareness",
            "Self-awareness nudge (remind model of its own constraints)",
        ),
        ctx(
            cs.implicit_feedback,
            "implicit_feedback",
            "Implicit-feedback block injected (recent turn outcomes)",
        ),
        ctx(
            cs.learned_feedback_rules,
            "learned_feedback_rules",
            "Learned corrections from prior sessions are active",
        ),
        guide(
            gs.parallel_feedback,
            "parallel_feedback",
            "Parallel-execution feedback attached",
        ),
        guide(
            gs.parallel_batching_nudge,
            "parallel_batching_nudge",
            "Recent rounds each ran one tool — nudge toward parallel batches",
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn build_decisions(trace: &ContextAssemblyTrace) -> Vec<DecisionItem> {
    trace
        .explanations
        .iter()
        .map(|d: &DecisionExplanation| DecisionItem {
            label: render_decision_label(&d.decision_type),
            reasoning: d.reasoning.clone(),
            confidence: d.confidence,
            alternatives: d
                .alternatives_considered
                .iter()
                .map(|a| AlternativeItem {
                    description: a.description.clone(),
                    score: a.score,
                    why_not_chosen: a.why_not_chosen.clone(),
                })
                .collect(),
        })
        .collect()
}

fn render_decision_label(t: &DecisionType) -> String {
    match t {
        DecisionType::ToolSurface { visible_tools } => {
            format!("Tool surface ({})", visible_tools.join(", "))
        }
        DecisionType::HistoryCompression { turns_affected } => {
            format!("History compression ({} turns)", turns_affected.len())
        }
        DecisionType::MemoryRetrieval { memories } => {
            format!("Memory retrieval ({} memories)", memories.len())
        }
        DecisionType::StrategyChoice { strategy } => format!("Strategy choice ({strategy})"),
    }
}

fn build_system_sections(
    trace: &ContextAssemblyTrace,
    snap: &ContextSnapshot<'_>,
) -> Vec<SectionItem> {
    let sp = &trace.system_prompt;
    let persona_preview = snap.model.map(|m| format!("model: {m}"));
    let env_preview = match (snap.cwd.as_deref(), snap.git_branch.as_deref()) {
        (Some(cwd), Some(branch)) => Some(format!("{cwd}  ·  git: {branch}")),
        (Some(cwd), None) => Some(cwd.to_string()),
        (None, Some(branch)) => Some(format!("git: {branch}")),
        (None, None) => None,
    };
    let prefs_preview = snap.user_rules_path.clone();
    let raw: [(&str, u32, Option<String>); 3] = [
        ("Persona", sp.base_persona_tokens, persona_preview),
        ("Environment", sp.environment_tokens, env_preview),
        (
            "User preferences",
            sp.user_preferences_tokens,
            prefs_preview,
        ),
    ];
    raw.into_iter()
        .filter(|(_, t, _)| *t > 0)
        .map(|(name, tokens, preview)| SectionItem {
            name: name.to_string(),
            tokens,
            preview,
        })
        .collect()
}
