//! Trace types for pipeline event filtering and observability.
//!
//! These types are shared across `astra-config` and `astra-pipeline` to avoid
//! duplication and ensure consistency.

use serde::{Deserialize, Serialize};

/// Trace event categories that can be toggled on/off per session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceCategory {
    /// Tool call lifecycle (start, complete, failures).
    ToolCalls,
    /// Full LLM request/response payloads.
    LlmExchanges,
    /// Context assembly and compression decisions.
    ContextAssembly,
    /// Decision explanations (tool selection rationale).
    DecisionExplain,
    /// Phase transitions (Perceive → Plan → Execute → Evaluate → Reflect).
    PhaseTransition,
    /// Budget tracking (set, update, expansion).
    Budget,
    /// Reflection generation and adaptation.
    Reflection,
    /// Verification and review events.
    Verification,
    /// LLM thinking/reasoning content blocks.
    Thinking,
    /// Memory retrieval queries and results.
    MemoryRetrieval,
    /// Skill loading, execution, and teardown lifecycle.
    SkillExecution,
    /// System prompt assembly and injection decisions.
    PromptAssembly,
    /// Safety guard evaluations and rulings.
    GuardEvaluation,
    /// Meta-category: enables all categories.
    All,
}

impl TraceCategory {
    /// Every individual category except `All`.
    pub fn individual_categories() -> &'static [TraceCategory] {
        &[
            TraceCategory::ToolCalls,
            TraceCategory::LlmExchanges,
            TraceCategory::ContextAssembly,
            TraceCategory::DecisionExplain,
            TraceCategory::PhaseTransition,
            TraceCategory::Budget,
            TraceCategory::Reflection,
            TraceCategory::Verification,
            TraceCategory::Thinking,
            TraceCategory::MemoryRetrieval,
            TraceCategory::SkillExecution,
            TraceCategory::PromptAssembly,
            TraceCategory::GuardEvaluation,
        ]
    }
}

/// Trace severity level, mirroring log-level semantics for pipeline events.
///
/// Used by `EventLog::min_level` to drop events below the configured threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraceLevel {
    /// Hard failures that require attention.
    Error = 0,
    /// Near-limit conditions, degraded quality signals.
    Warn = 1,
    /// Normal operational events — the baseline for production.
    Info = 2,
    /// Reasoning signals useful for debugging.
    Debug = 3,
    /// Fine-grained detail; only emitted in verbose/dev mode.
    Trace = 4,
}

impl Default for TraceLevel {
    fn default() -> Self {
        Self::Info
    }
}

impl TraceLevel {
    /// Parse a [`TraceLevel`] from a lowercase string (e.g. "debug", "warn").
    ///
    /// Returns `None` for unknown strings.
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "error" => Some(Self::Error),
            "warn" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            "trace" => Some(Self::Trace),
            _ => None,
        }
    }
}
