//! Pure data model for the `/context` panel — RED phase stub.

#![allow(dead_code)]

use ratatui::style::Color;

/// Categorical breakdown of the context window for a single turn.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ContextBreakdown {
    pub total_used: u32,
    pub limit: u32,
    pub pressure: f64,
    pub categories: Vec<Category>,
}

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
            Self::System => "system",
            Self::Tools => "tools",
            Self::Memory => "memory",
            Self::History => "history",
            Self::UserMessage => "current",
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
        }
    }

    /// Build from a turn-core [`TokenBudgetTrace`].
    ///
    /// Category order mirrors how the context is assembled: system
    /// first (immutable), tool schemas (semi-fixed), memory, then the
    /// growing history, with the current user turn last.  Empty
    /// (zero-token) categories are dropped so the rendered stacked bar
    /// doesn't ship micro-slices users can't read.
    pub fn from_trace(trace: &astra_turn_core::context_assembly_trace::TokenBudgetTrace) -> Self {
        let limit = trace.max_tokens;
        let pct = |tokens: u32| -> f64 {
            if limit == 0 {
                0.0
            } else {
                tokens as f64 / limit as f64 * 100.0
            }
        };
        let raw = [
            (CategoryKind::System, trace.system_prompt_tokens),
            (CategoryKind::Tools, trace.tool_schema_tokens),
            (CategoryKind::Memory, trace.memory_tokens),
            (CategoryKind::History, trace.history_tokens),
            (CategoryKind::UserMessage, trace.user_message_tokens),
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
        Self {
            total_used: trace.total_used,
            limit,
            pressure: trace.budget_pressure,
            categories,
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
