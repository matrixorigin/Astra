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

#![allow(dead_code)]

use astra_turn_core::context_assembly_trace::{
    ContextAssemblyTrace, DecisionExplanation, DecisionType, MemoryInjection, MemoryRejection,
    MemorySelection, RejectionReason, SkillInjection, ToolSelected,
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
    /// Per-event detail (from the latest trace's turns_compressed).
    pub events: Vec<CompactionEventItem>,
    /// Aggregate token shape (last turn only).
    pub tokens_before: u32,
    pub tokens_after: u32,
}

impl CompactionSummary {
    pub fn is_empty(&self) -> bool {
        !self.triggered_this_turn && self.compressed_turns.is_empty() && self.events.is_empty()
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

/// Extra detail for the Memory section.  Populated directly from
/// `MemoryRetrievalTrace` — query, candidates considered, rejection
/// list with reasons, retrieval latency.  Plus `repository_memories`
/// from the system prompt (distinct from `memories` which covers
/// retrieval-pipeline selections) so `.astra/memories` files get
/// their own rows.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct MemoryFocus {
    pub query: String,
    pub candidates_considered: u32,
    pub retrieval_latency_ms: u64,
    pub rejected: Vec<MemoryRejectionItem>,
    pub repository: Vec<RepositoryMemoryItem>,
}

impl MemoryFocus {
    pub fn is_empty(&self) -> bool {
        self.query.is_empty()
            && self.candidates_considered == 0
            && self.retrieval_latency_ms == 0
            && self.rejected.is_empty()
            && self.repository.is_empty()
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
pub(crate) struct RepositoryMemoryItem {
    pub memory_id: String,
    pub memory_type: String,
    pub tokens: u32,
    pub relevance: f64,
    pub preview: String,
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
    pub continuation_anchor: Option<String>,
    pub queued_message: Option<String>,
    pub diagnostics_context: Option<String>,
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
        match self {
            Self::System => Color::Cyan,
            Self::Tools => Color::Magenta,
            Self::Memory => Color::Blue,
            Self::History => Color::Yellow,
            Self::UserMessage => Color::Green,
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
    pub score: f64,
    /// Top-ranked selection factors from the tool scorer.  Each
    /// entry is `(factor_name, weight)` — kept short so expanded
    /// rows stay readable.
    pub factors: Vec<(String, f64)>,
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
    /// model data source carries it — e.g. selector shortlist).
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
        match self {
            Self::Low => Color::Green,
            Self::Warning => Color::Yellow,
            Self::Critical => Color::Red,
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
/// trace itself captures token counts only; this snapshot carries
/// the human-readable previews the expanded view renders under
/// each item (first line of each history turn's text, cwd + git
/// branch for Environment, etc). All fields are optional so
/// callers can opt in incrementally.
#[derive(Debug, Default, Clone)]
pub(crate) struct ContextSnapshot<'a> {
    /// Per-history-turn plain-text preview (first non-blank line).
    /// Key matches [`TurnRetention::turn_index`] / [`TurnCompression::turn_index`].
    pub history_previews: std::collections::HashMap<u32, String>,
    /// Per-history-turn full body. Used when the user drills into
    /// a turn. Same key as `history_previews`. Missing entries
    /// fall back to the preview in the drill view.
    pub history_bodies: std::collections::HashMap<u32, String>,
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
    /// Every turn in this session that fired compaction.  Sourced
    /// from `ObservabilitySession.compressed_turns` — the current
    /// trace only knows about the LAST turn's compaction events,
    /// so this list is what makes the Compaction section show a
    /// session-level timeline.
    pub compressed_turns: Vec<u32>,
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

    /// Build from the most recent full [`ContextAssemblyTrace`].
    /// Equivalent to [`from_trace_with`] with an empty snapshot —
    /// no content previews, just the counts the trace carries.
    pub fn from_trace(trace: &ContextAssemblyTrace) -> Self {
        Self::from_trace_with(trace, &ContextSnapshot::default())
    }

    /// Build from a trace plus an auxiliary [`ContextSnapshot`] of
    /// human-readable previews.  The caller passes what it knows
    /// (history text, cwd, git branch, user-rules path) and the
    /// expanded view renders them alongside the raw token counts.
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

        // Tools: keep the original selection order (trace-scoring
        // already ranks them).  Tokens come from the per-tool count
        // the selector emitted.  Zero-token entries are filtered so
        // a partial trace doesn't fill the panel with noise.
        let tools: Vec<ToolItem> = trace
            .tools
            .tools_selected
            .iter()
            .filter(|t: &&ToolSelected| t.tokens > 0)
            .map(|t| ToolItem {
                name: t.tool_name.clone(),
                tokens: t.tokens,
                score: t.score,
                factors: t
                    .selection_factors
                    .iter()
                    .take(3)
                    .map(|f| (f.factor_name.clone(), f.weight))
                    .collect(),
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
        // per-skill token counts.  When the runtime only records a
        // selector shortlist (common for providers that don't break
        // down per-skill cost), fall back to those names with
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
        let preview_of =
            |idx: u32| -> String { snap.history_previews.get(&idx).cloned().unwrap_or_default() };
        let body_of =
            |idx: u32| -> String { snap.history_bodies.get(&idx).cloned().unwrap_or_default() };
        for r in &h.turns_retained {
            turns.push(TurnDetail {
                index: r.turn_index,
                role: r.role.clone(),
                tokens: r.tokens,
                has_tool_calls: r.has_tool_calls,
                compressed_from: None,
                preview: preview_of(r.turn_index),
                body: body_of(r.turn_index),
            });
        }
        for c in &h.turns_compressed {
            turns.push(TurnDetail {
                index: c.turn_index,
                role: c.role.clone(),
                tokens: c.compressed_tokens,
                has_tool_calls: false,
                compressed_from: Some((c.original_tokens, format!("{:?}", c.compression_method))),
                preview: preview_of(c.turn_index),
                body: body_of(c.turn_index),
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
    CompactionSummary {
        triggered_this_turn: trace.token_budget.compression_triggered,
        compressed_turns: snap.compressed_turns.clone(),
        events,
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
    let repository: Vec<RepositoryMemoryItem> = trace
        .system_prompt
        .repository_memories
        .iter()
        .filter(|mi: &&MemoryInjection| mi.tokens > 0)
        .map(|mi| RepositoryMemoryItem {
            memory_id: mi.memory_id.clone(),
            memory_type: mi.memory_type.clone(),
            tokens: mi.tokens,
            relevance: mi.relevance_score,
            preview: mi.content_preview.clone(),
        })
        .collect();
    MemoryFocus {
        query: m.query.clone(),
        candidates_considered: m.candidates_considered,
        retrieval_latency_ms: m.retrieval_latency_ms,
        rejected,
        repository,
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
            cs.memoria_insights,
            "memoria_insights",
            "Memoria MCP provided cross-session insights",
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
            gs.round_budget_warning,
            "round_budget_warning",
            "Per-round token budget is tight — warn the model",
        ),
        guide(
            gs.synthesize_or_batch,
            "synthesize_or_batch",
            "Encourage synthesis / batching instead of drive-by actions",
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
        DecisionType::ToolSelection { tools } => format!("Tool selection ({})", tools.join(", ")),
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
