//! Trace types for pipeline event filtering and observability.
//!
//! These types are shared across `astra-config` and `astra-pipeline` to avoid
//! duplication and ensure consistency.

use std::str::FromStr;

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
    /// Decision explanations (tool surface rationale).
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

/// Tiered trace verbosity — a user-facing control that maps to `min_level`.
///
/// | Mode    | min_level  | What passes                          |
/// |---------|-----------|--------------------------------------|
/// | Off     | (none)    | Nothing is emitted                   |
/// | Terse   | Warn      | Only Error + Warn (problems only)    |
/// | Verbose | Trace     | Everything (LLM bodies, thinking…)   |
///
/// When unset, the default `min_level` is `Info` (normal operational events).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TraceVerbosity {
    /// No events emitted — equivalent to disabling tracing entirely.
    Off,
    /// Error + Warn only (problems and degraded signals).
    Terse,
    /// Use default `min_level` (Info) — normal operational events.
    #[default]
    Normal,
    /// All levels including Trace — LLM bodies, thinking, full detail.
    Verbose,
}

impl TraceVerbosity {
    /// Convert to the corresponding `TraceLevel` threshold, or `None` for `Off`.
    pub fn min_level(self) -> Option<TraceLevel> {
        match self {
            TraceVerbosity::Off => None,
            TraceVerbosity::Terse => Some(TraceLevel::Warn),
            TraceVerbosity::Normal => Some(TraceLevel::Info),
            TraceVerbosity::Verbose => Some(TraceLevel::Trace),
        }
    }
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
