//! Structured data source catalog for the context pipeline.
//!
//! `ContextSources` is the "information_schema" — the pipeline's typed view
//! of all data that can flow into an LLM context. It replaces the flat
//! `AgenticLoopState` as the pipeline's interface.
//!
//! 8 tiers ordered by volatility (most stable first). The Plan phase reads
//! only what it needs; the Bind phase fetches from all tiers Plan selected.

use std::collections::HashMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::emergent_context::EmergentContext;
use crate::microcompact::ProviderCacheStrategy;
use crate::pipeline_config::ProviderCachePolicy;
use crate::pipeline_stats::PipelineStats;
use crate::recovery_state::RecoveryState;
use crate::section_types::PromptSection;
use crate::session_latches::SessionLatches;
use crate::token_accounting::TokenAccounting;

/// The catalog that the pipeline queries during Plan and Bind.
///
/// All references are immutable — the pipeline never mutates its sources.
/// The loop orchestrator owns the mutable state and constructs this view
/// before each pipeline invocation.
pub struct ContextSources<'a> {
    pub statics: &'a StaticSections,
    pub agent: &'a AgentContext,
    pub latches: &'a SessionLatches,
    pub session: &'a SessionContext,
    pub turn: &'a TurnState,
    pub external: &'a ExternalSources,
    pub emergent: &'a EmergentContext,
    pub stats: &'a PipelineStats,
}

/// Pre-compiled static text sections. Immutable after build.
#[derive(Debug, Clone)]
pub struct StaticSections {
    pub core_rules: PromptSection,
    pub planning_protocol: PromptSection,
    pub coding_discipline: PromptSection,
    pub turn_discipline: PromptSection,
    pub parallel_efficiency: PromptSection,
    pub output_format: PromptSection,
    pub tool_error_recovery: PromptSection,
}

/// Agent-level context. Set at init, stable for agent lifetime.
#[derive(Debug, Clone, Default)]
pub struct AgentContext {
    pub agent_id: String,
    pub persona: String,
    pub tool_schemas: Vec<Value>,
    pub skill_names: Vec<String>,
    pub delegation_targets: Vec<String>,
}

/// Session-level context. Set at session start, stable within session.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub run_id: String,
    pub model_id: String,
    pub model_limit: u32,
    pub provider_policy: ProviderCachePolicy,
    pub provider_strategy: ProviderCacheStrategy,
    pub project_context: String,
    pub edge_profile: EdgeProfile,
    pub self_model: Option<String>,
}

/// Edge profile — workspace and runtime environment info.
#[derive(Debug, Clone, Default)]
pub struct EdgeProfile {
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub os: Option<String>,
    pub shell: Option<String>,
    pub agent_id: Option<String>,
    pub edge_executor_id: Option<String>,
}

/// Per-turn mutable state — the "working set."
///
/// This is a typed view over the relevant subset of `AgenticLoopState`.
/// The ~125 execution-machinery fields stay in the loop orchestrator.
#[derive(Debug)]
pub struct TurnState {
    pub messages: Vec<Value>,
    pub tool_results: Vec<Value>,
    pub tokens: TokenAccounting,
    pub active_skills: Vec<String>,
    pub recent_file_reads: HashMap<String, u32>,
    pub remaining_turns: u32,
    pub turn_index: u32,
    pub recovery: RecoveryState,
    /// The user's latest message text (for memory retrieval queries).
    pub last_user_message: String,
}

/// External data sources — fetched on demand, not owned by the pipeline.
///
/// Each field is a pre-computed prompt fragment from the runtime. The pipeline
/// includes non-empty fields in the RuntimeIdentity section (per-turn dynamic).
/// This keeps all runtime-specific logic in the runtime crate while the pipeline
/// owns structure, ordering, and cache optimization.
#[derive(Debug, Default)]
pub struct ExternalSources {
    /// Memory text already retrieved by the runtime before entering the pure
    /// core pipeline. Core does not perform Memoria I/O.
    pub memory_snippets: Vec<String>,
    pub spill_dir: Option<PathBuf>,
    /// Learned context from skill quality tracker / session history.
    pub learned_context: Option<String>,
    /// Delegation system override (injected by orchestrator).
    pub system_override: Option<String>,
    /// Plan-in-progress reminder ("You are executing step 3 of 5...").
    pub plan_context: Option<String>,
    /// Tool round guidance (budget warning, batching nudge, etc.).
    pub tool_guidance: Option<String>,
    /// Skill effort/agent_type hint ("effort: high", "agent: reviewer").
    pub effort_hint: Option<String>,
    /// Self-model section (tool-dependent capabilities description).
    pub self_model_text: Option<String>,
    /// Tool-conditional guidance (search strategy, task-type-specific rules).
    pub tool_conditional: Option<String>,
    /// Project profile description (cwd, git_branch, project facts).
    pub profile_desc: Option<String>,
}

impl StaticSections {
    /// Collect all static sections into a Vec for iteration.
    pub fn as_vec(&self) -> Vec<&PromptSection> {
        vec![
            &self.core_rules,
            &self.planning_protocol,
            &self.coding_discipline,
            &self.turn_discipline,
            &self.parallel_efficiency,
            &self.output_format,
            &self.tool_error_recovery,
        ]
    }

    /// Total estimated tokens across all static sections.
    pub fn total_tokens_estimate(&self) -> u32 {
        self.as_vec()
            .iter()
            .map(|s| (s.text.len() / 4) as u32)
            .sum()
    }
}

impl StaticSections {
    /// Build a minimal StaticSections for testing.
    /// Build a minimal StaticSections for testing.
    /// Available in tests (both unit and integration).
    pub fn test_default() -> Self {
        use crate::context_assembly_trace::PromptTraceSignals;
        use crate::section_types::CacheScope;
        Self {
            core_rules: PromptSection {
                text: "You are an expert.".into(),
                scope: CacheScope::Global,
                token_bucket: crate::section_types::PromptTokenBucket::BasePersona,
                trace_signals: PromptTraceSignals::default(),
            },
            planning_protocol: PromptSection::stable("Plan carefully.", CacheScope::Global),
            coding_discipline: PromptSection::stable("Read before write.", CacheScope::Global),
            turn_discipline: PromptSection::stable("Announce actions.", CacheScope::Global),
            parallel_efficiency: PromptSection::stable("Batch tool calls.", CacheScope::Global),
            output_format: PromptSection::stable("Be concise.", CacheScope::Global),
            tool_error_recovery: PromptSection::stable("Retry on error.", CacheScope::Global),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_sections_as_vec_has_7_entries() {
        let s = StaticSections::test_default();
        assert_eq!(s.as_vec().len(), 7);
    }

    #[test]
    fn static_sections_total_tokens_nonzero() {
        let s = StaticSections::test_default();
        assert!(s.total_tokens_estimate() > 0);
    }

    #[test]
    fn edge_profile_default_is_empty() {
        let e = EdgeProfile::default();
        assert!(e.cwd.is_none());
        assert!(e.git_branch.is_none());
    }
}
