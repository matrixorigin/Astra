//! Session-scoped pipeline orchestrator.
//!
//! `PipelineSession` owns the cross-turn mutable state (stats, latches,
//! emergent context, recovery) and provides a single `run_turn()` entry point
//! that the agentic loop calls before every LLM request.
//!
//! This replaces the scattered inline assembly with a structured, testable
//! pipeline invocation that carries forward learned behavior across turns.

use crate::compaction_types::CompactionTier;
use crate::context_feedback::ContextFeedback;
use crate::context_optimizer::ContextOptimized;
use crate::context_pipeline::{
    ContextPipeline, PipelineAbort, PipelineExplain, PipelineRunInput, PipelineRunMetrics,
    PipelineRunOutput,
};
use crate::context_planner::ContextPlan;
use crate::context_serializer::SerializedProviderRequest;
use crate::context_sources::{
    AgentContext, ContextSources, ExternalSources, SessionContext, StaticSections, TurnState,
};
use crate::emergent_context::EmergentContext;
use crate::optimize_limits::OptimizeLimits;
use crate::pipeline_config::PipelineConfig;
use crate::pipeline_stats::PipelineStats;
use crate::recovery_state::RecoveryState;
use crate::session_latches::SessionLatches;
use crate::shadow_diff::{ShadowDiffResult, diff_pipeline_outputs};

/// Per-turn input provided by the agentic loop to `PipelineSession::run_turn()`.
pub struct TurnInput<'a> {
    pub statics: &'a StaticSections,
    pub agent: &'a AgentContext,
    pub session: &'a SessionContext,
    pub turn: &'a TurnState,
    pub external: &'a ExternalSources,
    pub optimize_limits: &'a OptimizeLimits,
    pub model_id: &'a str,
    pub query_source: &'a str,
}

/// Simplified input for `run_turn_adaptive()` — limits derived from plan tier.
pub struct AdaptiveTurnInput<'a> {
    pub statics: &'a StaticSections,
    pub agent: &'a AgentContext,
    pub session: &'a SessionContext,
    pub turn: &'a TurnState,
    pub external: &'a ExternalSources,
    pub model_id: &'a str,
    pub query_source: &'a str,
}

/// Output from a successful pipeline turn.
pub struct TurnOutput {
    pub plan: ContextPlan,
    pub optimized: ContextOptimized,
    pub serialized: SerializedProviderRequest,
    pub explain: PipelineExplain,
    pub metrics: PipelineRunMetrics,
}

/// Shadow mode comparison result (when running dual paths during rollout).
pub struct ShadowTurnOutput {
    pub pipeline_output: TurnOutput,
    pub diff: ShadowDiffResult,
}

/// Session-scoped pipeline orchestrator.
///
/// Instantiated once per session. Accumulates statistics, latches, emergent
/// context, and recovery state across turns. The agentic loop calls
/// `run_turn()` before each LLM request and `record_feedback()` after.
pub struct PipelineSession {
    pipeline: ContextPipeline,
    pub stats: PipelineStats,
    pub latches: SessionLatches,
    pub emergent: EmergentContext,
    pub recovery: RecoveryState,
    turns_completed: u32,
    pending_audits: Vec<crate::pipeline_journal::PipelineJournalEvent>,
}

impl PipelineSession {
    /// Create a new session with the given pipeline configuration.
    #[must_use]
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            pipeline: ContextPipeline::new(config),
            stats: PipelineStats::default(),
            latches: SessionLatches::default(),
            emergent: EmergentContext::default(),
            recovery: RecoveryState::default(),
            turns_completed: 0,
            pending_audits: Vec::new(),
        }
    }

    /// Create a session with pre-loaded stats (warm start from persistence).
    #[must_use]
    pub fn with_warm_stats(config: PipelineConfig, stats: PipelineStats) -> Self {
        Self {
            pipeline: ContextPipeline::new(config),
            stats,
            latches: SessionLatches::default(),
            emergent: EmergentContext::default(),
            recovery: RecoveryState::default(),
            turns_completed: 0,
            pending_audits: Vec::new(),
        }
    }

    /// Create a session restored from a checkpoint (full warm start).
    ///
    /// Restores stats (EMA, percentile estimator), latches (frozen headers/scope),
    /// and recovery state (escalation history). Transient error counters are
    /// cleared by `deserialize_session_state()` before reaching this constructor.
    #[must_use]
    pub fn with_restored_state(
        config: PipelineConfig,
        stats: PipelineStats,
        latches: SessionLatches,
        recovery: RecoveryState,
    ) -> Self {
        Self {
            pipeline: ContextPipeline::new(config),
            stats,
            latches,
            emergent: EmergentContext::default(),
            recovery,
            turns_completed: 0,
            pending_audits: Vec::new(),
        }
    }

    /// Number of turns successfully completed in this session.
    #[must_use]
    pub fn turns_completed(&self) -> u32 {
        self.turns_completed
    }

    /// Run the pipeline for one turn. Returns the serialized provider request
    /// and associated metadata, or an abort if the session is in an
    /// unrecoverable error state.
    pub fn run_turn(&self, input: TurnInput<'_>) -> Result<TurnOutput, PipelineAbort> {
        let sources = ContextSources {
            statics: input.statics,
            agent: input.agent,
            latches: &self.latches,
            session: input.session,
            turn: input.turn,
            external: input.external,
            emergent: &self.emergent,
            stats: &self.stats,
        };

        let run_input = PipelineRunInput {
            sources: &sources,
            tokens: &input.turn.tokens,
            model_limit: input.session.model_limit,
            recovery: &self.recovery,
            latches: &self.latches,
            optimize_limits: input.optimize_limits,
            model_id: input.model_id,
            query_source: input.query_source,
        };

        let PipelineRunOutput {
            plan,
            optimized,
            serialized,
            explain,
            metrics,
        } = self.pipeline.run(run_input)?;

        Ok(TurnOutput {
            plan,
            optimized,
            serialized,
            explain,
            metrics,
        })
    }

    /// Run the pipeline with tier-adaptive limits. The Plan phase determines
    /// the compaction tier, then limits are derived automatically from that tier.
    /// When a compaction cascade is active, clearing is suppressed to break the loop.
    /// This is the preferred entry point for runtime integration.
    pub fn run_turn_adaptive(
        &self,
        input: AdaptiveTurnInput<'_>,
    ) -> Result<TurnOutput, PipelineAbort> {
        let limits = self.cascade_aware_limits(input.session.model_limit);
        let full_input = TurnInput {
            statics: input.statics,
            agent: input.agent,
            session: input.session,
            turn: input.turn,
            external: input.external,
            optimize_limits: &limits,
            model_id: input.model_id,
            query_source: input.query_source,
        };
        self.run_turn(full_input)
    }

    /// Run the pipeline in shadow mode: produce pipeline output AND compare
    /// against a pre-computed legacy `ContextOptimized` for diffing.
    pub fn run_turn_shadow(
        &self,
        input: TurnInput<'_>,
        legacy_output: &ContextOptimized,
    ) -> Result<ShadowTurnOutput, PipelineAbort> {
        let turn_index = input.turn.turn_index;
        let output = self.run_turn(input)?;
        let diff = diff_pipeline_outputs(legacy_output, &output.optimized, turn_index);
        Ok(ShadowTurnOutput {
            pipeline_output: output,
            diff,
        })
    }

    /// Record feedback from the API response. Updates stats, recovery state,
    /// and detects cache breaks. Call this after every successful API response.
    ///
    /// Pass `turn_output` to also record per-section token usage (enables
    /// adaptive budget allocation). If unavailable, pass `None`.
    pub fn record_feedback(
        &mut self,
        model_id: &str,
        query_source: &str,
        feedback: ContextFeedback,
        turn_output: Option<&TurnOutput>,
    ) {
        self.stats.record(model_id, query_source, &feedback);
        self.recovery.process_feedback(feedback.was_truncated);

        if feedback.was_truncated
            && self.stats.turns_executed > 1
            && self.recovery.consecutive_ptl_errors > 0
        {
            let tokens_freed = feedback.tokens.cache_creation;
            if tokens_freed > 0 {
                self.stats.record_compaction(tokens_freed);
            }
        }

        if let Some(output) = turn_output {
            let mut usage = std::collections::HashMap::new();
            for section in &output.optimized.sections {
                *usage.entry(section.plan.kind).or_insert(0u32) += section.actual_tokens;
            }
            self.stats.record_section_usage(&usage);
        }

        self.turns_completed += 1;
    }

    /// Record a prompt-too-long error (no successful response).
    pub fn record_ptl_error(&mut self) {
        self.recovery.record_ptl_error();
    }

    /// Record that a reactive compaction was attempted.
    pub fn record_reactive_compact(&mut self) {
        self.recovery.record_reactive_compact();
    }

    /// Get the current compaction tier based on pressure state.
    /// Useful for the runtime to decide whether to attempt reactive compaction.
    #[must_use]
    pub fn current_pressure_tier(&self) -> CompactionTier {
        if self.recovery.consecutive_ptl_errors >= 2 {
            CompactionTier::AggressivePrune
        } else if self.recovery.consecutive_ptl_errors >= 1 {
            CompactionTier::CompactHistory
        } else {
            CompactionTier::Normal
        }
    }

    /// Whether the session should abort (unrecoverable error streak).
    #[must_use]
    pub fn should_abort(&self) -> bool {
        self.recovery.should_abort()
    }

    /// Access the underlying pipeline config.
    #[must_use]
    pub fn config(&self) -> &PipelineConfig {
        self.pipeline.config()
    }

    // ── Cascade Responder ────────────────────────────────────────────────────

    /// Build optimize limits that respond to compaction cascade detection.
    /// When a cascade is active, suppress tool_result_clearing to break the loop.
    #[must_use]
    pub fn cascade_aware_limits(&self, model_limit: u32) -> OptimizeLimits {
        let mut limits = OptimizeLimits::for_tier(self.current_pressure_tier(), model_limit);
        if self.stats.has_compaction_cascade() {
            limits.allow_tool_result_clearing = false;
        }
        limits
    }

    // ── Compaction Audit Trail ───────────────────────────────────────────────

    /// Record a compaction operation for audit trail emission.
    /// The runtime calls this after each compaction step in the optimizer.
    pub fn record_compaction_audit(&mut self, strategy: &str, items: u32, tokens_freed: u32) {
        self.pending_audits.push(
            crate::pipeline_journal::PipelineJournalEvent::compaction_audit(
                self.stats.turns_executed,
                strategy,
                items,
                tokens_freed,
            ),
        );
    }

    /// Drain pending audit events for journal emission.
    /// The runtime calls this at end-of-turn to emit journal entries.
    pub fn drain_pending_audits(&mut self) -> Vec<crate::pipeline_journal::PipelineJournalEvent> {
        std::mem::take(&mut self.pending_audits)
    }

    // ── Emergent Context Lifecycle ───────────────────────────────────────────

    /// Push a discovered skill into emergent context for the next turn.
    /// The runtime calls this during tool execution when a skill trigger is detected.
    pub fn push_emergent_skill(
        &mut self,
        skill_name: impl Into<String>,
        trigger: impl Into<String>,
        current_turn: u32,
    ) {
        use crate::emergent_context::{DiscoveredSkill, EmergentItem};
        let skill_name = skill_name.into();
        let hash = content_dedup_hash(&skill_name);
        self.emergent.push_skill(EmergentItem {
            value: DiscoveredSkill {
                skill_name,
                trigger: trigger.into(),
            },
            created_at_turn: current_turn,
            content_hash: hash,
        });
    }

    /// Push prefetched memory into emergent context for the next turn.
    /// The runtime calls this when memory is fetched concurrently during streaming.
    pub fn push_emergent_memory(
        &mut self,
        content: impl Into<String>,
        relevance_score: f64,
        current_turn: u32,
    ) {
        use crate::emergent_context::{EmergentItem, PrefetchedMemory};
        let content = content.into();
        let hash = content_dedup_hash(&content);
        self.emergent.push_memory(EmergentItem {
            value: PrefetchedMemory {
                content,
                relevance_score,
            },
            created_at_turn: current_turn,
            content_hash: hash,
        });
    }

    /// Push a tool use summary into emergent context for the next turn.
    pub fn push_emergent_summary(
        &mut self,
        summary: impl Into<String>,
        tool_calls_covered: u32,
        current_turn: u32,
    ) {
        use crate::emergent_context::{EmergentItem, ToolUseSummary};
        let summary = summary.into();
        let hash = content_dedup_hash(&summary);
        self.emergent.push_summary(EmergentItem {
            value: ToolUseSummary {
                summary,
                tool_calls_covered,
            },
            created_at_turn: current_turn,
            content_hash: hash,
        });
    }

    // ── Session Latches Lifecycle ────────────────────────────────────────────

    /// Latch a beta header. Call when the runtime first evaluates a beta feature.
    /// Returns true if newly latched (first time), false if already latched.
    pub fn latch_header(
        &mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        turn: u32,
    ) -> bool {
        self.latches.latch_header(name, value, turn)
    }

    /// Latch the cache scope. Call on the first turn to freeze scope for the session.
    /// Returns true if newly latched.
    pub fn latch_cache_scope(&mut self, scope: crate::section_types::CacheScope, turn: u32) -> bool {
        self.latches.latch_cache_scope(scope, turn)
    }

    /// Latch a provider feature gate.
    pub fn latch_feature(&mut self, key: impl Into<String>, turn: u32) -> bool {
        self.latches.latch_feature(key, turn)
    }

    // ── Snapshot & Restore ──────────────────────────────────────────────────

    /// Capture a full snapshot of all session-scoped pipeline state.
    /// Used for checkpoint persistence (includes emergent context).
    #[must_use]
    pub fn snapshot_full_state(&self) -> PipelineSessionSnapshot {
        PipelineSessionSnapshot {
            stats: self.stats.clone(),
            latches: self.latches.clone(),
            recovery: self.recovery,
            emergent: self.emergent.clone(),
        }
    }

    /// Restore a session from a full snapshot (checkpoint restore).
    #[must_use]
    pub fn from_snapshot(config: PipelineConfig, snapshot: PipelineSessionSnapshot) -> Self {
        let mut recovery = snapshot.recovery;
        // Clear transient per-session error state on restore
        recovery.consecutive_ptl_errors = 0;
        recovery.consecutive_same_errors = 0;
        recovery.has_attempted_reactive_compact = false;

        Self {
            pipeline: ContextPipeline::new(config),
            stats: snapshot.stats,
            latches: snapshot.latches,
            emergent: snapshot.emergent,
            recovery,
            turns_completed: 0,
            pending_audits: Vec::new(),
        }
    }
}

/// Full snapshot of pipeline session state for checkpoint persistence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PipelineSessionSnapshot {
    pub stats: PipelineStats,
    pub latches: SessionLatches,
    pub recovery: RecoveryState,
    pub emergent: EmergentContext,
}

/// Summary metrics extracted from pipeline state for cloud_session_facts.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PipelineSessionMetrics {
    pub avg_cache_hit_ratio: f64,
    pub total_compactions: u32,
    pub turns_executed: u32,
}

impl PipelineSessionMetrics {
    /// Extract metrics from accumulated pipeline stats.
    #[must_use]
    pub fn from_stats(stats: &PipelineStats) -> Self {
        Self {
            avg_cache_hit_ratio: stats.avg_cache_hit_ratio,
            total_compactions: stats.compact_events.len() as u32,
            turns_executed: stats.turns_executed,
        }
    }
}

/// Stable in-process dedup hash for emergent context items.
/// Uses DefaultHasher which is fast and sufficient for same-process deduplication.
fn content_dedup_hash(content: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_sources::EdgeProfile;
    use crate::microcompact::ProviderCacheStrategy;
    use crate::pipeline_config::ProviderCachePolicy;
    use crate::token_accounting::TokenAccounting;
    use std::collections::HashMap;

    fn test_statics() -> StaticSections {
        StaticSections::test_default()
    }

    fn test_session_context() -> SessionContext {
        SessionContext {
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            model_id: "claude-sonnet-4-6".into(),
            model_limit: 200_000,
            provider_policy: ProviderCachePolicy::anthropic(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: "test project".into(),
            edge_profile: EdgeProfile::default(),
            self_model: None,
        }
    }

    fn test_turn_state(turn_index: u32) -> TurnState {
        TurnState {
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            tool_results: vec![],
            tokens: TokenAccounting::default(),
            active_skills: vec![],
            recent_file_reads: HashMap::new(),
            remaining_turns: 10,
            turn_index,
            recovery: RecoveryState::default(),
            last_user_message: "hello".into(),
        }
    }

    fn test_external() -> ExternalSources {
        ExternalSources {
            memory_snippets: vec![],
            spill_dir: None,
        }
    }

    #[test]
    fn new_session_starts_with_zero_turns() {
        let sess = PipelineSession::new(PipelineConfig::default());
        assert_eq!(sess.turns_completed(), 0);
        assert!(!sess.should_abort());
    }

    #[test]
    fn run_turn_produces_valid_output() {
        let sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(1);
        let external = test_external();
        let limits = OptimizeLimits::default();

        let input = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "claude-sonnet-4-6",
            query_source: "repl",
        };

        let output = sess.run_turn(input).expect("should not abort");
        assert_eq!(output.metrics.turn_index, 1);
        assert!(output.explain.phase_timings.len() == 4);
    }

    #[test]
    fn record_feedback_increments_turns() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        sess.record_feedback("model", "repl", feedback, None);
        assert_eq!(sess.turns_completed(), 1);
        assert_eq!(sess.stats.turns_executed, 1);
    }

    #[test]
    fn feedback_updates_cache_hit_ratio() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let feedback = ContextFeedback::from_usage(0, 900, 100, 200, false);
        sess.record_feedback("model", "repl", feedback, None);
        assert!((sess.stats.avg_cache_hit_ratio - 0.9).abs() < 1e-9);
    }

    #[test]
    fn warm_start_preserves_stats() {
        let mut stats = PipelineStats::default();
        stats.turns_executed = 5;
        stats.avg_cache_hit_ratio = 0.85;

        let sess = PipelineSession::with_warm_stats(PipelineConfig::default(), stats);
        assert_eq!(sess.stats.turns_executed, 5);
        assert!((sess.stats.avg_cache_hit_ratio - 0.85).abs() < 1e-9);
        assert_eq!(sess.turns_completed(), 0);
    }

    #[test]
    fn ptl_errors_escalate_and_abort() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        assert!(!sess.should_abort());

        sess.record_ptl_error();
        assert!(!sess.should_abort());
        assert_eq!(sess.current_pressure_tier(), CompactionTier::CompactHistory);

        sess.record_ptl_error();
        assert!(!sess.should_abort());
        assert_eq!(sess.current_pressure_tier(), CompactionTier::AggressivePrune);

        sess.record_ptl_error();
        assert!(sess.should_abort());
    }

    #[test]
    fn aborted_session_refuses_run_turn() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.record_ptl_error();
        sess.record_ptl_error();
        sess.record_ptl_error();

        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(1);
        let external = test_external();
        let limits = OptimizeLimits::default();

        let input = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "model",
            query_source: "repl",
        };

        let result = sess.run_turn(input);
        assert!(result.is_err());
    }

    #[test]
    fn successful_feedback_resets_recovery_after_clean_state() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        // Simulate: no PTL errors, just a normal successful turn
        let feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        sess.record_feedback("model", "repl", feedback, None);
        assert_eq!(sess.recovery.consecutive_ptl_errors, 0);
        assert!(!sess.recovery.is_in_recovery());
    }

    #[test]
    fn reset_after_ptl_requires_explicit_reset() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.record_ptl_error();
        sess.record_ptl_error();
        assert_eq!(sess.recovery.consecutive_ptl_errors, 2);

        // process_feedback won't clear PTL errors (they're "in flight")
        let feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        sess.record_feedback("model", "repl", feedback, None);
        assert_eq!(sess.recovery.consecutive_ptl_errors, 2);

        // Explicit reset (runtime calls this after successful retry)
        sess.recovery.reset_on_success();
        assert_eq!(sess.recovery.consecutive_ptl_errors, 0);
    }

    #[test]
    fn shadow_mode_detects_no_diff_on_same_output() {
        let sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(1);
        let external = test_external();
        let limits = OptimizeLimits::default();

        let input = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "model",
            query_source: "repl",
        };

        let output = sess.run_turn(input).unwrap();

        let input2 = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "model",
            query_source: "repl",
        };

        let shadow = sess
            .run_turn_shadow(input2, &output.optimized)
            .unwrap();
        assert!(shadow.diff.is_clean());
    }

    #[test]
    fn multi_turn_session_accumulates_stats() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let external = test_external();
        let limits = OptimizeLimits::default();

        for i in 1..=5 {
            let turn = test_turn_state(i);
            let input = TurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &turn,
                external: &external,
                optimize_limits: &limits,
                model_id: "claude-sonnet-4-6",
                query_source: "repl",
            };
            let _output = sess.run_turn(input).expect("should not abort");
            let feedback = ContextFeedback::from_usage(0, 800, 200, 300 + i as u64 * 50, false);
            sess.record_feedback("claude-sonnet-4-6", "repl", feedback, None);
        }

        assert_eq!(sess.turns_completed(), 5);
        assert_eq!(sess.stats.turns_executed, 5);
        assert!(sess.stats.avg_cache_hit_ratio > 0.0);
    }

    #[test]
    fn run_turn_adaptive_uses_tier_based_limits() {
        let sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(1);
        let external = test_external();

        let input = AdaptiveTurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            model_id: "claude-sonnet-4-6",
            query_source: "repl",
        };

        let output = sess.run_turn_adaptive(input).expect("should succeed");
        assert_eq!(output.metrics.turn_index, 1);
        assert!(output.explain.phase_timings.len() == 4);
    }

    #[test]
    fn adaptive_escalates_after_ptl_errors() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.record_ptl_error();
        sess.record_ptl_error();

        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(3);
        let external = test_external();

        let input = AdaptiveTurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            model_id: "claude-sonnet-4-6",
            query_source: "repl",
        };

        let output = sess.run_turn_adaptive(input).expect("2 PTL should not abort");
        assert_eq!(output.metrics.turn_index, 3);
    }

    #[test]
    fn record_feedback_with_output_tracks_section_usage() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(1);
        let external = test_external();
        let limits = OptimizeLimits::default();

        let input = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "model",
            query_source: "repl",
        };
        let output = sess.run_turn(input).unwrap();

        let feedback = ContextFeedback::from_usage(0, 800, 200, 500, false);
        sess.record_feedback("model", "repl", feedback, Some(&output));

        let history = sess.stats.section_token_history();
        assert!(
            !history.is_empty(),
            "section usage should be recorded from turn output"
        );
    }

    #[test]
    fn push_emergent_skill_populates_context() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        assert!(sess.emergent.is_empty());

        sess.push_emergent_skill("code-review", "detected REVIEW keyword", 1);
        assert!(!sess.emergent.is_empty());
        assert_eq!(sess.emergent.discovered_skills.len(), 1);
        assert_eq!(
            sess.emergent.discovered_skills[0].value.skill_name,
            "code-review"
        );
    }

    #[test]
    fn push_emergent_memory_deduplicates() {
        let mut sess = PipelineSession::new(PipelineConfig::default());

        sess.push_emergent_memory("User prefers Rust.", 0.9, 1);
        sess.push_emergent_memory("User prefers Rust.", 0.95, 1);
        assert_eq!(sess.emergent.prefetched_memory.len(), 1);
    }

    #[test]
    fn latch_header_freezes_on_first_call() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        assert!(sess.latch_header("anthropic-beta", "prompt-caching-2024-07-31", 1));
        assert!(!sess.latch_header("anthropic-beta", "something-else", 2));
        assert!(sess.latches.has_header("anthropic-beta"));
    }

    #[test]
    fn latch_cache_scope_freezes_on_first_call() {
        use crate::section_types::CacheScope;
        let mut sess = PipelineSession::new(PipelineConfig::default());
        assert!(sess.latch_cache_scope(CacheScope::Global, 1));
        assert!(!sess.latch_cache_scope(CacheScope::Session, 2));
        assert_eq!(sess.latches.cache_scope, Some(CacheScope::Global));
    }

    #[test]
    fn emergent_context_available_to_next_turn() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.push_emergent_skill("debug", "error in output", 1);

        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(2);
        let external = test_external();
        let limits = OptimizeLimits::default();

        let input = TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "model",
            query_source: "repl",
        };

        let _output = sess.run_turn(input).expect("should succeed with emergent");
    }
}
