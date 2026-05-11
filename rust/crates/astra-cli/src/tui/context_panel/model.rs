//! Pure data model for the `/context` panel.
//!
//! Sourced from the most recent [`ContextAssemblyTrace`] captured by
//! the observability session. Mirrors Claude Code's `/context`
//! visualization (grid + category legend + nested sections) but
//! adapted to the data that astra's trace actually carries:
//! token-budget breakdown, tool / memory / skill / system-prompt
//! sub-rows, and an explicit "free space" category derived from
//! `max_tokens - total_used`.
//!
//! This module has no render logic — it just produces a structured
//! snapshot the view can walk top-down. Keeping the model pure
//! makes the behaviour easy to unit-test without touching a Ratatui
//! buffer.

#![allow(dead_code)]

use astra_turn_core::context_assembly_trace::{
    ContextAssemblyTrace, MemorySelection, SkillInjection, ToolSelected,
};
use astra_turn_core::skill_selector_metrics::SkillSelectorShortlistEntry;
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
}

/// A labelled sub-section of the system prompt (e.g. "Environment",
/// "Guidance signals").  We keep the structure flat — the trace
/// doesn't currently surface named sections, so most deployments
/// will see just the aggregated system total.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SectionItem {
    pub name: String,
    pub tokens: u32,
}

/// Which nested section the user currently has focused. Drives
/// heading highlight + `Enter to expand` hint visibility. Cycles
/// via Tab / Shift+Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    SystemPrompt,
    Tools,
    Skills,
    Memory,
    History,
}

impl Section {
    pub fn all() -> &'static [Section] {
        &[
            Section::SystemPrompt,
            Section::Tools,
            Section::Skills,
            Section::Memory,
            Section::History,
        ]
    }

    pub fn label(self) -> &'static str {
        match self {
            Section::SystemPrompt => "System prompt",
            Section::Tools => "Tools · /tool",
            Section::Skills => "Skills · /skills",
            Section::Memory => "Memory · /memory",
            Section::History => "History · conversation turns",
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
            Section::SystemPrompt => !self.system_sections.is_empty(),
            Section::Tools => !self.tools.is_empty(),
            Section::Skills => !self.skills.is_empty(),
            Section::Memory => !self.memories.is_empty(),
            Section::History => !self.history.is_empty(),
        }
    }

    /// First section that has content, or None if nothing to drill
    /// into.
    pub fn first_focusable_section(&self) -> Option<Section> {
        Section::all().iter().copied().find(|s| self.section_non_empty(*s))
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
        }
    }

    /// Build from the most recent full [`ContextAssemblyTrace`].
    /// This is the richer source — in addition to the stacked-bar
    /// categories it populates the nested tool / memory / skill
    /// lists that render below the grid.
    pub fn from_trace(trace: &ContextAssemblyTrace) -> Self {
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
        // appear at the top — matches how Claude Code lists memory
        // files under /memory.  Content preview is truncated to
        // ~80 chars by the trace builder; we just pass it through.
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
        if skills.is_empty()
            && let Some(shortlist) = trace.skill_selector.as_ref()
        {
            skills = shortlist
                .skills
                .iter()
                .map(|e: &SkillSelectorShortlistEntry| SkillItem {
                    name: e.skill_name.clone(),
                    tokens: 0,
                    description: if e.description.is_empty() {
                        None
                    } else {
                        Some(e.description.clone())
                    },
                    source: Some(e.source.clone()),
                })
                .collect();
        }

        // System-prompt sub-rows: the trace doesn't currently split
        // the system prompt into named sections, so we synthesize a
        // coarse split from the known scalar fields.  Zero-token
        // rows are dropped.
        let system_sections = build_system_sections(trace);

        // History summary: counts + pre/post-compression token shape.
        // Rendered as a dedicated section so users can see how much
        // of their backlog survived the compactor this turn.
        let h = &trace.history;
        let mut turns: Vec<TurnDetail> = Vec::with_capacity(
            h.turns_retained.len() + h.turns_compressed.len(),
        );
        for r in &h.turns_retained {
            turns.push(TurnDetail {
                index: r.turn_index,
                role: r.role.clone(),
                tokens: r.tokens,
                has_tool_calls: r.has_tool_calls,
                compressed_from: None,
            });
        }
        for c in &h.turns_compressed {
            turns.push(TurnDetail {
                index: c.turn_index,
                role: c.role.clone(),
                tokens: c.compressed_tokens,
                has_tool_calls: false,
                compressed_from: Some((c.original_tokens, format!("{:?}", c.compression_method))),
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

fn build_system_sections(trace: &ContextAssemblyTrace) -> Vec<SectionItem> {
    let sp = &trace.system_prompt;
    let raw = [
        ("Persona", sp.base_persona_tokens),
        ("Environment", sp.environment_tokens),
        ("User preferences", sp.user_preferences_tokens),
    ];
    raw.into_iter()
        .filter(|(_, t)| *t > 0)
        .map(|(name, tokens)| SectionItem {
            name: name.to_string(),
            tokens,
        })
        .collect()
}
