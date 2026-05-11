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
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct MemoryItem {
    pub preview: String,
    pub tokens: u32,
    pub relevance: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SkillItem {
    pub name: String,
    pub tokens: u32,
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
            })
            .collect();
        memories.sort_by_key(|m| std::cmp::Reverse(m.tokens));

        // Skills injected into the system prompt (from the per-turn
        // skill selector / injector).  Same sort rule as memories.
        let mut skills: Vec<SkillItem> = trace
            .system_prompt
            .skills_injected
            .iter()
            .filter(|s: &&SkillInjection| s.tokens > 0)
            .map(|s| SkillItem {
                name: s.skill_name.clone(),
                tokens: s.tokens,
            })
            .collect();
        skills.sort_by_key(|s| std::cmp::Reverse(s.tokens));

        // System-prompt sub-rows: the trace doesn't currently split
        // the system prompt into named sections, so we synthesize a
        // coarse split from the known scalar fields.  Zero-token
        // rows are dropped.
        let system_sections = build_system_sections(trace);

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
