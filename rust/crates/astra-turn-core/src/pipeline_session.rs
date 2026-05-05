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
    pub fn record_feedback(&mut self, model_id: &str, query_source: &str, feedback: ContextFeedback) {
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
        sess.record_feedback("model", "repl", feedback);
        assert_eq!(sess.turns_completed(), 1);
        assert_eq!(sess.stats.turns_executed, 1);
    }

    #[test]
    fn feedback_updates_cache_hit_ratio() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let feedback = ContextFeedback::from_usage(0, 900, 100, 200, false);
        sess.record_feedback("model", "repl", feedback);
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
        sess.record_feedback("model", "repl", feedback);
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
        sess.record_feedback("model", "repl", feedback);
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
            sess.record_feedback("claude-sonnet-4-6", "repl", feedback);
        }

        assert_eq!(sess.turns_completed(), 5);
        assert_eq!(sess.stats.turns_executed, 5);
        assert!(sess.stats.avg_cache_hit_ratio > 0.0);
    }
}
