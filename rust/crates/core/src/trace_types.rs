//! Trace types for pipeline event filtering and observability.
//!
//! These types are shared across `astra-config` and `astra-pipeline` to avoid
//! duplication and ensure consistency.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use std::sync::RwLock;

/// Per-process global minimum trace level.
///
/// Set by [`set_global_min_level`] (typically from `SessionTraceConfig::set_current()`).
/// Read by `EventLog::new()` as its default filter.
pub static GLOBAL_MIN_LEVEL: RwLock<Option<TraceLevel>> = RwLock::new(None);

/// Per-process global enabled trace categories.
///
/// Set by [`set_global_enabled_categories`] (typically from `SessionTraceConfig::set_current()`).
/// Read by `EventLog::new()` as its default filter.
pub static GLOBAL_ENABLED_CATEGORIES: RwLock<Option<Vec<TraceCategory>>> = RwLock::new(None);

/// Set the global default min_level for all `EventLog::new()` instances.
///
/// After calling this, new `EventLog` instances will use the given level
/// unless overridden via `EventLog::with_config()`.
pub fn set_global_min_level(level: TraceLevel) {
    if let Ok(mut guard) = GLOBAL_MIN_LEVEL.write() {
        *guard = Some(level);
    }
}

/// Set the global default enabled categories for all `EventLog::new()` instances.
///
/// After calling this, new `EventLog` instances will filter by the given categories
/// unless overridden via `EventLog::with_config()`.
/// An empty vec means all categories pass through.
pub fn set_global_enabled_categories(categories: Vec<TraceCategory>) {
    if let Ok(mut guard) = GLOBAL_ENABLED_CATEGORIES.write() {
        *guard = Some(categories);
    }
}

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

impl FromStr for TraceCategory {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tool_calls" => Ok(Self::ToolCalls),
            "llm_exchanges" => Ok(Self::LlmExchanges),
            "context_assembly" => Ok(Self::ContextAssembly),
            "decision_explain" => Ok(Self::DecisionExplain),
            "phase_transition" => Ok(Self::PhaseTransition),
            "budget" => Ok(Self::Budget),
            "reflection" => Ok(Self::Reflection),
            "verification" => Ok(Self::Verification),
            "thinking" => Ok(Self::Thinking),
            "memory_retrieval" => Ok(Self::MemoryRetrieval),
            "skill_execution" => Ok(Self::SkillExecution),
            "prompt_assembly" => Ok(Self::PromptAssembly),
            "guard_evaluation" => Ok(Self::GuardEvaluation),
            "all" => Ok(Self::All),
            _ => Err(()),
        }
    }
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
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum TraceLevel {
    /// Hard failures that require attention.
    Error = 0,
    /// Near-limit conditions, degraded quality signals.
    Warn = 1,
    /// Normal operational events — the baseline for production.
    #[default]
    Info = 2,
    /// Reasoning signals useful for debugging.
    Debug = 3,
    /// Fine-grained detail; only emitted in verbose/dev mode.
    Trace = 4,
}

impl FromStr for TraceLevel {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "error" => Ok(Self::Error),
            "warn" => Ok(Self::Warn),
            "info" => Ok(Self::Info),
            "debug" => Ok(Self::Debug),
            "trace" => Ok(Self::Trace),
            _ => Err(()),
        }
    }
}
