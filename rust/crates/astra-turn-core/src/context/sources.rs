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
use crate::working_memory::WorkingMemoryState;

/// A structured memory item retrieved before entering the pure core pipeline.
///
/// Core still performs no Memoria I/O; this metadata gives Plan/Bind enough
/// catalog information to rank, deduplicate, and budget memory content instead
/// of treating recall as one opaque text blob.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    pub content: String,
    pub relevance_score: f64,
    pub source: Option<String>,
    pub token_estimate: u32,
    pub freshness_turn: Option<u32>,
    pub content_hash: u64,
}

impl MemoryEntry {
    #[must_use]
    pub fn new(content: impl Into<String>) -> Self {
        Self::scored(content, 0.0)
    }

    #[must_use]
    pub fn scored(content: impl Into<String>, relevance_score: f64) -> Self {
        let content = content.into();
        let token_estimate = crate::section_types::estimate_text_tokens(&content);
        let content_hash = stable_content_hash(&content);
        Self {
            content,
            relevance_score,
            source: None,
            token_estimate,
            freshness_turn: None,
            content_hash,
        }
    }

    #[must_use]
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    #[must_use]
    pub fn with_freshness_turn(mut self, turn: u32) -> Self {
        self.freshness_turn = Some(turn);
        self
    }

    #[must_use]
    pub fn with_token_estimate(mut self, token_estimate: u32) -> Self {
        self.token_estimate = token_estimate;
        self
    }
}

fn stable_content_hash(content: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x00000100000001b3;

    content.as_bytes().iter().fold(FNV_OFFSET, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

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
    pub working_memory: Option<&'a WorkingMemoryState>,
    pub stats: &'a PipelineStats,
}

/// Pre-compiled static text sections. Immutable after build.
#[derive(Debug, Clone)]
pub struct StaticSections {
    pub core_rules: PromptSection,
    pub safety: PromptSection,
    pub planning_protocol: PromptSection,
    pub coding_discipline: PromptSection,
    pub turn_discipline: PromptSection,
    pub plan_execution: PromptSection,
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
#[derive(Debug, Clone, Default)]
pub struct SessionContext {
    pub session_id: String,
    pub run_id: String,
    pub model_id: String,
    pub provider_name: String,
    pub model_limit: u32,
    pub provider_policy: ProviderCachePolicy,
    pub provider_strategy: ProviderCacheStrategy,
    pub project_context: String,
    pub edge_profile: EdgeProfile,
    pub self_model: Option<String>,
    /// Pre-rendered `<deferred_tools>` system block. Session-scoped so it
    /// joins the cached prefix. Empty when no tools are deferred.
    pub deferred_tools_block: String,
    /// Pre-rendered `<available_skills>` system block. Session-scoped.
    /// Empty when no skills are loaded.
    pub skill_listing_block: String,
    /// Session-creation date (YYYY-MM-DD). Session-stable; injected into
    /// RuntimeIdentity so the model knows the current date.
    pub current_date: String,
    /// Authenticated user identity. Session-stable; injected into
    /// RuntimeIdentity so the model knows who it is talking to.
    pub user_id: Option<String>,
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
#[derive(Default)]
pub struct ExternalSources {
    /// Memory entries already retrieved by the runtime before entering the
    /// pure core pipeline. Core does not perform Memoria I/O.
    pub memory_entries: Vec<MemoryEntry>,
    /// Current-session recovery state, carried as a dedicated typed source
    /// instead of being flattened into synthetic compacted-history messages.
    pub session_memory_entry: Option<MemoryEntry>,
    pub spill_dir: Option<PathBuf>,
    /// Optional spill backend for offloading oversized sections to disk.
    /// When set, the optimizer will persist section content and replace it
    /// with a lightweight `SpillReference` to free token budget.
    pub spill_backend: Option<std::sync::Arc<dyn crate::spill_backend::SpillBackend>>,
    /// Delegation system override (injected by orchestrator).
    pub system_override: Option<String>,
    /// Plan-in-progress reminder ("You are executing step 3 of 5...").
    pub plan_context: Option<String>,
    /// Tool round guidance (budget warning, batching nudge, etc.).
    pub tool_guidance: Option<String>,
    /// Skill effort/agent_type hint ("effort: high", "agent: reviewer").
    pub effort_hint: Option<String>,

    /// **Session-stable** pre-built sections — bridge-composed content
    /// that persists across turns (skill hint, accumulated feedback rules,
    /// self-awareness hint, any caller-composed static snippet).
    ///
    /// Bound into the `RuntimeIdentity` section (Session scope), so they
    /// sit BEFORE the Session→None cache marker and participate in
    /// Anthropic's per-session prompt cache. Empty by default; callers
    /// opt-in per fragment.
    ///
    /// Split from the legacy single `extra_dynamic_sections` field after
    /// observing that the bridge was shoving session-stable content into
    /// the volatile lane, losing ~6kB of cacheable tokens per turn.
    pub extra_stable_sections: Vec<PromptSection>,

    /// **Turn-volatile** pre-built sections — bridge-composed content
    /// that can change every turn (session anchor, memoria insights that
    /// rotate, recent-arg hints, per-turn tool round guidance).
    ///
    /// Bound into the `RuntimeVolatile` section (None scope), so they sit
    /// AFTER the Session→None cache marker and can churn freely without
    /// invalidating the cached session prefix.
    pub extra_dynamic_sections: Vec<PromptSection>,
}

impl StaticSections {
    /// Collect all static sections into a Vec for iteration.
    pub fn as_vec(&self) -> Vec<&PromptSection> {
        vec![
            &self.core_rules,
            &self.safety,
            &self.planning_protocol,
            &self.coding_discipline,
            &self.turn_discipline,
            &self.plan_execution,
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
            safety: PromptSection::stable("Refuse harmful requests.", CacheScope::Global),
            planning_protocol: PromptSection::stable("Plan carefully.", CacheScope::Global),
            coding_discipline: PromptSection::stable("Read before write.", CacheScope::Global),
            turn_discipline: PromptSection::stable("Announce actions.", CacheScope::Global),
            plan_execution: PromptSection::stable(
                "Execute plan subtasks faithfully.",
                CacheScope::Global,
            ),
            output_format: PromptSection::stable("Be concise.", CacheScope::Global),
            tool_error_recovery: PromptSection::stable("Retry on error.", CacheScope::Global),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_sections_as_vec_has_8_entries() {
        let s = StaticSections::test_default();
        assert_eq!(s.as_vec().len(), 8);
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
