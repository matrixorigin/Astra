//! Session-scoped pipeline orchestrator.
//!
//! `PipelineSession` owns the cross-turn mutable state (stats, latches,
//! recovery) and provides a single `run_turn()` entry point
//! that the agentic loop calls before every LLM request.
//!
//! This replaces the scattered inline assembly with a structured, testable
//! pipeline invocation that carries forward learned behavior across turns.

use crate::cache_diagnostics::{CacheBreakDetector, CacheBreakDetectorState, PromptStateSnapshot};
use crate::compaction_types::CompactionTier;
use crate::context_feedback::{ContextFeedback, RuntimeFeedbackFrame};
use crate::context_optimizer::ContextOptimized;
use crate::context_pipeline::{
    AdaptivePipelineRunInput, ContextPipeline, HistoryOptimizationOwner, PipelineAbort,
    PipelineExplain, PipelineRunInput, PipelineRunMetrics, PipelineRunOutput,
};
use crate::context_planner::ContextPlan;
use crate::context_serializer::SerializedProviderRequest;
use crate::context_sources::{
    AgentContext, ContextSources, ExternalSources, SessionContext, StaticSections, TurnState,
};
use crate::optimize_limits::OptimizeLimits;
use crate::pipeline_config::PipelineConfig;
use crate::pipeline_stats::PipelineStats;
use crate::recovery_state::RecoveryState;
use crate::session_latches::SessionLatches;
use crate::working_memory::WorkingMemoryState;
use std::sync::Arc;

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

/// Session-scoped pipeline orchestrator.
///
/// Instantiated once per session. Accumulates statistics, latches
/// context, and recovery state across turns. The agentic loop calls
/// `run_turn()` before each LLM request and `record_feedback()` after.
pub struct PipelineSession {
    pipeline: ContextPipeline,
    static_sections: Option<Arc<StaticSections>>,
    pub stats: PipelineStats,
    pub latches: SessionLatches,
    pub recovery: RecoveryState,
    session_current_date: String,
    working_memory: WorkingMemoryState,
    cache_detector: CacheBreakDetector,
    pending_prompt_snapshot: Option<PendingPromptSnapshot>,
    turns_completed: u32,
    latest_runtime_feedback: Option<RuntimeFeedbackFrame>,
    provider_cache_observed_since_feedback: bool,
    pending_provider_cache_break: Option<crate::cache_diagnostics::CacheBreakReason>,
    pending_audits: Vec<crate::pipeline_journal::PipelineJournalEvent>,
}

fn default_session_current_date() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct PendingPromptSnapshot {
    query_source: String,
    snapshot: PromptStateSnapshot,
    #[serde(default)]
    section_usage: std::collections::HashMap<crate::section_types::SectionKind, u32>,
    #[serde(default)]
    section_fingerprints: Vec<(crate::section_types::SectionKind, u64)>,
}

/// One exact, dispatched physical provider attempt ready for cache diagnosis.
/// The runtime constructs this only from the immutable prepared-body receipt
/// and an explicitly available (or unavailable) provider usage fact.
#[derive(Debug, Clone)]
pub struct ProviderAttemptCacheObservation {
    pub attempt_identity: crate::cache_diagnostics::ProviderAttemptCacheIdentity,
    pub dispatched: bool,
    pub fingerprint: crate::cache_diagnostics::ProviderFinalPromptFingerprint,
    pub cache_read_tokens: Option<u64>,
}

/// Merge cache-break evidence from multiple physical attempts belonging to
/// one logical feedback frame. Reasons are typed and deduplicated by value;
/// a later stable retry must never erase an earlier observed break.
fn merge_cache_break_reason(
    current: Option<crate::cache_diagnostics::CacheBreakReason>,
    incoming: Option<crate::cache_diagnostics::CacheBreakReason>,
) -> Option<crate::cache_diagnostics::CacheBreakReason> {
    use crate::cache_diagnostics::CacheBreakReason;

    fn append_unique(target: &mut Vec<CacheBreakReason>, reason: CacheBreakReason) {
        match reason {
            CacheBreakReason::Multiple(reasons) => {
                for reason in reasons {
                    append_unique(target, reason);
                }
            }
            reason if !target.contains(&reason) => target.push(reason),
            _ => {}
        }
    }

    let mut reasons = Vec::new();
    if let Some(reason) = current {
        append_unique(&mut reasons, reason);
    }
    if let Some(reason) = incoming {
        append_unique(&mut reasons, reason);
    }
    match reasons.len() {
        0 => None,
        1 => reasons.pop(),
        _ => Some(CacheBreakReason::Multiple(reasons)),
    }
}

impl PipelineSession {
    /// Create a new session with the given pipeline configuration.
    #[must_use]
    pub fn new(config: PipelineConfig) -> Self {
        Self::new_with_current_date(config, default_session_current_date())
    }

    /// Create a new session with the given pipeline configuration and an
    /// explicit session-stable current date.
    #[must_use]
    pub fn new_with_current_date(
        config: PipelineConfig,
        session_current_date: impl Into<String>,
    ) -> Self {
        Self {
            pipeline: ContextPipeline::new(config),
            static_sections: None,
            stats: PipelineStats::default(),
            latches: SessionLatches::default(),
            recovery: RecoveryState::default(),
            session_current_date: session_current_date.into(),
            working_memory: WorkingMemoryState::default(),
            cache_detector: CacheBreakDetector::new(),
            pending_prompt_snapshot: None,
            turns_completed: 0,
            latest_runtime_feedback: None,
            provider_cache_observed_since_feedback: false,
            pending_provider_cache_break: None,
            pending_audits: Vec::new(),
        }
    }

    /// Number of turns successfully completed in this session.
    #[must_use]
    pub fn turns_completed(&self) -> u32 {
        self.turns_completed
    }

    /// Return the immutable prompt sections owned by this pipeline session,
    /// building them once on first use. The returned `Arc` lets the caller
    /// borrow the sections while mutably advancing the rest of the session.
    pub fn static_sections_or_init(
        &mut self,
        init: impl FnOnce() -> StaticSections,
    ) -> Arc<StaticSections> {
        self.static_sections
            .get_or_insert_with(|| Arc::new(init()))
            .clone()
    }

    /// Run the pipeline for one turn. Returns the serialized provider request
    /// and associated metadata, or an abort if the session is in an
    /// unrecoverable error state.
    pub fn run_turn(&mut self, input: TurnInput<'_>) -> Result<TurnOutput, PipelineAbort> {
        let sources = ContextSources {
            statics: input.statics,
            agent: input.agent,
            latches: &self.latches,
            session: input.session,
            turn: input.turn,
            external: input.external,
            working_memory: Some(&self.working_memory),
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

        let output = self.pipeline.run(run_input)?;
        self.finish_turn_output(input.query_source, input.session, input.model_id, output)
    }

    /// Run the pipeline with tier-adaptive limits. The Plan phase determines
    /// the compaction tier, then limits are derived automatically from that tier.
    /// When a compaction cascade is active, clearing is suppressed to break the loop.
    /// This is the preferred entry point for runtime integration.
    pub fn run_turn_adaptive(
        &mut self,
        input: AdaptiveTurnInput<'_>,
    ) -> Result<TurnOutput, PipelineAbort> {
        self.run_turn_adaptive_with_history_owner(input, HistoryOptimizationOwner::Pipeline)
    }

    /// Adaptive pipeline entry point for runtimes with a downstream semantic
    /// history compactor. Planning and schema optimization still happen here,
    /// while lossy history reduction is delegated to exactly one owner.
    pub fn run_turn_adaptive_with_history_owner(
        &mut self,
        input: AdaptiveTurnInput<'_>,
        history_owner: HistoryOptimizationOwner,
    ) -> Result<TurnOutput, PipelineAbort> {
        let sources = ContextSources {
            statics: input.statics,
            agent: input.agent,
            latches: &self.latches,
            session: input.session,
            turn: input.turn,
            external: input.external,
            working_memory: Some(&self.working_memory),
            stats: &self.stats,
        };
        let run_input = AdaptivePipelineRunInput {
            sources: &sources,
            tokens: &input.turn.tokens,
            model_limit: input.session.model_limit,
            recovery: &self.recovery,
            latches: &self.latches,
            model_id: input.model_id,
            query_source: input.query_source,
            suppress_tool_result_clearing: self.stats.has_compaction_cascade(),
            history_owner,
        };
        let output = self.pipeline.run_adaptive(run_input)?;
        self.finish_turn_output(input.query_source, input.session, input.model_id, output)
    }

    fn finish_turn_output(
        &mut self,
        query_source: &str,
        session: &SessionContext,
        model_id: &str,
        output: PipelineRunOutput,
    ) -> Result<TurnOutput, PipelineAbort> {
        let PipelineRunOutput {
            plan,
            optimized,
            serialized,
            explain,
            metrics,
        } = output;
        let output = TurnOutput {
            plan,
            optimized,
            serialized,
            explain,
            metrics,
        };
        // A new pipeline request supersedes any orphaned observation
        // aggregation left by a prior request that failed after dispatch but
        // before runtime feedback. Host-internal physical retries and
        // continuations do not re-enter this boundary.
        self.provider_cache_observed_since_feedback = false;
        self.pending_provider_cache_break = None;
        self.pending_prompt_snapshot = Some(PendingPromptSnapshot::capture(
            query_source,
            session,
            model_id,
            &output,
        ));
        Ok(output)
    }

    /// Align planned cache diagnostics with the runtime's pre-client message
    /// and tool projection. This is not provider-final authority; transports
    /// that own an immutable prepared-body receipt must call
    /// [`Self::record_provider_attempt_cache_observation`] before feedback.
    ///
    /// Returns `false` only when no pipeline request is awaiting feedback.
    pub fn replace_pending_planned_wire_prompt(
        &mut self,
        messages: &[serde_json::Value],
        tool_schemas: &[serde_json::Value],
    ) -> bool {
        self.replace_pending_planned_wire_prompt_with_cache_capability(messages, tool_schemas, None)
    }

    /// Capability-aware counterpart used by provider-owning runtimes after
    /// final message consolidation. Keeping the capability and the exact wire
    /// projection together prevents a volatile system tail from being
    /// misdiagnosed as a leading-system mutation on prefix-cache providers.
    pub fn replace_pending_planned_wire_prompt_with_cache_capability(
        &mut self,
        messages: &[serde_json::Value],
        tool_schemas: &[serde_json::Value],
        cache_capability: Option<crate::cache_placement::CacheCapability>,
    ) -> bool {
        let Some(pending) = self.pending_prompt_snapshot.as_mut() else {
            return false;
        };
        let provider = pending.snapshot.provider.clone();
        let model = pending.snapshot.model.clone();
        let cache_eligible_tokens = pending.snapshot.cache_eligible_tokens;
        let Some(snapshot) =
            crate::cache_diagnostics::prompt_snapshot_from_messages_with_cache_capability(
                messages,
                tool_schemas,
                &provider,
                &model,
                cache_eligible_tokens,
                cache_capability,
            )
        else {
            return false;
        };
        pending.snapshot = snapshot;
        true
    }

    /// Consume one dispatched physical attempt from the immutable provider
    /// receipt. Each durable attempt identity is accepted at most once.
    /// Provider usage is optional: absent usage advances only the structural
    /// baseline and never fabricates a hit or miss.
    pub fn record_provider_attempt_cache_observation(
        &mut self,
        query_source: &str,
        observation: ProviderAttemptCacheObservation,
    ) -> bool {
        if !observation.dispatched {
            return false;
        }
        let Some(pending) = self.pending_prompt_snapshot.as_ref() else {
            return false;
        };
        let mut snapshot = pending.snapshot.clone();
        snapshot.attach_provider_final_fingerprint(observation.fingerprint);
        let (accepted, event) = self.cache_detector.record_provider_attempt_for_source(
            query_source,
            &observation.attempt_identity,
            snapshot,
            observation.cache_read_tokens,
        );
        if accepted {
            self.provider_cache_observed_since_feedback = true;
            self.pending_provider_cache_break = merge_cache_break_reason(
                self.pending_provider_cache_break.take(),
                event.map(|event| event.reason),
            );
        }
        accepted
    }

    /// Record feedback from the API response. Updates stats, recovery state,
    /// and enriches `feedback.cache_break_detected` from physical provider
    /// observations before persisting the turn into `PipelineStats`.
    ///
    /// Pass `turn_output` to also record per-section token usage (enables
    /// adaptive budget allocation). If unavailable, pass `None`.
    pub fn record_feedback(
        &mut self,
        model_id: &str,
        query_source: &str,
        feedback: &mut ContextFeedback,
        turn_output: Option<&TurnOutput>,
    ) {
        let provider_final_observed =
            std::mem::take(&mut self.provider_cache_observed_since_feedback);
        let provider_final_break = self.pending_provider_cache_break.take();
        let pending_snapshot = self.pending_prompt_snapshot.take();
        let recorded_pending_sections = pending_snapshot.as_ref().is_some_and(|pending| {
            !pending.section_usage.is_empty() || !pending.section_fingerprints.is_empty()
        });
        if let Some(pending) = pending_snapshot.as_ref() {
            self.stats.record_section_usage(&pending.section_usage);
            self.stats
                .record_section_fingerprint_hashes(&pending.section_fingerprints);
        }
        if provider_final_observed {
            if let Some(reason) = provider_final_break {
                feedback.attribute_cache_break(reason);
            }
        }

        self.stats.record(model_id, query_source, feedback);
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

        if !recorded_pending_sections && let Some(output) = turn_output {
            let mut usage = std::collections::HashMap::new();
            for section in &output.optimized.sections {
                *usage.entry(section.plan.kind).or_insert(0u32) += section.actual_tokens;
            }
            self.stats.record_section_usage(&usage);
            self.stats
                .record_section_fingerprints(&output.optimized.sections);
        }

        // Do not advance `turns_completed` on the abort path. A truncated
        // response that trips the unrecoverable-error streak is not a
        // successful turn — counting it inflates goal/milestone tracking and
        // pressure-tier denominators.
        if !self.recovery.should_abort() {
            self.turns_completed += 1;
        }
    }

    /// Record the canonical runtime frame for one successfully ingested model
    /// request. Cache diagnosis is enriched in place, then the exact final
    /// frame is retained for introspection and durable projection.
    pub fn record_runtime_feedback(
        &mut self,
        query_source: &str,
        frame: &mut RuntimeFeedbackFrame,
        turn_output: Option<&TurnOutput>,
    ) -> bool {
        if !self.can_accept_runtime_feedback(frame) {
            return false;
        }
        let Some(request_usage) = frame.request_usage else {
            if std::mem::take(&mut self.provider_cache_observed_since_feedback) {
                frame.cache_break_detected = self.pending_provider_cache_break.take();
                self.pending_prompt_snapshot = None;
            }
            self.latest_runtime_feedback = Some(frame.clone());
            return true;
        };
        let mut feedback = ContextFeedback {
            tokens: request_usage,
            cache_hit_ratio: request_usage.cache_hit_ratio(),
            was_truncated: frame.was_truncated,
            cache_break_detected: frame.cache_break_detected.clone(),
        };
        self.record_feedback(
            &frame.identity.model_id,
            query_source,
            &mut feedback,
            turn_output,
        );
        frame.cache_break_detected = feedback.cache_break_detected;
        self.latest_runtime_feedback = Some(frame.clone());
        true
    }

    /// Accept an immutable frame produced by a remote Server-owned loop.
    ///
    /// The receiving client may project this frame for introspection and its
    /// local journal, but must not run local cache diagnosis over it: doing so
    /// would mutate a Server-authored fact with client-local pending state.
    pub fn accept_authoritative_runtime_feedback(&mut self, frame: &RuntimeFeedbackFrame) -> bool {
        if !self.can_accept_runtime_feedback(frame) {
            return false;
        }
        self.latest_runtime_feedback = Some(frame.clone());
        true
    }

    #[must_use]
    pub fn latest_runtime_feedback(&self) -> Option<&RuntimeFeedbackFrame> {
        self.latest_runtime_feedback.as_ref()
    }

    fn can_accept_runtime_feedback(&self, candidate: &RuntimeFeedbackFrame) -> bool {
        if !candidate.is_valid() {
            return false;
        }
        let Some(current) = self.latest_runtime_feedback.as_ref() else {
            return true;
        };
        if candidate.progress.session_turn != current.progress.session_turn {
            return candidate.progress.session_turn > current.progress.session_turn;
        }
        if candidate.identity.run_id != current.identity.run_id {
            // A successfully ingested request from the current runtime is the
            // authority for a resumed/replaced run at the same user turn.
            return true;
        }
        candidate.progress.llm_rounds_completed > current.progress.llm_rounds_completed
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
        // Delegate to the single source of truth for recovery escalation
        // (CompactionTier::escalate_for_recovery) to avoid threshold desync.
        CompactionTier::Normal.escalate_for_recovery(&self.recovery)
    }

    /// Whether the session should abort (unrecoverable error streak).
    #[must_use]
    pub fn should_abort(&self) -> bool {
        self.recovery.should_abort()
    }

    /// Prompt-facing working memory for goal/task continuity.
    #[must_use]
    pub fn working_memory(&self) -> &WorkingMemoryState {
        &self.working_memory
    }

    /// Mutable working memory for runtime-owned decisions/blockers/next action.
    pub fn working_memory_mut(&mut self) -> &mut WorkingMemoryState {
        &mut self.working_memory
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
        self.pending_prompt_snapshot = None;
        self.cache_detector.reset_all_sources();
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

    /// Configure where prompt-cache break diff artifacts should be written.
    pub fn set_prompt_cache_diff_dir(&mut self, dir: impl Into<std::path::PathBuf>) {
        self.cache_detector.set_diff_dir(dir);
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
    pub fn latch_cache_scope(
        &mut self,
        scope: crate::section_types::CacheScope,
        turn: u32,
    ) -> bool {
        self.latches.latch_cache_scope(scope, turn)
    }

    /// Latch a provider feature gate.
    pub fn latch_feature(&mut self, key: impl Into<String>, turn: u32) -> bool {
        self.latches.latch_feature(key, turn)
    }

    // ── Snapshot & Restore ──────────────────────────────────────────────────

    /// Capture a full snapshot of all session-scoped pipeline state.
    /// Used for checkpoint persistence.
    #[must_use]
    pub fn snapshot_full_state(&self) -> PipelineSessionSnapshot {
        PipelineSessionSnapshot {
            stats: self.stats.clone(),
            latches: self.latches.clone(),
            recovery: self.recovery,
            working_memory: self.working_memory.clone(),
            cache_detector_state: self.cache_detector.snapshot_state(),
            pending_prompt_snapshot: self.pending_prompt_snapshot.clone(),
            turns_completed: self.turns_completed,
            latest_runtime_feedback: self.latest_runtime_feedback.clone(),
            provider_cache_observed_since_feedback: self.provider_cache_observed_since_feedback,
            pending_provider_cache_break: self.pending_provider_cache_break.clone(),
            session_current_date: Some(self.session_current_date.clone()),
        }
    }

    /// Restore a session from a full snapshot (checkpoint restore).
    #[must_use]
    pub fn from_snapshot(
        config: PipelineConfig,
        snapshot: PipelineSessionSnapshot,
        fallback_current_date: impl Into<String>,
    ) -> Self {
        let fallback_current_date = fallback_current_date.into();
        let PipelineSessionSnapshot {
            stats,
            latches,
            recovery,
            working_memory,
            cache_detector_state,
            pending_prompt_snapshot,
            turns_completed,
            latest_runtime_feedback,
            provider_cache_observed_since_feedback,
            pending_provider_cache_break,
            session_current_date,
        } = snapshot;
        let mut recovery = recovery;
        // Clear transient per-session error state on restore
        recovery.consecutive_ptl_errors = 0;
        recovery.consecutive_same_errors = 0;
        recovery.has_attempted_reactive_compact = false;
        let session_current_date = session_current_date
            .filter(|value| !value.is_empty())
            .unwrap_or(fallback_current_date);

        Self {
            pipeline: ContextPipeline::new(config),
            static_sections: None,
            stats,
            latches,
            recovery,
            session_current_date,
            working_memory,
            cache_detector: CacheBreakDetector::from_state(cache_detector_state),
            pending_prompt_snapshot,
            turns_completed,
            latest_runtime_feedback: latest_runtime_feedback.filter(RuntimeFeedbackFrame::is_valid),
            provider_cache_observed_since_feedback,
            pending_provider_cache_break,
            pending_audits: Vec::new(),
        }
    }

    #[must_use]
    pub fn current_date(&self) -> &str {
        &self.session_current_date
    }
}

/// Full snapshot of pipeline session state for checkpoint persistence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PipelineSessionSnapshot {
    pub stats: PipelineStats,
    pub latches: SessionLatches,
    pub recovery: RecoveryState,
    #[serde(default)]
    pub working_memory: WorkingMemoryState,
    #[serde(default)]
    pub cache_detector_state: CacheBreakDetectorState,
    #[serde(default)]
    pub(crate) pending_prompt_snapshot: Option<PendingPromptSnapshot>,
    #[serde(default)]
    pub turns_completed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_runtime_feedback: Option<RuntimeFeedbackFrame>,
    #[serde(default)]
    pub provider_cache_observed_since_feedback: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_provider_cache_break: Option<crate::cache_diagnostics::CacheBreakReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_current_date: Option<String>,
}

impl PendingPromptSnapshot {
    fn capture(
        query_source: &str,
        session: &SessionContext,
        model_id: &str,
        output: &TurnOutput,
    ) -> Self {
        let cache_eligible_tokens = output
            .optimized
            .sections
            .iter()
            .filter(|section| section.plan.scope != crate::section_types::CacheScope::None)
            .map(|section| section.actual_tokens as usize)
            .sum();
        let mut section_usage = std::collections::HashMap::new();
        let mut section_fingerprints = Vec::new();
        for section in &output.optimized.sections {
            *section_usage.entry(section.plan.kind).or_insert(0_u32) += section.actual_tokens;
            if let Some(text) = section.text() {
                section_fingerprints.push((
                    section.plan.kind,
                    crate::pipeline_stats::section_content_hash(section.plan.kind, text),
                ));
            }
        }
        Self {
            query_source: query_source.to_string(),
            snapshot: PromptStateSnapshot::capture_serialized(
                &output.serialized.system_blocks,
                &output.optimized.tool_schemas,
                &session.provider_name,
                model_id,
                cache_eligible_tokens,
            ),
            section_usage,
            section_fingerprints,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context_sources::EdgeProfile;
    use crate::microcompact::ProviderCacheStrategy;
    use crate::pipeline_config::ProviderCachePolicy;
    use crate::token_accounting::TokenAccounting;

    fn test_statics() -> StaticSections {
        StaticSections::test_default()
    }

    fn test_session_context() -> SessionContext {
        SessionContext {
            session_id: "sess-1".into(),
            run_id: "run-1".into(),
            model_id: "claude-sonnet-4-6".into(),
            provider_name: "anthropic".into(),
            pre_reserved_output_tokens: 0,
            model_limit: 200_000,
            provider_policy: ProviderCachePolicy::anthropic(),
            provider_strategy: ProviderCacheStrategy::default(),
            project_context: "test project".into(),
            edge_profile: EdgeProfile::default(),
            self_model: None,
            deferred_tools_block: String::new(),
            skill_listing_block: String::new(),
            current_date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
            user_id: None,
        }
    }

    fn test_turn_state(turn_index: u32) -> TurnState {
        TurnState {
            messages: vec![serde_json::json!({"role": "user", "content": "hello"})],
            tool_results: vec![],
            tokens: TokenAccounting::default(),
            active_skills: vec![],
            turn_index,
            recovery: RecoveryState::default(),
            last_user_message: "hello".into(),
        }
    }

    #[test]
    fn physical_retry_break_aggregation_is_typed_deduplicated_and_monotonic() {
        use crate::cache_diagnostics::CacheBreakReason;

        let system = CacheBreakReason::SystemPromptChanged;
        assert_eq!(
            merge_cache_break_reason(Some(system.clone()), None),
            Some(system.clone()),
            "a stable retry cannot erase an earlier physical-attempt break"
        );
        assert_eq!(
            merge_cache_break_reason(Some(system.clone()), Some(system.clone())),
            Some(system.clone()),
            "the same typed reason from two attempts is reported once"
        );
        assert_eq!(
            merge_cache_break_reason(
                Some(system.clone()),
                Some(CacheBreakReason::CacheControlChanged),
            ),
            Some(CacheBreakReason::Multiple(vec![
                system,
                CacheBreakReason::CacheControlChanged,
            ]))
        );
    }

    #[test]
    fn pending_cache_snapshot_is_replaced_with_final_wire_prompt() {
        let mut session = PipelineSession::new(PipelineConfig::default());
        session.pending_prompt_snapshot = Some(PendingPromptSnapshot {
            query_source: "test".to_string(),
            snapshot: PromptStateSnapshot::capture(
                "pipeline candidate",
                &[],
                "deepseek-v4-flash",
                7_225,
            ),
            section_usage: std::collections::HashMap::new(),
            section_fingerprints: Vec::new(),
        });
        let messages = vec![
            serde_json::json!({"role": "system", "content": "stable system"}),
            serde_json::json!({"role": "system", "content": "final wire runtime context"}),
            serde_json::json!({"role": "user", "content": "hello"}),
        ];
        let tools = vec![serde_json::json!({
            "type": "function",
            "function": {"name": "bash", "parameters": {"type": "object"}}
        })];
        let expected = crate::cache_diagnostics::prompt_snapshot_from_messages(
            &messages,
            &tools,
            "unknown",
            "deepseek-v4-flash",
            7_225,
        )
        .expect("wire snapshot");

        assert!(session.replace_pending_planned_wire_prompt(&messages, &tools));
        let actual = &session
            .pending_prompt_snapshot
            .as_ref()
            .expect("pending snapshot")
            .snapshot;
        assert_eq!(actual.system_prompt_hash, expected.system_prompt_hash);
        assert_eq!(actual.system_blocks, expected.system_blocks);
        assert_eq!(actual.tools_hash, expected.tools_hash);
        assert_eq!(actual.cache_eligible_tokens, 7_225);
    }

    #[test]
    fn static_sections_cache_is_scoped_to_pipeline_session() {
        let mut first_session = PipelineSession::new(PipelineConfig::default());
        let first = first_session.static_sections_or_init(|| {
            let mut sections = StaticSections::test_default();
            sections.core_rules.text = "first session".into();
            sections
        });
        let reused = first_session.static_sections_or_init(|| {
            panic!("a pipeline session must build static sections only once")
        });

        assert!(Arc::ptr_eq(&first, &reused));
        assert_eq!(reused.core_rules.text, "first session");

        let mut second_session = PipelineSession::new(PipelineConfig::default());
        let second = second_session.static_sections_or_init(|| {
            let mut sections = StaticSections::test_default();
            sections.core_rules.text = "second session".into();
            sections
        });

        assert!(!Arc::ptr_eq(&first, &second));
        assert_eq!(second.core_rules.text, "second session");
    }

    fn test_external() -> ExternalSources {
        ExternalSources {
            memory_entries: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn new_session_starts_with_zero_turns() {
        let sess = PipelineSession::new(PipelineConfig::default());
        assert_eq!(sess.turns_completed(), 0);
        assert!(!sess.should_abort());
    }

    #[test]
    fn new_pipeline_request_clears_orphaned_provider_observation_aggregation() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.provider_cache_observed_since_feedback = true;
        sess.pending_provider_cache_break =
            Some(crate::cache_diagnostics::CacheBreakReason::UnknownColdStart);
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(2);
        let external = test_external();
        let limits = OptimizeLimits::default();

        sess.run_turn(TurnInput {
            statics: &statics,
            agent: &agent,
            session: &session,
            turn: &turn,
            external: &external,
            optimize_limits: &limits,
            model_id: "m",
            query_source: "repl",
        })
        .expect("new pipeline request");

        assert!(!sess.provider_cache_observed_since_feedback);
        assert!(sess.pending_provider_cache_break.is_none());
        assert!(
            sess.pending_prompt_snapshot.is_some(),
            "a later pre-dispatch failure may retain only the new planned request"
        );
    }

    #[test]
    fn run_turn_produces_valid_output() {
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
            model_id: "claude-sonnet-4-6",
            query_source: "repl",
        };

        let output = sess.run_turn(input).expect("should not abort");
        assert_eq!(output.metrics.turn_index, 1);
        assert!(output.explain.phase_timings.len() == 4);
    }

    #[test]
    fn adaptive_pipeline_keeps_deferred_tools_in_the_capability_cache_epoch() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let mut session = test_session_context();
        session.deferred_tools_block = "<deferred-tools>\nweb_fetch\n</deferred-tools>".to_string();
        let turn = test_turn_state(1);
        let external = test_external();

        let output = sess
            .run_turn_adaptive(AdaptiveTurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &turn,
                external: &external,
                model_id: "claude-sonnet-4-6",
                query_source: "repl",
            })
            .expect("deferred tool discovery must remain serializable");

        let deferred = output
            .serialized
            .system_blocks
            .iter()
            .find(|block| block.kind == crate::section_types::SectionKind::DeferredTools)
            .expect("non-empty deferred manifest must reach the provider request");
        assert_eq!(
            deferred.scope,
            crate::section_types::CacheScope::Session,
            "deferred tool names are capability-epoch metadata and should be cacheable until admission changes"
        );
        assert!(deferred.text.contains("web_fetch"));

        let deferred_index = output
            .serialized
            .system_blocks
            .iter()
            .position(|block| block.kind == crate::section_types::SectionKind::DeferredTools)
            .expect("deferred tool manifest must be serialized");
        if let Some(first_volatile_index) = output
            .serialized
            .system_blocks
            .iter()
            .position(|block| block.scope == crate::section_types::CacheScope::None)
        {
            assert!(
                deferred_index < first_volatile_index,
                "capability metadata must remain in the stable cache epoch before turn-volatile blocks"
            );
        }
    }

    #[test]
    fn typed_user_feedback_reaches_the_serialized_provider_boundary() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.working_memory_mut().apply_user_feedback(
            astra_turn_types::UserFeedback {
                kind: astra_turn_types::UserFeedbackKind::Requirement,
                target: astra_turn_types::UserFeedbackTarget::Approach,
            },
            "Use the server-owned lifecycle, not a UI shadow state.",
        );
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let turn = test_turn_state(2);
        let external = test_external();
        let limits = OptimizeLimits::default();

        let output = sess
            .run_turn(TurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &turn,
                external: &external,
                optimize_limits: &limits,
                model_id: "claude-sonnet-4-6",
                query_source: "repl",
            })
            .expect("working-memory correction should serialize");

        let working_memory = output
            .serialized
            .system_blocks
            .iter()
            .find(|block| block.kind == crate::section_types::SectionKind::WorkingMemory)
            .expect("provider request must contain the working-memory section");
        assert!(
            working_memory
                .text
                .contains("Latest user requirement for approach")
        );
        assert!(
            working_memory
                .text
                .contains("Use the server-owned lifecycle, not a UI shadow state.")
        );
    }

    #[test]
    fn runtime_feedback_without_provider_receipt_does_not_invent_cache_observations() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let mut session = test_session_context();
        let external = test_external();
        let limits = OptimizeLimits::default();
        for turn_index in 1..=2 {
            session.project_context = format!("project revision {turn_index}");
            let turn = test_turn_state(turn_index);
            let output = sess
                .run_turn(TurnInput {
                    statics: &statics,
                    agent: &agent,
                    session: &session,
                    turn: &turn,
                    external: &external,
                    optimize_limits: &limits,
                    model_id: "model",
                    query_source: "repl",
                })
                .unwrap();
            let mut frame = crate::introspect::test_runtime_feedback(turn_index, 1, 9);
            assert!(sess.record_runtime_feedback("repl", &mut frame, Some(&output)));
            assert!(frame.cache_break_detected.is_none());
            assert_eq!(sess.cache_detector.stats().total_turns, 0);
            assert_eq!(sess.cache_detector.stats().cache_hits, 0);
            assert_eq!(sess.cache_detector.stats().cache_misses, 0);
            assert_eq!(sess.stats.turns_executed, turn_index);
            assert_eq!(sess.turns_completed(), turn_index);
            assert_eq!(sess.latest_runtime_feedback(), Some(&frame));
        }
    }

    #[test]
    fn provider_receipt_requires_dispatch_and_counts_each_identity_once() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let external = test_external();
        let limits = OptimizeLimits::default();
        let turn = test_turn_state(1);
        let output = sess
            .run_turn(TurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &turn,
                external: &external,
                optimize_limits: &limits,
                model_id: "model",
                query_source: "repl",
            })
            .unwrap();
        let mut observation = ProviderAttemptCacheObservation {
            attempt_identity: crate::cache_diagnostics::ProviderAttemptCacheIdentity {
                request_id: "dispatch-boundary".to_owned(),
                attempt: 0,
            },
            dispatched: false,
            fingerprint: crate::cache_diagnostics::ProviderFinalPromptFingerprint::default(),
            cache_read_tokens: None,
        };
        assert!(!sess.record_provider_attempt_cache_observation("repl", observation.clone()));
        observation.dispatched = true;
        assert!(sess.record_provider_attempt_cache_observation("repl", observation.clone()));
        observation.cache_read_tokens = Some(0);
        assert!(!sess.record_provider_attempt_cache_observation("repl", observation));
        let mut frame = crate::introspect::test_runtime_feedback(1, 1, 9);
        assert!(sess.record_runtime_feedback("repl", &mut frame, Some(&output)));
        assert!(frame.cache_break_detected.is_none());
        assert_eq!(sess.cache_detector.stats().total_turns, 0);
        assert_eq!(sess.cache_detector.stats().cache_hits, 0);
        assert_eq!(sess.cache_detector.stats().cache_misses, 0);
        assert_eq!(sess.stats.turns_executed, 1);
        assert_eq!(sess.turns_completed(), 1);
        assert_eq!(sess.latest_runtime_feedback(), Some(&frame));
    }

    #[test]
    fn exact_receipt_break_survives_retry_restore_and_feedback_without_usage() {
        use crate::cache_diagnostics::{
            CacheBreakReason, ProviderAttemptCacheIdentity, ProviderFinalPromptFingerprint,
        };
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let external = test_external();
        let limits = OptimizeLimits::default();
        let turn = test_turn_state(1);
        let output = sess
            .run_turn(TurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &turn,
                external: &external,
                optimize_limits: &limits,
                model_id: "model",
                query_source: "repl",
            })
            .unwrap();
        let mut retry = None;
        for (attempt, system, usage) in [
            (0, "system-v1", Some(0)),
            (1, "system-v2", Some(0)),
            (2, "system-v2", Some(u64::MAX)),
        ] {
            let observation = ProviderAttemptCacheObservation {
                attempt_identity: ProviderAttemptCacheIdentity {
                    request_id: "exact-receipt-retry".into(),
                    attempt,
                },
                dispatched: true,
                fingerprint: ProviderFinalPromptFingerprint {
                    cache_key_system_sha256: system.into(),
                    ..Default::default()
                },
                cache_read_tokens: usage,
            };
            assert!(sess.record_provider_attempt_cache_observation("repl", observation.clone()));
            retry = Some(observation);
        }
        assert_eq!(sess.cache_detector.stats().total_turns, 3);
        assert_eq!(sess.cache_detector.stats().cache_hits, 1);
        assert_eq!(sess.cache_detector.stats().cache_misses, 2);
        let checkpoint = serde_json::to_vec(&sess.snapshot_full_state()).unwrap();
        sess = PipelineSession::from_snapshot(
            PipelineConfig::default(),
            serde_json::from_slice(&checkpoint).unwrap(),
            "2026-10-06",
        );
        assert!(!sess.record_provider_attempt_cache_observation("repl", retry.unwrap()));
        let mut frame = crate::introspect::test_runtime_feedback(1, 1, 9);
        frame.request_usage = None;
        assert!(sess.record_runtime_feedback("repl", &mut frame, Some(&output)));
        assert_eq!(
            frame.cache_break_detected,
            Some(CacheBreakReason::SystemPromptChanged)
        );
        assert_eq!(sess.latest_runtime_feedback(), Some(&frame));
        assert_eq!(sess.cache_detector.stats().total_turns, 3);
        assert_eq!(sess.stats.turns_executed, 0);
        assert_eq!(sess.turns_completed(), 0);
        assert!(sess.pending_prompt_snapshot.is_none());
        assert!(sess.pending_provider_cache_break.is_none());
        let mut next_frame = crate::introspect::test_runtime_feedback(1, 2, 9);
        assert!(sess.record_runtime_feedback("repl", &mut next_frame, None));
        assert!(next_frame.cache_break_detected.is_none());
        assert_eq!(sess.stats.turns_executed, 1);
        assert_eq!(sess.turns_completed(), 1);
    }

    #[test]
    fn record_feedback_increments_turns() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let mut feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        sess.record_feedback("model", "repl", &mut feedback, None);
        assert_eq!(sess.turns_completed(), 1);
        assert_eq!(sess.stats.turns_executed, 1);
    }

    #[test]
    fn feedback_updates_cache_hit_ratio() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let mut feedback = ContextFeedback::from_usage(0, 900, 100, 200, false);
        sess.record_feedback("model", "repl", &mut feedback, None);
        assert!((sess.stats.avg_cache_hit_ratio - 0.9).abs() < 1e-9);
    }

    #[test]
    fn ptl_errors_escalate_and_abort() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        assert!(!sess.should_abort());

        sess.record_ptl_error();
        assert!(!sess.should_abort());
        // 1 PTL → TrimSchemas (matches escalate_for_recovery canonical mapping)
        assert_eq!(sess.current_pressure_tier(), CompactionTier::TrimSchemas);

        sess.record_ptl_error();
        assert!(!sess.should_abort());
        // 2 PTL → CompactHistory
        assert_eq!(sess.current_pressure_tier(), CompactionTier::CompactHistory);

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
        let mut feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        sess.record_feedback("model", "repl", &mut feedback, None);
        assert_eq!(sess.recovery.consecutive_ptl_errors, 0);
        assert!(!sess.recovery.is_in_recovery());
    }

    #[test]
    fn successful_feedback_clears_ptl_recovery() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        sess.record_ptl_error();
        sess.record_ptl_error();
        assert_eq!(sess.recovery.consecutive_ptl_errors, 2);

        // A successful response (not truncated) means recovery worked — clear PTL state
        let mut feedback = ContextFeedback::from_usage(1000, 800, 200, 500, false);
        sess.record_feedback("model", "repl", &mut feedback, None);
        assert_eq!(sess.recovery.consecutive_ptl_errors, 0);
        assert!(!sess.recovery.is_in_recovery());
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
            let mut feedback = ContextFeedback::from_usage(0, 800, 200, 300 + i as u64 * 50, false);
            sess.record_feedback("claude-sonnet-4-6", "repl", &mut feedback, None);
        }

        assert_eq!(sess.turns_completed(), 5);
        assert_eq!(sess.stats.turns_executed, 5);
        assert!(sess.stats.avg_cache_hit_ratio > 0.0);
    }

    #[test]
    fn run_turn_adaptive_uses_tier_based_limits() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext {
            tool_schemas: vec![serde_json::json!({
                "type": "function",
                "function": {
                    "name": "read_file",
                    "description": "A deliberately verbose description that must be pruned under aggressive pressure.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "A verbose property description that is not needed under pressure."
                            }
                        },
                        "required": ["path"]
                    }
                }
            })],
            ..Default::default()
        };
        let session = test_session_context();
        let mut turn = test_turn_state(1);
        turn.tokens = TokenAccounting::from_fields(195_000, 0, 0, 0);
        turn.messages = (0..12)
            .map(|index| {
                let role = if index % 2 == 0 { "user" } else { "assistant" };
                serde_json::json!({
                    "role": role,
                    "content": format!("round {index}: {}", "context ".repeat(64))
                })
            })
            .collect();
        let original_message_count = turn.messages.len();
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
        assert_eq!(
            output.plan.compact_tier,
            CompactionTier::AggressivePrune,
            "the plan must see the high-pressure request"
        );
        assert!(
            output.optimized.stats.schemas_pruned > 0,
            "adaptive limits must be derived from the selected plan tier"
        );
        assert!(
            output.optimized.messages.len() < original_message_count,
            "AggressivePrune must open the round-dropping gate"
        );
    }

    #[test]
    fn downstream_semantic_compactor_is_the_only_lossy_history_owner() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let mut turn = test_turn_state(1);
        turn.tokens = TokenAccounting::from_fields(195_000, 0, 0, 0);
        turn.messages = (0..12)
            .map(|index| {
                let role = if index % 2 == 0 { "user" } else { "assistant" };
                serde_json::json!({
                    "role": role,
                    "content": format!("round {index}: {}", "context ".repeat(64))
                })
            })
            .collect();
        let original_messages = turn.messages.clone();
        let external = test_external();

        let output = sess
            .run_turn_adaptive_with_history_owner(
                AdaptiveTurnInput {
                    statics: &statics,
                    agent: &agent,
                    session: &session,
                    turn: &turn,
                    external: &external,
                    model_id: "model",
                    query_source: "runtime",
                },
                HistoryOptimizationOwner::DownstreamSemanticCompactor,
            )
            .expect("should succeed");

        assert_eq!(output.plan.compact_tier, CompactionTier::AggressivePrune);
        assert_eq!(
            output.optimized.messages, original_messages,
            "planning must not pre-drop context that the semantic compactor needs to summarize"
        );
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

        let output = sess
            .run_turn_adaptive(input)
            .expect("2 PTL should not abort");
        assert_eq!(output.metrics.turn_index, 3);
    }

    #[test]
    fn record_feedback_without_retained_output_tracks_pending_section_usage() {
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
        let _output = sess.run_turn(input).unwrap();

        let mut feedback = ContextFeedback::from_usage(0, 800, 200, 500, false);
        sess.record_feedback("model", "repl", &mut feedback, None);

        let history = sess.stats.section_token_history();
        assert!(
            !history.is_empty(),
            "the pending plan/output observation must close the production \
             feedback loop even when the runtime no longer owns TurnOutput"
        );
    }

    #[test]
    fn snapshot_restore_preserves_pending_prompt_snapshot_and_turn_count() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let statics = test_statics();
        let agent = AgentContext::default();
        let session = test_session_context();
        let external = test_external();
        let limits = OptimizeLimits::default();

        let first_turn = test_turn_state(1);
        let first_output = sess
            .run_turn(TurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &first_turn,
                external: &external,
                optimize_limits: &limits,
                model_id: "model",
                query_source: "repl",
            })
            .unwrap();
        let mut first_feedback = ContextFeedback::from_usage(0, 800, 200, 500, false);
        sess.record_feedback("model", "repl", &mut first_feedback, Some(&first_output));

        let second_turn = test_turn_state(2);
        let _second_output = sess
            .run_turn(TurnInput {
                statics: &statics,
                agent: &agent,
                session: &session,
                turn: &second_turn,
                external: &external,
                optimize_limits: &limits,
                model_id: "model",
                query_source: "repl",
            })
            .unwrap();

        assert_eq!(sess.turns_completed(), 1);
        assert!(sess.pending_prompt_snapshot.is_some());

        let mut snapshot_json = serde_json::to_value(sess.snapshot_full_state()).unwrap();
        assert!(!sess.stats.section_token_history().is_empty());
        let crate::pipeline_session_serde::RestoreOutcome::Restored(snapshot) =
            crate::pipeline_session_serde::parse_pipeline_state(Some(&snapshot_json))
        else {
            panic!("current nonempty pipeline snapshot must decode");
        };
        let restored = PipelineSession::from_snapshot(
            PipelineConfig::default(),
            *snapshot,
            sess.current_date(),
        );
        assert_eq!(
            restored.stats.section_usage_ema,
            sess.stats.section_usage_ema
        );
        assert_eq!(
            restored.stats.section_fingerprints,
            sess.stats.section_fingerprints
        );
        assert_eq!(restored.stats.section_churns, sess.stats.section_churns);
        let mut restored_json = serde_json::to_value(restored.snapshot_full_state()).unwrap();
        // These maps serialize as unordered arrays; compare their typed values above.
        for field in [
            "section_usage_ema",
            "section_fingerprints",
            "section_churns",
        ] {
            snapshot_json["stats"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            restored_json["stats"]
                .as_object_mut()
                .unwrap()
                .remove(field);
        }
        assert_eq!(
            restored_json, snapshot_json,
            "current statistics and pending prompt attribution must round-trip intact"
        );

        assert_eq!(restored.turns_completed(), 1);
        assert!(
            restored.pending_prompt_snapshot.is_some(),
            "mid-turn pending prompt snapshot must survive restore for exact cache-break attribution"
        );
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
    fn runtime_feedback_is_monotonic_and_survives_snapshot() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let mut current = crate::introspect::test_runtime_feedback(4, 2, 8);
        assert!(sess.record_runtime_feedback("test", &mut current, None));

        let mut stale = crate::introspect::test_runtime_feedback(3, 99, 0);
        assert!(!sess.record_runtime_feedback("test", &mut stale, None));
        let mut duplicate = current.clone();
        assert!(!sess.record_runtime_feedback("test", &mut duplicate, None));
        let mut resumed = crate::introspect::test_runtime_feedback(4, 1, 9);
        resumed.identity.run_id = "run-2".into();
        assert!(sess.record_runtime_feedback("test", &mut resumed, None));
        assert_eq!(sess.latest_runtime_feedback(), Some(&resumed));

        let restored = PipelineSession::from_snapshot(
            PipelineConfig::default(),
            sess.snapshot_full_state(),
            "2026-08-09",
        );
        assert_eq!(restored.latest_runtime_feedback(), Some(&resumed));
    }

    #[test]
    fn invalid_runtime_feedback_never_becomes_pipeline_authority() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let mut invalid = crate::introspect::test_runtime_feedback(1, 1, 9);
        invalid.identity.run_id.clear();
        assert!(!sess.record_runtime_feedback("test", &mut invalid, None));
        assert!(sess.latest_runtime_feedback().is_none());
        assert_eq!(sess.stats.turns_executed, 0);

        let mut snapshot = sess.snapshot_full_state();
        snapshot.latest_runtime_feedback = Some(invalid);
        let restored =
            PipelineSession::from_snapshot(PipelineConfig::default(), snapshot, "2026-08-09");
        assert!(restored.latest_runtime_feedback().is_none());
    }

    #[test]
    fn authoritative_runtime_feedback_is_projected_without_local_enrichment() {
        let mut sess = PipelineSession::new(PipelineConfig::default());
        let frame = crate::introspect::test_runtime_feedback(2, 3, 7);

        assert!(sess.accept_authoritative_runtime_feedback(&frame));
        assert_eq!(sess.latest_runtime_feedback(), Some(&frame));
        assert_eq!(sess.stats.turns_executed, 0);

        let mut duplicate = frame.clone();
        duplicate.cache_break_detected =
            Some(crate::cache_diagnostics::CacheBreakReason::UnknownColdStart);
        assert!(!sess.accept_authoritative_runtime_feedback(&duplicate));
        assert_eq!(sess.latest_runtime_feedback(), Some(&frame));
    }
}
